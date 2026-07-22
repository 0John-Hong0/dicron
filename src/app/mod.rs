//! Top-level application composition and the eframe entry point.

mod actions;
mod background_tasks;
mod frame_cache;
mod state;
mod ui;

pub(crate) use state::DicronApp;

impl eframe::App for DicronApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        ui::show(self, ui, frame);
    }
}
