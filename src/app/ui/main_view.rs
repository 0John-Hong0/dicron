//! Composition of the application's major visible regions.

use eframe::egui;

use self::panel_resize::ResizeSide;
use super::toolbar::ToolbarAction;
use super::{metadata_panel, status_bar as status, toolbar, viewer};
use crate::app::DicronApp;
use crate::theme;

pub(super) fn show(app: &mut DicronApp, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    let check_for_updates_on_startup = app.settings.check_for_updates_on_startup;
    app.about_dialog
        .poll(ui.ctx(), check_for_updates_on_startup);

    app.receive_scan_messages(ui.ctx());
    app.handle_dropped_paths(ui.ctx());
    app.handle_keyboard_shortcuts(ui.ctx());
    app.handle_autoplay(ui.ctx());
    panel_resize::clamp_panel_widths(ui, &mut app.panel_layout);

    egui::Panel::top("toolbar_panel")
        .frame(theme::content_panel_frame(ui.style()))
        .show_inside(ui, |ui| {
            if let Some(action) = toolbar::show_actions(ui, app.settings.theme_preference) {
                app.handle_toolbar_action(ui.ctx(), action);
            }

            app.about_dialog.show(ui.ctx(), &mut app.settings);

            if toolbar::show_loaded_dicom_status(ui, app.selected_dicom_path.as_deref()) {
                ui.separator();

                if let Some(action) = viewer::show_controls(app, ui) {
                    app.handle_viewer_control_action(ui.ctx(), action);
                }
            }

            status::show_scan_status(ui, app.scan.progress());

            if status::show_error_status(ui, app.error_message.as_deref()) {
                app.error_message = None;
            }
        });

    if app.panel_layout.is_collapsed(ResizeSide::Left) {
        // `Panel::show_inside` consumes one auto ID. Keep later panel/widget IDs stable when this
        // conditional panel disappears, especially across the collapse discard pass.
        ui.skip_ahead_auto_ids(1);
    } else {
        egui::Panel::left("dicom_tree_panel")
            .exact_size(app.panel_layout.left_width())
            .resizable(false)
            .frame(theme::content_panel_frame(ui.style()))
            .show_inside(ui, |ui| {
                ui.take_available_width();

                ui.heading("DICOM Tree");
                ui.separator();

                let mut expand_tree = app.settings.expand_tree_by_default;
                if ui
                    .checkbox(&mut expand_tree, "Expand all by default")
                    .on_hover_text(
                        "Off: very large studies (1000+ slices) start collapsed for \
                         performance.",
                    )
                    .changed()
                {
                    app.settings.set_expand_tree_by_default(expand_tree);
                    app.tree_view_generation = app.tree_view_generation.wrapping_add(1);
                }

                app.show_dicom_tree(ui);
            });
    }

    panel_resize::show_resize_rail(ui, ResizeSide::Left, &mut app.panel_layout);

    if app.panel_layout.is_collapsed(ResizeSide::Right) {
        // Match the auto ID consumed by the expanded panel so the rail and viewer keep their IDs.
        ui.skip_ahead_auto_ids(1);
    } else {
        egui::Panel::right("metadata_panel")
            .exact_size(app.panel_layout.right_width())
            .resizable(false)
            .frame(theme::content_panel_frame(ui.style()))
            .show_inside(ui, |ui| {
                ui.take_available_width();
                metadata_panel::show(ui, &mut app.metadata);
            });
    }

    panel_resize::show_resize_rail(ui, ResizeSide::Right, &mut app.panel_layout);

    egui::CentralPanel::default()
        .frame(theme::viewer_frame(ui.style()))
        .show_inside(ui, |ui| {
            viewer::show(app, ui);
        });

    app.about_dialog.show_notification(ui.ctx());
}

impl DicronApp {
    fn handle_toolbar_action(&mut self, context: &egui::Context, action: ToolbarAction) {
        match action {
            ToolbarAction::OpenDicom => self.open_dicom_file(context),
            ToolbarAction::OpenFolder => self.open_dicom_folder(context),
            ToolbarAction::ShowAbout => self.about_dialog.open(context),
            ToolbarAction::SetTheme(theme_preference) => {
                self.set_theme_preference(context, theme_preference);
            }
        }
    }

