//! DICOM frame decoding, presentation transforms, and neutral RGBA output.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use dicom_core::header::HasLength;
use dicom_dictionary_std::tags;
use dicom_object::DefaultDicomObject;
use dicom_pixeldata::{ConvertOptions, DecodedPixelData, PixelDecoder, VoiLutOption, WindowLevel};

use super::jpeg_extended;
use super::metadata::{DicomMetadata, extract_dicom_metadata};
use super::overlay_planes::{OverlayBitmap, paint_overlay_bitmaps, read_overlay_bitmaps};
use super::scan::{open_dicom_file, open_dicom_file_with_encoding};
use super::text_encoding::TextEncoding;

pub(crate) struct DisplayPixels {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) rgba: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(crate) struct DicomWindow {
    pub(crate) center: f64,
    pub(crate) width: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PixelProbeValue {
    Monochrome(f64),
    Rgb([u8; 3]),
}

enum PixelProbeData {
    Monochrome {
        width: usize,
        height: usize,
        values: Vec<f64>,
    },
    Rgb {
        width: usize,
        height: usize,
        values: Vec<u8>,
    },
}

/// A single decoded frame plus everything needed to (re)window it without
/// touching the disk again. Re-applying a window/level is a cheap LUT pass over
/// the cached `decoded` samples, not a fresh open + decompress.
pub(crate) struct DecodedFrame {
    decoded: DecodedPixelData<'static>,
    overlay_bitmaps: Vec<OverlayBitmap>,
    pixel_probe: Option<PixelProbeData>,
    pub(crate) frame_count: u32,
    /// The file's own WindowCenter/WindowWidth, when present and finite.
    pub(crate) default_window: Option<DicomWindow>,
    /// Rescaled (modality-LUT) value range, used to seed a sane default window
    /// when the file carries none and to bound interactive windowing.
    pub(crate) value_range: (f64, f64),
    window_level_available: bool,
}

impl DecodedFrame {
    /// Default window for the UI readout / reset: the file's own window when
    /// present, otherwise a full-range window derived from the data.
    pub(crate) fn default_center_width(&self) -> (f64, f64) {
        if let Some(window) = self.default_window {
            return (window.center, window.width.max(1.0));
        }

        let (minimum, maximum) = self.value_range;
        let width = (maximum - minimum).max(1.0);
        let center = minimum + width / 2.0;

        (center, width)
    }

    pub(crate) fn full_dynamic_window(&self) -> Option<DicomWindow> {
        if !self.window_level_available {
            return None;
        }

        let values = self.decoded.to_vec_frame::<f64>(0).ok()?;
        let (minimum, maximum) = finite_value_range(&values)?;

        Some(DicomWindow {
            center: minimum / 2.0 + maximum / 2.0,
            width: (maximum - minimum).max(1.0),
        })
    }

    pub(crate) fn window_level_available(&self) -> bool {
        self.window_level_available
    }

