//! Conversion of neutral display pixels to egui textures and viewer image sizing.

use eframe::egui;

use crate::dicom::DisplayPixels;

/// An image shown in the 2D viewer. Its texture is smaller than the source
/// image when the source does not fit the GPU; layout and the pixel probe use
/// the source size.
pub(in crate::app) struct ViewerImage {
    pub(in crate::app) texture: egui::TextureHandle,
    /// Columns and rows of the source image.
    pub(in crate::app) source_size: [usize; 2],
}

impl ViewerImage {
    /// Upload the image the viewer shows, keeping the size of its source.
    pub(in crate::app) fn upload(
        context: &egui::Context,
        texture_name: &str,
        pixels: DisplayPixels,
    ) -> Self {
        let source_size = [pixels.width, pixels.height];

        Self {
            texture: upload_display_pixels(context, texture_name, pixels),
            source_size,
        }
    }
}

/// Upload a prepared `ColorImage` to the GPU, shrunk to fit the largest
/// texture it allows. Must run on the UI thread.
pub(in crate::app) fn upload_display_pixels(
    context: &egui::Context,
    texture_name: &str,
    pixels: DisplayPixels,
) -> egui::TextureHandle {
    let max_texture_side = context.input(|input| input.max_texture_side);
    let pixels = shrunk_to_texture_side(pixels, max_texture_side);
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([pixels.width, pixels.height], &pixels.rgba);

    context.load_texture(texture_name, color_image, egui::TextureOptions::LINEAR)
}

/// Fit oversized pixels to the texture limit, rounding the shorter side to
/// preserve their proportions. Average each source pixel's area of overlap
/// with the output texel, including fractional edge pixels.
fn shrunk_to_texture_side(pixels: DisplayPixels, max_texture_side: usize) -> DisplayPixels {
    let longest_side = pixels.width.max(pixels.height);
    let limit = max_texture_side.max(1);
    if longest_side <= limit {
        return pixels;
    }

    let scaled_side = |side: usize| {
        ((side as u128 * limit as u128 + longest_side as u128 / 2) / longest_side as u128).max(1)
            as usize
    };
    let width = scaled_side(pixels.width);
    let height = scaled_side(pixels.height);
    let mut rgba = Vec::with_capacity(width * height * 4);

    for row in 0..height {
        let top = row as f64 * pixels.height as f64 / height as f64;
        let bottom = (row + 1) as f64 * pixels.height as f64 / height as f64;
        let block_rows = top.floor() as usize..(bottom.ceil() as usize).min(pixels.height);

        for column in 0..width {
            let left = column as f64 * pixels.width as f64 / width as f64;
            let right = (column + 1) as f64 * pixels.width as f64 / width as f64;
            let block_columns = left.floor() as usize..(right.ceil() as usize).min(pixels.width);
            let mut channel_sums = [0.0_f64; 4];

            for block_row in block_rows.clone() {
                let row_weight = bottom.min((block_row + 1) as f64) - top.max(block_row as f64);
                let row_start = block_row * pixels.width;
                for block_column in block_columns.clone() {
                    let column_weight =
                        right.min((block_column + 1) as f64) - left.max(block_column as f64);
                    let weight = row_weight * column_weight;
                    let offset = (row_start + block_column) * 4;
                    for (sum, &channel) in channel_sums
                        .iter_mut()
                        .zip(&pixels.rgba[offset..offset + 4])
                    {
                        *sum += f64::from(channel) * weight;
                    }
                }
            }

            let area = (bottom - top) * (right - left);
            rgba.extend(channel_sums.map(|sum| (sum / area).round() as u8));
        }
    }

    DisplayPixels {
        width,
        height,
        rgba,
    }
}

