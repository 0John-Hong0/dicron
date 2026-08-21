//! Visible metadata search and table UI.

use eframe::egui;

use crate::app::state::MetadataPanelState;
use crate::dicom::{DicomMetadata, MetadataItem};

mod table {
    //! Rendering for the tabular DICOM metadata view.

    use eframe::egui;

    use crate::dicom::MetadataItem;
    use crate::theme;

    const TAG_COLUMN_PREFERRED_WIDTH: f32 = 92.0;
    const DESCRIPTION_COLUMN_MAX_WIDTH: f32 = 190.0;
    const DESCRIPTION_COLUMN_SHARE: f32 = 0.60;
    const ROW_HEIGHT: f32 = 18.0;
    const COLUMN_GAP: f32 = theme::SPACE_SM;

    #[derive(Clone, Copy)]
    struct ColumnWidths {
        tag: f32,
        description: f32,
        value: f32,
    }

    impl ColumnWidths {
        fn for_available_width(available_width: f32) -> Self {
            let content_width = (available_width - COLUMN_GAP * 2.0).max(0.0);
            let tag = TAG_COLUMN_PREFERRED_WIDTH.min(content_width * 0.35);
            let remaining_width = (content_width - tag).max(0.0);
            let description =
                (remaining_width * DESCRIPTION_COLUMN_SHARE).min(DESCRIPTION_COLUMN_MAX_WIDTH);
            let value = (remaining_width - description).max(0.0);

            Self {
                tag,
                description,
                value,
            }
        }
    }

    pub(super) fn show(ui: &mut egui::Ui, metadata_items: &[&MetadataItem]) {
        egui::ScrollArea::vertical()
            .id_salt("dicom_metadata_table_scroll_area")
            .scroll_source(
                egui::scroll_area::ScrollSource::SCROLL_BAR
                    | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
            )
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let column_widths = ColumnWidths::for_available_width(ui.available_width());

                egui::Grid::new("dicom_metadata_table_grid")
                    .num_columns(3)
                    .striped(true)
                    .spacing([COLUMN_GAP, theme::SPACE_XS])
                    .min_col_width(0.0)
                    .show(ui, |ui| {
                        header_cell(ui, column_widths.tag, ROW_HEIGHT, "Tag ID");
                        header_cell(ui, column_widths.description, ROW_HEIGHT, "Description");
                        header_cell(ui, column_widths.value, ROW_HEIGHT, "Value");
                        ui.end_row();

                        for metadata_item in metadata_items {
                            cell(
                                ui,
                                column_widths.tag,
                                ROW_HEIGHT,
                                metadata_item.tag.as_str(),
                                "Copy tag ID",
                            );
                            cell(
                                ui,
                                column_widths.description,
                                ROW_HEIGHT,
                                metadata_item.description.as_str(),
                                "Copy description",
                            );
                            cell(
                                ui,
                                column_widths.value,
                                ROW_HEIGHT,
                                metadata_item.value.as_str(),
                                "Copy full value",
                            );
                            ui.end_row();
                        }
                    });
            });
    }

    fn header_cell(ui: &mut egui::Ui, width: f32, height: f32, text: &str) {
        ui.allocate_ui_with_layout(
            egui::vec2(width, height),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add(egui::Label::new(egui::RichText::new(text).strong()).truncate());
            },
        );
    }

    fn cell(
        ui: &mut egui::Ui,
        width: f32,
        height: f32,
        text: &str,
        copy_action: &str,
    ) -> egui::Response {
        let layout = egui::Layout::left_to_right(egui::Align::Center)
            .with_main_align(egui::Align::Min)
            .with_main_justify(true)
            .with_cross_justify(true);

        let response = ui
            .allocate_ui_with_layout(egui::vec2(width, height), layout, |ui| {
                ui.add(
                    egui::Label::new(text)
                        .selectable(true)
                        .wrap_mode(egui::TextWrapMode::Truncate),
                )
            })
            .inner;

        response.context_menu(|ui| {
            if ui.button(copy_action).clicked() {
                ui.ctx().copy_text(text.to_owned());
                ui.close();
            }
        });

        response
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn responsive_columns_fit_without_horizontal_overflow() {
            for available_width in [100.0, 236.0, 316.0, 776.0] {
                let widths = ColumnWidths::for_available_width(available_width);
                let used_width = widths.tag + widths.description + widths.value + COLUMN_GAP * 2.0;

                assert!((used_width - available_width).abs() < f32::EPSILON);
                assert!(widths.tag <= TAG_COLUMN_PREFERRED_WIDTH);
                assert!(widths.description <= DESCRIPTION_COLUMN_MAX_WIDTH);
                assert!(widths.value >= 0.0);
            }
        }
    }
}

impl MetadataPanelState {
    pub(in crate::app) fn replace(&mut self, metadata: DicomMetadata) {
        self.curated_items = metadata.curated_items;
        self.all_items = metadata.all_items;
        self.overlay = Some(metadata.overlay);
    }

    pub(in crate::app) fn clear(&mut self) {
        self.curated_items.clear();
        self.all_items.clear();
        self.overlay = None;
    }
}

pub(super) fn show(ui: &mut egui::Ui, state: &mut MetadataPanelState) {
    ui.heading("DICOM Tags");
    ui.separator();

    ui.horizontal(|ui| {
        ui.label("Search");
        ui.add(
            egui::TextEdit::singleline(&mut state.search_text)
                .hint_text("tag, name, value")
                .desired_width(f32::INFINITY),
        );
    });

    ui.checkbox(&mut state.show_all, "Show all tags");
    ui.separator();

    let active_metadata_items =
        active_items(state.show_all, &state.curated_items, &state.all_items);

    if active_metadata_items.is_empty() {
        ui.label("No DICOM tags loaded.");
        return;
    }

    let visible_metadata_items = filtered_items(active_metadata_items, &state.search_text);

    if visible_metadata_items.is_empty() {
        ui.label("No matching DICOM tags.");
        return;
    }

    table::show(ui, &visible_metadata_items);
}

fn active_items<'a>(
    show_all_metadata: bool,
    curated_metadata_items: &'a [MetadataItem],
    all_metadata_items: &'a [MetadataItem],
) -> &'a [MetadataItem] {
    if show_all_metadata {
        all_metadata_items
    } else {
        curated_metadata_items
    }
}

fn filtered_items<'a>(
    metadata_items: &'a [MetadataItem],
    metadata_search_text: &str,
) -> Vec<&'a MetadataItem> {
    let search_text = metadata_search_text.trim().to_lowercase();

    if search_text.is_empty() {
        return metadata_items.iter().collect();
    }

    metadata_items
        .iter()
        .filter(|metadata_item| metadata_item.matches_search(&search_text))
        .collect()
}
