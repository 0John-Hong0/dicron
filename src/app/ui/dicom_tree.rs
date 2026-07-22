//! Patient/Study/Series/Instance selection tree.

use eframe::egui;

use crate::app::DicronApp;
use crate::app::state::{SeriesKey, SliceSelection};
use crate::dicom::{PatientGroup, SliceItem, StudyGroup};

const SLICE_LIST_VIRTUALIZE_THRESHOLD: usize = 1500;
const SERIES_AUTO_COLLAPSE_SLICE_COUNT: usize = 200;
const TREE_AUTO_COLLAPSE_SLICE_COUNT: usize = 1000;

impl DicronApp {
    pub(in crate::app) fn show_dicom_tree(&mut self, ui: &mut egui::Ui) {
        let expand_all = self.settings.expand_tree_by_default;
        let tree_generation = self.tree_view_generation;

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

        let selected_indices = self.selected_indices();
        let mut clicked_selection = None;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.push_id(tree_generation, |ui| {
                    for (patient_index, patient) in dicom_index.patients.iter().enumerate() {
                        let patient_slice_count = patient_total_slice_count(patient);

                        egui::CollapsingHeader::new(patient.display_name.as_str())
                            .default_open(
                                expand_all || patient_slice_count < TREE_AUTO_COLLAPSE_SLICE_COUNT,
                            )
                            .show(ui, |ui| {
                                for (study_index, study) in patient.studies.iter().enumerate() {
                                    let study_slice_count = study_total_slice_count(study);

                                    egui::CollapsingHeader::new(study.display_name.as_str())
                                        .default_open(
                                            expand_all
                                                || study_slice_count
                                                    < TREE_AUTO_COLLAPSE_SLICE_COUNT,
                                        )
                                        .show(ui, |ui| {
                                            for (series_index, series) in
                                                study.series_groups.iter().enumerate()
                                            {
                                                let series_label = format!(
                                                    "{} ({} slices)",
                                                    series.display_name,
                                                    series.slices.len()
                                                );

                                                egui::CollapsingHeader::new(series_label)
                                                    .default_open(
                                                        expand_all
                                                            || series.slices.len()
                                                                < SERIES_AUTO_COLLAPSE_SLICE_COUNT,
                                                    )
                                                    .show(ui, |ui| {
                                                        show_series_slices(
                                                            ui,
                                                            &series.slices,
                                                            (
                                                                patient_index,
                                                                study_index,
                                                                series_index,
                                                            ),
                                                            selected_indices,
                                                            &mut clicked_selection,
                                                        );
                                                    });
                                            }
                                        });
                                }
                            });
                    }
                });
            });

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

fn show_series_slices(
    ui: &mut egui::Ui,
    slices: &[SliceItem],
    (patient_index, study_index, series_index): SeriesKey,
    selected_selection: Option<SliceSelection>,
    clicked_selection: &mut Option<SliceSelection>,
) {
    if slices.len() >= SLICE_LIST_VIRTUALIZE_THRESHOLD {
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y;
        let list_height = ui.available_height().max(row_height);

        egui::ScrollArea::vertical()
            .id_salt(("series_slices", patient_index, study_index, series_index))
            .max_height(list_height)
            .auto_shrink([false, false])
            .show_rows(ui, row_height, slices.len(), |ui, row_range| {
                for slice_index in row_range {
                    let current_selection =
                        SliceSelection::new(patient_index, study_index, series_index, slice_index);
                    show_slice_row(
                        ui,
                        &slices[slice_index],
                        current_selection,
                        selected_selection,
                        clicked_selection,
                    );
                }
            });
    } else {
        for (slice_index, slice) in slices.iter().enumerate() {
            let current_selection =
                SliceSelection::new(patient_index, study_index, series_index, slice_index);
            show_slice_row(
                ui,
                slice,
                current_selection,
                selected_selection,
                clicked_selection,
            );
        }
    }
}

fn show_slice_row(
    ui: &mut egui::Ui,
    slice: &SliceItem,
    current_selection: SliceSelection,
    selected_selection: Option<SliceSelection>,
    clicked_selection: &mut Option<SliceSelection>,
) {
    let is_selected = selected_selection == Some(current_selection);

    if ui
        .selectable_label(is_selected, &slice.display_name)
        .clicked()
    {
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
