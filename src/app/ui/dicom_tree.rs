//! Patient/Study/Series/Instance selection tree.

use std::hash::Hash;

use eframe::egui;

use crate::app::DicronApp;
use crate::app::series_thumbnail_cache::SeriesThumbnailCache;
use crate::app::state::{SeriesKey, SliceSelection};
use crate::dicom::{PatientGroup, SliceItem, StudyGroup};
use crate::settings::DicomTreeViewMode;
use crate::theme;

// Large subtrees start collapsed, and every series lays out only the slice rows that intersect
// the tree's scroll viewport, so malformed or unusually large studies do not make every frame
// expensive.
const SERIES_AUTO_COLLAPSE_SLICE_COUNT: usize = 200;
const TREE_AUTO_COLLAPSE_SLICE_COUNT: usize = 1000;
// The preview view draws one card per series regardless of how many slices those series hold, so
// its auto-collapse threshold counts cards rather than slices.
const TREE_AUTO_COLLAPSE_CARD_COUNT: usize = 100;
const TREE_ROW_GAP: f32 = theme::SPACE_XXS;
const PATIENT_ROW_HEIGHT: f32 = 26.0;
const STUDY_ROW_HEIGHT: f32 = 24.0;
const SERIES_ROW_HEIGHT: f32 = 24.0;
const INSTANCE_ROW_HEIGHT: f32 = 20.0;
const SERIES_PREVIEW_HEIGHT: f32 = 88.0;
const SERIES_PREVIEW_INSET: f32 = theme::SPACE_SM;
// Tallest possible stack of sticky headers: one header per level plus the gaps between them.
const STICKY_STACK_MAX_HEIGHT: f32 =
    PATIENT_ROW_HEIGHT + STUDY_ROW_HEIGHT + SERIES_ROW_HEIGHT + 3.0 * TREE_ROW_GAP;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum TreeNodeLevel {
    Patient,
    Study,
    Series,
}

impl TreeNodeLevel {
    /// Outermost to innermost.
    const ALL: [Self; 3] = [Self::Patient, Self::Study, Self::Series];

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
        self.series_thumbnails.poll(ui.ctx());

        let expand_all = self.settings.expand_tree_by_default;
        let view_mode = self.settings.dicom_tree_view_mode;
        let tree_generation = self.tree_view_generation;
        let selected_indices = self.selected_indices();
        let last_slice_by_series = &self.last_slice_by_series;
        let series_thumbnails = &mut self.series_thumbnails;

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

        let mut clicked_selection = None;

        let mut sticky_candidates = Vec::new();
        let scroll_output = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = TREE_ROW_GAP;

                ui.push_id(tree_generation, |ui| {
                    for (patient_index, patient) in dicom_index.patients.iter().enumerate() {
                        let patient_slice_count = patient_total_slice_count(patient);
                        let patient_series_count = patient_total_series_count(patient);

                        show_tree_node(
                            ui,
                            ("patient", patient_index),
                            patient.display_name.as_str(),
                            expand_all
                                || tree_node_default_open(
                                    view_mode,
                                    patient_slice_count,
                                    patient_series_count,
                                ),
                            TreeNodeLevel::Patient,
                            &mut sticky_candidates,
                            |ui, sticky_candidates| {
                                for (study_index, study) in patient.studies.iter().enumerate() {
                                    let study_slice_count = study_total_slice_count(study);

                                    show_tree_node(
                                        ui,
                                        ("study", patient_index, study_index),
                                        study.display_name.as_str(),
                                        expand_all
                                            || tree_node_default_open(
                                                view_mode,
                                                study_slice_count,
                                                study.series_groups.len(),
                                            ),
                                        TreeNodeLevel::Study,
                                        sticky_candidates,
                                        |ui, sticky_candidates| {
                                            for (series_index, series) in
                                                study.series_groups.iter().enumerate()
                                            {
                                                let series_key = (
                                                    patient_index,
                                                    study_index,
                                                    series_index,
                                                );

                                                match view_mode {
                                                    DicomTreeViewMode::SeriesPreviews => {
                                                        show_series_preview(
                                                            ui,
                                                            series.series_description.as_deref(),
                                                            series.series_number,
                                                            series.modality.as_deref(),
                                                            &series.slices,
                                                            series_key,
                                                            selected_indices,
                                                            last_slice_by_series
                                                                .get(&series_key)
                                                                .copied(),
                                                            series_thumbnails,
                                                            &mut clicked_selection,
                                                        );
                                                    }
                                                    DicomTreeViewMode::FileList => {
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
                                                            sticky_candidates,
                                                            |ui, _| {
                                                                show_series_slices(
                                                                    ui,
                                                                    &series.slices,
                                                                    series_key,
                                                                    selected_indices,
                                                                    &mut clicked_selection,
                                                                );
                                                            },
                                                        );
                                                    }
                                                }
                                            }
                                        },
                                    );
                                }
                            },
                        );
                    }
                });

                // Paint the sticky headers inside the scroll area, after every row and before egui
                // paints the scrollbar, so they cover the rows but never the bar. The content clip
                // rect is the viewport grown by `clip_rect_margin` in the scroll direction.
                let clip_margin = ui.visuals().clip_rect_margin;
                let clip_rect = ui.clip_rect();
                let viewport = egui::Rect::from_x_y_ranges(
                    ui.max_rect().x_range(),
                    clip_rect.top() + clip_margin..=clip_rect.bottom() - clip_margin,
                );

                show_sticky_headers(ui, viewport, &sticky_candidates)
            });

        let egui::scroll_area::ScrollAreaOutput {
            inner: sticky_collapse,
            id: scroll_area_id,
            state: scroll_state,
            ..
        } = scroll_output;

        if let Some(collapse) = sticky_collapse {
            collapse_from_sticky_header(ui.ctx(), scroll_area_id, scroll_state, collapse);
        }

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

