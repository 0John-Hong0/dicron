//! Filesystem discovery and DICOM header scanning.

use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use dicom_core::{Length, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::decode::{self, Decode};
use dicom_encoding::text::SpecificCharacterSet;
use dicom_encoding::transfer_syntax::{Codec, TransferSyntax, TransferSyntaxIndex};
use dicom_object::{
    DefaultDicomObject, DicomCollectorOptions, FileMetaTable, FileMetaTableBuilder,
    InMemDicomObject, OpenFileOptions, file::ReadPreamble,
};
use dicom_parser::dataset::LazyDataToken;
use dicom_parser::dataset::lazy_read::LazyDataSetReader;
use dicom_parser::dataset::read::ValueReadStrategy;
use dicom_parser::{DynStatefulDecoder, StatefulDecode};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use walkdir::WalkDir;

use super::index::{DicomIndexBuilder, DicomIndexEntry, DicomIndexMetadata, PixelDataExtent};
use super::model::DicomIndex;
use super::text_encoding::{AutoEncodingEvidence, MAX_DETECTION_TEXT_VALUE_BYTES, TextEncoding};

const EXPLICIT_VR_BIG_ENDIAN_UID: &str = "1.2.840.10008.1.2.2";
const MAX_SCAN_WORKERS: usize = 8;
const FILES_PER_SCAN_WORKER: usize = 16;
// Most image headers fit in one read at this size, reducing read calls without substantially
// over-reading pixel data or increasing per-worker memory.
const SCAN_BUFFER_CAPACITY: usize = 32 * 1024;
/// The longest indexed value the header scan reads. The indexed elements are
/// identifiers, names, descriptions, dates and numbers, which the standard
/// limits to at most a few hundred bytes; the charset probe uses the same
/// bound for its text values.
const MAX_INDEXED_VALUE_BYTES: u32 = 4096;
/// The deepest sequence nesting a file may have. dicom-object reads each
/// level with nested calls, and a debug build overflowed the 2 MiB stack of
/// a loader thread after about 270 levels, which aborts the app; this limit
/// keeps a fourfold margin. Image files nest a few levels; enhanced
/// multi-frame functional groups three or four.
const MAX_SEQUENCE_DEPTH: usize = 64;
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(50);

struct ScannedDicomFile {
    index_entry: Option<DicomIndexEntry>,
}

struct ScannedIndexMetadata {
    metadata: DicomIndexMetadata,
    pixel_data: Option<PixelDataExtent>,
}

/// The top-level PixelData element the seekable scan stopped at.
enum PixelDataHeader {
    Native { length: u32 },
    Encapsulated,
}

/// The items of an encapsulated PixelData value, read up to the first one that
/// does not fit in the file.
struct PixelDataItems {
    /// Fragments after the Basic Offset Table that fit in the file.
    fragment_count: u32,
    /// The first item that declares more bytes than remain in the file.
    overrun: Option<PixelItemOverrun>,
    /// Where the data set resumes, if the items end with a sequence delimiter.
    sequence_end: Option<u64>,
}

struct PixelItemOverrun {
    /// 0 for the Basic Offset Table, then the fragments from 1.
    item_index: u32,
    declared_length: u32,
    remaining_length: u64,
}

impl fmt::Display for PixelItemOverrun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.item_index {
            0 => formatter.write_str("the Basic Offset Table of the pixel data")?,
            fragment => write!(formatter, "pixel data fragment {fragment}")?,
        }
        write!(
            formatter,
            " declares {} bytes, but only {} remain in the file",
            self.declared_length, self.remaining_length
        )
    }
}

pub(crate) struct OpenedDicomFile {
    pub(crate) object: DefaultDicomObject,
    // Read actual header elements separately: FileMetaTable fills missing fields and drops
    // unsupported tags. Its normalized values belong to decoding, not source metadata.
    pub(crate) original_meta: Option<InMemDicomObject>,
}

struct DatasetSource {
    reader: BufReader<File>,
    transfer_syntax_uid: String,
    file_meta: Option<FileMetaTable>,
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
    open_dicom_file_with_encoding(file_path, TextEncoding::DicomDefault).map(|opened| opened.object)
}

pub(crate) fn open_dicom_file_with_encoding(
    file_path: &Path,
    text_encoding: TextEncoding,
) -> Result<OpenedDicomFile> {
    ensure_declared_lengths_fit_file(file_path)?;
    let assumed_charset = resolve_assumed_charset(file_path, text_encoding);
    if assumed_charset != SpecificCharacterSet::ISO_IR_6 {
        return open_dicom_file_with_assumed_charset(file_path, assumed_charset);
    }

    match OpenFileOptions::new().open_file(file_path) {
        Ok(object) => Ok(OpenedDicomFile {
            original_meta: read_original_file_meta(file_path)?,
            object,
        }),
        Err(part_10_error) => open_raw_dicom_dataset(file_path, true)
            .map(|object| OpenedDicomFile {
                object,
                original_meta: None,
            })
            .with_context(|| format!("could not parse DICOM file: {part_10_error}")),
    }
}

fn open_dicom_file_with_assumed_charset(
    file_path: &Path,
    assumed_charset: SpecificCharacterSet,
) -> Result<OpenedDicomFile> {
    let DatasetSource {
        reader,
        transfer_syntax_uid,
        file_meta,
    } = open_dataset_source(file_path)?;
    let transfer_syntax = TransferSyntaxRegistry
        .get(&transfer_syntax_uid)
        .with_context(|| format!("unsupported DICOM transfer syntax {transfer_syntax_uid}"))?;
    let object =
        InMemDicomObject::read_dataset_with_ts_cs(reader, transfer_syntax, assumed_charset)?;
    let object = match file_meta {
        Some(meta) => object.with_exact_meta(meta),
        None => {
            object.with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))?
        }
    };
    Ok(OpenedDicomFile {
        object,
        original_meta: read_original_file_meta(file_path)?,
    })
}

/// Refuses a file whose data set declares an element or a pixel data item
/// longer than the bytes left in the file, or nests sequences deeper than
/// MAX_SEQUENCE_DEPTH. dicom-parser sets aside a value's declared length
/// before reading it, so one corrupt length, such as that of the Basic Offset
/// Table or of trailing padding, would make every full open allocate up to
/// gigabytes before it fails at the end of the file. The walk covers the
/// whole data set, the elements after PixelData included, and skips values
/// instead of decoding them. A file meta value longer than the file is
/// refused too. An incomplete walk is an error.
fn ensure_declared_lengths_fit_file(file_path: &Path) -> Result<()> {
    let DatasetSource {
        reader,
        transfer_syntax_uid,
        ..
    } = open_dataset_source(file_path)?;
    let transfer_syntax = TransferSyntaxRegistry
        .get(&transfer_syntax_uid)
        .with_context(|| format!("unsupported DICOM transfer syntax {transfer_syntax_uid}"))?;
    match transfer_syntax.codec() {
        // A deflated data set declares inflated lengths, which the file size
        // does not bound, so its values are read through to the end instead.
        Codec::Dataset(Some(adapter)) => {
            let decoder = DynStatefulDecoder::new_with_ts(
                adapter.adapt_reader(Box::new(reader)),
                transfer_syntax,
                0,
            )?;
            ensure_value_lengths_fit(&mut LazyDataSetReader::new(decoder), None).map(drop)
        }
        Codec::Dataset(None) => anyhow::bail!("unsupported DICOM data set encoding"),
        Codec::None | Codec::EncapsulatedPixelData(..) => {
            ensure_file_value_lengths_fit(reader, transfer_syntax)
        }
    }
}

/// Checks the data set of a file that is not deflated. Top-level pixel data
/// is passed by seeking, and the walk resumes after it.
fn ensure_file_value_lengths_fit(
    mut reader: BufReader<File>,
    transfer_syntax: &TransferSyntax,
) -> Result<()> {
    let file_length = reader.get_ref().metadata()?.len();
    loop {
        let mut tokens = LazyDataSetReader::new_with_ts(&mut reader, transfer_syntax)?;
        let pixel_data = ensure_value_lengths_fit(&mut tokens, Some(file_length))?;
        drop(tokens);
        let value_start = reader.stream_position()?;
        let value_end = match pixel_data {
            None => return Ok(()),
            Some(PixelDataHeader::Native { length }) => value_start + u64::from(length),
            Some(PixelDataHeader::Encapsulated) => {
                let items = read_pixel_data_items(reader.get_mut(), value_start, file_length)?;
                if let Some(overrun) = items.overrun {
                    anyhow::bail!("{overrun}");
                }
                items.sequence_end.context(
                    "encapsulated pixel data has no sequence delimiter before the next element",
                )?
            }
        };
        reader.seek(SeekFrom::Start(value_end))?;
    }
}

/// Checks each defined value length that `tokens` reads and skips the value.
/// With the `file_length` of a file, a value must fit in the bytes left in
/// the file, and the walk stops at the top-level PixelData, which it returns
/// for the caller to pass. Without one, the stream is inflated data, and each
/// value is read through and must not end the stream early.
fn ensure_value_lengths_fit<S>(
    tokens: &mut LazyDataSetReader<S>,
    file_length: Option<u64>,
) -> Result<Option<PixelDataHeader>>
where
    S: StatefulDecode,
{
    let mut sequence_depth = 0_usize;

    while let Some(token) = tokens.advance() {
        let (value_tag, value_length, decoder) = match token? {
            LazyDataToken::PixelSequenceStart if sequence_depth == 0 && file_length.is_some() => {
                return Ok(Some(PixelDataHeader::Encapsulated));
            }
            LazyDataToken::LazyValue { header, decoder } => {
                (Some(header.tag), header.len.0, decoder)
            }
            LazyDataToken::LazyItemValue { len, decoder } => (None, len, decoder),
            LazyDataToken::SequenceStart { .. } | LazyDataToken::PixelSequenceStart => {
                sequence_depth += 1;
                ensure_sequence_depth_allowed(sequence_depth)?;
                continue;
            }
            LazyDataToken::SequenceEnd => {
                sequence_depth = sequence_depth.saturating_sub(1);
                continue;
            }
            _ => continue,
        };
        let value_name = || match value_tag {
            Some(tag) => format!("DICOM element {tag}"),
            None => "an encapsulated pixel data item".to_owned(),
        };
        match file_length {
            Some(file_length) => {
                ensure_value_fits_file(value_length, decoder.position(), file_length, value_name)?;
                if sequence_depth == 0 && value_tag == Some(tags::PIXEL_DATA) {
                    return Ok(Some(PixelDataHeader::Native {
                        length: value_length,
                    }));
                }
                decoder.skip_bytes(value_length)?;
            }
            None => {
                // The count of inflated bytes decides, whether the copy stopped at
                // the end of the stream or at an inflate error: a stream that
                // breaks inside the value ends the data set there, and the full
                // open would still reserve the declared length before the break.
                let mut value_bytes = ByteCounter::default();
                let result = decoder.read_to(value_length, &mut value_bytes);
                if value_bytes.0 < u64::from(value_length) {
                    anyhow::bail!(
                        "{} declares {value_length} bytes, but only {} remain in the inflated data set",
                        value_name(),
                        value_bytes.0
                    );
                }
                result?;
            }
        }
    }
    Ok(None)
}

fn ensure_sequence_depth_allowed(sequence_depth: usize) -> Result<()> {
    ensure!(
        sequence_depth <= MAX_SEQUENCE_DEPTH,
        "DICOM sequences nest more than {MAX_SEQUENCE_DEPTH} levels deep"
    );
    Ok(())
}

/// Refuses a value of `value_length` bytes that starts at `position` and runs
/// past the end of a file of `file_length` bytes.
fn ensure_value_fits_file(
    value_length: u32,
    position: u64,
    file_length: u64,
    value_name: impl FnOnce() -> String,
) -> Result<()> {
    let remaining_length = file_length.saturating_sub(position);
    ensure!(
        u64::from(value_length) <= remaining_length,
        "{} declares {value_length} bytes, but only {remaining_length} remain in the file",
        value_name()
    );
    Ok(())
}

/// A writer that discards its bytes and counts them.
#[derive(Default)]
struct ByteCounter(u64);

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reads the item headers of the encapsulated pixel data that starts at
/// `start`, seeking past each item value instead of reading it. Reading stops
/// at the sequence delimiter, at the end of the file, at anything that is not
/// an item, or at the first item that does not fit in the file.
fn read_pixel_data_items(file: &mut File, start: u64, file_length: u64) -> Result<PixelDataItems> {
    const ITEM_TAG: [u8; 4] = [0xFE, 0xFF, 0x00, 0xE0];
    const SEQUENCE_DELIMITER_TAG: [u8; 4] = [0xFE, 0xFF, 0xDD, 0xE0];
    let mut position = start;
    let mut item_count = 0_u32;
    let mut item_header = [0_u8; 8];
    let mut sequence_end = None;

    let overrun = loop {
        file.seek(SeekFrom::Start(position))?;
        match file.read_exact(&mut item_header) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof && position == file_length => {
                sequence_end = Some(file_length);
                break None;
            }
            Err(error) => return Err(error.into()),
        }
        if item_header[..4] != ITEM_TAG {
            if item_header[..4] == SEQUENCE_DELIMITER_TAG {
                sequence_end = Some(position + item_header.len() as u64);
            }
            break None;
        }
        let declared_length = u32::from_le_bytes(item_header[4..].try_into()?);
        position += item_header.len() as u64;
        let remaining_length = file_length.saturating_sub(position);
        if u64::from(declared_length) > remaining_length {
            break Some(PixelItemOverrun {
                item_index: item_count,
                declared_length,
                remaining_length,
            });
        }
        position += u64::from(declared_length);
        item_count = item_count.saturating_add(1);
    };

    Ok(PixelDataItems {
        // The first item is the Basic Offset Table.
        fragment_count: item_count.saturating_sub(1),
        overrun,
        sequence_end,
    })
}

