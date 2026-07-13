use eframe::egui;

const APP_AUTHORS: &str = env!("CARGO_PKG_AUTHORS");

pub(super) struct AboutDialog {
    open: bool,
}

impl AboutDialog {
    pub(super) fn new() -> Self {
        Self { open: false }
    }

    pub(super) fn open(&mut self) {
        self.open = true;
    }

    pub(super) fn show(&mut self, context: &egui::Context) {
        if !self.open {
            return;
        }

        let mut open = self.open;

        egui::Window::new("About Dicron")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(context, |ui| {
                ui.heading("Dicron");
                ui.separator();

                egui::Grid::new("about_dialog_grid")
                    .num_columns(2)
                    .spacing(egui::vec2(12.0, 6.0))
                    .show(ui, |ui| {
                        ui.label("Version");
                        ui.monospace(env!("CARGO_PKG_VERSION"));
                        ui.end_row();

                        ui.label("Description");
                        ui.label(env!("CARGO_PKG_DESCRIPTION"));
                        ui.end_row();

                        ui.label("Maintainer");
                        ui.label(format_authors(APP_AUTHORS));
                        ui.end_row();

                        ui.label("License");
                        ui.label(env!("CARGO_PKG_LICENSE"));
                        ui.end_row();

                        ui.label("Platform");
                        ui.label(format!(
                            "{} / {}",
                            std::env::consts::OS,
                            std::env::consts::ARCH
                        ));
                        ui.end_row();
                    });
            });

        self.open = open;
    }
}

/// `CARGO_PKG_AUTHORS` is colon-separated and may be empty; render it for display.
fn format_authors(authors: &str) -> String {
    if authors.trim().is_empty() {
        "unknown".to_owned()
    } else {
        authors.replace(':', ", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_authors_handles_empty_and_multiple() {
        assert_eq!(format_authors(""), "unknown");
        assert_eq!(format_authors("   "), "unknown");
        assert_eq!(format_authors("Alice"), "Alice");
        assert_eq!(format_authors("Alice:Bob"), "Alice, Bob");
    }
}
