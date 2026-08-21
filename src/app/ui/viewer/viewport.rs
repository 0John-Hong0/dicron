//! Image display, fitting, zooming, panning, and clipping.

use std::sync::Arc;

use eframe::egui;

use super::{controls, image_texture::fit_image_to_available_space};
use crate::app::DicronApp;
use crate::app::state::ViewportTransform;
use crate::dicom::{DicomOverlayMetadata, PixelProbeValue};
use crate::theme;

const MIN_VIEWER_ZOOM: f32 = 0.1;
const MAX_VIEWER_ZOOM: f32 = 20.0;
const VIEWER_ZOOM_DRAG_SENSITIVITY: f32 = 0.01;
const OVERLAY_MARGIN: f32 = theme::SPACE_MD;
const OVERLAY_FONT_SIZE: f32 = 13.0;
const OVERLAY_BLOCK_GAP: f32 = theme::SPACE_LG;
const OVERLAY_LINE_GAP: f32 = theme::SPACE_XXS;

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

    if app.loaded_texture.is_some() {
        handle_viewport_transform(
            &mut app.viewport_transform,
            &mut app.viewport_zoom_anchor,
            viewer_rect,
            &viewer_response,
        );

        if viewer_response.dragged_by(egui::PointerButton::Middle) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if viewer_response.dragged_by(egui::PointerButton::Secondary) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
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
            let image_rect =
                transformed_image_rect(viewer_rect, fitted_image_size, app.viewport_transform);
            paint_transformed_image(
                &ui.painter().with_clip_rect(viewer_rect),
                loaded_texture.id(),
                image_rect,
                app.viewport_transform,
            );

            show_viewer_overlays(app, ui, viewer_rect, image_rect, loaded_texture.size());
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
}

fn handle_viewport_transform(
    transform: &mut ViewportTransform,
    zoom_anchor: &mut Option<egui::Pos2>,
    viewer_rect: egui::Rect,
    response: &egui::Response,
) {
    if response.double_clicked_by(egui::PointerButton::Primary) {
        *transform = ViewportTransform::default();
        *zoom_anchor = None;
        return;
    }

    if response.drag_started_by(egui::PointerButton::Secondary) {
        *zoom_anchor = response.interact_pointer_pos();
    }

    if response.dragged_by(egui::PointerButton::Secondary) {
        let anchor = zoom_anchor
            .or_else(|| response.interact_pointer_pos())
            .unwrap_or_else(|| viewer_rect.center());
        zoom_viewport_around_pointer(
            transform,
            response.drag_delta().y,
            viewer_rect.center(),
            anchor,
        );
    } else if response.dragged_by(egui::PointerButton::Middle) {
        *zoom_anchor = None;
        transform.pan += response.drag_delta();
    }

    if response.drag_stopped_by(egui::PointerButton::Secondary) {
        *zoom_anchor = None;
    }
}

fn zoom_viewport_around_pointer(
    transform: &mut ViewportTransform,
    vertical_drag: f32,
    viewer_center: egui::Pos2,
    anchor: egui::Pos2,
) {
    if vertical_drag == 0.0 {
        return;
    }

    let previous_zoom = transform.zoom;
    let zoom_factor = (vertical_drag * VIEWER_ZOOM_DRAG_SENSITIVITY).exp();
    let next_zoom = (previous_zoom * zoom_factor).clamp(MIN_VIEWER_ZOOM, MAX_VIEWER_ZOOM);
    let applied_factor = next_zoom / previous_zoom;
    let anchor_from_center = anchor - viewer_center;

    transform.pan = anchor_from_center + (transform.pan - anchor_from_center) * applied_factor;
    transform.zoom = next_zoom;
}

fn transformed_image_rect(
    viewer_rect: egui::Rect,
    fitted_image_size: egui::Vec2,
    transform: ViewportTransform,
) -> egui::Rect {
    let displayed_size = if transform.rotation_quarters.is_multiple_of(2) {
        fitted_image_size
    } else {
        egui::vec2(fitted_image_size.y, fitted_image_size.x)
    };

    egui::Rect::from_center_size(
        viewer_rect.center() + transform.pan,
        displayed_size * transform.zoom,
    )
}

