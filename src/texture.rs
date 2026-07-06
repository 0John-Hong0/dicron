use eframe::egui;

/// CPU-side conversion of a decoded image to an egui `ColorImage`. Kept separate
/// from the GPU upload so it can run off the UI thread.
pub fn color_image_from_dynamic_image(dynamic_image: &image::DynamicImage) -> egui::ColorImage {
    let rgba_image = dynamic_image.to_rgba8();
    let image_size = [rgba_image.width() as usize, rgba_image.height() as usize];

    egui::ColorImage::from_rgba_unmultiplied(image_size, rgba_image.as_raw())
}

/// Upload a prepared `ColorImage` to the GPU. Must run on the UI thread.
pub fn upload_color_image(
    context: &egui::Context,
    texture_name: &str,
    color_image: egui::ColorImage,
) -> egui::TextureHandle {
    context.load_texture(texture_name, color_image, egui::TextureOptions::LINEAR)
}

pub fn fit_image_to_available_space(
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
