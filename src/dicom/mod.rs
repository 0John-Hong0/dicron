//! DICOM file scanning, indexing, metadata, geometry, and pixel processing.

mod index;
mod metadata;
mod model;
mod pixels;
mod scan;

pub(crate) use metadata::{DicomMetadata, MetadataItem};
pub(crate) use model::{DicomIndex, PatientGroup, SliceItem, StudyGroup};
pub(crate) use pixels::{DecodedFrame, DicomWindow, DisplayPixels, load_dicom_frame, render_frame};
pub(crate) use scan::{
    BuildProgress, build_for_file, build_for_inputs_with_progress, build_from_folder_with_progress,
    is_candidate_path,
};
