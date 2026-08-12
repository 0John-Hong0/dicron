//! Viewer navigation, slice scrollbar, and playback controls.

use std::time::Instant;

use eframe::egui;

use super::ViewerControlAction;
use crate::app::DicronApp;
use crate::app::state::{PLAYBACK_MAX_FPS, PLAYBACK_MIN_FPS, PlaybackLoopMode};
use crate::theme;

impl PlaybackLoopMode {
    fn label(self) -> &'static str {
        match self {
            Self::StopAtEnd => "Stop at end",
            Self::Loop => "Loop",
            Self::PingPong => "Ping-pong",
        }
    }
}

pub(super) fn show_control_row(
    app: &mut DicronApp,
    ui: &mut egui::Ui,
) -> Option<ViewerControlAction> {
    let mut action = None;

    ui.horizontal_wrapped(|ui| {
        if ui.button("Reset WL").clicked() {
            action = Some(ViewerControlAction::ResetWindowLevel);
        }

        ui.separator();
        app.show_autoplay_controls(ui);
    });

    action
}

impl DicronApp {
    pub(super) fn show_autoplay_controls(&mut self, ui: &mut egui::Ui) {
        let can_autoplay = self.current_slice_count().is_some_and(|count| count > 1);

        if !can_autoplay {
            self.stop_autoplay();
        }

        let play_button_text = if self.playback.enabled {
            "Pause"
        } else {
            "Play"
        };

        if ui
            .add_enabled(can_autoplay, egui::Button::new(play_button_text))
            .clicked()
        {
            if self.playback.enabled {
                self.stop_autoplay();
            } else {
                self.start_autoplay();
                ui.ctx().request_repaint();
            }
        }

        ui.label("FPS");

        let fps_response = ui.add_enabled(
            can_autoplay,
            egui::DragValue::new(&mut self.playback.fps)
                .range(PLAYBACK_MIN_FPS..=PLAYBACK_MAX_FPS)
                .speed(0.25),
        );

        if fps_response.changed() {
            self.playback.fps = self.playback.fps.clamp(PLAYBACK_MIN_FPS, PLAYBACK_MAX_FPS);
            self.playback.last_tick = Some(Instant::now());
        }

        ui.add_enabled_ui(can_autoplay, |ui| {
            egui::ComboBox::from_id_salt("autoplay_loop_mode")
                .selected_text(self.playback.loop_mode.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.playback.loop_mode,
                        PlaybackLoopMode::StopAtEnd,
                        PlaybackLoopMode::StopAtEnd.label(),
                    );
                    ui.selectable_value(
                        &mut self.playback.loop_mode,
                        PlaybackLoopMode::Loop,
                        PlaybackLoopMode::Loop.label(),
                    );
                    ui.selectable_value(
                        &mut self.playback.loop_mode,
                        PlaybackLoopMode::PingPong,
                        PlaybackLoopMode::PingPong.label(),
                    );
                });
        });
    }
}

pub(super) fn show_slice_scrollbar(
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

    let track_width = theme::SPACE_XS;
    let thumb_width = theme::SPACE_SM;
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
