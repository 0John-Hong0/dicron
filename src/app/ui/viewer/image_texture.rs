//! Conversion of neutral display pixels to egui textures and viewer image sizing.

use eframe::egui;

use crate::dicom::DisplayPixels;

/// Upload a prepared `ColorImage` to the GPU. Must run on the UI thread.
pub(in crate::app) fn upload_display_pixels(
    context: &egui::Context,
    texture_name: &str,
    pixels: DisplayPixels,
) -> egui::TextureHandle {
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([pixels.width, pixels.height], &pixels.rgba);

    context.load_texture(texture_name, color_image, egui::TextureOptions::LINEAR)
}

pub(in crate::app) fn fit_image_to_available_space(
    texture_size: egui::Vec2,
    available_size: egui::Vec2,
) -> egui::Vec2 {
    let safe_available_size = egui::vec2(available_size.x.max(1.0), available_size.y.max(1.0));

    if texture_size.x <= 0.0 || texture_size.y <= 0.0 {
        return safe_available_size;
    }

    let width_scale = safe_available_size.x / texture_size.x;
    let height_scale = safe_available_size.y / texture_size.y;
    let scale = width_scale.min(height_scale).max(0.0);

    texture_size * scale
}