#[allow(clippy::too_many_arguments)]
fn show_series_preview(
    ui: &mut egui::Ui,
    series_description: Option<&str>,
    series_number: Option<i32>,
    modality: Option<&str>,
    slices: &[SliceItem],
    series_key: SeriesKey,
    selected_selection: Option<SliceSelection>,
    last_slice_index: Option<usize>,
    thumbnails: &mut SeriesThumbnailCache,
    clicked_selection: &mut Option<SliceSelection>,
) {
    if slices.is_empty() {
        return;
    }

    let selected_slice_index = selected_selection
        .filter(|selection| selection.series_key() == series_key)
        .map(|selection| selection.slice_index);
    let is_selected = selected_slice_index.is_some();
    let open_slice_index =
        series_open_slice_index(selected_slice_index, last_slice_index, slices.len());
    let selection = SliceSelection::new(series_key.0, series_key.1, series_key.2, open_slice_index);
    let series_title = series_title_text(modality, series_description);
    let series_number = series_number_text(series_number);
    let slice_count = slice_count_text(slices.len());

    let card_size = egui::vec2(ui.available_width(), SERIES_PREVIEW_HEIGHT);
    let (card_rect, mut response) = ui.allocate_exact_size(card_size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            is_selected,
            &series_title,
        )
    });

    if ui.is_rect_visible(card_rect) {
        thumbnails.request(ui.ctx(), series_key, slices);

        let visuals = ui.visuals();
        let card_fill = if is_selected {
            visuals
                .selection
                .bg_fill
                .lerp_to_gamma(visuals.panel_fill, 0.55)
        } else if response.hovered() {
            visuals.widgets.hovered.weak_bg_fill
        } else {
            visuals.faint_bg_color
        };
        ui.painter()
            .rect_filled(card_rect, theme::SPACE_XS, card_fill);

        let content_rect = card_rect.shrink(SERIES_PREVIEW_INSET);
        let thumbnail_size = content_rect.height();
        let thumbnail_rect =
            egui::Rect::from_min_size(content_rect.min, egui::vec2(thumbnail_size, thumbnail_size));
        ui.painter()
            .rect_filled(thumbnail_rect, theme::SPACE_XXS, egui::Color32::BLACK);

        if let Some(texture) = thumbnails.texture(series_key) {
            let image_rect = fit_texture_inside(texture.size_vec2(), thumbnail_rect);
            ui.painter().image(
                texture.id(),
                image_rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            let status = if thumbnails.failed(series_key) {
                "No preview"
            } else {
                "Loading…"
            };
            ui.painter().text(
                thumbnail_rect.center(),
                egui::Align2::CENTER_CENTER,
                status,
                egui::TextStyle::Small.resolve(ui.style()),
                visuals.weak_text_color(),
            );
        }

        let details_left = thumbnail_rect.right() + theme::SPACE_MD;
        let details_right = content_rect.right();
        let details_width = (details_right - details_left).max(0.0);
        let title_rect = egui::Rect::from_min_size(
            egui::pos2(details_left, content_rect.top() + theme::SPACE_XS),
            egui::vec2(details_width, 18.0),
        );
        let title_was_elided = paint_truncated_text(
            ui,
            title_rect,
            details_left,
            egui::RichText::new(&series_title).strong(),
            egui::TextStyle::Button,
            visuals.strong_text_color(),
        );

        let number_rect = title_rect.translate(egui::vec2(0.0, 22.0));
        paint_truncated_text(
            ui,
            number_rect,
            details_left,
            egui::RichText::new(&series_number).color(visuals.text_color()),
            egui::TextStyle::Body,
            visuals.text_color(),
        );

        let slice_count_rect = title_rect.translate(egui::vec2(0.0, 44.0));
        paint_truncated_text(
            ui,
            slice_count_rect,
            details_left,
            egui::RichText::new(&slice_count).color(visuals.text_color()),
            egui::TextStyle::Body,
            visuals.text_color(),
        );

        if title_was_elided {
            response = response.on_hover_text(&series_title);
        }
    }

    response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        *clicked_selection = Some(selection);
    }
}

