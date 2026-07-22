//! Application operations initiated by the user or UI.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use rfd::FileDialog;

use crate::app::DicronApp;
use crate::app::frame_cache::DecodedCacheEntry;
use crate::app::state::{
    PLAYBACK_MAX_FPS, PLAYBACK_MIN_FPS, PlaybackLoopMode, SliceSelection, WindowLevel,
};
use crate::app::ui::upload_display_pixels;
use crate::dicom::{
    DicomWindow, SliceItem, build_for_file, is_candidate_path, load_dicom_frame, render_frame,
};

impl DicronApp {
    pub(crate) fn open_startup_paths(
        &mut self,
        context: &egui::Context,
        startup_paths: Vec<PathBuf>,
    ) {
        if startup_paths.is_empty() {
            return;
        }

        let accepted_paths = filter_accepted_dropped_paths(startup_paths);

        if accepted_paths.is_empty() {
            self.error_message =
                Some("No readable DICOM files or folders were provided.".to_owned());
            return;
        }

        if let [startup_path] = accepted_paths.as_slice() {
            let startup_path = startup_path.clone();

            if startup_path.is_dir() {
                self.open_dicom_folder_path(context, startup_path);
                return;
            }

            if startup_path.is_file() {
                self.open_dicom_file_path(context, startup_path);
                return;
            }
        }

        self.open_dropped_dicom_inputs(context, accepted_paths);
    }

    pub(super) fn open_dicom_file(&mut self, context: &egui::Context) {
        let mut file_dialog = FileDialog::new()
            .set_title("Open DICOM")
            .add_filter("DICOM", &["dcm", "dicom"]);

        if let Some(open_dicom_directory) = &self.settings.open_dicom_directory {
            file_dialog = file_dialog.set_directory(open_dicom_directory);
        }

        let Some(selected_dicom_path) = file_dialog.pick_file() else {
            return;
        };

        self.open_dicom_file_path(context, selected_dicom_path);
    }

    pub(super) fn open_dicom_file_path(
        &mut self,
        context: &egui::Context,
        selected_dicom_path: PathBuf,
    ) {
        self.settings.remember_open_dicom_path(&selected_dicom_path);

        self.cancel_active_scan();
        self.clear_loaded_dicom_state();
        self.clear_scan();

        match build_for_file(&selected_dicom_path) {
            Ok(dicom_index) => {
                self.dicom_index = Some(dicom_index);
                self.error_message = None;
                self.load_first_available_slice(context);
            }
            Err(error) => {
                self.error_message = Some(format!("Failed to index DICOM: {error:#}"));
            }
        }
    }

    pub(super) fn open_dicom_folder(&mut self, context: &egui::Context) {
        let mut file_dialog = FileDialog::new().set_title("Open DICOM Folder");

        if let Some(open_folder_directory) = &self.settings.open_folder_directory {
            file_dialog = file_dialog.set_directory(open_folder_directory);
        }

        let Some(selected_folder_path) = file_dialog.pick_folder() else {
            return;
        };

        self.open_dicom_folder_path(context, selected_folder_path);
    }

    pub(super) fn handle_dropped_paths(&mut self, context: &egui::Context) {
        let dropped_paths: Vec<PathBuf> = context.input(|input_state| {
            input_state
                .raw
                .dropped_files
                .iter()
                .filter_map(|dropped_file| dropped_file.path.clone())
                .collect()
        });

        if dropped_paths.is_empty() {
            return;
        }

        let accepted_paths = filter_accepted_dropped_paths(dropped_paths);

        if accepted_paths.is_empty() {
            self.error_message = Some("Drop DICOM files or folders only.".to_owned());
            return;
        }

        if let [dropped_path] = accepted_paths.as_slice() {
            let dropped_path = dropped_path.clone();

            if dropped_path.is_dir() {
                self.open_dicom_folder_path(context, dropped_path);
                return;
            }

            if dropped_path.is_file() {
                self.open_dicom_file_path(context, dropped_path);
                return;
            }
        }

        self.open_dropped_dicom_inputs(context, accepted_paths);
    }
}

