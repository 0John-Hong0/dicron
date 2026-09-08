//! Shared helpers for deterministic DICOM-tree UI tests.

use eframe::egui;

use crate::dicom::SliceItem;

pub(super) fn test_slices(count: usize) -> Vec<SliceItem> {
    (0..count)
        .map(|index| SliceItem {
            path: std::path::PathBuf::from(format!("slice_{index}.dcm")),
            display_name: format!("#{index}"),
            frame_index: 0,
            instance_number: None,
            sort_position: None,
        })
        .collect()
}

pub(super) fn run_frame(
    context: &egui::Context,
    add_contents: impl FnOnce(&mut egui::Ui),
) -> egui::FullOutput {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(300.0, 400.0),
        )),
        ..Default::default()
    };
    let mut add_contents = Some(add_contents);

    context.run_ui(input, |ui| {
        if let Some(add_contents) = add_contents.take() {
            add_contents(ui);
        }
    })
}

/// Counts the 2px red outlines egui's debug check `warn_if_rect_changes_id` paints when a
/// rect is claimed by a different widget id than in the previous pass.
#[cfg(debug_assertions)]
pub(super) fn id_change_warning_count(shapes: &[egui::epaint::ClippedShape]) -> usize {
    shapes
        .iter()
        .filter(|clipped| clipped.clip_rect == egui::Rect::EVERYTHING)
        .filter(|clipped| {
            matches!(
                &clipped.shape,
                egui::Shape::Rect(rect_shape)
                    if rect_shape.stroke.color == egui::Color32::RED
                        && rect_shape.stroke.width == 2.0
            )
        })
        .count()
}

/// Every text drawn this frame, with the rect it occupies, outermost shapes first.
pub(super) fn painted_texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, egui::Rect)> {
    fn collect(shape: &egui::Shape, texts: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::Shape::Text(text) => {
                texts.push((
                    text.galley.text().to_owned(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                ));
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, texts);
                }
            }
            _ => {}
        }
    }

    let mut texts = Vec::new();
    for clipped in shapes {
        collect(&clipped.shape, &mut texts);
    }
    texts
}

#[cfg(debug_assertions)]
#[test]
fn id_change_warning_detector_sees_egui_warning() {
    // Control: an id that changes at an unchanged rect must be flagged by egui.
    let context = egui::Context::default();
    let rect = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(100.0, 20.0));

    run_frame(&context, |ui| {
        ui.interact(rect, egui::Id::new("first"), egui::Sense::click());
    });
    let output = run_frame(&context, |ui| {
        ui.interact(rect, egui::Id::new("second"), egui::Sense::click());
    });

    assert_eq!(id_change_warning_count(&output.shapes), 1);
}
