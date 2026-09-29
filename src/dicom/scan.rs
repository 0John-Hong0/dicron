//! Filesystem discovery and DICOM header scanning.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use dicom_core::{Length, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::text::SpecificCharacterSet;
use dicom_encoding::transfer_syntax::{Codec, TransferSyntax, TransferSyntaxIndex};
use dicom_object::{
    DefaultDicomObject, DicomCollectorOptions, FileMetaTable, FileMetaTableBuilder,
    InMemDicomObject, OpenFileOptions, file::ReadPreamble,
};
use dicom_parser::DataSetReader;
use dicom_parser::dataset::lazy_read::LazyDataSetReader;
use dicom_parser::dataset::read::{DataSetReaderOptions, ValueReadStrategy};
use dicom_parser::dataset::{DataToken, LazyDataToken};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use walkdir::WalkDir;

use super::index::{DicomIndexBuilder, DicomIndexEntry, DicomIndexMetadata};
use super::model::DicomIndex;
use super::text_encoding::{AutoEncodingEvidence, MAX_DETECTION_TEXT_VALUE_BYTES, TextEncoding};

const EXPLICIT_VR_BIG_ENDIAN_UID: &str = "1.2.840.10008.1.2.2";
const MAX_SCAN_WORKERS: usize = 8;
const FILES_PER_SCAN_WORKER: usize = 16;
// Most image headers fit in one read at this size, reducing read calls without substantially
// over-reading pixel data or increasing per-worker memory.
const SCAN_BUFFER_CAPACITY: usize = 32 * 1024;
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(50);

struct ScannedDicomFile {
    index_entry: Option<DicomIndexEntry>,
}

struct ScannedIndexMetadata {
    metadata: DicomIndexMetadata,
    has_pixel_data: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct BuildProgress {
    pub(crate) processed_file_count: usize,
    pub(crate) total_file_count: usize,
    pub(crate) readable_dicom_count: usize,
}

pub(crate) fn build_from_folder_with_progress_with_encoding<F>(
    folder_path: &Path,
    cancel: &AtomicBool,
    text_encoding: TextEncoding,
    mut on_progress: F,
) -> Result<DicomIndex>
where
    F: FnMut(BuildProgress),
{
    let file_paths = collect_walkdir_files(folder_path, cancel)?;
    build_for_files_with_progress(&file_paths, cancel, text_encoding, &mut on_progress)
}

#[cfg(test)]
pub(crate) fn build_for_inputs_with_progress<F>(
    input_paths: &[PathBuf],
    cancel: &AtomicBool,
    on_progress: F,
) -> Result<DicomIndex>
where
    F: FnMut(BuildProgress),
{
    build_for_inputs_with_progress_with_encoding(
        input_paths,
        cancel,
        TextEncoding::DicomDefault,
        on_progress,
    )
}

pub(crate) fn build_for_inputs_with_progress_with_encoding<F>(
    input_paths: &[PathBuf],
    cancel: &AtomicBool,
    text_encoding: TextEncoding,
    mut on_progress: F,
) -> Result<DicomIndex>
where
    F: FnMut(BuildProgress),
{
    let file_paths = collect_file_paths(input_paths, cancel)?;
    build_for_files_with_progress(&file_paths, cancel, text_encoding, &mut on_progress)
}

fn build_for_files_with_progress<F>(
    file_paths: &[PathBuf],
    cancel: &AtomicBool,
    text_encoding: TextEncoding,
    mut on_progress: F,
) -> Result<DicomIndex>
where
    F: FnMut(BuildProgress),
{
    let total_input_file_count = file_paths.len();
    let mut displayable_dicom_count = 0;
    let mut index_builder = DicomIndexBuilder::default();

    on_progress(BuildProgress {
        processed_file_count: 0,
        total_file_count: total_input_file_count,
        readable_dicom_count: 0,
    });

    scan_files_with_progress(
        file_paths,
        cancel,
        text_encoding,
        &mut on_progress,
        &mut |scanned_file| {
            let Some(index_entry) = scanned_file.and_then(|file| file.index_entry) else {
                return;
            };
            index_builder.add_entry(index_entry);
            displayable_dicom_count += 1;
        },
    )?;

    let patients = index_builder.into_patients();

    Ok(DicomIndex {
        patients,
        total_file_count: displayable_dicom_count,
    })
}

fn scan_files_with_progress<F, S>(
    file_paths: &[PathBuf],
    cancel: &AtomicBool,
    text_encoding: TextEncoding,
    on_progress: &mut F,
    on_scanned_file: &mut S,
) -> Result<()>
where
    F: FnMut(BuildProgress),
    S: FnMut(Option<ScannedDicomFile>),
{
    let total_file_count = file_paths.len();
    if total_file_count == 0 {
        return Ok(());
    }

    let mut completed_files = Vec::with_capacity(total_file_count);
    completed_files.resize_with(total_file_count, || None);
    let mut next_file_to_emit = 0;
    let next_file_index = AtomicUsize::new(0);
    let worker_count = scan_worker_count(total_file_count);
    let (result_sender, result_receiver) = mpsc::channel();
    let mut processed_file_count = 0;
    let mut readable_dicom_count = 0;
    let mut last_progress_report = Instant::now();

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let result_sender = result_sender.clone();
            let next_file_index = &next_file_index;

            scope.spawn(move || {
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }

                    let file_index = next_file_index.fetch_add(1, Ordering::Relaxed);
                    let Some(file_path) = file_paths.get(file_index) else {
                        break;
                    };
                    let scanned_file = scan_dicom_file(file_path, text_encoding).ok();

                    if result_sender.send((file_index, scanned_file)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(result_sender);

        for (file_index, scanned_file) in result_receiver {
            processed_file_count += 1;
            readable_dicom_count += usize::from(scanned_file.is_some());
            completed_files[file_index] = Some(scanned_file);
            while next_file_to_emit < total_file_count {
                let Some(scanned_file) = completed_files[next_file_to_emit].take() else {
                    break;
                };
                on_scanned_file(scanned_file);
                next_file_to_emit += 1;
            }
            if processed_file_count == total_file_count
                || last_progress_report.elapsed() >= PROGRESS_REPORT_INTERVAL
            {
                on_progress(BuildProgress {
                    processed_file_count,
                    total_file_count,
                    readable_dicom_count,
                });
                last_progress_report = Instant::now();
            }
        }
    });

    if cancel.load(Ordering::Relaxed) {
        return Err(scan_cancelled());
    }

    Ok(())
}

fn scan_worker_count(file_count: usize) -> usize {
    let available_workers = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);

