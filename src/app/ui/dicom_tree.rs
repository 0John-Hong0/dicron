//! Patient/Study/Series/Instance selection tree.

use std::hash::Hash;

use eframe::egui;

use crate::app::DicronApp;
use crate::app::state::{SeriesKey, SliceSelection};
use crate::dicom::{PatientGroup, SliceItem, StudyGroup};
use crate::theme;

// Large subtrees start collapsed, and every series lays out only the slice rows that intersect
// the tree's scroll viewport, so malformed or unusually large studies do not make every frame
// expensive.
const SERIES_AUTO_COLLAPSE_SLICE_COUNT: usize = 200;
const TREE_AUTO_COLLAPSE_SLICE_COUNT: usize = 1000;
const TREE_ROW_GAP: f32 = theme::SPACE_XXS;
const PATIENT_ROW_HEIGHT: f32 = 26.0;
const STUDY_ROW_HEIGHT: f32 = 24.0;
const SERIES_ROW_HEIGHT: f32 = 24.0;
const INSTANCE_ROW_HEIGHT: f32 = 20.0;

#[derive(Clone, Copy)]
enum TreeNodeLevel {
    Patient,
    Study,
    Series,
}

impl TreeNodeLevel {
    const fn row_height(self) -> f32 {
        match self {
            Self::Patient => PATIENT_ROW_HEIGHT,
            Self::Study => STUDY_ROW_HEIGHT,
            Self::Series => SERIES_ROW_HEIGHT,
        }
    }
}

impl DicronApp {
    pub(in crate::app) fn show_dicom_tree(&mut self, ui: &mut egui::Ui) {
        let expand_all = self.settings.expand_tree_by_default;
        let tree_generation = self.tree_view_generation;

        let Some(dicom_index) = &self.dicom_index else {
            if self.scan.is_active() {
                ui.label("Scanning folder...");
            } else {
                ui.label("Open a DICOM file or folder to build Patient / Study / Series tree.");
            }

            return;
        };

        ui.label(format!("{} DICOM files", dicom_index.total_file_count));
        ui.separator();

        let selected_indices = self.selected_indices();
        let mut clicked_selection = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = TREE_ROW_GAP;

                ui.push_id(tree_generation, |ui| {
                    for (patient_index, patient) in dicom_index.patients.iter().enumerate() {
                        let patient_slice_count = patient_total_slice_count(patient);

                        show_tree_node(
                            ui,
                            ("patient", patient_index),
                            patient.display_name.as_str(),
                            expand_all || patient_slice_count < TREE_AUTO_COLLAPSE_SLICE_COUNT,
                            TreeNodeLevel::Patient,
                            |ui| {
                                for (study_index, study) in patient.studies.iter().enumerate() {
                                    let study_slice_count = study_total_slice_count(study);

                                    show_tree_node(
                                        ui,
                                        ("study", patient_index, study_index),
                                        study.display_name.as_str(),
                                        expand_all
                                            || study_slice_count < TREE_AUTO_COLLAPSE_SLICE_COUNT,
                                        TreeNodeLevel::Study,
                                        |ui| {
                                            for (series_index, series) in
                                                study.series_groups.iter().enumerate()
                                            {
                                                let series_label = format!(
                                                    "{} ({} slices)",
                                                    series.display_name,
                                                    series.slices.len()
                                                );

                                                show_tree_node(
                                                    ui,
                                                    (
                                                        "series",
                                                        patient_index,
                                                        study_index,
                                                        series_index,
                                                    ),
                                                    &series_label,
                                                    expand_all
                                                        || series.slices.len()
                                                            < SERIES_AUTO_COLLAPSE_SLICE_COUNT,
                                                    TreeNodeLevel::Series,
                                                    |ui| {
                                                        show_series_slices(
                                                            ui,
                                                            &series.slices,
                                                            (
                                                                patient_index,
                                                                study_index,
                                                                series_index,
                                                            ),
                                                            selected_indices,
                                                            &mut clicked_selection,
                                                        );
                                                    },
                                                );
                                            }
                                        },
                                    );
                                }
                            },
                        );
                    }
                });
            });

        if let Some(selection) = clicked_selection {
            self.load_slice_by_indices(
                ui.ctx(),
                selection.patient_index,
                selection.study_index,
                selection.series_index,
                selection.slice_index,
            );
        }
    }
}

fn show_tree_node<R>(
    ui: &mut egui::Ui,
    id_salt: impl Hash,
    label: &str,
    default_open: bool,
    level: TreeNodeLevel,
    add_body: impl FnOnce(&mut egui::Ui) -> R,
) {
    let id = ui.make_persistent_id(id_salt);
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        default_open,
    );
    let openness = state.openness(ui.ctx());
    let header_response = show_tree_node_row(ui, label, level, openness);

    if header_response.clicked() {
        state.toggle(ui);
    }

    state.show_body_indented(&header_response, ui, add_body);
}