pub(super) fn fit_image_to_available_space(
    image_size: [usize; 2],
    pixel_aspect_ratio: Option<f64>,
    available_size: egui::Vec2,
) -> egui::Vec2 {
    let safe_available_size = egui::vec2(available_size.x.max(1.0), available_size.y.max(1.0));
    let proportions = egui::vec2(
        image_size[0] as f32,
        image_size[1] as f32 * pixel_aspect_ratio.unwrap_or(1.0) as f32,
    );
    if proportions.x <= 0.0 || proportions.y <= 0.0 {
        return safe_available_size;
    }
    let width_scale = safe_available_size.x / proportions.x;
    let height_scale = safe_available_size.y / proportions.y;
    let scale = width_scale.min(height_scale).max(0.0);
    proportions * scale
}

#[cfg(test)]
mod tests {
    use super::shrunk_to_texture_side;
    use crate::dicom::DisplayPixels;

    #[test]
    fn oversized_pixels_average_fractional_source_areas() {
        // Five gray pixels span three output texels, each covering 5/3 source pixels.
        let greys = [10_u8, 20, 30, 41, 50];
        let pixels = DisplayPixels {
            width: 5,
            height: 1,
            rgba: greys
                .iter()
                .flat_map(|&grey| [grey, grey, grey, 255])
                .collect(),
        };

        let shrunk = shrunk_to_texture_side(pixels, 3);

        assert_eq!([shrunk.width, shrunk.height], [3, 1]);
        assert_eq!(
            shrunk.rgba,
            [14, 14, 14, 255, 30, 30, 30, 255, 46, 46, 46, 255]
        );
    }

    fn pixels(
        width: usize,
        height: usize,
        pixel: impl Fn(usize, usize) -> [u8; 4],
    ) -> DisplayPixels {
        DisplayPixels {
            width,
            height,
            rgba: (0..height)
                .flat_map(|row| (0..width).map(move |column| (column, row)))
                .flat_map(|(column, row)| pixel(column, row))
                .collect(),
        }
    }

    /// A tall image averages all channels over the source areas it covers,
    /// with fractional rows contributing to both output rows.
    #[test]
    fn tall_pixels_average_each_channel_over_fractional_rows() {
        let tall = pixels(3, 5, |column, row| {
            let value = (10 * row + column) as u8;
            [value, 100 - value, column as u8 * 50, 200 + row as u8]
        });

        let shrunk = shrunk_to_texture_side(tall, 2);

        assert_eq!([shrunk.width, shrunk.height], [1, 2]);
        assert_eq!(shrunk.rgba, [9, 91, 50, 201, 33, 67, 50, 203]);
    }

    /// Crossing the texture limit by one pixel keeps nearly full resolution
    /// and retains fine alternating lines near the edges in either orientation.
    #[test]
    fn images_just_over_the_limit_retain_resolution_and_detail() {
        for (width, height, expected_size) in [(8193, 512, [8192, 512]), (512, 8193, [512, 8192])] {
            let source = pixels(width, height, |column, row| {
                let index = if width > height { column } else { row };
                let value = if index % 2 == 0 { 0 } else { 255 };
                [value, value, value, 200]
            });
            let shrunk = shrunk_to_texture_side(source, 8192);
            assert_eq!([shrunk.width, shrunk.height], expected_size);
            let offset = if width > height { 4 } else { shrunk.width * 4 };
            assert_eq!(&shrunk.rgba[..4], &[0, 0, 0, 200]);
            assert_eq!(&shrunk.rgba[offset..offset + 4], &[255, 255, 255, 200]);
            assert_eq!(&shrunk.rgba[shrunk.rgba.len() - 4..], &[0, 0, 0, 200]);
        }
    }

    /// Images at the limit keep every pixel; oversized images use the limit.
    #[test]
    fn only_images_over_the_limit_shrink() {
        let row = |width| pixels(width, 1, |column, _| [column as u8; 4]);

        let at_limit = shrunk_to_texture_side(row(4), 4);
        assert_eq!([at_limit.width, at_limit.height], [4, 1]);
        assert_eq!(at_limit.rgba, row(4).rgba);

        let over_limit = shrunk_to_texture_side(row(5), 4);
        assert_eq!([over_limit.width, over_limit.height], [4, 1]);

        let single = shrunk_to_texture_side(row(1), 0);
        assert_eq!([single.width, single.height], [1, 1]);
    }
}
