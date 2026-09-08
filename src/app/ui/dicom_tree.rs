//! Patient/Study/Series/Instance selection tree.

mod hierarchy;
mod series_preview;
mod slice_list;
#[cfg(test)]
mod test_support;

use eframe::egui;

use self::hierarchy::{
    TREE_ROW_GAP, TreeNodeLevel, collapse_from_sticky_header, show_sticky_headers, show_tree_node,
};
use self::series_preview::show_series_preview;
use self::slice_list::show_series_slices;
use crate::app::DicronApp;
use crate::dicom::{PatientGroup, StudyGroup};
use crate::settings::DicomTreeViewMode;

// Large subtrees start collapsed so malformed or unusually large studies do not make every frame
// expensive. The preview view draws one card per series, while the file-list view draws one row
// per slice.
const SERIES_AUTO_COLLAPSE_SLICE_COUNT: usize = 200;
const TREE_AUTO_COLLAPSE_SLICE_COUNT: usize = 1000;
const TREE_AUTO_COLLAPSE_CARD_COUNT: usize = 100;

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
}
