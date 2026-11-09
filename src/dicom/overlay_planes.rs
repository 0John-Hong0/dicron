//! Extraction and painting of DICOM overlay planes (groups 6000-601E).
//!
//! Overlay planes are 1-bit bitmaps stored beside the pixel data. Some producers put the
//! entire visible content there (for example Siemens dose report pages, whose pixel data is
//! all zero), so a viewer that ignores them shows a black image.

use std::ops::Range;

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
    pub(super) fn byte_len(&self) -> usize {
        self.bits.len()
    }

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

/// Collects the overlay bitmaps that apply to the zero-based `frame_index` of the image,
/// cut to the part that falls on an image `image_width` x `image_height` pixels.
pub(super) fn read_overlay_bitmaps(
    dicom_object: &DefaultDicomObject,
    frame_index: u32,
    image_width: u32,
    image_height: u32,
) -> Vec<OverlayBitmap> {
    (FIRST_OVERLAY_GROUP..=LAST_OVERLAY_GROUP)
        .step_by(2)
        .filter_map(|group| {
            read_overlay_bitmap(dicom_object, group, frame_index, image_width, image_height)
        })
        .collect()
}

fn read_overlay_bitmap(
    dicom_object: &DefaultDicomObject,
    group: u16,
    frame_index: u32,
    image_width: u32,
    image_height: u32,
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
    // Only the rows and columns that land on the image are kept; the rest would be clipped.
    let visible_rows = visible_range(origin_row, rows, image_height)?;
    let visible_columns = visible_range(origin_column, columns, image_width)?;
    let bits = extract_bits(
        &data,
        first_bit,
        columns,
        visible_rows.clone(),
        visible_columns.clone(),
    )?;

    Some(OverlayBitmap {
        rows: visible_rows.len(),
        columns: visible_columns.len(),
        origin_row: origin_row + visible_rows.start as i64,
        origin_column: origin_column + visible_columns.start as i64,
        bits,
    })
}