fn fit_texture_inside(texture_size: egui::Vec2, bounds: egui::Rect) -> egui::Rect {
    if texture_size.x <= 0.0 || texture_size.y <= 0.0 {
        return bounds;
    }

    let scale = (bounds.width() / texture_size.x)
        .min(bounds.height() / texture_size.y)
        .max(0.0);
    egui::Rect::from_center_size(bounds.center(), texture_size * scale)
}

fn slice_count_label(slice_count: usize) -> &'static str {
    if slice_count == 1 { "slice" } else { "slices" }
}

fn series_title_text(modality: Option<&str>, description: Option<&str>) -> String {
    let description = description
        .filter(|description| !description.is_empty())
        .unwrap_or("Unknown Series");
    match modality.filter(|modality| !modality.is_empty()) {
        Some(modality) => format!("{modality} - {description}"),
        None => description.to_owned(),
    }
}

fn series_number_text(series_number: Option<i32>) -> String {
    series_number
        .map(|number| format!("Series {number}"))
        .unwrap_or_else(|| "Series number unavailable".to_owned())
}

fn slice_count_text(slice_count: usize) -> String {
    format!("{slice_count} {}", slice_count_label(slice_count))
}

fn series_open_slice_index(
    selected_slice_index: Option<usize>,
    last_slice_index: Option<usize>,
    slice_count: usize,
) -> usize {
    selected_slice_index
        .or(last_slice_index)
        .filter(|slice_index| *slice_index < slice_count)
        .unwrap_or(0)
}

fn show_tree_node(
    ui: &mut egui::Ui,
    id_salt: impl Hash,
    label: &str,
    default_open: bool,
    level: TreeNodeLevel,
    sticky_candidates: &mut Vec<StickyHeaderCandidate>,
    add_body: impl FnOnce(&mut egui::Ui, &mut Vec<StickyHeaderCandidate>),
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

    let Some(body) = state.show_body_indented(&header_response, ui, |ui| {
        add_body(ui, sticky_candidates);
    }) else {
        return;
    };

    // Only an open node whose body reaches the top of the viewport can need a sticky header.
    let viewport_top = ui.clip_rect().top();
    let header_rect = header_response.rect;
    let body_bottom = body.response.rect.bottom();

    if header_rect.top() < viewport_top + STICKY_STACK_MAX_HEIGHT && viewport_top < body_bottom {
        sticky_candidates.push(StickyHeaderCandidate {
            level,
            node_id: id,
            label: label.to_owned(),
            openness,
            header_rect,
            body_bottom,
        });
    }
}

fn show_tree_node_row(
    ui: &mut egui::Ui,
    label: &str,
    level: TreeNodeLevel,
    openness: f32,
) -> egui::Response {
    let row_size = egui::vec2(ui.available_width(), level.row_height());
    let (row_rect, response) = ui.allocate_exact_size(row_size, egui::Sense::click());

    paint_tree_node_row(ui, row_rect, response, label, level, openness)
}

