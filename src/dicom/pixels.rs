//! DICOM frame decoding, presentation transforms, and neutral RGBA output.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail, ensure};
use dicom_core::header::{HasLength, Header};
use dicom_core::value::DataSetSequence;
use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::adapters::PixelDataObject;
use dicom_encoding::transfer_syntax::{Codec, TransferSyntaxIndex};
use dicom_object::DefaultDicomObject;
use dicom_pixeldata::{
    ConvertOptions, DecodedPixelData, PhotometricInterpretation, PixelDecoder, PixelRepresentation,
    VoiLutOption, WindowLevel,
};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;

use super::jpeg_extended;
use super::metadata::{DicomMetadata, extract_dicom_metadata};
use super::overlay_planes::{OverlayBitmap, paint_overlay_bitmaps, read_overlay_bitmaps};
use super::scan::{open_dicom_file, open_dicom_file_with_encoding};
use super::text_encoding::TextEncoding;

/// The most samples (Rows x Columns x SamplesPerPixel) a single frame of
/// compressed monochrome pixel data may hold. Compressed headers declare their
/// decoded size freely, and the decoders allocate it before reading the data.
/// The 2D view derives an f64 probe value (8 bytes) from every sample of a
/// monochrome frame and an RGBA pixel from every pixel, so counting samples
/// rather than decoded bytes bounds the decoded frame, the probe and the RGBA
/// image together, to about 2 GB at worst. 2^27 samples still admit every
/// realistic monochrome frame: mammography and stitched long-leg and spine
/// radiographs are well below it.
const MAX_DECODED_FRAME_SAMPLES: u64 = 1 << 27;

/// The most bytes a single frame of compressed colour pixel data may decode
/// to. A colour frame keeps an 8-bit RGB probe, no larger than the decoded
/// frame, and an RGBA image of 4/3 of it at 8 bits per sample, so this bounds
/// the decoded frame, the probe and the RGBA image together to about 1.8 GB.
/// 512 MiB admits 8-bit RGB photographs up to about 13,377 x 13,377 pixels
/// (179 MP), beyond any camera that writes DICOM.
const MAX_DECODED_COLOUR_FRAME_BYTES: u64 = 512 << 20;

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
    /// The decoded frame's own window, when usable.
    pub(crate) default_window: Option<DicomWindow>,
    /// The window the frame is drawn with when none is requested, if the file
    /// leaves it to the viewer; see [`implicit_window`].
    implicit_window: Option<DicomWindow>,
    /// Rescaled (modality-LUT) value range, used to bound interactive windowing
    /// and to seed a default window when neither the file nor the frame's
    /// drawing provides one.
    pub(crate) value_range: (f64, f64),
    window_level_available: bool,
}

