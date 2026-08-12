//! Top-level application composition and the eframe entry point.

mod actions;
mod background_tasks;
mod frame_cache;
mod state;
mod ui;

pub(crate) use state::DicronApp;

impl DicronApp {
    pub(crate) fn new(context: &eframe::egui::Context) -> Self {
        let app = Self::default();
        apply_theme_preference(context, app.settings.theme_preference);
        app
    }

    fn set_theme_preference(
        &mut self,
        context: &eframe::egui::Context,
        theme_preference: eframe::egui::ThemePreference,
    ) {
        apply_theme_preference(context, theme_preference);
        self.settings.set_theme_preference(theme_preference);
    }
}

impl eframe::App for DicronApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        ui::show(self, ui, frame);
    }

    fn clear_color(&self, visuals: &eframe::egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

fn apply_theme_preference(
    context: &eframe::egui::Context,
    theme_preference: eframe::egui::ThemePreference,
) {
    context.set_theme(theme_preference);
    context.send_viewport_cmd(eframe::egui::ViewportCommand::SetTheme(viewport_theme(
        theme_preference,
    )));
}

fn viewport_theme(theme_preference: eframe::egui::ThemePreference) -> eframe::egui::SystemTheme {
    match theme_preference {
        eframe::egui::ThemePreference::System => eframe::egui::SystemTheme::SystemDefault,
        eframe::egui::ThemePreference::Light => eframe::egui::SystemTheme::Light,
        eframe::egui::ThemePreference::Dark => eframe::egui::SystemTheme::Dark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_theme_preference_updates_egui_context() {
        let context = eframe::egui::Context::default();

        for theme_preference in [
            eframe::egui::ThemePreference::System,
            eframe::egui::ThemePreference::Light,
            eframe::egui::ThemePreference::Dark,
        ] {
            apply_theme_preference(&context, theme_preference);

            assert_eq!(
                context.options(|options| options.theme_preference),
                theme_preference
            );
        }
    }

    #[test]
    fn theme_preferences_map_to_matching_viewport_themes() {
        assert_eq!(
            viewport_theme(eframe::egui::ThemePreference::System),
            eframe::egui::SystemTheme::SystemDefault
        );
        assert_eq!(
            viewport_theme(eframe::egui::ThemePreference::Light),
            eframe::egui::SystemTheme::Light
        );
        assert_eq!(
            viewport_theme(eframe::egui::ThemePreference::Dark),
            eframe::egui::SystemTheme::Dark
        );
    }
}