fn paint_tree_node_row(
    ui: &mut egui::Ui,
    row_rect: egui::Rect,
    mut response: egui::Response,
    label: &str,
    level: TreeNodeLevel,
    openness: f32,
) -> egui::Response {
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

/// An open node laid out this frame whose header may need to stay pinned at the top of the
/// tree's viewport while its body is scrolled through.
#[derive(Clone)]
struct StickyHeaderCandidate {
    level: TreeNodeLevel,
    /// Collapsing-state id of the node, so the sticky copy can collapse it.
    node_id: egui::Id,
    label: String,
    openness: f32,
    header_rect: egui::Rect,
    body_bottom: f32,
}

struct StickyHeader<'a> {
    candidate: &'a StickyHeaderCandidate,
    rect: egui::Rect,
}

/// Picks, level by level, the open node enclosing the bottom of the stack built so far and pins
/// its header there. A header slides out as the end of its body approaches, and once a level is
/// not pinned no deeper level can be, because a child's header always sits below its parent's.
fn sticky_header_stack(
    viewport_top: f32,
    candidates: &[StickyHeaderCandidate],
) -> Vec<StickyHeader<'_>> {
    let mut stack = Vec::new();
    let mut stack_bottom = viewport_top;

    for level in TreeNodeLevel::ALL {
        let pinned = candidates.iter().find(|candidate| {
            candidate.level == level
                && candidate.header_rect.top() < stack_bottom
                && stack_bottom < candidate.body_bottom
        });
        let Some(candidate) = pinned else {
            break;
        };

        let height = level.row_height();
        let top = stack_bottom.min(candidate.body_bottom - height);
        let rect = egui::Rect::from_x_y_ranges(candidate.header_rect.x_range(), top..=top + height);

        stack.push(StickyHeader { candidate, rect });
        stack_bottom = rect.bottom() + TREE_ROW_GAP;
    }

    stack
}

/// A click on a sticky header: collapse `node_id` and scroll up by `scroll_delta` so the real
/// header lands where the sticky copy was.
struct StickyCollapse {
    node_id: egui::Id,
    scroll_delta: f32,
}

/// Keeps the patient, study, and series of the rows in view readable while scrolling by drawing
/// their headers pinned at the top of the viewport. Must run inside the scroll area's content
/// pass, after the rows. Returns the collapse requested by a click on a pinned header.
fn show_sticky_headers(
    ui: &mut egui::Ui,
    viewport: egui::Rect,
    candidates: &[StickyHeaderCandidate],
) -> Option<StickyCollapse> {
    let stack = sticky_header_stack(viewport.top(), candidates);
    let last = stack.last()?;

    // The scroll area lets its content bleed `clip_rect_margin` above the viewport, so the stack
    // must cover that band too or a sliver of a scrolled row shows above the pinned headers.
    let mut cover = viewport;
    cover.min.y -= ui.visuals().clip_rect_margin;

    let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(viewport));
    ui.set_clip_rect(ui.clip_rect().intersect(cover));

    let fill = ui.visuals().panel_fill;
    let separator = ui.visuals().widgets.noninteractive.bg_stroke;
    let stack_bottom = last.rect.bottom();
    let mut collapse = None;

    // Innermost first, so a header sliding out disappears behind its parent.
    for (level_index, sticky) in stack.iter().enumerate().rev() {
        let StickyHeader { candidate, rect } = sticky;
        let background_top = if level_index == 0 {
            cover.top().min(rect.top() - TREE_ROW_GAP)
        } else {
            rect.top() - TREE_ROW_GAP
        };
        let background =
            egui::Rect::from_x_y_ranges(viewport.x_range(), background_top..=rect.bottom());
        ui.painter().rect_filled(background, 0.0, fill);

        // One widget per level, whose content is whatever node is pinned there this frame.
        let id = ui.id().with(("sticky_tree_header", level_index));
        let response = ui.interact(*rect, id, egui::Sense::click());
        let response = paint_tree_node_row(
            &mut ui,
            *rect,
            response,
            &candidate.label,
            candidate.level,
            candidate.openness,
        );

        if response.clicked() {
            collapse = Some(StickyCollapse {
                node_id: candidate.node_id,
                scroll_delta: rect.top() - candidate.header_rect.top(),
            });
        }
    }

    ui.painter()
        .hline(viewport.x_range(), stack_bottom + 0.5, separator);

    collapse
}