/// The indices of an overlay's `length` rows (or columns), placed at the 1-based `origin`,
/// that fall on an image `image_length` pixels long, or `None` when none do.
fn visible_range(origin: i64, length: usize, image_length: u32) -> Option<Range<usize>> {
    // Index `i` lands on the 0-based image pixel `origin - 1 + i`.
    let start = 1_i64.saturating_sub(origin).max(0);
    let end = (i64::from(image_length) + 1)
        .saturating_sub(origin)
        .min(i64::try_from(length).ok()?);
    (start < end).then_some(start as usize..end as usize)
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

/// Re-packs the `rows` x `columns` window of an overlay frame that starts at `first_bit` of
/// `source` and is `stride` bits wide, so the result starts on a byte boundary. Only bits that
/// `source` holds are read; the rest stay clear.
fn extract_bits(
    source: &[u8],
    first_bit: usize,
    stride: usize,
    rows: Range<usize>,
    columns: Range<usize>,
) -> Option<Vec<u8>> {
    let source_bits = source.len().saturating_mul(8);
    let width = columns.len();
    let packed_length = (rows.len() * width).div_ceil(8);
    let mut packed = Vec::new();
    packed.try_reserve_exact(packed_length).ok()?;
    packed.resize(packed_length, 0);

    for (row_offset, row) in rows.enumerate() {
        // `row * stride` stays within the frame's bit count, which did not overflow.
        let row_start = first_bit.saturating_add(row * stride);
        if row_start >= source_bits {
            break;
        }
        for (column_offset, column) in columns.clone().enumerate() {
            if bit_at(source, row_start + column) {
                let offset = row_offset * width + column_offset;
                packed[offset / 8] |= 1 << (offset % 8);
            }
        }
    }
    Some(packed)
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

        let bitmaps = read_overlay_bitmaps(&object, 0, 8, 4);
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

        let bitmaps = read_overlay_bitmaps(&object, 0, 16, 1);
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
            let bitmaps = read_overlay_bitmaps(&object, frame_index, 3, 1);
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

        let bitmaps = read_overlay_bitmaps(&object, 0, 2, 2);
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

        assert_eq!(read_overlay_bitmaps(&object, 0, 1, 1).len(), 3);
    }

    #[test]
    fn missing_size_or_empty_data_yields_no_bitmap() {
        let mut object = overlay_test_object();
        put_overlay(&mut object, 0x6000, 0, 4, vec![0xFF]);
        put_overlay(&mut object, 0x6002, 4, 4, Vec::new());

        assert!(read_overlay_bitmaps(&object, 0, 4, 4).is_empty());
    }

    /// A header claiming a 40000 x 40000 overlay with two bytes of data yields a
    /// bitmap no larger than the image it is painted on.
    #[test]
    fn oversized_overlay_header_is_sized_by_the_image_and_the_data() {
        let mut object = overlay_test_object();
        put_overlay(&mut object, 0x6000, 40000, 40000, vec![0b0000_0101, 0xFF]);

        let bitmaps = read_overlay_bitmaps(&object, 0, 4, 4);
        let mut image = black_image(4, 4);
        paint_overlay_bitmaps(&bitmaps, &mut image);

        assert_eq!(bitmaps.len(), 1);
        assert!(bitmaps[0].rows <= 4 && bitmaps[0].columns <= 4);
        assert!(bitmaps[0].bits.len() <= 2);
        // Only the first row has data; its first four bits are 1, 0, 1, 0.
        assert_eq!(set_pixels(&image), vec![(0, 0), (2, 0)]);
    }

    /// Overlay sizes written as UL in an explicit VR file cannot abort the process.
    #[test]
    fn overlay_sizes_wider_than_us_do_not_abort() {
        let mut object = overlay_test_object();
        put_overlay(&mut object, 0x6000, 1, 1, vec![0b0000_0011]);
        for element in [0x0010, 0x0011] {
            object.put_element(DataElement::new(
                Tag(0x6000, element),
                VR::UL,
                PrimitiveValue::from(u32::MAX),
            ));
        }

        let bitmaps = read_overlay_bitmaps(&object, 0, 2, 2);
        let mut image = black_image(2, 2);
        paint_overlay_bitmaps(&bitmaps, &mut image);

        assert_eq!(set_pixels(&image), vec![(0, 0), (1, 0)]);
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

    /// Every declared bit of the selected overlay frame painted where it lands,
    /// with no trimming: the result the trimmed bitmaps must reproduce.
    fn reference_pixels(
        (rows, columns): (usize, usize),
        (origin_row, origin_column): (i64, i64),
        data: &[u8],
        overlay_frame: usize,
        (width, height): (u32, u32),
    ) -> Vec<(u32, u32)> {
        let mut pixels = Vec::new();
        for row in 0..rows {
            for column in 0..columns {
                let bit = overlay_frame * rows * columns + row * columns + column;
                let set = data
                    .get(bit / 8)
                    .is_some_and(|byte| byte & (1 << (bit % 8)) != 0);
                let x = origin_column - 1 + column as i64;
                let y = origin_row - 1 + row as i64;
                if set && (0..i64::from(width)).contains(&x) && (0..i64::from(height)).contains(&y)
                {
                    pixels.push((x as u32, y as u32));
                }
            }
        }
        pixels.sort_by_key(|&(x, y)| (y, x));
        pixels
    }

    /// Overlays smaller and larger than the image, at origins before, inside
    /// and past its edges, with odd bit counts, several frames and data cut
    /// short, paint exactly the pixels the untrimmed overlay would, from a
    /// bitmap no larger than the image.
    #[test]
    fn trimmed_overlays_paint_what_the_whole_overlay_would() {
        let pattern: Vec<u8> = (0_u32..64)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let origins = [
            (1, 1),
            (-2, -1),
            (0, 0),
            (2, 3),
            (3, 1),
            (1, 5),
            (-6, 2),
            (4, 6),
        ];
        let mut painted_cases = 0;

        for (width, height) in [(5_u32, 3_u32), (3, 5), (1, 1), (6, 6)] {
            for (rows, columns) in [(3_u16, 5_u16), (5, 3), (7, 9), (1, 13), (13, 1), (2, 2)] {
                for origin in origins {
                    for (multi_frame, frame_index, data_length) in [
                        (true, 0, 64),
                        (true, 1, 64),
                        (true, 2, 64),
                        (true, 3, 64),
                        (true, 4, 64),
                        (true, 1, 3),
                        (false, 0, 64),
                        (false, 1, 64),
                        (false, 0, 1),
                    ] {
                        let data = pattern[..data_length].to_vec();
                        let mut object = overlay_test_object();
                        put_overlay(&mut object, 0x6000, rows, columns, data.clone());
                        object.put_element(DataElement::new(
                            Tag(0x6000, 0x0050),
                            VR::SS,
                            dicom_value!(I16, [origin.0 as i16, origin.1 as i16]),
                        ));
                        // Three overlay frames shown with image frames 2 to 4;
                        // without these attributes, one shown with frame 1.
                        let (frame_count, first_image_frame) = if multi_frame {
                            object.put_element(DataElement::new(
                                Tag(0x6000, 0x0015),
                                VR::IS,
                                PrimitiveValue::from("3"),
                            ));
                            object.put_element(DataElement::new(
                                Tag(0x6000, 0x0051),
                                VR::US,
                                PrimitiveValue::from(2_u16),
                            ));
                            (3, 2)
                        } else {
                            (1, 1)
                        };

                        let bitmaps = read_overlay_bitmaps(&object, frame_index, width, height);
                        let mut image = black_image(width, height);
                        paint_overlay_bitmaps(&bitmaps, &mut image);
                        let case = format!(
                            "image {width}x{height}, overlay {columns}x{rows} at {origin:?}, \
                             frame {frame_index} of {frame_count}, {data_length} bytes"
                        );

                        let overlay_frame = (frame_index + 1)
                            .checked_sub(first_image_frame)
                            .filter(|&overlay_frame| overlay_frame < frame_count);
                        let expected = match overlay_frame {
                            Some(overlay_frame) => reference_pixels(
                                (usize::from(rows), usize::from(columns)),
                                origin,
                                &data,
                                overlay_frame as usize,
                                (width, height),
                            ),
                            None => Vec::new(),
                        };
                        assert_eq!(set_pixels(&image), expected, "{case}");
                        painted_cases += usize::from(!expected.is_empty());
                        for bitmap in &bitmaps {
                            assert!(
                                bitmap.rows <= height as usize
                                    && bitmap.columns <= width as usize
                                    && bitmap.bits.len()
                                        <= (bitmap.rows * bitmap.columns).div_ceil(8),
                                "{case}: {bitmap:?}"
                            );
                        }
                    }
                }
            }
        }
        assert!(painted_cases > 150, "{painted_cases}");
    }
}
