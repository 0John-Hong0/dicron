//! Toolbar actions and loaded-frame readouts.

use std::path::Path;

use eframe::egui;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolbarAction {
    OpenDicom,
    OpenFolder,
    OpenAbout,
    ResetWindowLevel,
}

pub(super) fn show_actions(ui: &mut egui::Ui) -> Option<ToolbarAction> {
    let mut action = None;

    ui.horizontal(|ui| {
        if ui.button("Open DICOM").clicked() {
            action = Some(ToolbarAction::OpenDicom);
        }
        if ui.button("Open Folder").clicked() {
            action = Some(ToolbarAction::OpenFolder);
        }
        if ui.button("About").clicked() {
            action = Some(ToolbarAction::OpenAbout);
        }
    });

    action
}

pub(super) fn show_loaded_dicom_status(
    ui: &mut egui::Ui,
    selected_dicom_path: Option<&Path>,
    selected_frame_index: u32,
    selected_frame_count: u32,
    window_center: f64,
    window_width: f64,
    selected_slice: Option<(usize, usize)>,
) -> Option<ToolbarAction> {
    let selected_dicom_path = selected_dicom_path?;

    if selected_frame_count > 1 {
        ui.label(format!(
            "{} [frame {} / {}]",
            selected_dicom_path.display(),
            selected_frame_index + 1,
            selected_frame_count
        ));
    } else {
        ui.label(selected_dicom_path.display().to_string());
    }

    ui.separator();
    let mut action = None;

    ui.horizontal(|ui| {
        show_window_level_readout(ui, "WindowCenter", window_center);
        show_window_level_readout(ui, "WindowWidth", window_width);

        if ui.button("Reset WL").clicked() {
            action = Some(ToolbarAction::ResetWindowLevel);
        }

        if let Some((selected_slice_index, slice_count)) = selected_slice {
            ui.separator();
            ui.label(format!(
                "Slice {} / {}",
                selected_slice_index + 1,
                slice_count
            ));
        }
    });

    action
}

fn show_window_level_readout(ui: &mut egui::Ui, label: &str, value: f64) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).weak());
        ui.label(egui::RichText::new(format!("{value:.1}")).monospace());
    });
}
