//! Extraction and painting of DICOM overlay planes (groups 6000-601E).
//!
//! Overlay planes are 1-bit bitmaps stored beside the pixel data. Some producers put the
//! entire visible content there (for example Siemens dose report pages, whose pixel data is
//! all zero), so a viewer that ignores them shows a black image.

use dicom_core::{PrimitiveValue, Tag};
use dicom_object::DefaultDicomObject;
use dicom_object::mem::InMemElement;

const FIRST_OVERLAY_GROUP: u16 = 0x6000;
const LAST_OVERLAY_GROUP: u16 = 0x601E;
const OVERLAY_ROWS: u16 = 0x0010;
const OVERLAY_COLUMNS: u16 = 0x0011;
const NUMBER_OF_FRAMES_IN_OVERLAY: u16 = 0x0015;
const OVERLAY_ORIGIN: u16 = 0x0050;
const IMAGE_FRAME_ORIGIN: u16 = 0x0051;
const OVERLAY_DATA: u16 = 0x3000;
const OVERLAY_COLOR: [u8; 4] = [255, 255, 255, 255];

/// One overlay plane already reduced to the single frame it contributes to an image frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OverlayBitmap {
    rows: usize,
    columns: usize,
    // DICOM Overlay Origin is 1-based; (1, 1) aligns the bitmap with the image's first pixel.
    origin_row: i64,
    origin_column: i64,
    // LSB-first packed bits, `rows * columns` of them.
    bits: Vec<u8>,
}

impl OverlayBitmap {
    fn is_set(&self, row: usize, column: usize) -> bool {
        bit_at(&self.bits, row * self.columns + column)
    }

    fn paint_onto(&self, image: &mut image::RgbaImage) {
        let width = i64::from(image.width());
        let height = i64::from(image.height());

        for row in 0..self.rows {
            let image_row = self.origin_row - 1 + row as i64;
            if image_row < 0 || image_row >= height {
                continue;
            }
            for column in 0..self.columns {
                let image_column = self.origin_column - 1 + column as i64;
                if image_column < 0 || image_column >= width || !self.is_set(row, column) {
                    continue;
                }
                image.put_pixel(
                    image_column as u32,
                    image_row as u32,
                    image::Rgba(OVERLAY_COLOR),
                );
            }
        }
    }
}

pub(super) fn paint_overlay_bitmaps(bitmaps: &[OverlayBitmap], image: &mut image::RgbaImage) {
    for bitmap in bitmaps {
        bitmap.paint_onto(image);
    }
}

/// Collects the overlay bitmaps that apply to the zero-based `frame_index` of the image.
pub(super) fn read_overlay_bitmaps(
    dicom_object: &DefaultDicomObject,
    frame_index: u32,
) -> Vec<OverlayBitmap> {
    (FIRST_OVERLAY_GROUP..=LAST_OVERLAY_GROUP)
        .step_by(2)
        .filter_map(|group| read_overlay_bitmap(dicom_object, group, frame_index))
        .collect()
}

fn read_overlay_bitmap(
    dicom_object: &DefaultDicomObject,
    group: u16,
    frame_index: u32,
) -> Option<OverlayBitmap> {
    let element = |element: u16| dicom_object.get(Tag(group, element));
    let data = packed_bytes(element(OVERLAY_DATA)?);
    let rows = element(OVERLAY_ROWS)?.to_int::<usize>().ok()?;
    let columns = element(OVERLAY_COLUMNS)?.to_int::<usize>().ok()?;
    if rows == 0 || columns == 0 || data.is_empty() {
        return None;
    }

    let overlay_frame_count = element(NUMBER_OF_FRAMES_IN_OVERLAY)
        .and_then(|element| element.to_int::<u64>().ok())
        .unwrap_or(1)
        .max(1);
    let image_frame_origin = element(IMAGE_FRAME_ORIGIN)
        .and_then(|element| element.to_int::<u64>().ok())
        .unwrap_or(1);
    // Image Frame Origin is the 1-based image frame shown together with the first overlay frame.
    let overlay_frame = (u64::from(frame_index) + 1).checked_sub(image_frame_origin)?;
    if overlay_frame >= overlay_frame_count {
        return None;
    }

    let (origin_row, origin_column) = element(OVERLAY_ORIGIN)
        .and_then(|element| element.to_multi_int::<i64>().ok())
        .and_then(|origin| match origin.as_slice() {
            [row, column, ..] => Some((*row, *column)),
            _ => None,
        })
        .unwrap_or((1, 1));

    let bits_per_frame = rows.checked_mul(columns)?;
    let first_bit = usize::try_from(overlay_frame)
        .ok()?
        .checked_mul(bits_per_frame)?;

    Some(OverlayBitmap {
        rows,
        columns,
        origin_row,
        origin_column,
        bits: extract_bits(&data, first_bit, bits_per_frame),
    })
}