    fn handle_viewer_control_action(
        &mut self,
        context: &egui::Context,
        action: viewer::ViewerControlAction,
    ) {
        match action {
            viewer::ViewerControlAction::ResetWindowLevel => self.reset_window_level(context),
        }
    }
}

mod panel_resize {
    use crate::app::state::{PanelLayout, PanelResizeDrag, SidePanelLayout};
    use crate::theme;
    use eframe::egui;

    const LEFT_MIN_WIDTH: f32 = 220.0;
    const RIGHT_MIN_WIDTH: f32 = 260.0;
    const LEFT_MAX_WIDTH: f32 = 700.0;
    const RIGHT_MAX_WIDTH: f32 = 800.0;
    const MIN_VIEWER_WIDTH: f32 = 300.0;
    const HANDLE_WIDTH: f32 = theme::SPACE_SM;
    const RESIZE_GRIP_LENGTH: f32 = 40.0;

    impl PanelLayout {
        pub(super) fn left_width(&self) -> f32 {
            self.effective_width(ResizeSide::Left)
        }

        pub(super) fn right_width(&self) -> f32 {
            self.effective_width(ResizeSide::Right)
        }

        pub(super) fn is_collapsed(&self, side: ResizeSide) -> bool {
            self.panel(side).collapsed
        }

        fn panel(&self, side: ResizeSide) -> &SidePanelLayout {
            match side {
                ResizeSide::Left => &self.left,
                ResizeSide::Right => &self.right,
            }
        }

        fn panel_mut(&mut self, side: ResizeSide) -> &mut SidePanelLayout {
            match side {
                ResizeSide::Left => &mut self.left,
                ResizeSide::Right => &mut self.right,
            }
        }

