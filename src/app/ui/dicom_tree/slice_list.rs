//! Virtualized filename rows for the DICOM tree's file-list mode.

use eframe::egui;

use super::hierarchy::paint_truncated_text;
use crate::app::state::{SeriesKey, SliceSelection};
use crate::dicom::SliceItem;
use crate::theme;

const INSTANCE_ROW_HEIGHT: f32 = 20.0;

pub(super) fn show_series_slices(
    ui: &mut egui::Ui,
    slices: &[SliceItem],
    (patient_index, study_index, series_index): SeriesKey,
    selected_selection: Option<SliceSelection>,
    clicked_selection: &mut Option<SliceSelection>,
) {
    if slices.is_empty() {
        return;
    }

    // Reserve the whole list's height inside the tree's scroll area, then lay out only the rows
    // that intersect its viewport. This keeps long series cheap without nesting a second scroll
    // area, and therefore a second scrollbar, inside the tree.
    let row_gap = ui.spacing().item_spacing.y;
    let row_stride = INSTANCE_ROW_HEIGHT + row_gap;
    let list_height = row_stride * slices.len() as f32 - row_gap;
    let (_, list_rect) = ui.allocate_space(egui::vec2(ui.available_width(), list_height));
    let window = row_window(
        list_rect.top(),
        row_stride,
        slices.len(),
        ui.clip_rect().y_range(),
    );

    for slice_index in window.rows.clone() {
        let row_rect = egui::Rect::from_min_size(
            egui::pos2(
                list_rect.left(),
                list_rect.top() + slice_index as f32 * row_stride,
            ),
            egui::vec2(list_rect.width(), INSTANCE_ROW_HEIGHT),
        );
        let current_selection =
            SliceSelection::new(patient_index, study_index, series_index, slice_index);

        show_slice_row(
            ui,
            window.slot(slice_index),
            row_rect,
            &slices[slice_index],
            current_selection,
            selected_selection,
            clicked_selection,
        );
    }
}

/// The rows of a fixed-stride list that overlap the visible part of the tree, plus the anchor
/// that maps them to screen-position slots.
struct RowWindow {
    /// Rows overlapping the visible range, clamped to the list.
    rows: std::ops::Range<usize>,
    /// Index of the row containing the top edge of the visible range. It is negative when the
    /// list starts below that edge. Because it is not clamped, `row - anchor_row` names a screen
    /// position: two frames whose list positions differ by whole rows give the same slot to the
    /// same rect, whether or not the list start is in view.
    anchor_row: i64,
}

impl RowWindow {
    /// Screen-position slot of `row`; only used as an id salt, so wrapping on absurd anchors
    /// (from non-finite visible ranges) is fine.
    fn slot(&self, row: usize) -> i64 {
        (row as i64).wrapping_sub(self.anchor_row)
    }
}

/// Rows of a fixed-stride list starting at `list_top` that overlap the vertical range
/// `visible_y`, with `rows` clamped to `0..row_count`.
fn row_window(
    list_top: f32,
    row_stride: f32,
    row_count: usize,
    visible_y: egui::Rangef,
) -> RowWindow {
    if row_count == 0 || row_stride <= 0.0 || visible_y.max <= visible_y.min {
        return RowWindow {
            rows: 0..0,
            anchor_row: 0,
        };
    }

    // `as` casts saturate, so oversized or non-finite offsets clamp instead of wrapping.
    let anchor_row = ((visible_y.min - list_top) / row_stride).floor() as i64;
    let first_row = anchor_row.max(0) as usize;
    let end_row = ((visible_y.max - list_top) / row_stride).ceil().max(0.0) as usize;

    RowWindow {
        rows: first_row.min(row_count)..end_row.min(row_count),
        anchor_row,
    }
}

