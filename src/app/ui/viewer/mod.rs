//! Composition entry point for the DICOM image viewer.

mod controls;
mod image_texture;
mod viewport;
mod window_level;

pub(in crate::app) use image_texture::upload_display_pixels;

pub(super) enum ViewerControlAction {
    ResetView,
    FlipHorizontal,
    FlipVertical,
    RotateClockwise,
    ApplyWindowPreset(crate::app::state::WindowPreset),
    OpenEditWindowing,
}

pub(super) fn show_controls(
    app: &mut crate::app::DicronApp,
    ui: &mut eframe::egui::Ui,
) -> Option<ViewerControlAction> {
    controls::show_control_row(app, ui)
}

pub(super) fn show(app: &mut crate::app::DicronApp, ui: &mut eframe::egui::Ui) {
    viewport::show(app, ui);
}
