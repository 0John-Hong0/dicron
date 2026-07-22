//! About, update notification, and small informational dialogs.

use eframe::egui;

use crate::app::state::{AboutDialogState, UpdateCheckStatus};
use crate::release_check::UpdateCheckOutcome;
use crate::settings::AppSettings;

const APP_AUTHORS: &str = env!("CARGO_PKG_AUTHORS");
const APP_REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");

impl AboutDialogState {
    pub(super) fn poll(&mut self, context: &egui::Context, check_for_updates_on_startup: bool) {
        if std::mem::take(&mut self.startup_check_pending) {
            self.start_update_check(context, true);
        }

        self.receive_update_result(context, check_for_updates_on_startup);
    }

    pub(super) fn open(&mut self, context: &egui::Context) {
        self.open = true;

        if matches!(self.update_status, UpdateCheckStatus::NotChecked) {
            self.start_update_check(context, false);
        }
    }

    pub(super) fn show(&mut self, context: &egui::Context, settings: &mut AppSettings) {
        if !self.open {
            return;
        }

        let modal_response = egui::Modal::new(egui::Id::new("about_dialog")).show(context, |ui| {
            ui.set_min_width(360.0);
            ui.set_max_width(420.0);

            ui.vertical_centered(|ui| {
                ui.heading("Dicron");
                ui.separator();
                ui.add(egui::Label::new(env!("CARGO_PKG_DESCRIPTION")).wrap());
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("Version v{}", env!("CARGO_PKG_VERSION")))
                        .monospace(),
                );
                ui.hyperlink_to("GitHub", APP_REPOSITORY_URL);
            });

            self.show_update_controls(ui, context, settings);
            ui.separator();
            ui.add_space(4.0);
            egui::CollapsingHeader::new("System information")
                .id_salt("about_system_information")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Platform");
                        ui.monospace(format!(
                            "{} / {}",
                            std::env::consts::OS,
                            std::env::consts::ARCH
                        ));
                    });
                });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let footer_text_color = ui
                    .visuals()
                    .text_color()
                    .lerp_to_gamma(ui.visuals().window_fill(), 0.25);

                ui.label(
                    egui::RichText::new(format!(
                        "{} · Maintained by {}",
                        env!("CARGO_PKG_LICENSE"),
                        format_authors(APP_AUTHORS)
                    ))
                    .color(footer_text_color),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        ui.close();
                    }
                });
            });
        });

        if modal_response.should_close() {
            self.open = false;
        }
    }

    pub(super) fn show_notification(&mut self, context: &egui::Context) {
        if !self.notification_visible || self.open {
            return;
        }

        let UpdateCheckStatus::Finished(UpdateCheckOutcome::UpdateAvailable {
            latest_tag,
            release_url,
        }) = &self.update_status
        else {
            self.notification_visible = false;
            return;
        };

        let mut notification_open = true;

        egui::Window::new("Update available")
            .id(egui::Id::new("update_available_notification"))
            .open(&mut notification_open)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -12.0))
            .order(egui::Order::Foreground)
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .show(context, |ui| {
                ui.add(
                    egui::Label::new(format!(
                        "A newer version of Dicron ({latest_tag}) is available."
                    ))
                    .wrap(),
                );
                ui.hyperlink_to("Open release", release_url);
            });

        if !notification_open {
            self.notification_visible = false;
        }
    }

    fn show_update_controls(
        &mut self,
        ui: &mut egui::Ui,
        context: &egui::Context,
        settings: &mut AppSettings,
    ) {
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);

        let can_check_for_updates = !self.update_status.is_checking();
        let check_button_clicked = ui
            .horizontal(|ui| {
                ui.label(egui::RichText::new("Updates").strong());

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled(
                        can_check_for_updates,
                        egui::Button::new("Check for updates"),
                    )
                    .clicked()
                })
                .inner
            })
            .inner;

        if check_button_clicked {
            self.start_update_check(context, false);
        }

        ui.add_space(4.0);
        let mut check_for_updates_on_startup = settings.check_for_updates_on_startup;

        if ui
            .checkbox(
                &mut check_for_updates_on_startup,
                "Check for updates on startup",
            )
            .changed()
        {
            settings.set_check_for_updates_on_startup(check_for_updates_on_startup);

            if !check_for_updates_on_startup {
                self.notification_visible = false;
            }
        }

        ui.add_space(4.0);
        match &self.update_status {
            UpdateCheckStatus::NotChecked => {}
            UpdateCheckStatus::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Checking...");
                });
            }
            UpdateCheckStatus::Finished(UpdateCheckOutcome::UpToDate { latest_tag }) => {
                ui.label(format!("Dicron is up to date ({latest_tag})."));
            }
            UpdateCheckStatus::Finished(UpdateCheckOutcome::UpdateAvailable {
                latest_tag,
                release_url,
            }) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("A newer version ({latest_tag}) is available."));
                    ui.hyperlink_to("Open release", release_url);
                });
            }
            UpdateCheckStatus::Failed(error_message) => {
                ui.add(egui::Label::new(format!("Update check failed: {error_message}")).wrap());
            }
        }

        ui.add_space(4.0);
    }

    fn start_update_check(&mut self, context: &egui::Context, notify_if_available: bool) {
        if self.update_status.is_checking() {
            return;
        }

        if !self.update_job.start(context) {
            return;
        }
        self.update_status = UpdateCheckStatus::Checking;
        self.notify_when_finished = notify_if_available;
    }

    fn receive_update_result(
        &mut self,
        context: &egui::Context,
        check_for_updates_on_startup: bool,
    ) {
        let Some(result) = self.update_job.poll(context) else {
            return;
        };

        match result {
            Ok(outcome) => {
                self.notification_visible = (self.notification_visible
                    || self.notify_when_finished)
                    && check_for_updates_on_startup
                    && matches!(&outcome, UpdateCheckOutcome::UpdateAvailable { .. });
                self.notify_when_finished = false;
                self.update_status = UpdateCheckStatus::Finished(outcome);
            }
            Err(error_message) => {
                self.notify_when_finished = false;
                self.update_status = UpdateCheckStatus::Failed(error_message);
            }
        }
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
