//! Composition of the application's major visible regions.

use eframe::egui;

use self::panel_resize::ResizeSide;
use super::toolbar::ToolbarAction;
use super::{metadata_panel, status_bar as status, toolbar, viewer};
use crate::app::DicronApp;

const CONTENT_MARGIN_X: i8 = 10;
const CONTENT_MARGIN_Y: i8 = 6;
const SIDE_MARGIN_X: i8 = 4;
const SIDE_MARGIN_Y: i8 = 8;

pub(super) fn show(app: &mut DicronApp, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    let check_for_updates_on_startup = app.settings.check_for_updates_on_startup;
    app.about_dialog
        .poll(ui.ctx(), check_for_updates_on_startup);

    app.receive_scan_messages(ui.ctx());
    app.handle_dropped_paths(ui.ctx());
    app.handle_keyboard_shortcuts(ui.ctx());
    app.handle_autoplay(ui.ctx());
    panel_resize::clamp_panel_widths(ui, &mut app.panel_layout);

    egui::Panel::top("toolbar_panel").show_inside(ui, |ui| {
        panel_content_frame().show(ui, |ui| {
            if let Some(action) = toolbar::show_actions(ui) {
                app.handle_toolbar_action(ui.ctx(), action);
            }

            app.about_dialog.show(ui.ctx(), &mut app.settings);

            let selected_slice = app.current_slice_index().zip(app.current_slice_count());
            let window_level = app.window_level.current();

            if let Some(action) = toolbar::show_loaded_dicom_status(
                ui,
                app.selected_dicom_path.as_deref(),
                app.selected_dicom_frame_index,
                app.selected_dicom_frame_count,
                window_level.center,
                window_level.width,
                selected_slice,
            ) {
                app.handle_toolbar_action(ui.ctx(), action);
            }

            status::show_scan_status(ui, app.scan.progress());

            if status::show_error_status(ui, app.error_message.as_deref()) {
                app.error_message = None;
            }
        });
    });

    egui::Panel::left("dicom_tree_panel")
        .exact_size(app.panel_layout.left_width())
        .resizable(false)
        .show_inside(ui, |ui| {
            ui.take_available_width();

            side_panel_frame().show(ui, |ui| {
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

            panel_resize::show_resize_handle(ui, ResizeSide::Left, &mut app.panel_layout);
        });

    egui::Panel::right("metadata_panel")
        .exact_size(app.panel_layout.right_width())
        .resizable(false)
        .show_inside(ui, |ui| {
            ui.take_available_width();

            side_panel_frame().show(ui, |ui| {
                metadata_panel::show(ui, &mut app.metadata);
            });

            panel_resize::show_resize_handle(ui, ResizeSide::Right, &mut app.panel_layout);
        });

    egui::CentralPanel::default().show_inside(ui, |ui| {
        viewer::show(app, ui);
    });

    app.about_dialog.show_notification(ui.ctx());
}

impl DicronApp {
    fn handle_toolbar_action(&mut self, context: &egui::Context, action: ToolbarAction) {
        match action {
            ToolbarAction::OpenDicom => self.open_dicom_file(context),
            ToolbarAction::OpenFolder => self.open_dicom_folder(context),
            ToolbarAction::OpenAbout => self.about_dialog.open(context),
            ToolbarAction::ResetWindowLevel => self.reset_window_level(context),
        }
    }
}

fn panel_content_frame() -> egui::Frame {
    egui::Frame::NONE.inner_margin(egui::Margin::symmetric(CONTENT_MARGIN_X, CONTENT_MARGIN_Y))
}

fn side_panel_frame() -> egui::Frame {
    egui::Frame::NONE.inner_margin(egui::Margin::symmetric(SIDE_MARGIN_X, SIDE_MARGIN_Y))
}

mod panel_resize {
    use crate::app::state::PanelLayout;
    use eframe::egui;

    const LEFT_MIN_WIDTH: f32 = 220.0;
    const RIGHT_MIN_WIDTH: f32 = 260.0;
    const LEFT_MAX_WIDTH: f32 = 700.0;
    const RIGHT_MAX_WIDTH: f32 = 800.0;
    const MIN_VIEWER_WIDTH: f32 = 300.0;
    const HANDLE_WIDTH: f32 = 8.0;

    impl PanelLayout {
        pub(super) fn left_width(&self) -> f32 {
            self.left_width
        }

        pub(super) fn right_width(&self) -> f32 {
            self.right_width
        }
    }

    #[derive(Clone, Copy)]
    pub(super) enum ResizeSide {
        Left,
        Right,
    }

    pub(super) fn clamp_panel_widths(ui: &egui::Ui, layout: &mut PanelLayout) {
        layout.left_width = layout
            .left_width
            .clamp(LEFT_MIN_WIDTH, max_left_panel_width(ui, layout.right_width));
        layout.right_width = layout.right_width.clamp(
            RIGHT_MIN_WIDTH,
            max_right_panel_width(ui, layout.left_width),
        );
    }

    pub(super) fn show_resize_handle(
        ui: &mut egui::Ui,
        side: ResizeSide,
        layout: &mut PanelLayout,
    ) {
        let panel_rect = ui.max_rect();
        let separator_x = match side {
            ResizeSide::Left => panel_rect.right(),
            ResizeSide::Right => panel_rect.left(),
        };
        let half_handle_width = HANDLE_WIDTH / 2.0;
        let handle_rect = egui::Rect::from_min_max(
            egui::pos2(separator_x - half_handle_width, panel_rect.top()),
            egui::pos2(separator_x + half_handle_width, panel_rect.bottom()),
        );

        let handle_id = match side {
            ResizeSide::Left => "left_panel_resize_handle",
            ResizeSide::Right => "right_panel_resize_handle",
        };

        let response = ui.interact(
            handle_rect,
            ui.id().with(handle_id),
            egui::Sense::click_and_drag(),
        );

        if response.hovered() || response.dragged() {
            ui.output_mut(|output| {
                output.cursor_icon = egui::CursorIcon::ResizeHorizontal;
            });
        }

        if response.dragged()
            && let Some(pointer_position) = pointer_position_inside_content(ui)
        {
            let content_rect = ui.ctx().content_rect();

            match side {
                ResizeSide::Left => {
                    let requested_width = pointer_position.x - content_rect.left();
                    layout.left_width = requested_width
                        .clamp(LEFT_MIN_WIDTH, max_left_panel_width(ui, layout.right_width));
                }
                ResizeSide::Right => {
                    let requested_width = content_rect.right() - pointer_position.x;
                    layout.right_width = requested_width.clamp(
                        RIGHT_MIN_WIDTH,
                        max_right_panel_width(ui, layout.left_width),
                    );
                }
            }
        }
    }

    fn max_left_panel_width(ui: &egui::Ui, right_panel_width: f32) -> f32 {
        (ui.ctx().content_rect().width() - right_panel_width - MIN_VIEWER_WIDTH)
            .clamp(LEFT_MIN_WIDTH, LEFT_MAX_WIDTH)
    }

    fn max_right_panel_width(ui: &egui::Ui, left_panel_width: f32) -> f32 {
        (ui.ctx().content_rect().width() - left_panel_width - MIN_VIEWER_WIDTH)
            .clamp(RIGHT_MIN_WIDTH, RIGHT_MAX_WIDTH)
    }

    fn pointer_position_inside_content(ui: &egui::Ui) -> Option<egui::Pos2> {
        let content_rect = ui.ctx().content_rect();

        ui.input(|input_state| {
            input_state
                .pointer
                .interact_pos()
                .filter(|pointer_position| content_rect.contains(*pointer_position))
        })
    }
}