    pub(crate) fn pixel_probe(&self, x: usize, y: usize) -> Option<PixelProbeValue> {
        match self.pixel_probe.as_ref()? {
            PixelProbeData::Monochrome {
                width,
                height,
                values,
            } => {
                if x >= *width || y >= *height {
                    return None;
                }

                values
                    .get(y.checked_mul(*width)?.checked_add(x)?)
                    .copied()
                    .map(PixelProbeValue::Monochrome)
            }
            PixelProbeData::Rgb {
                width,
                height,
                values,
            } => {
                if x >= *width || y >= *height {
                    return None;
                }

                let offset = y.checked_mul(*width)?.checked_add(x)?.checked_mul(3)?;
                Some(PixelProbeValue::Rgb([
                    *values.get(offset)?,
                    *values.get(offset + 1)?,
                    *values.get(offset + 2)?,
                ]))
            }
        }
    }
}

pub(crate) struct LoadedFrame {
    pub(crate) frame: DecodedFrame,
    pub(crate) metadata: DicomMetadata,
}

/// Open a DICOM file, extract its metadata, and decode a single frame.
/// This is the expensive step (disk read + decompress); callers cache the
/// result and use [`render_frame`] for window/level changes.
#[cfg(test)]
pub(crate) fn load_dicom_frame(dicom_path: &Path, frame_index: u32) -> Result<LoadedFrame> {
    load_dicom_frame_with_encoding(dicom_path, frame_index, TextEncoding::DicomDefault)
}

pub(crate) fn load_dicom_frame_with_encoding(
    dicom_path: &Path,
    frame_index: u32,
    text_encoding: TextEncoding,
) -> Result<LoadedFrame> {
    let mut dicom_object = open_dicom_file_with_encoding(dicom_path, text_encoding)
        .with_context(|| format!("could not open DICOM file {}", dicom_path.display()))?;

    let metadata = extract_dicom_metadata(&dicom_object);
    let frame = decode_frame(&mut dicom_object, frame_index)?;

    Ok(LoadedFrame { frame, metadata })
}

pub(crate) fn load_dicom_thumbnail(
    dicom_path: &Path,
    frame_index: u32,
    max_edge: u32,
) -> Result<DisplayPixels> {
    let mut dicom_object = open_dicom_file(dicom_path)
        .with_context(|| format!("could not open DICOM file {}", dicom_path.display()))?;
    remove_empty_voi_lut_function(&mut dicom_object);
    let decoded = decode_pixel_frame(&dicom_object, frame_index)?;
    let overlay_bitmaps = read_overlay_bitmaps(&dicom_object, frame_index);
    let rgba = render_decoded_frame(&decoded, None, &overlay_bitmaps)?;
    let (width, height) = thumbnail_dimensions(rgba.width(), rgba.height(), max_edge.max(1));
    let rgba = if (width, height) == (rgba.width(), rgba.height()) {
        rgba
    } else {
        image::imageops::resize(&rgba, width, height, image::imageops::FilterType::Triangle)
    };

    Ok(DisplayPixels {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

fn decode_frame(dicom_object: &mut DefaultDicomObject, frame_index: u32) -> Result<DecodedFrame> {
    remove_empty_voi_lut_function(dicom_object);
    let decoded = decode_pixel_frame(dicom_object, frame_index)?.to_owned();

    let frame_count = first_parsed::<u32>(dicom_object, "NumberOfFrames")
        .unwrap_or(1)
        .max(1);

    let window_level_available = decoded.photometric_interpretation().is_monochrome();
    let pixel_probe = build_pixel_probe(&decoded);

    Ok(DecodedFrame {
        decoded,
        overlay_bitmaps: read_overlay_bitmaps(dicom_object, frame_index),
        pixel_probe,
        frame_count,
        default_window: read_default_window(dicom_object),
        value_range: compute_value_range(dicom_object),
        window_level_available,
    })
}

fn decode_pixel_frame(object: &DefaultDicomObject, frame: u32) -> Result<DecodedPixelData<'_>> {
    let result = if object.meta().transfer_syntax.trim_end_matches(['\0', ' '])
        == jpeg_extended::TRANSFER_SYNTAX
    {
        jpeg_extended::decode_frame(object, frame)
    } else {
        object.decode_pixel_data_frame(frame).map_err(Into::into)
    };
    result.with_context(|| format!("could not decode DICOM pixel data frame {}", frame + 1))
}

fn remove_empty_voi_lut_function(dicom_object: &mut DefaultDicomObject) {
    // A zero-length optional VOI LUT Function is equivalent to an absent value.
    // dicom-pixeldata rejects the empty element before decoding any pixels.
    if dicom_object
        .element(tags::VOILUT_FUNCTION)
        .is_ok_and(|element| element.length().get() == Some(0))
    {
        dicom_object.remove_element(tags::VOILUT_FUNCTION);
    }
}

fn build_pixel_probe(decoded: &DecodedPixelData<'_>) -> Option<PixelProbeData> {
    if decoded.photometric_interpretation().is_monochrome() {
        return Some(PixelProbeData::Monochrome {
            width: decoded.columns() as usize,
            height: decoded.rows() as usize,
            values: decoded.to_vec_frame::<f64>(0).ok()?,
        });
    }

    let image = decoded
        .to_dynamic_image_with_options(0, &ConvertOptions::new().force_8bit())
        .ok()?
        .to_rgb8();

    Some(PixelProbeData::Rgb {
        width: image.width() as usize,
        height: image.height() as usize,
        values: image.into_raw(),
    })
}

/// Convert a cached decoded frame to an image with the requested window/level.
/// `window == None` defers to the file's own VOI (embedded window or VOI LUT
/// sequence, falling back to min-max normalization) instead of fabricating a
/// fixed window, which is correct across CT/MR/PET and arbitrary bit depths.
pub(crate) fn render_frame(
    frame: &DecodedFrame,
    window: Option<DicomWindow>,
) -> Result<DisplayPixels> {
    let rgba = render_decoded_frame(&frame.decoded, window, &frame.overlay_bitmaps)?;

    Ok(DisplayPixels {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

fn render_decoded_frame(
    decoded: &DecodedPixelData<'_>,
    window: Option<DicomWindow>,
    overlay_bitmaps: &[OverlayBitmap],
) -> Result<image::RgbaImage> {
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

    let mut rgba = decoded
        .to_dynamic_image_with_options(0, &convert_options)
        .context("could not convert DICOM pixel data to image")?
        .into_rgba8();
    paint_overlay_bitmaps(overlay_bitmaps, &mut rgba);

    Ok(rgba)
}

fn thumbnail_dimensions(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    if width <= max_edge && height <= max_edge {
        return (width, height);
    }

    if width >= height {
        let scaled_height = (u64::from(height) * u64::from(max_edge) / u64::from(width)) as u32;
        (max_edge, scaled_height.max(1))
    } else {
        let scaled_width = (u64::from(width) * u64::from(max_edge) / u64::from(height)) as u32;
        (scaled_width.max(1), max_edge)
    }
}

fn read_default_window(dicom_object: &DefaultDicomObject) -> Option<DicomWindow> {
    let center = first_parsed::<f64>(dicom_object, "WindowCenter")?;
    let width = first_parsed::<f64>(dicom_object, "WindowWidth")?;

    if !center.is_finite() || !width.is_finite() || width <= 0.0 {
        return None;
    }

    Some(DicomWindow { center, width })
}

/// Rescaled value range from BitsStored / PixelRepresentation and the modality
/// LUT (RescaleSlope/Intercept). Used to bound interactive windowing and to
/// derive a default window for files without WindowCenter/WindowWidth.
fn compute_value_range(dicom_object: &DefaultDicomObject) -> (f64, f64) {
    let bits_stored = first_parsed::<u32>(dicom_object, "BitsStored")
        .unwrap_or(16)
        .clamp(1, 32);
    let is_signed = first_parsed::<u32>(dicom_object, "PixelRepresentation").unwrap_or(0) == 1;
    let slope = finite_or(first_parsed::<f64>(dicom_object, "RescaleSlope"), 1.0);
    let intercept = finite_or(first_parsed::<f64>(dicom_object, "RescaleIntercept"), 0.0);

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

fn finite_value_range(values: &[f64]) -> Option<(f64, f64)> {
    values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(None, |range, value| match range {
            Some((minimum, maximum)) => Some((minimum.min(value), maximum.max(value))),
            None => Some((value, value)),
        })
}

fn finite_or(value: Option<f64>, fallback: f64) -> f64 {
    match value {
        Some(value) if value.is_finite() => value,
        _ => fallback,
    }
}

fn first_parsed<T>(dicom_object: &DefaultDicomObject, keyword: &str) -> Option<T>
where
    T: FromStr,
{
    dicom_object
        .element_by_name(keyword)
        .ok()?
        .to_str()
        .ok()?
        .trim()
        .trim_matches('\0')
        .split('\\')
        .next()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
    use dicom_dictionary_std::{tags, uids};
    use dicom_object::{DefaultDicomObject, FileDicomObject, FileMetaTableBuilder};
    use dicom_transfer_syntax_registry::{TransferSyntaxIndex, TransferSyntaxRegistry};

    use super::{
        decode_frame, finite_value_range, load_dicom_frame, load_dicom_thumbnail, render_frame,
        thumbnail_dimensions,
    };

    fn black_two_by_two_image_with_overlay_pixel() -> DefaultDicomObject {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(uids::SECONDARY_CAPTURE_IMAGE_STORAGE)
            .media_storage_sop_instance_uid("2.25.303")
            .build()
            .unwrap();
        let mut object = FileDicomObject::new_empty_with_meta(meta);

        for (tag, value) in [
            (tags::ROWS, 2_u16),
            (tags::COLUMNS, 2_u16),
            (tags::SAMPLES_PER_PIXEL, 1_u16),
            (tags::BITS_ALLOCATED, 8_u16),
            (tags::BITS_STORED, 8_u16),
            (tags::HIGH_BIT, 7_u16),
            (tags::PIXEL_REPRESENTATION, 0_u16),
            (Tag(0x6000, 0x0010), 2_u16),
            (Tag(0x6000, 0x0011), 2_u16),
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
            PrimitiveValue::from(vec![0_u8; 4]),
        ));
        object.put_element(DataElement::new(
            Tag(0x6000, 0x3000),
            VR::OB,
            PrimitiveValue::from(vec![0b0000_1000_u8]),
        ));

        object
    }

    #[test]
    fn overlay_plane_appears_in_full_image_and_thumbnail_without_changing_the_probe() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        let frame = decode_frame(&mut object, 0).unwrap();
        let pixels = render_frame(&frame, None).unwrap();

        assert_eq!((pixels.width, pixels.height), (2, 2));
        assert_eq!(
            &pixels.rgba[..12],
            &[0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]
        );
        assert_eq!(&pixels.rgba[12..], &[255, 255, 255, 255]);
        assert_eq!(
            frame.pixel_probe(1, 1),
            Some(super::PixelProbeValue::Monochrome(0.0))
        );

        let path =
            std::env::temp_dir().join(format!("dicron-overlay-plane-{}.dcm", std::process::id()));
        object.write_to_file(&path).unwrap();
        let thumbnail = load_dicom_thumbnail(&path, 0, 2).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (2, 2));
        assert_eq!(thumbnail.rgba, pixels.rgba);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn empty_voi_lut_function_does_not_block_frame_or_thumbnail() {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(uids::SECONDARY_CAPTURE_IMAGE_STORAGE)
            .media_storage_sop_instance_uid("2.25.302")
            .build()
            .unwrap();
        let mut object = FileDicomObject::new_empty_with_meta(meta);
        for (tag, value) in [
            (tags::ROWS, 2_u16),
            (tags::COLUMNS, 2_u16),
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
            PrimitiveValue::from(vec![0_u8; 4]),
        ));
        object.put_element(DataElement::new(
            tags::VOILUT_FUNCTION,
            VR::CS,
            PrimitiveValue::Empty,
        ));

        let frame = decode_frame(&mut object, 0).unwrap();
        assert_eq!(
            (render_frame(&frame, None).unwrap().width, frame.frame_count),
            (2, 1)
        );
        assert!(object.element(tags::VOILUT_FUNCTION).is_err());

        let path = std::env::temp_dir().join(format!(
            "dicron-empty-voi-lut-function-{}.dcm",
            std::process::id()
        ));
        object.put_element(DataElement::new(
            tags::VOILUT_FUNCTION,
            VR::CS,
            PrimitiveValue::Empty,
        ));
        object.write_to_file(&path).unwrap();
        let loaded = load_dicom_frame(&path, 0).unwrap();
        assert_eq!(render_frame(&loaded.frame, None).unwrap().height, 2);
        let thumbnail = load_dicom_thumbnail(&path, 0, 128).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (2, 2));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn thumbnail_dimensions_fit_landscape_images_within_the_bound() {
        assert_eq!(thumbnail_dimensions(400, 200, 192), (192, 96));
    }

    #[test]
    fn thumbnail_dimensions_fit_portrait_images_within_the_bound() {
        assert_eq!(thumbnail_dimensions(200, 400, 192), (96, 192));
    }

    #[test]
    fn thumbnail_dimensions_do_not_enlarge_small_images() {
        assert_eq!(thumbnail_dimensions(64, 48, 192), (64, 48));
    }

    #[test]
    fn finite_value_range_ignores_non_finite_values() {
        assert_eq!(
            finite_value_range(&[f64::NAN, 12.0, -4.0, f64::INFINITY, 7.0]),
            Some((-4.0, 12.0))
        );
    }

    #[test]
    fn finite_value_range_requires_a_finite_value() {
        assert_eq!(finite_value_range(&[f64::NAN, f64::NEG_INFINITY]), None);
    }

    #[test]
    fn jpeg2000_lossless_has_a_pixel_decoder() {
        const JPEG2000_LOSSLESS_UID: &str = "1.2.840.10008.1.2.4.90";

        let transfer_syntax = TransferSyntaxRegistry
            .get(JPEG2000_LOSSLESS_UID)
            .expect("JPEG 2000 Lossless transfer syntax should be registered");

        assert!(transfer_syntax.pixel_data_reader().is_some());
    }
}
