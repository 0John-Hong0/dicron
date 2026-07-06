use std::path::Path;

use anyhow::{Context, Result};
use dicom_object::{DefaultDicomObject, open_file};
use dicom_pixeldata::{ConvertOptions, DecodedPixelData, PixelDecoder, VoiLutOption, WindowLevel};
use eframe::egui;

use super::value as dicom_value;
use crate::metadata::{DicomMetadata, extract_dicom_metadata};
use crate::texture::color_image_from_dynamic_image;

#[derive(Clone, Copy)]
pub struct DicomWindow {
    pub center: f64,
    pub width: f64,
}

/// A single decoded frame plus everything needed to (re)window it without
/// touching the disk again. Re-applying a window/level is a cheap LUT pass over
/// the cached `decoded` samples, not a fresh open + decompress.
pub struct DecodedFrame {
    decoded: DecodedPixelData<'static>,
    pub frame_count: u32,
    /// The file's own WindowCenter/WindowWidth, when present and finite.
    pub default_window: Option<DicomWindow>,
    /// Rescaled (modality-LUT) value range, used to seed a sane default window
    /// when the file carries none and to bound interactive windowing.
    pub value_range: (f64, f64),
}

impl DecodedFrame {
    /// Default window for the UI readout / reset: the file's own window when
    /// present, otherwise a full-range window derived from the data.
    pub fn default_center_width(&self) -> (f64, f64) {
        if let Some(window) = self.default_window {
            return (window.center, window.width.max(1.0));
        }

        let (minimum, maximum) = self.value_range;
        let width = (maximum - minimum).max(1.0);
        let center = minimum + width / 2.0;

        (center, width)
    }
}

pub struct LoadedFrame {
    pub frame: DecodedFrame,
    pub metadata: DicomMetadata,
}

/// Open a DICOM file, extract its metadata, and decode a single frame.
/// This is the expensive step (disk read + decompress); callers cache the
/// result and use [`render_frame`] for window/level changes.
pub fn load_dicom_frame(dicom_path: &Path, frame_index: u32) -> Result<LoadedFrame> {
    let dicom_object = open_file(dicom_path)
        .with_context(|| format!("could not open DICOM file {}", dicom_path.display()))?;

    let metadata = extract_dicom_metadata(&dicom_object);
    let frame = decode_frame(&dicom_object, frame_index)?;

    Ok(LoadedFrame { frame, metadata })
}

fn decode_frame(dicom_object: &DefaultDicomObject, frame_index: u32) -> Result<DecodedFrame> {
    let decoded = dicom_object
        .decode_pixel_data_frame(frame_index)
        .with_context(|| {
            format!(
                "could not decode DICOM pixel data frame {}",
                frame_index + 1
            )
        })?
        .to_owned();

    let frame_count = dicom_value::first_parsed::<u32>(dicom_object, "NumberOfFrames")
        .unwrap_or(1)
        .max(1);

    Ok(DecodedFrame {
        decoded,
        frame_count,
        default_window: read_default_window(dicom_object),
        value_range: compute_value_range(dicom_object),
    })
}

/// Convert a cached decoded frame to an image with the requested window/level.
/// `window == None` defers to the file's own VOI (embedded window or VOI LUT
/// sequence, falling back to min-max normalization) instead of fabricating a
/// fixed window, which is correct across CT/MR/PET and arbitrary bit depths.
pub fn render_frame(frame: &DecodedFrame, window: Option<DicomWindow>) -> Result<egui::ColorImage> {
    let voi_lut = match window {
        Some(window) if window.center.is_finite() && window.width.is_finite() => {
            VoiLutOption::Custom(WindowLevel {
                center: window.center,
                width: window.width.max(1.0),
            })
        }
        _ => VoiLutOption::Default,
    };

    let convert_options = ConvertOptions::new().with_voi_lut(voi_lut).force_8bit();

    let dynamic_image = frame
        .decoded
        .to_dynamic_image_with_options(0, &convert_options)
        .context("could not convert DICOM pixel data to image")?;

    Ok(color_image_from_dynamic_image(&dynamic_image))
}

fn read_default_window(dicom_object: &DefaultDicomObject) -> Option<DicomWindow> {
    let center = dicom_value::first_parsed::<f64>(dicom_object, "WindowCenter")?;
    let width = dicom_value::first_parsed::<f64>(dicom_object, "WindowWidth")?;

    if !center.is_finite() || !width.is_finite() || width <= 0.0 {
        return None;
    }

    Some(DicomWindow { center, width })
}

/// Rescaled value range from BitsStored / PixelRepresentation and the modality
/// LUT (RescaleSlope/Intercept). Used to bound interactive windowing and to
/// derive a default window for files without WindowCenter/WindowWidth.
fn compute_value_range(dicom_object: &DefaultDicomObject) -> (f64, f64) {
    let bits_stored = dicom_value::first_parsed::<u32>(dicom_object, "BitsStored")
        .unwrap_or(16)
        .clamp(1, 32);
    let is_signed =
        dicom_value::first_parsed::<u32>(dicom_object, "PixelRepresentation").unwrap_or(0) == 1;
    let slope = finite_or(
        dicom_value::first_parsed::<f64>(dicom_object, "RescaleSlope"),
        1.0,
    );
    let intercept = finite_or(
        dicom_value::first_parsed::<f64>(dicom_object, "RescaleIntercept"),
        0.0,
    );

    let (stored_min, stored_max) = if is_signed {
        let half = 2_f64.powi(bits_stored as i32 - 1);
        (-half, half - 1.0)
    } else {
        (0.0, 2_f64.powi(bits_stored as i32) - 1.0)
    };

    let first = slope * stored_min + intercept;
    let second = slope * stored_max + intercept;

    (first.min(second), first.max(second))
}

fn finite_or(value: Option<f64>, fallback: f64) -> f64 {
    match value {
        Some(value) if value.is_finite() => value,
        _ => fallback,
    }
}
