//! Image display, fitting, zooming, panning, and clipping.

use eframe::egui;

use super::{controls, image_texture::fit_image_to_available_space};
use crate::app::DicronApp;
use crate::app::state::ViewportTransform;
use crate::app::ui::overlay::{
    OVERLAY_FONT_SIZE, OVERLAY_MARGIN, identification_overlay_text, join_overlay_lines,
    paint_overlay_text, paint_transformed_image, transformed_image_rect, transformed_image_uv,
    zoom_viewport_around_pointer,
};
use crate::dicom::dominant_patient_direction;
use crate::dicom::{DicomOverlayMetadata, PixelProbeValue};
use crate::theme;

const OVERLAY_BLOCK_GAP: f32 = theme::SPACE_LG;

pub(super) fn show(app: &mut DicronApp, ui: &mut egui::Ui) {
    let raw_available_size = ui.available_size();

    let safe_available_size =
        egui::vec2(raw_available_size.x.max(1.0), raw_available_size.y.max(1.0));

    let (panel_rect, _panel_response) =
        ui.allocate_exact_size(safe_available_size, egui::Sense::hover());

    let has_series_scrollbar = app
        .current_slice_count()
        .is_some_and(|slice_count| slice_count > 1)
        && panel_rect.width() > 48.0
        && panel_rect.height() > 48.0;

    let scrollbar_width = if has_series_scrollbar {
        theme::SPACE_LG
    } else {
        0.0
    };

    let viewer_width = (panel_rect.width() - scrollbar_width).max(1.0);
    let viewer_height = panel_rect.height().max(1.0);

    let viewer_rect =
        egui::Rect::from_min_size(panel_rect.min, egui::vec2(viewer_width, viewer_height));

    let scrollbar_rect = egui::Rect::from_min_size(
        egui::pos2(viewer_rect.right(), panel_rect.top()),
        egui::vec2(scrollbar_width.max(1.0), viewer_height),
    );

    // A menu or dialog over the viewer keeps the wheel for itself.
    let is_pointer_over_viewer = ui.rect_contains_pointer(viewer_rect);
    let viewer_response = ui.interact(
        viewer_rect,
        ui.id().with("window_level_drag_area"),
        egui::Sense::click_and_drag(),
    );

    app.handle_viewer_scroll(ui.ctx(), is_pointer_over_viewer);

    if app.loaded_image.is_some() && viewer_response.dragged_by(egui::PointerButton::Primary) {
        app.handle_window_level_drag(ui.ctx(), &viewer_response);
    }

    if app.loaded_image.is_some() {
        handle_viewport_transform(
            &mut app.viewport_transform,
            &mut app.viewport_zoom_anchor,
            viewer_rect,
            &viewer_response,
        );

        if viewer_response.dragged_by(egui::PointerButton::Middle) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if viewer_response.dragged_by(egui::PointerButton::Secondary) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
    }

    if has_series_scrollbar
        && let (Some(selected_slice_index), Some(slice_count)) =
            (app.current_slice_index(), app.current_slice_count())
        && let Some(requested_slice_index) =
            controls::show_slice_scrollbar(ui, scrollbar_rect, selected_slice_index, slice_count)
    {
        app.jump_to_slice(ui.ctx(), requested_slice_index);
    }

    if let Some(loaded_image) = &app.loaded_image {
        let pixel_aspect_ratio = app
            .metadata
            .overlay
            .as_ref()
            .and_then(|metadata| metadata.pixel_aspect_ratio);
        let fitted_image_size = fit_image_to_available_space(
            loaded_image.source_size,
            pixel_aspect_ratio,
            viewer_rect.size(),
        );

        if fitted_image_size.x > 0.0 && fitted_image_size.y > 0.0 {
            let image_rect =
                transformed_image_rect(viewer_rect, fitted_image_size, app.viewport_transform);
            paint_transformed_image(
                &ui.painter().with_clip_rect(viewer_rect),
                loaded_image.texture.id(),
                image_rect,
                app.viewport_transform,
            );

            show_viewer_overlays(app, ui, viewer_rect, image_rect, loaded_image.source_size);
        }
    } else {
        ui.painter().text(
            viewer_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Open a DICOM file or folder.",
            egui::FontId::proportional(16.0),
            ui.visuals().text_color(),
        );
    }
}