fn filter_accepted_dropped_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|path| path.is_dir() || is_candidate_path(path))
        .collect()
}

impl DicronApp {
    pub(super) fn load_first_available_slice(&mut self, context: &egui::Context) {
        let first_available_indices = {
            let Some(dicom_index) = &self.dicom_index else {
                return;
            };
            let mut found = None;

            'search: for (patient_index, patient) in dicom_index.patients.iter().enumerate() {
                for (study_index, study) in patient.studies.iter().enumerate() {
                    for (series_index, series) in study.series_groups.iter().enumerate() {
                        if !series.slices.is_empty() {
                            found = Some((patient_index, study_index, series_index, 0));
                            break 'search;
                        }
                    }
                }
            }
            found
        };

        if let Some((patient, study, series, slice)) = first_available_indices {
            self.load_slice_by_indices(context, patient, study, series, slice);
        }
    }

    pub(super) fn load_slice_by_indices(
        &mut self,
        context: &egui::Context,
        patient_index: usize,
        study_index: usize,
        series_index: usize,
        slice_index: usize,
    ) {
        let Some(slice_item) =
            self.get_slice_by_indices(patient_index, study_index, series_index, slice_index)
        else {
            return;
        };

        self.selected_slice = Some(SliceSelection::new(
            patient_index,
            study_index,
            series_index,
            slice_index,
        ));
        self.load_dicom_path(context, slice_item.path, slice_item.frame_index);
    }

    pub(super) fn get_slice_by_indices(
        &self,
        patient_index: usize,
        study_index: usize,
        series_index: usize,
        slice_index: usize,
    ) -> Option<SliceItem> {
        let patient = self.dicom_index.as_ref()?.patients.get(patient_index)?;
        let study = patient.studies.get(study_index)?;
        let series = study.series_groups.get(series_index)?;
        series.slices.get(slice_index).cloned()
    }

    pub(super) fn get_selected_series_slice_count(&self) -> Option<usize> {
        let selection = self.selected_slice?;
        let patient = self
            .dicom_index
            .as_ref()?
            .patients
            .get(selection.patient_index)?;
        let study = patient.studies.get(selection.study_index)?;
        let series = study.series_groups.get(selection.series_index)?;
        Some(series.slices.len())
    }

    pub(super) fn current_slice_index(&self) -> Option<usize> {
        if let Some(selection) = self.selected_slice {
            Some(selection.slice_index)
        } else if self.selected_dicom_path.is_some() && self.selected_dicom_frame_count > 1 {
            Some(self.selected_dicom_frame_index as usize)
        } else {
            None
        }
    }

    pub(super) fn current_slice_count(&self) -> Option<usize> {
        self.get_selected_series_slice_count().or_else(|| {
            if self.selected_dicom_path.is_some() && self.selected_dicom_frame_count > 1 {
                Some(self.selected_dicom_frame_count as usize)
            } else {
                None
            }
        })
    }

    pub(super) fn selected_indices(&self) -> Option<SliceSelection> {
        self.selected_slice
    }

    pub(super) fn clear_selected_indices(&mut self) {
        self.stop_autoplay();
        self.selected_slice = None;
        self.selected_dicom_frame_index = 0;
        self.selected_dicom_frame_count = 1;
        self.viewer_scroll_accumulator = 0.0;
    }
}

const SCROLL_SLICE_STEP: f32 = 40.0;
const PAGE_SLICE_STEP: isize = 10;

impl DicronApp {
    pub(super) fn handle_viewer_scroll(&mut self, context: &egui::Context, ui: &egui::Ui) {
        let scroll_delta_y = ui.input(|input_state| input_state.smooth_scroll_delta.y);

        if scroll_delta_y == 0.0 {
            return;
        }

        self.viewer_scroll_accumulator += scroll_delta_y;
        let wheel_steps = (self.viewer_scroll_accumulator / SCROLL_SLICE_STEP).trunc() as isize;

        if wheel_steps == 0 {
            return;
        }

        self.viewer_scroll_accumulator -= wheel_steps as f32 * SCROLL_SLICE_STEP;
        self.move_selected_slice(context, -wheel_steps);
    }

