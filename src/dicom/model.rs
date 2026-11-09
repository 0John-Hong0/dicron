//! Core records used by the DICOM index.

use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct DicomIndex {
    pub(crate) patients: Vec<PatientGroup>,
    pub(crate) total_file_count: usize,
}

#[derive(Clone)]
pub(crate) struct PatientGroup {
    pub(crate) display_name: String,
    pub(crate) studies: Vec<StudyGroup>,
}

#[derive(Clone)]
pub(crate) struct StudyGroup {
    pub(crate) display_name: String,
    pub(crate) study_date: Option<String>,
    pub(crate) study_time: Option<String>,
    pub(crate) series_groups: Vec<SeriesGroup>,
}

#[derive(Clone)]
pub(crate) struct SeriesGroup {
    pub(crate) display_name: String,
    pub(crate) series_description: Option<String>,
    pub(crate) series_number: Option<i32>,
    pub(crate) modality: Option<String>,
    pub(crate) slices: Vec<SliceItem>,
}

#[derive(Clone)]
pub(crate) struct SliceItem {
    pub(crate) path: PathBuf,
    pub(crate) display_name: String,
    pub(crate) frame_index: u32,
    pub(crate) instance_number: Option<i32>,
    /// When the instance was created, used for ordering slices without
    /// `InstanceNumber`; `None` without a usable `InstanceCreationTime`.
    pub(crate) instance_creation_time: Option<InstanceCreationTime>,
    /// Signed through-plane position used after instance number or creation time:
    /// the projection of `ImagePositionPatient` onto the slice normal when
    /// orientation is known, otherwise the raw Z position.
    pub(crate) sort_position: Option<f64>,
}

#[derive(Clone, Copy)]
pub(crate) struct InstanceCreationTime {
    /// `InstanceCreationDate` as the number `YYYYMMDD`, or `None` when the
    /// date is missing or unreadable.
    pub(crate) date: Option<u32>,
    /// `InstanceCreationTime` in microseconds since midnight. Omitted time
    /// components are zero.
    pub(crate) time_of_day: u64,
}
