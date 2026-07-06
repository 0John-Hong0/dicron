use super::*;

impl DicronApp {
    pub(super) fn show_viewer_panel(&mut self, ui: &mut egui::Ui) {
        let raw_available_size = ui.available_size();

        let safe_available_size =
            egui::vec2(raw_available_size.x.max(1.0), raw_available_size.y.max(1.0));

        let (panel_rect, _panel_response) =
            ui.allocate_exact_size(safe_available_size, egui::Sense::hover());

        let has_playback_bar = self.current_slice_count().is_some_and(|slice_count| {
            slice_count > 1 && panel_rect.width() > 220.0 && panel_rect.height() > 120.0
        });
        let playback_bar_height = if has_playback_bar {
            ui.spacing().interact_size.y + PLAYBACK_BAR_VERTICAL_PADDING * 2.0
        } else {
            0.0
        };

        let viewer_panel_rect = egui::Rect::from_min_max(
            panel_rect.min,
            egui::pos2(
                panel_rect.right(),
                panel_rect.bottom() - playback_bar_height,
            ),
        );

        let playback_bar_rect = egui::Rect::from_min_max(
            egui::pos2(panel_rect.left(), viewer_panel_rect.bottom()),
            panel_rect.max,
        );

        let has_series_scrollbar = self
            .current_slice_count()
            .is_some_and(|slice_count| slice_count > 1)
            && viewer_panel_rect.width() > 48.0
            && viewer_panel_rect.height() > 48.0;

        let scrollbar_width = if has_series_scrollbar { 16.0 } else { 0.0 };

        let viewer_width = (viewer_panel_rect.width() - scrollbar_width).max(1.0);
        let viewer_height = viewer_panel_rect.height().max(1.0);

        let viewer_rect = egui::Rect::from_min_size(
            viewer_panel_rect.min,
            egui::vec2(viewer_width, viewer_height),
        );

        let scrollbar_rect = egui::Rect::from_min_size(
            egui::pos2(viewer_rect.right(), viewer_panel_rect.top()),
            egui::vec2(scrollbar_width.max(1.0), viewer_height),
        );

        let is_pointer_over_viewer = ui.input(|input_state| {
            input_state
                .pointer
                .hover_pos()
                .is_some_and(|pointer_position| viewer_rect.contains(pointer_position))
        });
        let viewer_response = ui.interact(
            viewer_rect,
            ui.id().with("window_level_drag_area"),
            egui::Sense::click_and_drag(),
        );

        if is_pointer_over_viewer {
            self.handle_viewer_scroll(ui.ctx(), ui);
        } else {
            self.viewer_scroll_accumulator = 0.0;
        }

        if self.loaded_texture.is_some() && viewer_response.dragged_by(egui::PointerButton::Primary)
        {
            self.handle_window_level_drag(ui.ctx(), &viewer_response);
        }

        if let Some(loaded_texture) = &self.loaded_texture {
            let texture_size = loaded_texture.size_vec2();
            let fitted_image_size = fit_image_to_available_space(texture_size, viewer_rect.size());

            if fitted_image_size.x > 0.0 && fitted_image_size.y > 0.0 {
                let image_rect =
                    egui::Rect::from_center_size(viewer_rect.center(), fitted_image_size);

                ui.put(
                    image_rect,
                    egui::Image::from_texture(loaded_texture).fit_to_exact_size(fitted_image_size),
                );
            }
        } else {
            ui.painter().text(
                viewer_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Open a DICOM file or folder.",
                egui::FontId::proportional(16.0),
                ui.visuals().text_color(),
            );
        }

        if has_series_scrollbar
            && let (Some(selected_slice_index), Some(slice_count)) =
                (self.current_slice_index(), self.current_slice_count())
            && let Some(requested_slice_index) =
                self.show_slice_scrollbar(ui, scrollbar_rect, selected_slice_index, slice_count)
        {
            self.jump_to_slice(ui.ctx(), requested_slice_index);
        }

        if has_playback_bar {
            self.show_playback_bar(ui, playback_bar_rect);
        }
    }