    pub(in crate::app) fn handle_keyboard_shortcuts(&mut self, context: &egui::Context) {
        if context.egui_wants_keyboard_input() {
            return;
        }

        let keyboard_action = context.input_mut(|input_state| {
            let mut slice_delta = 0;

            slice_delta += input_state
                .count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                as isize;
            slice_delta += input_state
                .count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight)
                as isize;
            slice_delta -= input_state
                .count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                as isize;
            slice_delta -= input_state
                .count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft)
                as isize;
            slice_delta += input_state
                .count_and_consume_key(egui::Modifiers::NONE, egui::Key::PageDown)
                as isize
                * PAGE_SLICE_STEP;
            slice_delta -= input_state
                .count_and_consume_key(egui::Modifiers::NONE, egui::Key::PageUp)
                as isize
                * PAGE_SLICE_STEP;

            let jump_to_start = input_state.consume_key(egui::Modifiers::NONE, egui::Key::Home);
            let jump_to_end = input_state.consume_key(egui::Modifiers::NONE, egui::Key::End);

            (slice_delta, jump_to_start, jump_to_end)
        });

        let (slice_delta, jump_to_start, jump_to_end) = keyboard_action;

        if jump_to_start {
            self.viewer_scroll_accumulator = 0.0;
            self.jump_to_slice(context, 0);
            return;
        }

        if jump_to_end
            && let Some(slice_count) = self.current_slice_count()
            && slice_count > 0
        {
            self.viewer_scroll_accumulator = 0.0;
            self.jump_to_slice(context, slice_count - 1);
            return;
        }

        if slice_delta != 0 {
            self.viewer_scroll_accumulator = 0.0;
            self.move_selected_slice(context, slice_delta);
        }
    }

    pub(super) fn move_selected_slice(&mut self, context: &egui::Context, direction: isize) {
        if let Some(selection) = self.selected_slice {
            let Some(series_slice_count) = self.get_selected_series_slice_count() else {
                return;
            };

            if series_slice_count == 0 {
                return;
            }

            let max_slice_index = (series_slice_count - 1) as isize;
            let next_slice_index =
                (selection.slice_index as isize + direction).clamp(0, max_slice_index) as usize;

            if next_slice_index == selection.slice_index {
                return;
            }

            self.load_slice_by_indices(
                context,
                selection.patient_index,
                selection.study_index,
                selection.series_index,
                next_slice_index,
            );
            return;
        }

        self.move_selected_file_frame(context, direction);
    }

    pub(super) fn move_selected_file_frame(&mut self, context: &egui::Context, direction: isize) {
        let Some(selected_dicom_path) = self.selected_dicom_path.clone() else {
            return;
        };

        if self.selected_dicom_frame_count <= 1 {
            return;
        }

        let max_frame_index = (self.selected_dicom_frame_count - 1) as isize;
        let next_frame_index =
            (self.selected_dicom_frame_index as isize + direction).clamp(0, max_frame_index) as u32;

        if next_frame_index == self.selected_dicom_frame_index {
            return;
        }

        self.load_dicom_path(context, selected_dicom_path, next_frame_index);
    }

    pub(super) fn jump_to_slice(&mut self, context: &egui::Context, slice_index: usize) {
        if let Some(selection) = self.selected_slice {
            let Some(series_slice_count) = self.get_selected_series_slice_count() else {
                return;
            };

            if series_slice_count == 0 {
                return;
            }

            let next_slice_index = slice_index.min(series_slice_count - 1);
            if selection.slice_index == next_slice_index {
                return;
            }

            self.load_slice_by_indices(
                context,
                selection.patient_index,
                selection.study_index,
                selection.series_index,
                next_slice_index,
            );
            return;
        }

        let Some(selected_dicom_path) = self.selected_dicom_path.clone() else {
            return;
        };

        if self.selected_dicom_frame_count == 0 {
            return;
        }

        let next_frame_index =
            slice_index.min((self.selected_dicom_frame_count - 1) as usize) as u32;
        if next_frame_index == self.selected_dicom_frame_index {
            return;
        }

        self.load_dicom_path(context, selected_dicom_path, next_frame_index);
    }
}

