//! Front door for all egui rendering.

mod dialogs;
mod dicom_tree;
mod main_view;
mod metadata_panel;
mod status_bar;
mod toolbar;
mod viewer;

pub(in crate::app) use viewer::upload_display_pixels;

pub(super) fn show(
    app: &mut crate::app::DicronApp,
    ui: &mut eframe::egui::Ui,
    frame: &mut eframe::Frame,
) {
    main_view::show(app, ui, frame);
}
