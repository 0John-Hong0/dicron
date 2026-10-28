//! Front door for all egui rendering.

mod dialogs;
mod dicom_tree;
mod main_view;
mod metadata_panel;
mod overlay;
mod status_bar;
mod three_d;
mod toolbar;
mod viewer;

pub(in crate::app) use three_d::{GPU_UNAVAILABLE_MESSAGE, THREE_D_UNSUPPORTED_MESSAGE};
pub(in crate::app) use viewer::upload_display_pixels;

pub(super) fn show(
    app: &mut crate::app::DicronApp,
    ui: &mut eframe::egui::Ui,
    frame: &mut eframe::Frame,
) {
    main_view::show(app, ui, frame);
}
