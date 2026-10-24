//! A virtualized table with disclosure controls inside the description column.

use std::collections::HashSet;

use eframe::egui;

use super::tree::VisibleRow;
use crate::dicom::{MetadataItem, MetadataItemKind};
use crate::theme;

const TAG_COLUMN_PREFERRED_WIDTH: f32 = 92.0;
const DESCRIPTION_COLUMN_MAX_WIDTH: f32 = 190.0;
const DESCRIPTION_COLUMN_SHARE: f32 = 0.60;
const ROW_HEIGHT: f32 = 18.0;
const COLUMN_GAP: f32 = theme::SPACE_SM;
const ROW_GAP: f32 = theme::SPACE_XS;
const INDENT_WIDTH: f32 = 12.0;
const TOGGLE_WIDTH: f32 = 14.0;

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

pub(super) fn show(
    ui: &mut egui::Ui,
    rows: &[VisibleRow<'_>],
    expanded_paths: &mut HashSet<String>,
    searching: bool,
) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = ROW_GAP;
        egui::ScrollArea::vertical()
            .id_salt("dicom_metadata_table_scroll_area")
            .scroll_source(
                egui::scroll_area::ScrollSource::SCROLL_BAR
                    | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
            )
            .auto_shrink([false, false])
            .show_rows(ui, ROW_HEIGHT, rows.len() + 1, |ui, range| {
                let widths = ColumnWidths::for_available_width(ui.available_width());
                let stride = ROW_HEIGHT + ROW_GAP;
                // Widget identities follow screen slots while expansion follows source paths.
                // This also avoids egui's rect/ID warnings when whole rows scroll past a slot.
                let anchor = range.start as i64
                    + ((ui.max_rect().top() - ui.clip_rect().top()) / stride).floor() as i64;
                egui::Grid::new("dicom_metadata_table_grid")
                    .num_columns(3)
                    .striped(true)
                    .spacing([COLUMN_GAP, ROW_GAP])
                    .min_col_width(0.0)
                    .min_row_height(ROW_HEIGHT)
                    .start_row(range.start)
                    .show(ui, |ui| {
                        for index in range {
                            if index == 0 {
                                header_cell(ui, widths.tag, "Tag ID");
                                header_cell(ui, widths.description, "Description");
                                header_cell(ui, widths.value, "Value");
                            } else {
                                let row = &rows[index - 1];
                                let slot = index as i64 - anchor;
                                ui.scope_builder(
                                    egui::UiBuilder::new().id(ui.id().with((
                                        "metadata_cell",
                                        slot,
                                        0,
                                    ))),
                                    |ui| {
                                        cell(
                                            ui,
                                            widths.tag,
                                            &row.item.tag,
                                            "Copy tag ID",
                                            &row.item.path,
                                        );
                                    },
                                );
                                ui.scope_builder(
                                    egui::UiBuilder::new().id(ui.id().with((
                                        "metadata_cell",
                                        slot,
                                        1,
                                    ))),
                                    |ui| {
                                        description_cell(
                                            ui,
                                            widths.description,
                                            row,
                                            expanded_paths,
                                            searching,
                                        );
                                    },
                                );
                                ui.scope_builder(
                                    egui::UiBuilder::new().id(ui.id().with((
                                        "metadata_cell",
                                        slot,
                                        2,
                                    ))),
                                    |ui| {
                                        cell(
                                            ui,
                                            widths.value,
                                            &row.item.value,
                                            "Copy value",
                                            &row.item.path,
                                        );
                                    },
                                );
                            }
                            ui.end_row();
                        }
                    });
            });
    });
}

fn header_cell(ui: &mut egui::Ui, width: f32, text: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ROW_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Min),
        |ui| {
            ui.set_width(width);
            ui.add(egui::Label::new(egui::RichText::new(text).strong()).truncate());
        },
    );
}