fn read_original_file_meta(file_path: &Path) -> Result<Option<InMemDicomObject>> {
    let mut reader = BufReader::new(File::open(file_path)?);
    let meta_offset = {
        let prefix = reader.fill_buf()?;
        file_meta_offset(prefix)
    };
    let Some(meta_offset) = meta_offset else {
        return Ok(None);
    };
    reader.consume(meta_offset);
    let meta_start = reader.stream_position()?;
    let mut group_length_element = [0_u8; 12];
    reader.read_exact(&mut group_length_element)?;
    ensure!(
        &group_length_element[..8] == b"\x02\x00\x00\x00UL\x04\x00",
        "invalid DICOM file meta group length element"
    );
    let group_length = u32::from_le_bytes(group_length_element[8..12].try_into()?);
    let meta_length = u64::from(group_length) + 12;
    let transfer_syntax = TransferSyntaxRegistry
        .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
        .context("missing file meta transfer syntax decoder")?;
    // dicom-object reads nested sequences recursively and sets aside each
    // value's declared length, which FileMetaTable does not do for the
    // values it skips. Anything the walk cannot read is left to that read.
    let file_length = reader.get_ref().metadata()?.len();
    reader.seek(SeekFrom::Start(meta_start))?;
    let decoder = DynStatefulDecoder::new_with_ts(
        (&mut reader).take(meta_length),
        transfer_syntax,
        meta_start,
    )?;
    ensure_value_lengths_fit(&mut LazyDataSetReader::new(decoder), Some(file_length))?;
    reader.seek(SeekFrom::Start(meta_start))?;
    let mut meta_reader = reader.take(meta_length);
    let original_meta = InMemDicomObject::read_dataset_with_ts(&mut meta_reader, transfer_syntax)?;
    ensure!(meta_reader.limit() == 0, "truncated DICOM file metadata");
    ensure!(
        original_meta.tags().all(|tag| tag.0 == 0x0002),
        "DICOM file meta group length includes dataset elements"
    );
    Ok(Some(original_meta))
}

/// Refuses a file meta group whose elements declare a value longer than the
/// bytes left in the file. FileMetaTable, which the scan and every open use,
/// sets aside the declared length of each value it keeps before it reads it,
/// so one corrupt length would make it zero-fill up to 4 GB. The walk reads
/// the element headers that FileMetaTable reads, from the "DICM" marker at
/// `meta_start`, the position of `reader`, to the end of the group, and
/// skips each value. Anything else it cannot read is left to FileMetaTable,
/// which reports it. `reader` is left where it was. The walk counts its
/// position and moves relative to it, because asking the file for its
/// position or seeking to an absolute one costs a system call, and the seek
/// also drops the buffer, which usually holds the whole group. For the same
/// reason it asks for the file length only for a value that runs past the
/// buffer.
fn ensure_file_meta_values_fit_file(reader: &mut BufReader<File>, meta_start: u64) -> Result<()> {
    let header_decoder = decode::file_header_decoder();
    reader.seek_relative(4)?;
    let mut position = meta_start + 4;
    let mut group_end = None;

    // The position after the walk, unless a failed read left it unknown.
    let walk_end = loop {
        let Ok((header, header_length)) = header_decoder.decode_header(reader) else {
            break None;
        };
        position += header_length as u64;
        let Some(value_length) = header.len.get() else {
            break Some(position);
        };
        let value_end = match group_end {
            // FileMetaTable reads the group length first and requires 4 bytes.
            None if header.tag == Tag(0x0002, 0x0000) && value_length == 4 => {
                let mut group_length = [0_u8; 4];
                if reader.read_exact(&mut group_length).is_err() {
                    break None;
                }
                position += 4;
                group_end = Some(position + u64::from(u32::from_le_bytes(group_length)));
                position
            }
            None => break Some(position),
            Some(_) => {
                // A value that ends inside the buffer ends inside the file.
                if u64::from(value_length) > reader.buffer().len() as u64 {
                    let file_length = reader.get_ref().metadata()?.len();
                    ensure_value_fits_file(value_length, position, file_length, || {
                        format!("DICOM element {}", header.tag)
                    })
                    .context("invalid DICOM file meta group")?;
                }
                position + u64::from(value_length)
            }
        };
        if group_end.is_some_and(|group_end| value_end >= group_end) {
            break Some(position);
        }
        if value_end > position {
            reader.seek_relative(i64::try_from(value_end - position)?)?;
            position = value_end;
        }
    };
    match walk_end {
        Some(walk_end) => reader.seek_relative(-i64::try_from(walk_end - meta_start)?)?,
        None => {
            reader.seek(SeekFrom::Start(meta_start))?;
        }
    }
    Ok(())
}

fn file_meta_offset(prefix: &[u8]) -> Option<usize> {
    if prefix.len() >= 132 && &prefix[128..132] == b"DICM" {
        Some(132)
    } else if prefix.starts_with(b"DICM") {
        Some(4)
    } else {
        None
    }
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
    let DatasetSource {
        reader,
        transfer_syntax_uid,
        ..
    } = open_dataset_source(file_path)?;
    let transfer_syntax = TransferSyntaxRegistry
        .get(&transfer_syntax_uid)
        .with_context(|| format!("unsupported DICOM transfer syntax {transfer_syntax_uid}"))?;
    match transfer_syntax.codec() {
        Codec::Dataset(Some(adapter)) => {
            let decoder = DynStatefulDecoder::new_with_ts(
                adapter.adapt_reader(Box::new(reader)),
                transfer_syntax,
                0,
            )?;
            probe_dataset(&mut LazyDataSetReader::new(decoder))
        }
        Codec::Dataset(None) => anyhow::bail!("unsupported DICOM data set encoding"),
        Codec::None | Codec::EncapsulatedPixelData(..) => probe_dataset(
            &mut LazyDataSetReader::new_with_ts(reader, transfer_syntax)?,
        ),
    }
}

fn is_detection_text(tag: Tag, vr: VR) -> bool {
    tag.0.is_multiple_of(2)
        && matches!(
            vr,
            VR::PN | VR::LO | VR::SH | VR::ST | VR::LT | VR::UT | VR::UC
        )
}

/// Reads only the bounded text values used to detect an undeclared character set.
fn probe_dataset<S>(tokens: &mut LazyDataSetReader<S>) -> Result<Option<TextEncoding>>
where
    S: StatefulDecode,
{
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
                ensure_sequence_depth_allowed(sequence_depth)?;
            }
            LazyDataToken::SequenceEnd => {
                sequence_depth = sequence_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    Ok(evidence.detect())
}

fn scan_dicom_file(file_path: &Path, text_encoding: TextEncoding) -> Result<ScannedDicomFile> {
    let assumed_charset = resolve_assumed_charset(file_path, text_encoding);
    let DatasetSource {
        reader,
        transfer_syntax_uid,
        ..
    } = open_dataset_source(file_path)?;
    let transfer_syntax = TransferSyntaxRegistry
        .get(&transfer_syntax_uid)
        .with_context(|| format!("unsupported DICOM transfer syntax {transfer_syntax_uid}"))?;
    let scanned_metadata = match transfer_syntax.codec() {
        // Deflated data sets can only be read forward once inflated, so the scan cannot seek to
        // measure their pixel data.
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
        scanned_metadata.pixel_data,
    );
    Ok(ScannedDicomFile { index_entry })
}

fn open_dataset_source(file_path: &Path) -> Result<DatasetSource> {
    let file = File::open(file_path)?;
    let mut reader = BufReader::with_capacity(SCAN_BUFFER_CAPACITY, file);
    let (preamble_length, raw_transfer_syntax_uid) = {
        let prefix = reader.fill_buf()?;
        match file_meta_offset(prefix) {
            Some(offset) => (offset - 4, None),
            None => (0, Some(raw_dataset_transfer_syntax_from_prefix(prefix)?)),
        }
    };
    let file_meta = match raw_transfer_syntax_uid {
        Some(_) => None,
        None => {
            reader.consume(preamble_length);
            ensure_file_meta_values_fit_file(&mut reader, preamble_length as u64)?;
            Some(FileMetaTable::from_reader(&mut reader)?)
        }
    };
    let transfer_syntax_uid = file_meta
        .as_ref()
        .map(FileMetaTable::transfer_syntax)
        .or(raw_transfer_syntax_uid)
        .context("DICOM source has no decoding transfer syntax")?
        .to_owned();
    Ok(DatasetSource {
        reader,
        transfer_syntax_uid,
        file_meta,
    })
}

fn scan_seekable_dataset(
    mut reader: BufReader<File>,
    transfer_syntax: &TransferSyntax,
    assumed_charset: SpecificCharacterSet,
) -> Result<ScannedIndexMetadata> {
    let file_length = reader.get_ref().metadata()?.len();
    let mut tokens =
        LazyDataSetReader::new_with_ts_cs(&mut reader, transfer_syntax, assumed_charset)?;
    let (metadata, pixel_data_header) = read_index_metadata(&mut tokens, Some(file_length))?;
    drop(tokens);

    let pixel_data = match pixel_data_header {
        Some(header) => Some(measure_pixel_data(&mut reader, header)?),
        None => None,
    };
    Ok(ScannedIndexMetadata {
        metadata,
        pixel_data,
    })
}

