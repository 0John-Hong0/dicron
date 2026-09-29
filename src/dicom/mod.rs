//! DICOM file scanning, indexing, metadata, geometry, and pixel processing.

mod index;
mod metadata;
mod model;
mod pixels;
mod scan;
mod text_encoding;

pub(crate) use metadata::{DicomMetadata, DicomOverlayMetadata, MetadataItem};
pub(crate) use model::{DicomIndex, PatientGroup, SliceItem, StudyGroup};
#[cfg(test)]
pub(crate) use pixels::load_dicom_frame;
pub(crate) use pixels::{
    DecodedFrame, DicomWindow, DisplayPixels, PixelProbeValue, load_dicom_frame_with_encoding,
    load_dicom_thumbnail, render_frame,
};
#[cfg(test)]
pub(crate) use scan::build_for_file;
pub(crate) use scan::{
    BuildProgress, build_for_file_with_encoding, build_for_inputs_with_progress_with_encoding,
    build_from_folder_with_progress_with_encoding,
};
pub(crate) use text_encoding::TextEncoding;