fn handle_viewport_transform(
    transform: &mut ViewportTransform,
    zoom_anchor: &mut Option<egui::Pos2>,
    viewer_rect: egui::Rect,
    response: &egui::Response,
) {
    if response.double_clicked_by(egui::PointerButton::Primary) {
        *transform = ViewportTransform::default();
        *zoom_anchor = None;
        return;
    }

    if response.drag_started_by(egui::PointerButton::Secondary) {
        *zoom_anchor = response.interact_pointer_pos();
    }

    if response.dragged_by(egui::PointerButton::Secondary) {
        let anchor = zoom_anchor
            .or_else(|| response.interact_pointer_pos())
            .unwrap_or_else(|| viewer_rect.center());
        zoom_viewport_around_pointer(
            transform,
            response.drag_delta().y,
            viewer_rect.center(),
            anchor,
        );
    } else if response.dragged_by(egui::PointerButton::Middle) {
        *zoom_anchor = None;
        transform.pan += response.drag_delta();
    }

    if response.drag_stopped_by(egui::PointerButton::Secondary) {
        *zoom_anchor = None;
    }
}

fn show_viewer_overlays(
    app: &DicronApp,
    ui: &egui::Ui,
    viewer_rect: egui::Rect,
    image_rect: egui::Rect,
    image_size: [usize; 2],
) {
    let metadata = app.metadata.overlay.as_ref();
    let available_overlay_width =
        (viewer_rect.width() - OVERLAY_MARGIN * 2.0 - OVERLAY_BLOCK_GAP).max(0.0);
    let corner_max_height =
        ((viewer_rect.height() - OVERLAY_MARGIN * 2.0 - OVERLAY_BLOCK_GAP) / 2.0).max(0.0);
    let top_left_text = identification_overlay_text(metadata);
    let top_right_text = top_right_overlay_text(metadata);
    let (top_left_max_width, top_right_max_width) = paired_overlay_widths(
        ui.painter(),
        top_left_text.as_deref(),
        top_right_text.as_deref(),
        available_overlay_width,
        0.5,
    );

    if let Some(text) = top_left_text {
        paint_overlay_text(
            ui.painter(),
            viewer_rect.left_top() + egui::vec2(OVERLAY_MARGIN, OVERLAY_MARGIN),
            egui::Align2::LEFT_TOP,
            &text,
            top_left_max_width,
            corner_max_height,
        );
    }

    if let Some(text) = top_right_text {
        paint_overlay_text(
            ui.painter(),
            viewer_rect.right_top() + egui::vec2(-OVERLAY_MARGIN, OVERLAY_MARGIN),
            egui::Align2::RIGHT_TOP,
            &text,
            top_right_max_width,
            corner_max_height,
        );
    }

    let bottom_left_text = bottom_left_overlay_text(app, metadata);

    let mut bottom_right_lines = Vec::with_capacity(2);
    if let Some(pointer_position) = ui.input(|input_state| input_state.pointer.hover_pos())
        && let Some(probe_text) = pixel_probe_overlay_text(
            app,
            pointer_position,
            image_rect,
            image_size,
            metadata.and_then(|metadata| metadata.modality.as_deref()),
        )
    {
        bottom_right_lines.push(probe_text);
    }

    if app.window_level.is_available() {
        let window_level = app.window_level.current();
        bottom_right_lines.push(window_overlay_text(window_level.center, window_level.width));
    } else {
        bottom_right_lines.push("RGB".to_owned());
    }

    let bottom_right_text = bottom_right_lines.join("\n");
    let (bottom_left_max_width, bottom_right_max_width) = paired_overlay_widths(
        ui.painter(),
        bottom_left_text.as_deref(),
        Some(&bottom_right_text),
        available_overlay_width,
        0.4,
    );

    if let Some(text) = bottom_left_text {
        paint_overlay_text(
            ui.painter(),
            viewer_rect.left_bottom() + egui::vec2(OVERLAY_MARGIN, -OVERLAY_MARGIN),
            egui::Align2::LEFT_BOTTOM,
            &text,
            bottom_left_max_width,
            corner_max_height,
        );
    }

    paint_overlay_text(
        ui.painter(),
        viewer_rect.right_bottom() + egui::vec2(-OVERLAY_MARGIN, -OVERLAY_MARGIN),
        egui::Align2::RIGHT_BOTTOM,
        &bottom_right_text,
        bottom_right_max_width,
        corner_max_height,
    );

    if let Some((left, right)) = metadata
        .and_then(|metadata| metadata.image_orientation)
        .and_then(|orientation| side_orientation_markers(orientation, app.viewport_transform))
    {
        paint_overlay_text(
            ui.painter(),
            viewer_rect.left_center() + egui::vec2(OVERLAY_MARGIN, 0.0),
            egui::Align2::LEFT_CENTER,
            left,
            available_overlay_width,
            viewer_rect.height(),
        );
        paint_overlay_text(
            ui.painter(),
            viewer_rect.right_center() + egui::vec2(-OVERLAY_MARGIN, 0.0),
            egui::Align2::RIGHT_CENTER,
            right,
            available_overlay_width,
            viewer_rect.height(),
        );
    }
}

