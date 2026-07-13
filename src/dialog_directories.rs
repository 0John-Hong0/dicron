use std::path::{Path, PathBuf};

pub struct DialogDirectories {
    pub open_dicom_directory: Option<PathBuf>,
    pub open_folder_directory: Option<PathBuf>,
    /// Whether newly loaded Patient/Study/Series nodes start expanded.
    pub expand_tree_by_default: bool,
    /// Whether Dicron checks GitHub for a newer release when it starts.
    pub check_for_updates_on_startup: bool,
}

impl Default for DialogDirectories {
    fn default() -> Self {
        Self {
            open_dicom_directory: None,
            open_folder_directory: None,
            expand_tree_by_default: true,
            check_for_updates_on_startup: true,
        }
    }
}

impl DialogDirectories {
    pub fn load() -> Self {
        let Some(settings_path) = settings_path() else {
            return Self::default();
        };

        let Ok(settings_text) = std::fs::read_to_string(settings_path) else {
            return Self::default();
        };

        let mut dialog_directories = Self::default();

        for settings_line in settings_text.lines() {
            let Some((key, value)) = settings_line.split_once('=') else {
                continue;
            };

            match key {
                "open_dicom_directory" => {
                    let directory = PathBuf::from(value);
                    if directory.is_dir() {
                        dialog_directories.open_dicom_directory = Some(directory);
                    }
                }
                "open_folder_directory" => {
                    let directory = PathBuf::from(value);
                    if directory.is_dir() {
                        dialog_directories.open_folder_directory = Some(directory);
                    }
                }
                "expand_tree_by_default" => {
                    dialog_directories.expand_tree_by_default = value.trim() != "false";
                }
                "check_for_updates_on_startup" => {
                    dialog_directories.check_for_updates_on_startup = value.trim() != "false";
                }
                _ => {}
            }
        }

        dialog_directories
    }

    pub fn set_expand_tree_by_default(&mut self, expand_tree_by_default: bool) {
        self.expand_tree_by_default = expand_tree_by_default;
        self.save();
    }

    pub fn set_check_for_updates_on_startup(&mut self, check_for_updates_on_startup: bool) {
        self.check_for_updates_on_startup = check_for_updates_on_startup;
        self.save();
    }

    pub fn remember_open_dicom_path(&mut self, selected_dicom_path: &Path) {
        if let Some(directory) = selected_dicom_path.parent().filter(|path| path.is_dir()) {
            self.open_dicom_directory = Some(directory.to_path_buf());
            self.save();
        }
    }

    pub fn remember_open_folder_path(&mut self, selected_folder_path: &Path) {
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

        let _ = std::fs::write(settings_path, settings_text);
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