fn show_tree_node_row(
    ui: &mut egui::Ui,
    label: &str,
    level: TreeNodeLevel,
    openness: f32,
) -> egui::Response {
    let row_size = egui::vec2(ui.available_width(), level.row_height());
    let (row_rect, mut response) = ui.allocate_exact_size(row_size, egui::Sense::click());

    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::CollapsingHeader, ui.is_enabled(), label)
    });

    if ui.is_rect_visible(row_rect) {
        let icon_center = egui::pos2(
            row_rect.left() + ui.spacing().indent / 2.0,
            row_rect.center().y,
        );
        let icon_rect = egui::Rect::from_center_size(
            icon_center,
            egui::Vec2::splat(ui.spacing().icon_width_inner),
        );
        let icon_response = response.clone().with_new_rect(icon_rect);
        egui::collapsing_header::paint_default_icon(ui, openness, &icon_response);

        let text_color = tree_node_text_color(ui.visuals(), level);
        let rich_text = match level {
            TreeNodeLevel::Patient => egui::RichText::new(label).strong().size(14.0),
            TreeNodeLevel::Study => egui::RichText::new(label).color(text_color),
            TreeNodeLevel::Series => egui::RichText::new(label).color(text_color),
        };
        let text_left = row_rect.left() + ui.spacing().indent;
        let text_was_elided = paint_truncated_text(
            ui,
            row_rect,
            text_left,
            rich_text,
            egui::TextStyle::Button,
            text_color,
        );

        if text_was_elided {
            response = response.on_hover_text(label);
        }
    }

    response
}

fn tree_node_text_color(visuals: &egui::Visuals, level: TreeNodeLevel) -> egui::Color32 {
    match level {
        TreeNodeLevel::Patient => visuals.strong_text_color(),
        TreeNodeLevel::Study => visuals.text_color(),
        TreeNodeLevel::Series => visuals.text_color().gamma_multiply(0.90),
    }
}

fn paint_truncated_text(
    ui: &egui::Ui,
    row_rect: egui::Rect,
    text_left: f32,
    text: egui::RichText,
    text_style: egui::TextStyle,
    fallback_color: egui::Color32,
) -> bool {
    let text_width = (row_rect.right() - text_left - theme::SPACE_XS).max(0.0);
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        text_width,
        text_style,
    );
    let text_position = egui::pos2(text_left, row_rect.center().y - galley.size().y / 2.0);
    let text_was_elided = galley.elided;

    ui.painter().galley(text_position, galley, fallback_color);

    text_was_elided
}

fn show_series_slices(
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

fn patient_total_slice_count(patient: &PatientGroup) -> usize {
    patient.studies.iter().map(study_total_slice_count).sum()
}

fn study_total_slice_count(study: &StudyGroup) -> usize {
    study
        .series_groups
        .iter()
        .map(|series| series.slices.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn test_slices(count: usize) -> Vec<SliceItem> {
        (0..count)
            .map(|index| SliceItem {
                path: std::path::PathBuf::from(format!("slice_{index}.dcm")),
                display_name: format!("#{index}"),
                frame_index: 0,
                instance_number: None,
                sort_position: None,
            })
            .collect()
    }

    /// Counts the 2px red outlines egui's debug check `warn_if_rect_changes_id` paints when a
    /// rect is claimed by a different widget id than in the previous pass.
    fn id_change_warning_count(shapes: &[egui::epaint::ClippedShape]) -> usize {
        shapes
            .iter()
            .filter(|clipped| clipped.clip_rect == egui::Rect::EVERYTHING)
            .filter(|clipped| {
                matches!(
                    &clipped.shape,
                    egui::Shape::Rect(rect_shape)
                        if rect_shape.stroke.color == egui::Color32::RED
                            && rect_shape.stroke.width == 2.0
                )
            })
            .count()
    }

    fn run_frame(
        context: &egui::Context,
        add_contents: impl FnOnce(&mut egui::Ui),
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(300.0, 400.0),
            )),
            ..Default::default()
        };
        let mut add_contents = Some(add_contents);

        context.run_ui(input, |ui| {
            if let Some(add_contents) = add_contents.take() {
                add_contents(ui);
            }
        })
    }

    #[cfg(debug_assertions)]
    #[test]
    fn id_change_warning_detector_sees_egui_warning() {
        // Control: an id that changes at an unchanged rect must be flagged by egui.
        let context = egui::Context::default();
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(100.0, 20.0));

        run_frame(&context, |ui| {
            ui.interact(rect, egui::Id::new("first"), egui::Sense::click());
        });
        let output = run_frame(&context, |ui| {
            ui.interact(rect, egui::Id::new("second"), egui::Sense::click());
        });

        assert_eq!(id_change_warning_count(&output.shapes), 1);
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
