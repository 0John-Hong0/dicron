//! Filesystem discovery and DICOM header scanning.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use dicom_dictionary_std::tags;
use dicom_object::{DefaultDicomObject, OpenFileOptions};
use walkdir::WalkDir;

use super::index::{add_dicom_object_to_index, sort_index};
use super::model::{DicomIndex, PatientGroup};

#[derive(Clone, Debug)]
pub(crate) struct BuildProgress {
    pub(crate) processed_file_count: usize,
    pub(crate) total_file_count: usize,
    pub(crate) readable_dicom_count: usize,
}

/// Cheap accept-filter for dropped / CLI paths. This avoids fully parsing files
/// on the UI thread; the background scan still performs the authoritative parse.
pub(crate) fn is_candidate_path(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    if has_dicom_preamble(path) {
        return true;
    }

    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("dcm") | Some("dicom")
    )
}

fn has_dicom_preamble(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0_u8; 132];

    file.read_exact(&mut header).is_ok() && &header[128..132] == b"DICM"
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
        add_dicom_object_to_index(&mut patients, file_path, &dicom_object);

        on_progress(BuildProgress {
            processed_file_count,
            total_file_count: total_input_file_count,
            readable_dicom_count,
        });
    }

    sort_index(&mut patients);

    Ok(DicomIndex {
        patients,
        total_file_count: readable_dicom_count,
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

    add_dicom_object_to_index(&mut patients, file_path, &dicom_object);
    sort_index(&mut patients);

    Ok(DicomIndex {
        patients,
        total_file_count: 1,
    })
}

fn open_dicom_metadata(file_path: &Path) -> Result<DefaultDicomObject> {
    Ok(OpenFileOptions::new()
        .read_until(tags::PIXEL_DATA)
        .open_file(file_path)?)
}
