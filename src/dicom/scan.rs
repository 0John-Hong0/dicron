//! Filesystem discovery and DICOM header scanning.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use dicom_core::Length;
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::transfer_syntax::{Codec, TransferSyntaxIndex};
use dicom_object::{
    DefaultDicomObject, DicomCollectorOptions, FileMetaTable, FileMetaTableBuilder,
    InMemDicomObject, OpenFileOptions, file::ReadPreamble,
};
use dicom_parser::DataSetReader;
use dicom_parser::dataset::DataToken;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use walkdir::WalkDir;

use super::index::{add_dicom_object_to_index, sort_index};
use super::model::{DicomIndex, PatientGroup};

const EXPLICIT_VR_BIG_ENDIAN_UID: &str = "1.2.840.10008.1.2.2";

#[derive(Clone, Debug)]
pub(crate) struct BuildProgress {
    pub(crate) processed_file_count: usize,
    pub(crate) total_file_count: usize,
    pub(crate) readable_dicom_count: usize,
}

pub(crate) fn build_from_folder_with_progress<F>(
    folder_path: &Path,
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<DicomIndex>
where
    F: FnMut(BuildProgress),
{
    let file_paths = collect_walkdir_files(folder_path, cancel)?;
    build_for_files_with_progress(&file_paths, cancel, &mut on_progress)
}

pub(crate) fn build_for_inputs_with_progress<F>(
    input_paths: &[PathBuf],
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<DicomIndex>
where
    F: FnMut(BuildProgress),
{
    let file_paths = collect_file_paths(input_paths, cancel)?;
    build_for_files_with_progress(&file_paths, cancel, &mut on_progress)
}

fn build_for_files_with_progress<F>(
    file_paths: &[PathBuf],
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<DicomIndex>
where
    F: FnMut(BuildProgress),
{
    let total_input_file_count = file_paths.len();
    let mut readable_dicom_count = 0;
    let mut displayable_dicom_count = 0;
    let mut patients: Vec<PatientGroup> = Vec::new();

    on_progress(BuildProgress {
        processed_file_count: 0,
        total_file_count: total_input_file_count,
        readable_dicom_count: 0,
    });

    for (file_index, file_path) in file_paths.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(scan_cancelled());
        }

        let processed_file_count = file_index + 1;
        let Ok(dicom_object) = open_dicom_metadata(file_path) else {
            on_progress(BuildProgress {
                processed_file_count,
                total_file_count: total_input_file_count,
                readable_dicom_count,
            });
            continue;
        };

        readable_dicom_count += 1;
        let has_pixel_data = contains_pixel_data_element(file_path).unwrap_or(false);
        if add_dicom_object_to_index(&mut patients, file_path, &dicom_object, has_pixel_data) {
            displayable_dicom_count += 1;
        }

        on_progress(BuildProgress {
            processed_file_count,
            total_file_count: total_input_file_count,
            readable_dicom_count,
        });
    }

    sort_index(&mut patients);

    Ok(DicomIndex {
        patients,
        total_file_count: displayable_dicom_count,
    })
}

fn collect_file_paths(input_paths: &[PathBuf], cancel: &AtomicBool) -> Result<Vec<PathBuf>> {
    let mut file_paths = Vec::new();

    for input_path in input_paths {
        if cancel.load(Ordering::Relaxed) {
            return Err(scan_cancelled());
        }

        if input_path.is_dir() {
            file_paths.extend(collect_walkdir_files(input_path, cancel)?);
        } else if input_path.is_file() {
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

pub(crate) fn build_for_file(file_path: &Path) -> Result<DicomIndex> {
    let dicom_object = open_dicom_metadata(file_path)?;
    let mut patients: Vec<PatientGroup> = Vec::new();

    let has_pixel_data = contains_pixel_data_element(file_path)?;
    if !add_dicom_object_to_index(&mut patients, file_path, &dicom_object, has_pixel_data) {
        anyhow::bail!("DICOM object does not contain supported image pixel metadata");
    }
    sort_index(&mut patients);

    Ok(DicomIndex {
        patients,
        total_file_count: 1,
    })
}

fn open_dicom_metadata(file_path: &Path) -> Result<DefaultDicomObject> {
    let part_10_result = OpenFileOptions::new()
        .read_until(tags::PIXEL_DATA)
        .open_file(file_path);

    match part_10_result {
        Ok(object) => Ok(object),
        Err(part_10_error) => open_raw_dicom_dataset(file_path, false)
            .with_context(|| format!("could not parse DICOM file: {part_10_error}")),
    }
}

pub(crate) fn open_dicom_file(file_path: &Path) -> Result<DefaultDicomObject> {
    match OpenFileOptions::new().open_file(file_path) {
        Ok(object) => Ok(object),
        Err(part_10_error) => open_raw_dicom_dataset(file_path, true)
            .with_context(|| format!("could not parse DICOM file: {part_10_error}")),
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

fn contains_pixel_data_element(file_path: &Path) -> Result<bool> {
    let file = File::open(file_path)?;
    let mut reader = BufReader::new(file);
    let mut prefix = [0_u8; 132];
    let prefix_length = reader.read(&mut prefix)?;
    let (dataset_offset, transfer_syntax_uid) =
        if prefix_length >= 132 && &prefix[128..132] == b"DICM" {
            reader.seek(SeekFrom::Start(128))?;
            let file_meta = FileMetaTable::from_reader(&mut reader)?;
            (
                reader.stream_position()?,
                file_meta.transfer_syntax().to_owned(),
            )
        } else if prefix_length >= 4 && &prefix[..4] == b"DICM" {
            reader.seek(SeekFrom::Start(0))?;
            let file_meta = FileMetaTable::from_reader(&mut reader)?;
            (
                reader.stream_position()?,
                file_meta.transfer_syntax().to_owned(),
            )
        } else {
            (0, raw_dataset_transfer_syntax(file_path)?.to_owned())
        };
    reader.seek(SeekFrom::Start(dataset_offset))?;

    let transfer_syntax = TransferSyntaxRegistry
        .get(&transfer_syntax_uid)
        .with_context(|| format!("unsupported DICOM transfer syntax {}", transfer_syntax_uid))?;
    let dataset_reader: Box<dyn Read> = match transfer_syntax.codec() {
        Codec::Dataset(Some(adapter)) => adapter.adapt_reader(Box::new(reader)),
        Codec::Dataset(None) => anyhow::bail!("unsupported DICOM data set encoding"),
        Codec::None | Codec::EncapsulatedPixelData(..) => Box::new(reader),
    };
    let tokens = DataSetReader::new_with_ts(dataset_reader, transfer_syntax)?;
    let mut sequence_depth = 0_usize;

    for token in tokens {
        match token? {
            DataToken::ElementHeader(header)
                if sequence_depth == 0 && header.tag == tags::PIXEL_DATA =>
            {
                return Ok(header.len != Length(0));
            }
            DataToken::PixelSequenceStart if sequence_depth == 0 => return Ok(true),
            DataToken::SequenceStart { .. } | DataToken::PixelSequenceStart => {
                sequence_depth += 1;
            }
            DataToken::SequenceEnd => {
                sequence_depth = sequence_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use dicom_core::{DataElement, PrimitiveValue, VR, value::PixelFragmentSequence};
    use dicom_dictionary_std::{tags, uids};
    use dicom_object::{FileDicomObject, FileMetaTableBuilder};

    use super::build_for_file;

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

        assert_eq!(index.total_file_count, 1);
        assert_eq!(loaded.frame.frame_count, 1);
        std::fs::remove_file(part_10_path).unwrap();
        std::fs::remove_file(raw_path).unwrap();
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
}
