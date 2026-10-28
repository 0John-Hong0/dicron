//! Shared image painting and overlay text helpers for the viewer surfaces.

use std::sync::Arc;

use eframe::egui;

use crate::app::state::ViewportTransform;
use crate::dicom::DicomOverlayMetadata;
use crate::theme;

pub(super) const VIEWER_ZOOM_DRAG_SENSITIVITY: f32 = 0.01;
const MIN_VIEWER_ZOOM: f32 = 0.1;
const MAX_VIEWER_ZOOM: f32 = 20.0;
pub(super) const OVERLAY_FONT_SIZE: f32 = 13.0;
pub(super) const OVERLAY_LINE_GAP: f32 = theme::SPACE_XXS;
/// Space between overlay text and the edge of the view it labels.
pub(super) const OVERLAY_MARGIN: f32 = theme::SPACE_MD;

/// Zoom by a vertical drag, keeping the image point under `anchor` still.
/// `viewer_center` is where the fitted image sits before any pan.
pub(super) fn zoom_viewport_around_pointer(
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

/// Where a fitted image lands once its zoom, pan and quarter rotations apply.
pub(super) fn transformed_image_rect(
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

/// Paint a texture into `image_rect`, mapping the corners through `transform`
/// so flips and quarter rotations cost nothing but different texture
/// coordinates.
pub(super) fn paint_transformed_image(
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

/// The patient, study and series an image belongs to, one per line, as the
/// viewers show it in their top-left corner.
pub(super) fn identification_overlay_text(
    metadata: Option<&DicomOverlayMetadata>,
) -> Option<String> {
    let metadata = metadata?;
    join_overlay_lines([
        metadata.patient_label.as_deref(),
        metadata.study_description.as_deref(),
        metadata.series_description.as_deref(),
    ])
}

/// The non-empty lines, one per row, or `None` when there are none.
pub(super) fn join_overlay_lines<const N: usize>(lines: [Option<&str>; N]) -> Option<String> {
    let text = lines
        .into_iter()
        .flatten()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    (!text.is_empty()).then_some(text)
}

/// The source texture coordinate a displayed corner reads from.
pub(super) fn transformed_image_uv(
    mut u: f32,
    mut v: f32,
    transform: ViewportTransform,
) -> (f32, f32) {
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

/// Paint monospace overlay text anchored at `position`, truncating each line to
/// `max_width` and dropping lines that do not fit inside `max_height`.
pub(super) fn paint_overlay_text(
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

/// Lay out one overlay line, truncating it with an ellipsis when it is wider
/// than `max_width`.
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

/// Paint an overlay galley in white over a dark shadow, so it stays readable on
/// any image content.
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

    use super::{overlay_galley_paint_passes, transformed_image_uv};
    use crate::app::state::ViewportTransform;

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

    #[test]
    fn a_vertical_flip_swaps_the_top_and_bottom_texture_rows() {
        let flipped = ViewportTransform {
            flip_vertical: true,
            ..Default::default()
        };

        assert_eq!(transformed_image_uv(0.0, 0.0, flipped), (0.0, 1.0));
        assert_eq!(transformed_image_uv(1.0, 1.0, flipped), (1.0, 0.0));
        assert_eq!(
            transformed_image_uv(0.25, 0.75, ViewportTransform::default()),
            (0.25, 0.75)
        );
    }
}
