//! Top-level application composition and the eframe entry point.

mod actions;
mod background_tasks;
mod frame_cache;
mod frame_loading;
mod series_thumbnail_cache;
mod state;
mod three_d_state;
mod ui;
mod volume_job;

pub(crate) use state::DicronApp;

impl DicronApp {
    /// The application, opening `startup_paths` on its first frame.
    pub(crate) fn new(
        context: &eframe::egui::Context,
        startup_paths: Vec<std::path::PathBuf>,
    ) -> Self {
        let app = Self {
            startup_paths,
            ..Self::default()
        };
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
    fn logic(&mut self, context: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        let startup_paths = std::mem::take(&mut self.startup_paths);
        self.open_startup_paths(context, startup_paths);
    }

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

    fn missing_startup_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dicron-app-missing-startup-{}-{name}.dcm",
            std::process::id()
        ))
    }

    /// Runs one egui pass that calls the app's per-pass logic, as eframe does.
    fn run_logic_pass(context: &eframe::egui::Context, app: &mut DicronApp) {
        let _ = context.run_ui(eframe::egui::RawInput::default(), |ui| {
            eframe::App::logic(app, ui.ctx(), &mut eframe::Frame::_new_kittest());
        });
    }

    /// The paths given at launch are opened once: later frames, and whatever
    /// the user opens or dismisses after them, are not overridden by a
    /// reopen of the launch paths.
    #[test]
    fn startup_paths_open_on_the_first_frame_only() {
        let context = eframe::egui::Context::default();
        let mut app = DicronApp::new(&context, vec![missing_startup_path("once")]);
        assert_eq!(app.error_message, None, "nothing opens before a frame");

        run_logic_pass(&context, &mut app);
        assert_eq!(
            app.error_message.as_deref(),
            Some("The selected DICOM path is no longer available.")
        );

        app.error_message = None;
        run_logic_pass(&context, &mut app);
        run_logic_pass(&context, &mut app);
        assert_eq!(app.error_message, None, "the launch path is not reopened");
    }

    /// Without launch paths, frames neither open anything nor report an error.
    #[test]
    fn frames_without_startup_paths_leave_the_app_idle() {
        let context = eframe::egui::Context::default();
        let mut app = DicronApp::new(&context, Vec::new());

        run_logic_pass(&context, &mut app);
        run_logic_pass(&context, &mut app);

        assert_eq!(app.error_message, None);
        assert!(app.open_source.is_none());
        assert!(app.dicom_index.is_none());
    }

    /// An empty command-line argument is not a path: the first frame says no
    /// readable input was given.
    #[test]
    fn an_empty_startup_argument_reports_no_readable_input() {
        let context = eframe::egui::Context::default();
        let mut app = DicronApp::new(&context, vec![std::path::PathBuf::new()]);

        run_logic_pass(&context, &mut app);

        assert_eq!(
            app.error_message.as_deref(),
            Some("No readable DICOM files or folders were provided.")
        );
        assert!(app.open_source.is_none());
    }

    /// Several launch paths are opened together on the first frame, as a
    /// drop of the same paths would be, empty arguments left out.
    #[test]
    fn several_startup_paths_open_together_on_the_first_frame() {
        let context = eframe::egui::Context::default();
        let first = missing_startup_path("first");
        let second = missing_startup_path("second");
        let mut app = DicronApp::new(
            &context,
            vec![first.clone(), std::path::PathBuf::new(), second.clone()],
        );
        assert!(app.open_source.is_none(), "nothing opens before a frame");

        run_logic_pass(&context, &mut app);

        match &app.open_source {
            Some(state::OpenSource::Inputs(paths)) => assert_eq!(paths, &[first, second]),
            _ => panic!("the launch paths are opened as one set of inputs"),
        }
        assert_eq!(app.error_message, None);
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
