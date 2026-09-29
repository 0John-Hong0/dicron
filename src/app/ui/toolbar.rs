//! Toolbar actions and the loaded DICOM path.

use std::path::Path;

use eframe::egui;

use crate::dicom::TextEncoding;
use crate::theme;

const AUTO_ENCODING_HELP: &str = "Uses the DICOM-declared character set when present. If SpecificCharacterSet is missing, attempts strict UTF-8, Korean, and Japanese detection. Ambiguous text uses DICOM default.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolbarAction {
    OpenDicom,
    OpenFolder,
    SetEncoding(TextEncoding),
    ShowAbout,
    SetTheme(egui::ThemePreference),
}

pub(super) fn show_actions(
    ui: &mut egui::Ui,
    theme_preference: egui::ThemePreference,
    can_reopen: bool,
    text_encoding: TextEncoding,
) -> Option<ToolbarAction> {
    let mut action = None;

    theme::toolbar_row(ui, |ui| {
        if ui.button("Open DICOM").clicked() {
            action = Some(ToolbarAction::OpenDicom);
        }

        if ui.button("Open Folder").clicked() {
            action = Some(ToolbarAction::OpenFolder);
        }

        let button_label = encoding_button_label(text_encoding);
        if can_reopen {
            let mut selected_text_encoding = text_encoding;
            let (response, _) =
                egui::containers::menu::MenuButton::from_button(egui::Button::new(button_label))
                    .config(
                        egui::containers::menu::MenuConfig::new().style(theme::popup_menu_style),
                    )
                    .ui(ui, |ui| {
                        ui.selectable_value(
                            &mut selected_text_encoding,
                            TextEncoding::Auto,
                            "Auto",
                        )
                        .on_hover_text(AUTO_ENCODING_HELP);
                        ui.selectable_value(
                            &mut selected_text_encoding,
                            TextEncoding::DicomDefault,
                            "DICOM default",
                        );
                        ui.selectable_value(
                            &mut selected_text_encoding,
                            TextEncoding::Utf8,
                            "UTF-8",
                        );
                        ui.selectable_value(
                            &mut selected_text_encoding,
                            TextEncoding::KoreanEucKr,
                            "Korean (EUC-KR)",
                        );
                        ui.selectable_value(
                            &mut selected_text_encoding,
                            TextEncoding::JapaneseShiftJis,
                            "Japanese (Shift-JIS)",
                        );
                    });
            if text_encoding == TextEncoding::Auto {
                response.on_hover_text(AUTO_ENCODING_HELP);
            }
            if selected_text_encoding != text_encoding {
                action = Some(ToolbarAction::SetEncoding(selected_text_encoding));
            }
        } else {
            ui.add_enabled(false, egui::Button::new(button_label))
                .on_hover_text(AUTO_ENCODING_HELP);
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

        if ui.button("About").clicked() {
            action = Some(ToolbarAction::ShowAbout);
        }
    });

    action
}

fn encoding_button_label(text_encoding: TextEncoding) -> &'static str {
    match text_encoding {
        TextEncoding::Auto => "Encoding: Auto",
        TextEncoding::DicomDefault => "Encoding: DICOM",
        TextEncoding::KoreanEucKr => "Encoding: Korean",
        TextEncoding::Utf8 => "Encoding: UTF-8",
        TextEncoding::JapaneseShiftJis => "Encoding: Japanese",
    }
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
