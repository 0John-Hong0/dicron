//! Collapsible hierarchy rows and sticky headers for the DICOM tree.

use std::hash::Hash;

use eframe::egui;

use crate::theme;

pub(super) const TREE_ROW_GAP: f32 = theme::SPACE_XXS;
const PATIENT_ROW_HEIGHT: f32 = 26.0;
const STUDY_ROW_HEIGHT: f32 = 24.0;
const SERIES_ROW_HEIGHT: f32 = 24.0;
// Tallest possible stack of sticky headers: one header per level plus the gaps between them.
const STICKY_STACK_MAX_HEIGHT: f32 =
    PATIENT_ROW_HEIGHT + STUDY_ROW_HEIGHT + SERIES_ROW_HEIGHT + 3.0 * TREE_ROW_GAP;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum TreeNodeLevel {
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

pub(super) fn show_tree_node(
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
            TreeNodeLevel::Study | TreeNodeLevel::Series => {
                egui::RichText::new(label).color(text_color)
            }
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
pub(super) struct StickyHeaderCandidate {
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
pub(super) struct StickyCollapse {
    node_id: egui::Id,
    scroll_delta: f32,
}

/// Keeps the patient, study, and series of the rows in view readable while scrolling by drawing
/// their headers pinned at the top of the viewport. Must run inside the scroll area's content
/// pass, after the rows. Returns the collapse requested by a click on a pinned header.
pub(super) fn show_sticky_headers(
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
pub(super) fn collapse_from_sticky_header(
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

pub(super) fn paint_truncated_text(
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