    scan_worker_count_for(file_count, available_workers)
}

fn scan_worker_count_for(file_count: usize, available_workers: usize) -> usize {
    let available_workers = available_workers.min(MAX_SCAN_WORKERS);
    let useful_workers = file_count.div_ceil(FILES_PER_SCAN_WORKER);

    available_workers.min(useful_workers).max(1)
}

fn collect_file_paths(input_paths: &[PathBuf], cancel: &AtomicBool) -> Result<Vec<PathBuf>> {
    let mut file_paths = Vec::new();

    for input_path in input_paths {
        if cancel.load(Ordering::Relaxed) {
            return Err(scan_cancelled());
        }

        let Ok(metadata) = input_path.metadata() else {
            continue;
        };

        if metadata.is_dir() {
            file_paths.extend(collect_walkdir_files(input_path, cancel)?);
        } else if metadata.is_file() {
            file_paths.push(input_path.clone());
        }
    }

    Ok(file_paths)
}

fn collect_walkdir_files(folder_path: &Path, cancel: &AtomicBool) -> Result<Vec<PathBuf>> {
    let mut file_paths = Vec::new();

    for entry_result in WalkDir::new(folder_path) {
        if cancel.load(Ordering::Relaxed) {
            return Err(scan_cancelled());
        }

        let Ok(entry) = entry_result else {
            continue;
        };

        if entry.file_type().is_file() {
            file_paths.push(entry.path().to_path_buf());
        }
    }

    Ok(file_paths)
}

fn scan_cancelled() -> anyhow::Error {
    anyhow::anyhow!("scan cancelled")
}

#[cfg(test)]
pub(crate) fn build_for_file(file_path: &Path) -> Result<DicomIndex> {
    build_for_file_with_encoding(file_path, TextEncoding::DicomDefault)
}

pub(crate) fn build_for_file_with_encoding(
    file_path: &Path,
    text_encoding: TextEncoding,
) -> Result<DicomIndex> {
    let scanned_file = scan_dicom_file(file_path, text_encoding)?;
    let mut index_builder = DicomIndexBuilder::default();

    let Some(index_entry) = scanned_file.index_entry else {
        anyhow::bail!("DICOM object does not contain supported image pixel metadata");
    };
    index_builder.add_entry(index_entry);
    let patients = index_builder.into_patients();

    Ok(DicomIndex {
        patients,
        total_file_count: 1,
    })
}

pub(crate) fn open_dicom_file(file_path: &Path) -> Result<DefaultDicomObject> {
    open_dicom_file_with_encoding(file_path, TextEncoding::DicomDefault)
}

pub(crate) fn open_dicom_file_with_encoding(
    file_path: &Path,
    text_encoding: TextEncoding,
) -> Result<DefaultDicomObject> {
    let assumed_charset = resolve_assumed_charset(file_path, text_encoding);
    if assumed_charset != SpecificCharacterSet::ISO_IR_6 {
        return open_dicom_file_with_assumed_charset(file_path, assumed_charset);
    }

    match OpenFileOptions::new().open_file(file_path) {
        Ok(object) => Ok(object),
        Err(part_10_error) => open_raw_dicom_dataset(file_path, true)
            .with_context(|| format!("could not parse DICOM file: {part_10_error}")),
    }
}

fn open_dicom_file_with_assumed_charset(
    file_path: &Path,
    assumed_charset: SpecificCharacterSet,
) -> Result<DefaultDicomObject> {
    let (reader, meta) = open_dataset_source(file_path)?;
    let transfer_syntax = TransferSyntaxRegistry
        .get(meta.transfer_syntax())
        .with_context(|| {
            format!(
                "unsupported DICOM transfer syntax {}",
                meta.transfer_syntax()
            )
        })?;
    let object =
        InMemDicomObject::read_dataset_with_ts_cs(reader, transfer_syntax, assumed_charset)?;
    let mut file_object =
        object.with_meta(FileMetaTableBuilder::new().transfer_syntax(meta.transfer_syntax()))?;
    *file_object.meta_mut() = meta;
    Ok(file_object)
}