fn paint_transformed_image(
    painter: &egui::Painter,
    texture_id: egui::TextureId,
    image_rect: egui::Rect,
    transform: ViewportTransform,
) {
    let mut mesh = egui::Mesh::with_texture(texture_id);
    let positions = [
        image_rect.left_top(),
        image_rect.right_top(),
        image_rect.left_bottom(),
        image_rect.right_bottom(),
    ];
    let uvs = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)]
        .map(|(u, v)| transformed_image_uv(u, v, transform));

    for (position, (u, v)) in positions.into_iter().zip(uvs) {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: position,
            uv: egui::pos2(u, v),
            color: egui::Color32::WHITE,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 2, 1, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

fn transformed_image_uv(mut u: f32, mut v: f32, transform: ViewportTransform) -> (f32, f32) {
    if transform.flip_horizontal {
        u = 1.0 - u;
    }
    if transform.flip_vertical {
        v = 1.0 - v;
    }

    match transform.rotation_quarters % 4 {
        0 => (u, v),
        1 => (v, 1.0 - u),
        2 => (1.0 - u, 1.0 - v),
        3 => (1.0 - v, u),
        _ => unreachable!(),
    }
}

fn show_viewer_overlays(
    app: &DicronApp,
    ui: &egui::Ui,
    viewer_rect: egui::Rect,
    image_rect: egui::Rect,
    image_size: [usize; 2],
) {
    let metadata = app.metadata.overlay.as_ref();
    let available_overlay_width =
        (viewer_rect.width() - OVERLAY_MARGIN * 2.0 - OVERLAY_BLOCK_GAP).max(0.0);
    let corner_max_height =
        ((viewer_rect.height() - OVERLAY_MARGIN * 2.0 - OVERLAY_BLOCK_GAP) / 2.0).max(0.0);
    let top_left_text = top_left_overlay_text(metadata);
    let top_right_text = top_right_overlay_text(metadata);
    let (top_left_max_width, top_right_max_width) = paired_overlay_widths(
        ui.painter(),
        top_left_text.as_deref(),
        top_right_text.as_deref(),
        available_overlay_width,
        0.5,
    );

    if let Some(text) = top_left_text {
        paint_overlay_text(
            ui.painter(),
            viewer_rect.left_top() + egui::vec2(OVERLAY_MARGIN, OVERLAY_MARGIN),
            egui::Align2::LEFT_TOP,
            &text,
            top_left_max_width,
            corner_max_height,
        );
    }

    if let Some(text) = top_right_text {
        paint_overlay_text(
            ui.painter(),
            viewer_rect.right_top() + egui::vec2(-OVERLAY_MARGIN, OVERLAY_MARGIN),
            egui::Align2::RIGHT_TOP,
            &text,
            top_right_max_width,
            corner_max_height,
        );
    }

    let bottom_left_text = bottom_left_overlay_text(app, metadata);

    let mut bottom_right_lines = Vec::with_capacity(2);
    if let Some(pointer_position) = ui.input(|input_state| input_state.pointer.hover_pos())
        && let Some(probe_text) = pixel_probe_overlay_text(
            app,
            pointer_position,
            image_rect,
            image_size,
            metadata.and_then(|metadata| metadata.modality.as_deref()),
        )
    {
        bottom_right_lines.push(probe_text);
    }

    if app.window_level.is_available() {
        let window_level = app.window_level.current();
        bottom_right_lines.push(window_overlay_text(window_level.center, window_level.width));
    } else {
        bottom_right_lines.push("RGB".to_owned());
    }

    let bottom_right_text = bottom_right_lines.join("\n");
    let (bottom_left_max_width, bottom_right_max_width) = paired_overlay_widths(
        ui.painter(),
        bottom_left_text.as_deref(),
        Some(&bottom_right_text),
        available_overlay_width,
        0.4,
    );

    if let Some(text) = bottom_left_text {
        paint_overlay_text(
            ui.painter(),
            viewer_rect.left_bottom() + egui::vec2(OVERLAY_MARGIN, -OVERLAY_MARGIN),
            egui::Align2::LEFT_BOTTOM,
            &text,
            bottom_left_max_width,
            corner_max_height,
        );
    }

    paint_overlay_text(
        ui.painter(),
        viewer_rect.right_bottom() + egui::vec2(-OVERLAY_MARGIN, -OVERLAY_MARGIN),
        egui::Align2::RIGHT_BOTTOM,
        &bottom_right_text,
        bottom_right_max_width,
        corner_max_height,
    );

    if let Some((left, right)) = metadata.and_then(side_orientation_markers) {
        paint_overlay_text(
            ui.painter(),
            viewer_rect.left_center() + egui::vec2(OVERLAY_MARGIN, 0.0),
            egui::Align2::LEFT_CENTER,
            left,
            available_overlay_width,
            OVERLAY_FONT_SIZE + OVERLAY_LINE_GAP,
        );
        paint_overlay_text(
            ui.painter(),
            viewer_rect.right_center() + egui::vec2(-OVERLAY_MARGIN, 0.0),
            egui::Align2::RIGHT_CENTER,
            right,
            available_overlay_width,
            OVERLAY_FONT_SIZE + OVERLAY_LINE_GAP,
        );
    }
}

fn top_left_overlay_text(metadata: Option<&DicomOverlayMetadata>) -> Option<String> {
    let metadata = metadata?;
    join_overlay_lines([
        metadata.patient_label.as_deref(),
        metadata.study_description.as_deref(),
        metadata.series_description.as_deref(),
    ])
}

fn top_right_overlay_text(metadata: Option<&DicomOverlayMetadata>) -> Option<String> {
    let metadata = metadata?;
    let study_datetime = format_study_datetime(
        metadata.study_date.as_deref(),
        metadata.study_time.as_deref(),
    );

    join_overlay_lines([metadata.manufacturer.as_deref(), study_datetime.as_deref()])
}

fn bottom_left_overlay_text(
    app: &DicronApp,
    metadata: Option<&DicomOverlayMetadata>,
) -> Option<String> {
    let mut lines = Vec::with_capacity(3);

    if let Some(metadata) = metadata {
        lines.push(format_measurement("ST", metadata.slice_thickness));
        lines.push(format_measurement("SL", metadata.slice_location));
    }

    if let (Some(slice_index), Some(slice_count)) =
        (app.current_slice_index(), app.current_slice_count())
        && let Some(slice_text) = slice_overlay_text(slice_index, slice_count)
    {
        lines.push(slice_text);
    }

    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn join_overlay_lines<const N: usize>(lines: [Option<&str>; N]) -> Option<String> {
    let text = lines
        .into_iter()
        .flatten()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    (!text.is_empty()).then_some(text)
}

fn format_study_datetime(date: Option<&str>, time: Option<&str>) -> Option<String> {
    let date = date.map(format_dicom_date);
    let time = time.map(format_dicom_time);
    join_overlay_lines([date.as_deref(), time.as_deref()]).map(|text| text.replace('\n', " "))
}

fn format_dicom_date(value: &str) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];

    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 8 || !bytes[..8].iter().all(u8::is_ascii_digit) {
        return value.to_owned();
    }

    let month = value[4..6].parse::<usize>().ok();
    let Some(month_name) = month.and_then(|month| MONTHS.get(month.saturating_sub(1))) else {
        return value.to_owned();
    };

    format!("{}-{month_name}-{}", &value[6..8], &value[..4])
}