/// Overlay Data is a bit stream packed LSB-first; when it was parsed as 16-bit words
/// (VR OW) the words have already been converted to the host byte order, so they are
/// serialised back to little-endian to restore the standard bit order.
fn packed_bytes(element: &InMemElement) -> Vec<u8> {
    match element.value().primitive() {
        Some(PrimitiveValue::U16(words)) => {
            words.iter().flat_map(|word| word.to_le_bytes()).collect()
        }
        Some(PrimitiveValue::U8(bytes)) => bytes.to_vec(),
        Some(other) => other.to_bytes().into_owned(),
        None => Vec::new(),
    }
}

/// Re-packs `count` bits starting at `first_bit` so the result starts on a byte boundary.
fn extract_bits(source: &[u8], first_bit: usize, count: usize) -> Vec<u8> {
    let mut packed = vec![0_u8; count.div_ceil(8)];
    for offset in 0..count {
        if bit_at(source, first_bit + offset) {
            packed[offset / 8] |= 1 << (offset % 8);
        }
    }
    packed
}

fn bit_at(bits: &[u8], index: usize) -> bool {
    bits.get(index / 8)
        .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

#[cfg(test)]
mod tests {
    use dicom_core::{DataElement, PrimitiveValue, Tag, VR, dicom_value};
    use dicom_dictionary_std::uids;
    use dicom_object::{DefaultDicomObject, FileDicomObject, FileMetaTableBuilder};

    use super::{OVERLAY_COLOR, OverlayBitmap, paint_overlay_bitmaps, read_overlay_bitmaps};

    const BLACK: [u8; 4] = [0, 0, 0, 255];

    fn overlay_test_object() -> DefaultDicomObject {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(uids::SECONDARY_CAPTURE_IMAGE_STORAGE)
            .media_storage_sop_instance_uid("2.25.301")
            .build()
            .unwrap();
        FileDicomObject::new_empty_with_meta(meta)
    }

    fn put_overlay(
        object: &mut DefaultDicomObject,
        group: u16,
        rows: u16,
        columns: u16,
        data: Vec<u8>,
    ) {
        object.put_element(DataElement::new(
            Tag(group, 0x0010),
            VR::US,
            PrimitiveValue::from(rows),
        ));
        object.put_element(DataElement::new(
            Tag(group, 0x0011),
            VR::US,
            PrimitiveValue::from(columns),
        ));
        object.put_element(DataElement::new(
            Tag(group, 0x3000),
            VR::OB,
            PrimitiveValue::from(data),
        ));
    }

    fn set_pixels(image: &image::RgbaImage) -> Vec<(u32, u32)> {
        image
            .enumerate_pixels()
            .filter(|(_, _, pixel)| pixel.0 != BLACK)
            .inspect(|(_, _, pixel)| assert_eq!(pixel.0, OVERLAY_COLOR))
            .map(|(x, y, _)| (x, y))
            .collect()
    }

    fn black_image(width: u32, height: u32) -> image::RgbaImage {
        image::RgbaImage::from_pixel(width, height, image::Rgba(BLACK))
    }

    #[test]
    fn bits_are_unpacked_lsb_first_and_painted_at_the_overlay_origin() {
        let mut object = overlay_test_object();
        // 2 rows x 4 columns: bit 0 -> (row 0, column 0), bit 7 -> (row 1, column 3).
        put_overlay(&mut object, 0x6000, 2, 4, vec![0b1000_0001]);
        object.put_element(DataElement::new(
            Tag(0x6000, 0x0050),
            VR::SS,
            dicom_value!(I16, [2, 3]),
        ));

        let bitmaps = read_overlay_bitmaps(&object, 0);
        let mut image = black_image(8, 4);
        paint_overlay_bitmaps(&bitmaps, &mut image);

        assert_eq!(bitmaps.len(), 1);
        assert_eq!(set_pixels(&image), vec![(2, 1), (5, 2)]);
    }

    #[test]
    fn overlay_words_keep_the_little_endian_bit_order() {
        let mut object = overlay_test_object();
        put_overlay(&mut object, 0x6000, 1, 16, Vec::new());
        object.put_element(DataElement::new(
            Tag(0x6000, 0x3000),
            VR::OW,
            dicom_value!(U16, [0x8001]),
        ));

        let bitmaps = read_overlay_bitmaps(&object, 0);
        let mut image = black_image(16, 1);
        paint_overlay_bitmaps(&bitmaps, &mut image);

        assert_eq!(set_pixels(&image), vec![(0, 0), (15, 0)]);
    }

    #[test]
    fn overlay_frames_are_matched_through_the_image_frame_origin() {
        let mut object = overlay_test_object();
        // Two 1x3 frames packed back to back: frame 0 = 1,1,0 and frame 1 = 1,0,1.
        put_overlay(&mut object, 0x6000, 1, 3, vec![0b0010_1011]);
        object.put_element(DataElement::new(
            Tag(0x6000, 0x0015),
            VR::IS,
            PrimitiveValue::from("2"),
        ));
        object.put_element(DataElement::new(
            Tag(0x6000, 0x0051),
            VR::US,
            PrimitiveValue::from(2_u16),
        ));

        let painted_columns = |frame_index: u32| {
            let bitmaps = read_overlay_bitmaps(&object, frame_index);
            let mut image = black_image(3, 1);
            paint_overlay_bitmaps(&bitmaps, &mut image);
            set_pixels(&image)
                .into_iter()
                .map(|(x, _)| x)
                .collect::<Vec<_>>()
        };

        assert!(painted_columns(0).is_empty());
        assert_eq!(painted_columns(1), vec![0, 1]);
        assert_eq!(painted_columns(2), vec![0, 2]);
        assert!(painted_columns(3).is_empty());
    }

    #[test]
    fn pixels_outside_the_image_are_clipped() {
        let mut object = overlay_test_object();
        put_overlay(&mut object, 0x6000, 2, 2, vec![0b0000_1111]);
        object.put_element(DataElement::new(
            Tag(0x6000, 0x0050),
            VR::SS,
            dicom_value!(I16, [0, 2]),
        ));

        let bitmaps = read_overlay_bitmaps(&object, 0);
        let mut image = black_image(2, 2);
        paint_overlay_bitmaps(&bitmaps, &mut image);

        assert_eq!(set_pixels(&image), vec![(1, 0)]);
    }

    #[test]
    fn every_overlay_group_with_data_is_collected() {
        let mut object = overlay_test_object();
        put_overlay(&mut object, 0x6000, 1, 1, vec![1]);
        put_overlay(&mut object, 0x6002, 1, 1, vec![1]);
        put_overlay(&mut object, 0x601E, 1, 1, vec![1]);
        // Rows/columns without Overlay Data describe a retired in-pixel overlay; skip it.
        object.put_element(DataElement::new(
            Tag(0x6004, 0x0010),
            VR::US,
            PrimitiveValue::from(1_u16),
        ));
        object.put_element(DataElement::new(
            Tag(0x6004, 0x0011),
            VR::US,
            PrimitiveValue::from(1_u16),
        ));

        assert_eq!(read_overlay_bitmaps(&object, 0).len(), 3);
    }

    #[test]
    fn missing_size_or_empty_data_yields_no_bitmap() {
        let mut object = overlay_test_object();
        put_overlay(&mut object, 0x6000, 0, 4, vec![0xFF]);
        put_overlay(&mut object, 0x6002, 4, 4, Vec::new());

        assert!(read_overlay_bitmaps(&object, 0).is_empty());
    }

    #[test]
    fn short_overlay_data_treats_missing_bits_as_clear() {
        let bitmap = OverlayBitmap {
            rows: 2,
            columns: 8,
            origin_row: 1,
            origin_column: 1,
            bits: vec![0b0000_0001],
        };
        let mut image = black_image(8, 2);
        paint_overlay_bitmaps(&[bitmap], &mut image);

        assert_eq!(set_pixels(&image), vec![(0, 0)]);
    }
}
