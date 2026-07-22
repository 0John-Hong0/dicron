//! Core records used by the DICOM index.

use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct DicomIndex {
    pub(crate) patients: Vec<PatientGroup>,
    pub(crate) total_file_count: usize,
}

#[derive(Clone)]
pub(crate) struct PatientGroup {
    pub(crate) patient_key: String,
    pub(crate) display_name: String,
    pub(crate) studies: Vec<StudyGroup>,
}

#[derive(Clone)]
pub(crate) struct StudyGroup {
    pub(crate) study_key: String,
    pub(crate) display_name: String,
    pub(crate) study_date: Option<String>,
    pub(crate) study_time: Option<String>,
    pub(crate) series_groups: Vec<SeriesGroup>,
}

#[derive(Clone)]
pub(crate) struct SeriesGroup {
    pub(crate) series_key: String,
    pub(crate) display_name: String,
    pub(crate) series_number: Option<i32>,
    pub(crate) slices: Vec<SliceItem>,
}

#[derive(Clone)]
pub(crate) struct SliceItem {
    pub(crate) path: PathBuf,
    pub(crate) display_name: String,
    pub(crate) frame_index: u32,
    pub(crate) instance_number: Option<i32>,
    /// Signed through-plane position used to order slices: the projection of
    /// `ImagePositionPatient` onto the slice normal when orientation is known
    /// (correct for sagittal/coronal/oblique stacks), otherwise the raw Z.
    pub(crate) sort_position: Option<f64>,
}