fn open_raw_dicom_dataset(file_path: &Path, read_all: bool) -> Result<DefaultDicomObject> {
    let transfer_syntax_uid = raw_dataset_transfer_syntax(file_path)?;
    let mut collector = DicomCollectorOptions::new()
        .expected_ts(transfer_syntax_uid)
        .read_preamble(ReadPreamble::Never)
        .open_file(file_path)?;
    let mut object = InMemDicomObject::new_empty();

    if read_all {
        collector.read_dataset_to_end(&mut object)?;
    } else {
        collector.read_dataset_up_to_pixeldata(&mut object)?;
    }

    Ok(object.with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))?)
}

fn raw_dataset_transfer_syntax(file_path: &Path) -> Result<&'static str> {
    let mut prefix = [0_u8; 8];
    File::open(file_path)?.read_exact(&mut prefix)?;
    raw_dataset_transfer_syntax_from_prefix(&prefix)
}

fn is_value_representation(value: &[u8]) -> bool {
    matches!(
        value,
        b"AE"
            | b"AS"
            | b"AT"
            | b"CS"
            | b"DA"
            | b"DS"
            | b"DT"
            | b"FD"
            | b"FL"
            | b"IS"
            | b"LO"
            | b"LT"
            | b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"PN"
            | b"SH"
            | b"SL"
            | b"SQ"
            | b"SS"
            | b"ST"
            | b"SV"
            | b"TM"
            | b"UC"
            | b"UI"
            | b"UL"
            | b"UN"
            | b"UR"
            | b"US"
            | b"UT"
            | b"UV"
    )
}

fn resolve_assumed_charset(file_path: &Path, mode: TextEncoding) -> SpecificCharacterSet {
    match mode {
        TextEncoding::DicomDefault => SpecificCharacterSet::ISO_IR_6,
        TextEncoding::KoreanEucKr | TextEncoding::Utf8 | TextEncoding::JapaneseShiftJis => {
            mode.assumed_character_set()
        }
        TextEncoding::Auto => detect_missing_charset_encoding(file_path)
            .ok()
            .flatten()
            .map_or(
                SpecificCharacterSet::ISO_IR_6,
                TextEncoding::assumed_character_set,
            ),
    }
}

fn detect_missing_charset_encoding(file_path: &Path) -> Result<Option<TextEncoding>> {
    let (reader, meta) = open_dataset_source(file_path)?;
    let transfer_syntax = TransferSyntaxRegistry
        .get(meta.transfer_syntax())
        .with_context(|| {
            format!(
                "unsupported DICOM transfer syntax {}",
                meta.transfer_syntax()
            )
        })?;

    match transfer_syntax.codec() {
        Codec::Dataset(Some(adapter)) => {
            probe_streaming_dataset(adapter.adapt_reader(Box::new(reader)), transfer_syntax)
        }
        Codec::Dataset(None) => Ok(None),
        Codec::None | Codec::EncapsulatedPixelData(..) => {
            probe_seekable_dataset(reader, transfer_syntax)
        }
    }
}

fn is_detection_text(tag: Tag, vr: VR) -> bool {
    tag.0.is_multiple_of(2)
        && matches!(
            vr,
            VR::PN | VR::LO | VR::SH | VR::ST | VR::LT | VR::UT | VR::UC
        )
}

