//! Window/level UI interaction and per-series application state.

use eframe::egui;

use crate::app::DicronApp;
use crate::app::state::{SavedWindowLevel, SeriesKey, WindowLevel, WindowLevelState, WindowPreset};
use crate::dicom::DicomWindow;
use crate::theme;

const EDIT_CENTER_RANGE: std::ops::RangeInclusive<f64> = -100_000.0..=100_000.0;
const EDIT_WIDTH_RANGE: std::ops::RangeInclusive<f64> = 1.0..=200_000.0;

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
        available: bool,
        active_preset: Option<WindowPreset>,
    ) {
        self.default = default;
        self.current = current;
        self.value_range = value_range;
        self.customized = customized;
        self.available = available;
        self.active_preset = if customized {
            active_preset
        } else {
            Some(WindowPreset::Default)
        };
    }

    pub(in crate::app) fn is_available(&self) -> bool {
        self.available
    }

    pub(in crate::app) fn active_preset_label(&self) -> &'static str {
        self.active_preset.map_or("Custom", WindowPreset::label)
    }

    fn reset_current(&mut self) {
        self.current = self.default;
        self.customized = false;
        self.active_preset = Some(WindowPreset::Default);
    }

    fn adjust(&mut self, center_delta: f64, width_delta: f64) {
        let (minimum, maximum) = self.value_range;
        let span = (maximum - minimum).max(1.0);
        self.current.center =
            (self.current.center + center_delta).clamp(minimum - span, maximum + span);
        self.current.width = (self.current.width + width_delta).clamp(1.0, span * 4.0);
        self.customized = true;
        self.active_preset = None;
    }

    fn set_current(&mut self, window: WindowLevel, active_preset: Option<WindowPreset>) -> bool {
        if !window.center.is_finite() || !window.width.is_finite() || window.width <= 0.0 {
            return false;
        }

        self.current = window;
        self.customized = true;
        self.active_preset = active_preset;
        true
    }

    pub(in crate::app) fn clear_for_new_document(&mut self) {
        self.customized = false;
        self.available = false;
        self.active_preset = Some(WindowPreset::Default);
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
        if !self.window_level.is_available() {
            return;
        }

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
        self.window_level.by_series.insert(
            series_key,
            SavedWindowLevel {
                window: self.window_level.current,
                preset: self.window_level.active_preset,
            },
        );
    }

    pub(super) fn clear_current_series_window_level(&mut self) {
        let Some(series_key) = self.current_series_key() else {
            return;
        };
        self.window_level.by_series.remove(&series_key);
    }

    pub(in crate::app) fn current_series_window_level(&self) -> Option<SavedWindowLevel> {
        self.window_level
            .by_series
            .get(&self.current_series_key()?)
            .copied()
    }

    pub(in crate::app) fn current_series_key(&self) -> Option<SeriesKey> {
        self.selected_slice.map(|selection| selection.series_key())
    }

    pub(in crate::app) fn apply_window_preset(
        &mut self,
        context: &egui::Context,
        preset: WindowPreset,
    ) {
        if !self.window_level.is_available() {
            return;
        }

        if preset == WindowPreset::Default {
            self.reset_window_level(context);
            return;
        }

        let requested_window = match preset {
            WindowPreset::FullDynamic => self.current_full_dynamic_window(),
            _ => preset.fixed_window(),
        };

        let Some(requested_window) = requested_window else {
            self.error_message = Some(format!(
                "Could not apply the {} window preset to this image.",
                preset.label()
            ));
            return;
        };

        self.apply_custom_window_level(context, requested_window, Some(preset));
    }

    fn current_full_dynamic_window(&mut self) -> Option<WindowLevel> {
        let (path, frame_index) = self.current_frame_key.clone()?;
        let window = self
            .decoded_cache
            .get(&path, frame_index)?
            .frame
            .full_dynamic_window()?;

        Some(WindowLevel {
            center: window.center,
            width: window.width,
        })
    }

    fn apply_custom_window_level(
        &mut self,
        context: &egui::Context,
        window: WindowLevel,
        active_preset: Option<WindowPreset>,
    ) {
        if !self.window_level.set_current(window, active_preset) {
            self.error_message = Some(
                "Window level and width must be finite, and width must be greater than zero."
                    .to_owned(),
            );
            return;
        }

        self.save_current_series_window_level();
        self.refresh_dicom_texture(context);
    }

    pub(in crate::app) fn open_edit_windowing_dialog(&mut self) {
        if !self.window_level.is_available() {
            return;
        }

        let current = self.window_level.current();
        self.edit_windowing_dialog.center = current.center;
        self.edit_windowing_dialog.width = current.width;
        self.edit_windowing_dialog.open = true;
    }

    pub(in crate::app) fn show_edit_windowing_dialog(&mut self, context: &egui::Context) {
        if !self.edit_windowing_dialog.open {
            return;
        }

        let mut requested_window = None;
        let modal_response =
            egui::Modal::new(egui::Id::new("edit_windowing_dialog")).show(context, |ui| {
                ui.set_min_width(300.0);
                ui.heading("Edit Windowing");
                ui.separator();

                egui::Grid::new("edit_windowing_values")
                    .num_columns(2)
                    .spacing(egui::vec2(theme::SPACE_LG, theme::SPACE_SM))
                    .show(ui, |ui| {
                        ui.label("Window Level");
                        ui.add(
                            egui::DragValue::new(&mut self.edit_windowing_dialog.center)
                                .range(EDIT_CENTER_RANGE)
                                .speed(1.0),
                        );
                        ui.end_row();

                        ui.label("Window Width");
                        ui.add(
                            egui::DragValue::new(&mut self.edit_windowing_dialog.width)
                                .range(EDIT_WIDTH_RANGE)
                                .speed(1.0),
                        );
                        ui.end_row();
                    });

                ui.add_space(theme::SPACE_SM);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        ui.close();
                    }

                    if ui.button("Apply").clicked() {
                        requested_window = Some(WindowLevel {
                            center: self.edit_windowing_dialog.center,
                            width: self.edit_windowing_dialog.width,
                        });
                        ui.close();
                    }
                });
            });

        if modal_response.should_close() {
            self.edit_windowing_dialog.open = false;
        }

        if let Some(requested_window) = requested_window {
            self.apply_custom_window_level(context, requested_window, None);
        }
    }

    pub(in crate::app) fn reset_window_level(&mut self, context: &egui::Context) {
        self.window_level.reset_current();
        self.clear_current_series_window_level();
        self.refresh_dicom_texture(context);
    }
}

