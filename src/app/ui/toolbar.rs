//! Toolbar actions and the loaded DICOM path.

use std::path::Path;

use eframe::egui;

use crate::theme;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolbarAction {
    OpenDicom,
    OpenFolder,
    ShowAbout,
    SetTheme(egui::ThemePreference),
}

pub(super) fn show_actions(
    ui: &mut egui::Ui,
    theme_preference: egui::ThemePreference,
) -> Option<ToolbarAction> {
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

        let mut selected_theme_preference = theme_preference;

        let theme_button = egui::Button::new(format!(
            "Theme: {}",
            theme_preference_label(theme_preference)
        ));

        egui::containers::menu::MenuButton::from_button(theme_button)
            .config(egui::containers::menu::MenuConfig::new().style(theme::popup_menu_style))
            .ui(ui, |ui| {
                ui.selectable_value(
                    &mut selected_theme_preference,
                    egui::ThemePreference::System,
                    "System",
                );
                ui.selectable_value(
                    &mut selected_theme_preference,
                    egui::ThemePreference::Light,
                    "Light",
                );
                ui.selectable_value(
                    &mut selected_theme_preference,
                    egui::ThemePreference::Dark,
                    "Dark",
                );
            });

        if selected_theme_preference != theme_preference {
            action = Some(ToolbarAction::SetTheme(selected_theme_preference));
        }
    });

    action
}

fn theme_preference_label(theme_preference: egui::ThemePreference) -> &'static str {
    match theme_preference {
        egui::ThemePreference::System => "System",
        egui::ThemePreference::Light => "Light",
        egui::ThemePreference::Dark => "Dark",
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_preference_has_a_label() {
        assert_eq!(
            theme_preference_label(egui::ThemePreference::System),
            "System"
        );
        assert_eq!(
            theme_preference_label(egui::ThemePreference::Light),
            "Light"
        );
        assert_eq!(theme_preference_label(egui::ThemePreference::Dark), "Dark");
    }
}