fn format_dicom_time(value: &str) -> String {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 6 || !bytes[..6].iter().all(u8::is_ascii_digit) {
        return value.to_owned();
    }

    format!("{}:{}:{}", &value[..2], &value[2..4], &value[4..6])
}

fn format_measurement(label: &str, value: Option<f64>) -> String {
    value.map_or_else(
        || format!("{label}: -"),
        |value| format!("{label}: {value:.2} mm"),
    )
}

fn slice_overlay_text(slice_index: usize, slice_count: usize) -> Option<String> {
    (slice_count > 0 && slice_index < slice_count)
        .then(|| format!("Images: {}/{slice_count}", slice_index + 1))
}

fn window_overlay_text(center: f64, width: f64) -> String {
    format!("WL: {center:.0}  WW: {width:.0}")
}

fn pixel_probe_overlay_text(
    app: &DicronApp,
    pointer_position: egui::Pos2,
    image_rect: egui::Rect,
    image_size: [usize; 2],
    modality: Option<&str>,
) -> Option<String> {
    let (x, y) = image_pixel_coordinates(
        pointer_position,
        image_rect,
        image_size,
        app.viewport_transform,
    )?;
    let (path, frame_index) = app.current_frame_key.as_ref()?;
    let entry = app.decoded_cache.peek(path, *frame_index)?;
    let value = entry.frame.pixel_probe(x, y)?;

    Some(match value {
        PixelProbeValue::Monochrome(value) => {
            let label = if modality.is_some_and(|value| value.eq_ignore_ascii_case("CT")) {
                "HU"
            } else {
                "Value"
            };
            format!("X: {x}  Y: {y}  {label}: {}", format_probe_number(value))
        }
        PixelProbeValue::Rgb([red, green, blue]) => {
            format!("X: {x}  Y: {y}  RGB: ({red}, {green}, {blue})")
        }
    })
}

