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
                // Each cell's widgets follow its row's item (see row_cell), wrapped in a
                // scope that follows the screen slot to keep egui's rect/ID check quiet.
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
                                row_cell(ui, slot, row.item, 0, |ui| {
                                    cell(
                                        ui,
                                        widths.tag,
                                        &row.item.tag,
                                        "Copy tag ID",
                                        &row.item.path,
                                    );
                                });
                                row_cell(ui, slot, row.item, 1, |ui| {
                                    description_cell(
                                        ui,
                                        widths.description,
                                        row,
                                        expanded_paths,
                                        searching,
                                    );
                                });
                                row_cell(ui, slot, row.item, 2, |ui| {
                                    cell(
                                        ui,
                                        widths.value,
                                        &row.item.value,
                                        "Copy value",
                                        &row.item.path,
                                    );
                                });
                            }
                            ui.end_row();
                        }
                    });
            });
    });
}

/// Shows one cell under ids that follow its row, so a text selection or an
/// open menu stays with the row it was made on while the table scrolls.
///
/// A row is keyed by its item's path and occurrence, which name one item in the
/// metadata tree whichever rows scroll, expand or filter, and the same element
/// on the next slice. The path alone is not enough: a file can repeat an
/// element, such as a File Meta element inside its data set, and both rows then
/// share a path.
///
/// The cell is wrapped in a scope keyed by its screen slot. egui's debug check
/// flags a rect that changes id between passes, and the slot id that stays at
/// the same rect tells it that rows only scrolled past.
fn row_cell(
    ui: &mut egui::Ui,
    slot: i64,
    item: &MetadataItem,
    column: usize,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let table_id = ui.id();
    let row_key = (item.path.as_str(), item.occurrence);
    ui.scope_builder(
        egui::UiBuilder::new().id(table_id.with(("metadata_cell", slot, column))),
        |ui| {
            ui.scope_builder(
                egui::UiBuilder::new().id(table_id.with(("metadata_row_cell", row_key, column))),
                add_contents,
            );
        },
    );
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

    fn numbered_items(count: usize) -> Vec<MetadataItem> {
        (0..count)
            .map(|index| {
                MetadataItem::new(
                    format!("(7777,{index:04X})"),
                    "SH".to_owned(),
                    format!("Value{index:03}"),
                )
            })
            .collect()
    }

    fn text_rect(output: &egui::FullOutput, wanted: &str) -> egui::Rect {
        texts(output)
            .into_iter()
            .find(|(text, _)| text == wanted)
            .unwrap()
            .1
    }

    /// Scroll the table down by `rows` rows and draw `frames` more frames, as
    /// the scroll animates; returns the output of every frame.
    fn scroll_rows(
        context: &egui::Context,
        items: &[MetadataItem],
        expanded: &mut HashSet<String>,
        rows: f32,
        frames: usize,
    ) -> Vec<egui::FullOutput> {
        let mut outputs = vec![frame(
            context,
            items,
            expanded,
            "",
            vec![
                egui::Event::PointerMoved(egui::pos2(250.0, 120.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    phase: egui::TouchPhase::Move,
                    delta: egui::vec2(0.0, -(ROW_HEIGHT + ROW_GAP) * rows),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        )];
        for _ in 0..frames {
            outputs.push(frame(context, items, expanded, "", Vec::new()));
        }
        outputs
    }

    /// Red outlines egui paints in debug builds where a rect changed id
    /// between passes.
    #[cfg(debug_assertions)]
    fn id_change_warnings(output: &egui::FullOutput) -> usize {
        output
            .shapes
            .iter()
            .filter(|clipped| {
                clipped.clip_rect == egui::Rect::EVERYTHING
                    && matches!(&clipped.shape, egui::epaint::Shape::Rect(rect)
                        if rect.stroke.color == egui::Color32::RED && rect.stroke.width == 2.0)
            })
            .count()
    }

    fn copied_texts(output: &egui::FullOutput) -> Vec<&str> {
        output
            .platform_output
            .commands
            .iter()
            .filter_map(|command| match command {
                egui::OutputCommand::CopyText(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// A selection belongs to the row it was made on: after the table scrolls,
    /// Ctrl+C copies that row's value, not the one now drawn where it was.
    #[test]
    fn copying_a_selected_value_after_scrolling_copies_the_selected_row() {
        let context = egui::Context::default();
        let items = numbered_items(200);
        let mut expanded = HashSet::new();
        frame(&context, &items, &mut expanded, "", Vec::new());
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        let selected = text_rect(&output, "Value006");
        let start = egui::pos2(selected.left() + 1.0, selected.center().y);
        let end = egui::pos2(selected.right() + 4.0, selected.center().y);
        frame(
            &context,
            &items,
            &mut expanded,
            "",
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        frame(
            &context,
            &items,
            &mut expanded,
            "",
            vec![egui::Event::PointerMoved(end)],
        );
        frame(
            &context,
            &items,
            &mut expanded,
            "",
            vec![egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );

        let output = scroll_rows(&context, &items, &mut expanded, 5.0, 16)
            .pop()
            .unwrap();
        assert_eq!(text_rect(&output, "Value011").top(), selected.top());
        let output = frame(&context, &items, &mut expanded, "", vec![egui::Event::Copy]);

        assert_eq!(copied_texts(&output), ["Value006"]);
    }

    /// A cell's menu belongs to the row it was opened on, so Copy value after
    /// a scroll copies that row's value.
    #[test]
    fn copy_value_from_a_cell_menu_after_scrolling_copies_the_menus_row() {
        let context = egui::Context::default();
        let items = numbered_items(200);
        let mut expanded = HashSet::new();
        frame(&context, &items, &mut expanded, "", Vec::new());
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        let opened_on = text_rect(&output, "Value006");
        click(
            &context,
            &items,
            &mut expanded,
            opened_on.center(),
            egui::PointerButton::Secondary,
        );

        let output = scroll_rows(&context, &items, &mut expanded, 5.0, 16)
            .pop()
            .unwrap();
        assert_eq!(text_rect(&output, "Value011").top(), opened_on.top());
        let action = text_rect(&output, "Copy value");
        let output = click(
            &context,
            &items,
            &mut expanded,
            action.center(),
            egui::PointerButton::Primary,
        );

        assert_eq!(copied_texts(&output), ["Value006"]);
    }

    /// Drag across the value cell showing `wanted` with the empty query.
    fn select_value(
        context: &egui::Context,
        items: &[MetadataItem],
        expanded: &mut HashSet<String>,
        wanted: &str,
    ) -> egui::Rect {
        frame(context, items, expanded, "", Vec::new());
        let output = frame(context, items, expanded, "", Vec::new());
        let selected = text_rect(&output, wanted);
        let start = egui::pos2(selected.left() + 1.0, selected.center().y);
        let end = egui::pos2(selected.right() + 4.0, selected.center().y);
        frame(
            context,
            items,
            expanded,
            "",
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
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
            vec![egui::Event::PointerMoved(end)],
        );
        frame(
            context,
            items,
            expanded,
            "",
            vec![egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        selected
    }

    /// Rows also move without a scroll, when a sequence above the selection
    /// opens: the selection stays with its row.
    #[test]
    fn copying_a_selected_value_after_a_sequence_above_opens_copies_the_selected_row() {
        let context = egui::Context::default();
        let mut items = super::super::tree::tests::fixture();
        items.truncate(1);
        items.extend(numbered_items(20));
        let mut expanded = HashSet::new();
        select_value(&context, &items, &mut expanded, "Value003");

        expanded.insert(items[0].path.clone());
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        assert!(texts(&output).iter().any(|(text, _)| text == "Item 2"));
        let output = frame(&context, &items, &mut expanded, "", vec![egui::Event::Copy]);

        assert_eq!(copied_texts(&output), ["Value003"]);
    }

    /// Typing a search query moves the matching rows up: the selection made
    /// before stays with its row.
    #[test]
    fn copying_a_selected_value_after_the_search_moves_it_copies_the_selected_row() {
        let context = egui::Context::default();
        let items = numbered_items(20);
        let mut expanded = HashSet::new();
        let selected_top = select_value(&context, &items, &mut expanded, "Value006").top();

        let output = frame(&context, &items, &mut expanded, "value006", Vec::new());
        assert!(!texts(&output).iter().any(|(text, _)| text == "Value000"));
        assert!(text_rect(&output, "Value006").top() < selected_top);
        let output = frame(
            &context,
            &items,
            &mut expanded,
            "value006",
            vec![egui::Event::Copy],
        );

        assert_eq!(copied_texts(&output), ["Value006"]);
    }

    /// A file can repeat a File Meta element inside its dataset, so the full
    /// list holds two rows with the same tag and path. Each row keeps its own
    /// selection: selecting the second copies the second's value.
    fn items_repeating_a_path() -> Vec<MetadataItem> {
        let mut items = numbered_items(4);
        for value in ["MetaCopy", "DatasetCopy"] {
            items.push(MetadataItem::new(
                "(0002,0013)".to_owned(),
                "ImplementationVersionName".to_owned(),
                value.to_owned(),
            ));
        }
        MetadataItem::number_repeated_paths(&mut items);
        items
    }

    #[test]
    fn rows_that_share_a_path_keep_separate_selections() {
        let context = egui::Context::default();
        let items = items_repeating_a_path();
        let mut expanded = HashSet::new();
        select_value(&context, &items, &mut expanded, "DatasetCopy");

        let output = frame(&context, &items, &mut expanded, "", vec![egui::Event::Copy]);

        assert_eq!(copied_texts(&output), ["DatasetCopy"]);
    }

    /// A search that hides one of two rows sharing a path leaves the other
    /// row's selection its own: the hidden row's selection does not pass to
    /// the copy still shown, and the shown copy keeps its selection.
    #[test]
    fn rows_that_share_a_path_keep_separate_selections_while_a_search_hides_one() {
        for (selected, query, copied) in [
            ("MetaCopy", "datasetcopy", None),
            ("DatasetCopy", "datasetcopy", Some("DatasetCopy")),
            ("MetaCopy", "metacopy", Some("MetaCopy")),
            ("DatasetCopy", "metacopy", None),
        ] {
            let context = egui::Context::default();
            let items = items_repeating_a_path();
            let mut expanded = HashSet::new();
            select_value(&context, &items, &mut expanded, selected);

            let output = frame(&context, &items, &mut expanded, query, Vec::new());
            assert_eq!(
                texts(&output)
                    .iter()
                    .filter(|(text, _)| text.ends_with("Copy"))
                    .count(),
                1
            );
            let output = frame(
                &context,
                &items,
                &mut expanded,
                query,
                vec![egui::Event::Copy],
            );

            assert_eq!(
                copied_texts(&output),
                copied.as_slice(),
                "{selected} selected, searching {query}"
            );
        }
    }

    /// When the metadata is replaced, as the viewer steps to the next slice,
    /// the new rows can land in memory the old rows used. A selection stays
    /// with its element: here the next slice lacks the first element, and the
    /// rows after it shift down into the same storage.
    #[test]
    fn a_selection_stays_with_its_element_when_another_row_takes_its_memory() {
        let context = egui::Context::default();
        let mut items = numbered_items(20);
        let mut expanded = HashSet::new();
        select_value(&context, &items, &mut expanded, "Value006");

        let storage = items.as_ptr();
        items.remove(0);
        assert_eq!(items.as_ptr(), storage);
        let output = frame(&context, &items, &mut expanded, "", Vec::new());
        assert!(!texts(&output).iter().any(|(text, _)| text == "Value000"));
        let output = frame(&context, &items, &mut expanded, "", vec![egui::Event::Copy]);

        assert_eq!(copied_texts(&output), ["Value006"]);
    }

    /// Cells take their ids from their rows, yet egui sees no rect change its
    /// id while wheel steps of whole and half rows land mid-animation.
    #[cfg(debug_assertions)]
    #[test]
    fn scrolling_rows_raises_no_id_warnings() {
        let context = egui::Context::default();
        let items = numbered_items(500);
        let mut expanded = HashSet::new();
        frame(&context, &items, &mut expanded, "", Vec::new());
        let mut outputs = Vec::new();
        for _ in 0..10 {
            for rows in [1.0, 5.0, 0.5, 3.0] {
                outputs.extend(scroll_rows(&context, &items, &mut expanded, rows, 5));
            }
        }

        assert!(
            !texts(outputs.last().unwrap())
                .iter()
                .any(|(text, _)| text == "Value000")
        );
        for output in &outputs {
            assert_eq!(id_change_warnings(output), 0);
        }
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
            assert_eq!(id_change_warnings(&output), 0);
        }
    }
}
