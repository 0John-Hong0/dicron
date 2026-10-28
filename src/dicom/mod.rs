//! DICOM file scanning, indexing, metadata, geometry, and pixel processing.

mod elements;
mod geometry;
mod index;
mod jpeg_extended;
mod metadata;
mod model;
mod overlay_planes;
mod pixels;
mod scan;
mod text_encoding;
mod volume;

pub(crate) use geometry::{PatientAxis, dominant_patient_direction};
pub(crate) use metadata::{DicomMetadata, DicomOverlayMetadata, MetadataItem, MetadataItemKind};
#[cfg(test)]
pub(crate) use model::SeriesGroup;
pub(crate) use model::{DicomIndex, PatientGroup, SliceItem, StudyGroup};
pub(crate) use pixels::{
    DecodedFrame, DicomWindow, DisplayPixels, PixelProbeValue, load_dicom_frame_with_encoding,
    load_dicom_thumbnail, render_frame,
};
#[cfg(test)]
pub(crate) use pixels::{load_dicom_frame, write_rle_test_slice};
#[cfg(test)]
pub(crate) use scan::build_for_file;
pub(crate) use scan::{
    BuildProgress, build_for_file_with_encoding, build_for_inputs_with_progress_with_encoding,
    build_from_folder_with_progress_with_encoding,
};
pub(crate) use text_encoding::TextEncoding;
pub(crate) use volume::{
    PlaneImage, PlaneLayout, SeriesVolume, Volume, VolumeAxis, VolumeGeometry, auto_mpr_target_mm,
    build_series_volume, mpr_target_within_budget, slab_half_count,
};
