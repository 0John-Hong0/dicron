use super::*;

impl eframe::App for DicronApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_scan_messages(ui.ctx());
        self.handle_dropped_paths(ui.ctx());
        self.handle_keyboard_shortcuts(ui.ctx());
        self.handle_autoplay(ui.ctx());
        self.clamp_panel_widths(ui);

        egui::Panel::top("toolbar_panel").show_inside(ui, |ui| {
            panel_content_frame().show(ui, |ui| {
                self.show_toolbar_actions(ui);

                self.about_dialog.show(ui.ctx());

                if let Some(selected_dicom_path) = &self.selected_dicom_path {
                    if self.selected_dicom_frame_count > 1 {
                        ui.label(format!(
                            "{} [frame {} / {}]",
                            selected_dicom_path.display(),
                            self.selected_dicom_frame_index + 1,
                            self.selected_dicom_frame_count
                        ));
                    } else {
                        ui.label(selected_dicom_path.display().to_string());
                    }
                }

                if self.selected_dicom_path.is_some() {
                    ui.separator();

                    ui.horizontal(|ui| {
                        show_window_level_readout(ui, "WindowCenter", self.window_center);
                        show_window_level_readout(ui, "WindowWidth", self.window_width);

                        if ui.button("Reset WL").clicked() {
                            self.window_center = self.default_window_center;
                            self.window_width = self.default_window_width;
                            self.window_customized = false;
                            self.clear_current_series_window_level();
                            self.refresh_dicom_texture(ui.ctx());
                        }

                        if let (Some(selected_slice_index), Some(slice_count)) =
                            (self.current_slice_index(), self.current_slice_count())
                        {
                            ui.separator();
                            ui.label(format!(
                                "Slice {} / {}",
                                selected_slice_index + 1,
                                slice_count
                            ));
                        }
                    });
                }

                if let Some(scan_state) = &self.scan_state {
                    ui.separator();

                    ui.label(format!("Scanning: {}", scan_state.source_label));

                    let progress = if scan_state.total_file_count == 0 {
                        0.0
                    } else {
                        scan_state.processed_file_count as f32 / scan_state.total_file_count as f32
                    };

                    let eta_text = estimate_scan_eta(scan_state)
                        .map(format_duration)
                        .unwrap_or_else(|| "calculating...".to_owned());

                    ui.add(
                        egui::ProgressBar::new(progress)
                            .show_percentage()
                            .text(format!(
                                "Scanning {} / {} files | {} DICOM | ETA {}",
                                scan_state.processed_file_count,
                                scan_state.total_file_count,
                                scan_state.readable_dicom_count,
                                eta_text
                            )),
                    );
                }

                if self.error_message.is_some() {
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("✕").on_hover_text("Dismiss error").clicked() {
                            self.error_message = None;
                        }

                        if let Some(error_message) = &self.error_message {
                            ui.colored_label(egui::Color32::RED, error_message);
                        }
                    });
                }
            });
        });

        egui::Panel::left("dicom_tree_panel")
            .exact_size(self.left_panel_width)
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.take_available_width();

                side_panel_frame().show(ui, |ui| {
                    ui.heading("DICOM Tree");
                    ui.separator();

                    let mut expand_tree = self.dialog_directories.expand_tree_by_default;
                    if ui
                        .checkbox(&mut expand_tree, "Expand all by default")
                        .on_hover_text(
                            "Off: very large studies (1000+ slices) start collapsed for \
                             performance.",
                        )
                        .changed()
                    {
                        self.dialog_directories
                            .set_expand_tree_by_default(expand_tree);
                        self.tree_view_generation = self.tree_view_generation.wrapping_add(1);
                    }

                    self.show_dicom_tree(ui);
                });

                self.show_resize_handle(ui, ResizeSide::Left);
            });

        egui::Panel::right("metadata_panel")
            .exact_size(self.right_panel_width)
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.take_available_width();

                side_panel_frame().show(ui, |ui| {
                    ui.heading("DICOM Tags");
                    ui.separator();

                    ui.horizontal(|ui| {
                        ui.label("Search");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.metadata_search_text)
                                .hint_text("tag, name, value")
                                .desired_width(f32::INFINITY),
                        );
                    });

                    ui.checkbox(&mut self.show_all_metadata, "Show all tags");
                    ui.separator();

                    let visible_metadata_items = self.filtered_metadata_items();

                    if self.active_metadata_items().is_empty() {
                        ui.label("No DICOM tags loaded.");
                        return;
                    }

                    if visible_metadata_items.is_empty() {
                        ui.label("No matching DICOM tags.");
                        return;
                    }

                    metadata_table::show(ui, &visible_metadata_items);
                });

                self.show_resize_handle(ui, ResizeSide::Right);
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.show_viewer_panel(ui);
        });
    }
}

