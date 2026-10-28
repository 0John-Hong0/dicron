//! Application operations initiated by the user or UI.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use rfd::FileDialog;

use crate::app::DicronApp;
use crate::app::frame_cache::DecodedCacheEntry;
use crate::app::state::{
    ActiveVolumeView, LowResolutionPrompt, OpenSource, PLAYBACK_MAX_FPS, PLAYBACK_MIN_FPS,
    PlaybackLoopMode, ReopenViewState, SavedWindowLevel, SeriesKey, SliceSelection, ThreeDMode,
    ViewMode, WINDOW_PRESETS, WindowLevel, WindowPreset,
};
use crate::app::three_d_state::{
    GpuBudget, GpuVolume, MprQuality, MprState, TransferPreset, VolumeRenderState,
};
use crate::app::ui::{GPU_UNAVAILABLE_MESSAGE, THREE_D_UNSUPPORTED_MESSAGE, upload_display_pixels};
use crate::app::volume_job::{
    PreparedVolume, VolumeJobKind, VolumeJobOutput, VolumeJobRequest, series_fingerprint,
    spatially_ordered_slices,
};
#[cfg(test)]
use crate::dicom::build_for_file;
use crate::dicom::{
    DicomWindow, SeriesVolume, SliceItem, TextEncoding, Volume, VolumeAxis,
    build_for_file_with_encoding, load_dicom_frame_with_encoding, render_frame,
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

        let accepted_paths = filter_nonempty_paths(startup_paths);

        if accepted_paths.is_empty() {
            self.error_message =
                Some("No readable DICOM files or folders were provided.".to_owned());
            return;
        }

        if let [startup_path] = accepted_paths.as_slice() {
            let startup_path = startup_path.clone();
            let Ok(metadata) = startup_path.metadata() else {
                self.error_message =
                    Some("The selected DICOM path is no longer available.".to_owned());
                return;
            };

            if metadata.is_dir() {
                self.open_dicom_folder_path(context, startup_path);
                return;
            }

            if metadata.is_file() {
                self.open_dicom_file_path(context, startup_path);
                return;
            }
        }

        self.open_dropped_dicom_inputs(context, accepted_paths);
    }

    pub(super) fn open_dicom_file(&mut self, context: &egui::Context) {
        let mut file_dialog = FileDialog::new().set_title("Open DICOM");

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
        self.open_source = Some(OpenSource::File(selected_dicom_path.clone()));
        self.text_encoding = TextEncoding::Auto;
        self.reopen_selection = None;
        self.reopen_view_state = None;
        self.open_dicom_file_path_with_encoding(context, selected_dicom_path);
    }

    fn open_dicom_file_path_with_encoding(
        &mut self,
        context: &egui::Context,
        selected_dicom_path: PathBuf,
    ) {
        self.cancel_active_scan();
        self.clear_loaded_dicom_state();
        self.clear_scan();

        match build_for_file_with_encoding(&selected_dicom_path, self.text_encoding) {
            Ok(dicom_index) => {
                self.dicom_index = Some(dicom_index);
                self.error_message = None;
                let preferred = self.reopen_selection.take();
                self.load_preferred_slice(context, preferred.as_ref());
            }
            Err(error) => {
                self.reopen_selection = None;
                self.reopen_view_state = None;
                self.error_message = Some(format!("Failed to index DICOM: {error:#}"));
            }
        }
    }

    pub(super) fn reopen_with_encoding(
        &mut self,
        context: &egui::Context,
        text_encoding: TextEncoding,
    ) {
        let Some(source) = self.open_source.clone() else {
            return;
        };
        self.reopen_selection = self
            .selected_dicom_path
            .clone()
            .map(|path| (path, self.selected_dicom_frame_index));
        self.reopen_view_state = self.reopen_selection.as_ref().map(|_| ReopenViewState {
            viewport_transform: self.viewport_transform,
            window_level: self.window_level_for_selection(self.selected_slice),
        });
        self.text_encoding = text_encoding;

        match source {
            OpenSource::File(path) => self.open_dicom_file_path_with_encoding(context, path),
            OpenSource::Folder(path) => self.start_dicom_folder_scan(context, path),
            OpenSource::Inputs(paths) => self.start_dicom_inputs_scan(context, paths),
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

        let accepted_paths = filter_nonempty_paths(dropped_paths);

        if accepted_paths.is_empty() {
            self.error_message = Some("Drop DICOM files or folders only.".to_owned());
            return;
        }

        if let [dropped_path] = accepted_paths.as_slice() {
            let dropped_path = dropped_path.clone();
            let Ok(metadata) = dropped_path.metadata() else {
                self.error_message =
                    Some("The dropped DICOM path is no longer available.".to_owned());
                return;
            };

            if metadata.is_dir() {
                self.open_dicom_folder_path(context, dropped_path);
                return;
            }

            if metadata.is_file() {
                self.open_dicom_file_path(context, dropped_path);
                return;
            }
        }

        self.open_dropped_dicom_inputs(context, accepted_paths);
    }
}

fn filter_nonempty_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

impl DicronApp {
    pub(in crate::app) fn load_preferred_slice(
        &mut self,
        context: &egui::Context,
        preferred: Option<&(PathBuf, u32)>,
    ) {
        let preferred_indices = self.dicom_index.as_ref().and_then(|index| {
            let (path, frame_index) = preferred?;
            for (patient_index, patient) in index.patients.iter().enumerate() {
                for (study_index, study) in patient.studies.iter().enumerate() {
                    for (series_index, series) in study.series_groups.iter().enumerate() {
                        for (slice_index, slice) in series.slices.iter().enumerate() {
                            if &slice.path == path && slice.frame_index == *frame_index {
                                return Some(SliceSelection::new(
                                    patient_index,
                                    study_index,
                                    series_index,
                                    slice_index,
                                ));
                            }
                        }
                    }
                }
            }
            None
        });

        let reopen_view_state = self
            .reopen_view_state
            .take()
            .filter(|_| preferred_indices.is_some());
        let selection = preferred_indices.or_else(|| self.first_available_slice_selection());
        let Some(selection) = selection else {
            return;
        };
        let Some(slice) = self.get_slice_by_indices(
            selection.patient_index,
            selection.study_index,
            selection.series_index,
            selection.slice_index,
        ) else {
            return;
        };
        if let Some(window_level) = reopen_view_state.and_then(|state| state.window_level) {
            self.window_level
                .by_series
                .insert(selection.series_key(), window_level);
        }
        self.start_frame_load(context, slice, selection, reopen_view_state);
    }

    fn first_available_slice_selection(&self) -> Option<SliceSelection> {
        let dicom_index = self.dicom_index.as_ref()?;
        for (patient_index, patient) in dicom_index.patients.iter().enumerate() {
            for (study_index, study) in patient.studies.iter().enumerate() {
                for (series_index, series) in study.series_groups.iter().enumerate() {
                    if !series.slices.is_empty() {
                        return Some(SliceSelection::new(
                            patient_index,
                            study_index,
                            series_index,
                            0,
                        ));
                    }
                }
            }
        }
        None
    }

    pub(super) fn load_slice_by_indices(
        &mut self,
        context: &egui::Context,
        patient_index: usize,
        study_index: usize,
        series_index: usize,
        slice_index: usize,
    ) -> bool {
        let Some(slice_item) =
            self.get_slice_by_indices(patient_index, study_index, series_index, slice_index)
        else {
            return false;
        };

        let target_selection =
            SliceSelection::new(patient_index, study_index, series_index, slice_index);

        self.load_dicom_path(
            context,
            slice_item.path,
            slice_item.frame_index,
            Some(target_selection),
        )
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
        self.selected_series_slices().map(<[SliceItem]>::len)
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
        self.viewer_scroll_slice_remainder = 0.0;
    }
}

// Fractional deltas that should total one slice can sum to a hair under it, which would
// otherwise truncate to no step at all.
const SCROLL_SLICE_EPSILON: f32 = 1.0e-3;
const MAX_SCROLL_SLICES_PER_FRAME: isize = 8;
const PAGE_SLICE_STEP: isize = 10;

/// Drain this frame's plain wheel input into whole slice steps, one notch per
/// slice. Wheel down advances, so every surface that steps through slices
/// spends the wheel the same way.
///
/// Deliberately reads raw wheel events instead of `smooth_scroll_delta`: egui low-pass
/// filters wheel input over roughly 0.18 s, which delays a discrete slice step by the whole
/// filter tail and makes it depend on where the filtered sum lands against the threshold.
pub(in crate::app) fn consume_wheel_slice_steps(
    context: &egui::Context,
    remainder: &mut f32,
) -> isize {
    // Read before `input_mut`: both take the same non-reentrant lock.
    let (egui_wheel_modifiers, line_scroll_speed) = context.options(|options| {
        (
            options.input_options.zoom_modifier | options.input_options.horizontal_scroll_modifier,
            options.input_options.line_scroll_speed,
        )
    });
    let scrolled_slices = context.input_mut(|input_state| {
        consume_viewer_wheel_slices(input_state, egui_wheel_modifiers, line_scroll_speed)
    });

    // Scrolling moves the content, so content moving down reveals the previous slice.
    -take_whole_slice_steps(remainder, scrolled_slices)
}

impl DicronApp {
    pub(in crate::app) fn reset_view(&mut self, context: &egui::Context) {
        self.viewport_transform = Default::default();
        self.viewport_zoom_anchor = None;
        self.reset_window_level(context);
    }

    pub(in crate::app) fn flip_viewer_horizontal(&mut self) {
        self.viewport_transform.flip_horizontal = !self.viewport_transform.flip_horizontal;
    }

    pub(in crate::app) fn flip_viewer_vertical(&mut self) {
        self.viewport_transform.flip_vertical = !self.viewport_transform.flip_vertical;
    }

    pub(in crate::app) fn rotate_viewer_clockwise(&mut self) {
        self.viewport_transform.rotation_quarters =
            (self.viewport_transform.rotation_quarters + 1) % 4;
    }

    pub(super) fn handle_viewer_scroll(
        &mut self,
        context: &egui::Context,
        pointer_over_viewer: bool,
    ) {
        if !pointer_over_viewer {
            self.viewer_scroll_slice_remainder = 0.0;
            return;
        }

        let stepped_slices =
            consume_wheel_slice_steps(context, &mut self.viewer_scroll_slice_remainder);

        if stepped_slices != 0 {
            self.move_selected_slice(context, stepped_slices);
        }
    }

    pub(in crate::app) fn handle_keyboard_shortcuts(&mut self, context: &egui::Context) {
        // Anything that owns the keyboard - a modal, a focused widget, or one
        // that Escape has just released - keeps both the shortcuts and Escape
        // for itself.
        if self.edit_windowing_dialog.open
            || self.about_dialog.open
            || self.three_d.low_resolution_prompt.is_some()
            || context.egui_wants_keyboard_input()
            || self.keyboard_owned_last_frame
        {
            return;
        }

        if self.three_d.view_mode.three_d_mode().is_some() {
            // An open menu takes Escape to close itself, not to leave 3D.
            if !egui::Popup::is_any_open(context) {
                self.handle_three_d_keyboard_shortcuts(context);
            }
            return;
        }

        // Escape abandons a 3D view still being prepared, so it never opens
        // later by itself while the user reads on in 2D.
        if self.three_d.job.request().is_some()
            && context.input_mut(|input_state| {
                input_state.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
            })
        {
            self.three_d.job.cancel();
            return;
        }

        let preset_shortcuts_enabled = self.window_level.is_available();
        let (slice_keys, window_preset) = context.input_mut(|input_state| {
            (
                consume_slice_keys(input_state),
                consume_window_preset_keys(input_state, preset_shortcuts_enabled),
            )
        });

        if let Some(window_preset) = window_preset {
            self.apply_window_preset(context, window_preset);
        }

        if slice_keys.jump_to_start {
            self.viewer_scroll_slice_remainder = 0.0;
            self.jump_to_slice(context, 0);
            return;
        }

        if slice_keys.jump_to_end
            && let Some(slice_count) = self.current_slice_count()
            && slice_count > 0
        {
            self.viewer_scroll_slice_remainder = 0.0;
            self.jump_to_slice(context, slice_count - 1);
            return;
        }

        if slice_keys.slice_delta != 0 {
            self.viewer_scroll_slice_remainder = 0.0;
            self.move_selected_slice(context, slice_keys.slice_delta);
        }
    }

    /// The 3D workspace reads the same slice keys, but they step the plane
    /// that stacks head to foot. The digit presets stay a 2D shortcut: the
    /// planes are windowed from the control row instead.
    fn handle_three_d_keyboard_shortcuts(&mut self, context: &egui::Context) {
        let Some(mode) = self.three_d.view_mode.three_d_mode() else {
            return;
        };

        if context.input_mut(|input_state| {
            input_state.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
        }) {
            self.leave_three_d();
            return;
        }

        // Volume rendering shows the whole volume, so it has no slice to step.
        if mode == ThreeDMode::VolumeRendering {
            return;
        }

        let slice_keys = context.input_mut(consume_slice_keys);

        if slice_keys == SliceKeys::default() || !self.workspace_shows_selection() {
            return;
        }

        let Some(active) = &mut self.three_d.active else {
            return;
        };
        let geometry = &active.prepared.series.geometry;
        let axis = geometry.superior_inferior_axis();
        let last_index = active.mpr.volume.axis_len(axis).saturating_sub(1);
        // Down, Page Down and End head for the feet, whichever way the volume's
        // index runs.
        let towards_feet = geometry.reading_order_step(axis);
        let (head_end, feet_end) = if towards_feet > 0 {
            (0, last_index)
        } else {
            (last_index, 0)
        };

        active.mpr.clear_wheel_remainders();

        if slice_keys.jump_to_start {
            active.mpr.set_index(axis, head_end);
        } else if slice_keys.jump_to_end {
            active.mpr.set_index(axis, feet_end);
        } else {
            active
                .mpr
                .step_index(axis, slice_keys.slice_delta * towards_feet);
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

        self.load_dicom_path(context, selected_dicom_path, next_frame_index, None);
    }

    pub(super) fn jump_to_slice(&mut self, context: &egui::Context, slice_index: usize) -> bool {
        if let Some(selection) = self.selected_slice {
            let Some(series_slice_count) = self.get_selected_series_slice_count() else {
                return false;
            };

            if series_slice_count == 0 {
                return false;
            }

            let next_slice_index = slice_index.min(series_slice_count - 1);
            if selection.slice_index == next_slice_index {
                return true;
            }

            return self.load_slice_by_indices(
                context,
                selection.patient_index,
                selection.study_index,
                selection.series_index,
                next_slice_index,
            );
        }

        let Some(selected_dicom_path) = self.selected_dicom_path.clone() else {
            return false;
        };

        if self.selected_dicom_frame_count == 0 {
            return false;
        }

        let next_frame_index =
            slice_index.min((self.selected_dicom_frame_count - 1) as usize) as u32;
        if next_frame_index == self.selected_dicom_frame_index {
            return true;
        }

        self.load_dicom_path(context, selected_dicom_path, next_frame_index, None)
    }
}

/// One frame's slice navigation keys, read the same way in 2D and 3D.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SliceKeys {
    slice_delta: isize,
    jump_to_start: bool,
    jump_to_end: bool,
}

