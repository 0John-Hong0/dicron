//! Visible metadata search and table UI.

use eframe::egui;

use crate::app::state::MetadataPanelState;
use crate::dicom::{DicomMetadata, MetadataItem};

mod table;
mod tree;

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
        self.expanded_paths.clear();
    }
}

pub(super) fn show(ui: &mut egui::Ui, state: &mut MetadataPanelState) {
    ui.heading("DICOM Tags");
    ui.separator();

    ui.horizontal(|ui| {
        ui.label("Search");
        ui.add(
            egui::TextEdit::singleline(&mut state.search_text)
                .hint_text("tag, name, value, or path")
                .desired_width(f32::INFINITY),
        );
    });

    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut state.show_all, "Show all tags");
        if state.show_all {
            let can_change = state.search_text.trim().is_empty() && !state.all_items.is_empty();
            if ui
                .add_enabled(can_change, egui::Button::new("Expand all").small())
                .clicked()
            {
                tree::expand_all(&state.all_items, &mut state.expanded_paths);
            }
            if ui
                .add_enabled(can_change, egui::Button::new("Collapse all").small())
                .clicked()
            {
                state.expanded_paths.clear();
            }
        }
    });
    ui.separator();

    let active_metadata_items =
        active_items(state.show_all, &state.curated_items, &state.all_items);

    if active_metadata_items.is_empty() {
        ui.label("No DICOM tags loaded.");
        return;
    }

    let query = state.search_text.trim().to_lowercase();
    let visible_metadata_items =
        tree::visible_rows(active_metadata_items, &state.expanded_paths, &query);

    if visible_metadata_items.is_empty() {
        ui.label("No matching DICOM tags.");
        return;
    }

    table::show(
        ui,
        &visible_metadata_items,
        &mut state.expanded_paths,
        !query.is_empty(),
    );
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