fn description_cell(
    ui: &mut egui::Ui,
    width: f32,
    row: &VisibleRow<'_>,
    expanded_paths: &mut HashSet<String>,
    searching: bool,
) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ROW_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Min),
        |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.x = 0.0;
            let indent =
                (row.depth as f32 * INDENT_WIDTH).min((width - TOGGLE_WIDTH - 24.0).max(0.0));
            ui.add_space(indent);
            if row.item.kind != MetadataItemKind::Element && !row.item.children.is_empty() {
                let sense = if searching {
                    egui::Sense::hover()
                } else {
                    egui::Sense::click()
                };
                let (_, response) =
                    ui.allocate_exact_size(egui::vec2(TOGGLE_WIDTH, ROW_HEIGHT), sense);
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::CollapsingHeader,
                        ui.is_enabled(),
                        row.expanded,
                        &row.item.description,
                    )
                });
                let icon_response = response.clone().with_new_rect(egui::Rect::from_center_size(
                    response.rect.center(),
                    egui::Vec2::splat(10.0),
                ));
                egui::collapsing_header::paint_default_icon(
                    ui,
                    if row.expanded { 1.0 } else { 0.0 },
                    &icon_response,
                );
                if response.clicked() {
                    if row.expanded {
                        expanded_paths.remove(&row.item.path);
                    } else {
                        expanded_paths.insert(row.item.path.clone());
                    }
                    ui.ctx().request_repaint();
                }
                response.on_hover_text(if searching {
                    "Expanded for search"
                } else if row.expanded {
                    "Collapse"
                } else {
                    "Expand"
                });
            } else {
                ui.add_space(TOGGLE_WIDTH);
            }
            description_label(ui, row.item);
        },
    );
}

fn description_label(ui: &mut egui::Ui, item: &MetadataItem) {
    let response = ui.add(
        egui::Label::new(&item.description)
            .selectable(true)
            .truncate(),
    );
    context_menu(&response, &item.description, "Copy description", &item.path);
    response.on_hover_text(format!("{}\n{}", item.description, item.path));
}

fn cell(ui: &mut egui::Ui, width: f32, text: &str, copy_action: &str, path: &str) {
    let response = ui
        .allocate_ui_with_layout(
            egui::vec2(width, ROW_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center)
                .with_main_align(egui::Align::Min)
                .with_main_justify(true)
                .with_cross_justify(true),
            |ui| {
                ui.set_width(width);
                ui.add(egui::Label::new(text).selectable(true).truncate())
            },
        )
        .inner;
    context_menu(&response, text, copy_action, path);
}