fn consume_slice_keys(input_state: &mut egui::InputState) -> SliceKeys {
    let mut slice_delta = 0;

    slice_delta +=
        input_state.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) as isize;
    slice_delta +=
        input_state.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) as isize;
    slice_delta -=
        input_state.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) as isize;
    slice_delta -=
        input_state.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) as isize;

    slice_delta += input_state.count_and_consume_key(egui::Modifiers::NONE, egui::Key::PageDown)
        as isize
        * PAGE_SLICE_STEP;
    slice_delta -= input_state.count_and_consume_key(egui::Modifiers::NONE, egui::Key::PageUp)
        as isize
        * PAGE_SLICE_STEP;

    SliceKeys {
        slice_delta,
        jump_to_start: input_state.consume_key(egui::Modifiers::NONE, egui::Key::Home),
        jump_to_end: input_state.consume_key(egui::Modifiers::NONE, egui::Key::End),
    }
}

fn consume_window_preset_keys(
    input_state: &mut egui::InputState,
    enabled: bool,
) -> Option<WindowPreset> {
    enabled
        .then(|| {
            WINDOW_PRESETS
                .iter()
                .copied()
                .find(|preset| consume_window_preset_key(input_state, *preset))
        })
        .flatten()
}

fn window_preset_key(preset: WindowPreset) -> egui::Key {
    match preset {
        WindowPreset::Default => egui::Key::Num0,
        WindowPreset::FullDynamic => egui::Key::Num1,
        WindowPreset::Skull => egui::Key::Num2,
        WindowPreset::Lung => egui::Key::Num3,
        WindowPreset::Abdomen => egui::Key::Num4,
        WindowPreset::Mediastinum => egui::Key::Num5,
        WindowPreset::Bone => egui::Key::Num6,
        WindowPreset::Spine => egui::Key::Num7,
        WindowPreset::Postmyelo => egui::Key::Num8,
        WindowPreset::Felsenbein => egui::Key::Num9,
    }
}

fn consume_window_preset_key(input_state: &mut egui::InputState, preset: WindowPreset) -> bool {
    let preset_key = window_preset_key(preset);
    let mut consumed = false;

    input_state.events.retain(|event| {
        let matches_preset = matches!(
            event,
            egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                modifiers,
                ..
            } if modifiers.matches_logically(egui::Modifiers::NONE)
                && (*key == preset_key || *physical_key == Some(preset_key))
        );
        consumed |= matches_preset;
        !matches_preset
    });

    consumed
}

fn consume_viewer_wheel_slices(
    input_state: &mut egui::InputState,
    egui_wheel_modifiers: egui::Modifiers,
    line_scroll_speed: f32,
) -> f32 {
    let mut scrolled_slices = 0.0;

    input_state.events.retain(|event| {
        let egui::Event::MouseWheel {
            unit,
            delta,
            modifiers,
            ..
        } = event
        else {
            return true;
        };

        // Zoom and horizontal-scroll gestures belong to egui's own reading of the wheel.
        if modifiers.matches_any(egui_wheel_modifiers) {
            return true;
        }

        scrolled_slices += scrolled_slices_for_wheel(*unit, delta.y, line_scroll_speed);
        false
    });

    scrolled_slices
}

fn scrolled_slices_for_wheel(
    unit: egui::MouseWheelUnit,
    vertical_delta: f32,
    line_scroll_speed: f32,
) -> f32 {
    match unit {
        egui::MouseWheelUnit::Line => vertical_delta,
        egui::MouseWheelUnit::Point => vertical_delta / line_scroll_speed,
        egui::MouseWheelUnit::Page => vertical_delta * PAGE_SLICE_STEP as f32,
    }
}