fn top_right_overlay_text(metadata: Option<&DicomOverlayMetadata>) -> Option<String> {
    let metadata = metadata?;
    let study_datetime = format_study_datetime(
        metadata.study_date.as_deref(),
        metadata.study_time.as_deref(),
    );

    join_overlay_lines([metadata.manufacturer.as_deref(), study_datetime.as_deref()])
}

fn bottom_left_overlay_text(
    app: &DicronApp,
    metadata: Option<&DicomOverlayMetadata>,
) -> Option<String> {
    let mut lines = Vec::with_capacity(3);

    if let Some(metadata) = metadata {
        lines.push(format_measurement("ST", metadata.slice_thickness));
        lines.push(format_measurement("SL", metadata.slice_location));
    }

    if let (Some(slice_index), Some(slice_count)) =
        (app.current_slice_index(), app.current_slice_count())
        && let Some(slice_text) = slice_overlay_text(slice_index, slice_count)
    {
        lines.push(slice_text);
    }

    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn format_study_datetime(date: Option<&str>, time: Option<&str>) -> Option<String> {
    let date = date.map(format_dicom_date);
    let time = time.map(format_dicom_time);
    join_overlay_lines([date.as_deref(), time.as_deref()]).map(|text| text.replace('\n', " "))
}

fn format_dicom_date(value: &str) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];

    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 8 || !bytes[..8].iter().all(u8::is_ascii_digit) {
        return value.to_owned();
    }

    let month = value[4..6].parse::<usize>().ok();
    let Some(month_name) = month.and_then(|month| MONTHS.get(month.saturating_sub(1))) else {
        return value.to_owned();
    };

    format!("{}-{month_name}-{}", &value[6..8], &value[..4])
}

fn format_dicom_time(value: &str) -> String {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 6 || !bytes[..6].iter().all(u8::is_ascii_digit) {
        return value.to_owned();
    }

    format!("{}:{}:{}", &value[..2], &value[2..4], &value[4..6])
}

fn format_measurement(label: &str, value: Option<f64>) -> String {
    value.map_or_else(
        || format!("{label}: -"),
        |value| format!("{label}: {value:.2} mm"),
    )
}

fn slice_overlay_text(slice_index: usize, slice_count: usize) -> Option<String> {
    (slice_count > 0 && slice_index < slice_count)
        .then(|| format!("Images: {}/{slice_count}", slice_index + 1))
}

fn window_overlay_text(center: f64, width: f64) -> String {
    format!("WL: {center:.0}  WW: {width:.0}")
}

fn pixel_probe_overlay_text(
    app: &DicronApp,
    pointer_position: egui::Pos2,
    image_rect: egui::Rect,
    image_size: [usize; 2],
    modality: Option<&str>,
) -> Option<String> {
    let (x, y) = image_pixel_coordinates(
        pointer_position,
        image_rect,
        image_size,
        app.viewport_transform,
    )?;
    let (path, frame_index) = app.current_frame_key.as_ref()?;
    let entry = app.decoded_cache.peek(path, *frame_index)?;
    let value = entry.frame.pixel_probe(x, y)?;

    Some(match value {
        PixelProbeValue::Monochrome(value) => {
            let label = if modality.is_some_and(|value| value.eq_ignore_ascii_case("CT")) {
                "HU"
            } else {
                "Value"
            };
            format!("X: {x}  Y: {y}  {label}: {}", format_probe_number(value))
        }
        PixelProbeValue::Rgb([red, green, blue]) => {
            format!("X: {x}  Y: {y}  RGB: ({red}, {green}, {blue})")
        }
    })
}