fn probe_seekable_dataset<R>(
    reader: R,
    transfer_syntax: &TransferSyntax,
) -> Result<Option<TextEncoding>>
where
    R: Read + Seek,
{
    let mut tokens = LazyDataSetReader::new_with_ts(reader, transfer_syntax)?;
    let mut evidence = AutoEncodingEvidence::default();
    let mut sequence_depth = 0_usize;

    while let Some(token) = tokens.advance() {
        match token? {
            // The declared charset has priority, including an empty declaration.
            LazyDataToken::ElementHeader(header) if header.tag == tags::SPECIFIC_CHARACTER_SET => {
                return Ok(None);
            }
            // DICOM's tag order places (0008,0005) before pixel data. Leave image bytes unread.
            LazyDataToken::ElementHeader(header)
                if sequence_depth == 0 && header.tag == tags::PIXEL_DATA =>
            {
                break;
            }
            LazyDataToken::PixelSequenceStart if sequence_depth == 0 => break,
            LazyDataToken::ElementHeader(header)
                if sequence_depth == 0 && is_detection_text(header.tag, header.vr) =>
            {
                if header.len.0 as usize > MAX_DETECTION_TEXT_VALUE_BYTES {
                    return Ok(None);
                }
                let value = tokens
                    .advance()
                    .with_context(|| format!("missing value for DICOM element {}", header.tag))??
                    .into_value_with_strategy(ValueReadStrategy::Raw)?;
                evidence.observe(&value.to_bytes());
            }
            token @ (LazyDataToken::LazyValue { .. } | LazyDataToken::LazyItemValue { .. }) => {
                token.skip()?;
            }
            LazyDataToken::SequenceStart { .. } | LazyDataToken::PixelSequenceStart => {
                sequence_depth += 1;
            }
            LazyDataToken::SequenceEnd => {
                sequence_depth = sequence_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    Ok(evidence.detect())
}

fn probe_streaming_dataset<R>(
    reader: R,
    transfer_syntax: &TransferSyntax,
) -> Result<Option<TextEncoding>>
where
    R: Read,
{
    let options = DataSetReaderOptions::default().value_read(ValueReadStrategy::Raw);
    let tokens = DataSetReader::new_with_ts_options(reader, transfer_syntax, options)?;
    let mut evidence = AutoEncodingEvidence::default();
    let mut sequence_depth = 0_usize;
    let mut pending_text = false;

    for token in tokens {
        match token? {
            DataToken::ElementHeader(header) if header.tag == tags::SPECIFIC_CHARACTER_SET => {
                return Ok(None);
            }
            DataToken::ElementHeader(header)
                if sequence_depth == 0 && header.tag == tags::PIXEL_DATA =>
            {
                break;
            }
            DataToken::PixelSequenceStart if sequence_depth == 0 => break,
            DataToken::ElementHeader(header) => {
                if sequence_depth == 0
                    && is_detection_text(header.tag, header.vr)
                    && header.len.0 as usize > MAX_DETECTION_TEXT_VALUE_BYTES
                {
                    return Ok(None);
                }
                pending_text = sequence_depth == 0 && is_detection_text(header.tag, header.vr);
            }
            DataToken::PrimitiveValue(value) => {
                if pending_text {
                    evidence.observe(&value.to_bytes());
                    pending_text = false;
                }
            }
            DataToken::SequenceStart { .. } | DataToken::PixelSequenceStart => {
                pending_text = false;
                sequence_depth += 1;
            }
            DataToken::SequenceEnd => {
                pending_text = false;
                sequence_depth = sequence_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    Ok(evidence.detect())
}

fn scan_dicom_file(file_path: &Path, text_encoding: TextEncoding) -> Result<ScannedDicomFile> {
    let assumed_charset = resolve_assumed_charset(file_path, text_encoding);
    let (reader, meta) = open_dataset_source(file_path)?;
    let transfer_syntax_uid = meta.transfer_syntax();
    let transfer_syntax = TransferSyntaxRegistry
        .get(transfer_syntax_uid)
        .with_context(|| format!("unsupported DICOM transfer syntax {transfer_syntax_uid}"))?;
    let scanned_metadata = match transfer_syntax.codec() {
        // Deflated data sets are streaming-only after decompression, so retain the eager reader
        // for this uncommon transfer syntax.
        Codec::Dataset(Some(adapter)) => scan_streaming_dataset(
            adapter.adapt_reader(Box::new(reader)),
            transfer_syntax,
            assumed_charset,
        )?,
        Codec::Dataset(None) => anyhow::bail!("unsupported DICOM data set encoding"),
        // Native and encapsulated pixel-data syntaxes leave the data set itself seekable. The
        // lazy reader can skip irrelevant values without decoding or allocating them.
        Codec::None | Codec::EncapsulatedPixelData(..) => {
            scan_seekable_dataset(reader, transfer_syntax, assumed_charset)?
        }
    };
    let index_entry = DicomIndexEntry::from_metadata(
        file_path,
        scanned_metadata.metadata,
        scanned_metadata.has_pixel_data,
    );
    Ok(ScannedDicomFile { index_entry })
}

fn open_dataset_source(file_path: &Path) -> Result<(BufReader<File>, FileMetaTable)> {
    let file = File::open(file_path)?;
    let mut reader = BufReader::with_capacity(SCAN_BUFFER_CAPACITY, file);
    let (preamble_length, raw_transfer_syntax_uid) = {
        let prefix = reader.fill_buf()?;
        if prefix.len() >= 132 && &prefix[128..132] == b"DICM" {
            (128, None)
        } else if prefix.len() >= 4 && &prefix[..4] == b"DICM" {
            (0, None)
        } else {
            (0, Some(raw_dataset_transfer_syntax_from_prefix(prefix)?))
        }
    };
    let meta = match raw_transfer_syntax_uid {
        Some(transfer_syntax_uid) => FileMetaTableBuilder::new()
            .transfer_syntax(transfer_syntax_uid)
            .build()?,
        None => {
            reader.consume(preamble_length);
            FileMetaTable::from_reader(&mut reader)?
        }
    };
    Ok((reader, meta))
}

fn scan_seekable_dataset<R>(
    reader: R,
    transfer_syntax: &TransferSyntax,
    assumed_charset: SpecificCharacterSet,
) -> Result<ScannedIndexMetadata>
where
    R: Read + Seek,
{
    let mut tokens = LazyDataSetReader::new_with_ts_cs(reader, transfer_syntax, assumed_charset)?;
    let mut metadata = DicomIndexMetadata::default();
    let mut sequence_depth = 0_usize;
    let mut has_pixel_data = false;

    while let Some(token) = tokens.advance() {
        match token? {
            LazyDataToken::ElementHeader(header)
                if sequence_depth == 0 && header.tag == tags::PIXEL_DATA =>
            {
                has_pixel_data = header.len != Length(0);
                break;
            }
            LazyDataToken::PixelSequenceStart if sequence_depth == 0 => {
                has_pixel_data = true;
                break;
            }
            LazyDataToken::ElementHeader(header)
                if sequence_depth == 0
                    && (DicomIndexMetadata::includes_tag(header.tag)
                        || header.tag == tags::SPECIFIC_CHARACTER_SET) =>
            {
                let value = tokens
                    .advance()
                    .with_context(|| format!("missing value for DICOM element {}", header.tag))??
                    .into_value()
                    .with_context(|| format!("could not read DICOM element {}", header.tag))?;
                if DicomIndexMetadata::includes_tag(header.tag) {
                    metadata.put_primitive(header.tag, &value);
                }
            }
            token @ (LazyDataToken::LazyValue { .. } | LazyDataToken::LazyItemValue { .. }) => {
                token.skip()?;
            }
            LazyDataToken::SequenceStart { .. } | LazyDataToken::PixelSequenceStart => {
                sequence_depth += 1;
            }
            LazyDataToken::SequenceEnd => {
                sequence_depth = sequence_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    Ok(ScannedIndexMetadata {
        metadata,
        has_pixel_data,
    })
}

fn scan_streaming_dataset<R>(
    reader: R,
    transfer_syntax: &TransferSyntax,
    assumed_charset: SpecificCharacterSet,
) -> Result<ScannedIndexMetadata>
where
    R: Read,
{
    let mut tokens = DataSetReader::new_with_ts_cs(reader, transfer_syntax, assumed_charset)?;
    let mut metadata = DicomIndexMetadata::default();
    let mut sequence_depth = 0_usize;
    let mut has_pixel_data = false;

    while let Some(token) = tokens.next() {
        match token? {
            DataToken::ElementHeader(header)
                if sequence_depth == 0 && header.tag == tags::PIXEL_DATA =>
            {
                has_pixel_data = header.len != Length(0);
                break;
            }
            DataToken::PixelSequenceStart if sequence_depth == 0 => {
                has_pixel_data = true;
                break;
            }
            DataToken::ElementHeader(header)
                if sequence_depth == 0 && DicomIndexMetadata::includes_tag(header.tag) =>
            {
                let value = tokens
                    .next()
                    .with_context(|| format!("missing value for DICOM element {}", header.tag))??;
                let DataToken::PrimitiveValue(value) = value else {
                    anyhow::bail!("unexpected value token for DICOM element {}", header.tag);
                };
                metadata.put_primitive(header.tag, &value);
            }
            DataToken::SequenceStart { .. } | DataToken::PixelSequenceStart => {
                sequence_depth += 1;
            }
            DataToken::SequenceEnd => {
                sequence_depth = sequence_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    Ok(ScannedIndexMetadata {
        metadata,
        has_pixel_data,
    })
}

fn raw_dataset_transfer_syntax_from_prefix(prefix: &[u8]) -> Result<&'static str> {
    if prefix.len() < 8 {
        anyhow::bail!("DICOM data set is too short");
    }

    if !is_value_representation(&prefix[4..6]) {
        return Ok(uids::IMPLICIT_VR_LITTLE_ENDIAN);
    }

    let little_endian_group = u16::from_le_bytes([prefix[0], prefix[1]]);
    let big_endian_group = u16::from_be_bytes([prefix[0], prefix[1]]);
    if little_endian_group <= big_endian_group {
        Ok(uids::EXPLICIT_VR_LITTLE_ENDIAN)
    } else {
        Ok(EXPLICIT_VR_BIG_ENDIAN_UID)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use dicom_core::{DataElement, PrimitiveValue, VR, value::PixelFragmentSequence};
    use dicom_dictionary_std::{tags, uids};
    use dicom_encoding::text::TextCodec;
    use dicom_object::{FileDicomObject, FileMetaTableBuilder};

    use super::{
        TextEncoding, build_for_file, build_for_file_with_encoding, build_for_inputs_with_progress,
        build_for_inputs_with_progress_with_encoding, detect_missing_charset_encoding,
        scan_worker_count_for,
    };

    static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

    fn temporary_file_path(label: &str) -> PathBuf {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("dicron-{label}-{}-{sequence}", std::process::id()))
    }

    fn write_test_object(path: &Path, include_image_pixel_module: bool) {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(if include_image_pixel_module {
                uids::CT_IMAGE_STORAGE
            } else {
                uids::RT_STRUCTURE_SET_STORAGE
            })
            .media_storage_sop_instance_uid("2.25.500")
            .build()
            .unwrap();
        let mut object = FileDicomObject::new_empty_with_meta(meta);

        object.put_element(DataElement::new(
            tags::SOP_CLASS_UID,
            VR::UI,
            PrimitiveValue::from(if include_image_pixel_module {
                uids::CT_IMAGE_STORAGE
            } else {
                uids::RT_STRUCTURE_SET_STORAGE
            }),
        ));
        object.put_element(DataElement::new(
            tags::SOP_INSTANCE_UID,
            VR::UI,
            PrimitiveValue::from("2.25.500"),
        ));

        if include_image_pixel_module {
            for (tag, value) in [
                (tags::ROWS, 1_u16),
                (tags::COLUMNS, 1_u16),
                (tags::SAMPLES_PER_PIXEL, 1_u16),
                (tags::BITS_ALLOCATED, 8_u16),
                (tags::BITS_STORED, 8_u16),
                (tags::HIGH_BIT, 7_u16),
                (tags::PIXEL_REPRESENTATION, 0_u16),
            ] {
                object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
            }
            object.put_element(DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from("MONOCHROME2"),
            ));
            object.put_element(DataElement::new(
                tags::MODALITY,
                VR::CS,
                PrimitiveValue::from("CT"),
            ));
            object.put_element(DataElement::new(
                tags::PIXEL_DATA,
                VR::OB,
                PrimitiveValue::from(vec![0_u8]),
            ));
        }

        object.write_to_file(path).unwrap();
    }

    #[test]
    fn extensionless_dicom_files_are_accepted_by_parsing() {
        let path = temporary_file_path("extensionless");
        write_test_object(&path, true);

        let index = build_for_file(&path).unwrap();

        assert_eq!(index.total_file_count, 1);
        assert_eq!(
            index.patients[0].studies[0].series_groups[0]
                .modality
                .as_deref(),
            Some("CT")
        );
        assert_eq!(
            index.patients[0].studies[0].series_groups[0].slices.len(),
            1
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn raw_dataset_without_preamble_or_marker_is_accepted() {
        let part_10_path = temporary_file_path("part-10-source");
        let raw_path = temporary_file_path("raw-dataset");
        write_test_object(&part_10_path, true);
        let object = dicom_object::open_file(&part_10_path).unwrap();
        object
            .write_dataset(File::create(&raw_path).unwrap())
            .unwrap();

        let index = build_for_file(&raw_path).unwrap();
        let loaded = crate::dicom::load_dicom_frame(&raw_path, 0).unwrap();
        let korean_loaded =
            crate::dicom::load_dicom_frame_with_encoding(&raw_path, 0, TextEncoding::KoreanEucKr)
                .unwrap();
        let auto_index = build_for_file_with_encoding(&raw_path, TextEncoding::Auto).unwrap();

        assert_eq!(index.total_file_count, 1);
        assert_eq!(auto_index.total_file_count, 1);
        assert_eq!(loaded.frame.frame_count, 1);
        assert_eq!(korean_loaded.frame.frame_count, 1);
        std::fs::remove_file(part_10_path).unwrap();
        std::fs::remove_file(raw_path).unwrap();
    }

    #[test]
    fn single_pass_scan_keeps_hierarchy_and_slice_metadata() {
        let path = temporary_file_path("index-metadata");
        write_test_object(&path, true);
        let mut object = dicom_object::open_file(&path).unwrap();
        for (tag, vr, value) in [
            (tags::PATIENT_ID, VR::LO, "patient-7"),
            (tags::PATIENT_NAME, VR::PN, "Doe^Jane"),
            (tags::STUDY_INSTANCE_UID, VR::UI, "2.25.701"),
            (tags::STUDY_DESCRIPTION, VR::LO, "Head"),
            (tags::STUDY_DATE, VR::DA, "20260820"),
            (tags::STUDY_TIME, VR::TM, "101112"),
            (tags::SERIES_INSTANCE_UID, VR::UI, "2.25.702"),
            (tags::SERIES_DESCRIPTION, VR::LO, "Axial"),
            (tags::SERIES_NUMBER, VR::IS, "7"),
            (tags::INSTANCE_NUMBER, VR::IS, "12"),
            (tags::IMAGE_POSITION_PATIENT, VR::DS, "0\\0\\5"),
            (tags::IMAGE_ORIENTATION_PATIENT, VR::DS, "1\\0\\0\\0\\1\\0"),
            (tags::NUMBER_OF_FRAMES, VR::IS, "2"),
        ] {
            object.put_element(DataElement::new(tag, vr, PrimitiveValue::from(value)));
        }
        object.write_to_file(&path).unwrap();

        let index = build_for_file(&path).unwrap();
        let patient = &index.patients[0];
        let study = &patient.studies[0];
        let series = &study.series_groups[0];

        assert_eq!(patient.display_name, "Doe^Jane (patient-7)");
        assert_eq!(study.display_name, "20260820 101112 - Head");
        assert_eq!(series.display_name, "Series 7 - Axial");
        assert_eq!(series.series_description.as_deref(), Some("Axial"));
        assert_eq!(series.series_number, Some(7));
        assert_eq!(series.slices.len(), 2);
        assert_eq!(series.slices[0].instance_number, Some(12));
        assert_eq!(series.slices[0].sort_position, Some(5.0));
        assert_eq!(series.slices[1].frame_index, 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn lazy_scan_uses_the_declared_character_set_for_index_text() {
        let path = temporary_file_path("utf8-index-metadata");
        write_test_object(&path, true);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.convert_to_utf8();
        object.put_element(DataElement::new(
            tags::PATIENT_ID,
            VR::LO,
            PrimitiveValue::from("patient-utf8"),
        ));
        object.put_element(DataElement::new(
            tags::PATIENT_NAME,
            VR::PN,
            PrimitiveValue::from("홍길동"),
        ));
        object.write_to_file(&path).unwrap();

        assert_eq!(detect_missing_charset_encoding(&path).unwrap(), None);
        let index = build_for_file_with_encoding(&path, TextEncoding::KoreanEucKr).unwrap();

        assert_eq!(index.patients[0].display_name, "홍길동 (patient-utf8)");
        let auto_index = build_for_file_with_encoding(&path, TextEncoding::Auto).unwrap();
        assert_eq!(auto_index.patients[0].display_name, "홍길동 (patient-utf8)");
        let loaded =
            crate::dicom::load_dicom_frame_with_encoding(&path, 0, TextEncoding::KoreanEucKr)
                .unwrap();
        assert_eq!(
            loaded.metadata.overlay.patient_label.as_deref(),
            Some("홍길동 (patient-utf8)")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn explicit_korean_reopen_decodes_missing_charset_without_changing_the_file() {
        let path = temporary_file_path("missing-korean-charset");
        write_test_object(&path, true);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            tags::PATIENT_NAME,
            VR::PN,
            PrimitiveValue::from("ABCDEF"),
        ));
        object.write_to_file(&path).unwrap();

        let original = std::fs::read(&path).unwrap();
        let encoded_name = TextEncoding::KoreanEucKr
            .assumed_character_set()
            .encode("홍길동")
            .unwrap();
        assert_eq!(encoded_name.len(), 6);
        let mut encoded_file = original;
        let name_offset = encoded_file
            .windows(6)
            .position(|window| window == b"ABCDEF")
            .unwrap();
        encoded_file[name_offset..name_offset + 6].copy_from_slice(&encoded_name);
        std::fs::write(&path, &encoded_file).unwrap();

        assert_eq!(
            detect_missing_charset_encoding(&path).unwrap(),
            Some(TextEncoding::KoreanEucKr)
        );
        let default_index = build_for_file(&path).unwrap();
        assert_ne!(default_index.patients[0].display_name, "홍길동");
        let index = build_for_file_with_encoding(&path, TextEncoding::KoreanEucKr).unwrap();
        assert_eq!(index.patients[0].display_name, "홍길동");
        let auto_index = build_for_file_with_encoding(&path, TextEncoding::Auto).unwrap();
        assert_eq!(auto_index.patients[0].display_name, "홍길동");
        let scanned = build_for_inputs_with_progress_with_encoding(
            std::slice::from_ref(&path),
            &AtomicBool::new(false),
            TextEncoding::Auto,
            |_| {},
        )
        .unwrap();
        assert_eq!(scanned.patients[0].display_name, "홍길동");
        let loaded =
            crate::dicom::load_dicom_frame_with_encoding(&path, 0, TextEncoding::Auto).unwrap();
        assert_eq!(
            loaded.metadata.overlay.patient_label.as_deref(),
            Some("홍길동")
        );
        assert_eq!(std::fs::read(&path).unwrap(), encoded_file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn auto_decodes_undeclared_utf8_text() {
        let path = temporary_file_path("missing-utf8-charset");
        write_test_object(&path, true);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            tags::PATIENT_NAME,
            VR::PN,
            PrimitiveValue::from("ABCDEFGHIJ"),
        ));
        object.write_to_file(&path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let offset = bytes
            .windows(10)
            .position(|window| window == b"ABCDEFGHIJ")
            .unwrap();
        bytes[offset..offset + 10].copy_from_slice("홍길동 ".as_bytes());
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(
            detect_missing_charset_encoding(&path).unwrap(),
            Some(TextEncoding::Utf8)
        );
        let default = build_for_file_with_encoding(&path, TextEncoding::DicomDefault).unwrap();
        let auto = build_for_file_with_encoding(&path, TextEncoding::Auto).unwrap();
        let manual = build_for_file_with_encoding(&path, TextEncoding::Utf8).unwrap();
        assert_ne!(default.patients[0].display_name, "홍길동");
        assert_eq!(auto.patients[0].display_name, "홍길동");
        assert_eq!(manual.patients[0].display_name, "홍길동");
        let loaded =
            crate::dicom::load_dicom_frame_with_encoding(&path, 0, TextEncoding::Auto).unwrap();
        assert_eq!(
            loaded.metadata.overlay.patient_label.as_deref(),
            Some("홍길동")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn auto_decodes_undeclared_japanese_kana_and_manual_mode_matches() {
        let path = temporary_file_path("missing-japanese-charset");
        write_test_object(&path, true);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            tags::PATIENT_NAME,
            VR::PN,
            PrimitiveValue::from("ABCDEFGHIJKL"),
        ));
        object.write_to_file(&path).unwrap();

        let encoded_name = TextEncoding::JapaneseShiftJis
            .assumed_character_set()
            .encode("やまだたろう")
            .unwrap();
        assert_eq!(encoded_name.len(), 12);
        let mut bytes = std::fs::read(&path).unwrap();
        let offset = bytes
            .windows(12)
            .position(|window| window == b"ABCDEFGHIJKL")
            .unwrap();
        bytes[offset..offset + 12].copy_from_slice(&encoded_name);
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(
            detect_missing_charset_encoding(&path).unwrap(),
            Some(TextEncoding::JapaneseShiftJis)
        );
        let default = build_for_file_with_encoding(&path, TextEncoding::DicomDefault).unwrap();
        let auto = build_for_file_with_encoding(&path, TextEncoding::Auto).unwrap();
        let manual = build_for_file_with_encoding(&path, TextEncoding::JapaneseShiftJis).unwrap();
        assert_ne!(default.patients[0].display_name, "やまだたろう");
        assert_eq!(auto.patients[0].display_name, "やまだたろう");
        assert_eq!(manual.patients[0].display_name, "やまだたろう");
        let loaded =
            crate::dicom::load_dicom_frame_with_encoding(&path, 0, TextEncoding::Auto).unwrap();
        assert_eq!(
            loaded.metadata.overlay.patient_label.as_deref(),
            Some("やまだたろう")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn manual_japanese_decodes_kanji_only_when_auto_is_ambiguous() {
        let path = temporary_file_path("ambiguous-japanese-charset");
        write_test_object(&path, true);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            tags::PATIENT_NAME,
            VR::PN,
            PrimitiveValue::from("ABCDEFGH"),
        ));
        object.write_to_file(&path).unwrap();

        let encoded_name = TextEncoding::JapaneseShiftJis
            .assumed_character_set()
            .encode("山田太郎")
            .unwrap();
        assert_eq!(encoded_name.len(), 8);
        let mut bytes = std::fs::read(&path).unwrap();
        let offset = bytes
            .windows(8)
            .position(|window| window == b"ABCDEFGH")
            .unwrap();
        bytes[offset..offset + 8].copy_from_slice(&encoded_name);
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(detect_missing_charset_encoding(&path).unwrap(), None);
        let default = build_for_file_with_encoding(&path, TextEncoding::DicomDefault).unwrap();
        let auto = build_for_file_with_encoding(&path, TextEncoding::Auto).unwrap();
        let manual = build_for_file_with_encoding(&path, TextEncoding::JapaneseShiftJis).unwrap();
        assert_eq!(
            auto.patients[0].display_name,
            default.patients[0].display_name
        );
        assert_eq!(manual.patients[0].display_name, "山田太郎");
        let loaded =
            crate::dicom::load_dicom_frame_with_encoding(&path, 0, TextEncoding::JapaneseShiftJis)
                .unwrap();
        assert_eq!(
            loaded.metadata.overlay.patient_label.as_deref(),
            Some("山田太郎")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn encapsulated_pixel_data_is_classified_without_decoding_fragments() {
        let source_path = temporary_file_path("native-source");
        let compressed_path = temporary_file_path("encapsulated");
        write_test_object(&source_path, true);
        let mut object = dicom_object::open_file(&source_path).unwrap();
        object.update_meta(|meta| {
            meta.transfer_syntax = uids::JPEG_BASELINE8_BIT.to_owned();
        });
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new_fragments(vec![vec![0xFF, 0xD8, 0xFF, 0xD9]]),
        ));
        object.write_to_file(&compressed_path).unwrap();

        let index = build_for_file(&compressed_path).unwrap();

        assert_eq!(index.total_file_count, 1);
        std::fs::remove_file(source_path).unwrap();
        std::fs::remove_file(compressed_path).unwrap();
    }

    #[test]
    fn deflated_dataset_uses_the_streaming_scan_fallback() {
        let source_path = temporary_file_path("native-deflate-source");
        let deflated_path = temporary_file_path("deflated");
        write_test_object(&source_path, true);
        let mut object = dicom_object::open_file(&source_path).unwrap();
        object.update_meta(|meta| {
            meta.transfer_syntax = uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.to_owned();
        });
        object.write_to_file(&deflated_path).unwrap();

        let index = build_for_file(&deflated_path).unwrap();
        let korean_index =
            build_for_file_with_encoding(&deflated_path, TextEncoding::KoreanEucKr).unwrap();
        let auto_index = build_for_file_with_encoding(&deflated_path, TextEncoding::Auto).unwrap();

        assert_eq!(index.total_file_count, 1);
        assert_eq!(korean_index.total_file_count, 1);
        assert_eq!(auto_index.total_file_count, 1);
        std::fs::remove_file(source_path).unwrap();
        std::fs::remove_file(deflated_path).unwrap();
    }

    #[test]
    fn readable_non_image_dicom_files_are_not_displayable() {
        let path = temporary_file_path("non-image");
        write_test_object(&path, false);

        let error = build_for_file(&path).err().unwrap();

        assert!(
            error
                .to_string()
                .contains("does not contain supported image pixel metadata")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn image_metadata_without_pixel_data_is_not_displayable() {
        let path = temporary_file_path("missing-pixel-data");
        write_test_object(&path, true);
        let mut object = dicom_object::open_file(&path).unwrap();
        assert!(object.remove_element(tags::PIXEL_DATA));
        object.write_to_file(&path).unwrap();

        let error = build_for_file(&path).err().unwrap();

        assert!(
            error
                .to_string()
                .contains("does not contain supported image pixel metadata")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn multi_file_scan_indexes_every_file_and_reports_completed_work() {
        const FILE_COUNT: usize = 40;
        let paths: Vec<_> = (0..FILE_COUNT)
            .map(|file_index| {
                let path = temporary_file_path(&format!("batch-{file_index}"));
                write_test_object(&path, true);
                path
            })
            .collect();
        let cancel = AtomicBool::new(false);
        let mut progress_updates = Vec::new();

        let index = build_for_inputs_with_progress(&paths, &cancel, |progress| {
            progress_updates.push(progress)
        })
        .unwrap();

        assert_eq!(index.total_file_count, FILE_COUNT);
        assert_eq!(index.patients.len(), FILE_COUNT);
        assert_eq!(progress_updates.first().unwrap().processed_file_count, 0);
        let final_progress = progress_updates.last().unwrap();
        assert_eq!(final_progress.processed_file_count, FILE_COUNT);
        assert_eq!(final_progress.total_file_count, FILE_COUNT);
        assert_eq!(final_progress.readable_dicom_count, FILE_COUNT);
        assert!(
            progress_updates.windows(2).all(|updates| {
                updates[0].processed_file_count < updates[1].processed_file_count
            })
        );

        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn scan_worker_count_respects_workload_cpu_count_and_cap() {
        assert_eq!(scan_worker_count_for(1, 32), 1);
        assert_eq!(scan_worker_count_for(80, 32), 5);
        assert_eq!(scan_worker_count_for(10_000, 4), 4);
        assert_eq!(scan_worker_count_for(10_000, 32), 8);
    }
}