/// How much of the PixelData value whose header `reader` has just read is
/// present in the file.
fn measure_pixel_data(
    reader: &mut BufReader<File>,
    header: PixelDataHeader,
) -> Result<PixelDataExtent> {
    let value_start = reader.stream_position()?;
    let file_length = reader.get_ref().metadata()?.len();
    Ok(match header {
        PixelDataHeader::Native { length } => PixelDataExtent::Native {
            byte_count: u64::from(length).min(file_length.saturating_sub(value_start)),
        },
        PixelDataHeader::Encapsulated => PixelDataExtent::Encapsulated {
            fragment_count: read_pixel_data_items(reader.get_mut(), value_start, file_length)?
                .fragment_count,
        },
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
    let decoder = DynStatefulDecoder::new_with(reader, transfer_syntax, assumed_charset, 0)?;
    let (metadata, pixel_data_header) =
        read_index_metadata(&mut LazyDataSetReader::new(decoder), None)?;

    Ok(ScannedIndexMetadata {
        metadata,
        pixel_data: pixel_data_header.map(|_| PixelDataExtent::Unmeasured),
    })
}

/// Reads the indexed values of the top-level data set up to its PixelData,
/// which it returns, and skips every other value. dicom-parser sets aside a
/// value's declared length before it reads the value, so a value longer than
/// MAX_INDEXED_VALUE_BYTES is skipped, and with the `file_length` of a file,
/// a value that runs past the end of the file refuses the file, as the full
/// open does.
fn read_index_metadata<S>(
    tokens: &mut LazyDataSetReader<S>,
    file_length: Option<u64>,
) -> Result<(DicomIndexMetadata, Option<PixelDataHeader>)>
where
    S: StatefulDecode,
{
    let mut metadata = DicomIndexMetadata::default();
    let mut sequence_depth = 0_usize;

    while let Some(token) = tokens.advance() {
        match token? {
            LazyDataToken::ElementHeader(header)
                if sequence_depth == 0 && header.tag == tags::PIXEL_DATA =>
            {
                let pixel_data = (header.len != Length(0)).then_some(PixelDataHeader::Native {
                    length: header.len.0,
                });
                return Ok((metadata, pixel_data));
            }
            LazyDataToken::PixelSequenceStart if sequence_depth == 0 => {
                return Ok((metadata, Some(PixelDataHeader::Encapsulated)));
            }
            LazyDataToken::LazyValue { header, decoder }
                if sequence_depth == 0
                    && (DicomIndexMetadata::includes_tag(header.tag)
                        || header.tag == tags::SPECIFIC_CHARACTER_SET) =>
            {
                let value_name = || format!("DICOM element {}", header.tag);
                if let Some(file_length) = file_length {
                    ensure_value_fits_file(
                        header.len.0,
                        decoder.position(),
                        file_length,
                        value_name,
                    )?;
                }
                if header.len.0 > MAX_INDEXED_VALUE_BYTES {
                    decoder.skip_bytes(header.len.0)?;
                    continue;
                }
                let value = decoder
                    .read_value_preserved(&header)
                    .with_context(|| format!("could not read {}", value_name()))?;
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

    Ok((metadata, None))
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
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use dicom_core::{DataElement, PrimitiveValue, Tag, VR, value::PixelFragmentSequence};
    use dicom_dictionary_std::{tags, uids};
    use dicom_encoding::text::TextCodec;
    use dicom_object::{FileDicomObject, FileMetaTableBuilder, InMemDicomObject};
    use dicom_transfer_syntax_registry::{TransferSyntaxIndex, TransferSyntaxRegistry};

    use super::{
        TextEncoding, build_for_file, build_for_file_with_encoding, build_for_inputs_with_progress,
        build_for_inputs_with_progress_with_encoding, detect_missing_charset_encoding,
        open_dicom_file, scan_worker_count_for,
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

        for transfer_syntax_uid in [
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            uids::IMPLICIT_VR_LITTLE_ENDIAN,
            super::EXPLICIT_VR_BIG_ENDIAN_UID,
        ] {
            let transfer_syntax = TransferSyntaxRegistry.get(transfer_syntax_uid).unwrap();
            object
                .write_dataset_with_ts(File::create(&raw_path).unwrap(), transfer_syntax)
                .unwrap();
            let original_bytes = std::fs::read(&raw_path).unwrap();

            for encoding in [
                TextEncoding::DicomDefault,
                TextEncoding::Auto,
                TextEncoding::KoreanEucKr,
                TextEncoding::Utf8,
                TextEncoding::JapaneseShiftJis,
            ] {
                let index = build_for_file_with_encoding(&raw_path, encoding).unwrap();
                let loaded = if encoding == TextEncoding::DicomDefault {
                    crate::dicom::load_dicom_frame(&raw_path, 0).unwrap()
                } else {
                    crate::dicom::load_dicom_frame_with_encoding(&raw_path, 0, encoding).unwrap()
                };

                assert_eq!(index.total_file_count, 1);
                assert_eq!(loaded.frame.frame_count, 1);
                assert_eq!(
                    loaded.frame.pixel_probe(0, 0),
                    Some(crate::dicom::PixelProbeValue::Monochrome(0.0))
                );
                assert!(
                    loaded
                        .metadata
                        .all_items
                        .iter()
                        .all(|item| !item.tag.starts_with("(0002,")),
                    "generated file metadata leaked for {transfer_syntax_uid}, {encoding:?}"
                );
                for tag in ["(0002,0010)", "(0002,0002)", "(0002,0003)"] {
                    let item = loaded
                        .metadata
                        .curated_items
                        .iter()
                        .find(|item| item.tag == tag)
                        .unwrap();
                    assert_eq!(
                        item.value, "-",
                        "{tag}, {transfer_syntax_uid}, {encoding:?}"
                    );
                }
                assert!(
                    loaded
                        .metadata
                        .all_items
                        .iter()
                        .any(|item| item.tag == "(0008,0016)")
                );
            }
            crate::dicom::load_dicom_thumbnail(&raw_path, 0, 64).unwrap();
            assert_eq!(std::fs::read(&raw_path).unwrap(), original_bytes);
        }
        std::fs::remove_file(part_10_path).unwrap();
        std::fs::remove_file(raw_path).unwrap();
    }

    #[test]
    fn loading_keeps_original_file_metadata_and_source_bytes() {
        let path = temporary_file_path("original-file-metadata");
        write_test_object(&path, true);
        let original_bytes = std::fs::read(&path).unwrap();

        for encoding in [
            TextEncoding::DicomDefault,
            TextEncoding::Auto,
            TextEncoding::KoreanEucKr,
            TextEncoding::Utf8,
            TextEncoding::JapaneseShiftJis,
        ] {
            let loaded = crate::dicom::load_dicom_frame_with_encoding(&path, 0, encoding).unwrap();
            for items in [&loaded.metadata.curated_items, &loaded.metadata.all_items] {
                for (tag, value) in [
                    (
                        "(0002,0010)",
                        "Explicit VR Little Endian (1.2.840.10008.1.2.1)",
                    ),
                    (
                        "(0002,0002)",
                        "CT Image Storage (1.2.840.10008.5.1.4.1.1.2)",
                    ),
                    ("(0002,0003)", "2.25.500"),
                ] {
                    let item = items.iter().find(|item| item.tag == tag).unwrap();
                    assert_eq!(item.value, value, "{tag}, {encoding:?}");
                }
            }
        }

        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn incomplete_file_headers_keep_only_their_original_elements_and_values() {
        let path = temporary_file_path("incomplete-file-header");
        write_test_object(&path, true);
        let mut object = dicom_object::open_file(&path).unwrap();
        for (tag, vr) in [
            (tags::PATIENT_NAME, VR::PN),
            (tags::SPECIFIC_CHARACTER_SET, VR::CS),
            (tags::NUMBER_OF_FRAMES, VR::IS),
            (tags::SLICE_LOCATION, VR::DS),
            (tags::RESCALE_SLOPE, VR::DS),
            (tags::RESCALE_INTERCEPT, VR::DS),
            (tags::WINDOW_CENTER, VR::DS),
            (tags::WINDOW_WIDTH, VR::DS),
            (tags::VOILUT_FUNCTION, VR::CS),
        ] {
            object.put_element(DataElement::empty(tag, vr));
        }
        object.put_element(DataElement::new(
            tags::IMAGE_POSITION_PATIENT,
            VR::DS,
            "0\\0\\42",
        ));
        let transfer_syntax = TransferSyntaxRegistry
            .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .unwrap();
        let mut dataset_bytes = Vec::new();
        object.write_dataset(&mut dataset_bytes).unwrap();

        for (empty_fields, include_preamble) in [(false, true), (true, true), (false, false)] {
            let mut original_meta = InMemDicomObject::from_element_iter([
                DataElement::new(
                    tags::TRANSFER_SYNTAX_UID,
                    VR::UI,
                    uids::EXPLICIT_VR_LITTLE_ENDIAN,
                ),
                DataElement::new(tags::IMPLEMENTATION_VERSION_NAME, VR::SH, "SOURCE_V1"),
                DataElement::empty(tags::SOURCE_APPLICATION_ENTITY_TITLE, VR::AE),
                // FileMetaTable currently discards this standard header tag.
                DataElement::new(Tag(0x0002, 0x0026), VR::UR, "dicom://source.example"),
            ]);
            if empty_fields {
                for tag in [
                    tags::MEDIA_STORAGE_SOP_CLASS_UID,
                    tags::MEDIA_STORAGE_SOP_INSTANCE_UID,
                    tags::IMPLEMENTATION_CLASS_UID,
                ] {
                    original_meta.put_element(DataElement::empty(tag, VR::UI));
                }
            }
            let mut meta_bytes = Vec::new();
            original_meta
                .write_dataset_with_ts(&mut meta_bytes, transfer_syntax)
                .unwrap();
            let group_length = u32::try_from(meta_bytes.len()).unwrap();
            let mut bytes = if include_preamble {
                vec![0; 128]
            } else {
                Vec::new()
            };
            bytes.extend_from_slice(b"DICM\x02\x00\x00\x00UL\x04\x00");
            bytes.extend_from_slice(&group_length.to_le_bytes());
            bytes.extend_from_slice(&meta_bytes);
            bytes.extend_from_slice(&dataset_bytes);
            std::fs::write(&path, &bytes).unwrap();

            let mut expected_tags: Vec<_> =
                std::iter::once(tags::FILE_META_INFORMATION_GROUP_LENGTH)
                    .chain(original_meta.tags())
                    .chain(object.tags())
                    .map(|tag| tag.to_string())
                    .collect();
            expected_tags.sort();

            for encoding in [
                TextEncoding::DicomDefault,
                TextEncoding::Auto,
                TextEncoding::KoreanEucKr,
                TextEncoding::Utf8,
                TextEncoding::JapaneseShiftJis,
            ] {
                let loaded =
                    crate::dicom::load_dicom_frame_with_encoding(&path, 0, encoding).unwrap();
                let actual_tags: Vec<_> = loaded
                    .metadata
                    .all_items
                    .iter()
                    .map(|item| item.tag.clone())
                    .collect();
                assert_eq!(
                    actual_tags, expected_tags,
                    "{empty_fields}, {include_preamble}, {encoding:?}"
                );
                for (tag, value) in [
                    ("(0002,0000)", group_length.to_string()),
                    ("(0002,0013)", "SOURCE_V1".to_owned()),
                    ("(0002,0016)", "-".to_owned()),
                    ("(0002,0026)", "dicom://source.example".to_owned()),
                    ("(0008,0005)", "-".to_owned()),
                    ("(0010,0010)", "-".to_owned()),
                    ("(0020,1041)", "-".to_owned()),
                    ("(0028,0008)", "-".to_owned()),
                    ("(0028,1050)", "-".to_owned()),
                    ("(0028,1051)", "-".to_owned()),
                    ("(0028,1052)", "-".to_owned()),
                    ("(0028,1053)", "-".to_owned()),
                    ("(0028,1056)", "-".to_owned()),
                ] {
                    let item = loaded
                        .metadata
                        .all_items
                        .iter()
                        .find(|item| item.tag == tag)
                        .unwrap();
                    assert_eq!(item.value, value, "{tag}, {encoding:?}");
                }
                assert!(loaded.metadata.overlay.patient_label.is_none());
                assert!(loaded.metadata.overlay.slice_location.is_none());
                assert_eq!(loaded.frame.frame_count, 1);
                for tag in ["(0002,0002)", "(0002,0003)"] {
                    let item = loaded
                        .metadata
                        .curated_items
                        .iter()
                        .find(|item| item.tag == tag)
                        .unwrap();
                    assert_eq!(item.value, "-");
                }
            }
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn missing_or_empty_required_pixel_attributes_are_rejected_without_filling_them() {
        let path = temporary_file_path("missing-required-pixel-attributes");
        write_test_object(&path, true);
        let original = dicom_object::open_file(&path).unwrap();
        for tag in [
            tags::ROWS,
            tags::COLUMNS,
            tags::SAMPLES_PER_PIXEL,
            tags::PHOTOMETRIC_INTERPRETATION,
            tags::BITS_ALLOCATED,
            tags::BITS_STORED,
            tags::HIGH_BIT,
            tags::PIXEL_REPRESENTATION,
        ] {
            for empty in [false, true] {
                let mut object = original.clone();
                if empty {
                    let vr = object.element(tag).unwrap().vr();
                    object.put_element(DataElement::empty(tag, vr));
                } else {
                    object.remove_element(tag);
                }
                object.write_to_file(&path).unwrap();
                let source_bytes = std::fs::read(&path).unwrap();
                assert!(
                    crate::dicom::load_dicom_frame(&path, 0).is_err(),
                    "{tag}, empty: {empty}"
                );
                assert_eq!(std::fs::read(&path).unwrap(), source_bytes);
            }
        }
        std::fs::remove_file(path).unwrap();
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
    fn series_uses_numeric_instance_order_when_slice_positions_disagree() {
        let paths =
            ["instance-order-a", "instance-order-b", "instance-order-c"].map(temporary_file_path);
        for (path, instance_number, position) in [
            (&paths[0], "12", "0\\0\\-33"),
            (&paths[1], "1", "0\\0\\0"),
            (&paths[2], "2", "0\\0\\1"),
        ] {
            write_test_object(path, true);
            let mut object = dicom_object::open_file(path).unwrap();
            for (tag, vr, value) in [
                (tags::PATIENT_ID, VR::LO, "instance-order-patient"),
                (tags::STUDY_INSTANCE_UID, VR::UI, "2.25.711"),
                (tags::SERIES_INSTANCE_UID, VR::UI, "2.25.712"),
                (tags::INSTANCE_NUMBER, VR::IS, instance_number),
                (tags::IMAGE_POSITION_PATIENT, VR::DS, position),
                (tags::IMAGE_ORIENTATION_PATIENT, VR::DS, "1\\0\\0\\0\\1\\0"),
            ] {
                object.put_element(DataElement::new(tag, vr, PrimitiveValue::from(value)));
            }
            object.write_to_file(path).unwrap();
        }

        let index =
            build_for_inputs_with_progress(&paths, &AtomicBool::new(false), |_| {}).unwrap();
        for path in &paths {
            std::fs::remove_file(path).unwrap();
        }
        assert_eq!(index.total_file_count, 3);
        let slices = &index.patients[0].studies[0].series_groups[0].slices;
        assert_eq!(
            slices
                .iter()
                .map(|slice| slice.instance_number)
                .collect::<Vec<_>>(),
            [Some(1), Some(2), Some(12)]
        );
        assert_eq!(slices[0].path, paths[1]);
    }

    #[test]
    fn series_uses_creation_time_when_instance_number_is_missing_or_empty() {
        // Creation time must beat both filename and physical position, while
        // numbered images keep their numeric order ahead of unnumbered ones.
        let cases = [
            (None, Some("101112.9"), "-10"),
            (Some(""), Some("101112.10"), "10"),
            (None, Some("101112.100000"), "0"),
            (Some(""), Some("101112.000001"), "50"),
            (None, Some("101112"), "100"),
            (Some(""), Some(""), "-30"),
            (None, None, "-20"),
            (Some(""), Some("not-a-time"), "-40"),
            (None, Some("10"), "999"),
            (Some("12"), Some("000000"), "0"),
            (Some("2"), Some("120000"), "0"),
            (Some("1"), Some("235959"), "0"),
        ];
        let paths: Vec<_> = (0..cases.len())
            .map(|index| temporary_file_path(&format!("creation-time-{index}")))
            .collect();

        // Exercise both the lazy header scan and the streaming deflate path.
        for transfer_syntax in [
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN,
        ] {
            for (path, (instance_number, creation_time, position)) in paths.iter().zip(cases) {
                write_test_object(path, true);
                let mut object = dicom_object::open_file(path).unwrap();
                for (tag, vr, value) in [
                    (tags::PATIENT_ID, VR::LO, "creation-time-patient"),
                    (tags::STUDY_INSTANCE_UID, VR::UI, "2.25.721"),
                    (tags::SERIES_INSTANCE_UID, VR::UI, "2.25.722"),
                ] {
                    object.put_element(DataElement::new(tag, vr, value));
                }
                for (tag, vr, value) in [
                    (tags::INSTANCE_NUMBER, VR::IS, instance_number),
                    (tags::INSTANCE_CREATION_TIME, VR::TM, creation_time),
                ] {
                    if let Some(value) = value {
                        object.put_element(DataElement::new(tag, vr, value));
                    }
                }
                object.put_element(DataElement::new(
                    tags::IMAGE_POSITION_PATIENT,
                    VR::DS,
                    format!("0\\0\\{position}"),
                ));
                object.update_meta(|meta| {
                    meta.transfer_syntax = transfer_syntax.to_owned();
                });
                object.write_to_file(path).unwrap();
            }

            let index =
                build_for_inputs_with_progress(&paths, &AtomicBool::new(false), |_| {}).unwrap();
            let slices = &index.patients[0].studies[0].series_groups[0].slices;
            let actual: Vec<_> = slices.iter().map(|slice| &slice.path).collect();
            let expected = [11, 10, 9, 8, 4, 3, 2, 1, 0, 7, 5, 6].map(|index| &paths[index]);
            assert_eq!(actual, expected, "transfer syntax: {transfer_syntax}");
        }
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
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

    fn indexed_slice_count(path: &Path) -> usize {
        build_for_file(path).unwrap().patients[0].studies[0].series_groups[0]
            .slices
            .len()
    }

    /// Writes the 1x1 test image with its frame size, frame count and native
    /// pixel data replaced.
    fn write_native_frames(path: &Path, side: u16, number_of_frames: &str, pixel_data: Vec<u8>) {
        write_test_object(path, true);
        let mut object = dicom_object::open_file(path).unwrap();
        for tag in [tags::ROWS, tags::COLUMNS] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(side)));
        }
        object.put_element(DataElement::new(
            tags::NUMBER_OF_FRAMES,
            VR::IS,
            PrimitiveValue::from(number_of_frames),
        ));
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from(pixel_data),
        ));
        object.write_to_file(path).unwrap();
    }

    /// Writes the test image as JPEG Baseline with these fragments and frame
    /// count, and returns the file's bytes.
    fn write_encapsulated_frames(
        path: &Path,
        number_of_frames: &str,
        fragments: Vec<Vec<u8>>,
    ) -> Vec<u8> {
        write_test_object(path, true);
        let mut object = dicom_object::open_file(path).unwrap();
        object.update_meta(|meta| {
            meta.transfer_syntax = uids::JPEG_BASELINE8_BIT.to_owned();
        });
        object.put_element(DataElement::new(
            tags::NUMBER_OF_FRAMES,
            VR::IS,
            PrimitiveValue::from(number_of_frames),
        ));
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new_fragments(fragments),
        ));
        object.write_to_file(path).unwrap();
        std::fs::read(path).unwrap()
    }

    /// Where the length of the first item of the encapsulated pixel data, the
    /// Basic Offset Table, is stored in `bytes`.
    fn basic_offset_table_length_offset(bytes: &[u8]) -> usize {
        let pixel_sequence_start = [
            0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE, 0xFF, 0x00,
            0xE0,
        ];
        bytes
            .windows(pixel_sequence_start.len())
            .position(|window| window == pixel_sequence_start)
            .unwrap()
            + pixel_sequence_start.len()
    }

    /// A corrupt NumberOfFrames used to add one index entry per declared frame,
    /// a million here, for a file whose pixel data holds two.
    #[test]
    fn native_pixel_data_bounds_a_corrupt_frame_count() {
        let path = temporary_file_path("native-frame-count");
        write_native_frames(&path, 1, "1000000", vec![0, 0]);

        let slice_count = indexed_slice_count(&path);

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 2);
    }

    #[test]
    fn native_multiframe_files_index_every_declared_frame() {
        let path = temporary_file_path("native-three-frames");
        write_native_frames(&path, 2, "3", vec![0; 12]);

        let index = build_for_file(&path).unwrap();
        let loaded = crate::dicom::load_dicom_frame(&path, 2);

        std::fs::remove_file(path).unwrap();
        let slices = &index.patients[0].studies[0].series_groups[0].slices;
        assert_eq!(
            slices
                .iter()
                .map(|slice| slice.frame_index)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(loaded.unwrap().frame.frame_count, 3);
    }

    /// Every encapsulated frame starts a fragment, so two fragments hold at
    /// most two frames.
    #[test]
    fn encapsulated_fragments_bound_a_corrupt_frame_count() {
        let path = temporary_file_path("encapsulated-frame-count");
        let fragment = vec![0xFF, 0xD8, 0xFF, 0xD9];
        write_encapsulated_frames(&path, "1000000", vec![fragment.clone(), fragment]);

        let slice_count = indexed_slice_count(&path);

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 2);
    }

    /// The full open used to set aside four times the declared Basic Offset
    /// Table length, about 1.2 GB here, before failing at the end of the file.
    #[test]
    fn a_corrupt_basic_offset_table_length_is_refused_before_the_full_open() {
        let path = temporary_file_path("corrupt-basic-offset-table");
        let mut bytes = write_encapsulated_frames(&path, "1", vec![vec![0xFF, 0xD8, 0xFF, 0xD9]]);
        let length_offset = basic_offset_table_length_offset(&bytes);
        bytes[length_offset..length_offset + 4].copy_from_slice(&0x1200_0000_u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let remaining_length = bytes.len() - length_offset - 4;

        let slice_count = indexed_slice_count(&path);
        let error = open_dicom_file(&path).err().unwrap();

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 1);
        assert_eq!(
            error.to_string(),
            format!(
                "the Basic Offset Table of the pixel data declares 301989888 bytes, \
                 but only {remaining_length} remain in the file"
            )
        );
    }

    #[test]
    fn a_truncated_last_fragment_is_refused_by_the_full_open() {
        let path = temporary_file_path("truncated-fragment");
        let fragment = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let bytes = write_encapsulated_frames(&path, "2", vec![fragment.clone(), fragment]);
        // Drop the sequence delimiter and the last two bytes of the second fragment.
        std::fs::write(&path, &bytes[..bytes.len() - 10]).unwrap();

        let slice_count = indexed_slice_count(&path);
        let error = open_dicom_file(&path).err().unwrap();

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 1);
        assert_eq!(
            error.to_string(),
            "pixel data fragment 2 declares 4 bytes, but only 2 remain in the file"
        );
    }

    #[test]
    fn an_element_longer_than_the_file_is_refused_before_the_full_open() {
        let path = temporary_file_path("native-pixel-data-length");
        write_native_frames(&path, 1, "1", vec![0, 0]);
        let mut bytes = std::fs::read(&path).unwrap();
        let pixel_data_header = [0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 2, 0, 0, 0];
        let length_offset = bytes
            .windows(pixel_data_header.len())
            .position(|window| window == pixel_data_header)
            .unwrap()
            + 8;
        bytes[length_offset..length_offset + 4].copy_from_slice(&0x0100_0000_u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let slice_count = indexed_slice_count(&path);
        let error = open_dicom_file(&path).err().unwrap();

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 1);
        assert_eq!(
            error.to_string(),
            "DICOM element (7FE0,0010) declares 16777216 bytes, but only 2 remain in the file"
        );
    }

    /// dicom-parser sets aside a value's declared length before it reads the
    /// value, so the header scan used to zero-fill 4 GB for this PatientName
    /// before the read failed at the end of the file.
    #[test]
    fn an_indexed_value_longer_than_the_file_is_refused_without_reading_it() {
        let path = temporary_file_path("indexed-value-length");
        write_test_object(&path, true);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            tags::PATIENT_NAME,
            VR::PN,
            PrimitiveValue::from("Doe^Jane"),
        ));
        object
            .update_meta(|meta| meta.transfer_syntax = uids::IMPLICIT_VR_LITTLE_ENDIAN.to_owned());
        object.write_to_file(&path).unwrap();
        patch_length(&path, &[0x10, 0x00, 0x10, 0x00, 8, 0, 0, 0], 4, 0xFFFF_FFF0);

        let scan_error = build_for_file(&path).err().unwrap().to_string();
        let open_error = adversarial_open_error(&path);

        assert!(
            scan_error
                .starts_with("DICOM element (0010,0010) declares 4294967280 bytes, but only ")
                && scan_error.ends_with(" remain in the file"),
            "{scan_error}"
        );
        assert_eq!(scan_error, open_error);
    }

    #[test]
    fn indexed_values_longer_than_the_cap_are_left_out_of_the_index() {
        let cap = super::MAX_INDEXED_VALUE_BYTES as usize;
        for deflated in [false, true] {
            for (name_length, expected_display_name) in [
                (cap, format!("{} (patient-7)", "A".repeat(cap))),
                // PN values have even lengths, so this is the next one over the cap.
                (cap + 2, "patient-7".to_owned()),
            ] {
                let path = temporary_file_path("indexed-value-cap");
                write_test_object(&path, true);
                let mut object = dicom_object::open_file(&path).unwrap();
                for (tag, vr, value) in [
                    (tags::PATIENT_ID, VR::LO, "patient-7".to_owned()),
                    (tags::PATIENT_NAME, VR::PN, "A".repeat(name_length)),
                ] {
                    object.put_element(DataElement::new(tag, vr, PrimitiveValue::from(value)));
                }
                object.write_to_file(&path).unwrap();
                if deflated {
                    rewrite_as_deflated(&path);
                }

                let index = build_for_file(&path);

                std::fs::remove_file(path).unwrap();
                let display_name = &index.unwrap().patients[0].display_name;
                assert!(
                    *display_name == expected_display_name,
                    "deflated {deflated}, name of {name_length} bytes: shown {} bytes",
                    display_name.len()
                );
            }
        }
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

    /// Overwrites the four length bytes that follow the first `pattern` in the
    /// file at `path`, `offset` bytes into the pattern.
    fn patch_length(path: &Path, pattern: &[u8], offset: usize, length: u32) {
        let mut bytes = std::fs::read(path).unwrap();
        let start = bytes
            .windows(pattern.len())
            .position(|window| window == pattern)
            .unwrap()
            + offset;
        bytes[start..start + 4].copy_from_slice(&length.to_le_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    fn adversarial_open_error(path: &Path) -> String {
        let error = open_dicom_file(path).err().unwrap().to_string();
        std::fs::remove_file(path).unwrap();
        error
    }

    /// Writes the native test image with one sequence item before PixelData
    /// that holds `item_elements`.
    fn write_native_frames_with_item(
        path: &Path,
        item_elements: Vec<DataElement<InMemDicomObject>>,
    ) {
        write_native_frames(path, 1, "1", vec![0, 0]);
        let mut object = dicom_object::open_file(path).unwrap();
        object.put_element(DataElement::new(
            Tag(0x0088, 0x0200),
            VR::SQ,
            dicom_core::value::DataSetSequence::from(vec![InMemDicomObject::from_element_iter(
                item_elements,
            )]),
        ));
        object.write_to_file(path).unwrap();
    }

    #[test]
    fn an_element_inside_a_sequence_longer_than_the_file_is_refused() {
        let path = temporary_file_path("adv-nested-element");
        write_native_frames_with_item(
            &path,
            vec![DataElement::new(
                Tag(0x0009, 0x1010),
                VR::OB,
                PrimitiveValue::from(vec![0xAB_u8, 0xCD, 0xEF, 0x01]),
            )],
        );
        patch_length(
            &path,
            &[0x09, 0x00, 0x10, 0x10, b'O', b'B', 0, 0],
            8,
            0x0100_0000,
        );

        let error = adversarial_open_error(&path);

        assert!(
            error.starts_with("DICOM element (0009,1010) declares 16777216 bytes, but only "),
            "{error}"
        );
    }

    /// A PixelData inside a sequence item, such as an icon image after a
    /// nested sequence, is not the end of the walk: the image's own PixelData
    /// is still checked.
    #[test]
    fn a_nested_pixel_data_does_not_end_the_length_check() {
        let path = temporary_file_path("adv-icon-then-long-pixel-data");
        write_native_frames_with_item(
            &path,
            vec![
                DataElement::new(
                    Tag(0x0008, 0x1140),
                    VR::SQ,
                    dicom_core::value::DataSetSequence::from(vec![
                        InMemDicomObject::from_element_iter([DataElement::new(
                            Tag(0x0008, 0x1150),
                            VR::UI,
                            PrimitiveValue::from("1.2"),
                        )]),
                    ]),
                ),
                DataElement::new(
                    tags::PIXEL_DATA,
                    VR::OB,
                    PrimitiveValue::from(vec![0x5A_u8, 0x5A]),
                ),
            ],
        );
        // The top-level PixelData is the last one in the file.
        let bytes = std::fs::read(&path).unwrap();
        let header = [0xE0, 0x7F, 0x10, 0x00, b'O', b'B', 0, 0, 2, 0, 0, 0];
        let last = bytes
            .windows(header.len())
            .rposition(|window| window == header)
            .unwrap();
        let mut bytes = bytes;
        bytes[last + 8..last + 12].copy_from_slice(&0x0100_0000_u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let error = adversarial_open_error(&path);

        assert_eq!(
            error,
            "DICOM element (7FE0,0010) declares 16777216 bytes, but only 2 remain in the file"
        );
    }

    #[test]
    fn a_nested_encapsulated_item_longer_than_the_file_is_refused() {
        let path = temporary_file_path("adv-nested-fragment");
        write_native_frames_with_item(
            &path,
            vec![DataElement::new(
                tags::PIXEL_DATA,
                VR::OB,
                PixelFragmentSequence::new_fragments(vec![vec![0xAB_u8, 0xCD, 0xEF, 0x01]]),
            )],
        );
        patch_length(
            &path,
            &[0xFE, 0xFF, 0x00, 0xE0, 4, 0, 0, 0, 0xAB, 0xCD, 0xEF, 0x01],
            4,
            0x0100_0000,
        );

        let error = adversarial_open_error(&path);

        assert!(
            error.starts_with("an encapsulated pixel data item declares 16777216 bytes, but only "),
            "{error}"
        );
    }

    /// A sequence before the pixel data leaves the walk at the top level, so
    /// the Basic Offset Table is read as the pixel data's own.
    #[test]
    fn a_sequence_before_a_corrupt_basic_offset_table_does_not_hide_it() {
        let path = temporary_file_path("adv-sequence-then-bot");
        write_encapsulated_frames(&path, "1", vec![vec![0xFF, 0xD8, 0xFF, 0xD9]]);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            Tag(0x0008, 0x1140),
            VR::SQ,
            dicom_core::value::DataSetSequence::from(vec![InMemDicomObject::new_empty()]),
        ));
        object.write_to_file(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let length_offset = basic_offset_table_length_offset(&bytes);
        bytes[length_offset..length_offset + 4].copy_from_slice(&0x1200_0000_u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let error = adversarial_open_error(&path);

        assert!(
            error.starts_with(
                "the Basic Offset Table of the pixel data declares 301989888 bytes, but only "
            ),
            "{error}"
        );
    }

    /// Without its sequence delimiter the pixel data ends with the file, and
    /// a last fragment that ends exactly there is whole.
    #[test]
    fn encapsulated_pixel_data_ending_with_the_file_keeps_its_frames() {
        let path = temporary_file_path("adv-no-sequence-delimiter");
        let fragment = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let bytes = write_encapsulated_frames(&path, "2", vec![fragment.clone(), fragment]);
        std::fs::write(&path, &bytes[..bytes.len() - 8]).unwrap();

        let slice_count = indexed_slice_count(&path);
        let opened = open_dicom_file(&path);

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 2);
        assert!(opened.is_ok(), "{:?}", opened.err());
    }

    /// The full open reads the elements after PixelData too, and sets aside
    /// each one's declared length before reading it: 0xF000_0000 here makes
    /// it request a 4 GB buffer.
    #[test]
    fn an_element_after_native_pixel_data_longer_than_the_file_is_refused() {
        let path = temporary_file_path("adv-trailing-element");
        write_native_frames(&path, 1, "1", vec![0, 0]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&[0xFC, 0xFF, 0xFC, 0xFF, b'O', b'B', 0, 0]);
        bytes.extend_from_slice(&0x0100_0000_u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let error = adversarial_open_error(&path);

        assert_eq!(
            error,
            "DICOM element (FFFC,FFFC) declares 16777216 bytes, but only 0 remain in the file"
        );
    }

    #[test]
    fn an_element_after_encapsulated_pixel_data_longer_than_the_file_is_refused() {
        let path = temporary_file_path("adv-trailing-element-encapsulated");
        let mut bytes = write_encapsulated_frames(&path, "1", vec![vec![0xFF, 0xD8, 0xFF, 0xD9]]);
        bytes.extend_from_slice(&[0xFC, 0xFF, 0xFC, 0xFF, b'O', b'B', 0, 0]);
        bytes.extend_from_slice(&0x0100_0000_u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let error = adversarial_open_error(&path);

        assert_eq!(
            error,
            "DICOM element (FFFC,FFFC) declares 16777216 bytes, but only 0 remain in the file"
        );
    }

    /// A valid file with trailing padding after its pixel data keeps its frames
    /// and still opens.
    #[test]
    fn trailing_padding_after_pixel_data_changes_nothing() {
        let path = temporary_file_path("adv-trailing-padding");
        write_native_frames(&path, 2, "3", vec![0; 12]);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            Tag(0xFFFC, 0xFFFC),
            VR::OB,
            PrimitiveValue::from(vec![0_u8; 6]),
        ));
        object.write_to_file(&path).unwrap();

        let slice_count = indexed_slice_count(&path);
        let opened = open_dicom_file(&path);

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 3);
        assert!(opened.is_ok(), "{:?}", opened.err());
    }

    /// The native bound counts the bytes present, not the declared length.
    #[test]
    fn native_pixel_data_cut_short_bounds_the_frames_by_the_bytes_present() {
        let path = temporary_file_path("adv-native-cut-short");
        write_native_frames(&path, 1, "1000", vec![0; 1000]);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 997]).unwrap();

        let slice_count = indexed_slice_count(&path);

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 3);
    }

    /// Every sample of every pixel counts: two RGB frames are six bytes.
    #[test]
    fn native_color_frames_are_bounded_by_their_samples() {
        let path = temporary_file_path("adv-native-rgb");
        write_native_frames(&path, 1, "5", vec![0; 7]);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            tags::SAMPLES_PER_PIXEL,
            VR::US,
            PrimitiveValue::from(3_u16),
        ));
        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("RGB"),
        ));
        object.write_to_file(&path).unwrap();

        let slice_count = indexed_slice_count(&path);

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 2);
    }

    /// A frame split over several fragments, with a filled Basic Offset
    /// Table, keeps the declared frame count and opens.
    #[test]
    fn frames_split_over_several_fragments_keep_their_count() {
        let path = temporary_file_path("adv-split-fragments");
        write_encapsulated_frames(&path, "2", Vec::new());
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new(vec![0_u32, 24], vec![vec![0xFF, 0xD8]; 4]),
        ));
        object.write_to_file(&path).unwrap();

        let slice_count = indexed_slice_count(&path);
        let opened = open_dicom_file(&path);

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 2);
        assert!(opened.is_ok(), "{:?}", opened.err());
    }

    #[test]
    fn implicit_and_big_endian_native_frames_are_bounded_by_their_bytes() {
        for transfer_syntax in [
            uids::IMPLICIT_VR_LITTLE_ENDIAN,
            // Explicit VR Big Endian, retired but still found in archives.
            "1.2.840.10008.1.2.2",
        ] {
            let path = temporary_file_path("adv-other-native-syntax");
            write_native_frames(&path, 2, "5", vec![0; 12]);
            let mut object = dicom_object::open_file(&path).unwrap();
            object.update_meta(|meta| meta.transfer_syntax = transfer_syntax.to_owned());
            object.write_to_file(&path).unwrap();

            let slice_count = indexed_slice_count(&path);
            let opened = open_dicom_file(&path);

            std::fs::remove_file(path).unwrap();
            assert_eq!(slice_count, 3, "{transfer_syntax}");
            assert!(opened.is_ok(), "{transfer_syntax}: {:?}", opened.err());
        }
    }

    #[test]
    fn unusable_frame_counts_index_one_frame() {
        for number_of_frames in ["0", "abc", "-4"] {
            let path = temporary_file_path("adv-unusable-frame-count");
            write_native_frames(&path, 1, number_of_frames, vec![0, 0]);

            let slice_count = indexed_slice_count(&path);

            std::fs::remove_file(path).unwrap();
            assert_eq!(slice_count, 1, "{number_of_frames}");
        }
    }

    /// The streaming scan of a deflated file cannot measure its pixel data, so
    /// it keeps a declared count up to the cap and no more.
    #[test]
    fn deflated_frame_counts_are_kept_up_to_the_cap() {
        for (declared, expected) in [("3", 3), ("10000", 10_000), ("10001", 10_000)] {
            let source_path = temporary_file_path("adv-deflate-source");
            let deflated_path = temporary_file_path("adv-deflated");
            write_native_frames(&source_path, 1, declared, vec![0, 0]);
            let mut object = dicom_object::open_file(&source_path).unwrap();
            object.update_meta(|meta| {
                meta.transfer_syntax = uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.to_owned();
            });
            object.write_to_file(&deflated_path).unwrap();

            let slice_count = indexed_slice_count(&deflated_path);
            // Its lengths count inflated bytes, so the file size cannot refuse it.
            let opened = open_dicom_file(&deflated_path);

            std::fs::remove_file(source_path).unwrap();
            std::fs::remove_file(deflated_path).unwrap();
            assert_eq!(slice_count, expected, "{declared}");
            assert!(opened.is_ok(), "{declared}: {:?}", opened.err());
        }
    }

    /// Writes the native test image with a private OB value of two bytes
    /// before PixelData, as Deflated Explicit VR Little Endian whose inflated
    /// data set declares `private_length` for that value. `deflate` turns the
    /// inflated data set into the stream that is written.
    fn write_deflated_with_private_value_length(
        path: &Path,
        private_length: u32,
        deflate: impl FnOnce(&[u8]) -> Vec<u8>,
    ) {
        fn data_set_start(bytes: &[u8]) -> usize {
            144 + u32::from_le_bytes(bytes[140..144].try_into().unwrap()) as usize
        }

        write_native_frames(path, 1, "1", vec![0, 0]);
        let mut object = dicom_object::open_file(path).unwrap();
        object.put_element(DataElement::new(
            Tag(0x0009, 0x1010),
            VR::OB,
            PrimitiveValue::from(vec![0xAB_u8, 0xCD]),
        ));
        object.write_to_file(path).unwrap();
        patch_length(
            path,
            &[0x09, 0x00, 0x10, 0x10, b'O', b'B', 0, 0],
            8,
            private_length,
        );
        let explicit = std::fs::read(path).unwrap();
        object.update_meta(|meta| {
            meta.transfer_syntax = uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.to_owned();
        });
        object.write_to_file(path).unwrap();
        let deflated = std::fs::read(path).unwrap();

        let mut bytes = deflated[..data_set_start(&deflated)].to_vec();
        bytes.extend(deflate(&explicit[data_set_start(&explicit)..]));
        std::fs::write(path, bytes).unwrap();
    }

    /// Deflates `inflated` with the registry's own data set codec.
    fn deflate_with_registry(inflated: &[u8]) -> Vec<u8> {
        let transfer_syntax = TransferSyntaxRegistry
            .get(uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN)
            .unwrap();
        let dicom_encoding::transfer_syntax::Codec::Dataset(Some(adapter)) =
            transfer_syntax.codec()
        else {
            panic!("the deflated transfer syntax has no data set adapter");
        };
        let mut stream = Vec::new();
        {
            let mut writer = adapter.adapt_writer(Box::new(&mut stream));
            writer.write_all(inflated).unwrap();
            writer.flush().unwrap();
        }
        stream
    }

    /// Stores `inflated` in deflate blocks that are not the last block, then
    /// ends the stream with a block of the reserved type, which inflaters
    /// reject: the stream breaks right after the inflated bytes.
    fn stored_blocks_then_a_broken_block(inflated: &[u8]) -> Vec<u8> {
        let mut stream = Vec::new();
        for chunk in inflated.chunks(usize::from(u16::MAX)) {
            let length = u16::try_from(chunk.len()).unwrap();
            // BFINAL 0, BTYPE 00 (stored), padded to the byte boundary.
            stream.push(0x00);
            stream.extend_from_slice(&length.to_le_bytes());
            stream.extend_from_slice(&(!length).to_le_bytes());
            stream.extend_from_slice(chunk);
        }
        // BFINAL 1, BTYPE 11, a block type deflate reserves.
        stream.push(0x07);
        stream
    }

    /// A deflated file's lengths count inflated bytes, so the inflated data
    /// set bounds them: the full open used to request a 4 GB buffer here.
    #[test]
    fn a_deflated_element_longer_than_the_inflated_data_set_is_refused() {
        let valid_path = temporary_file_path("deflated-private-value");
        let corrupt_path = temporary_file_path("deflated-private-length");
        write_deflated_with_private_value_length(&valid_path, 2, deflate_with_registry);
        write_deflated_with_private_value_length(&corrupt_path, 0xF000_0000, deflate_with_registry);

        let valid = open_dicom_file(&valid_path);
        let error = adversarial_open_error(&corrupt_path);

        std::fs::remove_file(valid_path).unwrap();
        let private_value = valid
            .unwrap()
            .element(Tag(0x0009, 0x1010))
            .unwrap()
            .to_bytes()
            .unwrap()
            .into_owned();
        assert_eq!(private_value, [0xAB, 0xCD]);
        assert!(
            error.starts_with("DICOM element (0009,1010) declares 4026531840 bytes, but only ")
                && error.ends_with(" remain in the inflated data set"),
            "{error}"
        );
    }

    /// A deflate stream that breaks inside an over-long value ends the data
    /// set there, as the end of the stream does: the full open would still
    /// reserve the declared length before it reached the broken block.
    #[test]
    fn a_deflated_stream_that_breaks_inside_an_over_long_value_is_refused() {
        let path = temporary_file_path("deflated-broken-inside-value");
        // Several reads' worth of data follow the header, so the header is
        // read before the inflater reaches the broken block.
        write_deflated_with_private_value_length(&path, 0xF000_0000, |inflated| {
            stored_blocks_then_a_broken_block(&[inflated, &[0; 1 << 18]].concat())
        });

        let error = adversarial_open_error(&path);

        assert!(
            error.starts_with("DICOM element (0009,1010) declares 4026531840 bytes, but only ")
                && error.ends_with(" remain in the inflated data set"),
            "{error}"
        );
    }

    /// Rewrites the file at `path`, an Explicit VR Little Endian file, as
    /// Deflated Explicit VR Little Endian with the same data set bytes, even
    /// ones dicom-object could not write.
    fn rewrite_as_deflated(path: &Path) {
        fn data_set_start(bytes: &[u8]) -> usize {
            144 + u32::from_le_bytes(bytes[140..144].try_into().unwrap()) as usize
        }

        let explicit = std::fs::read(path).unwrap();
        let meta_path = temporary_file_path("x3-deflated-meta");
        write_test_object(&meta_path, true);
        let mut object = dicom_object::open_file(&meta_path).unwrap();
        object.update_meta(|meta| {
            meta.transfer_syntax = uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.to_owned();
        });
        object.write_to_file(&meta_path).unwrap();
        let deflated = std::fs::read(&meta_path).unwrap();
        std::fs::remove_file(meta_path).unwrap();

        let transfer_syntax = TransferSyntaxRegistry
            .get(uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN)
            .unwrap();
        let dicom_encoding::transfer_syntax::Codec::Dataset(Some(adapter)) =
            transfer_syntax.codec()
        else {
            panic!("the deflated transfer syntax has no data set adapter");
        };
        let mut bytes = deflated[..data_set_start(&deflated)].to_vec();
        {
            let mut writer = adapter.adapt_writer(Box::new(&mut bytes));
            writer
                .write_all(&explicit[data_set_start(&explicit)..])
                .unwrap();
            writer.flush().unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    /// Bytes of an Explicit VR Little Endian element header with a 4-byte
    /// length.
    fn long_header(group: u16, element: u16, vr: &[u8; 2], length: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&group.to_le_bytes());
        bytes.extend_from_slice(&element.to_le_bytes());
        bytes.extend_from_slice(vr);
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes
    }

    const ITEM_UNDEFINED: [u8; 8] = [0xFE, 0xFF, 0x00, 0xE0, 0xFF, 0xFF, 0xFF, 0xFF];
    const ITEM_DELIMITER: [u8; 8] = [0xFE, 0xFF, 0x0D, 0xE0, 0, 0, 0, 0];
    const SEQUENCE_DELIMITER: [u8; 8] = [0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0];

    /// After a large native PixelData the walk resumes at the element that
    /// follows it, so the bytes left are counted from there, not from the
    /// start of the pixel data or of the data set.
    #[test]
    fn x3_a_trailing_element_is_bounded_by_the_bytes_after_large_pixel_data() {
        let path = temporary_file_path("x3-trailing-after-large-native");
        write_native_frames(&path, 256, "1", vec![0x11; 65_536]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend(long_header(0xFFFC, 0xFFFC, b"OB", 1_000));
        bytes.extend_from_slice(&[0; 4]);
        std::fs::write(&path, bytes).unwrap();

        let error = adversarial_open_error(&path);

        assert_eq!(
            error,
            "DICOM element (FFFC,FFFC) declares 1000 bytes, but only 4 remain in the file"
        );
    }

    /// The same after encapsulated pixel data: the walk resumes right after
    /// the sequence delimiter.
    #[test]
    fn x3_a_trailing_element_is_bounded_by_the_bytes_after_large_fragments() {
        let path = temporary_file_path("x3-trailing-after-large-fragments");
        let mut bytes = write_encapsulated_frames(&path, "1", vec![vec![0x22; 65_536]]);
        bytes.extend(long_header(0xFFFC, 0xFFFC, b"OB", 1_000));
        bytes.extend_from_slice(&[0; 4]);
        std::fs::write(&path, bytes).unwrap();

        let error = adversarial_open_error(&path);

        assert_eq!(
            error,
            "DICOM element (FFFC,FFFC) declares 1000 bytes, but only 4 remain in the file"
        );
    }

    /// Valid elements after PixelData, a digital signature sequence and
    /// trailing padding, open in every native transfer syntax.
    #[test]
    fn x3_valid_signatures_and_padding_after_pixel_data_open() {
        for transfer_syntax in [
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            uids::IMPLICIT_VR_LITTLE_ENDIAN,
            "1.2.840.10008.1.2.2",
        ] {
            let path = temporary_file_path("x3-signature-padding");
            write_native_frames(&path, 2, "1", vec![0x33; 4]);
            let mut object = dicom_object::open_file(&path).unwrap();
            object.update_meta(|meta| meta.transfer_syntax = transfer_syntax.to_owned());
            object.put_element(DataElement::new(
                Tag(0xFFFA, 0xFFFA),
                VR::SQ,
                dicom_core::value::DataSetSequence::from(vec![
                    InMemDicomObject::from_element_iter([
                        DataElement::new(
                            Tag(0x0400, 0x0100),
                            VR::UI,
                            PrimitiveValue::from("1.2.840.10008.1.2.1"),
                        ),
                        DataElement::new(
                            Tag(0x0400, 0x0120),
                            VR::OB,
                            PrimitiveValue::from(vec![0x44_u8; 64]),
                        ),
                    ]),
                ]),
            ));
            object.put_element(DataElement::new(
                Tag(0xFFFC, 0xFFFC),
                VR::OB,
                PrimitiveValue::from(vec![0_u8; 256]),
            ));
            object.write_to_file(&path).unwrap();

            let opened = open_dicom_file(&path);

            std::fs::remove_file(path).unwrap();
            let opened = opened.unwrap_or_else(|error| panic!("{transfer_syntax}: {error}"));
            assert_eq!(
                opened
                    .element(Tag(0xFFFC, 0xFFFC))
                    .unwrap()
                    .to_bytes()
                    .unwrap()
                    .len(),
                256,
                "{transfer_syntax}"
            );
        }
    }

    /// A value nested in an undefined-length sequence after the encapsulated
    /// pixel data is checked too.
    #[test]
    fn x3_a_nested_element_after_encapsulated_pixel_data_is_refused() {
        let path = temporary_file_path("x3-nested-after-fragments");
        let mut bytes = write_encapsulated_frames(&path, "1", vec![vec![0xFF, 0xD8, 0xFF, 0xD9]]);
        bytes.extend(long_header(0xFFFA, 0xFFFA, b"SQ", 0xFFFF_FFFF));
        bytes.extend_from_slice(&ITEM_UNDEFINED);
        bytes.extend(long_header(0x0400, 0x0120, b"OB", 0x0010_0000));
        bytes.extend_from_slice(&[0; 4]);
        std::fs::write(&path, bytes).unwrap();

        let error = adversarial_open_error(&path);

        assert_eq!(
            error,
            "DICOM element (0400,0120) declares 1048576 bytes, but only 4 remain in the file"
        );
    }

    /// A private undefined-length UN value after PixelData, as some vendors
    /// write in group 7FE1, holding empty and delimited items, still opens.
    #[test]
    fn x3_an_undefined_length_private_value_after_pixel_data_opens() {
        let path = temporary_file_path("x3-private-un-after-pixel-data");
        write_native_frames(&path, 1, "1", vec![0, 0]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend(long_header(0x7FE1, 0x1010, b"UN", 0xFFFF_FFFF));
        bytes.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0, 0, 0, 0]);
        bytes.extend_from_slice(&ITEM_UNDEFINED);
        bytes.extend(long_header(0x7FE1, 0x1020, b"OB", 2));
        bytes.extend_from_slice(&[0x55, 0x66]);
        bytes.extend_from_slice(&ITEM_DELIMITER);
        bytes.extend_from_slice(&SEQUENCE_DELIMITER);
        // An odd length that ends exactly at the end of the file.
        bytes.extend(long_header(0xFFFC, 0xFFFC, b"OB", 3));
        bytes.extend_from_slice(&[0; 3]);
        std::fs::write(&path, bytes).unwrap();

        let opened = open_dicom_file(&path);

        std::fs::remove_file(path).unwrap();
        let opened = opened.unwrap();
        assert_eq!(
            opened
                .element(Tag(0xFFFC, 0xFFFC))
                .unwrap()
                .to_bytes()
                .unwrap()
                .len(),
            3
        );
    }

    /// Writes the test image with ReferencedImageSequence (0008,1115) nested
    /// `depth` levels deep through undefined-length sequences and items. The
    /// innermost item holds ReferencedSOPInstanceUID 2.25.9.
    fn write_nested_sequences(path: &Path, depth: usize) {
        write_test_object(path, true);
        let mut bytes = std::fs::read(path).unwrap();
        let samples_per_pixel_header = [0x28, 0x00, 0x02, 0x00, b'U', b'S', 2, 0];
        let insert_at = bytes
            .windows(samples_per_pixel_header.len())
            .position(|window| window == samples_per_pixel_header)
            .unwrap();
        let mut nesting = Vec::new();
        for _ in 0..depth {
            nesting.extend(long_header(0x0008, 0x1115, b"SQ", 0xFFFF_FFFF));
            nesting.extend_from_slice(&ITEM_UNDEFINED);
        }
        nesting.extend_from_slice(&[0x08, 0x00, 0x55, 0x11, b'U', b'I', 6, 0]);
        nesting.extend_from_slice(b"2.25.9");
        for _ in 0..depth {
            nesting.extend_from_slice(&ITEM_DELIMITER);
            nesting.extend_from_slice(&SEQUENCE_DELIMITER);
        }
        bytes.splice(insert_at..insert_at, nesting);
        std::fs::write(path, bytes).unwrap();
    }

    /// Writes the test image with a private (0002,0099) element in its file
    /// meta group that nests sequences `depth` levels deep through
    /// defined-length sequences and items. FileMetaTable skips the element.
    fn write_nested_meta_sequences(path: &Path, depth: usize) {
        write_test_object(path, true);
        let mut bytes = std::fs::read(path).unwrap();
        let group_length = u32::from_le_bytes(bytes[140..144].try_into().unwrap());
        let mut nesting = Vec::new();
        for level in 1..=depth {
            let mut item = vec![0xFE, 0xFF, 0x00, 0xE0];
            item.extend_from_slice(&u32::try_from(nesting.len()).unwrap().to_le_bytes());
            item.extend(nesting);
            let (group, element) = if level == depth {
                (0x0002, 0x0099)
            } else {
                (0x0008, 0x1115)
            };
            nesting = long_header(group, element, b"SQ", u32::try_from(item.len()).unwrap());
            nesting.extend(item);
        }
        let meta_end = 144 + group_length as usize;
        let group_length = group_length + u32::try_from(nesting.len()).unwrap();
        bytes.splice(meta_end..meta_end, nesting);
        bytes[140..144].copy_from_slice(&group_length.to_le_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    /// Runs `check` on a thread whose stack holds dicom-object's recursive
    /// read of thousands of nested sequences, so that a nesting limit that
    /// stops working fails the test instead of aborting the test binary with
    /// a stack overflow.
    fn on_a_deep_stack(check: impl FnOnce() + Send) {
        std::thread::scope(|scope| {
            let checked = std::thread::Builder::new()
                .stack_size(512 << 20)
                .spawn_scoped(scope, check)
                .unwrap()
                .join();
            if let Err(panic) = checked {
                std::panic::resume_unwind(panic);
            }
        });
    }

    /// dicom-object reads nested sequences recursively, so selecting such a
    /// file used to overflow the stack of the loader thread and abort the app:
    /// from about 270 levels on a 2 MiB thread in a debug build. The scan does
    /// not recurse and still lists the file.
    #[test]
    fn sequences_nested_too_deeply_are_refused_before_the_full_open() {
        let expected_error = format!(
            "DICOM sequences nest more than {} levels deep",
            super::MAX_SEQUENCE_DEPTH
        );
        let check = |depth: usize| {
            for in_file_meta in [false, true] {
                let path = temporary_file_path("deep-nesting");
                if in_file_meta {
                    write_nested_meta_sequences(&path, depth);
                } else {
                    write_nested_sequences(&path, depth);
                }

                let slice_count = indexed_slice_count(&path);
                let loaded = crate::dicom::load_dicom_frame(&path, 0);

                std::fs::remove_file(path).unwrap();
                assert_eq!(slice_count, 1, "depth {depth}, in file meta {in_file_meta}");
                let error = format!("{:#}", loaded.err().unwrap());
                assert!(
                    error.ends_with(&expected_error),
                    "depth {depth}, in file meta {in_file_meta}: {error}"
                );
            }
        };
        check(super::MAX_SEQUENCE_DEPTH + 1);
        on_a_deep_stack(|| check(10_000));
    }

    fn find_metadata_value<'a>(
        items: &'a [crate::dicom::MetadataItem],
        tag: &str,
    ) -> Option<&'a str> {
        items.iter().find_map(|item| {
            if item.tag == tag {
                Some(item.value.as_str())
            } else {
                find_metadata_value(&item.children, tag)
            }
        })
    }

    #[test]
    fn realistically_nested_sequences_open_and_show_their_metadata() {
        for depth in [8, super::MAX_SEQUENCE_DEPTH] {
            let path = temporary_file_path("nested-sequences");
            write_nested_sequences(&path, depth);

            let loaded = crate::dicom::load_dicom_frame(&path, 0);

            std::fs::remove_file(path).unwrap();
            let metadata = loaded.unwrap().metadata;
            assert_eq!(
                find_metadata_value(&metadata.all_items, "(0008,1155)"),
                Some("2.25.9"),
                "depth {depth}"
            );
        }
    }

    /// Crafted nesting after PixelData, many thousands of undefined-length
    /// sequences that never close, is refused by the walk without exhausting
    /// the stack; the full open would overflow it.
    #[test]
    fn x3_deep_unclosed_nesting_after_pixel_data_is_refused() {
        let path = temporary_file_path("x3-deep-nesting");
        write_native_frames(&path, 1, "1", vec![0, 0]);
        let mut bytes = std::fs::read(&path).unwrap();
        for _ in 0..50_000 {
            bytes.extend(long_header(0x0009, 0x1010, b"SQ", 0xFFFF_FFFF));
            bytes.extend_from_slice(&ITEM_UNDEFINED);
        }
        std::fs::write(&path, bytes).unwrap();

        let checked = super::ensure_declared_lengths_fit_file(&path);

        std::fs::remove_file(path).unwrap();
        assert_eq!(
            checked.err().unwrap().to_string(),
            format!(
                "DICOM sequences nest more than {} levels deep",
                super::MAX_SEQUENCE_DEPTH
            )
        );
    }

    /// A value inside a sequence item of a deflated data set that runs past
    /// the inflated data is refused like a top-level one.
    #[test]
    fn x3_a_nested_deflated_element_longer_than_the_inflated_data_is_refused() {
        let path = temporary_file_path("x3-deflated-nested");
        write_native_frames_with_item(
            &path,
            vec![DataElement::new(
                Tag(0x0009, 0x1010),
                VR::OB,
                PrimitiveValue::from(vec![0xAB_u8, 0xCD, 0xEF, 0x01]),
            )],
        );
        patch_length(
            &path,
            &[0x09, 0x00, 0x10, 0x10, b'O', b'B', 0, 0],
            8,
            0x0100_0000,
        );
        rewrite_as_deflated(&path);

        let error = adversarial_open_error(&path);

        assert!(
            error.starts_with("DICOM element (0009,1010) declares 16777216 bytes, but only ")
                && error.ends_with(" remain in the inflated data set"),
            "{error}"
        );
    }

    /// A deflated data set with encapsulated-looking pixel data, nested
    /// sequences and trailing padding is read through and still opens.
    #[test]
    fn x3_a_valid_deflated_file_with_fragments_and_sequences_opens() {
        let path = temporary_file_path("x3-deflated-fragments");
        write_native_frames_with_item(
            &path,
            vec![DataElement::new(
                Tag(0x0008, 0x1140),
                VR::SQ,
                dicom_core::value::DataSetSequence::from(vec![
                    InMemDicomObject::from_element_iter([DataElement::new(
                        Tag(0x0008, 0x1150),
                        VR::UI,
                        PrimitiveValue::from("1.2"),
                    )]),
                ]),
            )],
        );
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new_fragments(vec![vec![0xFF, 0xD8, 0xFF, 0xD9]]),
        ));
        object.put_element(DataElement::new(
            Tag(0xFFFC, 0xFFFC),
            VR::OB,
            PrimitiveValue::from(vec![0_u8; 8]),
        ));
        object.write_to_file(&path).unwrap();
        rewrite_as_deflated(&path);

        let checked = super::ensure_declared_lengths_fit_file(&path);

        std::fs::remove_file(path).unwrap();
        assert!(checked.is_ok(), "{:?}", checked.err());
    }

    /// A deflated file cut inside its last value is refused; the same file
    /// whole passes.
    #[test]
    fn x3_a_deflated_file_cut_short_is_refused() {
        let path = temporary_file_path("x3-deflated-cut");
        write_native_frames(&path, 64, "1", vec![0x77; 4_096]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend(long_header(0xFFFC, 0xFFFC, b"OB", 600));
        bytes.extend_from_slice(&[0; 100]);
        std::fs::write(&path, bytes).unwrap();
        rewrite_as_deflated(&path);

        let error = adversarial_open_error(&path);

        assert_eq!(
            error,
            "DICOM element (FFFC,FFFC) declares 600 bytes, but only 100 remain in the inflated data set"
        );
    }

    /// A deflated data set is read through past encapsulated pixel data, so
    /// an element after it that runs past the inflated data is refused.
    #[test]
    fn x3_a_deflated_element_after_encapsulated_pixel_data_is_refused() {
        let path = temporary_file_path("x3-deflated-after-fragments");
        let mut bytes = write_encapsulated_frames(&path, "1", vec![vec![0xFF, 0xD8, 0xFF, 0xD9]]);
        bytes.extend(long_header(0xFFFC, 0xFFFC, b"OB", 600));
        bytes.extend_from_slice(&[0; 100]);
        std::fs::write(&path, bytes).unwrap();
        rewrite_as_deflated(&path);

        let error = adversarial_open_error(&path);

        assert_eq!(
            error,
            "DICOM element (FFFC,FFFC) declares 600 bytes, but only 100 remain in the inflated data set"
        );
    }

    /// An element following fragments requires a delimiter. Otherwise the
    /// preflight cannot validate the rest of the data set.
    #[test]
    fn pixel_data_without_its_delimiter_before_trailing_data_is_refused() {
        let path = temporary_file_path("x3-fragments-then-element");
        let bytes = write_encapsulated_frames(&path, "1", vec![vec![0xFF, 0xD8, 0xFF, 0xD9]]);
        let mut bytes = bytes[..bytes.len() - 8].to_vec();
        bytes.extend(long_header(0xFFFC, 0xFFFC, b"OB", 0));
        bytes.extend(long_header(0xFFFC, 0xFFFC, b"OB", 0x0100_0000));
        std::fs::write(&path, bytes).unwrap();

        let checked = super::ensure_declared_lengths_fit_file(&path);

        std::fs::remove_file(path).unwrap();
        assert!(
            checked
                .unwrap_err()
                .to_string()
                .contains("no sequence delimiter")
        );
    }

    #[test]
    fn scan_worker_count_respects_workload_cpu_count_and_cap() {
        assert_eq!(scan_worker_count_for(1, 32), 1);
        assert_eq!(scan_worker_count_for(80, 32), 5);
        assert_eq!(scan_worker_count_for(10_000, 4), 4);
        assert_eq!(scan_worker_count_for(10_000, 32), 8);
    }

    fn implicit_nesting(depth: usize) -> Vec<u8> {
        let mut nesting = Vec::new();
        for _ in 0..depth {
            nesting.extend_from_slice(&[0x08, 0x00, 0x15, 0x11, 0xFF, 0xFF, 0xFF, 0xFF]);
            nesting.extend_from_slice(&ITEM_UNDEFINED);
        }
        nesting.extend_from_slice(&[0x08, 0x00, 0x55, 0x11, 6, 0, 0, 0]);
        nesting.extend_from_slice(b"2.25.9");
        for _ in 0..depth {
            nesting.extend_from_slice(&ITEM_DELIMITER);
            nesting.extend_from_slice(&SEQUENCE_DELIMITER);
        }
        nesting
    }

    /// Inserts `bytes` into the file at `path` just before the first
    /// occurrence of `header`.
    fn insert_before_header(path: &Path, header: &[u8], bytes: &[u8]) {
        let mut file_bytes = std::fs::read(path).unwrap();
        let insert_at = file_bytes
            .windows(header.len())
            .position(|window| window == header)
            .unwrap();
        file_bytes.splice(insert_at..insert_at, bytes.iter().copied());
        std::fs::write(path, file_bytes).unwrap();
    }

    const EXPLICIT_SAMPLES_PER_PIXEL: [u8; 8] = [0x28, 0x00, 0x02, 0x00, b'U', b'S', 2, 0];

    /// Writes the test image with sequences nested `depth` levels deep in the
    /// encoding `encoding` names.
    fn write_nesting_in_encoding(path: &Path, encoding: &str, depth: usize) {
        match encoding {
            "implicit" => {
                write_test_object(path, true);
                let mut object = dicom_object::open_file(path).unwrap();
                object.update_meta(|meta| {
                    meta.transfer_syntax = uids::IMPLICIT_VR_LITTLE_ENDIAN.to_owned();
                });
                object.write_to_file(path).unwrap();
                insert_before_header(
                    path,
                    &[0x28, 0x00, 0x02, 0x00, 2, 0, 0, 0],
                    &implicit_nesting(depth),
                );
            }
            "deflated" => {
                write_nested_sequences(path, depth);
                rewrite_as_deflated(path);
            }
            _ => unreachable!(),
        }
    }

    /// The nesting limit holds in every data set encoding the walk reads, at
    /// exactly the limit and one level past it.
    #[test]
    fn the_nesting_limit_holds_in_implicit_and_deflated_data_sets() {
        let expected_error = format!(
            "DICOM sequences nest more than {} levels deep",
            super::MAX_SEQUENCE_DEPTH
        );
        for encoding in ["implicit", "deflated"] {
            for depth in [super::MAX_SEQUENCE_DEPTH, super::MAX_SEQUENCE_DEPTH + 1] {
                let path = temporary_file_path("nesting-encoding");
                write_nesting_in_encoding(&path, encoding, depth);

                let slice_count = indexed_slice_count(&path);
                let loaded = crate::dicom::load_dicom_frame(&path, 0);

                std::fs::remove_file(path).unwrap();
                assert_eq!(slice_count, 1, "{encoding}, depth {depth}");
                match loaded {
                    Ok(_) => assert!(
                        depth == super::MAX_SEQUENCE_DEPTH,
                        "{encoding}, depth {depth} opened"
                    ),
                    Err(error) => {
                        let error = format!("{error:#}");
                        assert!(
                            depth > super::MAX_SEQUENCE_DEPTH && error.ends_with(&expected_error),
                            "{encoding}, depth {depth}: {error}"
                        );
                    }
                }
            }
        }
    }

    /// Sequences that follow one another do not nest: a file with more
    /// top-level sequences than the nesting limit, in its data set or in its
    /// file meta group, opens, as does a meta group nested exactly to the
    /// limit.
    #[test]
    fn sibling_sequences_do_not_count_as_nesting() {
        let sequence_count = 2 * super::MAX_SEQUENCE_DEPTH + 1;
        let defined_length_sequence = |group: u16, element: u16| {
            let mut item_value = vec![0x08, 0x00, 0x55, 0x11, b'U', b'I', 6, 0];
            item_value.extend_from_slice(b"2.25.9");
            let mut item = vec![0xFE, 0xFF, 0x00, 0xE0];
            item.extend_from_slice(&u32::try_from(item_value.len()).unwrap().to_le_bytes());
            item.extend(item_value);
            let mut sequence =
                long_header(group, element, b"SQ", u32::try_from(item.len()).unwrap());
            sequence.extend(item);
            sequence
        };

        let data_set_path = temporary_file_path("sibling-sequences");
        write_test_object(&data_set_path, true);
        let mut siblings = Vec::new();
        for index in 0..sequence_count {
            // Undefined and defined lengths alternate; private tags keep the
            // elements in ascending order.
            let element = 0x1000 + u16::try_from(index).unwrap();
            if index % 2 == 0 {
                siblings.extend(long_header(0x0009, element, b"SQ", 0xFFFF_FFFF));
                siblings.extend_from_slice(&ITEM_UNDEFINED);
                siblings.extend_from_slice(&ITEM_DELIMITER);
                siblings.extend_from_slice(&SEQUENCE_DELIMITER);
            } else {
                siblings.extend(defined_length_sequence(0x0009, element));
            }
        }
        insert_before_header(&data_set_path, &EXPLICIT_SAMPLES_PER_PIXEL, &siblings);

        let meta_path = temporary_file_path("sibling-meta-sequences");
        write_test_object(&meta_path, true);
        let mut bytes = std::fs::read(&meta_path).unwrap();
        let group_length = u32::from_le_bytes(bytes[140..144].try_into().unwrap());
        let mut meta_siblings = Vec::new();
        for index in 0..sequence_count {
            meta_siblings.extend(defined_length_sequence(
                0x0002,
                0x1000 + u16::try_from(index).unwrap(),
            ));
        }
        let meta_end = 144 + group_length as usize;
        let group_length = group_length + u32::try_from(meta_siblings.len()).unwrap();
        bytes.splice(meta_end..meta_end, meta_siblings);
        bytes[140..144].copy_from_slice(&group_length.to_le_bytes());
        std::fs::write(&meta_path, bytes).unwrap();

        let meta_at_limit_path = temporary_file_path("meta-nesting-at-limit");
        write_nested_meta_sequences(&meta_at_limit_path, super::MAX_SEQUENCE_DEPTH);

        for path in [data_set_path, meta_path, meta_at_limit_path] {
            let loaded = crate::dicom::load_dicom_frame(&path, 0);
            std::fs::remove_file(&path).unwrap();
            if let Err(error) = loaded {
                panic!("{}: {error:#}", path.display());
            }
        }
    }

    /// The character set probe runs before the scan and passes every value
    /// before PixelData in the default Auto mode, and those before (0008,0005)
    /// in DICOM default mode. In a deflated data set, a value that declares
    /// 0xFFFFFFF0 bytes, such as a PatientName stored as UN, is skipped
    /// instead of making the probe reserve 4 GB. Each scan runs in a child
    /// process with a 2 GiB address space, where such a reservation aborts it.
    #[cfg(target_os = "linux")]
    #[test]
    fn deflated_charset_probe_does_not_reserve_an_oversized_declared_length() {
        const CHILD_INPUT: &str = "DICRON_TEST_OVERSIZED_DEFLATED_VALUE";
        if let Some(input) = std::env::var_os(CHILD_INPUT) {
            let input = input.into_string().unwrap();
            let (mode, path) = input.split_once(':').unwrap();
            let encoding = match mode {
                "auto" => TextEncoding::Auto,
                _ => TextEncoding::DicomDefault,
            };
            let _ = build_for_file_with_encoding(Path::new(path), encoding);
            return;
        }

        // (mode, the start of the header the oversized value precedes, its tag)
        let cases: [(&str, [u8; 8], Tag); 2] = [
            ("auto", EXPLICIT_SAMPLES_PER_PIXEL, tags::PATIENT_NAME),
            (
                "dicom-default",
                [0x08, 0x00, 0x16, 0x00, b'U', b'I', 0, 0],
                Tag(0x0008, 0x0001),
            ),
        ];
        let mut failures = Vec::new();
        for (mode, before_header, tag) in cases {
            let path = temporary_file_path("deflated-oversized-value");
            write_test_object(&path, true);
            let mut oversized = long_header(tag.group(), tag.element(), b"UN", 0xFFFF_FFF0);
            oversized.extend_from_slice(b"Doe^Jane");
            insert_before_header(&path, &before_header[..6], &oversized);
            rewrite_as_deflated(&path);

            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(r#"ulimit -v 2097152 && exec "$0" --exact "$1" --test-threads=1"#)
                .arg(std::env::current_exe().unwrap())
                .arg(
                    "dicom::scan::tests::deflated_charset_probe_does_not_reserve_an_oversized_declared_length",
                )
                .env(CHILD_INPUT, format!("{mode}:{}", path.display()))
                .output()
                .unwrap();

            std::fs::remove_file(path).unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !(output.status.success() && stdout.contains("1 passed")) {
                failures.push(format!("{mode}: {:?}\n{stdout}\n{stderr}", output.status));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// Explicit VR Little Endian bytes of a defined-length sequence `tag` with
    /// one item that holds `item_value`.
    fn defined_length_sequence(tag: Tag, item_value: &[u8]) -> Vec<u8> {
        let mut item = vec![0xFE, 0xFF, 0x00, 0xE0];
        item.extend_from_slice(&u32::try_from(item_value.len()).unwrap().to_le_bytes());
        item.extend_from_slice(item_value);
        let mut sequence = long_header(
            tag.group(),
            tag.element(),
            b"SQ",
            u32::try_from(item.len()).unwrap(),
        );
        sequence.extend(item);
        sequence
    }

    /// A sequence `tag` whose item first holds a sequence that closes at
    /// once, then (0008,1115) nested `depth` levels deep: `depth + 1` levels
    /// in all.
    fn nesting_after_a_closed_sequence(tag: Tag, depth: usize) -> Vec<u8> {
        let mut nested = vec![0x08, 0x00, 0x55, 0x11, b'U', b'I', 6, 0];
        nested.extend_from_slice(b"2.25.9");
        for _ in 0..depth {
            nested = defined_length_sequence(Tag(0x0008, 0x1115), &nested);
        }
        let mut item_value = defined_length_sequence(Tag(0x0008, 0x1114), &[]);
        item_value.extend(nested);
        defined_length_sequence(tag, &item_value)
    }

    /// A sequence that closes inside another one leaves the depth of the
    /// outer one: nesting one level past the limit after it is refused, in
    /// the data set and in the file meta group.
    #[test]
    fn a_closed_inner_sequence_does_not_hide_the_depth_of_its_parent() {
        let expected_error = format!(
            "DICOM sequences nest more than {} levels deep",
            super::MAX_SEQUENCE_DEPTH
        );
        for in_file_meta in [false, true] {
            let path = temporary_file_path("closed-inner-sequence");
            write_test_object(&path, true);
            if in_file_meta {
                let nesting =
                    nesting_after_a_closed_sequence(Tag(0x0002, 0x0099), super::MAX_SEQUENCE_DEPTH);
                let mut bytes = std::fs::read(&path).unwrap();
                let group_length = u32::from_le_bytes(bytes[140..144].try_into().unwrap());
                let meta_end = 144 + group_length as usize;
                let group_length = group_length + u32::try_from(nesting.len()).unwrap();
                bytes.splice(meta_end..meta_end, nesting);
                bytes[140..144].copy_from_slice(&group_length.to_le_bytes());
                std::fs::write(&path, bytes).unwrap();
            } else {
                let nesting =
                    nesting_after_a_closed_sequence(Tag(0x0008, 0x1115), super::MAX_SEQUENCE_DEPTH);
                insert_before_header(&path, &EXPLICIT_SAMPLES_PER_PIXEL, &nesting);
            }

            let loaded = crate::dicom::load_dicom_frame(&path, 0);

            std::fs::remove_file(path).unwrap();
            let error = format!("{:#}", loaded.err().unwrap());
            assert!(
                error.ends_with(&expected_error),
                "in file meta {in_file_meta}: {error}"
            );
        }
    }

    /// Only top-level values are indexed: the same elements inside a
    /// sequence item, here after a nested sequence that has closed, leave the
    /// patient, series and instance of the image as they are, in both scans.
    #[test]
    fn indexed_elements_inside_sequences_leave_the_index_unchanged() {
        for deflated in [false, true] {
            let path = temporary_file_path("nested-indexed-elements");
            write_test_object(&path, true);
            let mut object = dicom_object::open_file(&path).unwrap();
            for (tag, vr, value) in [
                (tags::PATIENT_ID, VR::LO, "top-patient"),
                (tags::SERIES_NUMBER, VR::IS, "7"),
                (tags::INSTANCE_NUMBER, VR::IS, "3"),
            ] {
                object.put_element(DataElement::new(tag, vr, value));
            }
            object.write_to_file(&path).unwrap();
            let mut item_value = defined_length_sequence(Tag(0x0020, 0x9222), &[]);
            for (group, element, vr, value) in [
                (0x0010_u16, 0x0020_u16, b"LO", &b"nested-pt"[..]),
                (0x0020, 0x0011, b"IS", b"8 "),
                (0x0020, 0x0013, b"IS", b"9 "),
            ] {
                item_value.extend_from_slice(&group.to_le_bytes());
                item_value.extend_from_slice(&element.to_le_bytes());
                item_value.extend_from_slice(vr);
                let length = u16::try_from(value.len() + value.len() % 2).unwrap();
                item_value.extend_from_slice(&length.to_le_bytes());
                item_value.extend_from_slice(value);
                if value.len() % 2 == 1 {
                    item_value.push(b' ');
                }
            }
            let sequence = defined_length_sequence(Tag(0x0020, 0x9221), &item_value);
            insert_before_header(&path, &EXPLICIT_SAMPLES_PER_PIXEL, &sequence);
            if deflated {
                rewrite_as_deflated(&path);
            }

            let index = build_for_file(&path);

            std::fs::remove_file(path).unwrap();
            let index = index.unwrap();
            let patient = &index.patients[0];
            let series = &patient.studies[0].series_groups[0];
            assert_eq!(patient.display_name, "top-patient", "deflated {deflated}");
            assert_eq!(series.series_number, Some(7), "deflated {deflated}");
            assert_eq!(
                series.slices[0].instance_number,
                Some(3),
                "deflated {deflated}"
            );
        }
    }

    /// An empty PixelData holds no frame, in both scans.
    #[test]
    fn an_empty_pixel_data_is_not_displayable_in_either_scan() {
        for deflated in [false, true] {
            let path = temporary_file_path("empty-pixel-data");
            write_test_object(&path, true);
            let mut object = dicom_object::open_file(&path).unwrap();
            object.put_element(DataElement::new(
                tags::PIXEL_DATA,
                VR::OB,
                PrimitiveValue::from(Vec::<u8>::new()),
            ));
            object.write_to_file(&path).unwrap();
            if deflated {
                rewrite_as_deflated(&path);
            }

            let error = build_for_file(&path).err().map(|error| error.to_string());

            std::fs::remove_file(path).unwrap();
            assert_eq!(
                error.as_deref(),
                Some("DICOM object does not contain supported image pixel metadata"),
                "deflated {deflated}"
            );
        }
    }

    /// FileMetaTable, which the scan and every open use to read the file meta
    /// group, sets aside the declared length of a value it keeps before it
    /// reads it, so a value longer than the file is refused before it is read.
    #[test]
    fn a_file_meta_value_longer_than_the_file_is_refused_before_it_is_read() {
        let path = temporary_file_path("oversized-file-meta-value");
        write_test_object(&path, true);
        let mut bytes = std::fs::read(&path).unwrap();
        let group_length = u32::from_le_bytes(bytes[140..144].try_into().unwrap());
        let meta_end = 144 + group_length as usize;
        let mut element = long_header(0x0002, 0x0013, b"UN", 0x0400_0000);
        element.extend_from_slice(b"ABCD");
        let group_length = group_length + u32::try_from(element.len()).unwrap();
        bytes.splice(meta_end..meta_end, element);
        bytes[140..144].copy_from_slice(&group_length.to_le_bytes());
        let remaining_length = bytes.len() - (meta_end + 12);
        std::fs::write(&path, bytes).unwrap();

        let scanned = build_for_file(&path)
            .err()
            .map(|error| format!("{error:#}"));
        let loaded = crate::dicom::load_dicom_frame(&path, 0)
            .err()
            .map(|error| format!("{error:#}"));

        std::fs::remove_file(path).unwrap();
        let expected_error = format!(
            "DICOM element (0002,0013) declares 67108864 bytes, but only {remaining_length} remain in the file"
        );
        for (path_name, error) in [("scan", scanned), ("open", loaded)] {
            let error = error.unwrap_or_default();
            assert!(error.ends_with(&expected_error), "{path_name}: {error}");
        }
    }

    /// dicom-object reads the sequences of the file meta group, which
    /// FileMetaTable skips, and sets aside the declared length of each value
    /// in them before it reads it, so such a value longer than the file is
    /// refused before the read. The scan does not read it and lists the file.
    #[test]
    fn a_file_meta_value_inside_a_sequence_longer_than_the_file_is_refused() {
        let path = temporary_file_path("oversized-nested-file-meta-value");
        write_test_object(&path, true);
        let mut bytes = std::fs::read(&path).unwrap();
        let group_length = u32::from_le_bytes(bytes[140..144].try_into().unwrap());
        let meta_end = 144 + group_length as usize;
        let mut element = long_header(0x0008, 0x1155, b"UN", 0x0400_0000);
        element.extend_from_slice(b"ABCD");
        let sequence = defined_length_sequence(Tag(0x0002, 0x0099), &element);
        let value_start = meta_end + sequence.len() - 4;
        let group_length = group_length + u32::try_from(sequence.len()).unwrap();
        bytes.splice(meta_end..meta_end, sequence);
        bytes[140..144].copy_from_slice(&group_length.to_le_bytes());
        let remaining_length = bytes.len() - value_start;
        std::fs::write(&path, bytes).unwrap();

        let slice_count = indexed_slice_count(&path);
        let loaded = crate::dicom::load_dicom_frame(&path, 0);

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 1);
        let error = format!("{:#}", loaded.err().unwrap());
        assert!(
            error.ends_with(&format!(
                "DICOM element (0008,1155) declares 67108864 bytes, but only {remaining_length} remain in the file"
            )),
            "{error}"
        );
    }

    /// Writes the test image with a private (0002,0099) value of
    /// `value_length` bytes at the end of its file meta group, followed by
    /// `trailing_element`, and returns where the trailing element starts.
    fn write_long_meta_value(path: &Path, value_length: usize, trailing_element: &[u8]) -> usize {
        write_test_object(path, true);
        let mut bytes = std::fs::read(path).unwrap();
        let group_length = u32::from_le_bytes(bytes[140..144].try_into().unwrap());
        let meta_end = 144 + group_length as usize;
        let mut elements = long_header(0x0002, 0x0099, b"UN", value_length.try_into().unwrap());
        elements.resize(elements.len() + value_length, 0);
        let trailing_start = meta_end + elements.len();
        elements.extend_from_slice(trailing_element);
        let group_length = group_length + u32::try_from(elements.len()).unwrap();
        bytes.splice(meta_end..meta_end, elements);
        bytes[140..144].copy_from_slice(&group_length.to_le_bytes());
        std::fs::write(path, bytes).unwrap();
        trailing_start
    }

    /// The file meta walk passes a value that does not fit in the read buffer
    /// and leaves the file where FileMetaTable reads it.
    #[test]
    fn a_file_meta_value_longer_than_the_read_buffer_is_passed() {
        let path = temporary_file_path("long-file-meta-value");
        write_long_meta_value(&path, 40_000, &[]);

        let slice_count = indexed_slice_count(&path);
        let loaded = crate::dicom::load_dicom_frame(&path, 0);

        std::fs::remove_file(path).unwrap();
        assert_eq!(slice_count, 1);
        assert!(loaded.is_ok(), "{:#}", loaded.err().unwrap());
    }

    #[test]
    fn a_file_meta_value_longer_than_the_file_after_one_longer_than_the_read_buffer_is_refused() {
        let path = temporary_file_path("oversized-file-meta-value-after-long-one");
        let mut element = long_header(0x0002, 0x0013, b"UN", 0x0400_0000);
        element.extend_from_slice(b"ABCD");
        let element_start = write_long_meta_value(&path, 40_000, &element);
        let remaining_length =
            std::fs::metadata(&path).unwrap().len() - (element_start as u64 + 12);

        let scanned = build_for_file(&path)
            .err()
            .map(|error| format!("{error:#}"));
        let loaded = crate::dicom::load_dicom_frame(&path, 0)
            .err()
            .map(|error| format!("{error:#}"));

        std::fs::remove_file(path).unwrap();
        let expected_error = format!(
            "DICOM element (0002,0013) declares 67108864 bytes, but only {remaining_length} remain in the file"
        );
        for (path_name, error) in [("scan", scanned), ("open", loaded)] {
            let error = error.unwrap_or_default();
            assert!(error.ends_with(&expected_error), "{path_name}: {error}");
        }
    }
}