/// Shows one slice row. `slot` is the row's screen-position slot, see [`RowWindow::slot`].
fn show_slice_row(
    ui: &egui::Ui,
    slot: i64,
    row_rect: egui::Rect,
    slice: &SliceItem,
    current_selection: SliceSelection,
    selected_selection: Option<SliceSelection>,
    clicked_selection: &mut Option<SliceSelection>,
) {
    let is_selected = selected_selection == Some(current_selection);
    // Row widgets are identified by their screen-position slot rather than by slice index, so the
    // widget at a given position keeps its id while the list scrolls underneath it. Ids keyed by
    // slice would change identity at unchanged rects whenever the tree scrolls by whole rows,
    // which egui's id-stability debug check (`warn_if_rect_changes_id`) flags with red outlines.
    // The slice shown in a slot is data, re-read from `slices` every frame.
    let row_id = ui.id().with(("slice_row", slot));
    let mut response = ui.interact(row_rect, row_id, egui::Sense::click());

    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            is_selected,
            &slice.display_name,
        )
    });

    if ui.is_rect_visible(row_rect) {
        let visuals = ui.visuals();
        let row_fill = if is_selected {
            visuals
                .selection
                .bg_fill
                .lerp_to_gamma(visuals.panel_fill, 0.35)
        } else if response.hovered() {
            visuals.widgets.hovered.weak_bg_fill
        } else {
            egui::Color32::TRANSPARENT
        };

        if row_fill != egui::Color32::TRANSPARENT {
            ui.painter()
                .rect_filled(row_rect, theme::SPACE_XXS, row_fill);
        }

        let text_color = if is_selected || response.hovered() {
            visuals.text_color()
        } else {
            visuals.weak_text_color()
        };
        let text_left = row_rect.left() + theme::SPACE_XS;
        let text_was_elided = paint_truncated_text(
            ui,
            row_rect,
            text_left,
            egui::RichText::new(&slice.display_name).color(text_color),
            egui::TextStyle::Body,
            text_color,
        );

        if text_was_elided {
            response = response.on_hover_text(&slice.display_name);
        }
    }

    if response.clicked() {
        *clicked_selection = Some(current_selection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ui::dicom_tree::hierarchy::TREE_ROW_GAP;
    use crate::app::ui::dicom_tree::test_support::{
        id_change_warning_count, run_frame, test_slices,
    };

    const STRIDE: f32 = INSTANCE_ROW_HEIGHT + TREE_ROW_GAP;

    fn range(min: f32, max: f32) -> egui::Rangef {
        egui::Rangef::new(min, max)
    }

    #[test]
    fn visible_rows_cover_only_the_viewport() {
        // The viewport shows rows 10 through 14 of a long list that starts at y = 100.
        let visible = range(100.0 + 10.0 * STRIDE + 1.0, 100.0 + 15.0 * STRIDE - 1.0);

        assert_eq!(row_window(100.0, STRIDE, 2000, visible).rows, 10..15);
    }

    #[test]
    fn visible_rows_clamp_to_the_list() {
        // Entirely above the list.
        assert_eq!(row_window(500.0, STRIDE, 20, range(0.0, 400.0)).rows, 0..0);
        // Straddling the start of the list.
        assert_eq!(
            row_window(500.0, STRIDE, 20, range(400.0, 500.0 + 2.5 * STRIDE)).rows,
            0..3
        );
        // Straddling the end of the list.
        assert_eq!(
            row_window(0.0, STRIDE, 20, range(18.5 * STRIDE, 40.0 * STRIDE)).rows,
            18..20
        );
        // Entirely below the list.
        assert_eq!(
            row_window(0.0, STRIDE, 20, range(30.0 * STRIDE, 40.0 * STRIDE)).rows,
            20..20
        );
    }

    #[test]
    fn slots_follow_screen_position_across_whole_row_scrolls() {
        // The row whose top sits two rows below the visible top edge keeps slot 2 as the list
        // moves by whole rows, including when the list start is below that edge (negative anchor).
        let visible = range(100.0, 500.0);

        for shift in -2_i64..=5 {
            let list_top = 100.0 - shift as f32 * STRIDE;
            let window = row_window(list_top, STRIDE, 50, visible);
            let row = usize::try_from(shift + 2).unwrap();

            assert_eq!(window.anchor_row, shift, "shift {shift}");
            assert_eq!(window.rows.start, usize::try_from(shift.max(0)).unwrap());
            assert_eq!(window.slot(row), 2, "shift {shift}");
        }
    }

    #[test]
    fn visible_rows_handle_degenerate_inputs() {
        assert_eq!(row_window(0.0, STRIDE, 0, range(0.0, 100.0)).rows, 0..0);
        assert_eq!(row_window(0.0, STRIDE, 10, range(50.0, 50.0)).rows, 0..0);
        assert_eq!(row_window(0.0, 0.0, 10, range(0.0, 100.0)).rows, 0..0);
        let unbounded = row_window(0.0, STRIDE, 10, range(f32::NEG_INFINITY, f32::INFINITY));
        assert_eq!(unbounded.rows, 0..10);
        // The saturated anchor must not make slot computation panic.
        let _ = unbounded.slot(0);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn scrolling_by_whole_rows_keeps_row_ids_stable() {
        let slices = test_slices(500);
        let context = egui::Context::default();
        let stride = INSTANCE_ROW_HEIGHT + TREE_ROW_GAP;
        let show_scrolled_list = |scroll_offset: f32| {
            run_frame(&context, |ui| {
                ui.spacing_mut().item_spacing.y = TREE_ROW_GAP;
                egui::ScrollArea::vertical()
                    .vertical_scroll_offset(scroll_offset)
                    .show(ui, |ui| {
                        let mut clicked_selection = None;
                        show_series_slices(ui, &slices, (0, 0, 0), None, &mut clicked_selection);
                    });
            })
        };

        show_scrolled_list(0.0);

        // Whole-row jumps of various sizes, a fractional offset, and scrolling back up.
        for scroll_offset in [
            5.0 * stride,
            18.0 * stride,
            18.0 * stride + 7.0,
            40.0 * stride,
            3.0 * stride,
            0.0,
        ] {
            let output = show_scrolled_list(scroll_offset);

            assert_eq!(
                id_change_warning_count(&output.shapes),
                0,
                "scroll offset {scroll_offset}"
            );
        }
    }
}
