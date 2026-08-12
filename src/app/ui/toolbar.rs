//! Toolbar actions and the loaded DICOM path.

use std::path::Path;

use eframe::egui;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolbarAction {
    OpenDicom,
    OpenFolder,
    ShowAbout,
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
            action = Some(ToolbarAction::ShowAbout);
        }
    });

    action
}

pub(super) fn show_loaded_dicom_status(
    ui: &mut egui::Ui,
    selected_dicom_path: Option<&Path>,
) -> bool {
    let Some(selected_dicom_path) = selected_dicom_path else {
        return false;
    };

    ui.add(
        egui::Label::new(selected_dicom_path.display().to_string())
            .selectable(true)
            .truncate(),
    );
    true
}
