//! Image display, fitting, zooming, panning, and clipping.

use eframe::egui;

use super::{controls, image_texture::fit_image_to_available_space};
use crate::app::DicronApp;
use crate::theme;

const OVERLAY_MARGIN: f32 = theme::SPACE_MD;
const OVERLAY_FONT_SIZE: f32 = 13.0;

pub(super) fn show(app: &mut DicronApp, ui: &mut egui::Ui) {
    let raw_available_size = ui.available_size();

    let safe_available_size =
        egui::vec2(raw_available_size.x.max(1.0), raw_available_size.y.max(1.0));

    let (panel_rect, _panel_response) =
        ui.allocate_exact_size(safe_available_size, egui::Sense::hover());

    let has_series_scrollbar = app
        .current_slice_count()
        .is_some_and(|slice_count| slice_count > 1)
        && panel_rect.width() > 48.0
        && panel_rect.height() > 48.0;

    let scrollbar_width = if has_series_scrollbar {
        theme::SPACE_LG
    } else {
        0.0
    };

    let viewer_width = (panel_rect.width() - scrollbar_width).max(1.0);
    let viewer_height = panel_rect.height().max(1.0);

    let viewer_rect =
        egui::Rect::from_min_size(panel_rect.min, egui::vec2(viewer_width, viewer_height));

    let scrollbar_rect = egui::Rect::from_min_size(
        egui::pos2(viewer_rect.right(), panel_rect.top()),
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

    if has_series_scrollbar
        && let (Some(selected_slice_index), Some(slice_count)) =
            (app.current_slice_index(), app.current_slice_count())
        && let Some(requested_slice_index) =
            controls::show_slice_scrollbar(ui, scrollbar_rect, selected_slice_index, slice_count)
    {
        app.jump_to_slice(ui.ctx(), requested_slice_index);
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

        show_viewer_overlays(app, ui, viewer_rect);
    } else {
        ui.painter().text(
            viewer_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Open a DICOM file or folder.",
            egui::FontId::proportional(16.0),
            ui.visuals().text_color(),
        );
    }
}

fn show_viewer_overlays(app: &DicronApp, ui: &egui::Ui, viewer_rect: egui::Rect) {
    if let (Some(slice_index), Some(slice_count)) =
        (app.current_slice_index(), app.current_slice_count())
        && let Some(slice_text) = slice_overlay_text(slice_index, slice_count)
    {
        paint_overlay_text(
            ui.painter(),
            viewer_rect.left_bottom() + egui::vec2(OVERLAY_MARGIN, -OVERLAY_MARGIN),
            egui::Align2::LEFT_BOTTOM,
            &slice_text,
        );
    }

    let window_level = app.window_level.current();
    let window_text = window_overlay_text(window_level.center, window_level.width);

    paint_overlay_text(
        ui.painter(),
        viewer_rect.right_bottom() + egui::vec2(-OVERLAY_MARGIN, -OVERLAY_MARGIN),
        egui::Align2::RIGHT_BOTTOM,
        &window_text,
    );
}

fn slice_overlay_text(slice_index: usize, slice_count: usize) -> Option<String> {
    (slice_count > 0 && slice_index < slice_count)
        .then(|| format!("{} / {slice_count}", slice_index + 1))
}

fn window_overlay_text(center: f64, width: f64) -> String {
    format!("WL: {center:.0}  WW: {width:.0}")
}

fn paint_overlay_text(
    painter: &egui::Painter,
    position: egui::Pos2,
    anchor: egui::Align2,
    text: &str,
) {
    let font = egui::FontId::monospace(OVERLAY_FONT_SIZE);

    painter.text(
        position + egui::vec2(1.0, 1.0),
        anchor,
        text,
        font.clone(),
        egui::Color32::from_black_alpha(180),
    );
    painter.text(
        position,
        anchor,
        text,
        font,
        egui::Color32::from_white_alpha(210),
    );
}

#[cfg(test)]
mod tests {
    use super::{slice_overlay_text, window_overlay_text};

    #[test]
    fn slice_overlay_uses_human_readable_numbering() {
        assert_eq!(slice_overlay_text(0, 20).as_deref(), Some("1 / 20"));
        assert_eq!(slice_overlay_text(19, 20).as_deref(), Some("20 / 20"));
    }

    #[test]
    fn slice_overlay_rejects_inconsistent_ranges() {
        assert_eq!(slice_overlay_text(0, 0), None);
        assert_eq!(slice_overlay_text(20, 20), None);
    }

    #[test]
    fn window_overlay_uses_the_compact_single_line_layout() {
        assert_eq!(window_overlay_text(510.0, 1091.0), "WL: 510  WW: 1091");
    }
}