impl DicronApp {
    pub(in crate::app) fn handle_autoplay(&mut self, context: &egui::Context) {
        if !self.playback.enabled {
            return;
        }

        let Some(slice_count) = self.current_slice_count() else {
            self.stop_autoplay();
            return;
        };

        if slice_count <= 1 {
            self.stop_autoplay();
            return;
        }

        self.playback.fps = self.playback.fps.clamp(PLAYBACK_MIN_FPS, PLAYBACK_MAX_FPS);

        let frame_interval = Duration::from_secs_f64(1.0 / self.playback.fps as f64);
        let now = Instant::now();
        let last_tick = self.playback.last_tick.unwrap_or(now);
        let elapsed = now.saturating_duration_since(last_tick);
        let next_repaint_after = if elapsed >= frame_interval {
            self.advance_autoplay(context, slice_count);
            self.playback.last_tick = Some(now);
            frame_interval
        } else {
            frame_interval.saturating_sub(elapsed)
        };

        context.request_repaint_after(next_repaint_after);
    }

    pub(super) fn advance_autoplay(&mut self, context: &egui::Context, slice_count: usize) {
        let Some(current_slice_index) = self.current_slice_index() else {
            self.stop_autoplay();
            return;
        };

        let Some(next_slice_index) = next_slice_index(
            current_slice_index,
            slice_count,
            self.playback.loop_mode,
            &mut self.playback.direction,
        ) else {
            self.stop_autoplay();
            return;
        };

        self.jump_to_slice(context, next_slice_index);
    }

    pub(super) fn start_autoplay(&mut self) {
        self.playback.enabled = true;
        self.playback.last_tick = Some(Instant::now());
    }

    pub(in crate::app) fn stop_autoplay(&mut self) {
        self.playback.enabled = false;
        self.playback.last_tick = None;
    }
}

fn next_slice_index(
    current_slice_index: usize,
    slice_count: usize,
    loop_mode: PlaybackLoopMode,
    direction: &mut isize,
) -> Option<usize> {
    if slice_count <= 1 {
        return None;
    }

    let max_slice_index = slice_count - 1;

    match loop_mode {
        PlaybackLoopMode::StopAtEnd => {
            (current_slice_index < max_slice_index).then_some(current_slice_index + 1)
        }
        PlaybackLoopMode::Loop => Some(if current_slice_index >= max_slice_index {
            0
        } else {
            current_slice_index + 1
        }),
        PlaybackLoopMode::PingPong => {
            let candidate = current_slice_index as isize + *direction;

            if candidate > max_slice_index as isize {
                *direction = -1;
                Some(max_slice_index.saturating_sub(1))
            } else if candidate < 0 {
                *direction = 1;
                Some(1.min(max_slice_index))
            } else {
                Some(candidate as usize)
            }
        }
    }
}

struct PreparedFrame {
    default_window: WindowLevel,
    current_window: WindowLevel,
    window_customized: bool,
    frame_count: u32,
    value_range: (f64, f64),
    metadata: crate::dicom::DicomMetadata,
    pixels: anyhow::Result<crate::dicom::DisplayPixels>,
}