        fn effective_width(&self, side: ResizeSide) -> f32 {
            let panel = self.panel(side);
            if panel.collapsed { 0.0 } else { panel.width }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum ResizeSide {
        Left,
        Right,
    }

    pub(super) fn clamp_panel_widths(ui: &egui::Ui, layout: &mut PanelLayout) {
        clamp_panel_widths_for_content(layout, ui.ctx().content_rect().width());
    }

    pub(super) fn show_resize_rail(ui: &mut egui::Ui, side: ResizeSide, layout: &mut PanelLayout) {
        let rail_panel = match side {
            ResizeSide::Left => egui::Panel::left("left_panel_resize_rail"),
            ResizeSide::Right => egui::Panel::right("right_panel_resize_rail"),
        };

        rail_panel
            .exact_size(HANDLE_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::NONE)
            .show_inside(ui, |ui| {
                show_resize_handle(ui, side, layout);
            });
    }

    fn show_resize_handle(ui: &mut egui::Ui, side: ResizeSide, layout: &mut PanelLayout) {
        let handle_rect = ui.max_rect();
        let handle_id = match side {
            ResizeSide::Left => "left_panel_resize_handle",
            ResizeSide::Right => "right_panel_resize_handle",
        };
        let response = ui.interact(handle_rect, ui.id().with(handle_id), egui::Sense::drag());

        if response.hovered() || response.dragged() {
            show_resize_cursor(ui);
        }

        if response.drag_started()
            || (response.dragged() && layout.panel(side).resize_drag.is_none())
        {
            begin_resize_drag(layout, side);
        }

        if response.dragged()
            && let Some(total_drag_delta) = response.total_drag_delta()
        {
            let signed_drag_delta = side.signed_drag_delta(total_drag_delta.x);
            match update_panel_from_drag(
                layout,
                side,
                signed_drag_delta,
                ui.ctx().content_rect().width(),
            ) {
                PanelTransition::Collapsed => {
                    ui.ctx().request_discard("side panel collapsed");
                }
                PanelTransition::Restored => {
                    ui.ctx().request_discard("side panel restored");
                }
                PanelTransition::None => {}
            }
        }

        if response.drag_stopped() {
            finish_resize_drag(layout, side);
        }

        paint_resize_handle(ui, handle_rect, side, layout.is_collapsed(side), &response);
    }

    fn begin_resize_drag(layout: &mut PanelLayout, side: ResizeSide) {
        let panel = layout.panel_mut(side);
        if panel.resize_drag.is_some() {
            return;
        }

        panel.resize_drag = Some(PanelResizeDrag {
            start_width: if panel.collapsed { 0.0 } else { panel.width },
        });
    }

    fn finish_resize_drag(layout: &mut PanelLayout, side: ResizeSide) {
        layout.panel_mut(side).resize_drag = None;
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PanelTransition {
        None,
        Collapsed,
        Restored,
    }

    fn update_panel_from_drag(
        layout: &mut PanelLayout,
        side: ResizeSide,
        signed_drag_delta: f32,
        content_width: f32,
    ) -> PanelTransition {
        let Some(resize_drag) = layout.panel(side).resize_drag else {
            return PanelTransition::None;
        };

        let requested_width = resize_drag.start_width + signed_drag_delta;
        let should_be_collapsed = should_collapse(side, requested_width);

        if should_be_collapsed {
            if layout.is_collapsed(side) {
                PanelTransition::None
            } else {
                collapse_panel(layout, side);
                PanelTransition::Collapsed
            }
        } else {
            let was_collapsed = layout.is_collapsed(side);
            resize_expanded_panel(layout, side, requested_width, content_width);
            layout.panel_mut(side).collapsed = false;

            if was_collapsed {
                PanelTransition::Restored
            } else {
                PanelTransition::None
            }
        }
    }

    fn clamp_panel_widths_for_content(layout: &mut PanelLayout, content_width: f32) {
        for side in [ResizeSide::Left, ResizeSide::Right] {
            if !layout.is_collapsed(side) {
                let current_width = layout.panel(side).width;
                resize_expanded_panel(layout, side, current_width, content_width);
            }
        }
    }

    fn resize_expanded_panel(
        layout: &mut PanelLayout,
        side: ResizeSide,
        requested_width: f32,
        content_width: f32,
    ) {
        let width = clamp_expanded_width(layout, side, requested_width, content_width);
        layout.panel_mut(side).width = width;
    }

    fn clamp_expanded_width(
        layout: &PanelLayout,
        side: ResizeSide,
        requested_width: f32,
        content_width: f32,
    ) -> f32 {
        requested_width.clamp(
            side.minimum_width(),
            max_panel_width(layout, side, content_width),
        )
    }

    fn max_panel_width(layout: &PanelLayout, side: ResizeSide, content_width: f32) -> f32 {
        let opposite_width = layout.effective_width(side.opposite());
        let resize_rails_width = HANDLE_WIDTH * 2.0;
        (content_width - opposite_width - resize_rails_width - MIN_VIEWER_WIDTH)
            .clamp(side.minimum_width(), side.maximum_width())
    }

    fn collapse_panel(layout: &mut PanelLayout, side: ResizeSide) {
        layout.panel_mut(side).collapsed = true;
    }

    fn should_collapse(side: ResizeSide, requested_width: f32) -> bool {
        requested_width < side.collapse_threshold()
    }

    fn show_resize_cursor(ui: &mut egui::Ui) {
        ui.output_mut(|output| {
            output.cursor_icon = egui::CursorIcon::ResizeHorizontal;
        });
    }

    fn paint_resize_handle(
        ui: &egui::Ui,
        handle_rect: egui::Rect,
        side: ResizeSide,
        collapsed: bool,
        response: &egui::Response,
    ) {
        let visuals = if response.hovered() || response.dragged() {
            &ui.visuals().widgets.hovered
        } else {
            &ui.visuals().widgets.inactive
        };

        let rail_fill = if response.hovered() || response.dragged() {
            visuals.weak_bg_fill
        } else {
            ui.visuals().panel_fill
        };
        ui.painter().rect_filled(handle_rect, 0.0, rail_fill);

        let divider_x = match side {
            ResizeSide::Left => handle_rect.right() - 0.5,
            ResizeSide::Right => handle_rect.left() + 0.5,
        };
        ui.painter().line_segment(
            [
                egui::pos2(divider_x, handle_rect.top()),
                egui::pos2(divider_x, handle_rect.bottom()),
            ],
            ui.visuals().widgets.noninteractive.bg_stroke,
        );

        let half_grip_length =
            (RESIZE_GRIP_LENGTH / 2.0).min((handle_rect.height() / 2.0 - 2.0).max(0.0));
        let grip_center_x = handle_rect.center().x + side.grip_center_offset(collapsed);
        ui.painter().line_segment(
            [
                egui::pos2(grip_center_x, handle_rect.center().y - half_grip_length),
                egui::pos2(grip_center_x, handle_rect.center().y + half_grip_length),
            ],
            visuals.fg_stroke,
        );
    }

    impl ResizeSide {
        const fn opposite(self) -> Self {
            match self {
                Self::Left => Self::Right,
                Self::Right => Self::Left,
            }
        }

        const fn minimum_width(self) -> f32 {
            match self {
                Self::Left => LEFT_MIN_WIDTH,
                Self::Right => RIGHT_MIN_WIDTH,
            }
        }

        const fn maximum_width(self) -> f32 {
            match self {
                Self::Left => LEFT_MAX_WIDTH,
                Self::Right => RIGHT_MAX_WIDTH,
            }
        }

        const fn collapse_threshold(self) -> f32 {
            self.minimum_width() / 2.0
        }

        const fn signed_drag_delta(self, horizontal_drag_delta: f32) -> f32 {
            match self {
                Self::Left => horizontal_drag_delta,
                Self::Right => -horizontal_drag_delta,
            }
        }

        const fn grip_center_offset(self, collapsed: bool) -> f32 {
            if collapsed {
                return 0.0;
            }

            match self {
                Self::Left => -0.5,
                Self::Right => 0.5,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const WIDE_CONTENT: f32 = 2_000.0;

        fn show_test_panel_layout(ui: &mut egui::Ui, layout: &mut PanelLayout) {
            if layout.is_collapsed(ResizeSide::Left) {
                ui.skip_ahead_auto_ids(1);
            } else {
                egui::Panel::left("test_left_panel")
                    .exact_size(layout.left_width())
                    .resizable(false)
                    .show_inside(ui, |_| {});
            }

            show_resize_rail(ui, ResizeSide::Left, layout);

            if layout.is_collapsed(ResizeSide::Right) {
                ui.skip_ahead_auto_ids(1);
            } else {
                egui::Panel::right("test_right_panel")
                    .exact_size(layout.right_width())
                    .resizable(false)
                    .show_inside(ui, |_| {});
            }

            show_resize_rail(ui, ResizeSide::Right, layout);
            egui::CentralPanel::default().show_inside(ui, |_| {});
        }

        fn test_input() -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1_200.0, 700.0),
                )),
                ..Default::default()
            }
        }