#[cfg(test)]
mod tests {
    use super::{WindowLevel, WindowLevelState, WindowPreset};

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
            true,
            None,
        );
        state.adjust(1_000.0, -1_000.0);
        assert_eq!(state.current.center, 200.0);
        assert_eq!(state.current.width, 1.0);
        assert!(state.customized);
        assert_eq!(state.active_preset_label(), "Custom");
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
            true,
            Some(WindowPreset::Lung),
        );
        assert_eq!(state.active_preset_label(), "Lung");
        state.reset_current();
        assert_eq!(state.current.center, default.center);
        assert_eq!(state.current.width, default.width);
        assert!(!state.customized);
        assert_eq!(state.active_preset_label(), "Default");
    }

    #[test]
    fn fixed_presets_match_the_reference_values() {
        assert_eq!(
            WindowPreset::Lung.fixed_window(),
            Some(WindowLevel {
                center: -400.0,
                width: 1600.0,
            })
        );
        assert_eq!(
            WindowPreset::Felsenbein.fixed_window(),
            Some(WindowLevel {
                center: 500.0,
                width: 4000.0,
            })
        );
        assert_eq!(WindowPreset::Default.fixed_window(), None);
        assert_eq!(WindowPreset::FullDynamic.fixed_window(), None);
    }

    #[test]
    fn custom_window_requires_finite_values_and_positive_width() {
        let mut state = WindowLevelState::default();

        assert!(!state.set_current(
            WindowLevel {
                center: f64::NAN,
                width: 100.0,
            },
            None,
        ));
        assert!(!state.set_current(
            WindowLevel {
                center: 10.0,
                width: 0.0,
            },
            None,
        ));
        assert!(state.set_current(
            WindowLevel {
                center: 10.0,
                width: 50.0,
            },
            None,
        ));
        assert_eq!(
            state.current,
            WindowLevel {
                center: 10.0,
                width: 50.0,
            }
        );
        assert!(state.customized);
        assert_eq!(state.active_preset_label(), "Custom");
    }
}
