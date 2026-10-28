//! DICOM frame decoding, presentation transforms, and neutral RGBA output.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use dicom_core::header::HasLength;
use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_object::DefaultDicomObject;
use dicom_pixeldata::{
    ConvertOptions, DecodedPixelData, PhotometricInterpretation, PixelDecoder, VoiLutOption,
    WindowLevel,
};

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
    let opened = open_dicom_file_with_encoding(dicom_path, text_encoding)
        .with_context(|| format!("could not open DICOM file {}", dicom_path.display()))?;
    let dicom_object = opened.object;
    let metadata = extract_dicom_metadata(&dicom_object, opened.original_meta.as_ref());
    let frame = contain_decoder_panic(|| decode_frame(&dicom_object, frame_index))?;

    Ok(LoadedFrame { frame, metadata })
}

pub(crate) fn load_dicom_thumbnail(
    dicom_path: &Path,
    frame_index: u32,
    max_edge: u32,
) -> Result<DisplayPixels> {
    let dicom_object = open_dicom_file(dicom_path)
        .with_context(|| format!("could not open DICOM file {}", dicom_path.display()))?;
    let decoded = contain_decoder_panic(|| decode_pixel_frame(&dicom_object, frame_index))?;
    let overlay_bitmaps = read_overlay_bitmaps(&dicom_object, frame_index);
    let rgba = contain_decoder_panic(|| render_decoded_frame(&decoded, None, &overlay_bitmaps))?;
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

/// Run code that calls into the pixel decoders, turning a panic inside them
/// into an error.
///
/// Some codecs behind dicom-pixeldata index out of bounds on malformed input,
/// such as an RLE fragment cut short by an interrupted copy, instead of
/// returning an error. The 2D viewer decodes on the UI thread, where that
/// panic would close the application.
pub(crate) fn contain_decoder_panic<T>(decode: impl FnOnce() -> Result<T>) -> Result<T> {
    // Nothing the closure built survives a panic, so no broken state is seen.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(decode))
        .unwrap_or_else(|_| Err(anyhow!("the pixel data is damaged or not supported")))
}

/// Whether any frame of a JPEG image sets a restart interval. jpeg-decoder
/// 0.3.2 resets only its Huffman state at the restart markers of a lossless
/// scan and keeps predicting across them, so every interval after the first
/// decodes offset by a constant, with no error.
fn uses_jpeg_restart_intervals(dicom_object: &DefaultDicomObject) -> bool {
    dicom_object
        .element(tags::PIXEL_DATA)
        .ok()
        .and_then(|element| element.value().fragments())
        .is_some_and(|fragments| {
            fragments.iter().any(|fragment| {
                jpeg_restart_interval(fragment).is_some_and(|interval| interval > 0)
            })
        })
}

/// The restart interval a JPEG stream defines before its first scan, if the
/// stream starts one here.
fn jpeg_restart_interval(stream: &[u8]) -> Option<u16> {
    if !stream.starts_with(&[0xFF, 0xD8]) {
        return None;
    }

    let mut position = 2;
    loop {
        // Any number of fill bytes may precede a marker.
        while stream.get(position + 1) == Some(&0xFF) {
            position += 1;
        }
        let (0xFF, marker) = (*stream.get(position)?, *stream.get(position + 1)?) else {
            return None;
        };
        let length = usize::from(u16::from_be_bytes([
            *stream.get(position + 2)?,
            *stream.get(position + 3)?,
        ]));

        match marker {
            0xDD => {
                return Some(u16::from_be_bytes([
                    *stream.get(position + 4)?,
                    *stream.get(position + 5)?,
                ]));
            }
            // The first scan starts without a restart interval.
            0xDA => return None,
            _ => position += 2 + length,
        }
    }
}

/// The frame as an uncompressed copy of the object when it is 8-bit grayscale
/// RLE. dicom-transfer-syntax-registry 0.9.1 decodes that one sample late: for
/// one sample per pixel it starts writing at `samples_per_pixel - byte_offset`,
/// which is 1 for 8-bit data, so the first pixel reads 0, the last is lost and
/// every row begins with the previous row's last pixel. The single segment such
/// a frame holds is unpacked here instead, and dicom-pixeldata decodes the copy
/// as native pixel data with the object's own attributes.
fn native_8_bit_rle_frame(
    dicom_object: &DefaultDicomObject,
    frame_index: u32,
) -> Result<Option<DefaultDicomObject>> {
    if first_parsed::<u16>(dicom_object, "BitsAllocated") != Some(8)
        || first_parsed::<u16>(dicom_object, "SamplesPerPixel").unwrap_or(1) != 1
    {
        return Ok(None);
    }
    let (Some(rows), Some(columns)) = (
        first_parsed::<usize>(dicom_object, "Rows"),
        first_parsed::<usize>(dicom_object, "Columns"),
    ) else {
        return Ok(None);
    };
    let Some(fragment) = dicom_object
        .element(tags::PIXEL_DATA)
        .ok()
        .and_then(|element| element.value().fragments())
        .and_then(|fragments| fragments.get(frame_index as usize))
    else {
        return Ok(None);
    };
    // One segment, starting after the 64 byte header, is all 8-bit grayscale
    // data has; anything else is left to the library.
    let header = |index: usize| {
        fragment
            .get(index * 4..index * 4 + 4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
    };
    let (Some(1), Some(segment_start)) = (header(0), header(1)) else {
        return Ok(None);
    };

    let samples = fragment
        .get(segment_start..)
        .and_then(|segment| unpack_bits(segment, rows * columns))
        .context("the RLE pixel data is damaged")?;
    let mut native = dicom_object.clone();
    native.put_element(DataElement::new(
        tags::PIXEL_DATA,
        VR::OB,
        PrimitiveValue::from(samples),
    ));
    native.put_element(DataElement::new(
        tags::NUMBER_OF_FRAMES,
        VR::IS,
        PrimitiveValue::from("1"),
    ));
    native.meta_mut().transfer_syntax = uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned();

    Ok(Some(native))
}

/// The first `length` bytes a PackBits run sequence expands to, or `None` when
/// it ends short of them.
fn unpack_bits(packed: &[u8], length: usize) -> Option<Vec<u8>> {
    let mut packed = packed.iter().copied();
    let mut unpacked = Vec::with_capacity(length);

    while unpacked.len() < length {
        match packed.next()? as i8 {
            -128 => {}
            literal @ 0.. => {
                for _ in 0..=literal {
                    unpacked.push(packed.next()?);
                }
            }
            repeat => {
                let byte = packed.next()?;
                unpacked.extend(std::iter::repeat_n(
                    byte,
                    (1 - isize::from(repeat)) as usize,
                ));
            }
        }
    }

    unpacked.truncate(length);
    Some(unpacked)
}

fn decode_frame(dicom_object: &DefaultDicomObject, frame_index: u32) -> Result<DecodedFrame> {
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
    // dicom-pixeldata rejects a zero-length optional VOI LUT Function. Apply its missing-value
    // behavior on a decoding copy, keeping the loaded source dataset and its empty tag intact.
    if object
        .element(tags::VOILUT_FUNCTION)
        .is_ok_and(|element| element.length().get() == Some(0))
    {
        let mut decoding_object = object.clone();
        decoding_object.remove_element(tags::VOILUT_FUNCTION);
        return decode_pixel_frame_inner(&decoding_object, frame).map(|decoded| decoded.to_owned());
    }
    decode_pixel_frame_inner(object, frame)
}

fn decode_pixel_frame_inner(
    object: &DefaultDicomObject,
    frame: u32,
) -> Result<DecodedPixelData<'_>> {
    let transfer_syntax = object.meta().transfer_syntax();
    if matches!(
        transfer_syntax,
        uids::JPEG_LOSSLESS | uids::JPEG_LOSSLESS_SV1
    ) && uses_jpeg_restart_intervals(object)
    {
        bail!(
            "this JPEG Lossless image uses restart intervals, which the JPEG decoder reads wrongly"
        );
    }
    if transfer_syntax == uids::RLE_LOSSLESS
        && let Some(native) = native_8_bit_rle_frame(object, frame)?
    {
        return Ok(native.decode_pixel_data_frame(0)?.to_owned());
    }
    let result = if object.meta().transfer_syntax.trim_end_matches(['\0', ' '])
        == jpeg_extended::TRANSFER_SYNTAX
    {
        jpeg_extended::decode_frame(object, frame)
    } else {
        object.decode_pixel_data_frame(frame).map_err(Into::into)
    };
    result.with_context(|| format!("could not decode DICOM pixel data frame {}", frame + 1))
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
    let rgba = contain_decoder_panic(|| {
        render_decoded_frame(&frame.decoded, window, &frame.overlay_bitmaps)
    })?;

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

    // Overlay-only reports have no intensity range to normalize. Keep their zero-filled
    // MONOCHROME2 background black so white overlay content remains visible.
    let black_overlay_background = matches!(&voi_lut, VoiLutOption::Default)
        && !overlay_bitmaps.is_empty()
        && matches!(
            decoded.photometric_interpretation(),
            PhotometricInterpretation::Monochrome2
        )
        && decoded.window()?.is_none()
        && decoded.voi_lut_sequence()?.is_none()
        && !decoded.data().is_empty()
        && decoded.data().iter().all(|&byte| byte == 0);

    let mut rgba = if black_overlay_background {
        image::RgbaImage::from_pixel(
            decoded.columns(),
            decoded.rows(),
            image::Rgba([0, 0, 0, 255]),
        )
    } else {
        let convert_options = ConvertOptions::new().with_voi_lut(voi_lut).force_8bit();
        decoded
            .to_dynamic_image_with_options(0, &convert_options)
            .context("could not convert DICOM pixel data to image")?
            .into_rgba8()
    };
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
fn write_rle_test_slice(path: &Path, position: &str, bits_allocated: u16, kept_fraction: f64) {
    use dicom_core::value::PixelFragmentSequence;
    use dicom_object::{FileDicomObject, FileMetaTableBuilder};

    const SIDE: u16 = 64;
    // RLE stores each byte of a sample in a segment of its own, most
    // significant first; each segment is written here as literal runs of 128
    // bytes, after a 64 byte header that lists where the segments start.
    let segments: Vec<Vec<u8>> = if bits_allocated == 8 {
        vec![(0..SIDE * SIDE).map(|index| (index % 251) as u8).collect()]
    } else {
        let samples: Vec<u16> = (0..SIDE * SIDE).map(|index| index * 7).collect();
        (0..2)
            .map(|byte| {
                samples
                    .iter()
                    .map(|sample| sample.to_be_bytes()[byte])
                    .collect()
            })
            .collect()
    };
    let mut rle = Vec::new();
    rle.extend_from_slice(&(segments.len() as u32).to_le_bytes());
    let mut segment_start = 64;
    let packed: Vec<Vec<u8>> = segments
        .iter()
        .map(|bytes| {
            bytes
                .chunks(128)
                .flat_map(|run| std::iter::once((run.len() - 1) as u8).chain(run.iter().copied()))
                .collect()
        })
        .collect();
    for segment in &packed {
        rle.extend_from_slice(&(segment_start as u32).to_le_bytes());
        segment_start += segment.len();
    }
    rle.resize(64, 0);
    for segment in &packed {
        rle.extend_from_slice(segment);
    }

    let meta = FileMetaTableBuilder::new()
        .transfer_syntax(uids::RLE_LOSSLESS)
        .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
        .media_storage_sop_instance_uid("2.25.700")
        .build()
        .unwrap();
    let mut object = FileDicomObject::new_empty_with_meta(meta);
    for (tag, value) in [
        (tags::ROWS, SIDE),
        (tags::COLUMNS, SIDE),
        (tags::SAMPLES_PER_PIXEL, 1),
        (tags::BITS_ALLOCATED, bits_allocated),
        (tags::BITS_STORED, bits_allocated),
        (tags::HIGH_BIT, bits_allocated - 1),
        (tags::PIXEL_REPRESENTATION, 0),
    ] {
        object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
    }
    object.put_element(DataElement::new(
        tags::PHOTOMETRIC_INTERPRETATION,
        VR::CS,
        PrimitiveValue::from("MONOCHROME2"),
    ));
    for (tag, value) in [
        (tags::IMAGE_POSITION_PATIENT, position),
        (tags::IMAGE_ORIENTATION_PATIENT, "1\\0\\0\\0\\1\\0"),
        (tags::PIXEL_SPACING, "1\\1"),
    ] {
        object.put_element(DataElement::new(tag, VR::DS, PrimitiveValue::from(value)));
    }
    object.put_element(DataElement::new(
        tags::PIXEL_DATA,
        VR::OB,
        PixelFragmentSequence::new(Vec::<u32>::new(), vec![rle]),
    ));
    object.write_to_file(path).unwrap();

    let length = std::fs::metadata(path).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len((length as f64 * kept_fraction) as u64)
        .unwrap();
}

#[cfg(test)]
mod tests {
    use dicom_core::header::HasLength;
    use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
    use dicom_dictionary_std::{tags, uids};
    use dicom_object::{DefaultDicomObject, FileDicomObject, FileMetaTableBuilder};
    use dicom_transfer_syntax_registry::{TransferSyntaxIndex, TransferSyntaxRegistry};

    use super::{
        PixelProbeValue, decode_frame, finite_value_range, jpeg_restart_interval, load_dicom_frame,
        load_dicom_thumbnail, render_frame, thumbnail_dimensions, write_rle_test_slice,
    };

    /// A compressed file cut short must fail to open, not take the
    /// application down with a panic inside the decoder.
    #[test]
    fn damaged_compressed_pixel_data_is_an_error_not_a_crash() {
        let directory = std::env::temp_dir();
        let intact = directory.join(format!("dicron-pixels-rle-{}.dcm", std::process::id()));
        let truncated = directory.join(format!(
            "dicron-pixels-rle-truncated-{}.dcm",
            std::process::id()
        ));
        write_rle_test_slice(&intact, "0\\0\\0", 16, 1.0);
        write_rle_test_slice(&truncated, "0\\0\\0", 16, 0.7);

        let loaded = load_dicom_frame(&intact, 0);
        let damaged = load_dicom_frame(&truncated, 0);
        let damaged_thumbnail = load_dicom_thumbnail(&truncated, 0, 32);
        let _ = std::fs::remove_file(&intact);
        let _ = std::fs::remove_file(&truncated);

        // The intact fixture really is RLE that decodes to its samples.
        let loaded = loaded.unwrap();
        assert_eq!(
            loaded.frame.pixel_probe(1, 1),
            Some(PixelProbeValue::Monochrome(65.0 * 7.0))
        );
        let error = format!("{:#}", damaged.err().expect("the damaged file opened"));
        assert!(error.contains("damaged"), "{error}");
        assert!(damaged_thumbnail.is_err());
    }

    /// 8-bit RLE is unpacked in place: the library alone would put every
    /// sample one pixel late.
    #[test]
    fn eight_bit_rle_decodes_every_sample_in_place() {
        let directory = std::env::temp_dir();
        let intact = directory.join(format!("dicron-pixels-rle8-{}.dcm", std::process::id()));
        let truncated = directory.join(format!(
            "dicron-pixels-rle8-truncated-{}.dcm",
            std::process::id()
        ));
        write_rle_test_slice(&intact, "0\\0\\0", 8, 1.0);
        write_rle_test_slice(&truncated, "0\\0\\0", 8, 0.7);

        let loaded = load_dicom_frame(&intact, 0);
        let thumbnail = load_dicom_thumbnail(&intact, 0, 64);
        let damaged = load_dicom_frame(&truncated, 0);
        let _ = std::fs::remove_file(&intact);
        let _ = std::fs::remove_file(&truncated);

        let loaded = loaded.unwrap();
        for (x, y, expected) in [(0, 0, 0.0), (1, 1, 65.0), (63, 63, 79.0)] {
            assert_eq!(
                loaded.frame.pixel_probe(x, y),
                Some(PixelProbeValue::Monochrome(expected)),
                "({x}, {y})"
            );
        }
        assert!(thumbnail.is_ok());
        let error = format!("{:#}", damaged.err().expect("the damaged file opened"));
        assert!(error.contains("damaged"), "{error}");
    }

    #[test]
    fn restart_intervals_are_read_from_the_stream_header() {
        let header = |segments: &[&[u8]]| -> Vec<u8> {
            let mut stream = vec![0xFF, 0xD8];
            for segment in segments {
                stream.extend_from_slice(segment);
            }
            stream
        };
        let frame_header = [0xFF, 0xC3, 0x00, 0x05, 0x0C, 0x00, 0x01];
        let restart = [0xFF, 0xDD, 0x00, 0x04, 0x00, 0x08];
        let no_restart = [0xFF, 0xDD, 0x00, 0x04, 0x00, 0x00];
        let scan = [0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xDD, 0x00, 0x04, 0x00, 0x08];

        assert_eq!(
            jpeg_restart_interval(&header(&[&frame_header, &restart, &scan])),
            Some(8)
        );
        assert_eq!(
            jpeg_restart_interval(&header(&[&frame_header, &no_restart, &scan])),
            Some(0)
        );
        // A marker inside the scan is not a header.
        assert_eq!(
            jpeg_restart_interval(&header(&[&frame_header, &scan])),
            None
        );
        assert_eq!(jpeg_restart_interval(&restart), None);
        assert_eq!(jpeg_restart_interval(&header(&[&frame_header])), None);
    }

    /// A JPEG Lossless image that uses restart intervals is refused before the
    /// decoder can turn every other band of it into wrong values.
    #[test]
    fn jpeg_lossless_with_restart_intervals_is_refused() {
        use dicom_core::value::PixelFragmentSequence;
        use dicom_object::{FileDicomObject, FileMetaTableBuilder};

        let path = std::env::temp_dir().join(format!(
            "dicron-pixels-jpeg-restart-{}.dcm",
            std::process::id()
        ));
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(uids::JPEG_LOSSLESS_SV1)
            .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
            .media_storage_sop_instance_uid("2.25.701")
            .build()
            .unwrap();
        let mut object = FileDicomObject::new_empty_with_meta(meta);
        for (tag, value) in [
            (tags::ROWS, 2_u16),
            (tags::COLUMNS, 2),
            (tags::SAMPLES_PER_PIXEL, 1),
            (tags::BITS_ALLOCATED, 16),
            (tags::BITS_STORED, 12),
            (tags::HIGH_BIT, 11),
            (tags::PIXEL_REPRESENTATION, 0),
        ] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("MONOCHROME2"),
        ));
        let stream = vec![
            0xFF, 0xD8, 0xFF, 0xDD, 0x00, 0x04, 0x00, 0x01, 0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xD9,
        ];
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new(Vec::<u32>::new(), vec![stream]),
        ));
        object.write_to_file(&path).unwrap();

        let loaded = load_dicom_frame(&path, 0);
        let _ = std::fs::remove_file(&path);

        let error = format!("{:#}", loaded.err().expect("the image was decoded"));
        assert!(error.contains("restart intervals"), "{error}");
    }

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
        let object = black_two_by_two_image_with_overlay_pixel();
        let frame = decode_frame(&object, 0).unwrap();
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
    fn zero_overlay_background_respects_custom_and_file_windows() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        let frame = decode_frame(&object, 0).unwrap();
        let pixels = render_frame(
            &frame,
            Some(super::DicomWindow {
                center: 0.0,
                width: 1.0,
            }),
        )
        .unwrap();
        assert_eq!(pixels.rgba, vec![255; 16]);

        object.put_element(DataElement::new(tags::WINDOW_CENTER, VR::DS, "0"));
        object.put_element(DataElement::new(tags::WINDOW_WIDTH, VR::DS, "1"));
        let frame = decode_frame(&object, 0).unwrap();
        assert_eq!(render_frame(&frame, None).unwrap().rgba, vec![255; 16]);
    }

    #[test]
    fn zero_overlay_background_respects_declared_voi_lut() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        let lut = dicom_object::InMemDicomObject::from_element_iter([
            DataElement::new(
                tags::LUT_DESCRIPTOR,
                VR::US,
                PrimitiveValue::U16(vec![1_u16, 0, 8].into()),
            ),
            DataElement::new(
                tags::LUT_DATA,
                VR::OW,
                PrimitiveValue::U16(vec![255_u16].into()),
            ),
        ]);
        object.put_element(DataElement::new(
            tags::VOILUT_SEQUENCE,
            VR::SQ,
            dicom_core::value::Value::new_sequence(vec![lut], dicom_core::Length::UNDEFINED),
        ));
        let frame = decode_frame(&object, 0).unwrap();
        assert_eq!(render_frame(&frame, None).unwrap().rgba, vec![255; 16]);
    }

    #[test]
    fn zero_frame_without_overlay_keeps_library_rendering() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        object.remove_element(Tag(0x6000, 0x3000));
        let frame = decode_frame(&object, 0).unwrap();
        assert_eq!(render_frame(&frame, None).unwrap().rgba, vec![255; 16]);
    }

    #[test]
    fn nonzero_overlay_frame_keeps_normal_rendering() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from(vec![0_u8, 64, 128, 255]),
        ));
        let frame = decode_frame(&object, 0).unwrap();
        let pixels = render_frame(&frame, None).unwrap();
        assert_eq!(&pixels.rgba[..4], &[0, 0, 0, 255]);
        assert!(pixels.rgba[4] > 0 && pixels.rgba[4] < pixels.rgba[8]);
        assert!(pixels.rgba[8] < 255);
        assert_eq!(&pixels.rgba[12..], &[255, 255, 255, 255]);
    }

    #[test]
    fn sixteen_bit_zero_overlay_frame_keeps_black_background() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        for (tag, value) in [
            (tags::BITS_ALLOCATED, 16_u16),
            (tags::BITS_STORED, 16),
            (tags::HIGH_BIT, 15),
        ] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            PrimitiveValue::U16(vec![0_u16; 4].into()),
        ));
        let frame = decode_frame(&object, 0).unwrap();
        let pixels = render_frame(&frame, None).unwrap();
        assert_eq!(
            &pixels.rgba[..12],
            &[0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]
        );
        assert_eq!(&pixels.rgba[12..], &[255, 255, 255, 255]);
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

        let frame = decode_frame(&object, 0).unwrap();
        assert_eq!(
            (render_frame(&frame, None).unwrap().width, frame.frame_count),
            (2, 1)
        );
        assert_eq!(
            object
                .element(tags::VOILUT_FUNCTION)
                .unwrap()
                .length()
                .get(),
            Some(0)
        );

        let path = std::env::temp_dir().join(format!(
            "dicron-empty-voi-lut-function-{}.dcm",
            std::process::id()
        ));
        object.write_to_file(&path).unwrap();
        let original_bytes = std::fs::read(&path).unwrap();
        let loaded = load_dicom_frame(&path, 0).unwrap();
        assert_eq!(render_frame(&loaded.frame, None).unwrap().height, 2);
        let voi_lut = loaded
            .metadata
            .all_items
            .iter()
            .find(|item| item.tag == "(0028,1056)")
            .unwrap();
        assert_eq!(voi_lut.value, "-");
        let thumbnail = load_dicom_thumbnail(&path, 0, 128).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (2, 2));
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
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