        fn contains_outline_color(
            shapes: &[egui::epaint::ClippedShape],
            color: egui::Color32,
        ) -> bool {
            fn shape_contains_color(shape: &egui::Shape, color: egui::Color32) -> bool {
                match shape {
                    egui::Shape::Rect(rect) => {
                        rect.stroke.color == color && rect.stroke.width > 0.0
                    }
                    egui::Shape::Vec(shapes) => shapes
                        .iter()
                        .any(|shape| shape_contains_color(shape, color)),
                    _ => false,
                }
            }

            shapes
                .iter()
                .any(|shape| shape_contains_color(&shape.shape, color))
        }

        #[test]
        fn expanded_widths_clamp_to_normal_limits() {
            let mut layout = PanelLayout::default();
            collapse_panel(&mut layout, ResizeSide::Right);

            resize_expanded_panel(&mut layout, ResizeSide::Left, 10.0, WIDE_CONTENT);
            assert_eq!(layout.left.width, LEFT_MIN_WIDTH);

            resize_expanded_panel(&mut layout, ResizeSide::Left, 1_000.0, WIDE_CONTENT);
            assert_eq!(layout.left.width, LEFT_MAX_WIDTH);

            layout.right.collapsed = false;
            collapse_panel(&mut layout, ResizeSide::Left);

            resize_expanded_panel(&mut layout, ResizeSide::Right, 10.0, WIDE_CONTENT);
            assert_eq!(layout.right.width, RIGHT_MIN_WIDTH);

            resize_expanded_panel(&mut layout, ResizeSide::Right, 1_000.0, WIDE_CONTENT);
            assert_eq!(layout.right.width, RIGHT_MAX_WIDTH);
        }