fn image_pixel_coordinates(
    pointer_position: egui::Pos2,
    image_rect: egui::Rect,
    image_size: [usize; 2],
    transform: ViewportTransform,
) -> Option<(usize, usize)> {
    if !image_rect.contains(pointer_position) || image_size[0] == 0 || image_size[1] == 0 {
        return None;
    }

    let relative = (pointer_position - image_rect.min) / image_rect.size();
    let (source_u, source_v) = transformed_image_uv(relative.x, relative.y, transform);
    let x = (source_u * image_size[0] as f32).floor() as usize;
    let y = (source_v * image_size[1] as f32).floor() as usize;

    Some((x.min(image_size[0] - 1), y.min(image_size[1] - 1)))
}

fn format_probe_number(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn side_orientation_markers(
    metadata: &DicomOverlayMetadata,
) -> Option<(&'static str, &'static str)> {
    let orientation = metadata.image_orientation?;
    let right = dominant_patient_direction([orientation[0], orientation[1], orientation[2]])?;
    let left = opposite_patient_direction(right);
    Some((left, right))
}

fn dominant_patient_direction(direction: [f64; 3]) -> Option<&'static str> {
    let (axis, value) = direction
        .into_iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))?;

    if !value.is_finite() || value.abs() < f64::EPSILON {
        return None;
    }

    Some(match (axis, value.is_sign_positive()) {
        (0, true) => "L",
        (0, false) => "R",
        (1, true) => "P",
        (1, false) => "A",
        (2, true) => "H",
        (2, false) => "F",
        _ => unreachable!(),
    })
}

fn opposite_patient_direction(direction: &str) -> &'static str {
    match direction {
        "L" => "R",
        "R" => "L",
        "P" => "A",
        "A" => "P",
        "H" => "F",
        "F" => "H",
        _ => "",
    }
}