fn image_pixel_coordinates(
    pointer_position: egui::Pos2,
    image_rect: egui::Rect,
    image_size: [usize; 2],
    transform: ViewportTransform,
) -> Option<(usize, usize)> {
    if !image_rect.contains(pointer_position) || image_size[0] == 0 || image_size[1] == 0 {
        return None;
    }

    let relative = (pointer_position - image_rect.min) / image_rect.size();
    let (source_u, source_v) = transformed_image_uv(relative.x, relative.y, transform);
    let x = (source_u * image_size[0] as f32).floor() as usize;
    let y = (source_v * image_size[1] as f32).floor() as usize;

    Some((x.min(image_size[0] - 1), y.min(image_size[1] - 1)))
}

fn format_probe_number(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// The patient directions at the middle of the displayed left and right edges.
///
/// Each edge is traced back through the same mapping that draws the image, so
/// a flip or a quarter turn moves the letters with the anatomy.
fn side_orientation_markers(
    orientation: [f64; 6],
    transform: ViewportTransform,
) -> Option<(&'static str, &'static str)> {
    let edge_direction = |displayed_u: f32| {
        let (u, v) = transformed_image_uv(displayed_u, 0.5, transform);
        // From the image centre to that edge, along the image rows and columns.
        let (across_row, down_column) = (f64::from(u) - 0.5, f64::from(v) - 0.5);

        dominant_patient_direction(std::array::from_fn(|axis| {
            across_row * orientation[axis] + down_column * orientation[axis + 3]
        }))
    };

    Some((edge_direction(0.0)?, edge_direction(1.0)?))
}

fn paired_overlay_widths(
    painter: &egui::Painter,
    left_text: Option<&str>,
    right_text: Option<&str>,
    available_width: f32,
    constrained_left_share: f32,
) -> (f32, f32) {
    match (left_text, right_text) {
        (None, None) => (0.0, 0.0),
        (Some(_), None) => (available_width, 0.0),
        (None, Some(_)) => (0.0, available_width),
        (Some(left_text), Some(right_text)) => {
            let font = egui::FontId::monospace(OVERLAY_FONT_SIZE);
            let left_desired_width = overlay_text_width(painter, left_text, &font);
            let right_desired_width = overlay_text_width(painter, right_text, &font);

            if left_desired_width + right_desired_width <= available_width {
                return (left_desired_width, right_desired_width);
            }

            let left_width = available_width * constrained_left_share.clamp(0.0, 1.0);
            (left_width, available_width - left_width)
        }
    }
}

fn overlay_text_width(painter: &egui::Painter, text: &str, font: &egui::FontId) -> f32 {
    text.lines()
        .map(|line| {
            painter
                .layout_no_wrap(line.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
                .size()
                .x
        })
        .fold(0.0, f32::max)
}

#[cfg(test)]
mod tests {
    use eframe::egui;

    use super::{
        dominant_patient_direction, format_dicom_date, format_dicom_time, format_probe_number,
        image_pixel_coordinates, side_orientation_markers, slice_overlay_text,
        transformed_image_rect, window_overlay_text, zoom_viewport_around_pointer,
    };
    use crate::app::state::ViewportTransform;
    use crate::app::ui::overlay::VIEWER_ZOOM_DRAG_SENSITIVITY;
    use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
    use dicom_dictionary_std::{tags, uids};
    use dicom_object::{FileDicomObject, FileMetaTableBuilder};

    /// The side letters must name what is drawn at each edge after a flip or a
    /// quarter turn. A quarter turn is clockwise, and flips act on the screen
    /// after it: an axial slice turned once shows its posterior (bottom) side
    /// on the left.
    #[test]
    fn side_markers_follow_flips_and_rotations() {
        const AXIAL: [f64; 6] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        const CORONAL: [f64; 6] = [1.0, 0.0, 0.0, 0.0, 0.0, -1.0];
        let transform = |flip_horizontal, flip_vertical, rotation_quarters| ViewportTransform {
            flip_horizontal,
            flip_vertical,
            rotation_quarters,
            ..Default::default()
        };

        for (orientation, view, expected) in [
            (AXIAL, transform(false, false, 0), ("R", "L")),
            (AXIAL, transform(true, false, 0), ("L", "R")),
            (AXIAL, transform(false, true, 0), ("R", "L")),
            (AXIAL, transform(false, false, 1), ("P", "A")),
            (AXIAL, transform(false, false, 2), ("L", "R")),
            (AXIAL, transform(false, false, 3), ("A", "P")),
            (AXIAL, transform(true, false, 1), ("A", "P")),
            (AXIAL, transform(false, true, 1), ("P", "A")),
            (CORONAL, transform(false, false, 0), ("R", "L")),
            (CORONAL, transform(false, false, 1), ("F", "H")),
        ] {
            assert_eq!(
                side_orientation_markers(orientation, view),
                Some(expected),
                "flips {}/{}, quarters {}",
                view.flip_horizontal,
                view.flip_vertical,
                view.rotation_quarters
            );
        }
    }

    #[test]
    fn wheel_over_a_popup_is_not_consumed_by_the_viewer() {
        for show_popup in [false, true] {
            let context = egui::Context::default();
            let mut app = crate::app::DicronApp::default();
            for frame in 0..3 {
                let mut events = vec![egui::Event::PointerMoved(egui::pos2(150.0, 150.0))];
                if frame == 2 {
                    events.push(egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Line,
                        delta: egui::vec2(0.0, -1.0),
                        phase: egui::TouchPhase::Move,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(500.0, 400.0),
                    )),
                    events,
                    ..Default::default()
                };
                let _ = context.run_ui(input, |ui| {
                    if show_popup {
                        egui::Area::new(egui::Id::new("test_popup"))
                            .order(egui::Order::Foreground)
                            .fixed_pos(egui::pos2(100.0, 100.0))
                            .show(ui.ctx(), |ui| {
                                ui.set_min_size(egui::vec2(200.0, 200.0));
                                ui.label("Popup");
                            });
                    }
                    egui::CentralPanel::default().show_inside(ui, |ui| super::show(&mut app, ui));
                });
            }
            let remaining = context.input(|input| {
                input
                    .events
                    .iter()
                    .filter(|event| matches!(event, egui::Event::MouseWheel { .. }))
                    .count()
            });
            assert_eq!(remaining, usize::from(show_popup));
        }
    }

    /// A temporary 8-bit MONOCHROME2 image, `columns` wide and `rows` tall,
    /// carrying the given text elements, loaded the way the viewer opens a
    /// file. The file is removed once loaded.
    fn app_showing_image(
        context: &egui::Context,
        label: &str,
        [columns, rows]: [u16; 2],
        text_elements: &[(Tag, VR, &str)],
    ) -> crate::app::DicronApp {
        // The texture limit the viewer is given, before anything uploads.
        let _ = context.run_ui(viewer_input(Vec::new()), |_| {});
        app_loading_image(context, label, [columns, rows], text_elements)
    }

    /// The same image, loaded without running a frame first.
    fn app_loading_image(
        context: &egui::Context,
        label: &str,
        [columns, rows]: [u16; 2],
        text_elements: &[(Tag, VR, &str)],
    ) -> crate::app::DicronApp {
        let path = std::env::temp_dir().join(format!(
            "dicron-viewport-{label}-{}.dcm",
            std::process::id()
        ));
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(uids::SECONDARY_CAPTURE_IMAGE_STORAGE)
            .media_storage_sop_instance_uid("2.25.800")
            .build()
            .unwrap();
        let mut object = FileDicomObject::new_empty_with_meta(meta);
        for (tag, value) in [
            (tags::ROWS, rows),
            (tags::COLUMNS, columns),
            (tags::SAMPLES_PER_PIXEL, 1),
            (tags::BITS_ALLOCATED, 8),
            (tags::BITS_STORED, 8),
            (tags::HIGH_BIT, 7),
            (tags::PIXEL_REPRESENTATION, 0),
        ] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("MONOCHROME2"),
        ));
        for &(tag, vr, value) in text_elements {
            object.put_element(DataElement::new(tag, vr, PrimitiveValue::from(value)));
        }
        let pixel_count = usize::from(columns) * usize::from(rows);
        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from(
                (0..pixel_count)
                    .map(|index| (index % 251) as u8)
                    .collect::<Vec<_>>(),
            ),
        ));
        object.write_to_file(&path).unwrap();

        let mut app = crate::app::DicronApp {
            dicom_index: crate::dicom::build_for_file(&path).ok(),
            ..Default::default()
        };
        let loaded = app.load_slice_by_indices(context, 0, 0, 0, 0);
        std::fs::remove_file(&path).unwrap();
        assert!(loaded, "{:?}", app.error_message);
        app
    }

    fn viewer_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 400.0),
            )),
            max_texture_side: Some(8192),
            events,
            ..Default::default()
        }
    }

    /// Everything the viewer paints in one frame, with the pointer at `pointer`.
    fn paint_viewer(
        context: &egui::Context,
        app: &mut crate::app::DicronApp,
        pointer: Option<egui::Pos2>,
    ) -> Vec<egui::Shape> {
        let events = pointer.map(egui::Event::PointerMoved).into_iter().collect();
        let output = context.run_ui(viewer_input(events), |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| super::show(app, ui));
        });

        fn flatten(shape: egui::Shape, shapes: &mut Vec<egui::Shape>) {
            match shape {
                egui::Shape::Vec(nested) => {
                    for shape in nested {
                        flatten(shape, shapes);
                    }
                }
                shape => shapes.push(shape),
            }
        }
        let mut shapes = Vec::new();
        for clipped in output.shapes {
            flatten(clipped.shape, &mut shapes);
        }
        shapes
    }

    fn painted_texts(shapes: &[egui::Shape]) -> Vec<String> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect()
    }

    /// Where the image is painted, and the texture it is painted from.
    fn painted_image(shapes: &[egui::Shape]) -> (egui::Rect, egui::TextureId) {
        let mesh = shapes
            .iter()
            .find_map(|shape| match shape {
                egui::Shape::Mesh(mesh) => Some(mesh),
                _ => None,
            })
            .expect("the image is painted");

        (mesh.calc_bounds(), mesh.texture_id)
    }

    /// The source pixel the probe reports with the pointer at the given
    /// fraction of the painted image.
    fn probed_pixel(
        context: &egui::Context,
        app: &mut crate::app::DicronApp,
        image_rect: egui::Rect,
        fraction: egui::Vec2,
    ) -> String {
        let pointer = image_rect.min + fraction * image_rect.size();
        let texts = painted_texts(&paint_viewer(context, app, Some(pointer)));
        let probe = texts
            .iter()
            .find(|text| text.starts_with("X: "))
            .expect("the probe is shown");

        probe.split("  ").take(2).collect::<Vec<_>>().join(" ")
    }

    /// Both side letters must show however the app's fonts measure at the
    /// display's scale factor.
    #[test]
    fn side_markers_are_painted_with_the_app_fonts_at_any_scale() {
        for (app_fonts, pixels_per_point) in [
            (true, 1.0),
            (true, 1.25),
            (true, 1.5),
            (true, 2.0),
            (false, 1.25),
        ] {
            let context = egui::Context::default();
            if app_fonts {
                crate::configure_fonts(&context);
            }
            context.set_pixels_per_point(pixels_per_point);
            let mut app = app_showing_image(
                &context,
                "side-markers",
                [4, 4],
                &[(tags::IMAGE_ORIENTATION_PATIENT, VR::DS, "1\\0\\0\\0\\1\\0")],
            );

            paint_viewer(&context, &mut app, None);
            let texts = painted_texts(&paint_viewer(&context, &mut app, None));

            assert_eq!(context.pixels_per_point(), pixels_per_point);
            for letter in ["R", "L"] {
                assert!(
                    texts.iter().any(|text| text == letter),
                    "{letter} missing with app fonts {app_fonts} at {pixels_per_point}: {texts:?}"
                );
            }
        }
    }

    /// The painted image uses the pixel proportions recorded in the DICOM file.
    #[test]
    fn painted_images_use_declared_pixel_proportions() {
        for (tag, vr, value, expected_aspect) in [
            (tags::PIXEL_SPACING, VR::DS, "2\\1", 1.0),
            (tags::PIXEL_ASPECT_RATIO, VR::IS, "4\\1", 0.5),
        ] {
            let context = egui::Context::default();
            let mut app = app_showing_image(
                &context,
                "pixel-proportions",
                [100, 50],
                &[(tag, vr, value)],
            );
            let texture = app.loaded_image.as_ref().unwrap().texture.id();
            let shapes = paint_viewer(&context, &mut app, None);
            let image = shapes
                .iter()
                .find_map(|shape| match shape {
                    egui::Shape::Mesh(mesh) if mesh.texture_id == texture => {
                        Some(mesh.calc_bounds())
                    }
                    _ => None,
                })
                .expect("the image is painted");
            assert_approximately_equal(image.aspect_ratio(), expected_aspect);
        }
    }

    /// A long stitched radiograph wider or taller than the GPU allows is
    /// uploaded smaller, but still shown and probed at its own size.
    #[test]
    fn images_larger_than_the_texture_limit_are_shown_at_their_own_size() {
        for (size, fraction, expected_pixel) in [
            ([8200, 16], egui::vec2(6150.5 / 8200.0, 0.5), "X: 6150 Y: 8"),
            ([16, 8200], egui::vec2(0.5, 6150.5 / 8200.0), "X: 8 Y: 6150"),
        ] {
            let context = egui::Context::default();
            let mut app = app_showing_image(&context, "oversized", size, &[]);

            let (image_rect, texture_id) = painted_image(&paint_viewer(&context, &mut app, None));
            let texture_size = context.tex_manager().read().meta(texture_id).unwrap().size;

            assert!(
                texture_size.iter().all(|&side| side <= 8192),
                "{texture_size:?}"
            );
            let source_aspect = f32::from(size[0]) / f32::from(size[1]);
            let texture_aspect = texture_size[0] as f32 / texture_size[1] as f32;
            assert!(
                (texture_aspect / source_aspect - 1.0).abs() < 0.01,
                "{texture_size:?}"
            );
            assert!(
                (image_rect.aspect_ratio() / source_aspect - 1.0).abs() < 0.01,
                "{image_rect:?}"
            );
            assert_eq!(
                probed_pixel(&context, &mut app, image_rect, fraction),
                expected_pixel
            );
        }
    }

    fn assert_approximately_equal(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.001,
            "expected {expected}, got {actual}"
        );
    }

    /// Until the first frame tells egui the GPU's texture limit, egui assumes
    /// 2048 texels a side. A file opened from the command line waits for that
    /// frame, and an image loaded in it, well inside every GPU's limit,
    /// uploads at full size.
    #[test]
    fn images_opened_before_the_first_frame_upload_at_full_size() {
        let context = egui::Context::default();
        let missing_path = std::env::temp_dir().join(format!(
            "dicron-viewport-missing-startup-{}.dcm",
            std::process::id()
        ));
        let mut startup_app = crate::app::DicronApp::new(&context, vec![missing_path]);
        assert_eq!(
            startup_app.error_message, None,
            "opened before the first frame"
        );

        let mut first_frame_app = None;
        let _ = context.run_ui(viewer_input(Vec::new()), |ui| {
            eframe::App::logic(
                &mut startup_app,
                ui.ctx(),
                &mut eframe::Frame::_new_kittest(),
            );
            first_frame_app = Some(app_loading_image(ui.ctx(), "first-frame", [3000, 16], &[]));
        });
        assert!(
            startup_app.error_message.is_some(),
            "not opened on the first frame"
        );

        let mut app = first_frame_app.unwrap();
        let (_, texture_id) = painted_image(&paint_viewer(&context, &mut app, None));
        let texture_size = context.tex_manager().read().meta(texture_id).unwrap().size;

        assert_eq!(texture_size, [3000, 16]);
    }

    /// Shrinking a one-row strip keeps its single row, so the texture is half
    /// as long for its height as the source; the strip is still laid out and
    /// probed at the source's proportions.
    #[test]
    fn oversized_one_row_strips_keep_their_source_proportions() {
        let context = egui::Context::default();
        let mut app = app_showing_image(&context, "oversized-strip", [8200, 1], &[]);

        let (image_rect, texture_id) = painted_image(&paint_viewer(&context, &mut app, None));
        let texture_size = context.tex_manager().read().meta(texture_id).unwrap().size;

        assert_eq!(texture_size, [4100, 1]);
        assert!(
            (image_rect.aspect_ratio() / 8200.0 - 1.0).abs() < 0.01,
            "{image_rect:?}"
        );
        assert_eq!(
            probed_pixel(&context, &mut app, image_rect, egui::vec2(0.9999, 0.5)),
            "X: 8199 Y: 0"
        );
    }

    #[test]
    fn slice_overlay_uses_human_readable_numbering() {
        assert_eq!(slice_overlay_text(0, 20).as_deref(), Some("Images: 1/20"));
        assert_eq!(slice_overlay_text(19, 20).as_deref(), Some("Images: 20/20"));
    }

    #[test]
    fn slice_overlay_rejects_inconsistent_ranges() {
        assert_eq!(slice_overlay_text(0, 0), None);
        assert_eq!(slice_overlay_text(20, 20), None);
    }

    #[test]
    fn window_overlay_uses_the_compact_single_line_layout() {
        assert_eq!(window_overlay_text(510.0, 1091.0), "WL: 510  WW: 1091");
    }

    #[test]
    fn dicom_date_and_time_use_readable_overlay_formats() {
        assert_eq!(format_dicom_date("20241116"), "16-November-2024");
        assert_eq!(format_dicom_time("195257.123456"), "19:52:57");
        assert_eq!(format_dicom_date("unknown"), "unknown");
    }

    #[test]
    fn pointer_position_maps_to_fitted_image_pixels() {
        let image_rect = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(210.0, 120.0));

        assert_eq!(
            image_pixel_coordinates(
                egui::pos2(110.0, 70.0),
                image_rect,
                [400, 200],
                ViewportTransform::default(),
            ),
            Some((200, 100))
        );
        assert_eq!(
            image_pixel_coordinates(
                egui::pos2(9.0, 70.0),
                image_rect,
                [400, 200],
                ViewportTransform::default(),
            ),
            None
        );
    }

    #[test]
    fn viewport_transform_scales_from_fit_and_applies_pan() {
        let viewer_rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let transform = ViewportTransform {
            zoom: 2.0,
            pan: egui::vec2(20.0, -5.0),
            ..Default::default()
        };

        let image_rect = transformed_image_rect(viewer_rect, egui::vec2(100.0, 50.0), transform);

        assert_eq!(image_rect.center(), egui::pos2(120.0, 45.0));
        assert_eq!(image_rect.size(), egui::vec2(200.0, 100.0));
    }

    #[test]
    fn right_drag_down_zoom_keeps_the_drag_start_point_anchored() {
        let viewer_rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let fitted_size = egui::vec2(100.0, 50.0);
        let anchor = egui::pos2(125.0, 50.0);
        let mut transform = ViewportTransform::default();
        let before = transformed_image_rect(viewer_rect, fitted_size, transform);
        let relative_anchor = (anchor - before.min) / before.size();
        zoom_viewport_around_pointer(
            &mut transform,
            std::f32::consts::LN_2 / VIEWER_ZOOM_DRAG_SENSITIVITY,
            viewer_rect.center(),
            anchor,
        );
        let after = transformed_image_rect(viewer_rect, fitted_size, transform);
        let transformed_anchor = after.min + relative_anchor * after.size();

        assert_approximately_equal(transform.zoom, 2.0);
        assert_approximately_equal(transformed_anchor.x, anchor.x);
        assert_approximately_equal(transformed_anchor.y, anchor.y);
    }

    #[test]
    fn pixel_probe_numbers_keep_useful_fractional_precision() {
        assert_eq!(format_probe_number(43.01), "43");
        assert_eq!(format_probe_number(43.25), "43.2");
    }

    #[test]
    fn orientation_uses_the_dominant_patient_axis() {
        assert_eq!(dominant_patient_direction([1.0, 0.0, 0.0]), Some("L"));
        assert_eq!(dominant_patient_direction([0.0, -0.9, 0.1]), Some("A"));
        assert_eq!(dominant_patient_direction([0.0, 0.0, 0.0]), None);
    }
}
