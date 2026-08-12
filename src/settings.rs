//! Persisted local settings and remembered filesystem locations.

use std::path::{Path, PathBuf};

use eframe::egui::ThemePreference;

pub(crate) struct AppSettings {
    pub(crate) open_dicom_directory: Option<PathBuf>,
    pub(crate) open_folder_directory: Option<PathBuf>,
    /// Whether newly loaded Patient/Study/Series nodes start expanded.
    pub(crate) expand_tree_by_default: bool,
    /// Whether Dicron checks GitHub for a newer release when it starts.
    pub(crate) check_for_updates_on_startup: bool,
    /// Which application theme Dicron follows.
    pub(crate) theme_preference: ThemePreference,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            open_dicom_directory: None,
            open_folder_directory: None,
            expand_tree_by_default: true,
            check_for_updates_on_startup: true,
            theme_preference: ThemePreference::System,
        }
    }
}

impl AppSettings {
    pub(crate) fn load() -> Self {
        let Some(settings_path) = settings_path() else {
            return Self::default();
        };

        let Ok(settings_text) = std::fs::read_to_string(settings_path) else {
            return Self::default();
        };

        Self::from_text(&settings_text)
    }

    fn from_text(settings_text: &str) -> Self {
        let mut settings = Self::default();

        for settings_line in settings_text.lines() {
            let Some((key, value)) = settings_line.split_once('=') else {
                continue;
            };

            match key {
                "open_dicom_directory" => {
                    let directory = PathBuf::from(value);
                    if directory.is_dir() {
                        settings.open_dicom_directory = Some(directory);
                    }
                }
                "open_folder_directory" => {
                    let directory = PathBuf::from(value);
                    if directory.is_dir() {
                        settings.open_folder_directory = Some(directory);
                    }
                }
                "expand_tree_by_default" => {
                    settings.expand_tree_by_default = value.trim() != "false";
                }
                "check_for_updates_on_startup" => {
                    settings.check_for_updates_on_startup = value.trim() != "false";
                }
                "theme_preference" => {
                    if let Some(theme_preference) = parse_theme_preference(value) {
                        settings.theme_preference = theme_preference;
                    }
                }
                _ => {}
            }
        }

        settings
    }

    pub(crate) fn set_expand_tree_by_default(&mut self, expand_tree_by_default: bool) {
        self.expand_tree_by_default = expand_tree_by_default;
        self.save();
    }

    pub(crate) fn set_check_for_updates_on_startup(&mut self, check_for_updates_on_startup: bool) {
        self.check_for_updates_on_startup = check_for_updates_on_startup;
        self.save();
    }

    pub(crate) fn set_theme_preference(&mut self, theme_preference: ThemePreference) {
        self.theme_preference = theme_preference;
        self.save();
    }

    pub(crate) fn remember_open_dicom_path(&mut self, selected_dicom_path: &Path) {
        if let Some(directory) = selected_dicom_path.parent().filter(|path| path.is_dir()) {
            self.open_dicom_directory = Some(directory.to_path_buf());
            self.save();
        }
    }

    pub(crate) fn remember_open_folder_path(&mut self, selected_folder_path: &Path) {
        if let Some(directory) = selected_folder_path.parent().filter(|path| path.is_dir()) {
            self.open_folder_directory = Some(directory.to_path_buf());
            self.save();
        }
    }

    fn save(&self) {
        let Some(settings_path) = settings_path() else {
            return;
        };

        let Some(settings_directory) = settings_path.parent() else {
            return;
        };

        if std::fs::create_dir_all(settings_directory).is_err() {
            return;
        }

        let _ = std::fs::write(settings_path, self.to_text());
    }

    fn to_text(&self) -> String {
        let mut settings_text = String::new();

        push_setting_line(
            &mut settings_text,
            "open_dicom_directory",
            self.open_dicom_directory.as_deref(),
        );
        push_setting_line(
            &mut settings_text,
            "open_folder_directory",
            self.open_folder_directory.as_deref(),
        );

        settings_text.push_str("expand_tree_by_default=");
        settings_text.push_str(if self.expand_tree_by_default {
            "true"
        } else {
            "false"
        });
        settings_text.push('\n');

        settings_text.push_str("check_for_updates_on_startup=");
        settings_text.push_str(if self.check_for_updates_on_startup {
            "true"
        } else {
            "false"
        });
        settings_text.push('\n');

        settings_text.push_str("theme_preference=");
        settings_text.push_str(theme_preference_value(self.theme_preference));
        settings_text.push('\n');

        settings_text
    }
}

fn parse_theme_preference(value: &str) -> Option<ThemePreference> {
    match value.trim() {
        "system" => Some(ThemePreference::System),
        "light" => Some(ThemePreference::Light),
        "dark" => Some(ThemePreference::Dark),
        _ => None,
    }
}

fn theme_preference_value(theme_preference: ThemePreference) -> &'static str {
    match theme_preference {
        ThemePreference::System => "system",
        ThemePreference::Light => "light",
        ThemePreference::Dark => "dark",
    }
}

/// Write a `key=value` line, but only for paths that round-trip safely through
/// this line-based, unescaped format: valid UTF-8 with no embedded newline.
/// `to_string_lossy` would silently corrupt non-UTF-8 paths, so we skip them.
fn push_setting_line(settings_text: &mut String, key: &str, directory: Option<&Path>) {
    let Some(directory) = directory else {
        return;
    };

    let Some(directory) = directory.to_str() else {
        return;
    };

    if directory.contains(['\n', '\r']) {
        return;
    }

    settings_text.push_str(key);
    settings_text.push('=');
    settings_text.push_str(directory);
    settings_text.push('\n');
}

fn settings_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|home_directory| home_directory.join(".config"))
        })
        .map(|config_directory| config_directory.join("dicron").join("dialog-dirs.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_preferences_round_trip_through_settings_text() {
        for theme_preference in [
            ThemePreference::System,
            ThemePreference::Light,
            ThemePreference::Dark,
        ] {
            let settings = AppSettings {
                theme_preference,
                ..Default::default()
            };
            let loaded_settings = AppSettings::from_text(&settings.to_text());

            assert_eq!(loaded_settings.theme_preference, theme_preference);
        }
    }

    #[test]
    fn missing_or_unknown_theme_preference_uses_system() {
        let missing_setting = AppSettings::from_text("expand_tree_by_default=false\n");
        let unknown_setting = AppSettings::from_text("theme_preference=sepia\n");

        assert_eq!(missing_setting.theme_preference, ThemePreference::System);
        assert_eq!(unknown_setting.theme_preference, ThemePreference::System);
    }
}