    pub(super) fn show_autoplay_controls(&mut self, ui: &mut egui::Ui) {
        let can_autoplay = self.current_slice_count().is_some_and(|count| count > 1);

        if !can_autoplay {
            self.stop_autoplay();
        }

        let play_button_text = if self.autoplay_enabled {
            "Pause"
        } else {
            "Play"
        };

        if ui
            .add_enabled(can_autoplay, egui::Button::new(play_button_text))
            .clicked()
        {
            if self.autoplay_enabled {
                self.stop_autoplay();
            } else {
                self.start_autoplay();
                ui.ctx().request_repaint();
            }
        }

        ui.label("FPS");

        let fps_response = ui.add_enabled(
            can_autoplay,
            egui::DragValue::new(&mut self.autoplay_fps)
                .range(MIN_AUTOPLAY_FPS..=MAX_AUTOPLAY_FPS)
                .speed(0.25),
        );

        if fps_response.changed() {
            self.autoplay_fps = self.autoplay_fps.clamp(MIN_AUTOPLAY_FPS, MAX_AUTOPLAY_FPS);
            self.autoplay_last_tick = Some(Instant::now());
        }

        ui.add_enabled_ui(can_autoplay, |ui| {
            egui::ComboBox::from_id_salt("autoplay_loop_mode")
                .selected_text(self.autoplay_loop_mode.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.autoplay_loop_mode,
                        AutoplayLoopMode::StopAtEnd,
                        AutoplayLoopMode::StopAtEnd.label(),
                    );
                    ui.selectable_value(
                        &mut self.autoplay_loop_mode,
                        AutoplayLoopMode::Loop,
                        AutoplayLoopMode::Loop.label(),
                    );
                    ui.selectable_value(
                        &mut self.autoplay_loop_mode,
                        AutoplayLoopMode::PingPong,
                        AutoplayLoopMode::PingPong.label(),
                    );
                });
        });
    }

    pub(super) fn show_playback_bar(&mut self, ui: &mut egui::Ui, playback_bar_rect: egui::Rect) {
        let visuals = ui.visuals();
        let top_stroke = visuals.widgets.noninteractive.bg_stroke;

        ui.painter()
            .rect_filled(playback_bar_rect, 0.0, visuals.panel_fill);
        ui.painter().hline(
            playback_bar_rect.x_range(),
            playback_bar_rect.top(),
            top_stroke,
        );

        let control_height = ui.spacing().interact_size.y;
        let content_rect = egui::Rect::from_center_size(
            playback_bar_rect.center(),
            egui::vec2(
                (playback_bar_rect.width() - PLAYBACK_BAR_HORIZONTAL_PADDING * 2.0).max(1.0),
                control_height,
            ),
        );

        let content_rect = content_rect.translate(egui::vec2(
            0.0,
            (PLAYBACK_BAR_VERTICAL_PADDING * 0.5).round(),
        ));

        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(content_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.label("Playback");
                self.show_autoplay_controls(ui);
            },
        );
    }

    pub(super) fn effective_window(&self) -> Option<DicomWindow> {
        if self.window_customized {
            Some(DicomWindow {
                center: self.window_center,
                width: self.window_width,
            })
        } else {
            None
        }
    }

    /// Modality-appropriate value range for clamping interactive windowing,
    /// derived from the current frame rather than a fixed CT-ish constant.
    pub(super) fn window_value_bounds(&self) -> (f64, f64) {
        self.current_value_range
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

        let (minimum, maximum) = self.window_value_bounds();
        let span = (maximum - minimum).max(1.0);

        self.window_center =
            (self.window_center + drag_motion.y as f64).clamp(minimum - span, maximum + span);
        self.window_width = (self.window_width + drag_motion.x as f64).clamp(1.0, span * 4.0);
        self.window_customized = true;
        self.save_current_series_window_level();
        self.refresh_dicom_texture(context);
    }

    pub(super) fn save_current_series_window_level(&mut self) {
        let Some(series_key) = self.current_series_key() else {
            return;
        };

        self.window_level_by_series.insert(
            series_key,
            WindowLevel {
                center: self.window_center,
                width: self.window_width,
            },
        );
    }

    pub(super) fn clear_current_series_window_level(&mut self) {
        let Some(series_key) = self.current_series_key() else {
            return;
        };

        self.window_level_by_series.remove(&series_key);
    }

    pub(super) fn current_series_window_level(&self) -> Option<WindowLevel> {
        self.window_level_by_series
            .get(&self.current_series_key()?)
            .copied()
    }

    pub(super) fn current_series_key(&self) -> Option<SeriesKey> {
        Some((
            self.selected_patient_index?,
            self.selected_study_index?,
            self.selected_series_index?,
        ))
    }

    pub(super) fn handle_viewer_scroll(&mut self, context: &egui::Context, ui: &egui::Ui) {
        let scroll_delta_y = ui.input(|input_state| input_state.smooth_scroll_delta.y);

        if scroll_delta_y == 0.0 {
            return;
        }

        self.viewer_scroll_accumulator += scroll_delta_y;

        let wheel_steps =
            (self.viewer_scroll_accumulator / VIEWER_SCROLL_SLICE_STEP).trunc() as isize;

        if wheel_steps == 0 {
            return;
        }

        self.viewer_scroll_accumulator -= wheel_steps as f32 * VIEWER_SCROLL_SLICE_STEP;
        self.move_selected_slice(context, -wheel_steps);
    }

    pub(super) fn handle_autoplay(&mut self, context: &egui::Context) {
        if !self.autoplay_enabled {
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

        self.autoplay_fps = self.autoplay_fps.clamp(MIN_AUTOPLAY_FPS, MAX_AUTOPLAY_FPS);

        let frame_interval = Duration::from_secs_f64(1.0 / self.autoplay_fps as f64);
        let now = Instant::now();
        let last_tick = self.autoplay_last_tick.unwrap_or(now);
        let elapsed = now.saturating_duration_since(last_tick);
        let next_repaint_after = if elapsed >= frame_interval {
            self.advance_autoplay(context, slice_count);
            self.autoplay_last_tick = Some(now);

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

        let max_slice_index = slice_count - 1;
        let next_slice_index = match self.autoplay_loop_mode {
            AutoplayLoopMode::StopAtEnd => {
                if current_slice_index >= max_slice_index {
                    self.stop_autoplay();
                    return;
                }

                current_slice_index + 1
            }
            AutoplayLoopMode::Loop => {
                if current_slice_index >= max_slice_index {
                    0
                } else {
                    current_slice_index + 1
                }
            }
            AutoplayLoopMode::PingPong => {
                let next_slice_index = current_slice_index as isize + self.autoplay_direction;

                if next_slice_index > max_slice_index as isize {
                    self.autoplay_direction = -1;
                    max_slice_index.saturating_sub(1)
                } else if next_slice_index < 0 {
                    self.autoplay_direction = 1;
                    1.min(max_slice_index)
                } else {
                    next_slice_index as usize
                }
            }
        };

        self.jump_to_slice(context, next_slice_index);
    }

    pub(super) fn start_autoplay(&mut self) {
        self.autoplay_enabled = true;
        self.autoplay_last_tick = Some(Instant::now());
    }

    pub(super) fn stop_autoplay(&mut self) {
        self.autoplay_enabled = false;
        self.autoplay_last_tick = None;
    }

    pub(super) fn handle_keyboard_shortcuts(&mut self, context: &egui::Context) {
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
                * KEYBOARD_PAGE_SLICE_STEP;
            slice_delta -= input_state
                .count_and_consume_key(egui::Modifiers::NONE, egui::Key::PageUp)
                as isize
                * KEYBOARD_PAGE_SLICE_STEP;

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
        if let (
            Some(patient_index),
            Some(study_index),
            Some(series_index),
            Some(current_slice_index),
        ) = (
            self.selected_patient_index,
            self.selected_study_index,
            self.selected_series_index,
            self.selected_slice_index,
        ) {
            let Some(series_slice_count) = self.get_selected_series_slice_count() else {
                return;
            };

            if series_slice_count == 0 {
                return;
            }

            let max_slice_index = (series_slice_count - 1) as isize;
            let next_slice_index =
                (current_slice_index as isize + direction).clamp(0, max_slice_index) as usize;

            if next_slice_index == current_slice_index {
                return;
            }

            self.load_slice_by_indices(
                context,
                patient_index,
                study_index,
                series_index,
                next_slice_index,
            );

            return;
        }

        self.move_selected_file_frame(context, direction);
    }

    pub(super) fn show_slice_scrollbar(
        &self,
        ui: &mut egui::Ui,
        scrollbar_rect: egui::Rect,
        selected_slice_index: usize,
        series_slice_count: usize,
    ) -> Option<usize> {
        if series_slice_count <= 1 {
            return None;
        }

        let scrollbar_id = ui.id().with("slice_scrollbar");

        let response = ui.interact(scrollbar_rect, scrollbar_id, egui::Sense::click_and_drag());

        let track_width = 4.0;
        let thumb_width = 8.0;
        let minimum_thumb_height = 28.0;

        let track_rect = egui::Rect::from_center_size(
            scrollbar_rect.center(),
            egui::vec2(track_width, scrollbar_rect.height()),
        );

        let thumb_height = (scrollbar_rect.height() / series_slice_count as f32)
            .max(minimum_thumb_height)
            .min(scrollbar_rect.height());

        let max_thumb_top = scrollbar_rect.bottom() - thumb_height;
        let slice_ratio = selected_slice_index as f32 / (series_slice_count - 1) as f32;
        let thumb_top = egui::lerp(scrollbar_rect.top()..=max_thumb_top, slice_ratio);

        let thumb_rect = egui::Rect::from_min_size(
            egui::pos2(scrollbar_rect.center().x - thumb_width / 2.0, thumb_top),
            egui::vec2(thumb_width, thumb_height),
        );

        let visuals = ui.visuals();

        ui.painter()
            .rect_filled(track_rect, 2.0, visuals.widgets.noninteractive.bg_fill);

        let thumb_color = if response.dragged() || response.hovered() {
            visuals.widgets.hovered.bg_fill
        } else {
            visuals.widgets.inactive.bg_fill
        };

        ui.painter().rect_filled(thumb_rect, 4.0, thumb_color);

        if response.clicked() || response.dragged() {
            let pointer_position = response.interact_pointer_pos()?;

            let usable_height = (scrollbar_rect.height() - thumb_height).max(1.0);
            let normalized_position =
                ((pointer_position.y - scrollbar_rect.top() - thumb_height / 2.0) / usable_height)
                    .clamp(0.0, 1.0);

            let requested_slice_index =
                (normalized_position * (series_slice_count - 1) as f32).round() as usize;

            return Some(requested_slice_index);
        }

        None
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
        if let (Some(patient_index), Some(study_index), Some(series_index)) = (
            self.selected_patient_index,
            self.selected_study_index,
            self.selected_series_index,
        ) {
            let Some(series_slice_count) = self.get_selected_series_slice_count() else {
                return;
            };

            if series_slice_count == 0 {
                return;
            }

            let next_slice_index = slice_index.min(series_slice_count - 1);

            if self.selected_slice_index == Some(next_slice_index) {
                return;
            }

            self.load_slice_by_indices(
                context,
                patient_index,
                study_index,
                series_index,
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
