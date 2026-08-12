//! Patient/Study/Series/Instance selection tree.

use std::hash::Hash;

use eframe::egui;

use crate::app::DicronApp;
use crate::app::state::{SeriesKey, SliceSelection};
use crate::dicom::{PatientGroup, SliceItem, StudyGroup};
use crate::theme;

// Large subtrees start collapsed and large series use row virtualization so
// malformed or unusually large studies do not make every frame expensive.
const SLICE_LIST_VIRTUALIZE_THRESHOLD: usize = 1500;
const SERIES_AUTO_COLLAPSE_SLICE_COUNT: usize = 200;
const TREE_AUTO_COLLAPSE_SLICE_COUNT: usize = 1000;
const TREE_ROW_GAP: f32 = theme::SPACE_XXS;
const PATIENT_ROW_HEIGHT: f32 = 26.0;
const STUDY_ROW_HEIGHT: f32 = 24.0;
const SERIES_ROW_HEIGHT: f32 = 24.0;
const INSTANCE_ROW_HEIGHT: f32 = 20.0;

#[derive(Clone, Copy)]
enum TreeNodeLevel {
    Patient,
    Study,
    Series,
}

impl TreeNodeLevel {
    const fn row_height(self) -> f32 {
        match self {
            Self::Patient => PATIENT_ROW_HEIGHT,
            Self::Study => STUDY_ROW_HEIGHT,
            Self::Series => SERIES_ROW_HEIGHT,
        }
    }
}

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

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = TREE_ROW_GAP;

                ui.push_id(tree_generation, |ui| {
                    for (patient_index, patient) in dicom_index.patients.iter().enumerate() {
                        let patient_slice_count = patient_total_slice_count(patient);

                        show_tree_node(
                            ui,
                            ("patient", patient_index),
                            patient.display_name.as_str(),
                            expand_all || patient_slice_count < TREE_AUTO_COLLAPSE_SLICE_COUNT,
                            TreeNodeLevel::Patient,
                            |ui| {
                                for (study_index, study) in patient.studies.iter().enumerate() {
                                    let study_slice_count = study_total_slice_count(study);

                                    show_tree_node(
                                        ui,
                                        ("study", patient_index, study_index),
                                        study.display_name.as_str(),
                                        expand_all
                                            || study_slice_count < TREE_AUTO_COLLAPSE_SLICE_COUNT,
                                        TreeNodeLevel::Study,
                                        |ui| {
                                            for (series_index, series) in
                                                study.series_groups.iter().enumerate()
                                            {
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
                                                    |ui| {
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
                                                    },
                                                );
                                            }
                                        },
                                    );
                                }
                            },
                        );
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

fn show_tree_node<R>(
    ui: &mut egui::Ui,
    id_salt: impl Hash,
    label: &str,
    default_open: bool,
    level: TreeNodeLevel,
    add_body: impl FnOnce(&mut egui::Ui) -> R,
) {
    let id = ui.make_persistent_id(id_salt);
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        default_open,
    );
    let openness = state.openness(ui.ctx());
    let header_response = show_tree_node_row(ui, label, level, openness);

    if header_response.clicked() {
        state.toggle(ui);
    }

    state.show_body_indented(&header_response, ui, add_body);
}

fn show_tree_node_row(
    ui: &mut egui::Ui,
    label: &str,
    level: TreeNodeLevel,
    openness: f32,
) -> egui::Response {
    let row_size = egui::vec2(ui.available_width(), level.row_height());
    let (row_rect, mut response) = ui.allocate_exact_size(row_size, egui::Sense::click());

    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::CollapsingHeader, ui.is_enabled(), label)
    });

    if ui.is_rect_visible(row_rect) {
        let icon_center = egui::pos2(
            row_rect.left() + ui.spacing().indent / 2.0,
            row_rect.center().y,
        );
        let icon_rect = egui::Rect::from_center_size(
            icon_center,
            egui::Vec2::splat(ui.spacing().icon_width_inner),
        );
        let icon_response = response.clone().with_new_rect(icon_rect);
        egui::collapsing_header::paint_default_icon(ui, openness, &icon_response);

        let text_color = tree_node_text_color(ui.visuals(), level);
        let rich_text = match level {
            TreeNodeLevel::Patient => egui::RichText::new(label).strong().size(14.0),
            TreeNodeLevel::Study => egui::RichText::new(label).color(text_color),
            TreeNodeLevel::Series => egui::RichText::new(label).color(text_color),
        };
        let text_left = row_rect.left() + ui.spacing().indent;
        let text_was_elided = paint_truncated_text(
            ui,
            row_rect,
            text_left,
            rich_text,
            egui::TextStyle::Button,
            text_color,
        );

        if text_was_elided {
            response = response.on_hover_text(label);
        }
    }

    response
}

