//! JPEG Extended decoding with the JPEG stream's original 8- or 12-bit precision.

use std::borrow::Cow;
use std::ffi::{CStr, c_void};
use std::ptr::NonNull;

use anyhow::{Context, Result, ensure};
use dicom_core::{DataElement, Length, PrimitiveValue, VR, value::Value};
use dicom_dictionary_std::{tags, uids};
use dicom_object::DefaultDicomObject;
use dicom_pixeldata::{DecodedPixelData, PixelDecoder};
use turbojpeg_sys as jpeg;

// The crate's pregenerated bindings define size_t as c_ulong, which differs from
// size_t on 64-bit Windows. These three size-taking calls use usize on every target.
mod ffi {
    use std::ffi::{c_int, c_short, c_uchar, c_void};

    unsafe extern "C" {
        pub(super) fn tj3DecompressHeader(
            handle: *mut c_void,
            data: *const c_uchar,
            size: usize,
        ) -> c_int;
        pub(super) fn tj3Decompress8(
            handle: *mut c_void,
            data: *const c_uchar,
            size: usize,
            pixels: *mut c_uchar,
            pitch: c_int,
            format: c_int,
        ) -> c_int;
        pub(super) fn tj3Decompress12(
            handle: *mut c_void,
            data: *const c_uchar,
            size: usize,
            pixels: *mut c_short,
            pitch: c_int,
            format: c_int,
        ) -> c_int;
    }
}

pub(super) const TRANSFER_SYNTAX: &str = uids::JPEG_EXTENDED12_BIT;

pub(super) fn decode_frame(
    object: &DefaultDicomObject,
    frame: u32,
) -> Result<DecodedPixelData<'static>> {
    let frames = object
        .element_opt(tags::NUMBER_OF_FRAMES)?
        .map(|element| element.to_int::<u32>())
        .transpose()?
        .unwrap_or(1);
    ensure!(frame < frames, "JPEG frame index is out of range");
    let pixels = object.element(tags::PIXEL_DATA)?.value();
    let fragments = pixels
        .fragments()
        .context("JPEG pixel data is not encapsulated")?;
    let offsets = pixels.offset_table().context("Missing JPEG offset table")?;
    let encoded = frame_bytes(fragments, offsets, frames, frame)?;

    let width = object.element(tags::COLUMNS)?.to_int::<u16>()?;
    let height = object.element(tags::ROWS)?.to_int::<u16>()?;
    let samples = object.element(tags::SAMPLES_PER_PIXEL)?.to_int::<u16>()?;
    let allocated = object.element(tags::BITS_ALLOCATED)?.to_int::<u16>()?;
    let stored = object.element(tags::BITS_STORED)?.to_int::<u16>()?;
    let decoder = Decoder::new()?;
    let decoded = decoder.decode(&encoded, width, height, samples, allocated, stored)?;

    // Feed the decoded samples back through dicom-pixeldata's normal presentation handling.
    // Only this temporary object changes; the source file and displayed metadata keep their
    // original transfer syntax. Retain the selected frame's functional groups for its rescale/VOI.
    let mut native = object.clone();
    native.meta_mut().transfer_syntax = uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned();
    native.put_element(DataElement::new(tags::PIXEL_DATA, VR::OW, decoded));
    native.put_element(DataElement::new(tags::NUMBER_OF_FRAMES, VR::IS, "1"));
    if let Some(groups) = object.element_opt(tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE)? {
        let selected = groups
            .value()
            .items()
            .and_then(|items| items.get(frame as usize))
            .context("Missing functional groups for JPEG frame")?
            .clone();
        native.put_element(DataElement::new(
            tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
            VR::SQ,
            Value::new_sequence(vec![selected], Length::UNDEFINED),
        ));
    }
    if samples == 3 {
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
    }
    Ok(native.decode_pixel_data_frame(0)?.to_owned())
}

fn frame_bytes<'a>(
    fragments: &'a [Vec<u8>],
    offsets: &[u32],
    frames: u32,
    frame: u32,
) -> Result<Cow<'a, [u8]>> {
    ensure!(
        frame < frames && !fragments.is_empty(),
        "Missing JPEG frame"
    );
    if offsets.is_empty() && fragments.len() == frames as usize {
        return Ok(Cow::Borrowed(&fragments[frame as usize]));
    }
    if offsets.is_empty() {
        ensure!(
            frames == 1,
            "Fragmented multi-frame JPEG requires frame offsets"
        );
        return Ok(Cow::Owned(fragments.concat()));
    }

    ensure!(
        offsets.len() == frames as usize && offsets[0] == 0,
        "Invalid JPEG frame offset table"
    );
    ensure!(
        offsets.windows(2).all(|pair| pair[0] < pair[1]),
        "JPEG frame offsets are not ordered"
    );
    // DICOM offsets include each fragment's eight-byte item header, not just its JPEG bytes.
    let mut boundaries = Vec::with_capacity(fragments.len() + 1);
    boundaries.push(0_u64);
    for fragment in fragments {
        boundaries.push(boundaries.last().unwrap() + fragment.len() as u64 + 8);
    }
    let start = boundaries
        .binary_search(&u64::from(offsets[frame as usize]))
        .ok()
        .filter(|index| *index < fragments.len())
        .context("JPEG frame offset does not point to a fragment")?;
    let end = if frame + 1 < frames {
        boundaries
            .binary_search(&u64::from(offsets[frame as usize + 1]))
            .ok()
            .context("JPEG frame end does not point to a fragment")?
    } else {
        fragments.len()
    };
    ensure!(
        end > start && end <= fragments.len(),
        "Invalid JPEG frame fragment range"
    );
    if end == start + 1 {
        Ok(Cow::Borrowed(&fragments[start]))
    } else {
        Ok(Cow::Owned(fragments[start..end].concat()))
    }
}

