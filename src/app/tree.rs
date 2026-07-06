use super::*;

impl DicronApp {
    pub(super) fn show_dicom_tree(&mut self, ui: &mut egui::Ui) {
        let expand_all = self.dialog_directories.expand_tree_by_default;
        let tree_generation = self.tree_view_generation;

        let Some(dicom_index) = &self.dicom_index else {
            if self.scan_state.is_some() {
                ui.label("Scanning folder...");
            } else {
                ui.label("Open a DICOM file or folder to build Patient / Study / Series tree.");
            }

            return;
        };

        ui.label(format!("{} DICOM files", dicom_index.total_file_count));
        ui.separator();

        let selected_indices = self.selected_indices();
        let mut clicked_indices: Option<SliceKey> = None;

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
                                                            &mut clicked_indices,
                                                        );
                                                    });
                                            }
                                        });
                                }
                            });
                    }
                });
            });

        if let Some((patient_index, study_index, series_index, slice_index)) = clicked_indices {
            self.load_slice_by_indices(
                ui.ctx(),
                patient_index,
                study_index,
                series_index,
                slice_index,
            );
        }
    }

    pub(super) fn load_first_available_slice(&mut self, context: &egui::Context) {
        let first_available_indices = {
            let Some(dicom_index) = &self.dicom_index else {
                return;
            };

            let mut found_indices: Option<(usize, usize, usize, usize)> = None;

            'search: for (patient_index, patient) in dicom_index.patients.iter().enumerate() {
                for (study_index, study) in patient.studies.iter().enumerate() {
                    for (series_index, series) in study.series_groups.iter().enumerate() {
                        if !series.slices.is_empty() {
                            found_indices = Some((patient_index, study_index, series_index, 0));
                            break 'search;
                        }
                    }
                }
            }

            found_indices
        };

        if let Some((patient_index, study_index, series_index, slice_index)) =
            first_available_indices
        {
            self.load_slice_by_indices(
                context,
                patient_index,
                study_index,
                series_index,
                slice_index,
            );
        }
    }

    pub(super) fn load_slice_by_indices(
        &mut self,
        context: &egui::Context,
        patient_index: usize,
        study_index: usize,
        series_index: usize,
        slice_index: usize,
    ) {
        let Some(slice_item) =
            self.get_slice_by_indices(patient_index, study_index, series_index, slice_index)
        else {
            return;
        };

        self.selected_patient_index = Some(patient_index);
        self.selected_study_index = Some(study_index);
        self.selected_series_index = Some(series_index);
        self.selected_slice_index = Some(slice_index);

        self.load_dicom_path(context, slice_item.path, slice_item.frame_index);
    }

    pub(super) fn get_slice_by_indices(
        &self,
        patient_index: usize,
        study_index: usize,
        series_index: usize,
        slice_index: usize,
    ) -> Option<SliceItem> {
        let dicom_index = self.dicom_index.as_ref()?;
        let patient = dicom_index.patients.get(patient_index)?;
        let study = patient.studies.get(study_index)?;
        let series = study.series_groups.get(series_index)?;
        let slice = series.slices.get(slice_index)?;

        Some(slice.clone())
    }

    pub(super) fn get_selected_series_slice_count(&self) -> Option<usize> {
        let dicom_index = self.dicom_index.as_ref()?;
        let patient = dicom_index.patients.get(self.selected_patient_index?)?;
        let study = patient.studies.get(self.selected_study_index?)?;
        let series = study.series_groups.get(self.selected_series_index?)?;

        Some(series.slices.len())
    }

    pub(super) fn current_slice_index(&self) -> Option<usize> {
        if let Some(selected_slice_index) = self.selected_slice_index {
            Some(selected_slice_index)
        } else if self.selected_dicom_path.is_some() && self.selected_dicom_frame_count > 1 {
            Some(self.selected_dicom_frame_index as usize)
        } else {
            None
        }
    }

    pub(super) fn current_slice_count(&self) -> Option<usize> {
        self.get_selected_series_slice_count().or_else(|| {
            if self.selected_dicom_path.is_some() && self.selected_dicom_frame_count > 1 {
                Some(self.selected_dicom_frame_count as usize)
            } else {
                None
            }
        })
    }

    pub(super) fn selected_indices(&self) -> Option<SliceKey> {
        Some((
            self.selected_patient_index?,
            self.selected_study_index?,
            self.selected_series_index?,
            self.selected_slice_index?,
        ))
    }

    pub(super) fn clear_selected_indices(&mut self) {
        self.stop_autoplay();
        self.selected_patient_index = None;
        self.selected_study_index = None;
        self.selected_series_index = None;
        self.selected_slice_index = None;
        self.selected_dicom_frame_index = 0;
        self.selected_dicom_frame_count = 1;
        self.viewer_scroll_accumulator = 0.0;
    }
}

fn show_series_slices(
    ui: &mut egui::Ui,
    slices: &[SliceItem],
    (patient_index, study_index, series_index): SeriesKey,
    selected_indices: Option<SliceKey>,
    clicked_indices: &mut Option<SliceKey>,
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
                    let current_indices = (patient_index, study_index, series_index, slice_index);
                    show_slice_row(
                        ui,
                        &slices[slice_index],
                        current_indices,
                        selected_indices,
                        clicked_indices,
                    );
                }
            });
    } else {
        for (slice_index, slice) in slices.iter().enumerate() {
            let current_indices = (patient_index, study_index, series_index, slice_index);
            show_slice_row(
                ui,
                slice,
                current_indices,
                selected_indices,
                clicked_indices,
            );
        }
    }
}

fn show_slice_row(
    ui: &mut egui::Ui,
    slice: &SliceItem,
    current_indices: SliceKey,
    selected_indices: Option<SliceKey>,
    clicked_indices: &mut Option<SliceKey>,
) {
    let is_selected = selected_indices == Some(current_indices);

    if ui
        .selectable_label(is_selected, &slice.display_name)
        .clicked()
    {
        *clicked_indices = Some(current_indices);
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