/// Collapses the node and scrolls so its real header lands where the sticky copy was, so the
/// tree does not jump.
fn collapse_from_sticky_header(
    ctx: &egui::Context,
    scroll_area_id: egui::Id,
    mut scroll_state: egui::scroll_area::State,
    collapse: StickyCollapse,
) {
    if let Some(mut state) = egui::collapsing_header::CollapsingState::load(ctx, collapse.node_id) {
        state.set_open(false);
        state.store(ctx);
    }

    scroll_state.offset.y -= collapse.scroll_delta;
    scroll_state.store(ctx, scroll_area_id);
    ctx.request_repaint();
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

fn patient_total_series_count(patient: &PatientGroup) -> usize {
    patient
        .studies
        .iter()
        .map(|study| study.series_groups.len())
        .sum()
}

/// Whether a node opens on its own. Auto-collapse exists to bound layout cost, so it measures what
/// the active view actually draws: one row per slice in the file list, one card per series in the
/// preview view.
fn tree_node_default_open(
    view_mode: DicomTreeViewMode,
    slice_count: usize,
    series_count: usize,
) -> bool {
    match view_mode {
        DicomTreeViewMode::SeriesPreviews => series_count < TREE_AUTO_COLLAPSE_CARD_COUNT,
        DicomTreeViewMode::FileList => slice_count < TREE_AUTO_COLLAPSE_SLICE_COUNT,
    }
}

pub(super) fn auto_collapse_help_text(view_mode: DicomTreeViewMode) -> String {
    match view_mode {
        DicomTreeViewMode::SeriesPreviews => format!(
            "Off: patients or studies with {TREE_AUTO_COLLAPSE_CARD_COUNT}+ series start collapsed for performance."
        ),
        DicomTreeViewMode::FileList => format!(
            "Off: patients or studies with {TREE_AUTO_COLLAPSE_SLICE_COUNT}+ slices, and series with {SERIES_AUTO_COLLAPSE_SLICE_COUNT}+ slices, start collapsed for performance."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRIDE: f32 = INSTANCE_ROW_HEIGHT + TREE_ROW_GAP;

    fn range(min: f32, max: f32) -> egui::Rangef {
        egui::Rangef::new(min, max)
    }

    #[test]
    fn series_cards_open_at_start_then_resume_the_last_slice() {
        assert_eq!(series_open_slice_index(None, None, 50), 0);
        assert_eq!(series_open_slice_index(None, Some(17), 50), 17);
        assert_eq!(series_open_slice_index(Some(23), Some(17), 50), 23);
        assert_eq!(series_open_slice_index(None, Some(50), 50), 0);
    }

    #[test]
    fn series_details_are_split_into_title_number_and_count() {
        assert_eq!(
            series_title_text(Some("CT"), Some("CTA HEAD")),
            "CT - CTA HEAD"
        );
        assert_eq!(series_number_text(Some(12)), "Series 12");
        assert_eq!(slice_count_text(80), "80 slices");
        assert_eq!(series_title_text(None, None), "Unknown Series");
        assert_eq!(series_number_text(None), "Series number unavailable");
    }

    #[test]
    fn auto_collapse_counts_what_each_view_draws() {
        // One 1200-slice study is a handful of cards in the preview view but 1200 rows in the
        // file list, so only the file list collapses it.
        assert!(tree_node_default_open(
            DicomTreeViewMode::SeriesPreviews,
            1_200,
            3
        ));
        assert!(!tree_node_default_open(
            DicomTreeViewMode::FileList,
            1_200,
            3
        ));

        // A study with more cards than the preview threshold collapses in the preview view even
        // though its slices are few.
        assert!(!tree_node_default_open(
            DicomTreeViewMode::SeriesPreviews,
            300,
            TREE_AUTO_COLLAPSE_CARD_COUNT,
        ));
        assert!(tree_node_default_open(
            DicomTreeViewMode::FileList,
            300,
            TREE_AUTO_COLLAPSE_CARD_COUNT,
        ));
    }

    #[test]
    fn auto_collapse_help_matches_the_active_view_thresholds() {
        let previews = auto_collapse_help_text(DicomTreeViewMode::SeriesPreviews);
        let file_list = auto_collapse_help_text(DicomTreeViewMode::FileList);

        assert!(previews.contains("100+ series"), "{previews}");
        assert!(file_list.contains("1000+ slices"), "{file_list}");
        assert!(file_list.contains("200+ slices"), "{file_list}");
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

    fn candidate(level: TreeNodeLevel, header_top: f32, body_bottom: f32) -> StickyHeaderCandidate {
        let indent = match level {
            TreeNodeLevel::Patient => 0.0,
            TreeNodeLevel::Study => 18.0,
            TreeNodeLevel::Series => 36.0,
        };

        StickyHeaderCandidate {
            level,
            node_id: egui::Id::new((level, header_top.to_bits())),
            label: String::new(),
            openness: 1.0,
            header_rect: egui::Rect::from_min_size(
                egui::pos2(indent, header_top),
                egui::vec2(200.0 - indent, level.row_height()),
            ),
            body_bottom,
        }
    }

    fn stack_tops(stack: &[StickyHeader<'_>]) -> Vec<(TreeNodeLevel, f32, f32)> {
        stack
            .iter()
            .map(|sticky| {
                (
                    sticky.candidate.level,
                    sticky.rect.top(),
                    sticky.rect.bottom(),
                )
            })
            .collect()
    }

    #[test]
    fn nothing_is_pinned_until_a_header_scrolls_past_the_viewport_top() {
        let visible_patient = [candidate(TreeNodeLevel::Patient, 100.0, 900.0)];
        let lower_patient = [candidate(TreeNodeLevel::Patient, 120.0, 900.0)];

        assert!(sticky_header_stack(100.0, &[]).is_empty());
        assert!(sticky_header_stack(100.0, &visible_patient).is_empty());
        assert!(sticky_header_stack(100.0, &lower_patient).is_empty());
    }

    #[test]
    fn pinned_headers_stack_by_level_and_keep_their_indent() {
        let candidates = [
            candidate(TreeNodeLevel::Patient, 40.0, 900.0),
            candidate(TreeNodeLevel::Study, 70.0, 800.0),
            candidate(TreeNodeLevel::Series, 90.0, 700.0),
        ];
        let stack = sticky_header_stack(100.0, &candidates);

        assert_eq!(
            stack_tops(&stack),
            vec![
                (TreeNodeLevel::Patient, 100.0, 126.0),
                (TreeNodeLevel::Study, 128.0, 152.0),
                (TreeNodeLevel::Series, 154.0, 178.0),
            ]
        );
        assert_eq!(stack[1].rect.left(), 18.0);
        assert_eq!(stack[2].rect.left(), 36.0);
    }

    #[test]
    fn child_is_pinned_once_its_header_would_hide_under_the_stack() {
        let patient = candidate(TreeNodeLevel::Patient, 40.0, 900.0);
        // Header top 110 is inside the viewport but under the pinned patient header (100..128).
        let hidden_study = [
            patient.clone(),
            candidate(TreeNodeLevel::Study, 110.0, 800.0),
        ];
        // Header top 140 is visible below the stack, so it is not pinned.
        let visible_study = [patient, candidate(TreeNodeLevel::Study, 140.0, 800.0)];

        assert_eq!(sticky_header_stack(100.0, &hidden_study).len(), 2);
        assert_eq!(sticky_header_stack(100.0, &visible_study).len(), 1);
    }

    #[test]
    fn pinned_header_slides_out_as_its_body_ends() {
        let mut candidates = [
            candidate(TreeNodeLevel::Patient, 40.0, 900.0),
            candidate(TreeNodeLevel::Study, 70.0, 800.0),
            candidate(TreeNodeLevel::Series, 90.0, 170.0),
        ];

        // The series body ends at 170, so its 24px header is pushed up to 146..170.
        let stack = sticky_header_stack(100.0, &candidates);
        assert_eq!(stack_tops(&stack)[2], (TreeNodeLevel::Series, 146.0, 170.0));

        // Once the body ends at the stack line, the series header is gone.
        candidates[2].body_bottom = 154.0;
        assert_eq!(sticky_header_stack(100.0, &candidates).len(), 2);

        // A patient sliding out is partly above the viewport top.
        let leaving = [candidate(TreeNodeLevel::Patient, 40.0, 110.0)];
        assert_eq!(
            stack_tops(&sticky_header_stack(100.0, &leaving)),
            vec![(TreeNodeLevel::Patient, 84.0, 110.0)]
        );
    }

    #[test]
    fn deeper_levels_need_their_parent_pinned() {
        let candidates = [
            candidate(TreeNodeLevel::Patient, 40.0, 900.0),
            candidate(TreeNodeLevel::Series, 90.0, 700.0),
        ];

        assert_eq!(sticky_header_stack(100.0, &candidates).len(), 1);
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

    /// Every text drawn this frame, with the rect it occupies, outermost shapes first.
    fn painted_texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, egui::Rect)> {
        fn collect(shape: &egui::Shape, texts: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(text) => {
                    texts.push((
                        text.galley.text().to_owned(),
                        egui::Rect::from_min_size(text.pos, text.galley.size()),
                    ));
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, texts);
                    }
                }
                _ => {}
            }
        }

        let mut texts = Vec::new();
        for clipped in shapes {
            collect(&clipped.shape, &mut texts);
        }
        texts
    }

    fn show_single_card(
        context: &egui::Context,
        slices: &[SliceItem],
        selected: Option<SliceSelection>,
    ) -> Vec<(String, egui::Rect)> {
        let mut thumbnails = SeriesThumbnailCache::default();
        let output = run_frame(context, |ui| {
            let mut clicked_selection = None;
            show_series_preview(
                ui,
                Some("CTA HEAD"),
                Some(4),
                Some("CT"),
                slices,
                (0, 0, 0),
                selected,
                None,
                &mut thumbnails,
                &mut clicked_selection,
            );
        });

        painted_texts(&output.shapes)
    }

    #[test]
    fn a_series_card_names_the_series_above_its_summary() {
        let context = egui::Context::default();
        let slices = test_slices(394);

        let texts = show_single_card(&context, &slices, None);
        let line_of = |needle: &str| {
            texts
                .iter()
                .find(|(text, _)| text.starts_with(needle))
                .unwrap_or_else(|| panic!("no line starting with {needle:?} in {texts:?}"))
                .1
        };
        let title = line_of("CT - CTA HEAD");
        let number = line_of("Series 4");
        let count = line_of("394 slices");

        assert!(
            texts.iter().any(|(text, _)| text == "394 slices"),
            "{texts:?}"
        );
        assert!(title.bottom() <= number.top(), "{title:?} {number:?}");
        assert!(number.bottom() <= count.top(), "{number:?} {count:?}");
        assert!(
            count.bottom() - title.top() <= SERIES_PREVIEW_HEIGHT,
            "{title:?} {count:?}"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn scrolling_series_preview_cards_keeps_widget_ids_stable() {
        let slices = test_slices(40);
        let context = egui::Context::default();
        // Every card allocates whether or not it is on screen, so the thumbnail requests of the
        // visible ones are the only side effect; their files do not exist and fail immediately.
        let mut thumbnails = SeriesThumbnailCache::default();
        let show_scrolled_cards = |thumbnails: &mut SeriesThumbnailCache, scroll_offset: f32| {
            run_frame(&context, |ui| {
                ui.spacing_mut().item_spacing.y = TREE_ROW_GAP;
                egui::ScrollArea::vertical()
                    .vertical_scroll_offset(scroll_offset)
                    .show(ui, |ui| {
                        let mut clicked_selection = None;
                        for series_index in 0..30 {
                            show_series_preview(
                                ui,
                                Some("CTA HEAD"),
                                Some(1),
                                Some("CT"),
                                &slices,
                                (0, 0, series_index),
                                None,
                                None,
                                thumbnails,
                                &mut clicked_selection,
                            );
                        }
                    });
            })
        };

        show_scrolled_cards(&mut thumbnails, 0.0);

        let card_stride = SERIES_PREVIEW_HEIGHT + TREE_ROW_GAP;
        for scroll_offset in [
            card_stride,
            3.0 * card_stride,
            3.0 * card_stride + 7.0,
            9.0 * card_stride,
            card_stride,
            0.0,
        ] {
            let output = show_scrolled_cards(&mut thumbnails, scroll_offset);

            assert_eq!(
                id_change_warning_count(&output.shapes),
                0,
                "scroll offset {scroll_offset}"
            );
        }
    }
}