impl DecodedFrame {
    /// Use the inferred display window in the UI when available. Otherwise use
    /// the file's window or derive the readout from the value range.
    pub(crate) fn default_center_width(&self) -> (f64, f64) {
        if let Some(window) = self.implicit_window.or(self.default_window) {
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

        let Some(PixelProbeData::Monochrome { values, .. }) = &self.pixel_probe else {
            return None;
        };
        finite_value_range(values).map(spanning_window)
    }

    /// Approximate bytes this frame keeps in memory: the decoded samples, the
    /// probe values, the overlay bitmaps and any VOI LUTs. The remaining
    /// fields are a few numbers.
    pub(crate) fn resident_bytes(&self) -> usize {
        let probe_bytes = match &self.pixel_probe {
            Some(PixelProbeData::Monochrome { values, .. }) => size_of_val(values.as_slice()),
            Some(PixelProbeData::Rgb { values, .. }) => values.len(),
            None => 0,
        };
        let overlay_bytes: usize = self
            .overlay_bitmaps
            .iter()
            .map(OverlayBitmap::byte_len)
            .sum();
        let voi_lut_bytes: usize = self
            .decoded
            .voi_lut_sequence()
            .ok()
            .flatten()
            .unwrap_or_default()
            .iter()
            .map(|lut| size_of_val(lut.data.as_slice()))
            .sum();

        self.decoded.data().len() + probe_bytes + overlay_bytes + voi_lut_bytes
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
    let overlay_bitmaps = read_overlay_bitmaps(
        &dicom_object,
        frame_index,
        decoded.columns(),
        decoded.rows(),
    );
    let rgba = contain_decoder_panic(|| {
        let implicit_window = implicit_window(&decoded, &overlay_bitmaps, || {
            decoded
                .to_vec_frame::<f64>(0)
                .context("could not read rescaled grayscale values")
        })?;
        render_decoded_frame(&decoded, None, implicit_window, &overlay_bitmaps)
    })?;
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

/// The size of one frame of compressed pixel data, as the pixel decoders read
/// it from the header before they allocate their output.
#[derive(Clone, Copy)]
struct FrameLayout {
    rows: u16,
    columns: u16,
    samples_per_pixel: u16,
    bits_allocated: u16,
}

impl FrameLayout {
    /// The layout of the object's frames, refused when an attribute is not a
    /// US value, as the decoders read them, or a frame would be larger than
    /// the viewer decodes: [`MAX_DECODED_COLOUR_FRAME_BYTES`] for a colour
    /// frame, one of three or more samples per pixel whose photometric
    /// interpretation is not monochrome, [`MAX_DECODED_FRAME_SAMPLES`] for any
    /// other.
    fn read_within_budget(object: &DefaultDicomObject) -> Result<Self> {
        let attribute = |tag, name| {
            object
                .element(tag)
                .ok()
                .and_then(|element| element.to_int::<u16>().ok())
                .with_context(|| format!("the image has no valid {name}"))
        };
        let layout = Self {
            rows: attribute(tags::ROWS, "Rows")?,
            columns: attribute(tags::COLUMNS, "Columns")?,
            samples_per_pixel: attribute(tags::SAMPLES_PER_PIXEL, "SamplesPerPixel")?,
            bits_allocated: attribute(tags::BITS_ALLOCATED, "BitsAllocated")?,
        };
        // The photometric interpretation as dicom-pixeldata reads it: the 2D
        // view keeps an f64 probe for the frames it calls monochrome.
        let colour = layout.samples_per_pixel >= 3
            && photometric_interpretation(object).is_some_and(|value| !value.is_monochrome());
        let samples = layout.pixels() * u64::from(layout.samples_per_pixel);
        if colour {
            let bytes = samples * layout.bytes_per_sample();
            ensure!(
                bytes <= MAX_DECODED_COLOUR_FRAME_BYTES,
                "the image is too large to display: a colour frame decodes to {bytes} bytes, \
                 more than the {MAX_DECODED_COLOUR_FRAME_BYTES} the viewer decodes"
            );
        } else {
            ensure!(
                samples <= MAX_DECODED_FRAME_SAMPLES,
                "the image is too large to display: a frame holds {samples} samples, \
                 more than the {MAX_DECODED_FRAME_SAMPLES} the viewer decodes"
            );
        }
        Ok(layout)
    }

    fn pixels(self) -> u64 {
        u64::from(self.rows) * u64::from(self.columns)
    }

    fn bytes_per_sample(self) -> u64 {
        u64::from(self.bits_allocated.div_ceil(8))
    }

    /// Native frame length, with one-bit samples packed into bytes.
    fn frame_bytes(self) -> u64 {
        let samples = self.pixels() * u64::from(self.samples_per_pixel);
        if self.bits_allocated == 1 {
            samples.div_ceil(8)
        } else {
            samples * self.bytes_per_sample()
        }
    }
}

fn photometric_interpretation(object: &DefaultDicomObject) -> Option<PhotometricInterpretation> {
    let value = object
        .element(tags::PHOTOMETRIC_INTERPRETATION)
        .ok()?
        .string()
        .ok()?;
    Some(PhotometricInterpretation::from(
        value.trim_matches(|c: char| c.is_whitespace() || c == '\0'),
    ))
}

/// Validate the segments needed to reconstruct a frame before allocating
/// output or slicing the fragment at the declared offsets.
fn rle_segment_starts(fragment: &[u8], layout: FrameLayout) -> Result<Vec<usize>> {
    const MAX_SEGMENTS: usize = 15;
    const HEADER_BYTES: usize = 64;
    // A two-byte PackBits repeat run stands for 128 bytes, the most any run
    // expands to; a literal run never expands.
    const MAX_PACKBITS_EXPANSION: u64 = 64;
    // A one-byte literal run takes two bytes, the most any byte costs; an
    // encoder may pad a segment to an even length with one more.
    const MAX_PACKBITS_BYTES_PER_BYTE: u64 = 2;

    let header_value = |index: usize| {
        fragment
            .get(index * 4..index * 4 + 4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
            .context("the RLE pixel data is damaged: its header is cut short")
    };
    let segment_count = header_value(0)?;
    // Each byte of each sample has a segment of its own.
    let needed = usize::from(layout.samples_per_pixel) * layout.bytes_per_sample() as usize;
    ensure!(
        (needed.max(1)..=MAX_SEGMENTS).contains(&segment_count),
        "the RLE pixel data is damaged: its header lists {segment_count} segments \
         where {needed} are needed"
    );
    let starts = (1..=segment_count)
        .map(header_value)
        .collect::<Result<Vec<_>>>()?;
    // Each segment ends where the next one starts, the last at the end of the
    // fragment. We read only the needed segments, so the segments
    // after them matter only where the first one marks the end of the last
    // needed one.
    let ends = starts[1..].iter().copied().chain([fragment.len()]);
    let needed_segments: Vec<(usize, usize)> =
        starts.iter().copied().zip(ends).take(needed).collect();
    ensure!(
        starts[0] >= HEADER_BYTES
            && needed_segments
                .iter()
                .all(|&(start, end)| start < end && end <= fragment.len()),
        "the RLE pixel data is damaged: its segment offsets are out of order \
         or outside the fragment"
    );
    let lengths: Vec<u64> = needed_segments
        .iter()
        .map(|&(start, end)| (end - start) as u64)
        .collect();
    ensure!(
        lengths
            .iter()
            .all(|&length| length * MAX_PACKBITS_EXPANSION >= layout.pixels()),
        "the RLE pixel data is damaged: a segment is too short for a {} x {} image",
        layout.columns,
        layout.rows
    );
    ensure!(
        lengths
            .iter()
            .all(|&length| length <= MAX_PACKBITS_BYTES_PER_BYTE * layout.pixels() + 1),
        "the RLE pixel data is damaged: a segment is longer than a {} x {} image needs",
        layout.columns,
        layout.rows
    );
    Ok(starts)
}

/// Inflate a frame once, limiting the output to its declared size plus one byte.
fn inflate_frame(
    fragment: &[u8],
    layout: FrameLayout,
    expected: u64,
    frame: u32,
) -> Result<Vec<u8>> {
    // Deflated Image Frame Compression is raw deflate, as the data set codec
    // of Deflated Explicit VR Little Endian is; the library builds both with
    // its 'deflate' feature, so without that codec it has no decoder for
    // these frames either.
    let Some(Codec::Dataset(Some(inflater))) = TransferSyntaxRegistry
        .get(uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN)
        .map(|transfer_syntax| transfer_syntax.codec())
    else {
        bail!("this build has no decoder for Deflated Image Frame Compression");
    };
    let mut samples = Vec::new();
    std::io::copy(
        &mut std::io::Read::take(inflater.adapt_reader(Box::new(fragment)), expected + 1),
        &mut samples,
    )
    .context("the deflated pixel data is damaged")?;
    ensure!(
        samples.len() as u64 <= expected,
        "the deflated pixel data is damaged: a frame inflates to more than the {expected} bytes \
         of a {} x {} image",
        layout.columns,
        layout.rows
    );
    ensure!(
        samples.len() as u64 == expected,
        "the pixel data holds no frame {}",
        frame + 1
    );
    Ok(samples)
}

/// Whether a frame of a JPEG image sets a restart interval, read from the
/// frame's fragments joined, as the decoder reads them. jpeg-decoder 0.3.2
/// resets only its Huffman state at the restart markers of a lossless scan
/// and keeps predicting across them, so every interval after the first
/// decodes offset by a constant, with no error.
fn uses_jpeg_restart_intervals(dicom_object: &DefaultDicomObject, frame: u32) -> bool {
    dicom_object
        .frame_pixel_data(frame)
        .and_then(|stream| jpeg_restart_interval(&stream))
        .is_some_and(|interval| interval > 0)
}

/// The restart interval a JPEG stream defines before its first scan, if the
/// stream starts one here.
fn jpeg_restart_interval(stream: &[u8]) -> Option<u16> {
    let (_, interval) = jpeg_header_segments(stream).find(|&(marker, _)| marker == 0xDD)?;
    Some(u16::from_be_bytes(interval.get(..2)?.try_into().ok()?))
}

/// The marker segments of a JPEG or JPEG-LS stream through its first scan header, as
/// marker and contents, or none when the stream does not start with SOI. Like
/// jpeg-decoder, bytes between segments and fill bytes before a marker are
/// skipped.
fn jpeg_header_segments(stream: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut position = if stream.starts_with(&[0xFF, 0xD8]) {
        2
    } else {
        stream.len()
    };
    std::iter::from_fn(move || {
        let marker = loop {
            position += stream
                .get(position..)?
                .iter()
                .position(|&byte| byte == 0xFF)?
                + 1;
            position += stream[position..]
                .iter()
                .take_while(|&&byte| byte == 0xFF)
                .count()
                + 1;
            // 0xFF 0x00 is a stuffed byte, not a marker.
            match *stream.get(position - 1)? {
                0x00 => continue,
                marker => break marker,
            }
        };
        let length = usize::from(u16::from_be_bytes([
            *stream.get(position)?,
            *stream.get(position + 1)?,
        ]));
        let contents = stream.get(position + 2..position + length)?;
        position = if marker == 0xDA {
            stream.len()
        } else {
            position + length
        };
        Some((marker, contents))
    })
}

/// CharLS writes separate planes for interleave mode 0 and packed pixels for
/// line and sample interleaving. The JPEG-LS scan header specifies the mode.
fn jpeg_ls_samples_by_plane(stream: &[u8]) -> Result<bool> {
    let (_, scan) = jpeg_header_segments(stream)
        .find(|&(marker, _)| marker == 0xDA)
        .context("missing JPEG-LS scan header")?;
    let (&components, fields) = scan.split_first().context("empty JPEG-LS scan header")?;
    match (components, fields.get(2 * usize::from(components)..)) {
        (1, Some([_, 0, _])) => Ok(true),
        (3, Some([_, 1 | 2, _])) => Ok(false),
        _ => bail!("invalid JPEG-LS color interleave mode"),
    }
}

/// The size of a frame as its codestream declares it. The library decoders
/// for JPEG, JPEG-LS and JPEG 2000 size their output from it, not from Rows
/// and Columns.
#[derive(Clone, Copy)]
struct CodestreamSize {
    rows: u32,
    columns: u32,
    components: u16,
}

impl CodestreamSize {
    fn agrees_with(self, layout: FrameLayout) -> bool {
        self.rows > 0
            && self.columns > 0
            && self.rows == u32::from(layout.rows)
            && self.columns == u32::from(layout.columns)
            && self.components == layout.samples_per_pixel
    }
}

/// Reads the frame size from a codestream, `None` when it has none.
type CodestreamSizeReader = fn(&[u8]) -> Option<CodestreamSize>;

/// How the library decoder for `transfer_syntax` finds the frame size in a
/// codestream, for the transfer syntaxes whose decoder sizes its output from
/// it. JPEG Extended is left out: jpeg_extended.rs makes the same check.
fn codestream_size_reader(transfer_syntax: &str) -> Option<CodestreamSizeReader> {
    match transfer_syntax {
        uids::JPEG_BASELINE8_BIT | uids::JPEG_LOSSLESS | uids::JPEG_LOSSLESS_SV1 => {
            Some(jpeg_frame_size)
        }
        uids::JPEGLS_LOSSLESS | uids::JPEGLS_NEAR_LOSSLESS => Some(jpeg_ls_frame_size),
        uids::JPEG2000_LOSSLESS
        | uids::JPEG2000
        | uids::JPEG2000MC_LOSSLESS
        | uids::JPEG2000MC
        | uids::HTJ2K_LOSSLESS
        | uids::HTJ2K_LOSSLESS_RPCL
        | uids::HTJ2K => Some(jpeg2000_frame_size),
        _ => None,
    }
}

/// The size in a JPEG stream's first SOF0-SOF15 frame header.
fn jpeg_frame_size(stream: &[u8]) -> Option<CodestreamSize> {
    // 0xC4, 0xC8 and 0xCC are DHT, JPG and DAC, not frame headers.
    start_of_frame_size(stream, |marker| {
        matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
    })
}

/// The size in a JPEG-LS stream's SOF55 frame header.
fn jpeg_ls_frame_size(stream: &[u8]) -> Option<CodestreamSize> {
    start_of_frame_size(stream, |marker| marker == 0xF7)
}

/// The size in the first frame header of a JPEG or JPEG-LS stream: sample
/// precision, then lines, samples per line and component count.
fn start_of_frame_size(stream: &[u8], is_frame: fn(u8) -> bool) -> Option<CodestreamSize> {
    let (_, frame) = jpeg_header_segments(stream).find(|&(marker, _)| is_frame(marker))?;
    let header: [u8; 6] = frame.get(..6)?.try_into().ok()?;
    Some(CodestreamSize {
        rows: u16::from_be_bytes([header[1], header[2]]).into(),
        columns: u16::from_be_bytes([header[3], header[4]]).into(),
        components: header[5].into(),
    })
}

/// The image size in a JPEG 2000 codestream's SIZ marker, which follows its
/// SOC marker, or in the codestream box of a JP2 file.
fn jpeg2000_frame_size(stream: &[u8]) -> Option<CodestreamSize> {
    const JP2_SIGNATURE: [u8; 12] = [
        0x00, 0x00, 0x00, 0x0C, b'j', b'P', b' ', b' ', 0x0D, 0x0A, 0x87, 0x0A,
    ];

    let codestream = if stream.starts_with(&JP2_SIGNATURE) {
        jp2_codestream(stream)?
    } else {
        stream
    };
    // Lsiz and Rsiz, then the image and tile extents and offsets, then Csiz.
    let size = codestream.strip_prefix(&[0xFF, 0x4F, 0xFF, 0x51])?;
    let u32_at = |offset: usize| {
        Some(u32::from_be_bytes(
            size.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    Some(CodestreamSize {
        rows: u32_at(8)?.checked_sub(u32_at(16)?)?,
        columns: u32_at(4)?.checked_sub(u32_at(12)?)?,
        components: u16::from_be_bytes(size.get(36..38)?.try_into().ok()?),
    })
}

/// The contents of a JP2 file's first contiguous codestream ('jp2c') box.
fn jp2_codestream(file: &[u8]) -> Option<&[u8]> {
    let mut rest = file;
    loop {
        let length = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
        let (header, length) = match length {
            // The box runs to the end of the file.
            0 => (8, rest.len()),
            // An eight-byte length follows the box type.
            1 => (
                16,
                usize::try_from(u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?)).ok()?,
            ),
            length => (8, length as usize),
        };
        if rest.get(4..8)? == b"jp2c" {
            return rest.get(header..);
        }
        if length < header {
            return None;
        }
        rest = rest.get(length..)?;
    }
}

/// Rebuild one decoded frame as native pixel data, retaining shared attributes
/// and only that frame's item from the per-frame functional groups.
pub(super) fn single_frame_decoding_copy(
    object: &DefaultDicomObject,
    frame: u32,
    vr: VR,
    samples: PrimitiveValue,
) -> Result<DefaultDicomObject> {
    let frame_groups = object
        .element_opt(tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE)?
        .map(|sequence| {
            sequence
                .value()
                .items()
                .and_then(|items| items.get(frame as usize))
                .with_context(|| {
                    format!(
                        "missing functional groups for frame {}",
                        u64::from(frame) + 1
                    )
                })
        })
        .transpose()?;

    let mut meta = object.meta().clone();
    meta.transfer_syntax = uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned();
    let mut native = DefaultDicomObject::new_empty_with_meta(meta);
    for element in object.iter().filter(|element| {
        !matches!(
            element.tag(),
            tags::PIXEL_DATA | tags::NUMBER_OF_FRAMES | tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE
        )
    }) {
        native.put_element(element.clone());
    }
    native.put_element(DataElement::new(tags::PIXEL_DATA, vr, samples));
    native.put_element(DataElement::new(tags::NUMBER_OF_FRAMES, VR::IS, "1"));
    if let Some(frame_groups) = frame_groups {
        native.put_element(DataElement::new(
            tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
            VR::SQ,
            DataSetSequence::from(vec![frame_groups.clone()]),
        ));
    }
    Ok(native)
}

/// Build a single frame whose decoded samples are already interleaved RGB.
pub(super) fn rgb_decoding_copy(
    object: &DefaultDicomObject,
    frame: u32,
    vr: VR,
    samples: PrimitiveValue,
) -> Result<DefaultDicomObject> {
    let mut native = single_frame_decoding_copy(object, frame, vr, samples)?;
    native.put_element(DataElement::new(
        tags::PHOTOMETRIC_INTERPRETATION,
        VR::CS,
        "RGB",
    ));
    native.put_element(DataElement::new(
        tags::PLANAR_CONFIGURATION,
        VR::US,
        PrimitiveValue::from(0_u16),
    ));
    Ok(native)
}

/// Size of a frame stored as Y1 Y2 Cb Cr pixel pairs.
fn ybr_full_422_frame_bytes(layout: FrameLayout) -> Result<u64> {
    ensure!(
        layout.samples_per_pixel == 3
            && layout.bits_allocated == 8
            && layout.rows > 0
            && layout.columns > 0
            && layout.columns.is_multiple_of(2),
        "native YBR_FULL_422 needs three 8-bit samples, nonzero rows, and a positive even column count"
    );
    Ok(layout.pixels() * 2)
}

/// Read the sample layout of a native color frame.
fn planar_configuration(object: &DefaultDicomObject) -> Result<u16> {
    let value = object
        .element(tags::PLANAR_CONFIGURATION)?
        .to_int::<u16>()?;
    ensure!(value <= 1, "invalid Planar Configuration {value}");
    Ok(value)
}

/// Validate unsigned YBR precision and return a mask for the stored bits.
fn ybr_sample_mask(object: &DefaultDicomObject, layout: FrameLayout) -> Result<u16> {
    ensure!(
        object
            .element(tags::PIXEL_REPRESENTATION)?
            .to_int::<u16>()?
            == 0,
        "YBR color needs unsigned samples"
    );
    let bits_stored = object.element(tags::BITS_STORED)?.to_int::<u16>()?;
    ensure!(
        (1..=layout.bits_allocated).contains(&bits_stored),
        "Bits Stored {bits_stored} does not fit Bits Allocated {}",
        layout.bits_allocated
    );
    Ok(u16::MAX >> (16 - bits_stored))
}

/// Read the requested native frame at its packed byte offset.
fn native_ybr_frame(
    object: &DefaultDicomObject,
    frame: u32,
    subsampled: bool,
) -> Result<DefaultDicomObject> {
    let layout = FrameLayout::read_within_budget(object)?;
    let frame_bytes = if subsampled {
        ybr_full_422_frame_bytes(layout)?
    } else {
        validate_ybr_full_layout(layout)?;
        layout.frame_bytes()
    } as usize;
    let frames = object
        .element_opt(tags::NUMBER_OF_FRAMES)?
        .map(|element| element.to_int::<u32>())
        .transpose()?
        .unwrap_or(1);
    let number = u64::from(frame) + 1;
    ensure!(frame < frames, "the pixel data holds no frame {number}");
    let start = (frame as usize)
        .checked_mul(frame_bytes)
        .context("native frame offset overflows")?;
    let data = object.element(tags::PIXEL_DATA)?.to_bytes()?;
    let stored = data
        .get(start..)
        .and_then(|rest| rest.get(..frame_bytes))
        .with_context(|| format!("the pixel data holds no complete frame {number}"))?;
    if subsampled {
        ybr_full_422_frame(object, frame, layout, stored)
    } else {
        ybr_full_frame(
            object,
            frame,
            layout,
            stored,
            planar_configuration(object)? == 1,
        )
    }
}

/// Convert each packed pair directly to two RGB pixels.
fn ybr_full_422_frame(
    object: &DefaultDicomObject,
    frame: u32,
    layout: FrameLayout,
    stored: &[u8],
) -> Result<DefaultDicomObject> {
    ensure!(
        planar_configuration(object)? == 0,
        "native YBR_FULL_422 needs Planar Configuration 0"
    );
    let maximum = ybr_sample_mask(object, layout)?;
    let (pairs, _) = stored.as_chunks::<4>();
    let mut rgb = Vec::with_capacity(layout.pixels() as usize * 3);
    for &[first, second, cb, cr] in pairs {
        for luminance in [first, second] {
            let ybr = [luminance, cb, cr].map(|sample| u16::from(sample) & maximum);
            rgb.extend(ybr_full_to_rgb(ybr, maximum).map(|value| value as u8));
        }
    }
    rgb_decoding_copy(object, frame, VR::OB, PrimitiveValue::from(rgb))
}

/// Check that the layout fits the 8-bit or 16-bit YBR converter.
fn validate_ybr_full_layout(layout: FrameLayout) -> Result<()> {
    ensure!(
        layout.samples_per_pixel == 3
            && matches!(layout.bits_allocated, 8 | 16)
            && layout.pixels() > 0,
        "YBR_FULL needs three samples of 8 or 16 bits and nonzero dimensions"
    );
    Ok(())
}

/// Convert either planar or interleaved YBR_FULL to interleaved RGB.
fn ybr_full_frame(
    object: &DefaultDicomObject,
    frame: u32,
    layout: FrameLayout,
    samples: &[u8],
    by_plane: bool,
) -> Result<DefaultDicomObject> {
    validate_ybr_full_layout(layout)?;
    ensure!(
        samples.len() as u64 == layout.frame_bytes(),
        "YBR_FULL frame {} has {} bytes, expected {}",
        u64::from(frame) + 1,
        samples.len(),
        layout.frame_bytes()
    );
    let maximum = ybr_sample_mask(object, layout)?;
    let pixels = layout.pixels() as usize;
    let width = layout.bytes_per_sample() as usize;
    let sample = |pixel: usize, channel: usize| {
        let index = if by_plane {
            channel * pixels + pixel
        } else {
            pixel * 3 + channel
        } * width;
        let mut bytes = [0; 2];
        bytes[..width].copy_from_slice(&samples[index..index + width]);
        u16::from_le_bytes(bytes) & maximum
    };
    let mut rgb = Vec::with_capacity(samples.len());
    for pixel in 0..pixels {
        let ybr = [0, 1, 2].map(|channel| sample(pixel, channel));
        for value in ybr_full_to_rgb(ybr, maximum) {
            rgb.extend_from_slice(&value.to_le_bytes()[..width]);
        }
    }
    rgb_decoding_copy(object, frame, VR::OB, PrimitiveValue::from(rgb))
}

/// Convert full-range YBR samples using the equations in DICOM PS3.3 C.7.6.3.1.2.
fn ybr_full_to_rgb([luminance, cb, cr]: [u16; 3], maximum: u16) -> [u16; 3] {
    let midpoint = f64::from(maximum / 2 + 1);
    let luminance = f64::from(luminance);
    let cb = f64::from(cb) - midpoint;
    let cr = f64::from(cr) - midpoint;
    [
        luminance + 1.402 * cr,
        luminance - 0.344136 * cb - 0.714136 * cr,
        luminance + 1.772 * cb,
    ]
    .map(|value| value.round().clamp(0.0, f64::from(maximum)) as u16)
}

/// Rebuild the frame as pixel-interleaved little-endian samples. Each RLE
/// segment supplies one byte of one sample across all pixels, starting with
/// the most significant byte of each sample.
fn native_rle_frame(
    dicom_object: &DefaultDicomObject,
    frame: u32,
    layout: FrameLayout,
    fragment: &[u8],
) -> Result<DefaultDicomObject> {
    ensure!(
        matches!(layout.samples_per_pixel, 1 | 3) && matches!(layout.bits_allocated, 8 | 16 | 32),
        "RLE images of {} samples per pixel with {} bits allocated are not supported",
        layout.samples_per_pixel,
        layout.bits_allocated
    );
    let starts = rle_segment_starts(fragment, layout)?;
    let ends = starts[1..].iter().copied().chain([fragment.len()]);
    let bytes_per_sample = layout.bytes_per_sample() as usize;
    let bytes_per_pixel = usize::from(layout.samples_per_pixel) * bytes_per_sample;
    let mut samples = vec![0; layout.pixels() as usize * bytes_per_pixel];
    for (segment, (&start, end)) in starts.iter().zip(ends).take(bytes_per_pixel).enumerate() {
        let sample = segment / bytes_per_sample;
        let byte = bytes_per_sample - 1 - segment % bytes_per_sample;
        let offset = sample * bytes_per_sample + byte;
        let destination = samples
            .chunks_exact_mut(bytes_per_pixel)
            .map(|pixel| &mut pixel[offset]);
        unpack_rle_segment(&fragment[start..end], destination)
            .context("the RLE pixel data is damaged")?;
    }

    if photometric_interpretation(dicom_object) == Some(PhotometricInterpretation::YbrFull) {
        return ybr_full_frame(dicom_object, frame, layout, &samples, false);
    }
    let mut native =
        single_frame_decoding_copy(dicom_object, frame, VR::OB, PrimitiveValue::from(samples))?;
    if layout.samples_per_pixel == 3 {
        native.put_element(DataElement::new(
            tags::PLANAR_CONFIGURATION,
            VR::US,
            PrimitiveValue::from(0_u16),
        ));
    }
    Ok(native)
}

/// Decode PackBits directly into one byte position of every output pixel.
/// Return `None` when the segment ends before the destination is filled.
fn unpack_rle_segment<'a>(
    mut packed: &[u8],
    mut destination: impl ExactSizeIterator<Item = &'a mut u8>,
) -> Option<()> {
    while destination.len() != 0 {
        let (&control, remainder) = packed.split_first()?;
        packed = remainder;
        match control as i8 {
            -128 => {}
            literal @ 0.. => {
                let (run, remainder) = packed.split_at_checked(usize::from(literal as u8) + 1)?;
                packed = remainder;
                for (&byte, output) in run.iter().zip(destination.by_ref()) {
                    *output = byte;
                }
            }
            repeat => {
                let (&byte, remainder) = packed.split_first()?;
                packed = remainder;
                for output in destination.by_ref().take((1 - i16::from(repeat)) as usize) {
                    *output = byte;
                }
            }
        }
    }
    Some(())
}

fn decode_frame(dicom_object: &DefaultDicomObject, frame_index: u32) -> Result<DecodedFrame> {
    let decoded = decode_pixel_frame(dicom_object, frame_index)?.to_owned();

    let frame_count = first_parsed::<u32>(dicom_object, "NumberOfFrames")
        .unwrap_or(1)
        .max(1);

    let window_level_available = decoded.photometric_interpretation().is_monochrome();
    let pixel_probe = build_pixel_probe(&decoded);
    let overlay_bitmaps =
        read_overlay_bitmaps(dicom_object, frame_index, decoded.columns(), decoded.rows());
    let implicit_window = implicit_window(&decoded, &overlay_bitmaps, || match &pixel_probe {
        Some(PixelProbeData::Monochrome { values, .. }) => Ok(values.as_slice()),
        _ => bail!("could not read rescaled grayscale values"),
    })?;
    let default_window = decoded
        .window()?
        .and_then(<[_]>::first)
        .filter(|window| usable_window(window))
        .map(|window| DicomWindow {
            center: window.center,
            width: window.width,
        });
    let value_range = compute_value_range(&decoded)?;

    Ok(DecodedFrame {
        decoded,
        overlay_bitmaps,
        pixel_probe,
        frame_count,
        default_window,
        implicit_window,
        value_range,
        window_level_available,
    })
}

pub(crate) fn decode_pixel_frame(
    object: &DefaultDicomObject,
    frame: u32,
) -> Result<DecodedPixelData<'_>> {
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

/// Checks that every frame offset points to a fragment item header. Offsets
/// include the eight-byte item header of each preceding fragment.
fn validate_frame_offsets(fragments: &[Vec<u8>], offsets: &[u32], frames: u32) -> Result<()> {
    let invalid =
        "the compressed pixel data is damaged: its offset table does not match its fragments";
    ensure!(
        offsets.len() == frames as usize
            && offsets.first() == Some(&0)
            && offsets.windows(2).all(|pair| pair[0] < pair[1]),
        "{invalid}"
    );
    let mut fragment_starts = fragments.iter().scan(0_u64, |offset, fragment| {
        let start = *offset;
        *offset += fragment.len() as u64 + 8;
        Some(start)
    });
    for offset in offsets.iter().copied().map(u64::from) {
        ensure!(
            fragment_starts.find(|&start| start >= offset) == Some(offset),
            "{invalid}"
        );
    }
    Ok(())
}

/// Validate frame bounds before decoding and unpack native subsampled color.
fn decode_pixel_frame_inner(
    object: &DefaultDicomObject,
    frame: u32,
) -> Result<DecodedPixelData<'_>> {
    let transfer_syntax = object.meta().transfer_syntax();
    let pixel_data = object.element(tags::PIXEL_DATA)?.value();
    let fragments = pixel_data.fragments();
    let photometric = photometric_interpretation(object);
    if pixel_data.primitive().is_some()
        && matches!(
            photometric,
            Some(PhotometricInterpretation::YbrFull | PhotometricInterpretation::YbrFull422)
        )
        && TransferSyntaxRegistry
            .get(transfer_syntax)
            .is_some_and(|syntax| !syntax.is_encapsulated_pixel_data())
    {
        let native = native_ybr_frame(
            object,
            frame,
            photometric == Some(PhotometricInterpretation::YbrFull422),
        )?;
        return Ok(native.decode_pixel_data_frame(0)?.to_owned());
    }
    // Compressed pixel data is checked before any decoder sizes its output
    // from the header; native pixel data is already in memory at full size.
    let layout = fragments
        .map(|_| FrameLayout::read_within_budget(object))
        .transpose()?;
    let single_fragment_codec = matches!(
        transfer_syntax,
        uids::RLE_LOSSLESS | uids::DEFLATED_IMAGE_FRAME_COMPRESSION
    );
    if let Some(fragments) = fragments {
        let frames = object
            .element_opt(tags::NUMBER_OF_FRAMES)?
            .map(|element| element.to_int::<u32>())
            .transpose()?
            .unwrap_or(1);
        ensure!(
            frame < frames,
            "the compressed pixel data holds no frame {}",
            frame + 1
        );
        let offsets = pixel_data
            .offset_table()
            .context("missing pixel data offset table")?;
        if !offsets.is_empty() && fragments.len() != frames as usize {
            validate_frame_offsets(fragments, offsets, frames)?;
        }
        if single_fragment_codec {
            ensure!(
                fragments.len() <= frames as usize,
                "the compressed pixel data has more than one fragment per frame"
            );
        } else if offsets.is_empty() {
            ensure!(
                frames == 1 || fragments.len() == frames as usize,
                "fragmented multi-frame pixel data requires a frame offset table"
            );
        }
    }
    if matches!(
        transfer_syntax,
        uids::JPEG_LOSSLESS | uids::JPEG_LOSSLESS_SV1
    ) && uses_jpeg_restart_intervals(object, frame)
    {
        bail!(
            "this JPEG Lossless image uses restart intervals, which the JPEG decoder reads wrongly"
        );
    }
    let mut ybr_by_plane = false;
    if let Some(layout) = layout
        && let Some(read_size) = codestream_size_reader(transfer_syntax)
    {
        let stream = object
            .frame_pixel_data(frame)
            .context("the compressed pixel data is damaged: its frame header is missing")?;
        let size = read_size(&stream)
            .context("the compressed pixel data is damaged: its frame header is missing")?;
        ensure!(
            size.agrees_with(layout),
            "the compressed pixel data is damaged: its frame header describes {} x {} pixels \
             of {} samples, where the attributes describe {} x {} pixels of {}",
            size.columns,
            size.rows,
            size.components,
            layout.columns,
            layout.rows,
            layout.samples_per_pixel
        );
        if photometric == Some(PhotometricInterpretation::YbrFull)
            && matches!(
                transfer_syntax,
                uids::JPEGLS_LOSSLESS | uids::JPEGLS_NEAR_LOSSLESS
            )
        {
            ybr_by_plane = jpeg_ls_samples_by_plane(&stream)?;
        }
    }
    // RLE and Deflated Image Frame Compression store one fragment per frame.
    if single_fragment_codec {
        let layout = layout.context("compressed pixel data is not encapsulated")?;
        let fragment = fragments
            .and_then(|fragments| fragments.get(frame as usize))
            .with_context(|| {
                format!(
                    "the compressed pixel data has no fragment for frame {}",
                    frame + 1
                )
            })?;
        let native = if transfer_syntax == uids::RLE_LOSSLESS {
            native_rle_frame(object, frame, layout, fragment)?
        } else {
            match photometric {
                Some(PhotometricInterpretation::YbrFull422) => {
                    let expected = ybr_full_422_frame_bytes(layout)?;
                    let samples = inflate_frame(fragment, layout, expected, frame)?;
                    ybr_full_422_frame(object, frame, layout, &samples)?
                }
                Some(PhotometricInterpretation::YbrFull) => {
                    validate_ybr_full_layout(layout)?;
                    let samples = inflate_frame(fragment, layout, layout.frame_bytes(), frame)?;
                    ybr_full_frame(
                        object,
                        frame,
                        layout,
                        &samples,
                        planar_configuration(object)? == 1,
                    )?
                }
                _ => {
                    let samples = inflate_frame(fragment, layout, layout.frame_bytes(), frame)?;
                    single_frame_decoding_copy(
                        object,
                        frame,
                        VR::OB,
                        PrimitiveValue::from(samples),
                    )?
                }
            }
        };
        return Ok(native.decode_pixel_data_frame(0)?.to_owned());
    }
    let result = if object.meta().transfer_syntax.trim_end_matches(['\0', ' '])
        == jpeg_extended::TRANSFER_SYNTAX
    {
        jpeg_extended::decode_frame(object, frame)
    } else {
        object.decode_pixel_data_frame(frame).map_err(Into::into)
    };
    let decoded =
        result.with_context(|| format!("could not decode DICOM pixel data frame {}", frame + 1))?;
    if let Some(layout) = layout
        && photometric == Some(PhotometricInterpretation::YbrFull)
        && decodes_colour_as_stored(transfer_syntax)
    {
        let native = ybr_full_frame(object, frame, layout, decoded.data(), ybr_by_plane)?;
        return Ok(native.decode_pixel_data_frame(0)?.to_owned());
    }
    Ok(decoded)
}

/// These codecs return the stored color channels. JPEG Baseline and Extended
/// already convert YCbCr to RGB, so they take the regular decoding path.
fn decodes_colour_as_stored(transfer_syntax: &str) -> bool {
    matches!(
        transfer_syntax,
        uids::JPEG_LOSSLESS
            | uids::JPEG_LOSSLESS_SV1
            | uids::JPEGLS_LOSSLESS
            | uids::JPEGLS_NEAR_LOSSLESS
            | uids::JPEG2000_LOSSLESS
            | uids::JPEG2000
            | uids::JPEG2000MC_LOSSLESS
            | uids::JPEG2000MC
            | uids::HTJ2K_LOSSLESS
            | uids::HTJ2K_LOSSLESS_RPCL
            | uids::HTJ2K
    )
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
/// sequence) instead of fabricating a fixed window, and draws a frame that
/// states neither over its full dynamic range.
pub(crate) fn render_frame(
    frame: &DecodedFrame,
    window: Option<DicomWindow>,
) -> Result<DisplayPixels> {
    let rgba = contain_decoder_panic(|| {
        render_decoded_frame(
            &frame.decoded,
            window,
            frame.implicit_window,
            &frame.overlay_bitmaps,
        )
    })?;

    Ok(DisplayPixels {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

/// Render a frame with `window`; when that is `None`, with `implicit_window`,
/// and when both are `None`, with the file's own window or VOI LUT.
fn render_decoded_frame(
    decoded: &DecodedPixelData<'_>,
    window: Option<DicomWindow>,
    implicit_window: Option<DicomWindow>,
    overlay_bitmaps: &[OverlayBitmap],
) -> Result<image::RgbaImage> {
    let voi_lut = match window.or(implicit_window) {
        Some(window) if window.center.is_finite() && window.width.is_finite() => {
            VoiLutOption::Custom(WindowLevel {
                center: window.center,
                width: window.width.max(1.0),
            })
        }
        _ => VoiLutOption::Default,
    };

    let black_overlay_background = matches!(&voi_lut, VoiLutOption::Default)
        && has_black_overlay_background(decoded, overlay_bitmaps)?;

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

/// Overlay-only reports have no intensity range to normalize. Their zero-filled
/// MONOCHROME2 background, without a window or VOI LUT of its own, is kept
/// black so white overlay content remains visible.
fn has_black_overlay_background(
    decoded: &DecodedPixelData<'_>,
    overlay_bitmaps: &[OverlayBitmap],
) -> Result<bool> {
    Ok(!overlay_bitmaps.is_empty()
        && matches!(
            decoded.photometric_interpretation(),
            PhotometricInterpretation::Monochrome2
        )
        && decoded.window()?.is_none()
        && decoded.voi_lut_sequence()?.is_none()
        && !decoded.data().is_empty()
        && decoded.data().iter().all(|&byte| byte == 0))
}

/// The window a monochrome frame is drawn with when no window is requested and
/// the file states neither a usable window nor a VOI LUT: its full dynamic
/// window. dicom-pixeldata's own fallback normalizes over the raw samples read
/// as unsigned and unmasked, which draws signed data, or data with bits set
/// above BitsStored, almost black. `None` leaves the frame to its own window,
/// VOI LUT or black overlay background, and a flat frame, which has no range to
/// stretch, to the decoder's own rendering. A flat frame whose stated window is
/// not a number still gets the window spanning its value, because the decoder
/// would otherwise fall back to that window and fail; a zero or negative width
/// the decoder still draws.
///
/// `frame_values` yields the frame's rescaled values; it is only called when
/// the window is needed, so callers that already hold them pass them in.
fn implicit_window<V: AsRef<[f64]>>(
    decoded: &DecodedPixelData<'_>,
    overlay_bitmaps: &[OverlayBitmap],
    frame_values: impl FnOnce() -> Result<V>,
) -> Result<Option<DicomWindow>> {
    if !decoded.photometric_interpretation().is_monochrome() {
        return Ok(None);
    }

    let stated_window = decoded.window()?.and_then(<[_]>::first);
    let stated_window_is_not_a_number =
        stated_window.is_some_and(|window| !window.center.is_finite() || !window.width.is_finite());
    if stated_window.is_some_and(usable_window)
        || decoded.voi_lut_sequence()?.is_some()
        || has_black_overlay_background(decoded, overlay_bitmaps)?
    {
        return Ok(None);
    }

    let values = frame_values()?;
    Ok(finite_value_range(values.as_ref())
        .filter(|(minimum, maximum)| minimum < maximum || stated_window_is_not_a_number)
        .map(spanning_window))
}

/// The window from the lowest to the highest rescaled value of a frame.
fn spanning_window((minimum, maximum): (f64, f64)) -> DicomWindow {
    DicomWindow {
        center: minimum / 2.0 + maximum / 2.0,
        width: (maximum - minimum).max(1.0),
    }
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

fn usable_window(window: &WindowLevel) -> bool {
    window.center.is_finite() && window.width.is_finite() && window.width > 0.0
}

/// Bound interactive windowing using the stored sample range and the same
/// rescale parameters the decoder uses for this frame.
fn compute_value_range(decoded: &DecodedPixelData<'_>) -> Result<(f64, f64)> {
    let bits_stored = i32::from(decoded.bits_stored().clamp(1, 32));
    let rescale = decoded
        .rescale()?
        .first()
        .context("missing frame rescale parameters")?;
    let slope = finite_or(rescale.slope, 1.0);
    let intercept = finite_or(rescale.intercept, 0.0);

    let (stored_min, stored_max) = if decoded.pixel_representation() == PixelRepresentation::Signed
    {
        let half = 2_f64.powi(bits_stored - 1);
        (-half, half - 1.0)
    } else {
        (0.0, 2_f64.powi(bits_stored) - 1.0)
    };

    let first = slope * stored_min + intercept;
    let second = slope * stored_max + intercept;

    Ok((first.min(second), first.max(second)))
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

fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
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
/// A decoded 16-bit MONOCHROME2 frame of `rows` x `columns` zero samples.
pub(crate) fn monochrome_test_frame(rows: u16, columns: u16) -> DecodedFrame {
    use dicom_object::{FileDicomObject, FileMetaTableBuilder};

    let meta = FileMetaTableBuilder::new()
        .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
        .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
        .media_storage_sop_instance_uid("2.25.1201")
        .build()
        .unwrap();
    let mut object = FileDicomObject::new_empty_with_meta(meta);
    for (tag, value) in [
        (tags::ROWS, rows),
        (tags::COLUMNS, columns),
        (tags::SAMPLES_PER_PIXEL, 1),
        (tags::BITS_ALLOCATED, 16),
        (tags::BITS_STORED, 16),
        (tags::HIGH_BIT, 15),
        (tags::PIXEL_REPRESENTATION, 0),
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
        VR::OW,
        PrimitiveValue::U16(vec![0; usize::from(rows) * usize::from(columns)].into()),
    ));

    decode_frame(&object, 0).unwrap()
}

#[cfg(test)]
pub(crate) fn write_rle_test_slice(
    path: &Path,
    position: &str,
    bits_allocated: u16,
    kept_fraction: f64,
) {
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
    // Cut short as by an interrupted copy, inside a file whose lengths agree.
    rle.truncate((rle.len() as f64 * kept_fraction) as usize & !1);

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
}

#[cfg(test)]
mod tests {
    use dicom_core::header::HasLength;
    use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
    use dicom_dictionary_std::{tags, uids};
    use dicom_object::{DefaultDicomObject, FileDicomObject, FileMetaTableBuilder};
    use dicom_transfer_syntax_registry::{TransferSyntaxIndex, TransferSyntaxRegistry};

    use super::{
        PixelProbeValue, decode_frame, finite_value_range, jpeg_frame_size, jpeg_restart_interval,
        load_dicom_frame, load_dicom_thumbnail, monochrome_test_frame, render_frame,
        thumbnail_dimensions, write_rle_test_slice,
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

    /// The frame header is found where jpeg-decoder finds it: past stray
    /// bytes, fill bytes and stuffed bytes between segments, but only after
    /// SOI and before the first scan.
    #[test]
    fn jpeg_frame_size_is_read_where_the_decoder_reads_it() {
        let size = |stream: &[u8]| {
            jpeg_frame_size(stream).map(|size| (size.columns, size.rows, size.components))
        };
        let comment = [0xFF, 0xFE, 0x00, 0x04, 0xC0, 0xC0];
        // 8-bit, 300 rows of 400 samples, three components.
        let frame = [0xFF, 0xC0, 0x00, 0x08, 0x08, 0x01, 0x2C, 0x01, 0x90, 0x03];
        let scan = [0xFF, 0xDA, 0x00, 0x02];
        let stream = |parts: &[&[u8]]| [&[0xFF, 0xD8][..], &parts.concat()].concat();

        assert_eq!(size(&stream(&[&comment, &frame])), Some((400, 300, 3)));
        assert_eq!(
            size(&stream(&[&comment, &[0x12, 0xFF, 0x00, 0xFF], &frame])),
            Some((400, 300, 3))
        );
        assert_eq!(size(&stream(&[&scan, &frame])), None);
        assert_eq!(size(&frame), None);
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

    /// The cache budget sees every pixel three times over: the decoded
    /// sample, its f64 probe value and the overlay bit, plus any VOI LUT.
    #[test]
    fn resident_size_counts_decoded_samples_probe_values_overlays_and_luts() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        let with_overlay = decode_frame(&object, 0).unwrap();
        // Four 8-bit samples, four f64 probe values and one byte of overlay bits.
        assert_eq!(with_overlay.resident_bytes(), 4 + 4 * 8 + 1);

        let lut = dicom_object::InMemDicomObject::from_element_iter([
            DataElement::new(
                tags::LUT_DESCRIPTOR,
                VR::US,
                PrimitiveValue::U16(vec![256_u16, 0, 8].into()),
            ),
            DataElement::new(
                tags::LUT_DATA,
                VR::OW,
                PrimitiveValue::U16(vec![0_u16; 256].into()),
            ),
        ]);
        object.put_element(DataElement::new(
            tags::VOILUT_SEQUENCE,
            VR::SQ,
            dicom_core::value::Value::new_sequence(vec![lut], dicom_core::Length::UNDEFINED),
        ));
        let with_lut = decode_frame(&object, 0).unwrap();
        assert_eq!(with_lut.resident_bytes(), 4 + 4 * 8 + 1 + 256 * 2);

        // 2 bytes per 16-bit sample plus 8 per probe value.
        assert_eq!(
            monochrome_test_frame(100, 100).resident_bytes(),
            100 * 100 * 10
        );
    }

    /// A 2 x 2, 16-bit monochrome image without a window, VOI LUT or overlay.
    fn sixteen_bit_image(
        photometric_interpretation: &str,
        bits_stored: u16,
        pixel_representation: u16,
        samples: [u16; 4],
    ) -> DefaultDicomObject {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        object.remove_element(Tag(0x6000, 0x3000));
        for (tag, value) in [
            (tags::BITS_ALLOCATED, 16_u16),
            (tags::BITS_STORED, bits_stored),
            (tags::HIGH_BIT, bits_stored - 1),
            (tags::PIXEL_REPRESENTATION, pixel_representation),
        ] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from(photometric_interpretation),
        ));
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            PrimitiveValue::U16(samples.to_vec().into()),
        ));
        object
    }

    /// A colour frame keeps its samples and one byte per channel for the
    /// probe; a frame of a multi-frame image keeps only that frame.
    #[test]
    fn resident_size_counts_colour_probes_by_channel_and_one_frame_of_many() {
        let colour = |bits: u16, samples: Vec<u16>| {
            let mut object = sixteen_bit_image("RGB", bits, 0, [0; 4]);
            for (tag, value) in [
                (tags::BITS_ALLOCATED, bits),
                (tags::SAMPLES_PER_PIXEL, 3),
                (tags::PLANAR_CONFIGURATION, 0),
            ] {
                object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
            }
            object.put_element(if bits == 8 {
                DataElement::new(
                    tags::PIXEL_DATA,
                    VR::OB,
                    PrimitiveValue::from(samples.iter().map(|&s| s as u8).collect::<Vec<u8>>()),
                )
            } else {
                DataElement::new(
                    tags::PIXEL_DATA,
                    VR::OW,
                    PrimitiveValue::U16(samples.into()),
                )
            });
            decode_frame(&object, 0).unwrap()
        };
        // 2 x 2 x 3 samples of one byte, and the same again for the probe.
        assert_eq!(colour(8, vec![7; 12]).resident_bytes(), 12 + 12);
        // Two bytes per 16-bit sample; the probe keeps 8-bit colours.
        assert_eq!(colour(16, vec![700; 12]).resident_bytes(), 24 + 12);

        let mut frames = sixteen_bit_image("MONOCHROME2", 16, 0, [0; 4]);
        frames.put_element(DataElement::new(tags::NUMBER_OF_FRAMES, VR::IS, "3"));
        frames.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            PrimitiveValue::U16(vec![1_u16; 12].into()),
        ));
        for frame_index in 0..3 {
            // 4 samples of 2 bytes and 4 probe values of 8 bytes.
            assert_eq!(
                decode_frame(&frames, frame_index).unwrap().resident_bytes(),
                4 * 2 + 4 * 8
            );
        }
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

    fn signed_image(photometric_interpretation: &str) -> DefaultDicomObject {
        sixteen_bit_image(
            photometric_interpretation,
            16,
            1,
            [-1000_i16, 0, 500, 1000].map(|value| value as u16),
        )
    }

    fn greys(rgba: &[u8]) -> Vec<u8> {
        rgba.chunks_exact(4).map(|pixel| pixel[0]).collect()
    }

    fn thumbnail_greys(object: &DefaultDicomObject, name: &str) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!("dicron-{name}-{}.dcm", std::process::id()));
        object.write_to_file(&path).unwrap();
        let thumbnail = load_dicom_thumbnail(&path, 0, 2);
        std::fs::remove_file(path).unwrap();
        greys(&thumbnail.unwrap().rgba)
    }

    /// Without a window of its own, a signed image is drawn over its full
    /// dynamic range, and the W/L read-out names that same window.
    #[test]
    fn signed_image_without_window_spans_its_values() {
        let object = signed_image("MONOCHROME2");
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(frame.default_center_width(), (0.0, 2000.0));
        assert_eq!(
            greys(&render_frame(&frame, None).unwrap().rgba),
            [0, 128, 192, 255]
        );
        assert_eq!(
            thumbnail_greys(&object, "signed-no-window"),
            [0, 128, 192, 255]
        );
    }

    #[test]
    fn monochrome1_without_window_is_still_inverted() {
        let object = signed_image("MONOCHROME1");
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(
            greys(&render_frame(&frame, None).unwrap().rgba),
            [255, 127, 63, 0]
        );
        assert_eq!(
            thumbnail_greys(&object, "monochrome1-no-window"),
            [255, 127, 63, 0]
        );
    }

    /// Bits above BitsStored are not part of the sample value.
    #[test]
    fn bits_above_bits_stored_do_not_change_the_rendering() {
        let clean = sixteen_bit_image("MONOCHROME2", 12, 0, [0, 1000, 2000, 4000]);
        let stray = sixteen_bit_image("MONOCHROME2", 12, 0, [0xF000, 1000, 0x8000 | 2000, 4000]);
        let clean_frame = decode_frame(&clean, 0).unwrap();
        let stray_frame = decode_frame(&stray, 0).unwrap();

        assert_eq!(stray_frame.default_center_width(), (2000.0, 4000.0));
        assert_eq!(
            render_frame(&stray_frame, None).unwrap().rgba,
            render_frame(&clean_frame, None).unwrap().rgba
        );
        assert_eq!(
            thumbnail_greys(&stray, "stray-high-bits"),
            thumbnail_greys(&clean, "clean-high-bits")
        );
    }

    /// The read-out matches the drawn window, not the range BitsStored allows.
    #[test]
    fn default_window_without_file_window_is_the_data_range() {
        let object = sixteen_bit_image("MONOCHROME2", 12, 0, [0, 250, 500, 1000]);
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(frame.default_center_width(), (500.0, 1000.0));
        assert_eq!(
            render_frame(&frame, None).unwrap().rgba,
            render_frame(
                &frame,
                Some(super::DicomWindow {
                    center: 500.0,
                    width: 1000.0,
                }),
            )
            .unwrap()
            .rgba
        );
    }

    #[test]
    fn file_window_is_drawn_and_reported_unchanged() {
        let mut object = signed_image("MONOCHROME2");
        object.put_element(DataElement::new(tags::WINDOW_CENTER, VR::DS, "500"));
        object.put_element(DataElement::new(tags::WINDOW_WIDTH, VR::DS, "1000"));
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(frame.default_center_width(), (500.0, 1000.0));
        assert_eq!(
            greys(&render_frame(&frame, None).unwrap().rgba),
            [0, 0, 128, 255]
        );
        assert_eq!(thumbnail_greys(&object, "file-window"), [0, 0, 128, 255]);
    }

    /// A stated window that cannot be used (zero width, or a center that is
    /// not a number) is treated like no window: the image spans its values and
    /// the read-out names that window.
    #[test]
    fn unusable_file_window_is_treated_as_no_window() {
        for (center, width) in [("500", "0"), ("NaN", "1000"), ("500", "-20")] {
            let mut object = signed_image("MONOCHROME2");
            object.put_element(DataElement::new(tags::WINDOW_CENTER, VR::DS, center));
            object.put_element(DataElement::new(tags::WINDOW_WIDTH, VR::DS, width));
            let frame = decode_frame(&object, 0).unwrap();

            assert_eq!(
                frame.default_center_width(),
                (0.0, 2000.0),
                "{center}/{width}"
            );
            assert_eq!(
                greys(&render_frame(&frame, None).unwrap().rgba),
                [0, 128, 192, 255],
                "{center}/{width}"
            );
            assert_eq!(
                thumbnail_greys(&object, "unusable-window"),
                [0, 128, 192, 255],
                "{center}/{width}"
            );
        }
    }

    /// The implicit window is taken over rescaled values, so a negative slope
    /// flips the image and the read-out is in rescaled units.
    #[test]
    fn negative_rescale_slope_without_window_spans_rescaled_values() {
        let mut object = sixteen_bit_image("MONOCHROME2", 12, 0, [0, 1000, 2000, 4000]);
        object.put_element(DataElement::new(tags::RESCALE_SLOPE, VR::DS, "-1"));
        object.put_element(DataElement::new(tags::RESCALE_INTERCEPT, VR::DS, "0"));
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(frame.default_center_width(), (-2000.0, 4000.0));
        let drawn = render_frame(&frame, None).unwrap().rgba;
        assert_eq!(
            drawn,
            render_frame(
                &frame,
                Some(super::DicomWindow {
                    center: -2000.0,
                    width: 4000.0,
                }),
            )
            .unwrap()
            .rgba
        );
        let drawn = greys(&drawn);
        assert_eq!((drawn[0], drawn[3]), (255, 0));
        assert!(drawn[1] > drawn[2]);
    }

    /// Signed 12-bit samples are read the same whether or not the writer
    /// sign-extended them into the unused high bits.
    #[test]
    fn signed_twelve_bit_without_window_spans_its_values() {
        for minus_thousand in [0x0C18_u16, 0xFC18] {
            let object = sixteen_bit_image("MONOCHROME2", 12, 1, [minus_thousand, 0, 500, 1000]);
            let frame = decode_frame(&object, 0).unwrap();

            assert_eq!(
                frame.default_center_width(),
                (0.0, 2000.0),
                "{minus_thousand:#06x}"
            );
            assert_eq!(
                greys(&render_frame(&frame, None).unwrap().rgba),
                [0, 128, 192, 255],
                "{minus_thousand:#06x}"
            );
        }
    }

    /// Signed 8-bit data without a window spans its values too.
    #[test]
    fn signed_eight_bit_without_window_spans_its_values() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        object.remove_element(Tag(0x6000, 0x3000));
        object.put_element(DataElement::new(
            tags::PIXEL_REPRESENTATION,
            VR::US,
            PrimitiveValue::from(1_u16),
        ));
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from([-100_i8, 0, 50, 100].map(|value| value as u8).to_vec()),
        ));
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(frame.default_center_width(), (0.0, 200.0));
        assert_eq!(
            greys(&render_frame(&frame, None).unwrap().rgba),
            [0, 128, 192, 255]
        );
        assert_eq!(thumbnail_greys(&object, "signed-8-bit"), [0, 128, 192, 255]);
    }

    /// An overlay-only page keeps a read-out whose window still draws its zero
    /// background black, so Reset and the first drag do not turn it white and
    /// hide the white overlay.
    #[test]
    fn zero_overlay_frame_default_window_keeps_black_background() {
        let frame = decode_frame(&black_two_by_two_image_with_overlay_pixel(), 0).unwrap();
        let (center, width) = frame.default_center_width();

        assert_eq!(
            greys(
                &render_frame(&frame, Some(super::DicomWindow { center, width }))
                    .unwrap()
                    .rgba
            ),
            [0, 0, 0, 255]
        );
    }

    /// A flat frame has no range to stretch, so it keeps the decoder's own
    /// rendering and the read-out derived from BitsStored, as before the
    /// implicit window existed: black for signed data, white for unsigned.
    #[test]
    fn flat_signed_frame_without_window_stays_black() {
        let object = sixteen_bit_image("MONOCHROME2", 16, 1, [-1000_i16 as u16; 4]);
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(frame.default_center_width(), (-0.5, 65535.0));
        assert_eq!(greys(&render_frame(&frame, None).unwrap().rgba), [0; 4]);
        assert_eq!(thumbnail_greys(&object, "flat-signed"), [0; 4]);
    }

    #[test]
    fn flat_unsigned_frame_without_window_stays_white() {
        let object = sixteen_bit_image("MONOCHROME2", 12, 0, [500; 4]);
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(frame.default_center_width(), (2047.5, 4095.0));
        assert_eq!(greys(&render_frame(&frame, None).unwrap().rgba), [255; 4]);
        assert_eq!(thumbnail_greys(&object, "flat-unsigned"), [255; 4]);
    }

    /// A flat frame whose stated window is not a number would fall back to
    /// that window in the decoder and fail, so it is drawn through the window
    /// spanning its one value, and the read-out names that window.
    #[test]
    fn flat_frame_with_a_stated_window_that_is_not_a_number_still_draws() {
        for (center, width) in [("NaN", "1000"), ("500", "NaN")] {
            let mut object = sixteen_bit_image("MONOCHROME2", 16, 1, [-1000_i16 as u16; 4]);
            object.put_element(DataElement::new(tags::WINDOW_CENTER, VR::DS, center));
            object.put_element(DataElement::new(tags::WINDOW_WIDTH, VR::DS, width));
            let frame = decode_frame(&object, 0).unwrap();

            assert_eq!(
                frame.default_center_width(),
                (-1000.0, 1.0),
                "{center}/{width}"
            );
            let drawn = render_frame(&frame, None).unwrap().rgba;
            assert_eq!(greys(&drawn), [255; 4], "{center}/{width}");
            assert_eq!(
                thumbnail_greys(&object, "flat-unusable-window"),
                [255; 4],
                "{center}/{width}"
            );
        }
    }

    fn rgb(rgba: &[u8]) -> Vec<[u8; 3]> {
        rgba.chunks_exact(4)
            .map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect()
    }

    /// Colour images have nothing to window and keep their colours.
    #[test]
    fn rgb_image_keeps_its_colours() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        object.remove_element(Tag(0x6000, 0x3000));
        for (tag, value) in [
            (tags::SAMPLES_PER_PIXEL, 3_u16),
            (tags::PLANAR_CONFIGURATION, 0),
        ] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("RGB"),
        ));
        let samples = [[10, 20, 30], [40, 50, 60], [70, 80, 90], [100, 110, 120]];
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from(samples.concat()),
        ));
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(rgb(&render_frame(&frame, None).unwrap().rgba), samples);
        assert_eq!(frame.default_center_width(), (127.5, 255.0));
        let path = std::env::temp_dir().join(format!("dicron-rgb-{}.dcm", std::process::id()));
        object.write_to_file(&path).unwrap();
        let thumbnail = load_dicom_thumbnail(&path, 0, 2);
        std::fs::remove_file(path).unwrap();
        assert_eq!(rgb(&thumbnail.unwrap().rgba), samples);
    }

    /// Create a native single-row image with packed 4:2:2 pixel pairs.
    fn native_ybr_full_422_row(data: &[u8], columns: u16, frame_count: u32) -> DefaultDicomObject {
        let mut object = compressed_test_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::US,
            1,
            u32::from(columns),
            8,
            vec![],
        );
        set_layout(&mut object, 3);
        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            "YBR_FULL_422 ",
        ));
        object.put_element(DataElement::new(
            tags::NUMBER_OF_FRAMES,
            VR::IS,
            frame_count.to_string(),
        ));
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from(data.to_vec()),
        ));
        object
    }

    /// Opening a file, rendering it, and building its thumbnail agree on
    /// both the packed frame offset and the two pixels that share chroma.
    #[test]
    fn native_ybr_full_422_renders_each_frame_and_thumbnail() {
        let object = native_ybr_full_422_row(&[50, 200, 90, 170, 0, 255, 128, 128], 2, 2);
        let path = std::env::temp_dir().join(format!("dicron-ybr422-{}.dcm", std::process::id()));
        object.write_to_file(&path).unwrap();
        for (index, expected) in [
            (0, [[109, 33, 0], [255, 183, 133]]),
            (1, [[0, 0, 0], [255, 255, 255]]),
        ] {
            let loaded = load_dicom_frame(&path, index).unwrap();
            let frame = loaded.frame;
            assert_eq!(frame.frame_count, 2);
            assert_eq!(rgb(&render_frame(&frame, None).unwrap().rgba), expected);
            assert_eq!(
                frame.pixel_probe(1, 0),
                Some(PixelProbeValue::Rgb(expected[1]))
            );
            let thumbnail = load_dicom_thumbnail(&path, index, 2).unwrap();
            assert_eq!(rgb(&thumbnail.rgba), expected);
        }
        std::fs::remove_file(path).unwrap();
    }

    /// The conversion reads the stored bits and uses their chroma midpoint.
    #[test]
    fn native_ybr_full_422_ignores_unused_sample_bits() {
        let mut object = native_ybr_full_422_row(&[128, 255, 192, 192], 2, 1);
        for (tag, value) in [(tags::BITS_STORED, 7_u16), (tags::HIGH_BIT, 6)] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        let frame = decode_frame(&object, 0).unwrap();
        assert_eq!(
            rgb(&render_frame(&frame, None).unwrap().rgba),
            [[0; 3], [127; 3]]
        );
    }

    /// Reject an incomplete frame or a layout that cannot represent pairs.
    #[test]
    fn native_ybr_full_422_rejects_truncated_frames_and_invalid_layouts() {
        let truncated = native_ybr_full_422_row(&[128; 7], 2, 2);
        assert!(decode_frame(&truncated, 0).is_ok());
        let error = decode_frame(&truncated, 1).err().unwrap();
        assert!(format!("{error:#}").contains("no complete frame 2"));

        let odd_width = native_ybr_full_422_row(&[128; 6], 3, 1);
        assert!(decode_error(&odd_width).contains("even column count"));

        let mut by_plane = native_ybr_full_422_row(&[128; 4], 2, 1);
        by_plane.put_element(DataElement::new(
            tags::PLANAR_CONFIGURATION,
            VR::US,
            PrimitiveValue::from(1_u16),
        ));
        assert!(decode_error(&by_plane).contains("Planar Configuration 0"));
    }

    /// A requested window always wins over the implicit one.
    #[test]
    fn requested_window_overrides_the_implicit_window() {
        let frame = decode_frame(&signed_image("MONOCHROME2"), 0).unwrap();

        assert_eq!(
            greys(
                &render_frame(
                    &frame,
                    Some(super::DicomWindow {
                        center: 500.0,
                        width: 1000.0,
                    }),
                )
                .unwrap()
                .rgba
            ),
            [0, 0, 128, 255]
        );
    }

    /// A file with a VOI LUT Sequence but no window is drawn through its LUT,
    /// not over its full dynamic range.
    #[test]
    fn voi_lut_without_window_keeps_the_lut() {
        use dicom_core::value::DataSetSequence;
        use dicom_object::InMemDicomObject;

        let mut object = sixteen_bit_image("MONOCHROME2", 12, 0, [0, 1, 2, 3]);
        let lut_item = InMemDicomObject::from_element_iter([
            DataElement::new(
                tags::LUT_DESCRIPTOR,
                VR::US,
                PrimitiveValue::U16(vec![4_u16, 0, 8].into()),
            ),
            DataElement::new(
                tags::LUT_DATA,
                VR::OW,
                PrimitiveValue::U16(vec![0_u16, 255, 0, 255].into()),
            ),
        ]);
        object.put_element(DataElement::new(
            tags::VOILUT_SEQUENCE,
            VR::SQ,
            DataSetSequence::from(vec![lut_item]),
        ));
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(
            greys(&render_frame(&frame, None).unwrap().rgba),
            [0, 255, 0, 255]
        );
        assert_eq!(thumbnail_greys(&object, "voi-lut"), [0, 255, 0, 255]);
    }

    /// Each frame of a multi-frame file without a window spans its own values.
    #[test]
    fn multi_frame_without_window_spans_each_frame() {
        let mut object = sixteen_bit_image("MONOCHROME2", 16, 1, [0; 4]);
        object.put_element(DataElement::new(
            tags::NUMBER_OF_FRAMES,
            VR::IS,
            PrimitiveValue::from("2"),
        ));
        let samples: Vec<u16> = [-1000_i16, 0, 500, 1000, 0, 100, 200, 400]
            .map(|value| value as u16)
            .to_vec();
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            PrimitiveValue::U16(samples.into()),
        ));
        let first = decode_frame(&object, 0).unwrap();
        let second = decode_frame(&object, 1).unwrap();

        assert_eq!(first.default_center_width(), (0.0, 2000.0));
        assert_eq!(second.default_center_width(), (200.0, 400.0));
        assert_eq!(
            greys(&render_frame(&second, None).unwrap().rgba),
            [0, 64, 128, 255]
        );
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

    /// A single-frame grayscale image whose pixel data is one compressed
    /// fragment. Rows and Columns are written with `size_vr`, so a test can
    /// give them a VR wider than the standard US.
    fn compressed_test_object(
        transfer_syntax: &str,
        size_vr: VR,
        rows: u32,
        columns: u32,
        bits_allocated: u16,
        fragment: Vec<u8>,
    ) -> DefaultDicomObject {
        use dicom_core::value::PixelFragmentSequence;

        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(transfer_syntax)
            .media_storage_sop_class_uid(uids::SECONDARY_CAPTURE_IMAGE_STORAGE)
            .media_storage_sop_instance_uid("2.25.702")
            .build()
            .unwrap();
        let mut object = FileDicomObject::new_empty_with_meta(meta);
        for (tag, value) in [(tags::ROWS, rows), (tags::COLUMNS, columns)] {
            let value = if size_vr == VR::US {
                PrimitiveValue::from(u16::try_from(value).unwrap())
            } else {
                PrimitiveValue::from(value)
            };
            object.put_element(DataElement::new(tag, size_vr, value));
        }
        let bits_stored = if bits_allocated == 16 { 12 } else { 8 };
        for (tag, value) in [
            (tags::SAMPLES_PER_PIXEL, 1),
            (tags::BITS_ALLOCATED, bits_allocated),
            (tags::BITS_STORED, bits_stored),
            (tags::HIGH_BIT, bits_stored - 1),
            (tags::PIXEL_REPRESENTATION, 0),
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
            PixelFragmentSequence::new(Vec::<u32>::new(), vec![fragment]),
        ));
        object
    }

    /// An RLE fragment whose 64 byte header declares `segment_count` segments
    /// starting at `segment_starts`, followed by `body`.
    fn rle_fragment(segment_count: u32, segment_starts: &[u32], body: &[u8]) -> Vec<u8> {
        let mut fragment = segment_count.to_le_bytes().to_vec();
        for start in segment_starts {
            fragment.extend_from_slice(&start.to_le_bytes());
        }
        fragment.resize(64, 0);
        fragment.extend_from_slice(body);
        fragment
    }

    fn decode_error(object: &DefaultDicomObject) -> String {
        let decoded = super::contain_decoder_panic(|| decode_frame(object, 0));
        format!("{:#}", decoded.err().expect("the frame was decoded"))
    }

    /// About 800 bytes of PackBits can expand to at most 64 times their size,
    /// far short of the 64 million samples an 8000 x 8000 header declares.
    #[test]
    fn rle_too_short_for_its_declared_size_is_refused() {
        // Each two-byte run repeats one byte 128 times, PackBits' largest expansion.
        let body = [0x81, 0x00].repeat(375);
        let object = compressed_test_object(
            uids::RLE_LOSSLESS,
            VR::US,
            8000,
            8000,
            8,
            rle_fragment(1, &[64], &body),
        );

        let error = decode_error(&object);
        assert!(
            error.contains("too short for a 8000 x 8000 image"),
            "{error}"
        );
    }

    #[test]
    fn rle_header_with_a_huge_segment_count_is_refused() {
        let object = compressed_test_object(
            uids::RLE_LOSSLESS,
            VR::US,
            2,
            2,
            16,
            rle_fragment(u32::MAX, &[64], &[3, 1, 2, 3, 4]),
        );

        let error = decode_error(&object);
        assert!(error.contains("4294967295 segments"), "{error}");
    }

    #[test]
    fn rle_segment_offsets_must_ascend_inside_the_fragment() {
        let segment = [3, 1, 2, 3, 4];
        let reversed = rle_fragment(2, &[69, 64], &[segment, segment].concat());
        let beyond = rle_fragment(2, &[64, 9999], &[segment, segment].concat());
        // The extra third segment marks where the second ends, past the fragment.
        let ending_beyond = rle_fragment(3, &[64, 69, 9999], &[segment, segment].concat());

        for fragment in [reversed, beyond, ending_beyond] {
            let object = compressed_test_object(uids::RLE_LOSSLESS, VR::US, 2, 2, 16, fragment);
            let error = decode_error(&object);
            assert!(error.contains("segment offsets"), "{error}");
        }
    }

    /// The decoders read Rows and Columns as US; a wider value is refused
    /// instead of sizing a buffer from it.
    #[test]
    fn rle_rows_and_columns_wider_than_us_are_refused() {
        let object = compressed_test_object(
            uids::RLE_LOSSLESS,
            VR::UL,
            2_147_483_647,
            2_147_483_647,
            8,
            rle_fragment(1, &[64], &[3, 1, 2, 3, 4]),
        );

        let error = decode_error(&object);
        assert!(error.contains("no valid Rows"), "{error}");
    }

    /// A JPEG Extended stream whose frame header agrees with 65500 x 65500
    /// 12-bit Rows and Columns, the largest libjpeg accepts, would need 8.6 GB
    /// decoded.
    #[test]
    fn jpeg_extended_frame_over_the_decode_budget_is_refused() {
        let mut stream = vec![0xFF, 0xD8];
        // Quantization table 0, all ones.
        stream.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        stream.extend_from_slice(&[1; 64]);
        // Extended sequential frame: 12-bit, 65500 x 65500, one component.
        stream.extend_from_slice(&[
            0xFF, 0xC1, 0x00, 0x0B, 0x0C, 0xFF, 0xDC, 0xFF, 0xDC, 0x01, 0x01, 0x11, 0x00,
        ]);
        stream.extend_from_slice(&[
            0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xFF, 0xD9,
        ]);
        let object =
            compressed_test_object(uids::JPEG_EXTENDED12_BIT, VR::US, 65500, 65500, 16, stream);

        let error = decode_error(&object);
        assert!(error.contains("too large to display"), "{error}");
    }

    #[test]
    fn jpeg2000_lossless_has_a_pixel_decoder() {
        const JPEG2000_LOSSLESS_UID: &str = "1.2.840.10008.1.2.4.90";

        let transfer_syntax = TransferSyntaxRegistry
            .get(JPEG2000_LOSSLESS_UID)
            .expect("JPEG 2000 Lossless transfer syntax should be registered");

        assert!(transfer_syntax.pixel_data_reader().is_some());
    }

    /// PackBits for `bytes`: every run of equal bytes (up to 128) as one
    /// repeat run, the densest encoding, and a lone byte as a literal.
    fn packbits_runs(bytes: &[u8]) -> Vec<u8> {
        let mut packed = Vec::new();
        let mut rest = bytes;
        while let Some(&first) = rest.first() {
            let run = rest
                .iter()
                .take(128)
                .take_while(|&&byte| byte == first)
                .count();
            packed.push((1 - run as i16) as i8 as u8);
            packed.push(first);
            rest = &rest[run..];
        }
        packed
    }

    /// PackBits for `bytes` as literal runs of up to 128 bytes.
    fn packbits_literals(bytes: &[u8]) -> Vec<u8> {
        bytes
            .chunks(128)
            .flat_map(|run| std::iter::once((run.len() - 1) as u8).chain(run.iter().copied()))
            .collect()
    }

    /// An RLE fragment holding `segments` in order, after a header that lists them.
    fn rle_frame_fragment(segments: &[Vec<u8>]) -> Vec<u8> {
        let starts: Vec<u32> = segments
            .iter()
            .scan(64_u32, |start, segment| {
                let this = *start;
                *start += segment.len() as u32;
                Some(this)
            })
            .collect();
        rle_fragment(segments.len() as u32, &starts, &segments.concat())
    }

    /// The RLE segments of native little-endian `samples`, one per byte of
    /// each sample, most significant byte first, as the standard orders them.
    fn rle_segments(
        samples: &[u16],
        samples_per_pixel: usize,
        bytes_per_sample: usize,
        encode: fn(&[u8]) -> Vec<u8>,
    ) -> Vec<Vec<u8>> {
        let mut segments = Vec::new();
        for sample in 0..samples_per_pixel {
            for byte in (0..bytes_per_sample).rev() {
                let plane: Vec<u8> = samples
                    .iter()
                    .skip(sample)
                    .step_by(samples_per_pixel)
                    .map(|value| value.to_le_bytes()[byte])
                    .collect();
                segments.push(encode(&plane));
            }
        }
        segments
    }

    /// The native little-endian bytes the decoder must produce for `samples`.
    fn native_bytes(samples: &[u16], bits_allocated: u16) -> Vec<u8> {
        if bits_allocated == 8 {
            samples.iter().map(|&value| value as u8).collect()
        } else {
            samples
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect()
        }
    }

    fn set_layout(object: &mut DefaultDicomObject, samples_per_pixel: u16) {
        object.put_element(DataElement::new(
            tags::SAMPLES_PER_PIXEL,
            VR::US,
            PrimitiveValue::from(samples_per_pixel),
        ));
        if samples_per_pixel == 3 {
            object.put_element(DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from("RGB"),
            ));
            object.put_element(DataElement::new(
                tags::PLANAR_CONFIGURATION,
                VR::US,
                PrimitiveValue::from(0_u16),
            ));
        }
    }

    fn decoded_bytes(object: &DefaultDicomObject, frame: u32) -> Result<Vec<u8>, String> {
        super::contain_decoder_panic(|| super::decode_pixel_frame(object, frame))
            .map(|decoded| decoded.data().to_vec())
            .map_err(|error| format!("{error:#}"))
    }

    /// Valid RLE as encoders write it keeps decoding to its exact samples:
    /// one, two, three and six segments, the densest PackBits (exactly 64
    /// samples per byte, the bound the header check allows) and literal runs.
    #[test]
    fn valid_rle_frames_decode_to_their_samples() {
        for (rows, columns, samples_per_pixel, bits_allocated, constant, encode) in [
            (
                128_u16,
                128_u16,
                1_u16,
                8_u16,
                true,
                packbits_runs as fn(&[u8]) -> Vec<u8>,
            ),
            (128, 128, 1, 16, true, packbits_runs),
            (64, 128, 3, 8, true, packbits_runs),
            (64, 64, 3, 16, true, packbits_runs),
            (5, 7, 1, 8, false, packbits_literals),
            (5, 7, 1, 8, false, packbits_runs),
            (5, 7, 1, 16, false, packbits_runs),
            (3, 5, 3, 8, false, packbits_literals),
            (3, 5, 3, 16, false, packbits_literals),
        ] {
            let count = usize::from(rows) * usize::from(columns) * usize::from(samples_per_pixel);
            let mask = if bits_allocated == 8 { 0xFF } else { 0x0FFF };
            let samples: Vec<u16> = (0..count)
                .map(|index| if constant { 0x0A37 } else { (index / 3) as u16 * 97 + 3 } & mask)
                .collect();
            let segments = rle_segments(
                &samples,
                usize::from(samples_per_pixel),
                usize::from(bits_allocated / 8),
                encode,
            );
            let mut object = compressed_test_object(
                uids::RLE_LOSSLESS,
                VR::US,
                u32::from(rows),
                u32::from(columns),
                bits_allocated,
                rle_frame_fragment(&segments),
            );
            set_layout(&mut object, samples_per_pixel);

            let case = format!("{columns}x{rows}, {samples_per_pixel} x {bits_allocated} bit");
            assert_eq!(
                decoded_bytes(&object, 0),
                Ok(native_bytes(&samples, bits_allocated)),
                "{case}"
            );
        }
    }

    /// RLE stores color channels in separate segments even when the rebuilt
    /// native frame presents them as interleaved pixels.
    #[test]
    fn sixteen_bit_rgb_rle_renders_the_correct_colours() {
        let samples = [0x1234, 0x5678, 0x9ABC, 0xDEF0, 0x1357, 0x2468];
        let segments = rle_segments(&samples, 3, 2, packbits_literals);
        let mut object = compressed_test_object(
            uids::RLE_LOSSLESS,
            VR::US,
            1,
            2,
            16,
            rle_frame_fragment(&segments),
        );
        set_layout(&mut object, 3);
        object.put_element(DataElement::new(
            tags::PLANAR_CONFIGURATION,
            VR::US,
            PrimitiveValue::from(1_u16),
        ));

        let frame = decode_frame(&object, 0).unwrap();
        assert_eq!(
            render_frame(&frame, None).unwrap().rgba,
            [0x12, 0x56, 0x9A, 255, 0xDE, 0x13, 0x24, 255]
        );
        assert_eq!(
            frame.pixel_probe(1, 0),
            Some(PixelProbeValue::Rgb([0xDE, 0x13, 0x24]))
        );
    }

    /// Each 32-bit sample contributes four byte planes, in significance order.
    #[test]
    fn thirty_two_bit_rle_preserves_sample_bytes() {
        let values = [
            0x0102_0304_u32,
            0x0506_0708,
            0x090A_0B0C,
            0x0D0E_0F10,
            0x1112_1314,
            0x1516_1718,
        ];
        for samples_per_pixel in [1, 3] {
            let channels = usize::from(samples_per_pixel);
            let values = &values[..channels * 2];
            let mut segments = Vec::new();
            for channel in 0..channels {
                for byte in 0..4 {
                    let plane: Vec<_> = values
                        .iter()
                        .skip(channel)
                        .step_by(channels)
                        .map(|value| value.to_be_bytes()[byte])
                        .collect();
                    segments.push(packbits_literals(&plane));
                }
            }
            let mut object = compressed_test_object(
                uids::RLE_LOSSLESS,
                VR::US,
                1,
                2,
                32,
                rle_frame_fragment(&segments),
            );
            set_layout(&mut object, samples_per_pixel);
            assert_eq!(
                decoded_bytes(&object, 0),
                Ok(values
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect()),
                "{channels} channels"
            );
        }
    }

    /// The header may list more segments than the samples need, up to the
    /// fifteen the standard allows; the extra ones are ignored, not refused.
    #[test]
    fn rle_header_with_up_to_fifteen_segments_decodes() {
        let samples: Vec<u16> = (0..12).map(|index| index * 300).collect();
        let mut segments = rle_segments(&samples, 1, 2, packbits_literals);
        // One-byte no-op segments, the shortest a segment can be.
        while segments.len() < 15 {
            segments.push(vec![0x80]);
        }
        let object = compressed_test_object(
            uids::RLE_LOSSLESS,
            VR::US,
            3,
            4,
            16,
            rle_frame_fragment(&segments),
        );
        assert_eq!(decoded_bytes(&object, 0), Ok(native_bytes(&samples, 16)));

        segments.push(vec![0x80]);
        let object = compressed_test_object(
            uids::RLE_LOSSLESS,
            VR::US,
            3,
            4,
            16,
            rle_frame_fragment(&segments),
        );
        let error = decoded_bytes(&object, 0).unwrap_err();
        assert!(error.contains("16 segments where 2 are needed"), "{error}");
    }

    /// A header that lists fewer segments than each byte of each sample needs
    /// is refused, whatever the data.
    #[test]
    fn rle_header_with_too_few_segments_is_refused() {
        let samples: Vec<u16> = (0..12).map(|index| index * 7).collect();
        let segments = rle_segments(&samples, 3, 1, packbits_literals);
        let mut object = compressed_test_object(
            uids::RLE_LOSSLESS,
            VR::US,
            2,
            2,
            8,
            rle_frame_fragment(&segments[..2]),
        );
        set_layout(&mut object, 3);

        let error = decoded_bytes(&object, 0).unwrap_err();
        assert!(error.contains("2 segments where 3 are needed"), "{error}");
    }

    /// A segment that starts inside the 64 byte header is refused; the
    /// valid frames above all start their first segment at 64.
    #[test]
    fn rle_segment_inside_the_header_is_refused() {
        for start in [0, 4, 60, 63] {
            let mut fragment = rle_fragment(1, &[start], &[]);
            fragment.extend_from_slice(&[0x03, 1, 2, 3, 4]);
            let object = compressed_test_object(uids::RLE_LOSSLESS, VR::US, 2, 2, 8, fragment);

            let error = decoded_bytes(&object, 0).unwrap_err();
            assert!(error.contains("segment offsets"), "{start}: {error}");
        }
    }

    /// Padding after the last run and an odd fragment length are fine.
    #[test]
    fn eight_bit_rle_with_padding_and_odd_length_decodes() {
        // 1 x 3 pixels: one literal run of three bytes, with and without a
        // pad byte; and a repeat run of four, cut to the three pixels.
        for (body, expected) in [
            (vec![0x02, 9, 8, 7], vec![9, 8, 7]),
            (vec![0x02, 9, 8, 7, 0x00], vec![9, 8, 7]),
            (vec![0xFD, 5], vec![5, 5, 5]),
        ] {
            let object = compressed_test_object(
                uids::RLE_LOSSLESS,
                VR::US,
                1,
                3,
                8,
                rle_fragment(1, &[64], &body),
            );
            assert_eq!(decoded_bytes(&object, 0), Ok(expected), "{body:?}");
        }
    }

    /// Each frame of a multi-frame RLE image is its own fragment; the header
    /// of the requested frame is the one checked.
    #[test]
    fn multi_frame_rle_decodes_the_requested_frame() {
        use dicom_core::value::PixelFragmentSequence;

        for bits_allocated in [8_u16, 16] {
            let frames: Vec<Vec<u16>> = (0..2)
                .map(|frame| (0..6).map(|index| frame * 50 + index).collect())
                .collect();
            let fragments: Vec<Vec<u8>> = frames
                .iter()
                .map(|samples| {
                    let bytes = usize::from(bits_allocated / 8);
                    rle_frame_fragment(&rle_segments(samples, 1, bytes, packbits_literals))
                })
                .collect();
            let mut damaged = fragments.clone();
            damaged[0][0] = 9;
            let offset_table = vec![0, fragments[0].len() as u32 + 8];

            for (fragments, first_frame_valid) in [(fragments, true), (damaged, false)] {
                let mut object = compressed_test_object(
                    uids::RLE_LOSSLESS,
                    VR::US,
                    2,
                    3,
                    bits_allocated,
                    vec![],
                );
                object.put_element(DataElement::new(
                    tags::NUMBER_OF_FRAMES,
                    VR::IS,
                    PrimitiveValue::from("2"),
                ));
                object.put_element(DataElement::new(
                    tags::PIXEL_DATA,
                    VR::OB,
                    PixelFragmentSequence::new(offset_table.clone(), fragments),
                ));

                assert_eq!(
                    decoded_bytes(&object, 1),
                    Ok(native_bytes(&frames[1], bits_allocated)),
                    "{bits_allocated} bit"
                );
                assert_eq!(
                    decoded_bytes(&object, 0).is_ok(),
                    first_frame_valid,
                    "{bits_allocated} bit"
                );
            }
        }
    }

    fn enhanced_frame_groups(
        center: &str,
        width: &str,
        slope: &str,
        intercept: &str,
    ) -> dicom_object::InMemDicomObject {
        use dicom_core::value::DataSetSequence;
        use dicom_object::InMemDicomObject;

        let voi = InMemDicomObject::from_element_iter([
            DataElement::new(tags::WINDOW_CENTER, VR::DS, center),
            DataElement::new(tags::WINDOW_WIDTH, VR::DS, width),
        ]);
        let rescale = InMemDicomObject::from_element_iter([
            DataElement::new(tags::RESCALE_SLOPE, VR::DS, slope),
            DataElement::new(tags::RESCALE_INTERCEPT, VR::DS, intercept),
        ]);
        InMemDicomObject::from_element_iter([
            DataElement::new(
                tags::FRAME_VOILUT_SEQUENCE,
                VR::SQ,
                DataSetSequence::from(vec![voi]),
            ),
            DataElement::new(
                tags::PIXEL_VALUE_TRANSFORMATION_SEQUENCE,
                VR::SQ,
                DataSetSequence::from(vec![rescale]),
            ),
        ])
    }

    /// Two 8-bit grayscale frames with the same samples on each decode path.
    fn enhanced_test_image(transfer_syntax: &str) -> DefaultDicomObject {
        const SAMPLE: u16 = 11;
        if transfer_syntax == uids::RLE_LOSSLESS {
            let fragment = rle_frame_fragment(&[packbits_literals(&[SAMPLE as u8; 64])]);
            let mut object = compressed_test_object(uids::RLE_LOSSLESS, VR::US, 8, 8, 8, vec![]);
            object.put_element(DataElement::new(tags::NUMBER_OF_FRAMES, VR::IS, "2"));
            return with_fragments(&object, vec![], vec![fragment.clone(), fragment]);
        }

        if transfer_syntax == uids::JPEG_EXTENDED12_BIT {
            let baseline = encoded_object(uids::JPEG_BASELINE8_BIT, (8, 8, 1), 8, &[SAMPLE; 128]);
            let (_, fragments) = pixel_fragments(&baseline);
            let mut object = with_fragments(&baseline, vec![], fragments);
            object.meta_mut().transfer_syntax = transfer_syntax.to_owned();
            return object;
        }

        encoded_object(transfer_syntax, (8, 8, 1), 8, &[SAMPLE; 128])
    }

    #[test]
    fn enhanced_frames_use_their_own_window_and_rescale() {
        use dicom_core::value::DataSetSequence;

        for syntax in [
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            uids::RLE_LOSSLESS,
            uids::JPEG_EXTENDED12_BIT,
        ] {
            let mut object = enhanced_test_image(syntax);
            object.put_element(DataElement::new(
                tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
                VR::SQ,
                DataSetSequence::from(vec![
                    enhanced_frame_groups("20", "40", "2", "-5"),
                    enhanced_frame_groups("150", "200", "3", "100"),
                ]),
            ));

            for (index, window, range, sample) in [
                (0, (20.0, 40.0), (-5.0, 505.0), 17.0),
                (1, (150.0, 200.0), (100.0, 865.0), 133.0),
            ] {
                let frame = decode_frame(&object, index).unwrap();
                assert_eq!(
                    frame.default_center_width(),
                    window,
                    "{syntax}, frame {index}"
                );
                assert_eq!(frame.value_range, range, "{syntax}, frame {index}");
                assert_eq!(
                    frame.pixel_probe(0, 0),
                    Some(PixelProbeValue::Monochrome(sample)),
                    "{syntax}, frame {index}"
                );
                let (center, width) = window;
                let original = render_frame(&frame, None).unwrap();
                let explicit =
                    render_frame(&frame, Some(super::DicomWindow { center, width })).unwrap();
                assert_eq!(original.rgba, explicit.rgba, "{syntax}, frame {index}");
            }
        }
    }

    #[test]
    fn rebuilt_frames_keep_shared_groups_and_reject_missing_frame_items() {
        use dicom_core::value::DataSetSequence;

        for syntax in [uids::RLE_LOSSLESS, uids::JPEG_EXTENDED12_BIT] {
            let mut object = enhanced_test_image(syntax);
            object.put_element(DataElement::new(
                tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
                VR::SQ,
                DataSetSequence::from(vec![enhanced_frame_groups("20", "40", "2", "-5")]),
            ));
            let second = decode_frame(&object, 1).unwrap();
            assert_eq!(second.default_center_width(), (20.0, 40.0), "{syntax}");
            assert_eq!(
                second.pixel_probe(0, 0),
                Some(PixelProbeValue::Monochrome(17.0)),
                "{syntax}"
            );

            object.remove_element(tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE);
            object.put_element(DataElement::new(
                tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
                VR::SQ,
                DataSetSequence::from(vec![enhanced_frame_groups("20", "40", "2", "-5")]),
            ));
            assert!(decode_frame(&object, 0).is_ok(), "{syntax}");
            let error = decode_frame(&object, 1)
                .err()
                .expect("missing group must fail");
            assert!(
                format!("{error:#}").contains("missing functional groups for frame 2"),
                "{syntax}: {error:#}"
            );
        }
    }

    /// A monochrome frame may hold 2^27 samples, counting samples per pixel
    /// but not bytes per sample; a colour frame may decode to 512 MiB. A
    /// frame of exactly that size reaches the RLE header check, one row or
    /// column more is refused by the budget. Neither allocates its size. A
    /// 45 MP camera's RGB photograph is within the colour budget, and three
    /// samples per pixel under a monochrome interpretation are counted as
    /// samples, as the 2D view keeps an f64 probe value for each of them.
    #[test]
    fn decode_budget_counts_monochrome_samples_and_colour_bytes() {
        for (photometric, rows, columns, samples_per_pixel, bits_allocated, within) in [
            ("MONOCHROME2", 8192_u32, 16384_u32, 1_u16, 16_u16, true),
            ("MONOCHROME2", 8193, 16384, 1, 16, false),
            ("MONOCHROME2", 8192, 16384, 1, 8, true),
            ("MONOCHROME2", 8192, 16385, 1, 8, false),
            ("MONOCHROME1", 11585, 11585, 1, 8, true),
            ("MONOCHROME1", 11586, 11586, 1, 8, false),
            ("PALETTE COLOR", 11585, 11585, 1, 8, true),
            ("PALETTE COLOR", 11586, 11586, 1, 8, false),
            ("MONOCHROME2", 6688, 6688, 3, 8, true),
            ("MONOCHROME2", 6689, 6689, 3, 8, false),
            ("RGB", 5464, 8192, 3, 8, true),
            ("RGB", 13377, 13377, 3, 8, true),
            ("RGB", 13378, 13377, 3, 8, false),
            ("YBR_FULL", 13377, 13377, 3, 8, true),
            ("YBR_FULL", 13377, 13378, 3, 8, false),
            ("RGB", 9459, 9459, 3, 16, true),
            ("RGB", 9460, 9459, 3, 16, false),
        ] {
            let segment_count = u32::from(samples_per_pixel * bits_allocated / 8);
            let starts: Vec<u32> = (0..segment_count).map(|index| 64 + 2 * index).collect();
            let body = [0x81, 0x00].repeat(segment_count as usize);
            let mut object = compressed_test_object(
                uids::RLE_LOSSLESS,
                VR::US,
                rows,
                columns,
                bits_allocated,
                rle_fragment(segment_count, &starts, &body),
            );
            set_layout(&mut object, samples_per_pixel);
            object.put_element(DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from(photometric),
            ));

            let error = decoded_bytes(&object, 0).unwrap_err();
            let case = format!(
                "{photometric} {columns}x{rows}, {samples_per_pixel} x {bits_allocated} bit"
            );
            if within {
                assert!(error.contains("a segment is too short"), "{case}: {error}");
            } else {
                assert!(error.contains("too large to display"), "{case}: {error}");
            }
        }
    }

    /// The JPEG decoder sizes its output from the stream's own frame header,
    /// so a baseline header claiming 65500 x 65500 behind 8 x 8 DICOM
    /// attributes would fill 4.2 GB. It is refused as damaged before the
    /// codec runs, as JPEG Extended is when its frame header disagrees with
    /// Rows and Columns.
    #[test]
    fn jpeg_baseline_frame_header_larger_than_rows_columns_is_refused() {
        let mut stream = vec![0xFF, 0xD8];
        stream.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        stream.extend_from_slice(&[1; 64]);
        // Baseline frame: 8-bit, 65500 x 65500, one component.
        stream.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0xFF, 0xDC, 0xFF, 0xDC, 0x01, 0x01, 0x11, 0x00,
        ]);
        // One-code DC and AC tables.
        stream.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x26]);
        for class in [0x00_u8, 0x10] {
            stream.extend_from_slice(&[class, 1]);
            stream.extend_from_slice(&[0; 15]);
            stream.push(0);
        }
        stream.extend_from_slice(&[
            0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x00, 0x00, 0xFF, 0xD9,
        ]);
        let object = compressed_test_object(uids::JPEG_BASELINE8_BIT, VR::US, 8, 8, 8, stream);

        let error = decode_error(&object);
        assert!(
            error.contains("damaged: its frame header describes 65500 x 65500 pixels"),
            "{error}"
        );
    }

    /// The JPEG-LS decoder sizes its output from the stream's own frame
    /// header: 65535 x 65535 16-bit RGB there, behind 8 x 8 grayscale DICOM
    /// attributes, would ask for 25.8 GB. It is refused as damaged before
    /// the codec runs.
    #[test]
    fn jpeg_ls_frame_header_larger_than_rows_columns_is_refused() {
        let mut stream = vec![0xFF, 0xD8];
        // JPEG-LS frame: 16-bit, 65535 x 65535, three components.
        stream.extend_from_slice(&[
            0xFF, 0xF7, 0x00, 0x11, 0x10, 0xFF, 0xFF, 0xFF, 0xFF, 0x03, 0x01, 0x11, 0x00, 0x02,
            0x11, 0x00, 0x03, 0x11, 0x00,
        ]);
        stream.extend_from_slice(&[
            0xFF, 0xDA, 0x00, 0x0C, 0x03, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x00, 0x02, 0x00,
        ]);
        stream.extend_from_slice(&[0x00; 64]);
        stream.extend_from_slice(&[0xFF, 0xD9]);
        let object = compressed_test_object(uids::JPEGLS_LOSSLESS, VR::US, 8, 8, 16, stream);

        let error = decode_error(&object);
        assert!(
            error.contains("damaged: its frame header describes 65535 x 65535 pixels of 3 samples"),
            "{error}"
        );
    }

    /// An overlay plane on a non-square image is cut to the image's columns
    /// and rows, not to its rows and columns, in the full image and the thumbnail.
    #[test]
    fn overlay_on_a_wide_image_is_painted_to_its_last_column() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        for (tag, value) in [
            (tags::ROWS, 2_u16),
            (tags::COLUMNS, 6_u16),
            (Tag(0x6000, 0x0010), 2_u16),
            (Tag(0x6000, 0x0011), 6_u16),
        ] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from(vec![0_u8; 12]),
        ));
        // Row 0: columns 0 and 5; row 1: column 5.
        object.put_element(DataElement::new(
            Tag(0x6000, 0x3000),
            VR::OB,
            PrimitiveValue::from(vec![0b0010_0001_u8, 0b0000_1000]),
        ));
        let white_pixels = |rgba: &[u8]| -> Vec<usize> {
            rgba.chunks(4)
                .enumerate()
                .filter(|(_, pixel)| pixel[0] == 255)
                .map(|(index, _)| index)
                .collect()
        };

        let frame = decode_frame(&object, 0).unwrap();
        let pixels = render_frame(&frame, None).unwrap();
        assert_eq!((pixels.width, pixels.height), (6, 2));
        assert_eq!(white_pixels(&pixels.rgba), vec![0, 5, 11]);

        let path =
            std::env::temp_dir().join(format!("dicron-overlay-wide-{}.dcm", std::process::id()));
        object.write_to_file(&path).unwrap();
        let thumbnail = load_dicom_thumbnail(&path, 0, 6);
        std::fs::remove_file(&path).unwrap();
        let thumbnail = thumbnail.unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (6, 2));
        assert_eq!(white_pixels(&thumbnail.rgba), vec![0, 5, 11]);
    }

    /// Two PackBits bytes expand to at most 128 samples, so a two-byte
    /// segment for a 129-pixel image is refused by the header check rather
    /// than decoded.
    #[test]
    fn rle_segment_one_sample_beyond_the_densest_packbits_is_refused() {
        let object = compressed_test_object(
            uids::RLE_LOSSLESS,
            VR::US,
            1,
            129,
            8,
            rle_fragment(1, &[64], &[0x81, 7]),
        );

        let error = decoded_bytes(&object, 0).unwrap_err();
        assert!(error.contains("too short for a 129 x 1 image"), "{error}");
    }

    /// An overlay plane that lies entirely outside the image draws nothing,
    /// so it yields no bitmap and the page is drawn like a zero frame without
    /// overlays. The black background exists only to keep visible overlay
    /// text readable, and there is none here.
    #[test]
    fn overlay_outside_the_image_draws_the_page_like_one_without_overlays() {
        let mut object = black_two_by_two_image_with_overlay_pixel();
        object.put_element(DataElement::new(
            Tag(0x6000, 0x0050),
            VR::SS,
            PrimitiveValue::I16([3_i16, 1].to_vec().into()),
        ));
        let off_image = render_frame(&decode_frame(&object, 0).unwrap(), None)
            .unwrap()
            .rgba;

        object.remove_element(Tag(0x6000, 0x3000));
        let without_overlay = render_frame(&decode_frame(&object, 0).unwrap(), None)
            .unwrap()
            .rgba;
        assert_eq!(off_image, without_overlay);
    }

    /// The shortest PackBits a segment can need costs two bytes for every
    /// byte it holds (one-byte literal runs), plus one pad byte. An 8-bit RGB
    /// frame written that way decodes; one byte more in any segment is
    /// refused before the library expands it.
    #[test]
    fn rle_segment_longer_than_any_encoding_is_refused() {
        let samples: Vec<u16> = (0..12).map(|index| index * 20 + 1).collect();
        let one_byte_literals = |plane: &[u8]| -> Vec<u8> {
            // A no-op run as the pad byte, which the library skips.
            plane
                .iter()
                .flat_map(|&byte| [0, byte])
                .chain([0x80])
                .collect()
        };
        let segments = rle_segments(&samples, 3, 1, one_byte_literals);
        let object_with = |segments: &[Vec<u8>]| {
            let mut object = compressed_test_object(
                uids::RLE_LOSSLESS,
                VR::US,
                2,
                2,
                8,
                rle_frame_fragment(segments),
            );
            set_layout(&mut object, 3);
            object
        };
        assert_eq!(
            decoded_bytes(&object_with(&segments), 0),
            Ok(native_bytes(&samples, 8))
        );

        for index in 0..3 {
            let mut longer = segments.clone();
            longer[index].push(0x80);
            let error = decoded_bytes(&object_with(&longer), 0).unwrap_err();
            assert!(
                error.contains("a segment is longer than a 2 x 2 image needs"),
                "segment {index}: {error}"
            );
        }
    }

    /// Segments past the ones the samples need are never read, so an empty
    /// one at the very end of the fragment does not stop the frame decoding.
    #[test]
    fn rle_empty_extra_segment_at_the_end_decodes() {
        let samples: Vec<u16> = (0..4).map(|index| index * 300 + 5).collect();
        let segments = rle_segments(&samples, 1, 2, packbits_literals);
        let mut fragment = rle_frame_fragment(&segments);
        fragment[..4].copy_from_slice(&3_u32.to_le_bytes());
        let end = fragment.len() as u32;
        fragment[12..16].copy_from_slice(&end.to_le_bytes());
        let object = compressed_test_object(uids::RLE_LOSSLESS, VR::US, 2, 2, 16, fragment);

        assert_eq!(decoded_bytes(&object, 0), Ok(native_bytes(&samples, 16)));
    }

    /// A 5 x 3 8-bit grayscale JPEG 2000 codestream, lossless, as
    /// opj_compress wrote it; it holds `(index * 17 + 3) as u8` per pixel.
    const JPEG2000_5_BY_3: [u8; 101] = [
        0xFF, 0x4F, 0xFF, 0x51, 0x00, 0x29, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x01,
        0xFF, 0x52, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x04, 0x04, 0x00, 0x01, 0xFF,
        0x5C, 0x00, 0x04, 0x40, 0x40, 0xFF, 0x90, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x22,
        0x00, 0x01, 0xFF, 0x93, 0xCF, 0xB4, 0x44, 0x07, 0x73, 0xD2, 0xB7, 0xDF, 0x99, 0x2F, 0x1E,
        0xA0, 0x28, 0xAE, 0xBF, 0x0E, 0xB7, 0x07, 0xE2, 0x3F, 0xFF, 0xD9,
    ];

    /// `codestream` in a JP2 file: signature, file type, header and the
    /// contiguous codestream box.
    fn jp2_file(codestream: &[u8]) -> Vec<u8> {
        let mut file = vec![
            0x00, 0x00, 0x00, 0x0C, b'j', b'P', b' ', b' ', 0x0D, 0x0A, 0x87, 0x0A, 0x00, 0x00,
            0x00, 0x14, b'f', b't', b'y', b'p', b'j', b'p', b'2', b' ', 0x00, 0x00, 0x00, 0x00,
            b'j', b'p', b'2', b' ', 0x00, 0x00, 0x00, 0x2D, b'j', b'p', b'2', b'h', 0x00, 0x00,
            0x00, 0x16, b'i', b'h', b'd', b'r', 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x05,
            0x00, 0x01, 0x07, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0F, b'c', b'o', b'l', b'r',
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x11,
        ];
        file.extend_from_slice(&(codestream.len() as u32 + 8).to_be_bytes());
        file.extend_from_slice(b"jp2c");
        file.extend_from_slice(codestream);
        file
    }

    /// A JPEG 2000 frame is decoded at the size its SIZ marker states. A SIZ
    /// that agrees with Rows, Columns and SamplesPerPixel decodes, raw or in
    /// a JP2 file. One that is transposed is refused rather than drawn
    /// scrambled, and one of 40000 x 40000 behind 5 x 3 attributes is refused
    /// as damaged before openjpeg allocates 6.4 GB for it.
    #[test]
    fn jpeg2000_frame_header_must_agree_with_rows_and_columns() {
        let expected: Vec<u8> = (0..15).map(|index| (index * 17 + 3) as u8).collect();
        let mut huge = JPEG2000_5_BY_3;
        for offset in [8, 12, 24, 28] {
            huge[offset..offset + 4].copy_from_slice(&40000_u32.to_be_bytes());
        }

        // The same image placed at (10, 20) on the reference grid, in one tile.
        let mut offset = JPEG2000_5_BY_3;
        for (field, value) in [8, 12, 16, 20, 24, 28]
            .into_iter()
            .zip([15_u32, 23, 10, 20, 15, 23])
        {
            offset[field..field + 4].copy_from_slice(&value.to_be_bytes());
        }

        for (name, stream) in [
            ("codestream", JPEG2000_5_BY_3.to_vec()),
            ("codestream with an image offset", offset.to_vec()),
            ("JP2", jp2_file(&JPEG2000_5_BY_3)),
        ] {
            let object =
                compressed_test_object(uids::JPEG2000_LOSSLESS, VR::US, 3, 5, 8, stream.clone());
            assert_eq!(decoded_bytes(&object, 0), Ok(expected.clone()), "{name}");

            let object = compressed_test_object(uids::JPEG2000_LOSSLESS, VR::US, 5, 3, 8, stream);
            let error = decoded_bytes(&object, 0).unwrap_err();
            assert!(
                error.contains("its frame header describes 5 x 3 pixels of 1 samples"),
                "{name}: {error}"
            );
        }
        for (name, stream) in [("codestream", huge.to_vec()), ("JP2", jp2_file(&huge))] {
            let object = compressed_test_object(uids::JPEG2000_LOSSLESS, VR::US, 3, 5, 8, stream);
            let error = decoded_bytes(&object, 0).unwrap_err();
            assert!(
                error.contains("damaged: its frame header describes 40000 x 40000 pixels"),
                "{name}: {error}"
            );
        }
    }

    /// JPEG Baseline and JPEG-LS frames that agree with Rows and Columns
    /// decode; the same streams behind transposed Rows and Columns, which the
    /// library would decode to scrambled rows, are refused.
    #[test]
    fn jpeg_and_jpeg_ls_frames_must_agree_with_rows_and_columns() {
        use dicom_pixeldata::Transcode;
        use dicom_transfer_syntax_registry::entries;

        let samples: Vec<u8> = (0..15).map(|index| (index * 17 + 3) as u8).collect();
        for (name, transfer_syntax, lossless) in [
            ("JPEG Baseline", entries::JPEG_BASELINE.erased(), false),
            (
                "JPEG-LS",
                entries::JPEG_LS_LOSSLESS_IMAGE_COMPRESSION.erased(),
                true,
            ),
        ] {
            let mut object =
                compressed_test_object(uids::EXPLICIT_VR_LITTLE_ENDIAN, VR::US, 3, 5, 8, vec![]);
            object.put_element(DataElement::new(
                tags::PIXEL_DATA,
                VR::OB,
                PrimitiveValue::from(samples.clone()),
            ));
            object.transcode(&transfer_syntax).unwrap();

            let decoded = decoded_bytes(&object, 0).unwrap();
            assert_eq!(decoded.len(), samples.len(), "{name}");
            if lossless {
                assert_eq!(decoded, samples, "{name}");
            }

            for (tag, value) in [(tags::ROWS, 5_u16), (tags::COLUMNS, 3)] {
                object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
            }
            let error = decoded_bytes(&object, 0).unwrap_err();
            assert!(
                error.contains("its frame header describes 5 x 3 pixels of 1 samples"),
                "{name}: {error}"
            );
        }
    }

    /// A JPEG-LS frame header may leave its size to a later preset parameters
    /// segment, which CharLS reads up to 2^32 x 2^32. A frame header of 0 x 0
    /// never agrees with the image, even one whose Rows and Columns are 0.
    #[test]
    fn jpeg_ls_frame_that_leaves_its_size_to_a_preset_segment_is_refused() {
        let mut stream = vec![0xFF, 0xD8];
        // JPEG-LS frame: 8-bit, 0 x 0, one component.
        stream.extend_from_slice(&[
            0xFF, 0xF7, 0x00, 0x0B, 0x08, 0, 0, 0, 0, 0x01, 0x01, 0x11, 0x00,
        ]);
        // Oversize image dimension: two-byte values, 2 x 2.
        stream.extend_from_slice(&[0xFF, 0xF8, 0x00, 0x08, 0x04, 0x02, 0x00, 0x02, 0x00, 0x02]);
        stream.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00]);
        stream.extend_from_slice(&[0x00; 8]);
        stream.extend_from_slice(&[0xFF, 0xD9]);
        let object = compressed_test_object(uids::JPEGLS_LOSSLESS, VR::US, 0, 0, 8, stream);

        let error = decode_error(&object);
        assert!(
            error.contains("its frame header describes 0 x 0 pixels"),
            "{error}"
        );
    }

    /// JPEG XL decoders size their output from the codestream too, but this
    /// build has no decoder for it, so it needs no frame header check.
    #[test]
    fn jpeg_xl_has_no_pixel_decoder() {
        for uid in [
            uids::JPEGXL_LOSSLESS,
            uids::JPEGXLJPEG_RECOMPRESSION,
            uids::JPEGXL,
        ] {
            let transfer_syntax = TransferSyntaxRegistry.get(uid).unwrap();
            assert!(transfer_syntax.pixel_data_reader().is_none(), "{uid}");
        }
    }

    /// Deflated Image Frame Compression frames decode at their size, 8 or
    /// 16 bits per sample. The library inflates a frame with no limit, so
    /// one that inflates past Rows x Columns x SamplesPerPixel x bytes per
    /// sample, by 16 bytes or by a megabyte of zeros, is refused as damaged
    /// before the library runs.
    #[test]
    fn deflated_frame_that_inflates_past_its_size_is_refused() {
        let samples: Vec<u16> = (0..256).map(|index| index * 7 % 256).collect();
        let object = encoded_object(
            uids::DEFLATED_IMAGE_FRAME_COMPRESSION,
            (16, 16, 1),
            8,
            &samples,
        );
        assert_eq!(decoded_bytes(&object, 0), Ok(native_bytes(&samples, 8)));

        let wide: Vec<u16> = (0..16).map(|index| index * 4099).collect();
        let sixteen_bit =
            encoded_object(uids::DEFLATED_IMAGE_FRAME_COMPRESSION, (4, 4, 1), 16, &wide);
        assert_eq!(decoded_bytes(&sixteen_bit, 0), Ok(native_bytes(&wide, 16)));

        for (rows, columns) in [(16_u16, 17_u16), (1024, 1024)] {
            let larger = encoded_object(
                uids::DEFLATED_IMAGE_FRAME_COMPRESSION,
                (rows, columns, 1),
                8,
                &vec![0; usize::from(rows) * usize::from(columns)],
            );
            let (_, fragments) = pixel_fragments(&larger);
            let object = with_fragments(&object, vec![], fragments);
            let error = decoded_bytes(&object, 0).unwrap_err();
            assert!(
                error.contains(
                    "the deflated pixel data is damaged: a frame inflates to more than \
                     the 256 bytes of a 16 x 16 image"
                ),
                "{columns} x {rows}: {error}"
            );
        }
    }

    /// Compress raw pixel bytes without changing the sample layout.
    fn raw_deflate(bytes: &[u8]) -> Vec<u8> {
        use dicom_encoding::transfer_syntax::Codec;
        use std::io::Write;

        let Some(Codec::Dataset(Some(adapter))) = TransferSyntaxRegistry
            .get(uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN)
            .map(|transfer_syntax| transfer_syntax.codec())
        else {
            panic!("no deflate codec");
        };
        let mut stream = Vec::new();
        {
            let mut writer = adapter.adapt_writer(Box::new(&mut stream));
            writer.write_all(bytes).unwrap();
        }
        stream
    }

    /// Native, RLE, and deflated frames produce the same RGB.
    #[test]
    fn ybr_full_colors_match_for_native_rle_and_deflate() {
        let stored = [50_u8, 90, 170, 200, 90, 170];
        let samples = stored.map(u16::from);
        let mut native =
            compressed_test_object(uids::EXPLICIT_VR_LITTLE_ENDIAN, VR::US, 1, 2, 8, vec![]);
        native.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from(stored.to_vec()),
        ));
        let rle = compressed_test_object(
            uids::RLE_LOSSLESS,
            VR::US,
            1,
            2,
            8,
            rle_frame_fragment(&rle_segments(&samples, 3, 1, packbits_literals)),
        );
        let deflated = compressed_test_object(
            uids::DEFLATED_IMAGE_FRAME_COMPRESSION,
            VR::US,
            1,
            2,
            8,
            raw_deflate(&stored),
        );
        for (name, mut object) in [("native", native), ("RLE", rle), ("deflated", deflated)] {
            set_layout(&mut object, 3);
            object.put_element(DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                "YBR_FULL",
            ));
            let frame =
                decode_frame(&object, 0).unwrap_or_else(|error| panic!("{name}: {error:#}"));
            assert_eq!(
                rgb(&render_frame(&frame, None).unwrap().rgba),
                [[109, 33, 0], [255, 183, 133]],
                "{name}"
            );
        }
    }

    /// These CharLS fixtures encode the same two pixels with three layouts.
    #[test]
    fn ybr_full_jpeg_ls_uses_the_layout_from_the_scan_header() {
        const HEADER: &[u8] = &[
            0xff, 0xd8, 0xff, 0xf7, 0x00, 0x11, 0x08, 0x00, 0x01, 0x00, 0x02, 0x03, 0x01, 0x11,
            0x00, 0x02, 0x11, 0x00, 0x03, 0x11, 0x00,
        ];
        const PLANAR: &[u8] = &[
            0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x62,
            0x00, 0x00, 0x01, 0xd3, 0xff, 0xda, 0x00, 0x08, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 0xb2, 0x80, 0xff, 0xda, 0x00, 0x08, 0x01, 0x03, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x01, 0xa9, 0x80, 0xff, 0xd9,
        ];
        const LINE: &[u8] = &[
            0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00, 0x01, 0x62, 0x00, 0x00, 0x01, 0xd3, 0x03, 0x38, 0x23, 0x54, 0x10, 0xff,
            0xd9,
        ];
        const SAMPLE: &[u8] = &[
            0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x00, 0x02, 0x00,
            0x00, 0x00, 0x01, 0x63, 0x06, 0x86, 0xb0, 0x00, 0x00, 0x1d, 0x38, 0x30, 0x40, 0xff,
            0xd9,
        ];
        for (name, scan) in [("planar", PLANAR), ("line", LINE), ("sample", SAMPLE)] {
            let mut object = compressed_test_object(
                uids::JPEGLS_LOSSLESS,
                VR::US,
                1,
                2,
                8,
                [HEADER, scan].concat(),
            );
            set_layout(&mut object, 3);
            object.put_element(DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                "YBR_FULL",
            ));
            let frame =
                decode_frame(&object, 0).unwrap_or_else(|error| panic!("{name}: {error:#}"));
            assert_eq!(
                rgb(&render_frame(&frame, None).unwrap().rgba),
                [[109, 33, 0], [255, 183, 133]],
                "{name}"
            );
        }
    }

    /// Each plane retains its sample precision and neutral chroma midpoint.
    #[test]
    fn planar_twelve_bit_ybr_full_preserves_luminance() {
        let mut object =
            compressed_test_object(uids::EXPLICIT_VR_LITTLE_ENDIAN, VR::US, 1, 2, 16, vec![]);
        set_layout(&mut object, 3);
        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            "YBR_FULL",
        ));
        for (tag, value) in [
            (tags::BITS_STORED, 12_u16),
            (tags::HIGH_BIT, 11),
            (tags::PLANAR_CONFIGURATION, 1),
        ] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        let samples = [0xF123, 0x0ABC, 0x0800, 0x0800, 0x0800, 0x0800];
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            PrimitiveValue::U16(samples.to_vec().into()),
        ));
        let expected = native_bytes(&[0x0123, 0x0123, 0x0123, 0x0ABC, 0x0ABC, 0x0ABC], 16);
        assert_eq!(decoded_bytes(&object, 0), Ok(expected));
    }

    /// Deflate uses the stored size of the shared chroma pairs.
    #[test]
    fn deflated_ybr_full_422_decodes_the_packed_frame() {
        let mut object = compressed_test_object(
            uids::DEFLATED_IMAGE_FRAME_COMPRESSION,
            VR::US,
            1,
            2,
            8,
            raw_deflate(&[50, 200, 90, 170]),
        );
        set_layout(&mut object, 3);
        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            "YBR_FULL_422",
        ));
        let frame = decode_frame(&object, 0).unwrap();
        assert_eq!(
            rgb(&render_frame(&frame, None).unwrap().rgba),
            [[109, 33, 0], [255, 183, 133]]
        );
    }

    /// `samples` as frames of `rows` x `columns` x `samples_per_pixel`,
    /// compressed by the library's own writer for `transfer_syntax`.
    fn encoded_object(
        transfer_syntax: &str,
        (rows, columns, samples_per_pixel): (u16, u16, u16),
        bits_allocated: u16,
        samples: &[u16],
    ) -> DefaultDicomObject {
        use dicom_pixeldata::Transcode;

        let frames = samples.len()
            / (usize::from(rows) * usize::from(columns) * usize::from(samples_per_pixel));
        let mut object = compressed_test_object(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            VR::US,
            rows.into(),
            columns.into(),
            bits_allocated,
            vec![],
        );
        set_layout(&mut object, samples_per_pixel);
        object.put_element(DataElement::new(
            tags::NUMBER_OF_FRAMES,
            VR::IS,
            PrimitiveValue::from(frames.to_string()),
        ));
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            if bits_allocated == 16 { VR::OW } else { VR::OB },
            PrimitiveValue::from(native_bytes(samples, bits_allocated)),
        ));
        object
            .transcode(TransferSyntaxRegistry.get(transfer_syntax).unwrap())
            .unwrap();
        object
    }

    fn pixel_fragments(object: &DefaultDicomObject) -> (Vec<u32>, Vec<Vec<u8>>) {
        let value = object.element(tags::PIXEL_DATA).unwrap().value();
        (
            value.offset_table().unwrap().to_vec(),
            value.fragments().unwrap().to_vec(),
        )
    }

    fn with_fragments(
        object: &DefaultDicomObject,
        offset_table: Vec<u32>,
        fragments: Vec<Vec<u8>>,
    ) -> DefaultDicomObject {
        use dicom_core::value::PixelFragmentSequence;

        let mut object = object.clone();
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new(offset_table, fragments),
        ));
        object
    }

    /// Where the `marker` segment of a well-formed JPEG or JPEG-LS stream
    /// starts, at its 0xFF byte.
    fn segment_offset(stream: &[u8], marker: u8) -> usize {
        let mut position = 2;
        while stream[position + 1] != marker {
            assert_eq!(stream[position], 0xFF);
            position += 2 + usize::from(u16::from_be_bytes([
                stream[position + 2],
                stream[position + 3],
            ]));
        }
        position
    }

    /// Rewrites the frame header at `segment` (its 0xFF byte) to 65535 x 65535.
    fn enlarge_frame_header(stream: &mut [u8], segment: usize) {
        stream[segment + 5..segment + 9].copy_from_slice(&[0xFF; 4]);
    }

    /// The frame header check reads each frame's own codestream: in a
    /// two-frame image, a second frame claiming 65535 x 65535 is refused
    /// while the first still decodes, and a valid second frame decodes.
    #[test]
    fn each_frame_is_checked_against_its_own_frame_header() {
        let samples: Vec<u16> = (0..30).map(|index| (index * 37 + 11) % 256).collect();
        for (name, uid, marker, lossless) in [
            ("JPEG Baseline", uids::JPEG_BASELINE8_BIT, 0xC0, false),
            ("JPEG-LS", uids::JPEGLS_LOSSLESS, 0xF7, true),
        ] {
            let object = encoded_object(uid, (3, 5, 1), 8, &samples);
            let (offset_table, fragments) = pixel_fragments(&object);
            assert_eq!(fragments.len(), 2, "{name}");
            let frames: Vec<Vec<u8>> = (0..2)
                .map(|frame| decoded_bytes(&object, frame).unwrap())
                .collect();
            for (frame, decoded) in frames.iter().enumerate() {
                assert_eq!(decoded.len(), 15, "{name} frame {frame}");
                if lossless {
                    assert_eq!(
                        decoded[..],
                        native_bytes(&samples[frame * 15..][..15], 8)[..],
                        "{name} frame {frame}"
                    );
                }
            }

            let mut damaged = fragments.clone();
            let segment = segment_offset(&damaged[1], marker);
            enlarge_frame_header(&mut damaged[1], segment);
            let damaged = with_fragments(&object, offset_table, damaged);
            assert_eq!(decoded_bytes(&damaged, 0), Ok(frames[0].clone()), "{name}");
            let error = decoded_bytes(&damaged, 1).unwrap_err();
            assert!(
                error.contains("damaged: its frame header describes 65535 x 65535 pixels"),
                "{name}: {error}"
            );
        }
    }

    /// A frame split over several fragments is checked on the bytes the
    /// decoder reads: the fragments the offset table gives it, joined, even
    /// when the frame header starts in a later fragment.
    #[test]
    fn frame_header_in_a_later_fragment_of_its_frame_is_checked() {
        let samples: Vec<u16> = (0..30).map(|index| (index * 53 + 7) % 256).collect();
        for (name, uid, marker) in [
            ("JPEG Baseline", uids::JPEG_BASELINE8_BIT, 0xC0),
            ("JPEG-LS", uids::JPEGLS_LOSSLESS, 0xF7),
        ] {
            let object = encoded_object(uid, (3, 5, 1), 8, &samples);
            let (_, fragments) = pixel_fragments(&object);
            let frames: Vec<Vec<u8>> = (0..2)
                .map(|frame| decoded_bytes(&object, frame).unwrap())
                .collect();

            // Each frame in two fragments, the second starting at its frame header.
            let mut split = Vec::new();
            for fragment in &fragments {
                let segment = segment_offset(fragment, marker);
                split.push(fragment[..segment].to_vec());
                split.push(fragment[segment..].to_vec());
            }
            let second_frame = (split[0].len() + split[1].len() + 16) as u32;
            let object = with_fragments(&object, vec![0, second_frame], split.clone());
            for frame in 0..2 {
                assert_eq!(
                    decoded_bytes(&object, frame),
                    Ok(frames[frame as usize].clone()),
                    "{name} frame {frame}"
                );
            }

            let mut damaged = split.clone();
            enlarge_frame_header(&mut damaged[3], 0);
            let damaged = with_fragments(&object, vec![0, second_frame], damaged);
            assert_eq!(decoded_bytes(&damaged, 0), Ok(frames[0].clone()), "{name}");
            let error = decoded_bytes(&damaged, 1).unwrap_err();
            assert!(
                error.contains("damaged: its frame header describes 65535 x 65535 pixels"),
                "{name}: {error}"
            );

            // One frame in two fragments with an empty offset table.
            let mut single = encoded_object(uid, (3, 5, 1), 8, &samples[..15]);
            single.put_element(DataElement::new(
                tags::NUMBER_OF_FRAMES,
                VR::IS,
                PrimitiveValue::from("1"),
            ));
            let expected = decoded_bytes(&single, 0).unwrap();
            let (_, fragments) = pixel_fragments(&single);
            let segment = segment_offset(&fragments[0], marker);
            let mut halves = vec![
                fragments[0][..segment + 4].to_vec(),
                fragments[0][segment + 4..].to_vec(),
            ];
            let object = with_fragments(&single, vec![], halves.clone());
            assert_eq!(
                decoded_bytes(&object, 0),
                Ok(expected.clone()),
                "{name} single"
            );
            halves[1][1..5].copy_from_slice(&[0xFF; 4]);
            let object = with_fragments(&single, vec![], halves);
            let error = decoded_bytes(&object, 0).unwrap_err();
            assert!(
                error.contains("damaged: its frame header describes 65535 x 65535 pixels"),
                "{name}: {error}"
            );

            // The same frame split at its scan, so the first fragment holds
            // the whole frame header and the second the scan.
            let header_end = segment_offset(&fragments[0], 0xDA);
            let mut halves = vec![
                fragments[0][..header_end].to_vec(),
                fragments[0][header_end..].to_vec(),
            ];
            let object = with_fragments(&single, vec![], halves.clone());
            assert_eq!(decoded_bytes(&object, 0), Ok(expected), "{name} first");
            enlarge_frame_header(&mut halves[0], segment);
            let object = with_fragments(&single, vec![], halves);
            let error = decoded_bytes(&object, 0).unwrap_err();
            assert!(
                error.contains("damaged: its frame header describes 65535 x 65535 pixels"),
                "{name} first: {error}"
            );
        }
    }

    #[test]
    fn rejects_an_offset_inside_a_fragment_before_decoding() {
        let object = encoded_object(uids::JPEG_BASELINE8_BIT, (3, 5, 1), 8, &[0; 30]);
        let (_, mut fragments) = pixel_fragments(&object);
        let first_fragment_tail = fragments[0].split_off(2);
        fragments.insert(1, first_fragment_tail);
        // The second item header begins at 10. Offset 11 points inside it.
        let object = with_fragments(&object, vec![0, 11], fragments);
        assert!(decode_error(&object).contains("offset table does not match its fragments"));
    }

    /// Valid colour and 16-bit frames from the library's own encoders agree
    /// with their frame headers and keep decoding: JPEG Baseline RGB (which
    /// the writer stores as YBR_FULL_422 with subsampled chroma), JPEG-LS RGB
    /// and 16-bit JPEG-LS.
    #[test]
    fn valid_colour_and_sixteen_bit_frames_pass_the_frame_header_check() {
        let rgb: Vec<u16> = (0..45).map(|index| (index * 29 + 1) % 256).collect();
        let object = encoded_object(uids::JPEG_BASELINE8_BIT, (3, 5, 3), 8, &rgb);
        assert_eq!(decoded_bytes(&object, 0).map(|bytes| bytes.len()), Ok(45));

        let object = encoded_object(uids::JPEGLS_LOSSLESS, (3, 5, 3), 8, &rgb);
        assert_eq!(decoded_bytes(&object, 0), Ok(native_bytes(&rgb, 8)));

        let grey: Vec<u16> = (0..15).map(|index| index * 271 + 9).collect();
        let object = encoded_object(uids::JPEGLS_LOSSLESS, (3, 5, 1), 16, &grey);
        assert_eq!(decoded_bytes(&object, 0), Ok(native_bytes(&grey, 16)));
    }

    /// The JPEG header walk ends on every malformed segment instead of
    /// looping or reading past the stream, and finds the frame header past
    /// fill bytes and the DHT, JPG and DAC markers that share its range.
    #[test]
    fn jpeg_frame_header_walk_survives_malformed_segments() {
        let size = |stream: &[u8]| {
            jpeg_frame_size(stream).map(|size| (size.columns, size.rows, size.components))
        };
        let ls_size = |stream: &[u8]| {
            super::jpeg_ls_frame_size(stream).map(|size| (size.columns, size.rows, size.components))
        };
        let stream = |parts: &[&[u8]]| [&[0xFF, 0xD8][..], &parts.concat()].concat();
        // 8-bit, 2 rows of 7 samples, one component.
        let frame = [
            0xFF, 0xC1, 0x00, 0x0B, 0x08, 0x00, 0x02, 0x00, 0x07, 0x01, 0x01, 0x11, 0x00,
        ];
        let ls_frame = [
            0xFF, 0xF7, 0x00, 0x0B, 0x08, 0x00, 0x02, 0x00, 0x07, 0x01, 0x01, 0x11, 0x00,
        ];

        assert_eq!(size(&stream(&[&frame])), Some((7, 2, 1)));
        assert_eq!(ls_size(&stream(&[&ls_frame])), Some((7, 2, 1)));
        // Each format reads only its own frame header.
        assert_eq!(size(&stream(&[&ls_frame])), None);
        assert_eq!(ls_size(&stream(&[&frame])), None);
        // Fill bytes before the marker.
        assert_eq!(
            size(&stream(&[&[0xFF, 0xFF, 0xFF], &frame])),
            Some((7, 2, 1))
        );
        // DHT, JPG and DAC segments are not frame headers.
        for marker in [0xC4, 0xC8, 0xCC] {
            let other = [0xFF, marker, 0x00, 0x08, 0x08, 0x01, 0x00, 0x01, 0x00, 0x03];
            assert_eq!(
                size(&stream(&[&other, &frame])),
                Some((7, 2, 1)),
                "{marker:#X}"
            );
        }
        // Every SOF from 0xC0 to 0xCF other than those three is a frame header.
        for marker in (0xC0..=0xCF).filter(|marker| ![0xC4, 0xC8, 0xCC].contains(marker)) {
            let mut sof = frame;
            sof[1] = marker;
            assert_eq!(size(&stream(&[&sof])), Some((7, 2, 1)), "{marker:#X}");
        }
        // Segment lengths below two, or past the stream, end the walk.
        for length in [0_u8, 1] {
            assert_eq!(size(&stream(&[&[0xFF, 0xE0, 0x00, length], &frame])), None);
        }
        assert_eq!(size(&stream(&[&[0xFF, 0xE0, 0x00, 0x40, 0x00]])), None);
        assert_eq!(size(&stream(&[&[0xFF, 0xFE, 0x01, 0x00], &frame])), None);
        // A frame header too short for its fields, or a stream that stops
        // in a marker.
        assert_eq!(
            size(&stream(&[&[
                0xFF, 0xC0, 0x00, 0x07, 0x08, 0x00, 0x02, 0x00, 0x07
            ]])),
            None
        );
        assert_eq!(size(&stream(&[&[0xFF]])), None);
        assert_eq!(size(&stream(&[&[0xFF, 0xFF]])), None);
        assert_eq!(size(&stream(&[&[0xFF, 0xC0, 0x00]])), None);
        assert_eq!(size(&[]), None);
    }

    /// A JP2 file's codestream box is found however its length is written:
    /// as a 64-bit length, or as 0 for "to the end of the file", behind
    /// boxes with either form. A box length that cannot be valid ends the
    /// walk instead of looping.
    #[test]
    fn jp2_box_lengths_in_every_form_are_followed() {
        let size = |stream: &[u8]| {
            super::jpeg2000_frame_size(stream)
                .map(|size| (size.columns, size.rows, size.components))
        };
        let plain = jp2_file(&JPEG2000_5_BY_3);
        // The jp2c box header starts 8 bytes before the codestream.
        let jp2c = plain.len() - JPEG2000_5_BY_3.len() - 8;
        let with_jp2c = |header: &[u8]| [&plain[..jp2c], header, &JPEG2000_5_BY_3].concat();
        let extended_length = (JPEG2000_5_BY_3.len() as u64 + 16).to_be_bytes();

        let extended = with_jp2c(&[&[0, 0, 0, 1][..], b"jp2c", &extended_length].concat());
        let to_the_end = with_jp2c(&[&[0, 0, 0, 0][..], b"jp2c"].concat());
        // A free box with a 64-bit length before the codestream box.
        let free = [
            &plain[..jp2c],
            &[0, 0, 0, 1],
            b"free",
            &20_u64.to_be_bytes(),
            &[0xAA; 4],
            &plain[jp2c..],
        ]
        .concat();
        // An empty free box, only its eight-byte header.
        let empty = [&plain[..jp2c], &[0, 0, 0, 8], b"free", &plain[jp2c..]].concat();
        for (name, stream) in [
            ("plain", &plain),
            ("64-bit length", &extended),
            ("to the end", &to_the_end),
            ("free box", &free),
            ("empty box", &empty),
        ] {
            assert_eq!(size(stream), Some((5, 3, 1)), "{name}");
            let object =
                compressed_test_object(uids::JPEG2000_LOSSLESS, VR::US, 3, 5, 8, stream.clone());
            assert_eq!(
                decoded_bytes(&object, 0).map(|bytes| bytes.len()),
                Ok(15),
                "{name}"
            );
        }

        for (name, header) in [
            (
                "length below the header",
                [&[0, 0, 0, 4][..], b"free"].concat(),
            ),
            (
                "to the end before the codestream",
                [&[0, 0, 0, 0][..], b"free"].concat(),
            ),
            (
                "64-bit length below the header",
                [&[0, 0, 0, 1][..], b"free", &8_u64.to_be_bytes()].concat(),
            ),
            (
                "64-bit length past the file",
                [&[0, 0, 0, 1][..], b"free", &u64::MAX.to_be_bytes()].concat(),
            ),
        ] {
            let stream = [&plain[..jp2c], &header, &plain[jp2c..]].concat();
            assert_eq!(size(&stream), None, "{name}");
        }
        // A box too short for its own header ends the walk even when a valid
        // codestream box follows its length field, as openjpeg refuses it.
        let stream = [&plain[..jp2c], &[0, 0, 0, 4], &plain[jp2c..]].concat();
        assert_eq!(size(&stream), None);
        // A codestream box cut short in its SIZ marker.
        assert_eq!(size(&plain[..jp2c + 8 + 40]), None);
        assert_eq!(size(&JPEG2000_5_BY_3[..41]), None);
        // An image offset past the image extent.
        let mut inverted = JPEG2000_5_BY_3;
        inverted[16..20].copy_from_slice(&6_u32.to_be_bytes());
        assert_eq!(size(&inverted), None);
    }

    /// 8-bit grayscale RLE goes through the app's own unpacker because the
    /// library decodes it one sample late. A header that lists an unused
    /// extra segment after the needed one, empty or not, decodes the same
    /// samples in place, not the library's shifted ones.
    #[test]
    fn eight_bit_rle_with_an_extra_segment_decodes_every_sample_in_place() {
        let samples: Vec<u16> = vec![10, 20, 30, 40];
        let segments = rle_segments(&samples, 1, 1, packbits_literals);
        for extra in [vec![], vec![0x00, 0x00]] {
            let mut fragment = rle_frame_fragment(&segments);
            let end = fragment.len() as u32;
            fragment.extend_from_slice(&extra);
            fragment[..4].copy_from_slice(&2_u32.to_le_bytes());
            fragment[8..12].copy_from_slice(&end.to_le_bytes());
            let object = compressed_test_object(uids::RLE_LOSSLESS, VR::US, 2, 2, 8, fragment);

            assert_eq!(
                decoded_bytes(&object, 0),
                Ok(native_bytes(&samples, 8)),
                "extra segment of {} bytes",
                extra.len()
            );
        }
    }

    /// A JPEG Lossless frame of 2 x 2 12-bit samples whose header defines a
    /// restart interval of `interval` and has no Huffman tables, so the
    /// decoder, when it gets that far, fails without allocating.
    fn jpeg_lossless_stream(interval: u8) -> Vec<u8> {
        [
            &[0xFF, 0xD8][..],
            &[
                0xFF, 0xC3, 0x00, 0x0B, 0x0C, 0x00, 0x02, 0x00, 0x02, 0x01, 0x01, 0x11, 0x00,
            ],
            &[0xFF, 0xDD, 0x00, 0x04, 0x00, interval],
            &[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00],
            &[0xFF, 0xD9],
        ]
        .concat()
    }

    /// A restart interval of 0 means none, so such a JPEG Lossless frame
    /// reaches the decoder instead of being refused for restart intervals.
    #[test]
    fn jpeg_lossless_with_a_zero_restart_interval_reaches_the_decoder() {
        let object = compressed_test_object(
            uids::JPEG_LOSSLESS_SV1,
            VR::US,
            2,
            2,
            16,
            jpeg_lossless_stream(1),
        );
        let error = decoded_bytes(&object, 0).unwrap_err();
        assert!(error.contains("restart intervals"), "{error}");

        let object = compressed_test_object(
            uids::JPEG_LOSSLESS_SV1,
            VR::US,
            2,
            2,
            16,
            jpeg_lossless_stream(0),
        );
        let error = decoded_bytes(&object, 0).unwrap_err();
        assert!(!error.contains("restart intervals"), "{error}");
        assert!(
            error.contains("could not decode DICOM pixel data"),
            "{error}"
        );
    }

    /// The restart-interval refusal reads the frame the decoder reads: its
    /// fragments joined. A DRI segment in a later fragment of the frame than
    /// its SOI is refused like one in the first, rather than reaching
    /// jpeg-decoder, which decodes restart intervals wrongly.
    #[test]
    fn jpeg_lossless_restart_interval_in_a_later_fragment_is_refused() {
        let stream = jpeg_lossless_stream(1);
        // SOI and the frame header in the first fragment, the DRI and the
        // scan in the second.
        let object = compressed_test_object(uids::JPEG_LOSSLESS_SV1, VR::US, 2, 2, 16, vec![]);
        let object = with_fragments(
            &object,
            vec![],
            vec![stream[..16].to_vec(), stream[16..].to_vec()],
        );

        let error = decoded_bytes(&object, 0).unwrap_err();
        assert!(error.contains("restart intervals"), "{error}");
    }

    /// One dimension of 0 in a frame header never agrees, even with a 0 in
    /// the attributes: JPEG-LS leaves either dimension to a preset segment.
    /// Images one row or one column wide still decode.
    #[test]
    fn frame_header_with_one_zero_dimension_is_refused() {
        for (rows, columns) in [(0_u8, 2_u8), (2, 0)] {
            let stream = [
                &[0xFF, 0xD8][..],
                &[
                    0xFF, 0xF7, 0x00, 0x0B, 0x08, 0x00, rows, 0x00, columns, 0x01, 0x01, 0x11, 0x00,
                ],
                &[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00],
                &[0xFF, 0xD9],
            ]
            .concat();
            let object = compressed_test_object(
                uids::JPEGLS_LOSSLESS,
                VR::US,
                rows.into(),
                columns.into(),
                8,
                stream,
            );
            let error = decoded_bytes(&object, 0).unwrap_err();
            assert!(
                error.contains(&format!("describes {columns} x {rows} pixels")),
                "{error}"
            );
        }

        let samples: Vec<u16> = (0..5).map(|index| index * 50 + 1).collect();
        for layout in [(1, 5, 1), (5, 1, 1)] {
            let object = encoded_object(uids::JPEGLS_LOSSLESS, layout, 8, &samples);
            assert_eq!(
                decoded_bytes(&object, 0),
                Ok(native_bytes(&samples, 8)),
                "{layout:?}"
            );
        }
    }

    /// Every transfer syntax whose library decoder sizes its output from the
    /// codestream has that size checked against Rows, Columns and
    /// SamplesPerPixel before its decoder runs. Behind 3 x 5 attributes, a
    /// codestream that claims 65535 x 65535 (JPEG, JPEG-LS) or 40000 x 40000
    /// (JPEG 2000, HTJ2K), or the transposed 3 x 5, is refused as damaged;
    /// one that agrees passes the check.
    #[test]
    fn every_codestream_transfer_syntax_checks_the_frame_size() {
        // A JPEG or JPEG-LS stream with this frame marker, or a JPEG 2000
        // codestream for none, of `rows` x `columns` one-sample pixels.
        let stream = |marker: Option<u8>, rows: u16, columns: u16| match marker {
            Some(marker) => [
                &[0xFF, 0xD8, 0xFF, marker, 0x00, 0x0B, 0x08][..],
                &rows.to_be_bytes(),
                &columns.to_be_bytes(),
                &[0x01, 0x01, 0x11, 0x00, 0xFF, 0xD9],
            ]
            .concat(),
            None => {
                let mut stream = JPEG2000_5_BY_3;
                for (offset, value) in [(8, columns), (12, rows), (24, columns), (28, rows)] {
                    stream[offset..offset + 4].copy_from_slice(&u32::from(value).to_be_bytes());
                }
                stream.to_vec()
            }
        };
        for (uid, marker, huge) in [
            (uids::JPEG_BASELINE8_BIT, Some(0xC0), 65535),
            (uids::JPEG_LOSSLESS, Some(0xC3), 65535),
            (uids::JPEG_LOSSLESS_SV1, Some(0xC3), 65535),
            (uids::JPEGLS_LOSSLESS, Some(0xF7), 65535),
            (uids::JPEGLS_NEAR_LOSSLESS, Some(0xF7), 65535),
            (uids::JPEG2000_LOSSLESS, None, 40000),
            (uids::JPEG2000, None, 40000),
            (uids::JPEG2000MC_LOSSLESS, None, 40000),
            (uids::JPEG2000MC, None, 40000),
            (uids::HTJ2K_LOSSLESS, None, 40000),
            (uids::HTJ2K_LOSSLESS_RPCL, None, 40000),
            (uids::HTJ2K, None, 40000),
        ] {
            for (rows, columns) in [(huge, huge), (5, 3)] {
                let object =
                    compressed_test_object(uid, VR::US, 3, 5, 8, stream(marker, rows, columns));
                let error = decoded_bytes(&object, 0).unwrap_err();
                assert!(
                    error.contains(&format!(
                        "damaged: its frame header describes {columns} x {rows} pixels \
                         of 1 samples, where the attributes describe 5 x 3 pixels of 1"
                    )),
                    "{uid}: {error}"
                );
            }

            let object = compressed_test_object(uid, VR::US, 3, 5, 8, stream(marker, 3, 5));
            if let Err(error) = decoded_bytes(&object, 0) {
                assert!(!error.contains("frame header"), "{uid}: {error}");
            }
        }
    }

    /// Two values one unit apart are not flat: the lower one is drawn black,
    /// the higher white, and the read-out names that one-unit window.
    #[test]
    fn two_adjacent_values_without_window_split_black_and_white() {
        let object = sixteen_bit_image(
            "MONOCHROME2",
            16,
            1,
            [-1000_i16, -1000, -999, -999].map(|value| value as u16),
        );
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(frame.default_center_width(), (-999.5, 1.0));
        assert_eq!(
            greys(&render_frame(&frame, None).unwrap().rgba),
            [0, 0, 255, 255]
        );
        assert_eq!(thumbnail_greys(&object, "two-values"), [0, 0, 255, 255]);
    }

    /// A flat MONOCHROME1 frame keeps the decoder's rendering too, which is
    /// the inverse of the same MONOCHROME2 frame.
    #[test]
    fn flat_monochrome1_frame_without_window_is_the_inverse_of_monochrome2() {
        for (bits_stored, representation, sample, monochrome2_grey) in
            [(16, 1, -1000_i16 as u16, 0_u8), (12, 0, 500, 255)]
        {
            let object = sixteen_bit_image("MONOCHROME1", bits_stored, representation, [sample; 4]);
            let frame = decode_frame(&object, 0).unwrap();
            let monochrome2 = decode_frame(
                &sixteen_bit_image("MONOCHROME2", bits_stored, representation, [sample; 4]),
                0,
            )
            .unwrap();

            assert_eq!(
                frame.default_center_width(),
                monochrome2.default_center_width(),
                "{sample}"
            );
            assert_eq!(
                greys(&render_frame(&frame, None).unwrap().rgba),
                [255 - monochrome2_grey; 4],
                "{sample}"
            );
            assert_eq!(
                thumbnail_greys(&object, "flat-monochrome1"),
                [255 - monochrome2_grey; 4],
                "{sample}"
            );
        }
    }

    /// In a multi-frame file only the flat frame falls back to the decoder;
    /// the other frame still spans its own values.
    #[test]
    fn multi_frame_with_one_flat_frame_falls_back_only_for_that_frame() {
        let mut object = sixteen_bit_image("MONOCHROME2", 16, 1, [0; 4]);
        object.put_element(DataElement::new(
            tags::NUMBER_OF_FRAMES,
            VR::IS,
            PrimitiveValue::from("2"),
        ));
        let samples: Vec<u16> = [-1000_i16, 0, 500, 1000, -1000, -1000, -1000, -1000]
            .map(|value| value as u16)
            .to_vec();
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            PrimitiveValue::U16(samples.into()),
        ));
        let first = decode_frame(&object, 0).unwrap();
        let flat = decode_frame(&object, 1).unwrap();

        assert_eq!(first.default_center_width(), (0.0, 2000.0));
        assert_eq!(
            greys(&render_frame(&first, None).unwrap().rgba),
            [0, 128, 192, 255]
        );
        assert_eq!(flat.default_center_width(), (-0.5, 65535.0));
        assert_eq!(greys(&render_frame(&flat, None).unwrap().rgba), [0; 4]);
    }

    /// A range narrower than one rescaled unit is not flat, and its window,
    /// in the read-out and the Full Dynamic preset alike, is one unit wide,
    /// the narrowest window the viewer draws.
    #[test]
    fn sub_unit_range_window_is_one_unit_wide() {
        let mut object = sixteen_bit_image("MONOCHROME2", 12, 0, [0, 1, 2, 3]);
        object.put_element(DataElement::new(tags::RESCALE_SLOPE, VR::DS, "0.25"));
        object.put_element(DataElement::new(tags::RESCALE_INTERCEPT, VR::DS, "0"));
        let frame = decode_frame(&object, 0).unwrap();

        assert_eq!(frame.default_center_width(), (0.375, 1.0));
        let full_dynamic = frame.full_dynamic_window().unwrap();
        assert_eq!((full_dynamic.center, full_dynamic.width), (0.375, 1.0));
    }
}
