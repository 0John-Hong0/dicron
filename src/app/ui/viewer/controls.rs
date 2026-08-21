//! Viewer navigation, slice scrollbar, and playback controls.

use std::time::Instant;

use eframe::egui;

use super::ViewerControlAction;
use crate::app::DicronApp;
use crate::app::state::{
    PLAYBACK_MAX_FPS, PLAYBACK_MIN_FPS, PlaybackLoopMode, WINDOW_PRESETS, WindowPreset,
};
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
    let window_level_available = app.window_level.is_available();
    let image_loaded = app.loaded_texture.is_some();
    let active_window_preset = app.window_level.active_preset;
    let preset_button_label = format!("Preset: {}", app.window_level.active_preset_label());

    theme::toolbar_row(ui, |ui| {
        if ui
            .add_enabled(image_loaded, egui::Button::new("Reset Display"))
            .clicked()
        {
            action = Some(ViewerControlAction::ResetView);
        }

        ui.separator();

        if ui
            .add_enabled(window_level_available, egui::Button::new("Edit WL"))
            .clicked()
        {
            action = Some(ViewerControlAction::OpenEditWindowing);
        }

        show_window_preset_menu(
            ui,
            &preset_button_label,
            window_level_available,
            active_window_preset,
            &mut action,
        );

        ui.separator();

        if ui
            .add_enabled(image_loaded, egui::Button::new("Flip H"))
            .clicked()
        {
            action = Some(ViewerControlAction::FlipHorizontal);
        }

        if ui
            .add_enabled(image_loaded, egui::Button::new("Flip V"))
            .clicked()
        {
            action = Some(ViewerControlAction::FlipVertical);
        }

        if ui
            .add_enabled(image_loaded, egui::Button::new("Rotate 90°"))
            .clicked()
        {
            action = Some(ViewerControlAction::RotateClockwise);
        }

        ui.separator();
        app.show_autoplay_controls(ui);
    });

    action
}

fn show_window_preset_menu(
    ui: &mut egui::Ui,
    button_label: &str,
    window_level_available: bool,
    active_preset: Option<WindowPreset>,
    action: &mut Option<ViewerControlAction>,
) {
    if !window_level_available {
        ui.add_enabled(false, egui::Button::new(button_label));
        return;
    }

    egui::containers::menu::MenuButton::from_button(egui::Button::new(button_label))
        .config(egui::containers::menu::MenuConfig::new().style(theme::popup_menu_style))
        .ui(ui, |ui| {
            ui.set_min_width(190.0);

            for preset in WINDOW_PRESETS {
                let response = ui.selectable_label(
                    active_preset == Some(preset),
                    format!("{}  {}", preset.shortcut(), preset.label()),
                );
                let response = if let Some(window) = preset.fixed_window() {
                    response
                        .on_hover_text(format!("WL {:.0} / WW {:.0}", window.center, window.width))
                } else {
                    response
                };

                if response.clicked() {
                    *action = Some(ViewerControlAction::ApplyWindowPreset(preset));
                    ui.close();
                }
            }
        });
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

        show_autoplay_loop_menu(ui, can_autoplay, &mut self.playback.loop_mode);
    }
}

fn show_autoplay_loop_menu(
    ui: &mut egui::Ui,
    can_autoplay: bool,
    loop_mode: &mut PlaybackLoopMode,
) {
    let chevron_size = egui::vec2(theme::SPACE_SM, theme::SPACE_XS);
    let chevron_id = ui.id().with("autoplay_loop_menu_chevron");
    let button = egui::Button::new(loop_mode.label())
        .right_text(egui::Atom::custom(chevron_id, chevron_size))
        .min_size(egui::vec2(
            ui.spacing().combo_width,
            ui.spacing().interact_size.y,
        ));

    if !can_autoplay {
        let response = ui.add_enabled(false, button);
        paint_menu_chevron(ui, &response, false);
        return;
    }

    let (response, _) = egui::containers::menu::MenuButton::from_button(button)
        .config(egui::containers::menu::MenuConfig::new().style(theme::popup_menu_style))
        .ui(ui, |ui| {
            ui.selectable_value(
                loop_mode,
                PlaybackLoopMode::StopAtEnd,
                PlaybackLoopMode::StopAtEnd.label(),
            );
            ui.selectable_value(
                loop_mode,
                PlaybackLoopMode::Loop,
                PlaybackLoopMode::Loop.label(),
            );
            ui.selectable_value(
                loop_mode,
                PlaybackLoopMode::PingPong,
                PlaybackLoopMode::PingPong.label(),
            );
        });

    paint_menu_chevron(ui, &response, true);
}

fn paint_menu_chevron(ui: &egui::Ui, response: &egui::Response, enabled: bool) {
    if !ui.is_rect_visible(response.rect) {
        return;
    }

    let icon_center = egui::pos2(
        response.rect.right() - ui.spacing().button_padding.x - theme::SPACE_XS,
        response.rect.center().y,
    );
    let icon_rect = egui::Rect::from_center_size(
        icon_center,
        egui::vec2(theme::SPACE_SM * 0.7, theme::SPACE_XS),
    );
    let color = if enabled {
        ui.style().interact(response).fg_stroke.color
    } else {
        ui.visuals()
            .weak_text_color()
            .gamma_multiply(ui.visuals().disabled_alpha())
    };

    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            icon_rect.left_top(),
            icon_rect.right_top(),
            icon_rect.center_bottom(),
        ],
        color,
        egui::Stroke::NONE,
    ));
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
