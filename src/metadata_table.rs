use eframe::egui;

use crate::dicom::MetadataItem;

pub fn show(ui: &mut egui::Ui, metadata_items: &[&MetadataItem]) {
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