impl DicronApp {
    pub(in crate::app) fn load_dicom_path(
        &mut self,
        context: &egui::Context,
        dicom_path: PathBuf,
        frame_index: u32,
    ) {
        if self.decoded_cache.get(&dicom_path, frame_index).is_none() {
            match load_dicom_frame(&dicom_path, frame_index) {
                Ok(loaded) => self.decoded_cache.insert(DecodedCacheEntry {
                    path: dicom_path.clone(),
                    frame_index,
                    frame: loaded.frame,
                    metadata: loaded.metadata,
                }),
                Err(error) => {
                    self.error_message = Some(format!("Failed to open DICOM: {error:#}"));
                    return;
                }
            }
        }

        let saved_window_level = self.current_series_window_level();
        let prepared = {
            let Some(entry) = self.decoded_cache.get(&dicom_path, frame_index) else {
                self.error_message = Some("Failed to retrieve decoded DICOM frame.".to_owned());
                return;
            };
            let (default_center, default_width) = entry.frame.default_center_width();
            let window_customized = saved_window_level.is_some();
            let center = saved_window_level.map_or(default_center, |window| window.center);
            let width = saved_window_level.map_or(default_width, |window| window.width);
            let effective_window = window_customized.then_some(DicomWindow { center, width });

            PreparedFrame {
                default_window: WindowLevel {
                    center: default_center,
                    width: default_width,
                },
                current_window: WindowLevel { center, width },
                window_customized,
                frame_count: entry.frame.frame_count,
                value_range: entry.frame.value_range,
                metadata: entry.metadata.clone(),
                pixels: render_frame(&entry.frame, effective_window),
            }
        };

        let pixels = match prepared.pixels {
            Ok(pixels) => pixels,
            Err(error) => {
                self.error_message = Some(format!("Failed to render DICOM: {error:#}"));
                return;
            }
        };

        self.window_level.apply_loaded_frame(
            prepared.default_window,
            prepared.current_window,
            prepared.value_range,
            prepared.window_customized,
        );
        self.selected_dicom_frame_index = frame_index;
        self.selected_dicom_frame_count = prepared.frame_count;
        self.loaded_texture = Some(upload_display_pixels(
            context,
            texture_name(&dicom_path),
            pixels,
        ));
        self.metadata.replace(prepared.metadata);
        self.current_frame_key = Some((dicom_path.clone(), frame_index));
        self.selected_dicom_path = Some(dicom_path);
        self.error_message = None;
    }

    pub(in crate::app) fn refresh_dicom_texture(&mut self, context: &egui::Context) {
        let Some((path, frame_index)) = self.current_frame_key.clone() else {
            return;
        };
        let effective_window = self.effective_window();
        let rendered = {
            let Some(entry) = self.decoded_cache.get(&path, frame_index) else {
                return;
            };
            render_frame(&entry.frame, effective_window)
        };

        match rendered {
            Ok(pixels) => {
                self.loaded_texture =
                    Some(upload_display_pixels(context, texture_name(&path), pixels));
                self.error_message = None;
            }
            Err(error) => {
                self.error_message = Some(format!("Failed to refresh DICOM: {error:#}"));
            }
        }
    }
}

fn texture_name(dicom_path: &std::path::Path) -> &str {
    dicom_path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or("loaded-dicom-image")
}

impl DicronApp {
    pub(in crate::app) fn clear_loaded_dicom_state(&mut self) {
        self.selected_dicom_path = None;
        self.loaded_texture = None;
        self.decoded_cache.clear();
        self.current_frame_key = None;
        self.window_level.clear_for_new_document();
        self.metadata.clear();
        self.dicom_index = None;
        self.clear_selected_indices();
    }
}

#[cfg(test)]
mod playback_tests {
    use super::{PlaybackLoopMode, next_slice_index};

    #[test]
    fn stop_at_end_stops_on_the_last_slice() {
        let mut direction = 1;

        assert_eq!(
            next_slice_index(1, 3, PlaybackLoopMode::StopAtEnd, &mut direction),
            Some(2)
        );
        assert_eq!(
            next_slice_index(2, 3, PlaybackLoopMode::StopAtEnd, &mut direction),
            None
        );
    }

    #[test]
    fn loop_wraps_to_the_first_slice() {
        let mut direction = 1;

        assert_eq!(
            next_slice_index(2, 3, PlaybackLoopMode::Loop, &mut direction),
            Some(0)
        );
    }

    #[test]
    fn ping_pong_reverses_at_each_end() {
        let mut direction = 1;

        assert_eq!(
            next_slice_index(2, 3, PlaybackLoopMode::PingPong, &mut direction),
            Some(1)
        );
        assert_eq!(direction, -1);
        assert_eq!(
            next_slice_index(0, 3, PlaybackLoopMode::PingPong, &mut direction),
            Some(1)
        );
        assert_eq!(direction, 1);
    }
}