struct Decoder(NonNull<c_void>);

impl Decoder {
    fn new() -> Result<Self> {
        // SAFETY: creates a fresh decompressor owned by this wrapper and released in Drop.
        let handle = unsafe { jpeg::tj3Init(jpeg::TJINIT_TJINIT_DECOMPRESS as i32) };
        let decoder = Self(NonNull::new(handle).context("Could not create JPEG decoder")?);
        // Reject incomplete/corrupt streams instead of accepting libjpeg's warning recovery.
        // SAFETY: the handle is live and this is a documented integer parameter.
        decoder.check(unsafe {
            jpeg::tj3Set(
                decoder.0.as_ptr(),
                jpeg::TJPARAM_TJPARAM_STOPONWARNING as i32,
                1,
            )
        })?;
        Ok(decoder)
    }

    fn check(&self, status: i32) -> Result<()> {
        if status != 0 {
            // SAFETY: the error string is owned by the live handle and is NUL-terminated.
            let message = unsafe { CStr::from_ptr(jpeg::tj3GetErrorStr(self.0.as_ptr())) };
            anyhow::bail!(
                "JPEG Extended decoding failed: {}",
                message.to_string_lossy()
            );
        }
        Ok(())
    }

    fn parameter(&self, parameter: jpeg::TJPARAM) -> i32 {
        // SAFETY: only documented, read-only header parameters are queried on a live handle.
        unsafe { jpeg::tj3Get(self.0.as_ptr(), parameter as i32) }
    }

    fn decode(
        &self,
        encoded: &[u8],
        width: u16,
        height: u16,
        samples: u16,
        allocated: u16,
        stored: u16,
    ) -> Result<PrimitiveValue> {
        // SAFETY: encoded remains live and its exact byte length is passed to the parser.
        self.check(unsafe {
            ffi::tj3DecompressHeader(self.0.as_ptr(), encoded.as_ptr(), encoded.len())
        })?;
        ensure!(
            width > 0
                && height > 0
                && self.parameter(jpeg::TJPARAM_TJPARAM_JPEGWIDTH) == i32::from(width)
                && self.parameter(jpeg::TJPARAM_TJPARAM_JPEGHEIGHT) == i32::from(height),
            "JPEG dimensions disagree with DICOM Rows/Columns"
        );
        let precision = self.parameter(jpeg::TJPARAM_TJPARAM_PRECISION);
        ensure!(
            precision == 8 || precision == 12,
            "Unsupported JPEG Extended precision: {precision}"
        );
        ensure!(
            (allocated == 8 || allocated == 16)
                && stored == precision as u16
                && stored <= allocated,
            "JPEG precision disagrees with DICOM BitsStored/BitsAllocated"
        );
        let colorspace = self.parameter(jpeg::TJPARAM_TJPARAM_COLORSPACE);
        let format = match samples {
            1 if colorspace == jpeg::TJCS_TJCS_GRAY as i32 => jpeg::TJPF_TJPF_GRAY,
            3 if colorspace == jpeg::TJCS_TJCS_RGB as i32
                || colorspace == jpeg::TJCS_TJCS_YCbCr as i32 =>
            {
                jpeg::TJPF_TJPF_RGB
            }
            _ => anyhow::bail!("JPEG components disagree with DICOM SamplesPerPixel"),
        };
        let count = usize::from(width)
            .checked_mul(usize::from(height))
            .and_then(|pixels| pixels.checked_mul(usize::from(samples)))
            .context("JPEG output size overflow")?;
        if precision == 8 {
            let mut pixels = Vec::<u8>::new();
            pixels.try_reserve_exact(count)?;
            pixels.resize(count, 0);
            // SAFETY: validated dimensions/components require exactly count u8 samples.
            // Pitch 0 means tightly packed rows; no scaling or cropping is configured.
            self.check(unsafe {
                ffi::tj3Decompress8(
                    self.0.as_ptr(),
                    encoded.as_ptr(),
                    encoded.len(),
                    pixels.as_mut_ptr(),
                    0,
                    format as i32,
                )
            })?;
            Ok(if allocated == 8 {
                PrimitiveValue::from(pixels)
            } else {
                PrimitiveValue::U16(pixels.into_iter().map(u16::from).collect())
            })
        } else {
            let mut pixels = Vec::<u16>::new();
            pixels.try_reserve_exact(count)?;
            pixels.resize(count, 0);
            // SAFETY: validated dimensions/components require count 16-bit samples.
            // u16 and C short have identical size/alignment; 12-bit values fit both.
            self.check(unsafe {
                ffi::tj3Decompress12(
                    self.0.as_ptr(),
                    encoded.as_ptr(),
                    encoded.len(),
                    pixels.as_mut_ptr().cast(),
                    0,
                    format as i32,
                )
            })?;
            Ok(PrimitiveValue::U16(pixels.into()))
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the handle and destroys it exactly once.
        unsafe { jpeg::tj3Destroy(self.0.as_ptr()) };
    }
}