fn tree_node_text_color(visuals: &egui::Visuals, level: TreeNodeLevel) -> egui::Color32 {
    match level {
        TreeNodeLevel::Patient => visuals.strong_text_color(),
        TreeNodeLevel::Study => visuals.text_color(),
        TreeNodeLevel::Series => visuals.text_color().gamma_multiply(0.90),
    }
}

fn paint_truncated_text(
    ui: &egui::Ui,
    row_rect: egui::Rect,
    text_left: f32,
    text: egui::RichText,
    text_style: egui::TextStyle,
    fallback_color: egui::Color32,
) -> bool {
    let text_width = (row_rect.right() - text_left - theme::SPACE_XS).max(0.0);
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        text_width,
        text_style,
    );
    let text_position = egui::pos2(text_left, row_rect.center().y - galley.size().y / 2.0);
    let text_was_elided = galley.elided;

    ui.painter().galley(text_position, galley, fallback_color);

    text_was_elided
}

fn show_series_slices(
    ui: &mut egui::Ui,
    slices: &[SliceItem],
    (patient_index, study_index, series_index): SeriesKey,
    selected_selection: Option<SliceSelection>,
    clicked_selection: &mut Option<SliceSelection>,
) {
    if slices.len() >= SLICE_LIST_VIRTUALIZE_THRESHOLD {
        let list_height = ui.available_height().max(INSTANCE_ROW_HEIGHT);

        egui::ScrollArea::vertical()
            .id_salt(("series_slices", patient_index, study_index, series_index))
            .max_height(list_height)
            .auto_shrink([false, false])
            .show_rows(ui, INSTANCE_ROW_HEIGHT, slices.len(), |ui, row_range| {
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
    let row_size = egui::vec2(ui.available_width(), INSTANCE_ROW_HEIGHT);
    let (row_rect, mut response) = ui.allocate_exact_size(row_size, egui::Sense::click());

    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            is_selected,
            &slice.display_name,
        )
    });

    if ui.is_rect_visible(row_rect) {
        let visuals = ui.visuals();
        let row_fill = if is_selected {
            visuals
                .selection
                .bg_fill
                .lerp_to_gamma(visuals.panel_fill, 0.35)
        } else if response.hovered() {
            visuals.widgets.hovered.weak_bg_fill
        } else {
            egui::Color32::TRANSPARENT
        };

        if row_fill != egui::Color32::TRANSPARENT {
            ui.painter()
                .rect_filled(row_rect, theme::SPACE_XXS, row_fill);
        }

        let text_color = if is_selected || response.hovered() {
            visuals.text_color()
        } else {
            visuals.weak_text_color()
        };
        let text_left = row_rect.left() + theme::SPACE_XS;
        let text_was_elided = paint_truncated_text(
            ui,
            row_rect,
            text_left,
            egui::RichText::new(&slice.display_name).color(text_color),
            egui::TextStyle::Body,
            text_color,
        );

        if text_was_elided {
            response = response.on_hover_text(&slice.display_name);
        }
    }

    if response.clicked() {
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
