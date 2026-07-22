//! Image display, fitting, zooming, panning, and clipping.

use eframe::egui;

use super::{controls, image_texture::fit_image_to_available_space};
use crate::app::DicronApp;

pub(super) fn show(app: &mut DicronApp, ui: &mut egui::Ui) {
    let raw_available_size = ui.available_size();
    let safe_available_size =
        egui::vec2(raw_available_size.x.max(1.0), raw_available_size.y.max(1.0));
    let (panel_rect, _panel_response) =
        ui.allocate_exact_size(safe_available_size, egui::Sense::hover());

    let has_playback_bar = app.current_slice_count().is_some_and(|slice_count| {
        slice_count > 1 && panel_rect.width() > 220.0 && panel_rect.height() > 120.0
    });
    let playback_bar_height = if has_playback_bar {
        controls::bar_height(ui)
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
    let has_series_scrollbar = app
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
        app.handle_viewer_scroll(ui.ctx(), ui);
    } else {
        app.viewer_scroll_accumulator = 0.0;
    }

    if app.loaded_texture.is_some() && viewer_response.dragged_by(egui::PointerButton::Primary) {
        app.handle_window_level_drag(ui.ctx(), &viewer_response);
    }

    if let Some(loaded_texture) = &app.loaded_texture {
        let texture_size = loaded_texture.size_vec2();
        let fitted_image_size = fit_image_to_available_space(texture_size, viewer_rect.size());

        if fitted_image_size.x > 0.0 && fitted_image_size.y > 0.0 {
            let image_rect = egui::Rect::from_center_size(viewer_rect.center(), fitted_image_size);
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
            (app.current_slice_index(), app.current_slice_count())
        && let Some(requested_slice_index) =
            controls::show_slice_scrollbar(ui, scrollbar_rect, selected_slice_index, slice_count)
    {
        app.jump_to_slice(ui.ctx(), requested_slice_index);
    }

    if has_playback_bar {
        app.show_playback_bar(ui, playback_bar_rect);
    }
}