fn context_menu(response: &egui::Response, text: &str, copy_action: &str, path: &str) {
    response.context_menu(|ui| {
        if ui.button(copy_action).clicked() {
            ui.ctx().copy_text(text.to_owned());
            ui.close();
        }
        if ui.button("Copy full path").clicked() {
            ui.ctx().copy_text(path.to_owned());
            ui.close();
        }
    });
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

    fn frame(
        context: &egui::Context,
        items: &[MetadataItem],
        expanded: &mut HashSet<String>,
        query: &str,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(500.0, 240.0),
            )),
            events,
            ..Default::default()
        };
        context.run_ui(input, |ui| {
            let rows = super::super::tree::visible_rows(items, expanded, query);
            show(ui, &rows, expanded, !query.is_empty());
        })
    }

    fn texts(output: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        fn walk(shape: &egui::epaint::Shape, result: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::epaint::Shape::Text(text) => result.push((
                    text.galley.text().to_owned(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                )),
                egui::epaint::Shape::Vec(shapes) => {
                    shapes.iter().for_each(|shape| walk(shape, result))
                }
                _ => {}
            }
        }
        let mut result = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut result);
        }
        result
    }

    fn click(
        context: &egui::Context,
        items: &[MetadataItem],
        expanded: &mut HashSet<String>,
        point: egui::Pos2,
        button: egui::PointerButton,
    ) -> egui::FullOutput {
        frame(
            context,
            items,
            expanded,
            "",
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        frame(
            context,
            items,
            expanded,
            "",
            vec![egui::Event::PointerButton {
                pos: point,
                button,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        )
    }

    #[test]
    fn clicking_disclosures_reveals_sequence_items_and_their_values() {
        let context = egui::Context::default();
        let items = super::super::tree::tests::fixture();
        let mut expanded = HashSet::new();
        frame(&context, &items, &mut expanded, "", Vec::new());
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        assert!(!texts(&output).iter().any(|(text, _)| text == "alpha"));
        let root = texts(&output)
            .into_iter()
            .find(|(text, _)| text == "SequenceRoot")
            .unwrap()
            .1;
        click(
            &context,
            &items,
            &mut expanded,
            egui::pos2(root.left() - TOGGLE_WIDTH / 2.0, root.center().y),
            egui::PointerButton::Primary,
        );
        assert!(expanded.contains(&items[0].path));
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        let item = texts(&output)
            .into_iter()
            .find(|(text, _)| text == "Item 1")
            .unwrap()
            .1;
        click(
            &context,
            &items,
            &mut expanded,
            egui::pos2(item.left() - TOGGLE_WIDTH / 2.0, item.center().y),
            egui::PointerButton::Primary,
        );
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        assert!(texts(&output).iter().any(|(text, _)| text == "alpha"));
        assert!(!texts(&output).iter().any(|(text, _)| text == "deepneedle"));
    }

    #[test]
    fn nested_value_context_menu_copies_the_full_item_path() {
        let context = egui::Context::default();
        let items = super::super::tree::tests::fixture();
        let mut expanded = HashSet::new();
        super::super::tree::expand_all(&items, &mut expanded);
        frame(&context, &items, &mut expanded, "", Vec::new());
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        let value = texts(&output)
            .into_iter()
            .find(|(text, _)| text == "deepneedle")
            .unwrap()
            .1;
        click(
            &context,
            &items,
            &mut expanded,
            value.center(),
            egui::PointerButton::Secondary,
        );
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        let action = texts(&output)
            .into_iter()
            .find(|(text, _)| text == "Copy full path")
            .unwrap()
            .1;
        let output = click(
            &context,
            &items,
            &mut expanded,
            action.center(),
            egui::PointerButton::Primary,
        );
        assert!(
            output
                .platform_output
                .commands
                .iter()
                .any(|command| matches!(command,
            egui::OutputCommand::CopyText(path) if path == "(0008,1032) / Item 2 / (0008,0100)"))
        );
    }

    #[test]
    fn large_lists_keep_fixed_columns_and_stable_ids_while_scrolling() {
        let context = egui::Context::default();
        let items: Vec<_> = (0..10_000)
            .map(|index| {
                MetadataItem::new(
                    format!("(7777,{index:04X})"),
                    if index < 20 {
                        "Long DICOM element description".to_owned()
                    } else {
                        "SH".to_owned()
                    },
                    format!("Value {index}"),
                )
            })
            .collect();
        let mut expanded = HashSet::new();
        let geometry = |output: &egui::FullOutput| {
            let value_x = texts(output)
                .iter()
                .find(|(text, _)| text.starts_with("Value "))
                .unwrap()
                .1
                .left();
            let stripe_width = output
                .shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::epaint::Shape::Rect(rect)
                        if rect.fill == context.global_style().visuals.faint_bg_color =>
                    {
                        Some(rect.rect.width())
                    }
                    _ => None,
                })
                .unwrap();
            (value_x, stripe_width)
        };
        frame(&context, &items, &mut expanded, "", Vec::new());
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        let initial_geometry = geometry(&output);
        assert!(texts(&output).len() < 80);
        let output = frame(
            &context,
            &items,
            &mut expanded,
            "",
            vec![
                egui::Event::PointerMoved(egui::pos2(250.0, 120.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    phase: egui::TouchPhase::Move,
                    delta: egui::vec2(0.0, -(ROW_HEIGHT + ROW_GAP) * 30.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let mut outputs = vec![output];
        for _ in 0..15 {
            outputs.push(frame(&context, &items, &mut expanded, "", Vec::new()));
        }
        assert!(
            texts(outputs.last().unwrap())
                .iter()
                .any(|(text, _)| text == "SH")
        );
        for output in outputs {
            assert_eq!(geometry(&output), initial_geometry);
            assert!(texts(&output).len() < 80);
            #[cfg(debug_assertions)]
            assert!(!output.shapes.iter().any(|clipped| clipped.clip_rect == egui::Rect::EVERYTHING && matches!(&clipped.shape,
                egui::epaint::Shape::Rect(rect) if rect.stroke.color == egui::Color32::RED && rect.stroke.width == 2.0)));
        }
    }
}