impl DicronApp {
    pub(super) fn show_toolbar_actions(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Open DICOM").clicked() {
                self.open_dicom_file(ui.ctx());
            }

            if ui.button("Open Folder").clicked() {
                self.open_dicom_folder(ui.ctx());
            }

            if ui.button("About").clicked() {
                self.about_dialog.open();
            }
        });
    }

    pub(super) fn clamp_panel_widths(&mut self, ui: &egui::Ui) {
        self.left_panel_width = self
            .left_panel_width
            .clamp(LEFT_PANEL_MIN_WIDTH, self.max_left_panel_width(ui));
        self.right_panel_width = self
            .right_panel_width
            .clamp(RIGHT_PANEL_MIN_WIDTH, self.max_right_panel_width(ui));
    }

    pub(super) fn show_resize_handle(&mut self, ui: &mut egui::Ui, side: ResizeSide) {
        let panel_rect = ui.max_rect();
        let separator_x = match side {
            ResizeSide::Left => panel_rect.right(),
            ResizeSide::Right => panel_rect.left(),
        };
        let half_handle_width = RESIZE_HANDLE_WIDTH / 2.0;
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
                    self.left_panel_width =
                        requested_width.clamp(LEFT_PANEL_MIN_WIDTH, self.max_left_panel_width(ui));
                }
                ResizeSide::Right => {
                    let requested_width = content_rect.right() - pointer_position.x;
                    self.right_panel_width = requested_width
                        .clamp(RIGHT_PANEL_MIN_WIDTH, self.max_right_panel_width(ui));
                }
            }
        }
    }

    pub(super) fn max_left_panel_width(&self, ui: &egui::Ui) -> f32 {
        (ui.ctx().content_rect().width() - self.right_panel_width - MIN_VIEWER_WIDTH)
            .clamp(LEFT_PANEL_MIN_WIDTH, LEFT_PANEL_MAX_WIDTH)
    }

    pub(super) fn max_right_panel_width(&self, ui: &egui::Ui) -> f32 {
        (ui.ctx().content_rect().width() - self.left_panel_width - MIN_VIEWER_WIDTH)
            .clamp(RIGHT_PANEL_MIN_WIDTH, RIGHT_PANEL_MAX_WIDTH)
    }

    pub(super) fn active_metadata_items(&self) -> &[MetadataItem] {
        if self.show_all_metadata {
            &self.all_metadata_items
        } else {
            &self.curated_metadata_items
        }
    }

    pub(super) fn filtered_metadata_items(&self) -> Vec<&MetadataItem> {
        let search_text = self.metadata_search_text.trim().to_lowercase();

        if search_text.is_empty() {
            return self.active_metadata_items().iter().collect();
        }

        self.active_metadata_items()
            .iter()
            .filter(|metadata_item| metadata_item.matches_search(&search_text))
            .collect()
    }

    pub(super) fn clear_metadata_items(&mut self) {
        self.curated_metadata_items.clear();
        self.all_metadata_items.clear();
    }
}

fn panel_content_frame() -> egui::Frame {
    egui::Frame::NONE.inner_margin(egui::Margin::symmetric(
        PANEL_CONTENT_MARGIN_X,
        PANEL_CONTENT_MARGIN_Y,
    ))
}

fn side_panel_frame() -> egui::Frame {
    egui::Frame::NONE.inner_margin(egui::Margin::symmetric(
        SIDE_PANEL_MARGIN_X,
        SIDE_PANEL_MARGIN_Y,
    ))
}

fn estimate_scan_eta(scan_state: &DicomFolderScanState) -> Option<Duration> {
    if scan_state.processed_file_count == 0 || scan_state.total_file_count == 0 {
        return None;
    }

    let elapsed_seconds = scan_state.started_at.elapsed().as_secs_f64();
    let seconds_per_file = elapsed_seconds / scan_state.processed_file_count as f64;
    let remaining_file_count = scan_state
        .total_file_count
        .saturating_sub(scan_state.processed_file_count);

    Some(Duration::from_secs_f64(
        seconds_per_file * remaining_file_count as f64,
    ))
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();

    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
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

fn show_window_level_readout(ui: &mut egui::Ui, label: &str, value: f64) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).weak());
        ui.label(egui::RichText::new(format!("{value:.1}")).monospace());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_rolls_over_to_hours() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 1m 1s");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h 0m 0s");
    }
}