        #[test]
        fn collapse_requires_dragging_into_the_outer_edge_threshold() {
            let left_threshold = LEFT_MIN_WIDTH / 2.0;
            let right_threshold = RIGHT_MIN_WIDTH / 2.0;

            assert!(should_collapse(ResizeSide::Left, left_threshold - 1.0));
            assert!(!should_collapse(ResizeSide::Left, left_threshold));
            assert!(should_collapse(ResizeSide::Right, right_threshold - 1.0));
            assert!(!should_collapse(ResizeSide::Right, right_threshold));
        }

        #[test]
        fn collapsed_grips_use_the_geometric_rail_center() {
            assert_eq!(ResizeSide::Left.grip_center_offset(true), 0.0);
            assert_eq!(ResizeSide::Right.grip_center_offset(true), 0.0);
            assert_eq!(ResizeSide::Left.grip_center_offset(false), -0.5);
            assert_eq!(ResizeSide::Right.grip_center_offset(false), 0.5);
        }

        #[test]
        fn collapsed_panels_survive_clamping_and_use_no_layout_width() {
            let mut layout = PanelLayout::default();
            collapse_panel(&mut layout, ResizeSide::Left);
            collapse_panel(&mut layout, ResizeSide::Right);

            clamp_panel_widths_for_content(&mut layout, 700.0);

            assert!(layout.is_collapsed(ResizeSide::Left));
            assert!(layout.is_collapsed(ResizeSide::Right));
            assert_eq!(layout.left_width(), 0.0);
            assert_eq!(layout.right_width(), 0.0);
        }

        #[test]
        fn a_collapsed_opposite_panel_increases_the_available_maximum() {
            let mut layout = PanelLayout::default();
            let content_width = 900.0;

            assert_eq!(
                max_panel_width(&layout, ResizeSide::Left, content_width),
                244.0
            );

            collapse_panel(&mut layout, ResizeSide::Right);

            assert_eq!(
                max_panel_width(&layout, ResizeSide::Left, content_width),
                584.0
            );
        }

        #[test]
        fn clamping_preserves_the_minimum_viewer_width() {
            let mut layout = PanelLayout::default();
            layout.left.width = LEFT_MAX_WIDTH;
            layout.right.width = RIGHT_MAX_WIDTH;
            let content_width = 1_200.0;

            clamp_panel_widths_for_content(&mut layout, content_width);

            let viewer_width =
                content_width - layout.left_width() - layout.right_width() - HANDLE_WIDTH * 2.0;
            assert!(viewer_width >= MIN_VIEWER_WIDTH);
        }

        #[test]
        fn one_expanded_drag_can_collapse_and_restore_without_releasing() {
            let mut layout = PanelLayout::default();
            layout.left.width = 350.0;
            begin_resize_drag(&mut layout, ResizeSide::Left);

            assert_eq!(
                update_panel_from_drag(&mut layout, ResizeSide::Left, -250.0, WIDE_CONTENT),
                PanelTransition::Collapsed
            );
            assert!(layout.is_collapsed(ResizeSide::Left));
            assert!(layout.left.resize_drag.is_some());

            assert_eq!(
                update_panel_from_drag(&mut layout, ResizeSide::Left, -200.0, WIDE_CONTENT),
                PanelTransition::Restored
            );
            assert!(!layout.is_collapsed(ResizeSide::Left));
            assert_eq!(layout.left.width, LEFT_MIN_WIDTH);
            assert!(layout.left.resize_drag.is_some());
            clamp_panel_widths_for_content(&mut layout, WIDE_CONTENT);
            assert_eq!(layout.left.width, LEFT_MIN_WIDTH);

            finish_resize_drag(&mut layout, ResizeSide::Left);
            assert_eq!(layout.left.width, LEFT_MIN_WIDTH);
            assert!(layout.left.resize_drag.is_none());
        }