fn paint_overlay_text(
    painter: &egui::Painter,
    position: egui::Pos2,
    anchor: egui::Align2,
    text: &str,
    max_width: f32,
    max_height: f32,
) {
    let font = egui::FontId::monospace(OVERLAY_FONT_SIZE);
    let line_height = painter
        .layout_no_wrap("Ag".to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
        .size()
        .y;
    let line_step = line_height + OVERLAY_LINE_GAP;
    let lines: Vec<_> = text.lines().filter(|line| !line.is_empty()).collect();
    let max_lines = if max_height.is_finite() {
        ((max_height + OVERLAY_LINE_GAP) / line_step).floor() as usize
    } else {
        lines.len()
    };

    if max_lines == 0 || lines.is_empty() || max_width <= 0.0 {
        return;
    }

    let visible_line_count = lines.len().min(max_lines);
    let first_visible_line = if anchor.y() == egui::Align::Max {
        lines.len() - visible_line_count
    } else {
        0
    };
    let visible_lines = &lines[first_visible_line..first_visible_line + visible_line_count];
    let block_height = line_height * visible_line_count as f32
        + OVERLAY_LINE_GAP * visible_line_count.saturating_sub(1) as f32;
    let block_top = match anchor.y() {
        egui::Align::Min => position.y,
        egui::Align::Center => position.y - block_height / 2.0,
        egui::Align::Max => position.y - block_height,
    };
    let line_anchor = egui::Align2([anchor.x(), egui::Align::Min]);

    for (line_index, line) in visible_lines.iter().enumerate() {
        let Some(galley) = layout_overlay_line(painter, line, &font, max_width) else {
            continue;
        };
        let line_position = egui::pos2(position.x, block_top + line_index as f32 * line_step);
        let line_rect = line_anchor.anchor_size(line_position, galley.size());

        paint_overlay_galley(painter, line_rect.min, galley);
    }
}

fn paired_overlay_widths(
    painter: &egui::Painter,
    left_text: Option<&str>,
    right_text: Option<&str>,
    available_width: f32,
    constrained_left_share: f32,
) -> (f32, f32) {
    match (left_text, right_text) {
        (None, None) => (0.0, 0.0),
        (Some(_), None) => (available_width, 0.0),
        (None, Some(_)) => (0.0, available_width),
        (Some(left_text), Some(right_text)) => {
            let font = egui::FontId::monospace(OVERLAY_FONT_SIZE);
            let left_desired_width = overlay_text_width(painter, left_text, &font);
            let right_desired_width = overlay_text_width(painter, right_text, &font);

            if left_desired_width + right_desired_width <= available_width {
                return (left_desired_width, right_desired_width);
            }

            let left_width = available_width * constrained_left_share.clamp(0.0, 1.0);
            (left_width, available_width - left_width)
        }
    }
}

fn overlay_text_width(painter: &egui::Painter, text: &str, font: &egui::FontId) -> f32 {
    text.lines()
        .map(|line| {
            painter
                .layout_no_wrap(line.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
                .size()
                .x
        })
        .fold(0.0, f32::max)
}

fn layout_overlay_line(
    painter: &egui::Painter,
    text: &str,
    font: &egui::FontId,
    max_width: f32,
) -> Option<Arc<egui::Galley>> {
    let full_galley =
        painter.layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::PLACEHOLDER);
    if full_galley.size().x <= max_width {
        return Some(full_galley);
    }

    const TRUNCATION_SUFFIX: &str = "...";
    let suffix_galley = painter.layout_no_wrap(
        TRUNCATION_SUFFIX.to_owned(),
        font.clone(),
        egui::Color32::PLACEHOLDER,
    );
    if suffix_galley.size().x > max_width {
        return None;
    }

    let characters: Vec<_> = text.chars().collect();
    let mut minimum = 0;
    let mut maximum = characters.len();

    while minimum < maximum {
        let candidate_length = (minimum + maximum).div_ceil(2);
        let candidate = truncated_overlay_line(&characters, candidate_length);
        let candidate_width = painter
            .layout_no_wrap(candidate, font.clone(), egui::Color32::PLACEHOLDER)
            .size()
            .x;

        if candidate_width <= max_width {
            minimum = candidate_length;
        } else {
            maximum = candidate_length - 1;
        }
    }

    Some(painter.layout_no_wrap(
        truncated_overlay_line(&characters, minimum),
        font.clone(),
        egui::Color32::PLACEHOLDER,
    ))
}

fn truncated_overlay_line(characters: &[char], prefix_length: usize) -> String {
    characters
        .iter()
        .take(prefix_length)
        .chain(['.', '.', '.'].iter())
        .collect()
}

fn paint_overlay_galley(painter: &egui::Painter, position: egui::Pos2, galley: Arc<egui::Galley>) {
    let paint_passes = overlay_galley_paint_passes(painter);

    for _ in 0..paint_passes {
        painter.galley_with_override_text_color(
            position + egui::vec2(1.0, 1.0),
            galley.clone(),
            egui::Color32::from_black_alpha(180),
        );
    }

    for _ in 0..paint_passes {
        painter.galley_with_override_text_color(
            position,
            galley.clone(),
            egui::Color32::from_white_alpha(220),
        );
    }
}

fn overlay_galley_paint_passes(painter: &egui::Painter) -> usize {
    let alpha_from_coverage = painter.fonts(|fonts| fonts.options().alpha_from_coverage);

    // The font atlas is configured from the active global theme, even though the viewer uses a
    // local dark style. Dark coverage is `2c - c²`, equivalent to compositing an opaque linear
    // coverage mask twice. A second coincident overlay pass restores that stronger edge coverage.
    // Keep the correction local so ordinary light-theme UI text is unaffected, and leave the
    // existing dark overlay path as a single pass.
    if alpha_from_coverage == egui::epaint::AlphaFromCoverage::LIGHT_MODE_DEFAULT {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui;

    use super::{
        dominant_patient_direction, format_dicom_date, format_dicom_time, format_probe_number,
        image_pixel_coordinates, overlay_galley_paint_passes, slice_overlay_text,
        transformed_image_rect, window_overlay_text, zoom_viewport_around_pointer,
    };
    use crate::app::state::ViewportTransform;

    fn assert_approximately_equal(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.001,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn slice_overlay_uses_human_readable_numbering() {
        assert_eq!(slice_overlay_text(0, 20).as_deref(), Some("Images: 1/20"));
        assert_eq!(slice_overlay_text(19, 20).as_deref(), Some("Images: 20/20"));
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

    #[test]
    fn dicom_date_and_time_use_readable_overlay_formats() {
        assert_eq!(format_dicom_date("20241116"), "16-November-2024");
        assert_eq!(format_dicom_time("195257.123456"), "19:52:57");
        assert_eq!(format_dicom_date("unknown"), "unknown");
    }

    #[test]
    fn pointer_position_maps_to_fitted_image_pixels() {
        let image_rect = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(210.0, 120.0));

        assert_eq!(
            image_pixel_coordinates(
                egui::pos2(110.0, 70.0),
                image_rect,
                [400, 200],
                ViewportTransform::default(),
            ),
            Some((200, 100))
        );
        assert_eq!(
            image_pixel_coordinates(
                egui::pos2(9.0, 70.0),
                image_rect,
                [400, 200],
                ViewportTransform::default(),
            ),
            None
        );
    }

    #[test]
    fn viewport_transform_scales_from_fit_and_applies_pan() {
        let viewer_rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let transform = ViewportTransform {
            zoom: 2.0,
            pan: egui::vec2(20.0, -5.0),
            ..Default::default()
        };

        let image_rect = transformed_image_rect(viewer_rect, egui::vec2(100.0, 50.0), transform);

        assert_eq!(image_rect.center(), egui::pos2(120.0, 45.0));
        assert_eq!(image_rect.size(), egui::vec2(200.0, 100.0));
    }

    #[test]
    fn right_drag_down_zoom_keeps_the_drag_start_point_anchored() {
        let viewer_rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let fitted_size = egui::vec2(100.0, 50.0);
        let anchor = egui::pos2(125.0, 50.0);
        let mut transform = ViewportTransform::default();
        let before = transformed_image_rect(viewer_rect, fitted_size, transform);
        let relative_anchor = (anchor - before.min) / before.size();
        zoom_viewport_around_pointer(
            &mut transform,
            std::f32::consts::LN_2 / super::VIEWER_ZOOM_DRAG_SENSITIVITY,
            viewer_rect.center(),
            anchor,
        );
        let after = transformed_image_rect(viewer_rect, fitted_size, transform);
        let transformed_anchor = after.min + relative_anchor * after.size();

        assert_approximately_equal(transform.zoom, 2.0);
        assert_approximately_equal(transformed_anchor.x, anchor.x);
        assert_approximately_equal(transformed_anchor.y, anchor.y);
    }

    #[test]
    fn pixel_probe_numbers_keep_useful_fractional_precision() {
        assert_eq!(format_probe_number(43.01), "43");
        assert_eq!(format_probe_number(43.25), "43.2");
    }

    #[test]
    fn orientation_uses_the_dominant_patient_axis() {
        assert_eq!(dominant_patient_direction([1.0, 0.0, 0.0]), Some("L"));
        assert_eq!(dominant_patient_direction([0.0, -0.9, 0.1]), Some("A"));
        assert_eq!(dominant_patient_direction([0.0, 0.0, 0.0]), None);
    }

    #[test]
    fn overlay_compensates_for_light_font_coverage_only() {
        let context = egui::Context::default();

        for (theme, expected_passes) in [(egui::Theme::Dark, 1), (egui::Theme::Light, 2)] {
            context.set_theme(theme);

            let _ = context.run_ui(Default::default(), |ui| {
                assert_eq!(overlay_galley_paint_passes(ui.painter()), expected_passes);
            });
        }
    }
}
