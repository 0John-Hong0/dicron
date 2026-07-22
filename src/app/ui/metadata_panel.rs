//! Visible metadata search and table UI.

use eframe::egui;

use crate::app::state::MetadataPanelState;
use crate::dicom::{DicomMetadata, MetadataItem};

mod table {
    use eframe::egui;

    use crate::dicom::MetadataItem;

    pub(super) fn show(ui: &mut egui::Ui, metadata_items: &[&MetadataItem]) {
        const TAG_COLUMN_WIDTH: f32 = 96.0;
        const DESCRIPTION_COLUMN_WIDTH: f32 = 190.0;
        const VALUE_COLUMN_WIDTH: f32 = 430.0;
        const ROW_HEIGHT: f32 = 18.0;
        const TABLE_MIN_WIDTH: f32 =
            TAG_COLUMN_WIDTH + DESCRIPTION_COLUMN_WIDTH + VALUE_COLUMN_WIDTH + 64.0;

        egui::ScrollArea::both()
            .id_salt("dicom_metadata_table_scroll_area")
            .scroll_source(
                egui::scroll_area::ScrollSource::SCROLL_BAR
                    | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
            )
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(TABLE_MIN_WIDTH);

                egui::Grid::new("dicom_metadata_table_grid")
                    .num_columns(3)
                    .striped(true)
                    .spacing([16.0, 6.0])
                    .min_col_width(0.0)
                    .show(ui, |ui| {
                        header_cell(ui, TAG_COLUMN_WIDTH, ROW_HEIGHT, "Tag ID");
                        header_cell(ui, DESCRIPTION_COLUMN_WIDTH, ROW_HEIGHT, "Description");
                        header_cell(ui, VALUE_COLUMN_WIDTH, ROW_HEIGHT, "Value");
                        ui.end_row();

                        for metadata_item in metadata_items {
                            cell(ui, TAG_COLUMN_WIDTH, ROW_HEIGHT, metadata_item.tag.as_str());
                            cell(
                                ui,
                                DESCRIPTION_COLUMN_WIDTH,
                                ROW_HEIGHT,
                                metadata_item.description.as_str(),
                            );
                            cell(
                                ui,
                                VALUE_COLUMN_WIDTH,
                                ROW_HEIGHT,
                                metadata_item.value.as_str(),
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
                ui.label(egui::RichText::new(text).strong());
            },
        );
    }

    fn cell(ui: &mut egui::Ui, width: f32, height: f32, text: &str) -> egui::Response {
        let layout = egui::Layout::left_to_right(egui::Align::Center)
            .with_main_align(egui::Align::Min)
            .with_main_justify(true)
            .with_cross_justify(true);

        ui.allocate_ui_with_layout(egui::vec2(width, height), layout, |ui| {
            ui.add(
                egui::Label::new(text)
                    .selectable(true)
                    .wrap_mode(egui::TextWrapMode::Extend),
            )
        })
        .inner
    }
}

impl MetadataPanelState {
    pub(in crate::app) fn replace(&mut self, metadata: DicomMetadata) {
        self.curated_items = metadata.curated_items;
        self.all_items = metadata.all_items;
    }

    pub(in crate::app) fn clear(&mut self) {
        self.curated_items.clear();
        self.all_items.clear();
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