        #[test]
        fn a_drag_started_collapsed_uses_the_same_threshold_and_pointer_origin() {
            let mut layout = PanelLayout::default();
            layout.left.width = 350.0;
            collapse_panel(&mut layout, ResizeSide::Left);
            begin_resize_drag(&mut layout, ResizeSide::Left);
            let threshold = ResizeSide::Left.collapse_threshold();

            assert_eq!(layout.left.resize_drag.unwrap().start_width, 0.0);

            assert_eq!(
                update_panel_from_drag(
                    &mut layout,
                    ResizeSide::Left,
                    threshold - 1.0,
                    WIDE_CONTENT,
                ),
                PanelTransition::None
            );
            assert!(layout.is_collapsed(ResizeSide::Left));

            assert_eq!(
                update_panel_from_drag(&mut layout, ResizeSide::Left, threshold, WIDE_CONTENT,),
                PanelTransition::Restored
            );
            assert_eq!(layout.left.width, LEFT_MIN_WIDTH);
            clamp_panel_widths_for_content(&mut layout, WIDE_CONTENT);
            assert_eq!(layout.left.width, LEFT_MIN_WIDTH);

            assert_eq!(
                update_panel_from_drag(
                    &mut layout,
                    ResizeSide::Left,
                    threshold - 1.0,
                    WIDE_CONTENT,
                ),
                PanelTransition::Collapsed
            );
            assert!(layout.left.resize_drag.is_some());
        }

        #[test]
        fn panels_collapse_and_restore_independently() {
            let mut layout = PanelLayout::default();
            collapse_panel(&mut layout, ResizeSide::Left);
            collapse_panel(&mut layout, ResizeSide::Right);

            begin_resize_drag(&mut layout, ResizeSide::Right);
            assert_eq!(
                update_panel_from_drag(
                    &mut layout,
                    ResizeSide::Right,
                    ResizeSide::Right.collapse_threshold() + 1.0,
                    WIDE_CONTENT,
                ),
                PanelTransition::Restored
            );
            finish_resize_drag(&mut layout, ResizeSide::Right);

            assert!(layout.is_collapsed(ResizeSide::Left));
            assert!(!layout.is_collapsed(ResizeSide::Right));
            assert_eq!(layout.right.width, RIGHT_MIN_WIDTH);

            begin_resize_drag(&mut layout, ResizeSide::Left);
            assert_eq!(
                update_panel_from_drag(
                    &mut layout,
                    ResizeSide::Left,
                    ResizeSide::Left.collapse_threshold() + 1.0,
                    WIDE_CONTENT,
                ),
                PanelTransition::Restored
            );
            finish_resize_drag(&mut layout, ResizeSide::Left);

            assert!(!layout.is_collapsed(ResizeSide::Left));
            assert_eq!(layout.left.width, LEFT_MIN_WIDTH);
        }

        #[test]
        fn collapsing_either_panel_does_not_paint_an_egui_error_outline() {
            for side in [ResizeSide::Left, ResizeSide::Right] {
                let context = egui::Context::default();
                let error_color = context.style_of(egui::Theme::Dark).visuals.error_fg_color;
                let mut layout = PanelLayout::default();

                let _ = context.run_ui(test_input(), |ui| {
                    show_test_panel_layout(ui, &mut layout);
                });

                collapse_panel(&mut layout, side);
                let output = context.run_ui(test_input(), |ui| {
                    show_test_panel_layout(ui, &mut layout);
                });

                assert!(
                    !contains_outline_color(&output.shapes, error_color),
                    "{side:?} collapse painted an egui error outline"
                );
            }
        }
    }
}
