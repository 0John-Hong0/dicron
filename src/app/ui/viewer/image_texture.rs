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

/// The pixels shrunk by one whole factor on both sides, so that neither side
/// exceeds `max_texture_side`. Each texel is the mean of the block of pixels
/// it stands for.
fn shrunk_to_texture_side(pixels: DisplayPixels, max_texture_side: usize) -> DisplayPixels {
    let factor = pixels
        .width
        .max(pixels.height)
        .div_ceil(max_texture_side.max(1));
    if factor <= 1 {
        return pixels;
    }

    let width = pixels.width.div_ceil(factor);
    let height = pixels.height.div_ceil(factor);
    let mut rgba = Vec::with_capacity(width * height * 4);

    for row in 0..height {
        let block_rows = row * factor..((row + 1) * factor).min(pixels.height);

        for column in 0..width {
            let block_columns = column * factor..((column + 1) * factor).min(pixels.width);
            let mut channel_sums = [0_u64; 4];

            for block_row in block_rows.clone() {
                let row_start = block_row * pixels.width;
                let block_pixels = &pixels.rgba
                    [(row_start + block_columns.start) * 4..(row_start + block_columns.end) * 4];

                for pixel in block_pixels.chunks_exact(4) {
                    for (sum, &channel) in channel_sums.iter_mut().zip(pixel) {
                        *sum += u64::from(channel);
                    }
                }
            }

            let pixel_count = (block_rows.len() * block_columns.len()) as u64;
            rgba.extend(channel_sums.map(|sum| ((sum + pixel_count / 2) / pixel_count) as u8));
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
    fn oversized_pixels_shrink_to_the_block_means() {
        // Five grey pixels in a row, two to a texel; the last texel holds one.
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
            [15, 15, 15, 255, 36, 36, 36, 255, 50, 50, 50, 255]
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

    /// A tall image shrinks on both sides by one factor, each channel
    /// averaged over its own block of rows and columns, partial blocks
    /// included.
    #[test]
    fn tall_pixels_shrink_by_one_factor_per_channel() {
        let tall = pixels(3, 5, |column, row| {
            let value = (10 * row + column) as u8;
            [value, 100 - value, column as u8 * 50, 200 + row as u8]
        });

        let shrunk = shrunk_to_texture_side(tall, 2);

        assert_eq!([shrunk.width, shrunk.height], [1, 2]);
        assert_eq!(shrunk.rgba, [11, 89, 50, 201, 36, 64, 50, 204]);
    }

    /// Images at the limit keep every pixel; one pixel over halves them.
    #[test]
    fn only_images_over_the_limit_shrink() {
        let row = |width| pixels(width, 1, |column, _| [column as u8; 4]);

        let at_limit = shrunk_to_texture_side(row(4), 4);
        assert_eq!([at_limit.width, at_limit.height], [4, 1]);
        assert_eq!(at_limit.rgba, row(4).rgba);

        let over_limit = shrunk_to_texture_side(row(5), 4);
        assert_eq!([over_limit.width, over_limit.height], [3, 1]);

        let single = shrunk_to_texture_side(row(1), 0);
        assert_eq!([single.width, single.height], [1, 1]);
    }
}
