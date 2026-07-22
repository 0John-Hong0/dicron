//! Window/level UI interaction and per-series application state.

use eframe::egui;

use crate::app::DicronApp;
use crate::app::state::{SeriesKey, WindowLevel, WindowLevelState};
use crate::dicom::DicomWindow;

impl WindowLevelState {
    pub(in crate::app) fn current(&self) -> WindowLevel {
        self.current
    }

    pub(in crate::app) fn apply_loaded_frame(
        &mut self,
        default: WindowLevel,
        current: WindowLevel,
        value_range: (f64, f64),
        customized: bool,
    ) {
        self.default = default;
        self.current = current;
        self.value_range = value_range;
        self.customized = customized;
    }

    fn reset_current(&mut self) {
        self.current = self.default;
        self.customized = false;
    }

    fn adjust(&mut self, center_delta: f64, width_delta: f64) {
        let (minimum, maximum) = self.value_range;
        let span = (maximum - minimum).max(1.0);
        self.current.center =
            (self.current.center + center_delta).clamp(minimum - span, maximum + span);
        self.current.width = (self.current.width + width_delta).clamp(1.0, span * 4.0);
        self.customized = true;
    }

    pub(in crate::app) fn clear_for_new_document(&mut self) {
        self.customized = false;
        self.by_series.clear();
    }
}

impl DicronApp {
    pub(in crate::app) fn effective_window(&self) -> Option<DicomWindow> {
        self.window_level.customized.then_some(DicomWindow {
            center: self.window_level.current.center,
            width: self.window_level.current.width,
        })
    }

    pub(super) fn handle_window_level_drag(
        &mut self,
        context: &egui::Context,
        response: &egui::Response,
    ) {
        let drag_motion = response.drag_motion();
        if drag_motion == egui::Vec2::ZERO {
            return;
        }

        self.window_level
            .adjust(drag_motion.y as f64, drag_motion.x as f64);
        self.save_current_series_window_level();
        self.refresh_dicom_texture(context);
    }

    pub(super) fn save_current_series_window_level(&mut self) {
        let Some(series_key) = self.current_series_key() else {
            return;
        };
        self.window_level
            .by_series
            .insert(series_key, self.window_level.current);
    }

    pub(super) fn clear_current_series_window_level(&mut self) {
        let Some(series_key) = self.current_series_key() else {
            return;
        };
        self.window_level.by_series.remove(&series_key);
    }

    pub(in crate::app) fn current_series_window_level(&self) -> Option<WindowLevel> {
        self.window_level
            .by_series
            .get(&self.current_series_key()?)
            .copied()
    }

    pub(in crate::app) fn current_series_key(&self) -> Option<SeriesKey> {
        self.selected_slice.map(|selection| selection.series_key())
    }

    pub(in crate::app) fn reset_window_level(&mut self, context: &egui::Context) {
        self.window_level.reset_current();
        self.clear_current_series_window_level();
        self.refresh_dicom_texture(context);
    }
}

#[cfg(test)]
mod tests {
    use super::{WindowLevel, WindowLevelState};

    #[test]
    fn adjustment_clamps_width_and_center_to_the_frame_range() {
        let mut state = WindowLevelState::default();
        state.apply_loaded_frame(
            WindowLevel {
                center: 50.0,
                width: 100.0,
            },
            WindowLevel {
                center: 50.0,
                width: 100.0,
            },
            (0.0, 100.0),
            false,
        );
        state.adjust(1_000.0, -1_000.0);
        assert_eq!(state.current.center, 200.0);
        assert_eq!(state.current.width, 1.0);
        assert!(state.customized);
    }

    #[test]
    fn reset_restores_the_loaded_default() {
        let mut state = WindowLevelState::default();
        let default = WindowLevel {
            center: 40.0,
            width: 80.0,
        };
        state.apply_loaded_frame(
            default,
            WindowLevel {
                center: 10.0,
                width: 20.0,
            },
            (0.0, 100.0),
            true,
        );
        state.reset_current();
        assert_eq!(state.current.center, default.center);
        assert_eq!(state.current.width, default.width);
        assert!(!state.customized);
    }
}