fn take_whole_slice_steps(remainder: &mut f32, scrolled_slices: f32) -> isize {
    let pending_slices = *remainder + scrolled_slices;
    let whole_slices = (pending_slices + SCROLL_SLICE_EPSILON.copysign(pending_slices)).trunc();

    if whole_slices == 0.0 {
        *remainder = pending_slices;
        return 0;
    }

    let stepped_slices = whole_slices.clamp(
        -MAX_SCROLL_SLICES_PER_FRAME as f32,
        MAX_SCROLL_SLICES_PER_FRAME as f32,
    );

    // Drop the backlog of a capped fling instead of replaying it over the following frames.
    *remainder = if stepped_slices == whole_slices {
        pending_slices - whole_slices
    } else {
        0.0
    };

    stepped_slices as isize
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

        let mut next_direction = self.playback.direction;
        let Some(next_slice_index) = next_slice_index(
            current_slice_index,
            slice_count,
            self.playback.loop_mode,
            &mut next_direction,
        ) else {
            self.stop_autoplay();
            return;
        };

        if self.jump_to_slice(context, next_slice_index) {
            self.playback.direction = next_direction;
        }
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

pub(in crate::app) struct PreparedFrame {
    default_window: WindowLevel,
    current_window: WindowLevel,
    window_customized: bool,
    active_window_preset: Option<WindowPreset>,
    frame_count: u32,
    value_range: (f64, f64),
    window_level_available: bool,
    metadata: crate::dicom::DicomMetadata,
}

pub(in crate::app) fn prepare_frame(
    entry: &DecodedCacheEntry,
    saved_window_level: Option<SavedWindowLevel>,
) -> anyhow::Result<(PreparedFrame, crate::dicom::DisplayPixels)> {
    let (default_center, default_width) = entry.frame.default_center_width();
    let window_customized = saved_window_level.is_some();
    let center = saved_window_level.map_or(default_center, |saved| saved.window.center);
    let width = saved_window_level.map_or(default_width, |saved| saved.window.width);
    let effective_window = window_customized.then_some(DicomWindow { center, width });
    let pixels = render_frame(&entry.frame, effective_window)?;
    let prepared = PreparedFrame {
        default_window: WindowLevel {
            center: default_center,
            width: default_width,
        },
        current_window: WindowLevel { center, width },
        window_customized,
        active_window_preset: saved_window_level.and_then(|saved| saved.preset),
        frame_count: entry.frame.frame_count,
        value_range: entry.frame.value_range,
        window_level_available: entry.frame.window_level_available(),
        metadata: entry.metadata.clone(),
    };
    Ok((prepared, pixels))
}

impl DicronApp {
    pub(in crate::app) fn load_dicom_path(
        &mut self,
        context: &egui::Context,
        dicom_path: PathBuf,
        frame_index: u32,
        target_selection: Option<SliceSelection>,
    ) -> bool {
        // A direct slice selection replaces any pending initial/reopened image.
        self.frame_load.clear();

        if self.decoded_cache.get(&dicom_path, frame_index).is_none() {
            match load_dicom_frame_with_encoding(&dicom_path, frame_index, self.text_encoding) {
                Ok(loaded) => self.decoded_cache.insert(DecodedCacheEntry {
                    path: dicom_path.clone(),
                    frame_index,
                    frame: loaded.frame,
                    metadata: loaded.metadata,
                }),
                Err(error) => {
                    self.error_message = Some(format!("Failed to open DICOM: {error:#}"));
                    return false;
                }
            }
        }

        let saved_window_level = self.window_level_for_selection(target_selection);
        let prepared = {
            let Some(entry) = self.decoded_cache.get(&dicom_path, frame_index) else {
                self.error_message = Some("Failed to retrieve decoded DICOM frame.".to_owned());
                return false;
            };
            prepare_frame(entry, saved_window_level)
        };

        let (prepared, pixels) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.error_message = Some(format!("Failed to render DICOM: {error:#}"));
                return false;
            }
        };

        let loaded_texture = upload_display_pixels(context, texture_name(&dicom_path), pixels);
        self.display_prepared_frame(
            dicom_path,
            frame_index,
            target_selection,
            prepared,
            loaded_texture,
        );
        true
    }

    pub(in crate::app) fn display_prepared_frame(
        &mut self,
        dicom_path: PathBuf,
        frame_index: u32,
        target_selection: Option<SliceSelection>,
        prepared: PreparedFrame,
        loaded_texture: egui::TextureHandle,
    ) {
        let should_fit_viewport = target_selection.is_some_and(|target| {
            self.selected_slice
                .is_none_or(|current| current.series_key() != target.series_key())
        });
        if should_fit_viewport {
            self.viewport_transform = Default::default();
            self.viewport_zoom_anchor = None;
        }
        self.selected_slice = target_selection;
        if let Some(selection) = target_selection {
            self.last_slice_by_series
                .insert(selection.series_key(), selection.slice_index);
        }
        self.window_level.apply_loaded_frame(
            prepared.default_window,
            prepared.current_window,
            prepared.value_range,
            prepared.window_customized,
            prepared.window_level_available,
            prepared.active_window_preset,
        );
        self.selected_dicom_frame_index = frame_index;
        self.selected_dicom_frame_count = prepared.frame_count;
        self.loaded_texture = Some(loaded_texture);
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

pub(in crate::app) fn texture_name(dicom_path: &std::path::Path) -> &str {
    dicom_path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or("loaded-dicom-image")
}

/// The window the source application uses for its MPR views, and the last
/// resort when neither the series nor its voxels offer anything better.
const DEFAULT_MPR_WINDOW: WindowLevel = WindowLevel {
    center: 40.0,
    width: 350.0,
};

/// The window that spans everything in the volume, the way the 2D viewer
/// exposes a frame that states no window of its own. `None` when the voxels
/// carry no usable range.
fn value_range_window(volume: &Volume) -> Option<WindowLevel> {
    let (lowest, highest) = volume.value_range;
    let width = f64::from(highest) - f64::from(lowest);

    (width.is_finite() && width > 0.0).then(|| WindowLevel {
        center: f64::midpoint(f64::from(lowest), f64::from(highest)),
        width,
    })
}

impl DicronApp {
    pub(in crate::app) fn selected_series_slices(&self) -> Option<&[SliceItem]> {
        let selection = self.selected_slice?;
        let patient = self
            .dicom_index
            .as_ref()?
            .patients
            .get(selection.patient_index)?;
        let study = patient.studies.get(selection.study_index)?;
        let series = study.series_groups.get(selection.series_index)?;

        Some(series.slices.as_slice())
    }

    /// Whether the selected series can be reconstructed: a grayscale image, by
    /// the same test the window controls use, with at least two single-frame
    /// slices.
    pub(in crate::app) fn series_supports_three_d(&self) -> bool {
        self.window_level.is_available()
            && self.selected_series_slices().is_some_and(|slices| {
                slices.len() >= 2 && slices.iter().all(|slice| slice.frame_index == 0)
            })
    }

    /// Whether the 3D workspace was built from the selected series. While
    /// another series is selected - its volume still building, or its
    /// low-resolution warning still open - the views show nothing of the old
    /// workspace and take no input for it, rather than show its images under
    /// the new series' name.
    pub(in crate::app) fn workspace_shows_selection(&self) -> bool {
        self.selected_series_identity()
            .is_some_and(|(series_key, fingerprint)| {
                self.three_d.active_matches(series_key, fingerprint)
            })
    }

    /// The selected series together with the fingerprint of its exact frames.
    fn selected_series_identity(&self) -> Option<(SeriesKey, u64)> {
        let series_key = self.current_series_key()?;
        let slices = self.selected_series_slices()?;

        Some((series_key, series_fingerprint(slices)))
    }

    pub(in crate::app) fn open_three_d_mode(&mut self, context: &egui::Context, mode: ThreeDMode) {
        let Some((series_key, fingerprint)) = self.selected_series_identity() else {
            self.error_message = Some("Open a DICOM series to use 3D.".to_owned());
            return;
        };

        if mode == ThreeDMode::VolumeRendering && !self.three_d.gpu_available {
            self.error_message = Some(GPU_UNAVAILABLE_MESSAGE.to_owned());
            return;
        }

        self.stop_autoplay();
        self.clear_slice_scroll_remainders();

        if self.three_d.active_matches(series_key, fingerprint) {
            // A texture is wanted only while volume rendering is on screen.
            if mode != ThreeDMode::VolumeRendering
                && self
                    .three_d
                    .job
                    .request()
                    .is_some_and(|request| matches!(request.kind, VolumeJobKind::GpuUpload { .. }))
            {
                self.three_d.job.cancel();
            }
            self.three_d.view_mode = ViewMode::ThreeD(mode);
            self.ensure_gpu_volume(context);
            return;
        }

        if self.three_d.cached_matches(series_key, fingerprint)
            && let Some((cached_key, cached_fingerprint, prepared)) = self.three_d.cached.take()
        {
            self.activate_or_prompt(prepared, cached_key, cached_fingerprint, mode);
            return;
        }

        if self.three_d.job_matches(series_key, fingerprint) {
            self.three_d.job.set_requested_mode(mode);
            // Inside 3D the click shows at once, over the preparation
            // placeholder; a build asked for from 2D still waits in 2D.
            if self.three_d.view_mode.three_d_mode().is_some() {
                self.three_d.view_mode = ViewMode::ThreeD(mode);
            }
            return;
        }

        self.start_volume_build(context, series_key, fingerprint, mode);
    }

    pub(in crate::app) fn leave_three_d(&mut self) {
        self.three_d.view_mode = ViewMode::TwoD;
        self.three_d.job.cancel();
        self.three_d.pending_selection_frames = 0;
        self.clear_slice_scroll_remainders();

        // A drag the release of which never reaches the views would stay
        // latched until the next press.
        if let Some(active) = &mut self.three_d.active {
            active.mpr.drag = None;
        }
    }

    /// Forget partially spent wheel motion, so entering or leaving 3D never
    /// steps a slice from input the other view mode collected.
    fn clear_slice_scroll_remainders(&mut self) {
        self.viewer_scroll_slice_remainder = 0.0;

        if let Some(active) = &mut self.three_d.active {
            active.mpr.clear_wheel_remainders();
        }
    }

    /// Restore the default view of the mode on screen: re-centred planes in
    /// MPR and MIP, the default camera in volume rendering. The other modes
    /// keep theirs.
    pub(in crate::app) fn reset_three_d_view(&mut self) {
        let Some(mode) = self.three_d.view_mode.three_d_mode() else {
            return;
        };

        if let Some(active) = &mut self.three_d.active {
            if mode == ThreeDMode::VolumeRendering {
                active.render.reset_view();
            } else {
                active.mpr.recenter();
            }
        }
    }

    /// Start preparing the volume rendering texture for the workspace on
    /// screen. Only one job runs at a time, so this waits for a build or a
    /// resample to finish instead of cancelling it.
    pub(in crate::app) fn ensure_gpu_volume(&mut self, context: &egui::Context) {
        if self.three_d.view_mode != ViewMode::ThreeD(ThreeDMode::VolumeRendering)
            || self.three_d.job.request().is_some()
        {
            return;
        }

        let Some(gpu) = self.three_d.gpu else {
            return;
        };
        let Some(active) = &self.three_d.active else {
            return;
        };

        if active.prepared.gpu.is_some() || active.gpu_budget.exhausted() {
            return;
        }

        // A workspace kept on screen while the selection has moved on is about
        // to be replaced, and its texture would only be cancelled again.
        if self.selected_series_identity() != Some((active.series_key, active.fingerprint)) {
            return;
        }

        let request = VolumeJobRequest {
            series_key: active.series_key,
            fingerprint: active.fingerprint,
            requested_mode: ThreeDMode::VolumeRendering,
            kind: VolumeJobKind::GpuUpload {
                series: Arc::clone(&active.prepared.series),
                max_texture_dimension_3d: gpu.max_texture_dimension_3d,
                max_voxels: active.gpu_budget.max_voxels,
            },
        };

        self.three_d.job.start(context, request);
    }

    /// Keep the workspace on the selected series, debounced so dragging through
    /// the tree does not start a build per frame.
    pub(in crate::app) fn ensure_volume_for_selection(&mut self, context: &egui::Context) {
        let Some(mode) = self.three_d.view_mode.three_d_mode() else {
            self.three_d.pending_selection_frames = 0;
            self.cancel_build_left_behind_in_two_d();
            return;
        };

        if self.three_d.low_resolution_prompt.is_some() {
            return;
        }

        let Some((series_key, fingerprint)) = self.selected_series_identity() else {
            self.three_d.pending_selection_frames = 0;
            return;
        };

        // Only the workspace built from the selected series may stay on screen.
        if self.three_d.active_matches(series_key, fingerprint) {
            self.three_d.pending_selection_frames = 0;
            // Whatever a superseded selection started is no longer wanted.
            self.cancel_superseded_volume_job(series_key, fingerprint);
            return;
        }

        // A volume kept from an earlier visit opens as it does from 2D, so a
        // declined low-resolution warning is asked again.
        if self.three_d.cached_matches(series_key, fingerprint)
            && let Some((cached_key, cached_fingerprint, prepared)) = self.three_d.cached.take()
        {
            self.three_d.pending_selection_frames = 0;
            self.cancel_superseded_volume_job(series_key, fingerprint);
            self.activate_or_prompt(prepared, cached_key, cached_fingerprint, mode);
            return;
        }

        // A series that cannot be reconstructed, or already failed to be,
        // returns to 2D with its reason rather than being decoded again.
        // Leaving 3D also stops this check from running every frame.
        let failure = if self.series_supports_three_d() {
            self.three_d
                .build_failure(series_key, fingerprint)
                .map(str::to_owned)
        } else {
            Some(THREE_D_UNSUPPORTED_MESSAGE.to_owned())
        };

        if let Some(failure) = failure {
            self.error_message = Some(failure);
            self.leave_three_d();
            return;
        }

        if self.three_d.job_matches(series_key, fingerprint) {
            self.three_d.pending_selection_frames = 0;
            return;
        }

        if self.three_d.pending_selection_frames == 0 {
            self.three_d.pending_selection_frames = 1;
            context.request_repaint();
            return;
        }

        self.start_volume_build(context, series_key, fingerprint, mode);
    }

    /// A build requested from 2D is for the series selected at the time.
    /// Once the selection leaves that series the user is no longer waiting
    /// for it, and it must not open 3D on its own when it finishes.
    fn cancel_build_left_behind_in_two_d(&mut self) {
        let Some(request) = self.three_d.job.request() else {
            return;
        };
        let requested = (request.series_key, request.fingerprint);

        if self.selected_series_identity() != Some(requested) {
            self.three_d.job.cancel();
        }
    }

    /// Abandon a 3D view still being prepared from 2D.
    pub(in crate::app) fn cancel_volume_build(&mut self) {
        if self.three_d.view_mode == ViewMode::TwoD {
            self.three_d.job.cancel();
        }
    }

    pub(in crate::app) fn receive_volume_job_messages(&mut self, context: &egui::Context) {
        let Some((request, output)) = self.three_d.job.poll(context) else {
            return;
        };

        self.receive_volume_job_output(&request, output);
    }

    fn receive_volume_job_output(
        &mut self,
        request: &VolumeJobRequest,
        output: Result<VolumeJobOutput, String>,
    ) {
        match output {
            Ok(VolumeJobOutput::Built(prepared)) => {
                self.receive_built_volume(prepared, request);
            }
            Ok(VolumeJobOutput::Resampled(volume)) => {
                self.install_resampled_volume(request, volume);
            }
            Ok(VolumeJobOutput::Gpu(gpu)) => self.install_gpu_volume(request, gpu),
            Err(message) => {
                let failure = format!("Failed to prepare 3D: {message}");
                // Only a failed decode says the series cannot be reconstructed;
                // a resample or texture job failing leaves its workspace usable.
                if matches!(request.kind, VolumeJobKind::Build { .. }) {
                    self.three_d
                        .build_failures
                        .insert((request.series_key, request.fingerprint), failure.clone());
                }
                self.error_message = Some(failure);
                if self.selected_series_identity()
                    == Some((request.series_key, request.fingerprint))
                    && !self
                        .three_d
                        .active_matches(request.series_key, request.fingerprint)
                {
                    self.leave_three_d();
                }
            }
        }
    }

    /// Open a finished volume only when the user is still pointing at the
    /// series it was built from; otherwise keep it for when they come back.
    ///
    /// Without this a build the user navigated away from would replace the
    /// workspace with a series the tree, the toolbar and the metadata panel all
    /// disagree with, and could pull the viewer out of 2D on its own.
    fn receive_built_volume(&mut self, prepared: PreparedVolume, request: &VolumeJobRequest) {
        // A series that failed before, perhaps while its files were still
        // being copied, is reconstructable now.
        self.three_d
            .build_failures
            .remove(&(request.series_key, request.fingerprint));

        let still_selected =
            self.selected_series_identity()
                .is_some_and(|(series_key, fingerprint)| {
                    series_key == request.series_key && fingerprint == request.fingerprint
                });

        if !still_selected {
            self.three_d.cached = Some((request.series_key, request.fingerprint, prepared));
            return;
        }

        self.activate_or_prompt(
            prepared,
            request.series_key,
            request.fingerprint,
            request.requested_mode,
        );
    }

    /// Stop a job preparing anything other than the given series.
    fn cancel_superseded_volume_job(&mut self, series_key: SeriesKey, fingerprint: u64) {
        let superseded = self.three_d.job.request().is_some_and(|request| {
            request.series_key != series_key || request.fingerprint != fingerprint
        });

        if superseded {
            self.three_d.job.cancel();
        }
    }

    /// Switch which volume the MPR views sample. A choice whose grid is
    /// already in memory applies at once; any other runs a resample job while
    /// the current planes stay on screen, and takes effect, label and all,
    /// only once its volume is in, so a cancelled resample leaves nothing
    /// claiming a grid that is not shown.
    pub(in crate::app) fn set_mpr_quality(&mut self, context: &egui::Context, quality: MprQuality) {
        let Some(mode) = self.three_d.view_mode.three_d_mode() else {
            return;
        };
        let three_d = &mut self.three_d;
        let Some(active) = &mut three_d.active else {
            return;
        };

        if let Some(volume) = active.volume_for_quality(quality) {
            // A resample still running was for an earlier choice.
            if three_d
                .job
                .request()
                .is_some_and(|request| matches!(request.kind, VolumeJobKind::Resample { .. }))
            {
                three_d.job.cancel();
            }
            active.mpr.quality = quality;
            active.mpr.install_volume(volume);
            active.release_unshown_planes();
            return;
        }

        // A resample cannot be interrupted part way, and starting one cancels
        // whatever else is running, so while another job is in flight the menu
        // offers only the grids already in memory.
        if three_d.job.request().is_some() {
            return;
        }

        let Some(target_mm) = quality.target_mm(&active.prepared.series.volume) else {
            return;
        };
        let request = VolumeJobRequest {
            series_key: active.series_key,
            fingerprint: active.fingerprint,
            requested_mode: mode,
            kind: VolumeJobKind::Resample {
                series: Arc::clone(&active.prepared.series),
                target_mm,
                quality,
            },
        };

        three_d.job.start(context, request);
    }

    /// Show a resampled volume with the quality it was made for, unless the
    /// workspace has moved on to another series in the meantime.
    fn install_resampled_volume(&mut self, request: &VolumeJobRequest, volume: Arc<Volume>) {
        let VolumeJobKind::Resample { quality, .. } = request.kind else {
            return;
        };
        let Some(active) = &mut self.three_d.active else {
            return;
        };

        if active.series_key != request.series_key || active.fingerprint != request.fingerprint {
            return;
        }

        active.mpr.quality = quality;
        active.mpr.install_volume(volume);
        active.release_unshown_planes();
    }

    /// The device refused the texture of volume `volume_id`, `voxels` large.
    /// The workspace showing it prepares a texture of half as many voxels, so
    /// the next attempt fits where this one failed, until its attempts are
    /// spent.
    pub(in crate::app) fn receive_gpu_refusal(&mut self, volume_id: u64, voxels: u64) {
        let Some(active) = &mut self.three_d.active else {
            return;
        };

        if active
            .prepared
            .gpu
            .as_ref()
            .is_some_and(|gpu| gpu.id == volume_id)
        {
            active.prepared.gpu = None;
            active.gpu_budget = active.gpu_budget.after_refusal(voxels);
        }
    }

    /// Show a prepared volume rendering texture, unless the workspace has moved
    /// on to another series in the meantime.
    fn install_gpu_volume(&mut self, request: &VolumeJobRequest, gpu: Arc<GpuVolume>) {
        if let Some(active) = &mut self.three_d.active
            && active.series_key == request.series_key
            && active.fingerprint == request.fingerprint
        {
            active.prepared.gpu = Some(gpu);
        }
    }

    pub(in crate::app) fn accept_low_resolution_prompt(&mut self) {
        let Some(prompt) = self.three_d.low_resolution_prompt.take() else {
            return;
        };

        self.three_d
            .low_resolution_accepted
            .insert((prompt.series_key, prompt.fingerprint));
        self.activate_volume(
            prompt.prepared,
            prompt.series_key,
            prompt.fingerprint,
            prompt.mode,
            true,
        );
    }

    /// Return to the selected series in 2D and keep the declined volume, so
    /// the warning is shown again the next time the user asks for 3D.
    pub(in crate::app) fn cancel_low_resolution_prompt(&mut self) {
        let Some(prompt) = self.three_d.low_resolution_prompt.take() else {
            return;
        };

        self.three_d.cached = Some((prompt.series_key, prompt.fingerprint, prompt.prepared));
        // The selection already names the declined series. Keeping another
        // series in the 3D workspace would disagree with the tree and metadata.
        self.leave_three_d();
    }

    fn start_volume_build(
        &mut self,
        context: &egui::Context,
        series_key: SeriesKey,
        fingerprint: u64,
        mode: ThreeDMode,
    ) {
        let Some(slices) = self.selected_series_slices() else {
            return;
        };
        let slices = Arc::from(spatially_ordered_slices(slices));

        // A volume kept for another series would be replaced when this one
        // lands anyway; while it builds, the workspace on screen is the only
        // other one worth keeping.
        if !self.three_d.cached_matches(series_key, fingerprint) {
            self.three_d.cached = None;
        }

        self.three_d.pending_selection_frames = 0;
        self.three_d.job.start(
            context,
            VolumeJobRequest {
                series_key,
                fingerprint,
                requested_mode: mode,
                kind: VolumeJobKind::Build { slices },
            },
        );
    }

    fn activate_or_prompt(
        &mut self,
        prepared: PreparedVolume,
        series_key: SeriesKey,
        fingerprint: u64,
        mode: ThreeDMode,
    ) {
        let low_resolution_cause = prepared.series.geometry.low_resolution_cause();
        let low_resolution = low_resolution_cause.is_some();

        if let Some(cause) = low_resolution_cause
            && !self
                .three_d
                .low_resolution_accepted
                .contains(&(series_key, fingerprint))
        {
            self.three_d.low_resolution_prompt = Some(LowResolutionPrompt {
                prepared,
                series_key,
                fingerprint,
                mode,
                cause,
            });
            return;
        }

        self.activate_volume(prepared, series_key, fingerprint, mode, low_resolution);
    }

    fn activate_volume(
        &mut self,
        prepared: PreparedVolume,
        series_key: SeriesKey,
        fingerprint: u64,
        mode: ThreeDMode,
        low_resolution: bool,
    ) {
        // The workspace this one replaces is remembered as the user left it.
        if let Some(previous) = &self.three_d.active {
            self.three_d.saved_views.insert(
                (previous.series_key, previous.fingerprint),
                (previous.mpr.saved_view(), previous.render),
            );
        }

        let window = self.mpr_window(series_key, &prepared.series);
        let mut mpr = MprState::new(Arc::clone(&prepared.mpr), window);
        // The selection names this series whenever a workspace opens.
        let series_overlay = self.metadata.overlay.clone();
        let mut render = VolumeRenderState {
            preset: TransferPreset::for_modality(
                series_overlay
                    .as_ref()
                    .and_then(|overlay| overlay.modality.as_deref()),
            ),
            ..VolumeRenderState::default()
        };

        if let Some((saved_mpr, saved_render)) =
            self.three_d.saved_views.get(&(series_key, fingerprint))
        {
            mpr.restore_view(saved_mpr);
            render = *saved_render;
        } else if let Some(selection) = self
            .selected_slice
            .filter(|selection| selection.series_key() == series_key)
        {
            // A first visit opens on the slice the 2D viewer shows, so the
            // anatomy stays in view, wherever interpolation or resampling has
            // moved it along z.
            let slices = self
                .selected_series_slices()
                .expect("selected series exists");
            let selected = &slices[selection.slice_index];
            let ordered = spatially_ordered_slices(slices);
            let spatial_index = ordered
                .iter()
                .position(|slice| {
                    slice.path == selected.path && slice.frame_index == selected.frame_index
                })
                .expect("selected frame belongs to the reconstructed series");
            let fraction = prepared.series.geometry.depth_fraction(spatial_index);
            mpr.set_index(VolumeAxis::Z, (fraction * mpr.volume.depth as f64) as usize);
        }

        self.three_d.active = Some(ActiveVolumeView {
            series_key,
            fingerprint,
            series_overlay,
            prepared,
            low_resolution,
            mpr,
            render,
            gpu_budget: GpuBudget::FULL,
        });
        self.three_d.cached = None;
        self.three_d.pending_selection_frames = 0;
        self.three_d.view_mode = ViewMode::ThreeD(mode);
        self.stop_autoplay();
        // It edits the 2D window, which the 3D views no longer show.
        self.edit_windowing_dialog.open = false;
    }

    /// The window the MPR views start from: the series' saved 2D window, else
    /// the window the shown frame states, else that frame's own value range,
    /// else the volume's.
    ///
    /// The last steps matter for the series that state no window at all, which
    /// are common in MR, PET and converted data. The 2D viewer shows each such
    /// frame across its own range, so 3D opens on the range of the frame on
    /// screen and shows that slice as 2D does. The range of the whole volume
    /// would let one hot spot, such as a PET bladder, darken every other slice.
    fn mpr_window(&self, series_key: SeriesKey, series: &SeriesVolume) -> WindowLevel {
        // The voxels keep their stored values, MONOCHROME1 or not, so every
        // window here applies to them unchanged, as it does in 2D.
        if let Some(saved) = self.window_level.by_series.get(&series_key) {
            return saved.window;
        }

        if let Some((path, frame_index)) = &self.current_frame_key
            && let Some(entry) = self.decoded_cache.peek(path, *frame_index)
            && let Some(window) = entry
                .frame
                .default_window
                .or_else(|| entry.frame.full_dynamic_window())
        {
            return WindowLevel {
                center: window.center,
                width: window.width.max(1.0),
            };
        }

        value_range_window(&series.volume).unwrap_or(DEFAULT_MPR_WINDOW)
    }

    fn clear_three_d_state(&mut self) {
        self.three_d.job.cancel();
        self.three_d.active = None;
        self.three_d.cached = None;
        self.three_d.low_resolution_prompt = None;
        self.three_d.low_resolution_accepted.clear();
        self.three_d.build_failures.clear();
        self.three_d.saved_views.clear();
        self.three_d.view_mode = ViewMode::TwoD;
        self.three_d.pending_selection_frames = 0;
    }
}

impl DicronApp {
    pub(in crate::app) fn clear_loaded_dicom_state(&mut self) {
        self.frame_load.clear();
        self.selected_dicom_path = None;
        self.loaded_texture = None;
        self.decoded_cache.clear();
        self.series_thumbnails.clear();
        self.current_frame_key = None;
        self.window_level.clear_for_new_document();
        self.viewport_transform = Default::default();
        self.viewport_zoom_anchor = None;
        self.edit_windowing_dialog.open = false;
        self.metadata.clear();
        self.dicom_index = None;
        self.clear_three_d_state();
        self.last_slice_by_series.clear();
        self.clear_selected_indices();
    }
}

/// Test support: series held only in memory, whose files are never read, so
/// 3D state can be tested against a real selection.
#[cfg(test)]
impl DicronApp {
    /// Index `series_count` series of two slices each in one study, and select
    /// the first.
    pub(in crate::app) fn index_in_memory_series(&mut self, series_count: usize) {
        use crate::dicom::{DicomIndex, PatientGroup, StudyGroup};

        let series = |series_index: usize| crate::dicom::SeriesGroup {
            display_name: format!("series {series_index}"),
            series_description: None,
            series_number: None,
            modality: None,
            slices: (0..2)
                .map(|slice_index| SliceItem {
                    path: PathBuf::from(format!("in-memory-{series_index}-{slice_index}.dcm")),
                    display_name: String::new(),
                    frame_index: 0,
                    instance_number: Some(slice_index + 1),
                    sort_position: Some(f64::from(slice_index)),
                })
                .collect(),
        };

        self.dicom_index = Some(DicomIndex {
            patients: vec![PatientGroup {
                display_name: String::new(),
                studies: vec![StudyGroup {
                    display_name: String::new(),
                    study_date: None,
                    study_time: None,
                    series_groups: (0..series_count).map(series).collect(),
                }],
            }],
            total_file_count: series_count * 2,
        });
        self.select_in_memory_series(0);
    }

    /// Select a series of the in-memory index, returning its identity.
    pub(in crate::app) fn select_in_memory_series(
        &mut self,
        series_index: usize,
    ) -> (SeriesKey, u64) {
        self.selected_slice = Some(SliceSelection::new(0, 0, series_index, 0));
        self.selected_series_identity().unwrap()
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::*;

    #[test]
    fn shifted_one_uses_the_physical_digit_for_the_full_dynamic_preset() {
        let mut input_state = egui::InputState::default();
        input_state.events.push(egui::Event::Key {
            key: egui::Key::Exclamationmark,
            physical_key: Some(egui::Key::Num1),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::SHIFT,
        });

        assert!(consume_window_preset_key(
            &mut input_state,
            WindowPreset::FullDynamic
        ));
        assert!(input_state.events.is_empty());
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;

    const LINE_SCROLL_SPEED: f32 = 40.0;

    fn slices_moved_by(remainder: &mut f32, lines: f32) -> isize {
        -take_whole_slice_steps(
            remainder,
            scrolled_slices_for_wheel(egui::MouseWheelUnit::Line, lines, LINE_SCROLL_SPEED),
        )
    }

    #[test]
    fn each_wheel_unit_converts_to_slices() {
        let slices = |unit, delta| scrolled_slices_for_wheel(unit, delta, LINE_SCROLL_SPEED);

        assert_eq!(slices(egui::MouseWheelUnit::Line, 1.0), 1.0);
        assert_eq!(slices(egui::MouseWheelUnit::Point, LINE_SCROLL_SPEED), 1.0);
        assert_eq!(
            slices(egui::MouseWheelUnit::Page, 1.0),
            PAGE_SLICE_STEP as f32
        );
    }

    #[test]
    fn one_wheel_notch_moves_exactly_one_slice() {
        let mut remainder = 0.0;

        assert_eq!(slices_moved_by(&mut remainder, -1.0), 1);
        assert_eq!(remainder, 0.0);
        assert_eq!(slices_moved_by(&mut remainder, 1.0), -1);
        assert_eq!(remainder, 0.0);
    }

    #[test]
    fn high_resolution_wheel_fragments_add_up_to_one_slice() {
        let mut remainder = 0.0;
        let moved_slices: isize = (0..15)
            .map(|_| slices_moved_by(&mut remainder, -1.0 / 15.0))
            .sum();

        assert_eq!(moved_slices, 1);
    }

    #[test]
    fn a_fling_is_capped_and_drops_its_backlog() {
        let mut remainder = 0.0;

        assert_eq!(
            slices_moved_by(&mut remainder, -100.0),
            MAX_SCROLL_SLICES_PER_FRAME
        );
        assert_eq!(remainder, 0.0);
    }

    #[test]
    fn one_wheel_notch_down_advances_one_slice_through_the_shared_helper() {
        let context = egui::Context::default();
        let raw_input = egui::RawInput {
            events: vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, -1.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut remainder = 0.0;
        let mut stepped_slices = 0;

        let _ = context.run_ui(raw_input, |ui| {
            stepped_slices = consume_wheel_slice_steps(ui.ctx(), &mut remainder);
        });

        assert_eq!(stepped_slices, 1);
        assert_eq!(remainder, 0.0);
    }

    #[test]
    fn the_viewer_consumes_plain_wheel_events_but_leaves_egui_gestures() {
        let mut input_state = egui::InputState::default();
        input_state.events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -1.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        input_state.events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -1.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::COMMAND,
        });

        let scrolled_slices = consume_viewer_wheel_slices(
            &mut input_state,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            LINE_SCROLL_SPEED,
        );

        assert_eq!(scrolled_slices, -1.0);
        assert_eq!(input_state.events.len(), 1);
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

#[cfg(test)]
mod loading_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use dicom_core::{DataElement, PrimitiveValue, VR};
    use dicom_dictionary_std::{tags, uids};
    use dicom_encoding::text::TextCodec;
    use dicom_object::{FileDicomObject, FileMetaTableBuilder};

    use super::*;

    static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

    /// A temporary file path whose file, if any, is removed when the guard is
    /// dropped, so a failing assertion leaves nothing behind.
    pub(super) struct TemporaryFile(PathBuf);

    impl std::ops::Deref for TemporaryFile {
        type Target = PathBuf;

        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for TemporaryFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    pub(super) fn temporary_file(label: &str) -> TemporaryFile {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        TemporaryFile(std::env::temp_dir().join(format!(
            "dicron-actions-{label}-{}-{sequence}.dcm",
            std::process::id()
        )))
    }

    pub(super) fn write_single_pixel_dicom(path: &std::path::Path) {
        write_single_pixel_dicom_with_window(path, None);
    }

    /// A one-pixel image holding 128 that states `window`, center then width.
    pub(super) fn write_single_pixel_dicom_with_window(
        path: &std::path::Path,
        window: Option<(&str, &str)>,
    ) {
        write_single_pixel(path, "MONOCHROME2", vec![128], window);
    }

    /// A one-pixel colour image, which has no window to level.
    pub(super) fn write_single_pixel_rgb_dicom(path: &std::path::Path) {
        write_single_pixel(path, "RGB", vec![200, 100, 50], None);
    }

    /// One pixel of `samples`, a sample per channel.
    fn write_single_pixel(
        path: &std::path::Path,
        photometric_interpretation: &str,
        samples: Vec<u8>,
        window: Option<(&str, &str)>,
    ) {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
            .media_storage_sop_instance_uid("2.25.600")
            .build()
            .unwrap();
        let mut object = FileDicomObject::new_empty_with_meta(meta);

        for (tag, value) in [
            (tags::ROWS, 1_u16),
            (tags::COLUMNS, 1_u16),
            (tags::SAMPLES_PER_PIXEL, samples.len() as u16),
            (tags::PLANAR_CONFIGURATION, 0_u16),
            (tags::BITS_ALLOCATED, 8_u16),
            (tags::BITS_STORED, 8_u16),
            (tags::HIGH_BIT, 7_u16),
            (tags::PIXEL_REPRESENTATION, 0_u16),
        ] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from(photometric_interpretation),
        ));
        if let Some((center, width)) = window {
            object.put_element(DataElement::new(
                tags::WINDOW_CENTER,
                VR::DS,
                PrimitiveValue::from(center),
            ));
            object.put_element(DataElement::new(
                tags::WINDOW_WIDTH,
                VR::DS,
                PrimitiveValue::from(width),
            ));
        }
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from(samples),
        ));
        object.write_to_file(path).unwrap();
    }

    fn finish_source_load(app: &mut DicronApp, context: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.scan.is_active() || app.frame_load.is_active() {
            assert!(Instant::now() < deadline, "source load did not finish");
            app.receive_scan_messages(context);
            app.receive_frame_load(context);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.error_message.is_none());
    }

    #[test]
    fn reopening_with_encoding_updates_tree_and_metadata_and_keeps_the_selected_file() {
        let file = temporary_file("reopen-korean");
        let path = file.to_path_buf();
        write_single_pixel_dicom(&path);
        let mut object = dicom_object::open_file(&path).unwrap();
        object.put_element(DataElement::new(
            tags::PATIENT_NAME,
            VR::PN,
            PrimitiveValue::from("ABCDEF"),
        ));
        object.write_to_file(&path).unwrap();
        let mut encoded_file = std::fs::read(&path).unwrap();
        let encoded_name = TextEncoding::KoreanEucKr
            .assumed_character_set()
            .encode("홍길동")
            .unwrap();
        let name_offset = encoded_file
            .windows(6)
            .position(|window| window == b"ABCDEF")
            .unwrap();
        encoded_file[name_offset..name_offset + 6].copy_from_slice(&encoded_name);
        std::fs::write(&path, &encoded_file).unwrap();

        let context = egui::Context::default();
        let mut app = DicronApp {
            dicom_index: Some(build_for_file(&path).unwrap()),
            open_source: Some(OpenSource::File(path.clone())),
            ..Default::default()
        };
        app.load_preferred_slice(&context, None);
        assert!(app.frame_load.is_active());
        finish_source_load(&mut app, &context);
        assert_eq!(app.selected_dicom_path.as_deref(), Some(path.as_path()));
        let viewport = crate::app::state::ViewportTransform {
            zoom: 2.0,
            pan: egui::vec2(12.0, -4.0),
            flip_horizontal: true,
            flip_vertical: false,
            rotation_quarters: 1,
        };
        let saved_window = crate::app::state::SavedWindowLevel {
            window: WindowLevel {
                center: 100.0,
                width: 200.0,
            },
            preset: None,
        };
        app.viewport_transform = viewport;
        app.window_level.by_series.insert((0, 0, 0), saved_window);

        app.reopen_with_encoding(&context, TextEncoding::Auto);
        assert_eq!(app.text_encoding, TextEncoding::Auto);
        assert!(app.frame_load.is_active());
        finish_source_load(&mut app, &context);
        assert_eq!(
            app.dicom_index.as_ref().unwrap().patients[0].display_name,
            "홍길동"
        );
        assert_eq!(
            app.metadata
                .overlay
                .as_ref()
                .unwrap()
                .patient_label
                .as_deref(),
            Some("홍길동")
        );
        assert_eq!(app.selected_dicom_path.as_deref(), Some(path.as_path()));
        assert_eq!(app.viewport_transform, viewport);
        assert_eq!(app.window_level.current, saved_window.window);

        app.open_source = Some(OpenSource::Inputs(vec![path.clone()]));
        app.reopen_with_encoding(&context, TextEncoding::DicomDefault);
        assert!(app.scan.is_active());
        finish_source_load(&mut app, &context);
        assert_ne!(
            app.dicom_index.as_ref().unwrap().patients[0].display_name,
            "홍길동"
        );
        app.reopen_with_encoding(&context, TextEncoding::KoreanEucKr);
        assert_eq!(app.text_encoding, TextEncoding::KoreanEucKr);
        finish_source_load(&mut app, &context);
        assert_eq!(
            app.dicom_index.as_ref().unwrap().patients[0].display_name,
            "홍길동"
        );
        assert_eq!(app.selected_dicom_path.as_deref(), Some(path.as_path()));
        assert_eq!(app.viewport_transform, viewport);
        assert_eq!(app.window_level.current, saved_window.window);
        assert_eq!(std::fs::read(&path).unwrap(), encoded_file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_slice_load_keeps_displayed_selection_and_autoplay_direction() {
        let valid_path = temporary_file("valid");
        let missing_path = temporary_file("missing");
        write_single_pixel_dicom(&valid_path);

        let mut index = build_for_file(&valid_path).unwrap();
        index.patients[0].studies[0].series_groups[0].slices.insert(
            0,
            SliceItem {
                path: missing_path.to_path_buf(),
                display_name: "missing".to_owned(),
                frame_index: 0,
                instance_number: None,
                sort_position: None,
            },
        );

        let context = egui::Context::default();
        let mut app = DicronApp {
            dicom_index: Some(index),
            ..Default::default()
        };
        assert!(app.load_slice_by_indices(&context, 0, 0, 0, 1));
        assert_eq!(app.last_slice_by_series.get(&(0, 0, 0)), Some(&1));
        app.viewport_transform = crate::app::state::ViewportTransform {
            zoom: 2.0,
            pan: egui::vec2(12.0, -8.0),
            ..Default::default()
        };
        let displayed_selection = app.selected_slice;
        let displayed_path = app.selected_dicom_path.clone();
        let displayed_frame_key = app.current_frame_key.clone();
        let displayed_texture_id = app.loaded_texture.as_ref().map(egui::TextureHandle::id);
        let displayed_metadata: Vec<_> = app
            .metadata
            .all_items
            .iter()
            .map(|item| {
                (
                    item.tag.clone(),
                    item.description.clone(),
                    item.value.clone(),
                )
            })
            .collect();
        let displayed_window = app.window_level.current;
        let displayed_default_window = app.window_level.default;
        let displayed_value_range = app.window_level.value_range;
        let displayed_window_customized = app.window_level.customized;
        let displayed_window_available = app.window_level.available;
        let displayed_window_preset = app.window_level.active_preset;
        let displayed_viewport_transform = app.viewport_transform;

        app.playback.loop_mode = PlaybackLoopMode::PingPong;
        app.playback.direction = 1;
        app.advance_autoplay(&context, 2);

        assert_eq!(app.selected_slice, displayed_selection);
        assert_eq!(app.selected_dicom_path, displayed_path);
        assert_eq!(app.current_frame_key, displayed_frame_key);
        assert_eq!(
            app.loaded_texture.as_ref().map(egui::TextureHandle::id),
            displayed_texture_id
        );
        let current_metadata: Vec<_> = app
            .metadata
            .all_items
            .iter()
            .map(|item| {
                (
                    item.tag.clone(),
                    item.description.clone(),
                    item.value.clone(),
                )
            })
            .collect();
        assert_eq!(current_metadata, displayed_metadata);
        assert_eq!(app.window_level.current, displayed_window);
        assert_eq!(app.window_level.default, displayed_default_window);
        assert_eq!(app.window_level.value_range, displayed_value_range);
        assert_eq!(app.window_level.customized, displayed_window_customized);
        assert_eq!(app.window_level.available, displayed_window_available);
        assert_eq!(app.window_level.active_preset, displayed_window_preset);
        assert_eq!(app.viewport_transform, displayed_viewport_transform);
        assert_eq!(app.selected_dicom_frame_index, 0);
        assert_eq!(app.selected_dicom_frame_count, 1);
        assert_eq!(app.last_slice_by_series.get(&(0, 0, 0)), Some(&1));
        assert_eq!(app.playback.direction, 1);
        assert!(
            app.error_message
                .as_deref()
                .is_some_and(|message| { message.starts_with("Failed to open DICOM:") })
        );
    }

    #[test]
    fn entering_3d_for_a_new_selection_is_debounced_by_one_frame() {
        let first_path = temporary_file("debounce-first");
        let second_path = temporary_file("debounce-second");
        write_single_pixel_dicom(&first_path);
        write_single_pixel_dicom(&second_path);
        let mut index = build_for_file(&first_path).unwrap();
        index.patients[0].studies[0].series_groups[0]
            .slices
            .push(SliceItem {
                path: second_path.clone(),
                display_name: "second".to_owned(),
                frame_index: 0,
                instance_number: Some(2),
                sort_position: Some(1.0),
            });

        let context = egui::Context::default();
        let mut app = DicronApp {
            dicom_index: Some(index),
            ..Default::default()
        };
        assert!(app.load_slice_by_indices(&context, 0, 0, 0, 0));
        app.three_d.view_mode = ViewMode::ThreeD(ThreeDMode::Mpr);

        app.ensure_volume_for_selection(&context);

        assert_eq!(app.three_d.pending_selection_frames, 1);
        assert!(app.three_d.job.request().is_none());

        app.ensure_volume_for_selection(&context);

        assert_eq!(app.three_d.pending_selection_frames, 0);
        assert_eq!(
            app.three_d.job.request().map(|request| request.series_key),
            Some((0, 0, 0))
        );

        app.leave_three_d();
    }

    #[test]
    fn loading_a_different_series_fits_the_new_image() {
        let valid_path = temporary_file("series-fit");
        write_single_pixel_dicom(&valid_path);
        let mut index = build_for_file(&valid_path).unwrap();
        let second_series = index.patients[0].studies[0].series_groups[0].clone();
        index.patients[0].studies[0]
            .series_groups
            .push(second_series);

        let context = egui::Context::default();
        let mut app = DicronApp {
            dicom_index: Some(index),
            ..Default::default()
        };
        assert!(app.load_slice_by_indices(&context, 0, 0, 0, 0));
        app.viewport_transform = crate::app::state::ViewportTransform {
            zoom: 3.0,
            pan: egui::vec2(20.0, 10.0),
            ..Default::default()
        };

        assert!(app.load_slice_by_indices(&context, 0, 0, 1, 0));

        assert_eq!(
            app.viewport_transform,
            crate::app::state::ViewportTransform::default()
        );
    }
}

#[cfg(test)]
mod three_d_tests {
    use crate::app::state::GpuInfo;
    use crate::dicom::{SeriesVolume, Volume, VolumeAxis, VolumeGeometry};

    use super::loading_tests::{
        TemporaryFile, temporary_file, write_single_pixel_dicom,
        write_single_pixel_dicom_with_window, write_single_pixel_rgb_dicom,
    };
    use super::*;

    const SERIES_KEY: SeriesKey = (0, 0, 0);
    const OTHER_SERIES_KEY: SeriesKey = (0, 0, 1);
    const FINGERPRINT: u64 = 7;

    /// Run one frame of keyboard handling, reporting the events no shortcut
    /// consumed.
    fn press_key(app: &mut DicronApp, key: egui::Key) -> usize {
        press_key_after(app, key, |_| {})
    }

    /// `press_key`, with `setup` run on the frame's context first, as the
    /// widgets drawn before the shortcuts are handled would.
    fn press_key_after(
        app: &mut DicronApp,
        key: egui::Key,
        setup: impl FnOnce(&egui::Context),
    ) -> usize {
        let context = egui::Context::default();
        let raw_input = egui::RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut remaining_events = 0;

        let mut setup = Some(setup);

        let _ = context.run_ui(raw_input, |ui| {
            if let Some(setup) = setup.take() {
                setup(ui.ctx());
            }
            app.handle_keyboard_shortcuts(ui.ctx());
            remaining_events = ui.input(|input_state| input_state.events.len());
        });

        remaining_events
    }

    /// The 2D viewer steps slices and applies the preset digits, also with a
    /// menu open, and leaves Escape alone while no 3D view is being prepared.
    /// A modal dialog or a focused field keeps every key for itself.
    #[test]
    fn two_d_keys_step_slices_and_apply_presets() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("two-d-keys");
        select_series(&mut app, &context, 0);
        let slice = |app: &DicronApp| app.selected_slice.unwrap().slice_index;

        for (key, expected) in [
            (egui::Key::ArrowDown, 1),
            (egui::Key::ArrowUp, 0),
            (egui::Key::End, 1),
            (egui::Key::Home, 0),
            (egui::Key::PageDown, 1),
            (egui::Key::PageUp, 0),
        ] {
            assert_eq!(press_key(&mut app, key), 0, "{key:?}");
            assert_eq!(slice(&app), expected, "{key:?}");
        }

        assert_eq!(press_key(&mut app, egui::Key::Escape), 1);

        let menu_open = |context: &egui::Context| {
            egui::Popup::open_id(context, egui::Id::new("a menu"));
        };
        assert_eq!(press_key_after(&mut app, egui::Key::Num3, menu_open), 0);
        assert_eq!(
            app.window_level.current,
            WindowPreset::Lung.fixed_window().unwrap()
        );
        assert_eq!(
            press_key_after(&mut app, egui::Key::ArrowDown, menu_open),
            0
        );
        assert_eq!(slice(&app), 1);

        let field_focused = |context: &egui::Context| {
            context.memory_mut(|memory| memory.request_focus(egui::Id::new("a field")));
        };
        assert_eq!(
            press_key_after(&mut app, egui::Key::ArrowUp, field_focused),
            1
        );
        assert_eq!(slice(&app), 1);

        app.edit_windowing_dialog.open = true;
        assert_eq!(press_key(&mut app, egui::Key::ArrowUp), 1);
        assert_eq!(slice(&app), 1);
    }

    /// The fingerprint of the selected series, which `app_in_mpr` opened.
    fn selected_fingerprint(app: &DicronApp) -> u64 {
        app.selected_series_identity().unwrap().1
    }

    /// MPR open on the first of two in-memory series, which is selected.
    fn app_in_mpr(slice_spacing_mm: f64) -> DicronApp {
        let mut app = DicronApp::default();
        app.index_in_memory_series(2);
        let (series_key, fingerprint) = app.select_in_memory_series(0);
        app.activate_or_prompt(
            prepared_volume(Some(1.0), slice_spacing_mm),
            series_key,
            fingerprint,
            ThreeDMode::Mpr,
        );

        app
    }

    #[test]
    fn opening_three_d_finds_the_selected_frame_when_instance_and_spatial_orders_disagree() {
        let mut app = DicronApp::default();
        app.index_in_memory_series(1);
        let slices =
            &mut app.dicom_index.as_mut().unwrap().patients[0].studies[0].series_groups[0].slices;
        slices[0].sort_position = Some(1.0);
        slices[1].sort_position = Some(0.0);
        let original_paths: Vec<_> = slices.iter().map(|slice| slice.path.clone()).collect();
        let (key, fingerprint) = app.selected_series_identity().unwrap();
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            key,
            fingerprint,
            ThreeDMode::Mpr,
        );
        assert_eq!(
            app.three_d
                .active
                .as_ref()
                .unwrap()
                .mpr
                .index(VolumeAxis::Z),
            1
        );
        assert_eq!(app.selected_slice.unwrap().slice_index, 0);
        assert_eq!(
            app.selected_series_slices()
                .unwrap()
                .iter()
                .map(|slice| slice.path.clone())
                .collect::<Vec<_>>(),
            original_paths
        );
    }

    fn prepared_volume(slice_thickness_mm: Option<f64>, slice_spacing_mm: f64) -> PreparedVolume {
        prepared_volume_of(slice_thickness_mm, slice_spacing_mm, vec![0.0; 8])
    }

    /// A 2 x 2 x 2 volume holding `values`.
    fn prepared_volume_of(
        slice_thickness_mm: Option<f64>,
        slice_spacing_mm: f64,
        values: Vec<f32>,
    ) -> PreparedVolume {
        let volume = Arc::new(Volume::new(2, 2, 2, [1.0, 1.0, slice_spacing_mm], values));
        let geometry = VolumeGeometry {
            inverted: false,
            row_direction: [1.0, 0.0, 0.0],
            column_direction: [0.0, 1.0, 0.0],
            slice_direction: [0.0, 0.0, 1.0],
            slice_thickness_mm,
            slice_spacing_mm,
            slice_count: 2,
            uneven_slice_offsets_mm: None,
        };

        PreparedVolume {
            series: Arc::new(SeriesVolume {
                volume: Arc::clone(&volume),
                geometry,
            }),
            mpr: volume,
            gpu: None,
        }
    }

    /// A volume that states no window of its own must open on its own values,
    /// not on a Hounsfield window that would clamp an MR or PET flat.
    #[test]
    fn a_series_without_a_window_opens_on_the_volumes_own_range() {
        let volume = Volume::new(
            2,
            2,
            2,
            [1.0, 1.0, 1.0],
            vec![100.0, 400.0, 800.0, 1100.0, 100.0, 100.0, 100.0, 100.0],
        );

        let window = value_range_window(&volume).unwrap();

        assert_eq!(window.center, 600.0);
        assert_eq!(window.width, 1000.0);

        let ct = Volume::new(
            2,
            2,
            2,
            [1.0, 1.0, 1.0],
            vec![-1024.0, 3071.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        let window = value_range_window(&ct).unwrap();
        assert_eq!(window.center, 1023.5);
        assert_eq!(window.width, 4095.0);

        // A flat volume carries no range, so the last resort still applies.
        let flat = Volume::new(2, 2, 2, [1.0, 1.0, 1.0], vec![7.0; 8]);
        assert_eq!(value_range_window(&flat), None);
    }

    /// Volume rendering opens on the CT preset only for CT; any other
    /// modality opens in Grayscale, on its own window.
    #[test]
    fn the_volume_rendering_preset_follows_the_modality() {
        for (modality, preset) in [
            (Some("CT"), TransferPreset::CtSoftTissue),
            (Some("MR"), TransferPreset::Grayscale),
            (None, TransferPreset::Grayscale),
        ] {
            let mut app = DicronApp::default();
            app.metadata.overlay = Some(crate::dicom::DicomOverlayMetadata {
                modality: modality.map(str::to_owned),
                ..Default::default()
            });

            app.activate_or_prompt(
                prepared_volume(Some(1.0), 1.0),
                SERIES_KEY,
                FINGERPRINT,
                ThreeDMode::VolumeRendering,
            );

            assert_eq!(
                app.three_d.active.as_ref().unwrap().render.preset,
                preset,
                "{modality:?}"
            );
        }
    }

    /// A series that states no window opens 3D on the range of the frame the
    /// 2D viewer shows, as 2D displays it, not on the range of the whole
    /// volume, which one bright outlier elsewhere would stretch.
    #[test]
    fn a_series_without_a_window_opens_on_the_shown_frames_range() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("frame-range-window");
        // The shown frame holds a single value, 128, and states no window.
        let (series_key, fingerprint) = select_series(&mut app, &context, 0);

        app.activate_or_prompt(
            prepared_volume_of(
                Some(1.0),
                1.0,
                vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 5000.0],
            ),
            series_key,
            fingerprint,
            ThreeDMode::Mpr,
        );

        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.window,
            WindowLevel {
                center: 128.0,
                width: 1.0
            }
        );
    }

    /// The planes open on the window the shown frame states, as 2D does, and
    /// a window the user set on the series in 2D wins over it.
    #[test]
    fn the_planes_open_on_the_window_the_series_shows_in_two_d() {
        let context = egui::Context::default();
        let (mut app, files) = app_with_three_series("stated-window");
        for path in &files[..4] {
            write_single_pixel_dicom_with_window(path, Some(("40", "400")));
        }
        let open = |app: &mut DicronApp, series_index| {
            let (series_key, fingerprint) = select_series(app, &context, series_index);
            app.activate_or_prompt(
                prepared_volume_of(
                    Some(1.0),
                    1.0,
                    vec![100.0, 1100.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                ),
                series_key,
                fingerprint,
                ThreeDMode::Mpr,
            );
            app.three_d.active.as_ref().unwrap().mpr.window
        };

        assert_eq!(
            open(&mut app, 0),
            WindowLevel {
                center: 40.0,
                width: 400.0
            }
        );

        app.leave_three_d();
        select_series(&mut app, &context, 1);
        app.apply_window_preset(&context, WindowPreset::Lung);
        assert_eq!(
            open(&mut app, 1),
            WindowLevel {
                center: -400.0,
                width: 1600.0
            }
        );
    }

    /// A MONOCHROME1 series keeps its stored values, so the window it shows in
    /// 2D applies to the 3D planes unchanged, with the same numbers in the
    /// WL and WW fields.
    #[test]
    fn an_inverted_volume_keeps_the_window_it_was_given() {
        let mut app = DicronApp::default();
        app.index_in_memory_series(1);
        let (series_key, fingerprint) = app.select_in_memory_series(0);
        let window = WindowLevel {
            center: 200.0,
            width: 100.0,
        };
        app.window_level.by_series.insert(
            series_key,
            crate::app::state::SavedWindowLevel {
                window,
                preset: None,
            },
        );
        let mut prepared = prepared_volume_of(
            Some(1.0),
            1.0,
            vec![100.0, 1100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0],
        );
        Arc::get_mut(&mut prepared.series)
            .unwrap()
            .geometry
            .inverted = true;

        app.activate_or_prompt(prepared, series_key, fingerprint, ThreeDMode::Mpr);

        assert_eq!(app.three_d.active.as_ref().unwrap().mpr.window, window);
    }

    /// A build the user navigated away from must never replace the workspace,
    /// and must never pull the viewer into 3D behind their back: everything on
    /// screen would then name a different series from the one being shown.
    #[test]
    fn a_build_for_an_abandoned_selection_is_kept_rather_than_shown() {
        let mut app = app_in_mpr(1.0);
        let other_series = app.select_in_memory_series(1);
        let selected = app.select_in_memory_series(0);
        // The selected series' files changed since the build was started.
        let earlier_files = (selected.0, selected.1 + 1);

        for abandoned in [other_series, earlier_files] {
            land_build(
                &mut app,
                abandoned,
                Ok(VolumeJobOutput::Built(prepared_volume(Some(1.0), 2.0))),
            );

            // The workspace is untouched and the stray volume is kept for later.
            let active = app.three_d.active.as_ref().unwrap();
            assert_eq!((active.series_key, active.fingerprint), selected);
            assert_eq!(active.prepared.series.geometry.slice_spacing_mm, 1.0);
            assert!(app.three_d.cached_matches(abandoned.0, abandoned.1));
        }

        // A build of exactly the selected series is shown.
        land_build(
            &mut app,
            selected,
            Ok(VolumeJobOutput::Built(prepared_volume(Some(1.0), 2.0))),
        );
        let active = app.three_d.active.as_ref().unwrap();
        assert_eq!((active.series_key, active.fingerprint), selected);
        assert_eq!(active.prepared.series.geometry.slice_spacing_mm, 2.0);
        assert_eq!(shown_series(&app), Some(selected.0));
    }

    /// An app over three series of one study: series 0 and 1 have two images
    /// each and can be reconstructed, series 2 is a single image.
    fn app_with_three_series(label: &str) -> (DicronApp, Vec<TemporaryFile>) {
        let files: Vec<_> = (0..5)
            .map(|index| temporary_file(&format!("{label}-{index}")))
            .collect();
        for path in &files {
            write_single_pixel_dicom(path);
        }

        let mut index = build_for_file(&files[0]).unwrap();
        let template = index.patients[0].studies[0].series_groups[0].clone();
        index.patients[0].studies[0].series_groups = [&files[0..2], &files[2..4], &files[4..]]
            .into_iter()
            .map(|paths| {
                let mut series = template.clone();
                series.slices = paths
                    .iter()
                    .enumerate()
                    .map(|(position, path)| SliceItem {
                        path: path.to_path_buf(),
                        display_name: format!("slice {position}"),
                        frame_index: 0,
                        instance_number: Some(position as i32 + 1),
                        sort_position: Some(position as f64),
                    })
                    .collect();
                series
            })
            .collect();

        let app = DicronApp {
            dicom_index: Some(index),
            ..Default::default()
        };

        (app, files)
    }

    /// Select a series as a click in the tree does, returning its identity.
    fn select_series(
        app: &mut DicronApp,
        context: &egui::Context,
        series_index: usize,
    ) -> (SeriesKey, u64) {
        assert!(app.load_slice_by_indices(context, 0, 0, series_index, 0));
        app.selected_series_identity().unwrap()
    }

    /// Hand the app a finished build of the series, as the job poll does.
    fn land_build(
        app: &mut DicronApp,
        (series_key, fingerprint): (SeriesKey, u64),
        output: Result<VolumeJobOutput, String>,
    ) {
        app.receive_volume_job_output(
            &VolumeJobRequest {
                series_key,
                fingerprint,
                requested_mode: ThreeDMode::Mpr,
                kind: VolumeJobKind::Build {
                    slices: Arc::from(Vec::new()),
                },
            },
            output,
        );
    }

    /// The series the 3D body shows, while a 3D mode is open.
    fn shown_series(app: &DicronApp) -> Option<SeriesKey> {
        app.three_d.view_mode.three_d_mode()?;
        app.three_d.active.as_ref().map(|active| active.series_key)
    }

    /// A reconstruction that failed must be remembered, or the next frame
    /// starts it again and the app spawns a worker thread per frame.
    #[test]
    fn a_failed_build_is_not_started_again_for_the_same_series() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("failed-not-restarted");
        let first = select_series(&mut app, &context, 0);
        land_build(&mut app, first, Err("unreadable slice".to_owned()));
        app.three_d.view_mode = ViewMode::ThreeD(ThreeDMode::Mpr);

        for _ in 0..2 {
            app.ensure_volume_for_selection(&context);
            assert!(app.three_d.job.request().is_none());
        }
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);

        // Opening another document clears the record, so the same series is
        // tried again from scratch.
        app.clear_three_d_state();
        assert!(app.three_d.build_failure(first.0, first.1).is_none());
        app.three_d.view_mode = ViewMode::ThreeD(ThreeDMode::Mpr);
        for _ in 0..2 {
            app.ensure_volume_for_selection(&context);
        }
        assert!(app.three_d.job_matches(first.0, first.1));
        app.leave_three_d();
    }

    /// Returning to a series whose low-resolution warning was declined asks
    /// again, instead of leaving the previous series on screen under its name.
    #[test]
    fn reselecting_a_declined_series_inside_3d_asks_again() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("revisit-declined");
        let first = select_series(&mut app, &context, 0);
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            first.0,
            first.1,
            ThreeDMode::Mpr,
        );
        let second = select_series(&mut app, &context, 1);
        land_build(
            &mut app,
            second,
            Ok(VolumeJobOutput::Built(prepared_volume(Some(5.0), 5.0))),
        );
        app.cancel_low_resolution_prompt();
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);

        select_series(&mut app, &context, 0);
        app.open_three_d_mode(&context, ThreeDMode::Mpr);
        assert_eq!(shown_series(&app), Some(first.0));

        select_series(&mut app, &context, 1);
        app.ensure_volume_for_selection(&context);

        let prompt = app
            .three_d
            .low_resolution_prompt
            .as_ref()
            .expect("the warning is asked again");
        assert_eq!((prompt.series_key, prompt.fingerprint), second);
        app.cancel_low_resolution_prompt();
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
    }

    /// A volume that finished after the user moved on is shown once they
    /// select its series again inside 3D.
    #[test]
    fn reselecting_a_kept_volume_inside_3d_shows_it() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("revisit-kept");
        let second = select_series(&mut app, &context, 1);
        let first = select_series(&mut app, &context, 0);
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            first.0,
            first.1,
            ThreeDMode::Mpr,
        );
        land_build(
            &mut app,
            second,
            Ok(VolumeJobOutput::Built(prepared_volume(Some(1.0), 1.0))),
        );
        assert!(app.three_d.cached_matches(second.0, second.1));

        select_series(&mut app, &context, 1);
        app.ensure_volume_for_selection(&context);

        assert_eq!(shown_series(&app), Some(second.0));
        assert!(app.three_d.job.request().is_none());
    }

    /// A series that failed to build returns to 2D with its reason every time
    /// it is selected inside 3D, and is not decoded again.
    #[test]
    fn reselecting_a_failed_series_inside_3d_returns_to_two_d() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("revisit-failed");
        let first = select_series(&mut app, &context, 0);
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            first.0,
            first.1,
            ThreeDMode::Mpr,
        );
        let second = select_series(&mut app, &context, 1);
        land_build(&mut app, second, Err("unreadable slice".to_owned()));
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);

        select_series(&mut app, &context, 0);
        app.open_three_d_mode(&context, ThreeDMode::Mpr);
        assert_eq!(shown_series(&app), Some(first.0));
        // The first report was read and dismissed; the second visit reports
        // the failure again.
        app.error_message = None;

        select_series(&mut app, &context, 1);
        app.ensure_volume_for_selection(&context);

        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
        assert_eq!(
            app.error_message.as_deref(),
            Some("Failed to prepare 3D: unreadable slice")
        );
        assert!(app.three_d.job.request().is_none());
    }

    #[test]
    fn reselecting_an_unsupported_series_inside_3d_returns_to_two_d() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("revisit-unsupported");
        let first = select_series(&mut app, &context, 0);
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            first.0,
            first.1,
            ThreeDMode::Mpr,
        );

        for _ in 0..2 {
            select_series(&mut app, &context, 2);
            app.ensure_volume_for_selection(&context);

            assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
            assert_eq!(
                app.error_message.as_deref(),
                Some(THREE_D_UNSUPPORTED_MESSAGE)
            );

            select_series(&mut app, &context, 0);
            app.open_three_d_mode(&context, ThreeDMode::Mpr);
            assert_eq!(shown_series(&app), Some(first.0));
        }
    }

    /// A build that succeeds clears the record of an earlier failure, such as
    /// one made while the series was still being copied.
    #[test]
    fn a_successful_build_forgets_an_earlier_failure() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("failure-forgotten");
        let first = select_series(&mut app, &context, 0);
        land_build(&mut app, first, Err("still copying".to_owned()));
        assert!(app.three_d.build_failure(first.0, first.1).is_some());

        land_build(
            &mut app,
            first,
            Ok(VolumeJobOutput::Built(prepared_volume(Some(1.0), 1.0))),
        );

        assert!(app.three_d.build_failure(first.0, first.1).is_none());
        assert_eq!(shown_series(&app), Some(first.0));
    }

    /// Only a failed decode says a series cannot be reconstructed.
    #[test]
    fn a_failed_resample_leaves_the_series_reconstructable() {
        let mut app = app_in_mpr(1.0);
        let series = Arc::clone(&app.three_d.active.as_ref().unwrap().prepared.series);

        app.receive_volume_job_output(
            &VolumeJobRequest {
                series_key: SERIES_KEY,
                fingerprint: FINGERPRINT,
                requested_mode: ThreeDMode::Mpr,
                kind: VolumeJobKind::Resample {
                    series,
                    target_mm: 2.0,
                    quality: MprQuality::Iso(2.0),
                },
            },
            Err("3D preparation stopped unexpectedly.".to_owned()),
        );

        assert!(app.three_d.build_failure(SERIES_KEY, FINGERPRINT).is_none());
        assert_eq!(shown_series(&app), Some(SERIES_KEY));
    }

    /// Selecting a kept volume in Volume Rendering prepares its texture once,
    /// rather than cancelling and restarting a texture job on every frame.
    #[test]
    fn a_reselected_series_gets_one_texture_job() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("texture-once");
        app.three_d.gpu = Some(GpuInfo {
            max_texture_dimension_3d: 64,
        });
        app.three_d.gpu_available = true;
        let second = select_series(&mut app, &context, 1);
        let first = select_series(&mut app, &context, 0);
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            first.0,
            first.1,
            ThreeDMode::VolumeRendering,
        );
        land_build(
            &mut app,
            second,
            Ok(VolumeJobOutput::Built(prepared_volume(Some(1.0), 1.0))),
        );
        select_series(&mut app, &context, 1);

        let mut first_start = None;
        for _ in 0..5 {
            app.ensure_volume_for_selection(&context);
            app.ensure_gpu_volume(&context);

            let request = app.three_d.job.request().expect("a texture job runs");
            assert_eq!((request.series_key, request.fingerprint), second);
            let started_at = app.three_d.job.progress().unwrap().started_at;
            assert_eq!(
                *first_start.get_or_insert(started_at),
                started_at,
                "the texture job was restarted"
            );
        }

        app.leave_three_d();
    }

    /// A texture job runs only for a workspace without a texture, and its
    /// texture is installed only in the workspace it was prepared for.
    #[test]
    fn a_texture_job_serves_only_the_workspace_it_was_made_for() {
        let context = egui::Context::default();
        let mut app = app_in_mpr(1.0);
        app.three_d.gpu = Some(GpuInfo {
            max_texture_dimension_3d: 64,
        });
        app.three_d.view_mode = ViewMode::ThreeD(ThreeDMode::VolumeRendering);
        let selected = app.selected_series_identity().unwrap();
        let other_series = app.select_in_memory_series(1);
        app.select_in_memory_series(0);
        let source = Volume::new(2, 2, 2, [1.0; 3], vec![0.0; 8]);
        let texture = || Arc::new(GpuVolume::from_volume(&source, 64));
        let land_texture = |app: &mut DicronApp, (series_key, fingerprint), gpu| {
            app.receive_volume_job_output(
                &VolumeJobRequest {
                    series_key,
                    fingerprint,
                    requested_mode: ThreeDMode::VolumeRendering,
                    kind: VolumeJobKind::GpuUpload {
                        series: Arc::clone(&app.three_d.active.as_ref().unwrap().prepared.series),
                        max_texture_dimension_3d: 64,
                        max_voxels: 1000,
                    },
                },
                Ok(VolumeJobOutput::Gpu(gpu)),
            );
        };

        // Textures prepared for another series, or for earlier files of this
        // one, are not installed.
        for elsewhere in [other_series, (selected.0, selected.1 + 1)] {
            land_texture(&mut app, elsewhere, texture());
            assert!(app.three_d.active.as_ref().unwrap().prepared.gpu.is_none());
        }

        let gpu = texture();
        land_texture(&mut app, selected, Arc::clone(&gpu));
        let installed = app.three_d.active.as_ref().unwrap().prepared.gpu.as_ref();
        assert!(installed.is_some_and(|installed| Arc::ptr_eq(installed, &gpu)));

        // A workspace with its texture needs no job, however many frames pass.
        for _ in 0..3 {
            app.ensure_gpu_volume(&context);
            assert!(app.three_d.job.request().is_none());
        }
    }

    /// Returning to the series on screen stops the build the selection
    /// started on its way elsewhere.
    #[test]
    fn returning_to_the_shown_series_cancels_the_build_of_the_other() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("return-cancels");
        let first = select_series(&mut app, &context, 0);
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            first.0,
            first.1,
            ThreeDMode::Mpr,
        );
        let second = select_series(&mut app, &context, 1);
        for _ in 0..2 {
            app.ensure_volume_for_selection(&context);
        }
        assert!(app.three_d.job_matches(second.0, second.1));

        select_series(&mut app, &context, 0);
        app.ensure_volume_for_selection(&context);

        assert!(app.three_d.job.request().is_none());
        assert_eq!(shown_series(&app), Some(first.0));
    }

    /// A texture the device refused is prepared again at half its voxels for
    /// the workspace showing it; a refusal for any other volume changes
    /// nothing, and three refusals end the attempts for this workspace only.
    #[test]
    fn a_refused_texture_is_prepared_again_smaller() {
        let context = egui::Context::default();
        let mut app = app_in_mpr(1.0);
        app.three_d.gpu = Some(GpuInfo {
            max_texture_dimension_3d: 64,
        });
        app.three_d.view_mode = ViewMode::ThreeD(ThreeDMode::VolumeRendering);
        let source = Volume::new(2, 2, 2, [1.0; 3], vec![0.0; 8]);
        let shown = |app: &mut DicronApp| {
            let gpu = Arc::new(GpuVolume::from_volume(&source, 64));
            app.three_d.active.as_mut().unwrap().prepared.gpu = Some(Arc::clone(&gpu));
            gpu.id
        };

        let id = shown(&mut app);
        app.receive_gpu_refusal(id + 1, 1000);
        assert!(app.three_d.active.as_ref().unwrap().prepared.gpu.is_some());

        app.receive_gpu_refusal(id, 1000);
        assert!(app.three_d.active.as_ref().unwrap().prepared.gpu.is_none());
        app.ensure_gpu_volume(&context);
        let request = app
            .three_d
            .job
            .request()
            .expect("a smaller texture is prepared");
        assert!(matches!(
            request.kind,
            VolumeJobKind::GpuUpload {
                max_voxels: 500,
                ..
            }
        ));
        app.three_d.job.cancel();

        for voxels in [400, 150] {
            let id = shown(&mut app);
            app.receive_gpu_refusal(id, voxels);
        }
        app.ensure_gpu_volume(&context);
        assert!(app.three_d.active.as_ref().unwrap().gpu_budget.exhausted());
        assert!(app.three_d.job.request().is_none());
    }

    /// A series over the MPR budget is built with a downsampled copy for the
    /// planes; once another quality is shown, that copy is let go.
    #[test]
    fn an_unshown_downsampled_copy_is_released() {
        let context = egui::Context::default();
        let mut app = app_in_mpr(1.0);
        let active = app.three_d.active.as_mut().unwrap();
        let copy = Arc::new(Volume::new(1, 1, 1, [2.0, 2.0, 2.0], vec![0.0]));
        let copy_alive = Arc::downgrade(&copy);
        active.prepared.mpr = Arc::clone(&copy);
        active.mpr.install_volume(copy);

        app.set_mpr_quality(&context, MprQuality::Original);

        assert!(copy_alive.upgrade().is_none());
    }

    /// Starting a build for a new series drops a volume kept for another one:
    /// it would be replaced when the build lands anyway.
    #[test]
    fn a_new_build_drops_a_volume_kept_for_another_series() {
        let context = egui::Context::default();
        let mut app = DicronApp::default();
        app.index_in_memory_series(2);
        let first = app.select_in_memory_series(0);
        app.three_d.cached = Some((first.0, first.1, prepared_volume(Some(1.0), 1.0)));

        let (series_key, fingerprint) = app.select_in_memory_series(1);
        app.start_volume_build(&context, series_key, fingerprint, ThreeDMode::Mpr);

        assert!(app.three_d.cached.is_none());
        app.three_d.job.cancel();
    }

    /// A volume rendering texture is wanted only while volume rendering is on
    /// screen; switching to MPR stops one being prepared.
    #[test]
    fn leaving_volume_rendering_stops_its_texture_job() {
        let context = egui::Context::default();
        let mut app = app_in_mpr(1.0);
        app.three_d.gpu = Some(GpuInfo {
            max_texture_dimension_3d: 64,
        });
        app.three_d.gpu_available = true;
        app.open_three_d_mode(&context, ThreeDMode::VolumeRendering);
        assert!(app.three_d.job.request().is_some());

        app.open_three_d_mode(&context, ThreeDMode::Mpr);

        assert!(app.three_d.job.request().is_none());
        assert_eq!(app.three_d.view_mode, ViewMode::ThreeD(ThreeDMode::Mpr));
    }

    /// While a new selection waits for its build, the workspace it replaces
    /// gets no texture job of its own.
    #[test]
    fn a_workspace_being_replaced_gets_no_texture_job() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("texture-replaced");
        app.three_d.gpu = Some(GpuInfo {
            max_texture_dimension_3d: 64,
        });
        app.three_d.gpu_available = true;
        let first = select_series(&mut app, &context, 0);
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            first.0,
            first.1,
            ThreeDMode::VolumeRendering,
        );
        select_series(&mut app, &context, 1);

        app.ensure_volume_for_selection(&context);
        app.ensure_gpu_volume(&context);

        assert_eq!(app.three_d.pending_selection_frames, 1);
        assert!(app.three_d.job.request().is_none());
    }

    #[test]
    fn opening_3d_without_a_series_reports_an_error() {
        let context = egui::Context::default();
        let mut app = DicronApp::default();

        app.open_three_d_mode(&context, ThreeDMode::Mpr);

        assert_eq!(
            app.error_message.as_deref(),
            Some("Open a DICOM series to use 3D.")
        );
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
    }

    #[test]
    fn volume_rendering_needs_a_gpu() {
        let context = egui::Context::default();
        let mut app = DicronApp::default();
        app.index_in_memory_series(1);
        let (series_key, fingerprint) = app.select_in_memory_series(0);
        app.three_d.cached = Some((series_key, fingerprint, prepared_volume(Some(1.0), 1.0)));
        app.three_d.gpu_available = false;

        app.open_three_d_mode(&context, ThreeDMode::VolumeRendering);

        assert_eq!(app.error_message.as_deref(), Some(GPU_UNAVAILABLE_MESSAGE));
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
        assert!(app.three_d.job.request().is_none());
        assert!(app.three_d.cached_matches(series_key, fingerprint));

        // With a GPU the same request opens the kept volume.
        app.error_message = None;
        app.three_d.gpu_available = true;
        app.open_three_d_mode(&context, ThreeDMode::VolumeRendering);

        assert_eq!(app.error_message, None);
        assert_eq!(
            app.three_d.view_mode,
            ViewMode::ThreeD(ThreeDMode::VolumeRendering)
        );
        assert!(app.three_d.active_matches(series_key, fingerprint));
    }

    #[test]
    fn a_usable_volume_opens_the_requested_mode() {
        let mut app = DicronApp::default();

        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            SERIES_KEY,
            FINGERPRINT,
            ThreeDMode::Mip,
        );

        assert!(app.three_d.low_resolution_prompt.is_none());
        assert_eq!(app.three_d.view_mode, ViewMode::ThreeD(ThreeDMode::Mip));
        let active = app.three_d.active.as_ref().unwrap();
        assert!(!active.low_resolution);
        assert_eq!(active.mpr.window, DEFAULT_MPR_WINDOW);
        assert_eq!(active.mpr.indices, [1, 1, 1]);
    }

    #[test]
    fn a_low_resolution_volume_warns_until_the_user_accepts_it() {
        let mut app = DicronApp::default();

        app.activate_or_prompt(
            prepared_volume(Some(5.0), 5.0),
            SERIES_KEY,
            FINGERPRINT,
            ThreeDMode::Mpr,
        );

        assert!(app.three_d.low_resolution_prompt.is_some());
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);

        app.cancel_low_resolution_prompt();

        assert!(app.three_d.low_resolution_prompt.is_none());
        assert!(app.three_d.active.is_none());
        assert!(app.three_d.low_resolution_accepted.is_empty());
        let (cached_key, cached_fingerprint, cached) = app.three_d.cached.take().unwrap();
        assert_eq!((cached_key, cached_fingerprint), (SERIES_KEY, FINGERPRINT));

        app.activate_or_prompt(cached, SERIES_KEY, FINGERPRINT, ThreeDMode::Mpr);
        assert!(app.three_d.low_resolution_prompt.is_some());

        app.accept_low_resolution_prompt();

        assert!(app.three_d.low_resolution_prompt.is_none());
        assert!(app.three_d.cached.is_none());
        assert!(
            app.three_d
                .low_resolution_accepted
                .contains(&(SERIES_KEY, FINGERPRINT))
        );
        assert_eq!(app.three_d.view_mode, ViewMode::ThreeD(ThreeDMode::Mpr));
        assert!(app.three_d.active.as_ref().unwrap().low_resolution);
    }

    #[test]
    fn leaving_3d_keeps_the_prepared_workspace() {
        let mut app = DicronApp::default();
        app.activate_or_prompt(
            prepared_volume(None, 1.0),
            SERIES_KEY,
            FINGERPRINT,
            ThreeDMode::Mpr,
        );

        app.leave_three_d();

        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
        assert!(app.three_d.active.is_some());
    }

    #[test]
    fn clearing_the_document_drops_every_3d_workspace() {
        let mut app = DicronApp::default();
        app.activate_or_prompt(
            prepared_volume(Some(5.0), 5.0),
            SERIES_KEY,
            FINGERPRINT,
            ThreeDMode::Mpr,
        );
        app.accept_low_resolution_prompt();

        app.clear_loaded_dicom_state();

        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
        assert!(app.three_d.active.is_none());
        assert!(app.three_d.cached.is_none());
        assert!(app.three_d.low_resolution_prompt.is_none());
        assert!(app.three_d.low_resolution_accepted.is_empty());
    }

    #[test]
    fn resetting_the_view_recenters_every_plane() {
        let mut app = DicronApp::default();
        app.activate_or_prompt(
            prepared_volume(None, 1.0),
            SERIES_KEY,
            FINGERPRINT,
            ThreeDMode::Mpr,
        );
        app.three_d.active.as_mut().unwrap().mpr.indices = [0, 0, 0];

        app.reset_three_d_view();

        assert_eq!(app.three_d.active.as_ref().unwrap().mpr.indices, [1, 1, 1]);
    }

    #[test]
    fn escape_leaves_the_3d_workspace() {
        let mut app = app_in_mpr(1.0);

        let remaining_events = press_key(&mut app, egui::Key::Escape);

        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
        assert_eq!(remaining_events, 0);
        assert!(app.three_d.active.is_some());
    }

    #[test]
    fn escape_stays_out_of_3d_while_the_low_resolution_prompt_is_open() {
        let mut app = app_in_mpr(1.0);
        app.activate_or_prompt(
            prepared_volume(Some(5.0), 5.0),
            OTHER_SERIES_KEY,
            FINGERPRINT,
            ThreeDMode::Mpr,
        );
        assert!(app.three_d.low_resolution_prompt.is_some());

        let remaining_events = press_key(&mut app, egui::Key::Escape);

        assert_eq!(app.three_d.view_mode, ViewMode::ThreeD(ThreeDMode::Mpr));
        assert!(app.three_d.low_resolution_prompt.is_some());
        assert!(app.three_d.active.is_some());
        // The modal closes itself, so the key must survive the shortcut pass.
        assert_eq!(remaining_events, 1);
    }

    #[test]
    fn slice_keys_move_the_head_to_foot_plane_only_in_mpr_and_mip() {
        let mut app = app_in_mpr(1.0);
        let superior_inferior = app
            .three_d
            .active
            .as_ref()
            .unwrap()
            .prepared
            .series
            .geometry
            .superior_inferior_axis();
        assert_eq!(superior_inferior, VolumeAxis::Z);
        let indices = |app: &DicronApp| app.three_d.active.as_ref().unwrap().mpr.indices;

        // The slices stack towards the head: End is index 0, at the feet.
        press_key(&mut app, egui::Key::End);
        assert_eq!(indices(&app), [1, 1, 0]);

        press_key(&mut app, egui::Key::ArrowUp);
        assert_eq!(indices(&app), [1, 1, 1]);

        press_key(&mut app, egui::Key::ArrowDown);
        assert_eq!(indices(&app), [1, 1, 0]);

        app.three_d.view_mode = ViewMode::ThreeD(ThreeDMode::VolumeRendering);
        let remaining_events = press_key(&mut app, egui::Key::ArrowUp);

        assert_eq!(indices(&app), [1, 1, 0]);
        assert_eq!(remaining_events, 1);
    }

    /// Up heads for the head whichever way the series was stacked: here the
    /// slices run towards the feet, so the head is index 0.
    #[test]
    fn slice_keys_head_for_the_head_in_a_feet_first_stack() {
        let mut app = app_in_mpr(1.0);
        Arc::get_mut(&mut app.three_d.active.as_mut().unwrap().prepared.series)
            .unwrap()
            .geometry
            .slice_direction = [0.0, 0.0, -1.0];
        let indices = |app: &DicronApp| app.three_d.active.as_ref().unwrap().mpr.indices;

        press_key(&mut app, egui::Key::Home);
        assert_eq!(indices(&app), [1, 1, 0]);

        press_key(&mut app, egui::Key::ArrowDown);
        assert_eq!(indices(&app), [1, 1, 1]);
    }

    /// In a sagittal series the head-to-foot axis is the volume's y, with its
    /// index running towards the feet, and only that plane moves.
    #[test]
    fn slice_keys_step_the_head_to_foot_axis_of_a_sagittal_series() {
        let mut app = app_in_mpr(1.0);
        let geometry = &mut Arc::get_mut(&mut app.three_d.active.as_mut().unwrap().prepared.series)
            .unwrap()
            .geometry;
        geometry.row_direction = [0.0, 1.0, 0.0];
        geometry.column_direction = [0.0, 0.0, -1.0];
        geometry.slice_direction = [-1.0, 0.0, 0.0];
        let indices = |app: &DicronApp| app.three_d.active.as_ref().unwrap().mpr.indices;
        let [x, _, z] = indices(&app);

        press_key(&mut app, egui::Key::Home);
        assert_eq!(indices(&app), [x, 0, z]);

        press_key(&mut app, egui::Key::ArrowDown);
        assert_eq!(indices(&app), [x, 1, z]);

        press_key(&mut app, egui::Key::ArrowUp);
        assert_eq!(indices(&app), [x, 0, z]);
    }

    /// A first visit opens on the slice the 2D viewer shows; a return to a
    /// series opens its workspace as the user left it, window included.
    #[test]
    fn a_workspace_opens_where_the_user_was() {
        let mut app = DicronApp::default();
        app.index_in_memory_series(2);
        let first = app.select_in_memory_series(0);
        let open = |app: &mut DicronApp, (series_key, fingerprint)| {
            app.activate_or_prompt(
                prepared_volume(Some(1.0), 1.0),
                series_key,
                fingerprint,
                ThreeDMode::Mpr,
            );
        };

        // Slice 0 of two: the lower of the two planes, not the centre.
        open(&mut app, first);
        let active = app.three_d.active.as_mut().unwrap();
        assert_eq!(active.mpr.indices[VolumeAxis::Z.index()], 0);

        active.mpr.indices = [0, 1, 1];
        active.mpr.window = WindowLevel {
            center: 300.0,
            width: 1500.0,
        };
        active.mpr.view_transforms[0].zoom = 3.0;
        active.render.rotate_by(egui::vec2(100.0, 0.0));
        let edited_yaw = active.render.yaw_deg;

        let second = app.select_in_memory_series(1);
        open(&mut app, second);
        app.select_in_memory_series(0);
        open(&mut app, first);

        let active = app.three_d.active.as_ref().unwrap();
        assert_eq!(active.mpr.indices, [0, 1, 1]);
        assert_eq!(
            active.mpr.window,
            WindowLevel {
                center: 300.0,
                width: 1500.0
            }
        );
        assert_eq!(active.mpr.view_transforms[0].zoom, 3.0);
        assert_eq!(active.render.yaw_deg, edited_yaw);

        // A new document forgets them.
        app.clear_three_d_state();
        assert!(app.three_d.saved_views.is_empty());
    }

    /// Unevenly spaced slices are interpolated onto more planes than there
    /// are slices, and the 2D slice is found where it lies, not by its number.
    #[test]
    fn a_first_visit_finds_the_two_d_slice_among_interpolated_planes() {
        let mut app = DicronApp::default();
        app.index_in_memory_series(1);
        let (series_key, fingerprint) = app.select_in_memory_series(0);
        app.selected_slice = Some(SliceSelection::new(0, 0, 0, 1));
        // Two slices 2 mm apart, interpolated onto three planes 1 mm apart.
        let volume = Arc::new(Volume::new(2, 2, 3, [1.0; 3], vec![0.0; 12]));
        let mut geometry = prepared_volume(Some(1.0), 1.0).series.geometry.clone();
        geometry.uneven_slice_offsets_mm = Some(vec![0.0, 2.0]);
        let prepared = PreparedVolume {
            series: Arc::new(SeriesVolume {
                volume: Arc::clone(&volume),
                geometry,
            }),
            mpr: volume,
            gpu: None,
        };

        app.activate_or_prompt(prepared, series_key, fingerprint, ThreeDMode::Mpr);

        let active = app.three_d.active.as_ref().unwrap();
        assert_eq!(active.mpr.indices[VolumeAxis::Z.index()], 2);
    }

    /// A 3D view requested from 2D is abandoned by Escape, or by selecting
    /// another series; a new slice of the same series keeps it coming.
    #[test]
    fn a_build_requested_from_two_d_can_be_abandoned() {
        let context = egui::Context::default();
        let mut app = DicronApp::default();
        app.index_in_memory_series(2);
        let (series_key, fingerprint) = app.select_in_memory_series(0);
        let start = |app: &mut DicronApp| {
            app.start_volume_build(&context, series_key, fingerprint, ThreeDMode::Mpr)
        };

        start(&mut app);
        assert_eq!(press_key(&mut app, egui::Key::Escape), 0);
        assert!(app.three_d.job.request().is_none());

        start(&mut app);
        app.selected_slice = Some(SliceSelection::new(0, 0, 0, 1));
        app.ensure_volume_for_selection(&context);
        assert!(app.three_d.job.request().is_some());

        app.select_in_memory_series(1);
        app.ensure_volume_for_selection(&context);
        assert!(app.three_d.job.request().is_none());
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
    }

    /// egui releases a focused field as soon as Escape arrives, so the Escape
    /// that ends an edit must not also leave 3D.
    #[test]
    fn escape_from_a_field_stays_in_three_d() {
        let mut app = app_in_mpr(1.0);
        app.keyboard_owned_last_frame = true;

        assert_eq!(press_key(&mut app, egui::Key::Escape), 1);
        assert_eq!(app.three_d.view_mode, ViewMode::ThreeD(ThreeDMode::Mpr));
    }

    #[test]
    fn leaving_three_d_releases_a_drag_in_progress() {
        let mut app = app_in_mpr(1.0);
        app.three_d.active.as_mut().unwrap().mpr.drag =
            Some(crate::app::three_d_state::MprDrag::Pan {
                view: VolumeAxis::Z,
            });

        app.leave_three_d();

        assert!(app.three_d.active.as_ref().unwrap().mpr.drag.is_none());
    }

    /// The 2D window dialog would edit a window the 3D views no longer show.
    #[test]
    fn opening_three_d_closes_the_two_d_window_dialog() {
        let mut app = DicronApp::default();
        app.edit_windowing_dialog.open = true;

        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            SERIES_KEY,
            FINGERPRINT,
            ThreeDMode::Mpr,
        );

        assert!(!app.edit_windowing_dialog.open);
    }

    /// Reset View restores the mode on screen and leaves the others alone.
    #[test]
    fn reset_view_resets_only_the_mode_on_screen() {
        let mut app = app_in_mpr(1.0);
        let active = app.three_d.active.as_mut().unwrap();
        active.mpr.indices = [0, 0, 0];
        active.render.rotate_by(egui::vec2(100.0, 0.0));

        app.three_d.view_mode = ViewMode::ThreeD(ThreeDMode::VolumeRendering);
        app.reset_three_d_view();
        let active = app.three_d.active.as_ref().unwrap();
        assert_eq!(active.render.yaw_deg, 0.0);
        assert_eq!(active.mpr.indices, [0, 0, 0]);

        app.three_d
            .active
            .as_mut()
            .unwrap()
            .render
            .rotate_by(egui::vec2(100.0, 0.0));
        app.three_d.view_mode = ViewMode::ThreeD(ThreeDMode::Mpr);
        app.reset_three_d_view();
        let active = app.three_d.active.as_ref().unwrap();
        assert_eq!(active.mpr.indices, [1, 1, 1]);
        assert_ne!(active.render.yaw_deg, 0.0);
    }

    /// While another series is selected, its volume still to come, the keys
    /// must not step the planes of the workspace being replaced.
    #[test]
    fn slice_keys_wait_for_the_selected_series_workspace() {
        let mut app = app_in_mpr(1.0);
        let before = app.three_d.active.as_ref().unwrap().mpr.indices;
        app.select_in_memory_series(1);

        press_key(&mut app, egui::Key::ArrowUp);
        assert_eq!(app.three_d.active.as_ref().unwrap().mpr.indices, before);

        // Escape still leaves 3D.
        press_key(&mut app, egui::Key::Escape);
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
    }

    #[test]
    fn preset_digits_do_not_window_the_mpr_planes() {
        let mut app = app_in_mpr(1.0);

        let remaining_events = press_key(&mut app, egui::Key::Num6);

        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.window,
            DEFAULT_MPR_WINDOW
        );
        assert_eq!(remaining_events, 1);
    }

    #[test]
    fn three_d_needs_a_grayscale_series_of_single_frame_slices() {
        let first_path = temporary_file("supports-first");
        let second_path = temporary_file("supports-second");
        write_single_pixel_dicom(&first_path);
        write_single_pixel_dicom(&second_path);
        let mut index = build_for_file(&first_path).unwrap();
        let context = egui::Context::default();
        let mut app = DicronApp {
            dicom_index: Some(index.clone()),
            ..Default::default()
        };
        assert!(app.load_slice_by_indices(&context, 0, 0, 0, 0));

        // One image is not a volume.
        assert!(!app.series_supports_three_d());

        index.patients[0].studies[0].series_groups[0]
            .slices
            .push(SliceItem {
                path: second_path.clone(),
                display_name: "second".to_owned(),
                frame_index: 0,
                instance_number: Some(2),
                sort_position: Some(1.0),
            });
        app.dicom_index = Some(index.clone());
        assert!(app.series_supports_three_d());

        // Frames of one file stack in time, not in space.
        index.patients[0].studies[0].series_groups[0].slices[1].frame_index = 1;
        app.dicom_index = Some(index);
        assert!(!app.series_supports_three_d());

        // Colour images have no window to reconstruct a grayscale volume with.
        let colour = [
            temporary_file("supports-colour-first"),
            temporary_file("supports-colour-second"),
        ];
        for path in &colour {
            write_single_pixel_rgb_dicom(path);
        }
        let mut colour_index = build_for_file(&colour[0]).unwrap();
        colour_index.patients[0].studies[0].series_groups[0]
            .slices
            .push(SliceItem {
                path: colour[1].to_path_buf(),
                display_name: "second".to_owned(),
                frame_index: 0,
                instance_number: Some(2),
                sort_position: Some(1.0),
            });
        let mut app = DicronApp {
            dicom_index: Some(colour_index),
            ..Default::default()
        };
        assert!(app.load_slice_by_indices(&context, 0, 0, 0, 0));
        assert!(!app.series_supports_three_d());
    }

    /// A mode chosen inside 3D while the selected series is still being
    /// built is shown at once and kept for the build; from 2D the build is
    /// redirected but the viewer stays in 2D until it lands.
    #[test]
    fn a_mode_chosen_during_a_build_is_shown_and_kept() {
        let context = egui::Context::default();
        let (mut app, _files) = app_with_three_series("mode-during-build");
        let first = select_series(&mut app, &context, 0);
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            first.0,
            first.1,
            ThreeDMode::Mpr,
        );
        let second = select_series(&mut app, &context, 1);
        for _ in 0..2 {
            app.ensure_volume_for_selection(&context);
        }
        assert!(app.three_d.job_matches(second.0, second.1));

        app.open_three_d_mode(&context, ThreeDMode::Mip);

        assert_eq!(app.three_d.view_mode, ViewMode::ThreeD(ThreeDMode::Mip));
        assert_eq!(
            app.three_d.job.request().unwrap().requested_mode,
            ThreeDMode::Mip
        );
        assert!(app.three_d.job_matches(second.0, second.1));

        app.leave_three_d();
        app.open_three_d_mode(&context, ThreeDMode::Mpr);
        app.open_three_d_mode(&context, ThreeDMode::Mip);
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
        assert_eq!(
            app.three_d.job.request().unwrap().requested_mode,
            ThreeDMode::Mip
        );
        app.three_d.job.cancel();
    }

    #[test]
    fn switching_mode_reuses_the_workspace_of_the_selected_series() {
        let slice_path = temporary_file("view-mode-toggle");
        write_single_pixel_dicom(&slice_path);
        let index = build_for_file(&slice_path).unwrap();
        let context = egui::Context::default();
        let mut app = DicronApp {
            dicom_index: Some(index),
            ..Default::default()
        };
        assert!(app.load_slice_by_indices(&context, 0, 0, 0, 0));
        let fingerprint = series_fingerprint(app.selected_series_slices().unwrap());
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            SERIES_KEY,
            fingerprint,
            ThreeDMode::Mpr,
        );

        app.open_three_d_mode(&context, ThreeDMode::Mip);

        assert_eq!(app.three_d.view_mode, ViewMode::ThreeD(ThreeDMode::Mip));
        assert!(app.three_d.job.request().is_none());
        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.volume_generation,
            0
        );

        app.leave_three_d();
        app.open_three_d_mode(&context, ThreeDMode::Mpr);

        assert_eq!(app.three_d.view_mode, ViewMode::ThreeD(ThreeDMode::Mpr));
        assert!(app.three_d.job.request().is_none());
    }

    #[test]
    fn materialised_qualities_apply_without_a_job() {
        let context = egui::Context::default();
        let mut app = app_in_mpr(1.0);

        app.set_mpr_quality(&context, MprQuality::Iso(1.0));

        // The volume already has 1 mm voxels, so nothing is rebuilt.
        let active = app.three_d.active.as_ref().unwrap();
        assert_eq!(active.mpr.quality, MprQuality::Iso(1.0));
        assert_eq!(active.mpr.volume_generation, 0);
        assert!(app.three_d.job.request().is_none());

        app.set_mpr_quality(&context, MprQuality::Original);

        // The source volume is the one already on screen, so the planes keep
        // their crosshair, slabs and zoom instead of being re-centred.
        let active = app.three_d.active.as_ref().unwrap();
        assert_eq!(active.mpr.quality, MprQuality::Original);
        assert_eq!(active.mpr.volume_generation, 0);
        assert!(app.three_d.job.request().is_none());
    }

    #[test]
    fn a_new_quality_target_runs_a_resample_job() {
        let context = egui::Context::default();
        let mut app = app_in_mpr(2.0);

        app.set_mpr_quality(&context, MprQuality::Iso(1.5));

        let request = app.three_d.job.request().unwrap();
        assert_eq!(request.series_key, SERIES_KEY);
        assert_eq!(request.fingerprint, selected_fingerprint(&app));
        assert!(matches!(request.kind, VolumeJobKind::Resample { .. }));
        assert_eq!(
            app.three_d.job.progress().map(|progress| progress.stage),
            Some(crate::app::volume_job::VolumeJobStage::Resampling)
        );
        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.volume_generation,
            0,
            "the current planes stay on screen while the job runs"
        );

        // Another grid that needs a job is refused while this one runs; a job
        // cannot be interrupted part way, so a second would run beside it.
        let started_at = app.three_d.job.progress().unwrap().started_at;
        app.set_mpr_quality(&context, MprQuality::Iso(1.0));
        let request = app.three_d.job.request().unwrap();
        assert!(matches!(
            request.kind,
            VolumeJobKind::Resample {
                quality: MprQuality::Iso(1.5),
                ..
            }
        ));
        assert_eq!(app.three_d.job.progress().unwrap().started_at, started_at);
        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.quality,
            MprQuality::Auto
        );

        app.leave_three_d();
    }

    #[test]
    fn a_resampled_volume_takes_the_quality_it_was_made_for() {
        let context = egui::Context::default();
        let mut app = app_in_mpr(2.0);
        app.set_mpr_quality(&context, MprQuality::Iso(1.5));
        // The choice takes effect when its volume is in, not before.
        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.quality,
            MprQuality::Auto
        );
        let request = |series_key| VolumeJobRequest {
            series_key,
            fingerprint: selected_fingerprint(&app),
            requested_mode: ThreeDMode::Mpr,
            kind: VolumeJobKind::Resample {
                series: Arc::clone(&app.three_d.active.as_ref().unwrap().prepared.series),
                target_mm: 1.5,
                quality: MprQuality::Iso(1.5),
            },
        };
        let resampled = Arc::new(
            app.three_d
                .active
                .as_ref()
                .unwrap()
                .prepared
                .series
                .volume
                .resample_isotropic(1.5),
        );

        let for_other_series = request(OTHER_SERIES_KEY);
        let for_this_series = request(SERIES_KEY);

        // Not for a workspace of another series.
        app.install_resampled_volume(&for_other_series, Arc::clone(&resampled));
        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.volume_generation,
            0
        );

        app.install_resampled_volume(&for_this_series, resampled);

        let active = app.three_d.active.as_ref().unwrap();
        assert_eq!(active.mpr.quality, MprQuality::Iso(1.5));
        assert_eq!(active.mpr.volume_generation, 1);
        assert_eq!(active.mpr.volume.spacing, [1.0, 1.0, 4.0 / 3.0]);
        assert_eq!(active.mpr.volume.physical_size_mm(), [2.0, 2.0, 4.0]);

        app.leave_three_d();
    }

    /// A resample abandoned on the way leaves the quality naming the grid that
    /// is shown, and choosing the same quality again starts it again.
    #[test]
    fn a_cancelled_resample_leaves_the_quality_of_the_shown_grid() {
        let context = egui::Context::default();
        let mut app = app_in_mpr(2.0);
        app.set_mpr_quality(&context, MprQuality::Iso(1.5));

        app.leave_three_d();
        app.open_three_d_mode(&context, ThreeDMode::Mpr);
        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.quality,
            MprQuality::Auto
        );

        app.set_mpr_quality(&context, MprQuality::Iso(1.5));
        assert!(app.three_d.job.request().is_some());

        // A grid already in memory applies at once and supersedes the resample.
        app.set_mpr_quality(&context, MprQuality::Original);
        assert!(app.three_d.job.request().is_none());
        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.quality,
            MprQuality::Original
        );
    }

    #[test]
    fn cancelling_a_warning_never_displays_a_different_series_from_selection() {
        let path = temporary_file("cancel-selection");
        let second_path = temporary_file("cancel-selection-second");
        write_single_pixel_dicom(&path);
        write_single_pixel_dicom(&second_path);
        let mut index = build_for_file(&path).unwrap();
        index.patients[0].studies[0].series_groups[0]
            .slices
            .push(SliceItem {
                path: second_path.clone(),
                display_name: "second".to_owned(),
                frame_index: 0,
                instance_number: Some(2),
                sort_position: Some(1.0),
            });
        let second = index.patients[0].studies[0].series_groups[0].clone();
        index.patients[0].studies[0].series_groups.push(second);
        let context = egui::Context::default();
        let mut app = DicronApp {
            dicom_index: Some(index),
            ..Default::default()
        };
        assert!(app.load_slice_by_indices(&context, 0, 0, 1, 0));
        assert!(app.series_supports_three_d());
        let (selected_key, fingerprint) = app.selected_series_identity().unwrap();
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            SERIES_KEY,
            FINGERPRINT,
            ThreeDMode::Mpr,
        );
        app.activate_or_prompt(
            prepared_volume(Some(5.0), 5.0),
            selected_key,
            fingerprint,
            ThreeDMode::Mpr,
        );
        assert!(app.three_d.low_resolution_prompt.is_some());
        app.cancel_low_resolution_prompt();
        app.ensure_volume_for_selection(&context);
        let displayed_key = app.three_d.active.as_ref().map(|active| active.series_key);
        let still_3d = app.three_d.view_mode.three_d_mode().is_some();
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
        assert!(app.three_d.cached_matches(selected_key, fingerprint));
        assert!(
            !still_3d || displayed_key == Some(selected_key),
            "selected {selected_key:?}, displayed {displayed_key:?}, still in 3D: {still_3d}"
        );
    }

    #[test]
    fn a_failed_build_returns_to_the_selected_series_in_two_d() {
        let path = temporary_file("failed-volume-first");
        let missing = temporary_file("failed-volume-missing");
        write_single_pixel_dicom(&path);
        let mut index = build_for_file(&path).unwrap();
        let mut second = index.patients[0].studies[0].series_groups[0].clone();
        second.slices.push(SliceItem {
            path: missing.to_path_buf(),
            display_name: "missing".to_owned(),
            frame_index: 0,
            instance_number: Some(2),
            sort_position: Some(1.0),
        });
        index.patients[0].studies[0].series_groups.push(second);
        let context = egui::Context::default();
        let mut app = DicronApp {
            dicom_index: Some(index),
            ..Default::default()
        };
        assert!(app.load_slice_by_indices(&context, 0, 0, 1, 0));
        let (key, fingerprint) = app.selected_series_identity().unwrap();
        app.activate_or_prompt(
            prepared_volume(Some(1.0), 1.0),
            SERIES_KEY,
            FINGERPRINT,
            ThreeDMode::Mpr,
        );
        app.start_volume_build(&context, key, fingerprint, ThreeDMode::Mpr);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while app.three_d.job.request().is_some() && std::time::Instant::now() < deadline {
            app.receive_volume_job_messages(&context);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            app.three_d.job.request().is_none(),
            "the failed job did not finish"
        );
        assert!(app.three_d.build_failure(key, fingerprint).is_some());
        assert_eq!(app.three_d.view_mode, ViewMode::TwoD);
        assert_eq!(app.current_series_key(), Some(key));
        assert!(
            app.error_message
                .as_deref()
                .unwrap()
                .starts_with("Failed to prepare 3D:")
        );
    }

    #[test]
    fn rounded_resampled_grids_are_reused_without_another_job() {
        let context = egui::Context::default();
        let mut app = app_in_mpr(2.0);
        let active = app.three_d.active.as_mut().unwrap();
        let cached = Arc::new(active.prepared.series.volume.resample_isotropic(1.5));
        active.prepared.mpr = Arc::clone(&cached);
        app.set_mpr_quality(&context, MprQuality::Iso(1.5));
        assert!(app.three_d.job.request().is_none());
        let active = app.three_d.active.as_ref().unwrap();
        assert!(Arc::ptr_eq(&active.mpr.volume, &cached));
        assert_eq!(active.mpr.volume.physical_size_mm(), [2.0, 2.0, 4.0]);
    }
}
