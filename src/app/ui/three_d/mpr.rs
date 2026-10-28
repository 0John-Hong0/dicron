//! The three-plane MPR and thick-slab MIP views.

use std::ops::RangeInclusive;
use std::sync::Arc;

use eframe::egui;

use super::paint_centered_message;
use crate::app::DicronApp;
use crate::app::actions::consume_wheel_slice_steps;
use crate::app::state::{ViewportTransform, WindowLevel};
use crate::app::three_d_state::{MprDrag, MprMode, MprState, PlaneTextureKey};
use crate::app::ui::overlay::{
    identification_overlay_text, paint_overlay_text, paint_transformed_image,
    transformed_image_rect, zoom_viewport_around_pointer,
};
use crate::dicom::{PatientAxis, PlaneImage, PlaneLayout, Volume, VolumeAxis, VolumeGeometry};
use crate::theme;

/// Height of the band the plane title is painted in, matching the source
/// application.
const TITLE_BAND_HEIGHT: f32 = 34.0;
const VIEW_INSET: f32 = theme::SPACE_SM;
/// How close to a slab boundary a press still grabs it.
const BOUNDARY_GRAB_TOLERANCE: f32 = 9.0;
const CROSSHAIR_STROKE: f32 = 1.4;
const BOUNDARY_STROKE: f32 = 1.2;
const DASH_LENGTH: f32 = 2.0;
const DASH_GAP: f32 = 3.0;

/// The patient planes the three views show, left to right.
const VIEW_PLANES: [PatientAxis; 3] = [
    PatientAxis::SuperiorInferior,
    PatientAxis::AnteriorPosterior,
    PatientAxis::LeftRight,
];

/// The volume axes the views cut, always ordered axial, coronal, sagittal
/// whatever plane the series was acquired in.
fn view_axes(geometry: &VolumeGeometry) -> [VolumeAxis; 3] {
    VIEW_PLANES.map(|plane| {
        VolumeAxis::ALL
            .into_iter()
            .find(|axis| geometry.patient_axis(*axis).0 == plane)
            .unwrap_or(VolumeAxis::Z)
    })
}

pub(super) fn show(app: &mut DicronApp, ui: &mut egui::Ui, mode: MprMode) {
    // The low-resolution warning owns the pointer while it is up, so the views
    // below it keep painting but stop reacting.
    let interactive = app.three_d.low_resolution_prompt.is_none();
    let shows_selection = app.workspace_shows_selection();
    let Some(active) = app.three_d.active.as_mut().filter(|_| shows_selection) else {
        paint_centered_message(ui, super::PREPARING_VOLUME_MESSAGE);
        return;
    };

    let available_size = ui.available_size();
    let (panel_rect, _panel_response) = ui.allocate_exact_size(
        egui::vec2(available_size.x.max(1.0), available_size.y.max(1.0)),
        egui::Sense::hover(),
    );

    // The three views always share the panel: they shrink together instead of
    // scrolling, which would fight the viewer's own wheel handling.
    let view_width = (panel_rect.width() / 3.0).max(1.0);
    let view_rects: [egui::Rect; 3] = std::array::from_fn(|position| {
        egui::Rect::from_min_size(
            egui::pos2(
                panel_rect.left() + position as f32 * view_width,
                panel_rect.top(),
            ),
            egui::vec2(view_width, panel_rect.height()),
        )
    });
    let geometry = &active.prepared.series.geometry;
    let mpr = &mut active.mpr;

    let axes = view_axes(geometry);

    handle_wheel(
        ui.ctx(),
        mpr,
        geometry,
        hovered_view_axis(ui, axes, &view_rects, interactive),
    );

    let views: [MprViewParams<'_>; 3] = std::array::from_fn(|position| MprViewParams {
        geometry,
        axis: axes[position],
        layout: geometry.plane_layout(axes[position]),
        mode,
        view_rect: view_rects[position],
    });

    // Every view reacts before any of them paints, so a crosshair dragged in
    // one view moves the other two in the same frame.
    if interactive {
        for params in &views {
            if let Some(image_rect) = view_image_rect(mpr, params) {
                handle_view_interaction(ui, mpr, params, image_rect);
            }
        }
    }

    for params in &views {
        paint_view(ui, mpr, params);
    }

    paint_view_dividers(ui, &view_rects);

    // Once, in the corner of the first view the title band leaves free.
    if let Some(text) = identification_overlay_text(active.series_overlay.as_ref()) {
        let content_rect = view_content_rect(view_rects[0]);
        paint_overlay_text(
            &ui.painter().with_clip_rect(view_rects[0]),
            content_rect.left_bottom(),
            egui::Align2::LEFT_BOTTOM,
            &text,
            content_rect.width().max(0.0),
            content_rect.height().max(0.0),
        );
    }
}

/// Hairlines over the edges the views share, so they cost no space.
fn paint_view_dividers(ui: &egui::Ui, view_rects: &[egui::Rect; 3]) {
    let stroke = ui.visuals().widgets.noninteractive.fg_stroke;

    for view_rect in &view_rects[1..] {
        ui.painter()
            .line_segment([view_rect.left_top(), view_rect.left_bottom()], stroke);
    }
}

/// The slab thickness read-out, one entry per view in view order: the tissue
/// each projection actually covers, fewer slices where a slab meets the end of
/// the volume.
pub(super) fn slab_thickness_label(mpr: &MprState, geometry: &VolumeGeometry) -> String {
    let entries: Vec<String> = view_axes(geometry)
        .into_iter()
        .map(|axis| {
            let initial = geometry
                .plane_label(axis)
                .chars()
                .next()
                .unwrap_or_default();
            let (lowest, highest) = slab_boundary_indices(
                mpr.index(axis),
                mpr.slab_half_count(axis),
                mpr.volume.axis_len(axis),
            );
            let covered_mm = (highest + 1 - lowest) as f64 * mpr.volume.axis_spacing(axis);

            format!("{initial}: {covered_mm:.1} mm")
        })
        .collect();

    format!("Slab thickness  {}", entries.join("   "))
}

/// Everything one view needs besides the state it edits.
struct MprViewParams<'a> {
    geometry: &'a VolumeGeometry,
    axis: VolumeAxis,
    /// Which volume axis runs across the view and which runs down it, signed so
    /// the plane is shown the standard way round.
    layout: PlaneLayout,
    mode: MprMode,
    view_rect: egui::Rect,
}

/// The space under a view's title band that its plane is fitted into.
fn view_content_rect(view_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        view_rect.min + egui::vec2(VIEW_INSET, TITLE_BAND_HEIGHT),
        view_rect.max - egui::vec2(VIEW_INSET, VIEW_INSET),
    )
}

/// Where a view's plane is drawn: its millimetre aspect fitted into the space
/// under the title band, then zoomed and panned as the user left it. `None`
/// when there is nothing to show.
fn view_image_rect(mpr: &MprState, params: &MprViewParams<'_>) -> Option<egui::Rect> {
    let [column_extent, row_extent] = mpr
        .volume
        .plane_extent_mm(params.axis)
        .map(|size| size as f32);
    // The extents come out in the plane's own order, which a transposed view
    // swaps before fitting.
    let [physical_width, physical_height] = if params.layout.transposed {
        [row_extent, column_extent]
    } else {
        [column_extent, row_extent]
    };

    if mpr.volume.axis_len(params.axis) == 0 || physical_width <= 0.0 || physical_height <= 0.0 {
        return None;
    }

    let content_rect = view_content_rect(params.view_rect);
    let fitted = fit_physical_aspect(content_rect, physical_width, physical_height);

    if fitted.width() <= 0.0 || fitted.height() <= 0.0 {
        return None;
    }

    Some(transformed_image_rect(
        content_rect,
        fitted.size(),
        mpr.view_transforms[params.axis.index()],
    ))
}

fn paint_view(ui: &egui::Ui, mpr: &mut MprState, params: &MprViewParams<'_>) {
    let view_rect = params.view_rect;
    let axis = params.axis;
    let painter = ui.painter().with_clip_rect(view_rect);
    painter.rect_filled(view_rect, 0.0, egui::Color32::BLACK);

    let axis_len = mpr.volume.axis_len(axis);
    let index = mpr.index(axis);

    // Painted last, so an image zoomed or panned into the title band never
    // covers it.
    let paint_title = |painter: &egui::Painter| {
        paint_overlay_text(
            painter,
            view_rect.left_top() + egui::vec2(VIEW_INSET, VIEW_INSET),
            egui::Align2::LEFT_TOP,
            &format!(
                "{}  {}/{}",
                params.geometry.plane_label(axis),
                index + 1,
                axis_len
            ),
            (view_rect.width() - 2.0 * VIEW_INSET).max(0.0),
            TITLE_BAND_HEIGHT,
        );
    };

    let Some(image_rect) = view_image_rect(mpr, params) else {
        paint_title(&painter);
        return;
    };

    let half_count = match params.mode {
        MprMode::Slice => 0,
        MprMode::MipSlab => mpr.slab_half_count(axis),
    };
    // A plane drawn over fewer physical pixels than it has is reduced before
    // it is uploaded: bilinear sampling alone reads only the four texels
    // around each screen pixel, so structures a voxel wide fall between them.
    let reduction = plane_reduction(
        &mpr.volume,
        axis,
        params.layout.transposed,
        image_rect.size() * ui.ctx().pixels_per_point(),
        ui.ctx().input(|input_state| input_state.max_texture_side),
    );
    let key = PlaneTextureKey::new(
        mpr.volume_generation,
        index,
        params.mode,
        half_count,
        mpr.window,
        reduction,
    );
    let volume = Arc::clone(&mpr.volume);
    let window = mpr.window;
    let inverted = params.geometry.inverted;
    let context = ui.ctx().clone();
    let mode = params.mode;
    let texture_id = mpr.plane_texture(
        axis,
        key,
        || {
            let plane = match mode {
                MprMode::Slice => volume.plane(axis, index),
                MprMode::MipSlab => volume.slab_maximum(axis, index, half_count),
            };

            reduced_plane(plane, reduction, mode)
        },
        |plane| upload_plane_texture(&context, axis, plane, window, inverted),
    );

    paint_transformed_image(
        &painter,
        texture_id,
        image_rect,
        display_transform(params.layout),
    );

    paint_crosshair(&painter, mpr, params, image_rect);
    paint_orientation_markers(&painter, params);
    paint_title(&painter);
}

/// The texture mapping that puts the plane on screen the standard way round.
///
/// A transposed view swaps the plane's columns and rows, which the quarter
/// rotation does together with the extra horizontal flip it brings with it.
fn display_transform(layout: PlaneLayout) -> ViewportTransform {
    if layout.transposed {
        ViewportTransform {
            flip_horizontal: !layout.horizontal.flipped,
            flip_vertical: layout.vertical.flipped,
            rotation_quarters: 1,
            ..Default::default()
        }
    } else {
        ViewportTransform {
            flip_horizontal: layout.horizontal.flipped,
            flip_vertical: layout.vertical.flipped,
            ..Default::default()
        }
    }
}

/// The patient direction letters on each edge, so a reformat can never be read
/// the wrong way round. The 2D viewer paints the same markers.
fn paint_orientation_markers(painter: &egui::Painter, params: &MprViewParams<'_>) {
    let content_rect = view_content_rect(params.view_rect);

    if content_rect.width() <= 0.0 || content_rect.height() <= 0.0 {
        return;
    }

    let layout = params.layout;
    let markers = [
        (
            content_rect.left_center(),
            egui::Align2::LEFT_CENTER,
            layout.horizontal.near_label(),
        ),
        (
            content_rect.right_center(),
            egui::Align2::RIGHT_CENTER,
            layout.horizontal.far_label(),
        ),
        (
            content_rect.center_top(),
            egui::Align2::CENTER_TOP,
            layout.vertical.near_label(),
        ),
        (
            content_rect.center_bottom(),
            egui::Align2::CENTER_BOTTOM,
            layout.vertical.far_label(),
        ),
    ];

    for (position, anchor, label) in markers {
        paint_overlay_text(
            painter,
            position,
            anchor,
            label,
            content_rect.width(),
            content_rect.height(),
        );
    }
}

/// One of the two other axes a view shows, placed on the screen direction its
/// patient axis belongs on.
#[derive(Clone, Copy)]
struct ViewLine {
    axis: VolumeAxis,
    vertical: bool,
    /// Whether the axis runs backwards along that screen direction.
    flipped: bool,
}

/// The lines a view draws: the axis running across it is marked by a vertical
/// line, the one running down it by a horizontal line.
fn view_lines(layout: PlaneLayout) -> [ViewLine; 2] {
    [
        ViewLine {
            axis: layout.horizontal.axis,
            vertical: true,
            flipped: layout.horizontal.flipped,
        },
        ViewLine {
            axis: layout.vertical.axis,
            vertical: false,
            flipped: layout.vertical.flipped,
        },
    ]
}

impl ViewLine {
    /// The screen span the line moves along.
    fn range(self, image_rect: egui::Rect) -> RangeInclusive<f32> {
        if self.vertical {
            image_rect.left()..=image_rect.right()
        } else {
            image_rect.top()..=image_rect.bottom()
        }
    }

    fn segment(self, image_rect: egui::Rect, position: f32) -> [egui::Pos2; 2] {
        if self.vertical {
            [
                egui::pos2(position, image_rect.top()),
                egui::pos2(position, image_rect.bottom()),
            ]
        } else {
            [
                egui::pos2(image_rect.left(), position),
                egui::pos2(image_rect.right(), position),
            ]
        }
    }

    /// The pointer coordinate that moves this line.
    fn pointer_coordinate(self, pointer: egui::Pos2) -> f32 {
        if self.vertical { pointer.x } else { pointer.y }
    }

    fn resize_cursor(self) -> egui::CursorIcon {
        if self.vertical {
            egui::CursorIcon::ResizeHorizontal
        } else {
            egui::CursorIcon::ResizeVertical
        }
    }
}

/// Crosshair colours follow the plane each axis cuts: sagittal green, coronal
/// yellow, axial red.
fn axis_color(geometry: &VolumeGeometry, axis: VolumeAxis) -> egui::Color32 {
    match geometry.patient_axis(axis).0 {
        PatientAxis::LeftRight => egui::Color32::GREEN,
        PatientAxis::AnteriorPosterior => egui::Color32::YELLOW,
        PatientAxis::SuperiorInferior => egui::Color32::RED,
    }
}

fn paint_crosshair(
    painter: &egui::Painter,
    mpr: &MprState,
    params: &MprViewParams<'_>,
    image_rect: egui::Rect,
) {
    for line in view_lines(params.layout) {
        let axis_len = mpr.volume.axis_len(line.axis);
        let index = mpr.index(line.axis);
        let color = axis_color(params.geometry, line.axis);
        let flipped = line.flipped;
        let position = position_at_index(index, line.range(image_rect), axis_len, flipped);

        painter.line_segment(
            line.segment(image_rect, position),
            egui::Stroke::new(CROSSHAIR_STROKE, color),
        );

        if !params.mode.has_slab() {
            continue;
        }

        let (lowest, highest) =
            slab_boundary_indices(index, mpr.slab_half_count(line.axis), axis_len);

        for position in
            slab_face_positions(lowest, highest, line.range(image_rect), axis_len, flipped)
        {
            painter.extend(egui::Shape::dashed_line(
                &line.segment(image_rect, position),
                egui::Stroke::new(BOUNDARY_STROKE, color),
                DASH_LENGTH,
                DASH_GAP,
            ));
        }
    }
}

fn handle_view_interaction(
    ui: &egui::Ui,
    mpr: &mut MprState,
    params: &MprViewParams<'_>,
    image_rect: egui::Rect,
) {
    // Drag-only, so egui marks the widget dragged on the press frame instead of
    // waiting to tell a click and a drag apart.
    let response = ui.interact(
        params.view_rect,
        ui.id().with(("mpr_view", params.axis.index())),
        egui::Sense::drag(),
    );

    // The buttons follow the 2D viewer: right drag zooms, middle drag pans.
    if response.drag_started_by(egui::PointerButton::Primary) {
        mpr.drag = response
            .interact_pointer_pos()
            .filter(|press| image_rect.contains(*press))
            .map(|press| latched_drag(mpr, params, image_rect, press));
    } else if response.drag_started_by(egui::PointerButton::Secondary) {
        mpr.drag = response.interact_pointer_pos().map(|anchor| MprDrag::Zoom {
            view: params.axis,
            anchor,
        });
    } else if response.drag_started_by(egui::PointerButton::Middle) {
        mpr.drag = Some(MprDrag::Pan { view: params.axis });
    }

    if response.dragged()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        match mpr.drag {
            Some(MprDrag::Crosshair { view }) if view == params.axis => {
                move_crosshair(mpr, params.layout, image_rect, pointer);
            }
            Some(MprDrag::SlabBoundary {
                view,
                axis,
                grab_offset,
            }) if view == params.axis => {
                resize_slab(mpr, params.layout, axis, image_rect, pointer, grab_offset);
            }
            Some(MprDrag::Zoom { view, anchor }) if view == params.axis => {
                zoom_viewport_around_pointer(
                    &mut mpr.view_transforms[params.axis.index()],
                    response.drag_delta().y,
                    view_content_rect(params.view_rect).center(),
                    anchor,
                );
            }
            Some(MprDrag::Pan { view }) if view == params.axis => {
                mpr.view_transforms[params.axis.index()].pan += response.drag_delta();
            }
            _ => {}
        }
    }

    if response.drag_stopped() {
        mpr.drag = None;
    }

    // A tap whose press and release reach egui in the same frame, as touchpad
    // taps and quick clicks on a slow frame do, starts no drag: it moves the
    // crosshair all the same, unless it landed on a slab boundary.
    if mpr.drag.is_none()
        && let Some(tap) = ui.input(|input| {
            // Read from the press itself: egui forgets where a press began
            // once the release in the same frame is processed.
            input
                .pointer
                .primary_released()
                .then(|| {
                    input.events.iter().find_map(|event| match event {
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            ..
                        } => Some(*pos),
                        _ => None,
                    })
                })
                .flatten()
        })
        && response.rect.contains(tap)
        && image_rect.contains(tap)
        && matches!(
            latched_drag(mpr, params, image_rect, tap),
            MprDrag::Crosshair { .. }
        )
    {
        move_crosshair(mpr, params.layout, image_rect, tap);
    }

    if let Some(cursor) = view_cursor(mpr, params, image_rect, &response) {
        ui.ctx().set_cursor_icon(cursor);
    }
}

/// What a press starts moving: a slab boundary when one is within grabbing
/// distance, the crosshair otherwise.
fn latched_drag(
    mpr: &MprState,
    params: &MprViewParams<'_>,
    image_rect: egui::Rect,
    press: egui::Pos2,
) -> MprDrag {
    if params.mode.has_slab()
        && let Some((axis, grab_offset)) = grabbed_boundary(mpr, params.layout, image_rect, press)
    {
        return MprDrag::SlabBoundary {
            view: params.axis,
            axis,
            grab_offset,
        };
    }

    MprDrag::Crosshair { view: params.axis }
}

/// The slab boundary a press grabs, with how far from it the press landed:
/// one within reach, and nearer than the crosshair line of its own axis, so a
/// press on the crosshair moves the crosshair however thin the slab is.
fn grabbed_boundary(
    mpr: &MprState,
    layout: PlaneLayout,
    image_rect: egui::Rect,
    pointer: egui::Pos2,
) -> Option<(VolumeAxis, f32)> {
    view_lines(layout).into_iter().find_map(|line| {
        let axis_len = mpr.volume.axis_len(line.axis);
        let index = mpr.index(line.axis);
        let flipped = line.flipped;
        let (lowest, highest) =
            slab_boundary_indices(index, mpr.slab_half_count(line.axis), axis_len);
        let range = line.range(image_rect);
        let coordinate = line.pointer_coordinate(pointer);
        let to_crosshair =
            (coordinate - position_at_index(index, range.clone(), axis_len, flipped)).abs();

        slab_face_positions(lowest, highest, range, axis_len, flipped)
            .into_iter()
            .map(|position| coordinate - position)
            .filter(|offset| offset.abs() <= BOUNDARY_GRAB_TOLERANCE && offset.abs() < to_crosshair)
            .min_by(|one, other| one.abs().total_cmp(&other.abs()))
            .map(|offset| (line.axis, offset))
    })
}

/// Point the two other planes at the pointer. Unlike the source application the
/// drag clamps at the image edges instead of freezing when it leaves them.
fn move_crosshair(
    mpr: &mut MprState,
    layout: PlaneLayout,
    image_rect: egui::Rect,
    pointer: egui::Pos2,
) {
    for line in view_lines(layout) {
        let axis_len = mpr.volume.axis_len(line.axis);
        let index = index_at_position(
            line.pointer_coordinate(pointer),
            line.range(image_rect),
            axis_len,
            line.flipped,
        );

        mpr.set_index(line.axis, index);
    }
}

/// Thicken or thin one slab by dragging a boundary away from its centre.
/// Move the grabbed boundary with the pointer, keeping the offset it was
/// grabbed at, so a press alone changes nothing.
fn resize_slab(
    mpr: &mut MprState,
    layout: PlaneLayout,
    slab_axis: VolumeAxis,
    image_rect: egui::Rect,
    pointer: egui::Pos2,
    grab_offset: f32,
) {
    let Some(line) = view_lines(layout)
        .into_iter()
        .find(|line| line.axis == slab_axis)
    else {
        return;
    };

    let axis_len = mpr.volume.axis_len(slab_axis);
    let face = face_at_position(
        line.pointer_coordinate(pointer) - grab_offset,
        line.range(image_rect),
        axis_len,
        line.flipped,
    );
    let thickness_mm = dragged_slab_thickness_mm(
        face,
        mpr.index(slab_axis),
        mpr.volume.axis_spacing(slab_axis),
    );

    mpr.set_slab_thickness_mm(slab_axis, thickness_mm);
}

/// The slab a boundary dragged to slice face `face` asks for: the slices from
/// the centre out to that face, mirrored on the other side. Face `i` is the
/// edge between slices `i - 1` and `i`.
fn dragged_slab_thickness_mm(face: usize, center_index: usize, spacing_mm: f64) -> f64 {
    let half_count = if face > center_index {
        face - 1 - center_index
    } else {
        center_index - face
    };

    (2 * half_count + 1) as f64 * spacing_mm
}

/// Where the outer faces of the slab's end slices are drawn: the dashed
/// boundaries enclose exactly the slices the projection takes.
fn slab_face_positions(
    lowest: usize,
    highest: usize,
    range: RangeInclusive<f32>,
    axis_len: usize,
    flipped: bool,
) -> [f32; 2] {
    [lowest, highest + 1].map(|face| {
        let fraction = face as f32 / axis_len.max(1) as f32;

        egui::lerp(
            range.clone(),
            if flipped { 1.0 - fraction } else { fraction },
        )
    })
}

/// The slice face nearest a pointer coordinate, from 0 to `axis_len`.
fn face_at_position(
    position: f32,
    range: RangeInclusive<f32>,
    axis_len: usize,
    flipped: bool,
) -> usize {
    let start = *range.start();
    let span = *range.end() - start;
    let fraction = if span > 0.0 {
        ((position - start) / span).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let fraction = if flipped { 1.0 - fraction } else { fraction };

    ((fraction * axis_len as f32).round() as usize).min(axis_len)
}

fn view_cursor(
    mpr: &MprState,
    params: &MprViewParams<'_>,
    image_rect: egui::Rect,
    response: &egui::Response,
) -> Option<egui::CursorIcon> {
    match mpr.drag {
        Some(MprDrag::Crosshair { view }) if view == params.axis => {
            return Some(egui::CursorIcon::Grabbing);
        }
        Some(MprDrag::SlabBoundary { view, axis, .. }) if view == params.axis => {
            return Some(boundary_cursor(params.layout, axis));
        }
        Some(MprDrag::Zoom { view, .. }) if view == params.axis => {
            return Some(egui::CursorIcon::ResizeVertical);
        }
        Some(MprDrag::Pan { view }) if view == params.axis => {
            return Some(egui::CursorIcon::Grabbing);
        }
        _ => {}
    }

    let pointer = response
        .hover_pos()
        .filter(|pointer| image_rect.contains(*pointer))?;

    if params.mode.has_slab()
        && let Some((axis, _)) = grabbed_boundary(mpr, params.layout, image_rect, pointer)
    {
        return Some(boundary_cursor(params.layout, axis));
    }

    Some(egui::CursorIcon::Crosshair)
}

fn boundary_cursor(layout: PlaneLayout, slab_axis: VolumeAxis) -> egui::CursorIcon {
    view_lines(layout)
        .into_iter()
        .find(|line| line.axis == slab_axis)
        .map_or(egui::CursorIcon::Crosshair, ViewLine::resize_cursor)
}

/// The first and last slice a slab covers, clamped to the axis.
fn slab_boundary_indices(index: usize, half_count: usize, axis_len: usize) -> (usize, usize) {
    (
        index.saturating_sub(half_count),
        (index + half_count).min(axis_len.saturating_sub(1)),
    )
}

/// Drain the frame's wheel input into the axis of the view the pointer is over.
/// Wheel-down steps the hovered view in reading order: towards the feet, the
/// back or the patient's left.
fn handle_wheel(
    context: &egui::Context,
    mpr: &mut MprState,
    geometry: &VolumeGeometry,
    hovered_axis: Option<VolumeAxis>,
) {
    for axis in VolumeAxis::ALL {
        if Some(axis) != hovered_axis {
            mpr.wheel_remainder[axis.index()] = 0.0;
        }
    }

    let Some(axis) = hovered_axis else {
        return;
    };
    let stepped_slices = consume_wheel_slice_steps(context, &mut mpr.wheel_remainder[axis.index()]);

    if stepped_slices != 0 {
        mpr.step_index(axis, stepped_slices * geometry.reading_order_step(axis));
    }
}

/// The view that owns this frame's wheel input: the one under the pointer,
/// unless a menu or dialog lies between them.
fn hovered_view_axis(
    ui: &egui::Ui,
    axes: [VolumeAxis; 3],
    view_rects: &[egui::Rect; 3],
    interactive: bool,
) -> Option<VolumeAxis> {
    if !interactive {
        return None;
    }

    axes.into_iter()
        .zip(view_rects)
        .find(|(_axis, view_rect)| ui.rect_contains_pointer(**view_rect))
        .map(|(axis, _view_rect)| axis)
}

/// The fraction of an axis the centre of cell `index` sits at.
fn axis_fraction(index: usize, axis_len: usize) -> f32 {
    (index as f32 + 0.5) / axis_len.max(1) as f32
}

/// The cell a fraction of an axis falls in, clamped to the axis.
fn index_from_fraction(fraction: f32, axis_len: usize) -> usize {
    let last_index = axis_len.saturating_sub(1) as isize;

    ((fraction * axis_len as f32).floor() as isize).clamp(0, last_index) as usize
}

/// The fraction of a view an index is displayed at, head-up flip included.
fn displayed_fraction(index: usize, axis_len: usize, flipped: bool) -> f32 {
    let fraction = axis_fraction(index, axis_len);

    if flipped { 1.0 - fraction } else { fraction }
}

/// The index a displayed fraction points at, head-up flip included.
fn index_from_displayed_fraction(fraction: f32, axis_len: usize, flipped: bool) -> usize {
    let fraction = if flipped { 1.0 - fraction } else { fraction };

    index_from_fraction(fraction, axis_len)
}

/// Where the centre of `index` is drawn along a view axis.
fn position_at_index(
    index: usize,
    range: RangeInclusive<f32>,
    axis_len: usize,
    flipped: bool,
) -> f32 {
    egui::lerp(range, displayed_fraction(index, axis_len, flipped))
}

/// The index a pointer coordinate points at, clamped to the axis.
fn index_at_position(
    position: f32,
    range: RangeInclusive<f32>,
    axis_len: usize,
    flipped: bool,
) -> usize {
    let start = *range.start();
    let span = *range.end() - start;
    let fraction = if span > 0.0 {
        (position - start) / span
    } else {
        0.0
    };

    index_from_displayed_fraction(fraction.clamp(0.0, 1.0), axis_len, flipped)
}

/// Fit an image of the given physical size into `available`, centred and
/// preserving its millimetre aspect ratio.
fn fit_physical_aspect(
    available: egui::Rect,
    physical_width: f32,
    physical_height: f32,
) -> egui::Rect {
    if available.width() <= 0.0 || available.height() <= 0.0 {
        return egui::Rect::from_center_size(available.center(), egui::Vec2::ZERO);
    }

    let image_ratio = physical_width / physical_height;
    let available_ratio = available.width() / available.height();
    let size = if image_ratio > available_ratio {
        egui::vec2(available.width(), available.width() / image_ratio)
    } else {
        egui::vec2(available.height() * image_ratio, available.height())
    };

    egui::Rect::from_center_size(available.center(), size)
}

fn upload_plane_texture(
    context: &egui::Context,
    axis: VolumeAxis,
    plane: &PlaneImage,
    window: WindowLevel,
    inverted: bool,
) -> egui::TextureHandle {
    let image = egui::ColorImage::from_gray_iter(
        [plane.columns, plane.rows],
        plane
            .pixels
            .iter()
            .map(|value| windowed_gray(*value, window, inverted)),
    );

    context.load_texture(
        plane_texture_name(axis),
        image,
        egui::TextureOptions::LINEAR,
    )
}

/// The whole factors to reduce a plane's columns and rows by, so that it is
/// uploaded at no fewer texels than the physical pixels it is drawn over.
fn plane_reduction(
    volume: &Volume,
    axis: VolumeAxis,
    transposed: bool,
    image_size_px: egui::Vec2,
    max_texture_side: usize,
) -> [usize; 2] {
    let (column_axis, row_axis) = axis.plane_axes();
    // A transposed view draws the plane's columns down the screen.
    let [columns_px, rows_px] = if transposed {
        [image_size_px.y, image_size_px.x]
    } else {
        [image_size_px.x, image_size_px.y]
    };

    [(column_axis, columns_px), (row_axis, rows_px)].map(|(plane_axis, pixels)| {
        let axis_len = volume.axis_len(plane_axis);
        let factor = axis_len as f32 / pixels.max(1.0);
        // However large the view, the texture must fit the device.
        let fitting_factor = axis_len.div_ceil(max_texture_side.max(1));

        (factor.floor() as usize).max(fitting_factor).max(1)
    })
}

/// The plane shrunk by whole factors, each texel standing for a block of
/// plane pixels: their mean for a slice, and their maximum for a projection,
/// which keeps a vessel narrower than a block at full brightness.
fn reduced_plane(plane: PlaneImage, factors: [usize; 2], mode: MprMode) -> PlaneImage {
    let [column_factor, row_factor] = factors.map(|factor| factor.max(1));
    if [column_factor, row_factor] == [1, 1] {
        return plane;
    }

    let columns = plane.columns.div_ceil(column_factor);
    let rows = plane.rows.div_ceil(row_factor);
    let mut pixels = Vec::with_capacity(columns * rows);

    for row in 0..rows {
        let block_rows = row * row_factor..((row + 1) * row_factor).min(plane.rows);

        for column in 0..columns {
            let block_columns =
                column * column_factor..((column + 1) * column_factor).min(plane.columns);
            let block = block_rows.clone().flat_map(|block_row| {
                plane.pixels[block_row * plane.columns..][block_columns.clone()]
                    .iter()
                    .copied()
            });

            pixels.push(match mode {
                MprMode::Slice => {
                    let (sum, count) = block.fold((0.0_f64, 0_usize), |(sum, count), value| {
                        (sum + f64::from(value), count + 1)
                    });
                    (sum / count.max(1) as f64) as f32
                }
                MprMode::MipSlab => block.fold(f32::NEG_INFINITY, f32::max),
            });
        }
    }

    PlaneImage {
        columns,
        rows,
        pixels,
    }
}

/// The grey a value shows in under a window. A MONOCHROME1 series shows its
/// lowest values brightest, so its grey is inverted after windowing, as the
/// 2D viewer does it.
fn windowed_gray(value: f32, window: WindowLevel, inverted: bool) -> u8 {
    let width = window.width.max(1.0);
    let lowest = window.center - width / 2.0;
    let gray = ((f64::from(value).clamp(lowest, lowest + width) - lowest) * 255.0 / width) as u8;

    if inverted { u8::MAX - gray } else { gray }
}

fn plane_texture_name(axis: VolumeAxis) -> &'static str {
    match axis {
        VolumeAxis::X => "mpr-plane-x",
        VolumeAxis::Y => "mpr-plane-y",
        VolumeAxis::Z => "mpr-plane-z",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::app::ui::overlay::transformed_image_uv;
    use crate::dicom::Volume;

    const IMAGE_RANGE: RangeInclusive<f32> = 100.0..=200.0;

    fn geometry() -> VolumeGeometry {
        VolumeGeometry {
            inverted: false,
            row_direction: [1.0, 0.0, 0.0],
            column_direction: [0.0, 1.0, 0.0],
            slice_direction: [0.0, 0.0, 1.0],
            slice_thickness_mm: Some(1.0),
            slice_spacing_mm: 1.0,
            slice_count: 8,
            uneven_slice_offsets_mm: None,
        }
    }

    /// A workspace on screen, with a volume big enough to step through.
    fn app_with_workspace() -> DicronApp {
        use crate::app::state::{ActiveVolumeView, ThreeDMode, ViewMode};
        use crate::app::volume_job::PreparedVolume;
        use crate::dicom::SeriesVolume;

        let volume = Arc::new(Volume::new(4, 4, 8, [1.0, 1.0, 1.0], vec![0.0; 4 * 4 * 8]));
        let mut app = DicronApp::default();
        app.index_in_memory_series(2);
        let (series_key, fingerprint) = app.select_in_memory_series(0);

        app.three_d.view_mode = ViewMode::ThreeD(ThreeDMode::Mpr);
        app.three_d.active = Some(ActiveVolumeView {
            series_key,
            fingerprint,
            series_overlay: None,
            prepared: PreparedVolume {
                series: Arc::new(SeriesVolume {
                    volume: Arc::clone(&volume),
                    geometry: geometry(),
                }),
                mpr: Arc::clone(&volume),
                gpu: None,
            },
            low_resolution: false,
            mpr: MprState::new(
                volume,
                WindowLevel {
                    center: 40.0,
                    width: 350.0,
                },
            ),
            render: crate::app::three_d_state::VolumeRenderState::default(),
            gpu_budget: crate::app::three_d_state::GpuBudget::FULL,
        });

        app
    }

    /// Run one frame of the workspace with the given input, and report the
    /// plane indices it leaves behind.
    fn run_workspace_pass(
        context: &egui::Context,
        app: &mut DicronApp,
        events: Vec<egui::Event>,
    ) -> [usize; 3] {
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            events,
            ..Default::default()
        };

        let _ = context.run_ui(raw_input, |ui| crate::app::ui::three_d::show_body(app, ui));

        app.three_d.active.as_ref().unwrap().mpr.indices
    }

    fn wheel_over_workspace(app: &mut DicronApp, pointer: egui::Pos2, lines: f32) -> [usize; 3] {
        run_workspace_pass(
            &egui::Context::default(),
            app,
            vec![
                egui::Event::PointerMoved(pointer),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, lines),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        )
    }

    /// A plane drawn at a quarter of its size is reduced four-fold, and a line
    /// one pixel wide survives it: dimmed by the mean of a slice, at full
    /// value in the maximum of a projection.
    #[test]
    fn a_minified_plane_keeps_its_one_pixel_lines() {
        let volume = Volume::new(400, 3, 2, [1.0; 3], vec![0.0; 400 * 3 * 2]);
        let reduction =
            plane_reduction(&volume, VolumeAxis::Z, false, egui::vec2(100.0, 3.0), 2048);
        assert_eq!(reduction, [4, 1]);
        assert_eq!(
            plane_reduction(&volume, VolumeAxis::Z, false, egui::vec2(800.0, 6.0), 2048),
            [1, 1]
        );
        assert_eq!(
            plane_reduction(&volume, VolumeAxis::Z, true, egui::vec2(3.0, 100.0), 2048),
            [4, 1]
        );
        // A plane wider than the device's largest texture is reduced to fit,
        // even where the view has room for every column.
        assert_eq!(
            plane_reduction(&volume, VolumeAxis::Z, false, egui::vec2(800.0, 6.0), 128),
            [4, 1]
        );

        let line = || PlaneImage {
            columns: 400,
            rows: 3,
            pixels: (0..400 * 3)
                .map(|index| if index % 400 == 201 { 1000.0 } else { 0.0 })
                .collect(),
        };

        let slice = reduced_plane(line(), reduction, MprMode::Slice);
        let projection = reduced_plane(line(), reduction, MprMode::MipSlab);

        assert_eq!((slice.columns, slice.rows), (100, 3));
        assert_eq!(slice.pixels[50], 250.0);
        assert_eq!(projection.pixels[50], 1000.0);
        assert!(
            slice
                .pixels
                .iter()
                .enumerate()
                .all(|(index, value)| index % 100 == 50 || *value == 0.0)
        );
    }

    #[test]
    fn inverted_series_show_their_greys_the_other_way_round() {
        let window = WindowLevel {
            center: 100.0,
            width: 200.0,
        };

        assert_eq!(windowed_gray(0.0, window, false), 0);
        assert_eq!(windowed_gray(200.0, window, false), 255);
        assert_eq!(windowed_gray(0.0, window, true), 255);
        assert_eq!(windowed_gray(200.0, window, true), 0);
        // A non-zero lower edge, so a formula that drops it cannot pass.
        let raised = WindowLevel {
            center: 1100.0,
            width: 200.0,
        };
        assert_eq!(windowed_gray(1000.0, raised, false), 0);
        assert_eq!(windowed_gray(1100.0, raised, false), 127);
    }

    /// Values beyond the window clamp to its ends, and a window narrower than
    /// one unit is treated as one unit wide rather than dividing by zero.
    #[test]
    fn plane_greys_clamp_to_the_window_and_floor_its_width() {
        let window = WindowLevel {
            center: 1100.0,
            width: 200.0,
        };
        assert_eq!(windowed_gray(-5000.0, window, false), 0);
        assert_eq!(windowed_gray(5000.0, window, false), 255);
        assert_eq!(windowed_gray(-5000.0, window, true), 255);

        let collapsed = WindowLevel {
            center: 10.0,
            width: 0.0,
        };
        assert_eq!(windowed_gray(9.0, collapsed, false), 0);
        assert_eq!(windowed_gray(10.0, collapsed, false), 127);
        assert_eq!(windowed_gray(11.0, collapsed, false), 255);
    }

    /// One frame of the 3D body with no input, returning what it painted.
    fn paint_body(app: &mut DicronApp) -> Vec<String> {
        let context = egui::Context::default();
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..Default::default()
        };
        let output = context.run_ui(raw_input, |ui| crate::app::ui::three_d::show_body(app, ui));

        super::super::painted_texts(&output)
    }

    /// While another series is selected, the workspace being replaced is
    /// neither shown nor moved: its images would be read as the new series'.
    #[test]
    fn a_workspace_of_another_series_is_not_shown() {
        let mut app = app_with_workspace();
        app.select_in_memory_series(1);

        let texts = paint_body(&mut app);
        let indices = wheel_over_workspace(&mut app, egui::pos2(120.0, 300.0), -1.0);

        assert_eq!(texts, [super::super::PREPARING_VOLUME_MESSAGE]);
        assert_eq!(indices, [2, 2, 4]);
    }

    /// The views name the patient, study and series the workspace was built
    /// from.
    #[test]
    fn the_views_name_their_series() {
        let mut app = app_with_workspace();
        app.three_d.active.as_mut().unwrap().series_overlay =
            Some(crate::dicom::DicomOverlayMetadata {
                patient_label: Some("Test Patient".to_owned()),
                study_description: Some("Test Study".to_owned()),
                series_description: Some("Test Series".to_owned()),
                ..Default::default()
            });

        let texts = paint_body(&mut app);

        for line in ["Test Patient", "Test Study", "Test Series"] {
            assert!(texts.iter().any(|text| text == line), "{line}: {texts:?}");
        }
    }

    /// A dialog over the views takes the wheel: the plane underneath stays
    /// where it is.
    #[test]
    fn the_wheel_over_a_dialog_leaves_the_planes_alone() {
        let mut app = app_with_workspace();
        let context = egui::Context::default();
        let before = app.three_d.active.as_ref().unwrap().mpr.indices;
        let pointer = egui::Event::PointerMoved(egui::pos2(120.0, 300.0));
        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -1.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        };

        // The first frame lays the dialog out, the second wheels over it.
        for events in [vec![pointer.clone()], vec![pointer, wheel]] {
            let raw_input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let _ = context.run_ui(raw_input, |ui| {
                crate::app::ui::three_d::show_body(&mut app, ui);
                egui::Modal::new(egui::Id::new("a dialog")).show(ui.ctx(), |ui| {
                    ui.label("A dialog");
                });
            });
        }

        assert_eq!(app.three_d.active.as_ref().unwrap().mpr.indices, before);
    }

    #[test]
    fn the_wheel_steps_the_axis_of_the_view_it_is_over() {
        let mut app = app_with_workspace();

        // The left view is axial, so a notch down steps its own axis, towards
        // the feet: the slices stack towards the head, so the index falls.
        assert_eq!(
            wheel_over_workspace(&mut app, egui::pos2(120.0, 300.0), -1.0),
            [2, 2, 3]
        );

        // The middle view is coronal; down steps towards the back.
        assert_eq!(
            wheel_over_workspace(&mut app, egui::pos2(450.0, 300.0), -1.0),
            [2, 3, 3]
        );

        // Nothing moves while the pointer is outside every view.
        assert_eq!(
            wheel_over_workspace(&mut app, egui::pos2(450.0, 900.0), -1.0),
            [2, 3, 3]
        );
    }

    #[test]
    fn a_press_inside_a_view_moves_the_two_other_planes() {
        let mut app = app_with_workspace();
        let context = egui::Context::default();
        let press = egui::pos2(20.0, 180.0);

        // The first pass registers the views, so the press of the second one
        // can land on the axial view.
        run_workspace_pass(&context, &mut app, vec![egui::Event::PointerMoved(press)]);
        let indices = run_workspace_pass(
            &context,
            &mut app,
            vec![egui::Event::PointerButton {
                pos: press,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
        );

        // The axial view sets the sagittal and coronal planes, near its own
        // top left corner, and leaves its own plane alone.
        assert_eq!(indices, [0, 0, 4]);
        assert_eq!(
            app.three_d.active.as_ref().unwrap().mpr.drag,
            Some(MprDrag::Crosshair {
                view: VolumeAxis::Z
            })
        );
    }

    /// A tap whose press and release arrive in the same frame moves the
    /// crosshair as a held press does.
    #[test]
    fn a_tap_within_one_frame_moves_the_two_other_planes() {
        let mut app = app_with_workspace();
        let context = egui::Context::default();
        let tap = egui::pos2(20.0, 180.0);
        let button = |pressed| egui::Event::PointerButton {
            pos: tap,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };

        run_workspace_pass(&context, &mut app, vec![egui::Event::PointerMoved(tap)]);
        let indices = run_workspace_pass(&context, &mut app, vec![button(true), button(false)]);

        assert_eq!(indices, [0, 0, 4]);
        assert_eq!(app.three_d.active.as_ref().unwrap().mpr.drag, None);
    }

    /// However thin a slab is, a press on the crosshair moves the crosshair,
    /// and a press on a boundary grabs it without changing the slab until the
    /// pointer moves.
    #[test]
    fn a_press_on_the_crosshair_never_collapses_the_slab() {
        // One voxel per 100 points, one-slice slabs: the sagittal slab is
        // column 2, between faces at x = 200 and x = 300, around the crosshair
        // line at x = 250.
        let mut mpr = test_state();
        mpr.set_slab_thickness_mm(VolumeAxis::X, 1.0);
        mpr.set_slab_thickness_mm(VolumeAxis::Y, 1.0);
        let image_rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(400.0, 600.0));
        let layout = geometry().plane_layout(VolumeAxis::Z);

        assert_eq!(
            grabbed_boundary(&mpr, layout, image_rect, egui::pos2(252.0, 352.0)),
            None
        );
        assert_eq!(
            grabbed_boundary(&mpr, layout, image_rect, egui::pos2(270.0, 360.0)),
            None
        );

        let press = egui::pos2(298.0, 450.0);
        let (axis, grab_offset) = grabbed_boundary(&mpr, layout, image_rect, press).unwrap();
        assert_eq!((axis, grab_offset), (VolumeAxis::X, -2.0));

        resize_slab(&mut mpr, layout, axis, image_rect, press, grab_offset);
        assert_eq!(mpr.slab_half_count(VolumeAxis::X), 0);

        // One voxel further out takes a slice on each side.
        resize_slab(
            &mut mpr,
            layout,
            axis,
            image_rect,
            egui::pos2(398.0, 450.0),
            grab_offset,
        );
        assert_eq!(mpr.slab_half_count(VolumeAxis::X), 1);
    }

    /// Press `button` at `pointer` on one frame, then move by `delta` on the
    /// next, so the drag is latched before it moves.
    fn drag_in_workspace(
        app: &mut DicronApp,
        button: egui::PointerButton,
        pointer: egui::Pos2,
        delta: egui::Vec2,
    ) {
        let context = egui::Context::default();

        run_workspace_pass(&context, app, vec![egui::Event::PointerMoved(pointer)]);
        run_workspace_pass(
            &context,
            app,
            vec![egui::Event::PointerButton {
                pos: pointer,
                button,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        run_workspace_pass(
            &context,
            app,
            vec![egui::Event::PointerMoved(pointer + delta)],
        );
    }

    #[test]
    fn a_right_drag_down_zooms_only_the_dragged_view() {
        let mut app = app_with_workspace();

        drag_in_workspace(
            &mut app,
            egui::PointerButton::Secondary,
            egui::pos2(120.0, 300.0),
            egui::vec2(0.0, 60.0),
        );

        let mpr = &app.three_d.active.as_ref().unwrap().mpr;
        assert!(mpr.view_transforms[VolumeAxis::Z.index()].zoom > 1.0);
        assert_eq!(mpr.view_transforms[VolumeAxis::Y.index()].zoom, 1.0);
        assert_eq!(mpr.view_transforms[VolumeAxis::X.index()].zoom, 1.0);
        // The crosshair is untouched by a zoom.
        assert_eq!(mpr.indices, [2, 2, 4]);
    }

    #[test]
    fn a_middle_drag_pans_only_the_dragged_view() {
        let mut app = app_with_workspace();

        drag_in_workspace(
            &mut app,
            egui::PointerButton::Middle,
            egui::pos2(450.0, 300.0),
            egui::vec2(-30.0, 15.0),
        );

        let mpr = &app.three_d.active.as_ref().unwrap().mpr;
        assert_eq!(
            mpr.view_transforms[VolumeAxis::Y.index()].pan,
            egui::vec2(-30.0, 15.0)
        );
        assert_eq!(
            mpr.view_transforms[VolumeAxis::Z.index()].pan,
            egui::Vec2::ZERO
        );
        assert_eq!(mpr.indices, [2, 2, 4]);
    }

    #[test]
    fn the_low_resolution_prompt_freezes_the_views_underneath_it() {
        use crate::app::state::{LowResolutionPrompt, ThreeDMode};
        use crate::app::volume_job::PreparedVolume;
        use crate::dicom::SeriesVolume;

        let mut app = app_with_workspace();
        let volume = Arc::new(Volume::new(2, 2, 2, [4.0, 4.0, 4.0], vec![0.0; 8]));
        app.three_d.low_resolution_prompt = Some(LowResolutionPrompt {
            prepared: PreparedVolume {
                series: Arc::new(SeriesVolume {
                    volume: Arc::clone(&volume),
                    geometry: geometry(),
                }),
                mpr: volume,
                gpu: None,
            },
            series_key: (0, 0, 1),
            fingerprint: 1,
            mode: ThreeDMode::Mpr,
            cause: "slices 3 mm or thicker and as far apart".to_owned(),
        });

        assert_eq!(
            wheel_over_workspace(&mut app, egui::pos2(120.0, 300.0), -1.0),
            [2, 2, 4]
        );
    }

    fn test_state() -> MprState {
        MprState::new(
            Arc::new(Volume::new(4, 6, 8, [1.0, 1.0, 2.0], vec![0.0; 4 * 6 * 8])),
            WindowLevel {
                center: 40.0,
                width: 350.0,
            },
        )
    }

    #[test]
    fn every_index_maps_to_its_own_cell_and_back() {
        for axis_len in [1_usize, 2, 7, 394] {
            for index in 0..axis_len {
                for flipped in [false, true] {
                    let fraction = displayed_fraction(index, axis_len, flipped);

                    assert!((0.0..=1.0).contains(&fraction), "{fraction} out of range");
                    assert_eq!(
                        index_from_displayed_fraction(fraction, axis_len, flipped),
                        index,
                        "axis_len {axis_len}, index {index}, flipped {flipped}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_single_slice_axis_stays_centered() {
        assert_eq!(displayed_fraction(0, 1, false), 0.5);
        assert_eq!(displayed_fraction(0, 1, true), 0.5);
        assert_eq!(index_from_displayed_fraction(0.0, 1, false), 0);
        assert_eq!(index_from_displayed_fraction(1.0, 1, true), 0);
        assert_eq!(index_from_fraction(f32::NAN, 0), 0);
    }

    #[test]
    fn a_flip_mirrors_the_axis() {
        assert_eq!(
            displayed_fraction(0, 4, true),
            displayed_fraction(3, 4, false)
        );
        assert_eq!(index_from_displayed_fraction(0.1, 4, true), 3);
    }

    #[test]
    fn a_crosshair_dragged_past_the_image_clamps_to_its_edges() {
        assert_eq!(index_at_position(-40.0, IMAGE_RANGE, 8, false), 0);
        assert_eq!(index_at_position(1_000.0, IMAGE_RANGE, 8, false), 7);
        assert_eq!(index_at_position(-40.0, IMAGE_RANGE, 8, true), 7);
        assert_eq!(index_at_position(1_000.0, IMAGE_RANGE, 8, true), 0);
        // A collapsed image maps everything to the first slice.
        assert_eq!(index_at_position(150.0, 150.0..=150.0, 8, false), 0);
    }

    #[test]
    fn indices_and_screen_positions_agree() {
        let position = position_at_index(2, IMAGE_RANGE, 8, false);

        assert_eq!(position, 131.25);
        assert_eq!(index_at_position(position, IMAGE_RANGE, 8, false), 2);
    }

    #[test]
    fn only_a_press_near_a_dashed_boundary_grabs_a_slab() {
        // A 4 x 6 x 8 volume with 3 mm slabs, one voxel per 100 points: in the
        // axial view the sagittal slab takes columns 1 to 3, between faces at
        // x = 100 and x = 400, and the coronal slab rows 2 to 4, between faces
        // at y = 200 and y = 500.
        let mut mpr = test_state();
        mpr.set_slab_thickness_mm(VolumeAxis::X, 3.0);
        mpr.set_slab_thickness_mm(VolumeAxis::Y, 3.0);
        let image_rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(400.0, 600.0));
        let layout = geometry().plane_layout(VolumeAxis::Z);
        let grabbed =
            |pointer| grabbed_boundary(&mpr, layout, image_rect, pointer).map(|(axis, _)| axis);

        assert_eq!(grabbed(egui::pos2(106.0, 350.0)), Some(VolumeAxis::X));
        assert_eq!(grabbed(egui::pos2(250.0, 495.0)), Some(VolumeAxis::Y));
        assert_eq!(grabbed(egui::pos2(250.0, 350.0)), None);
    }

    #[test]
    fn a_dragged_boundary_spans_both_sides_of_the_center() {
        // Face 8 closes slice 7 and face 1 opens slice 1: three slices on
        // either side of slice 4.
        assert_eq!(dragged_slab_thickness_mm(8, 4, 2.0), 14.0);
        assert_eq!(dragged_slab_thickness_mm(1, 4, 2.0), 14.0);
        // The faces of the centre slice itself leave that slice alone.
        assert_eq!(dragged_slab_thickness_mm(5, 4, 2.0), 2.0);
        assert_eq!(dragged_slab_thickness_mm(4, 4, 2.0), 2.0);
    }

    /// The dashed lines enclose exactly the slices the projection takes, and
    /// a press on one picks the face it is drawn at, so it resizes nothing.
    #[test]
    fn slab_faces_enclose_the_projected_slices() {
        let range = 0.0..=800.0;

        assert_eq!(
            slab_face_positions(3, 5, range.clone(), 8, false),
            [300.0, 600.0]
        );
        assert_eq!(
            slab_face_positions(3, 5, range.clone(), 8, true),
            [500.0, 200.0]
        );
        for (position, flipped) in [(300.0, false), (600.0, false), (500.0, true)] {
            let face = face_at_position(position, range.clone(), 8, flipped);
            assert_eq!(dragged_slab_thickness_mm(face, 4, 2.0), 6.0, "{position}");
        }
    }

    #[test]
    fn slab_boundaries_stay_inside_the_axis() {
        assert_eq!(slab_boundary_indices(4, 2, 8), (2, 6));
        assert_eq!(slab_boundary_indices(0, 5, 8), (0, 5));
        assert_eq!(slab_boundary_indices(7, 5, 8), (2, 7));
        assert_eq!(slab_boundary_indices(0, 5, 0), (0, 0));
    }

    #[test]
    fn crosshair_colors_follow_the_plane_labels() {
        let geometry = geometry();

        for (axis, expected_color) in [
            (VolumeAxis::X, egui::Color32::GREEN),
            (VolumeAxis::Y, egui::Color32::YELLOW),
            (VolumeAxis::Z, egui::Color32::RED),
        ] {
            assert_eq!(axis_color(&geometry, axis), expected_color);
            assert_eq!(
                geometry.plane_label(axis),
                match axis {
                    VolumeAxis::X => "Sagittal",
                    VolumeAxis::Y => "Coronal",
                    VolumeAxis::Z => "Axial",
                }
            );
        }
    }

    #[test]
    fn each_view_draws_the_two_other_axes() {
        let geometry = geometry();

        for axis in VolumeAxis::ALL {
            let lines = view_lines(geometry.plane_layout(axis));

            assert!(lines[0].vertical);
            assert!(!lines[1].vertical);
            assert_ne!(lines[0].axis, axis);
            assert_ne!(lines[1].axis, axis);
            assert_ne!(lines[0].axis, lines[1].axis);
        }
    }

    /// The four edge markers of a view, as `[left, right, top, bottom]`.
    fn edge_markers(geometry: &VolumeGeometry, axis: VolumeAxis) -> [&'static str; 4] {
        let layout = geometry.plane_layout(axis);

        [
            layout.horizontal.near_label(),
            layout.horizontal.far_label(),
            layout.vertical.near_label(),
            layout.vertical.far_label(),
        ]
    }

    /// Whatever the acquisition, every view must come out the standard way
    /// round: patient left to the right, the head up, anterior at the top of an
    /// axial view and at the left of a sagittal one.
    fn assert_standard_presentation(geometry: &VolumeGeometry) {
        for axis in VolumeAxis::ALL {
            let expected = match geometry.plane_label(axis) {
                "Axial" => ["R", "L", "A", "P"],
                "Coronal" => ["R", "L", "H", "F"],
                _ => ["A", "P", "H", "F"],
            };

            assert_eq!(
                edge_markers(geometry, axis),
                expected,
                "{} view of {geometry:?}",
                geometry.plane_label(axis)
            );
        }
    }

    fn geometry_with(
        row_direction: [f64; 3],
        column_direction: [f64; 3],
        slice_direction: [f64; 3],
    ) -> VolumeGeometry {
        VolumeGeometry {
            inverted: false,
            row_direction,
            column_direction,
            slice_direction,
            slice_thickness_mm: Some(1.0),
            slice_spacing_mm: 1.0,
            slice_count: 8,
            uneven_slice_offsets_mm: None,
        }
    }

    #[test]
    fn an_axial_acquisition_is_shown_the_standard_way_round() {
        let geometry = geometry();

        assert_standard_presentation(&geometry);

        // Nothing is transposed, and only the two views whose rows run towards
        // the head are flipped, exactly as before the layout was generalised.
        assert!(!geometry.plane_layout(VolumeAxis::Z).transposed);
        assert!(!geometry.plane_layout(VolumeAxis::Z).vertical.flipped);
        assert!(geometry.plane_layout(VolumeAxis::Y).vertical.flipped);
        assert!(geometry.plane_layout(VolumeAxis::X).vertical.flipped);
    }

    #[test]
    fn a_prone_acquisition_is_unmirrored_rather_than_drawn_backwards() {
        // Prone CT: the rows run towards the patient's right and the columns
        // towards their anterior.
        let geometry = geometry_with([-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0]);

        assert_standard_presentation(&geometry);

        let axial = geometry.plane_layout(VolumeAxis::Z);
        assert!(axial.horizontal.flipped, "left-right must be unmirrored");
        assert!(axial.vertical.flipped, "anterior-posterior must be flipped");
    }

    #[test]
    fn a_sagittal_acquisition_is_transposed_instead_of_drawn_sideways() {
        // Sagittal MR: rows run posterior, columns inferior, slices to the
        // patient's right.
        let geometry = geometry_with([0.0, 1.0, 0.0], [0.0, 0.0, -1.0], [-1.0, 0.0, 0.0]);

        assert_standard_presentation(&geometry);

        // The head-to-foot axis is the plane's column axis in the axial and
        // coronal views, so both are transposed to stand the head up.
        assert!(!geometry.plane_layout(VolumeAxis::Z).transposed);
        assert!(geometry.plane_layout(VolumeAxis::Y).transposed);
        assert!(geometry.plane_layout(VolumeAxis::X).transposed);
    }

    #[test]
    fn a_coronal_acquisition_is_shown_the_standard_way_round() {
        // Coronal CT: rows run to the patient's left, columns inferior.
        let geometry = geometry_with([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]);

        assert_standard_presentation(&geometry);
    }

    #[test]
    fn the_views_are_always_ordered_axial_coronal_sagittal() {
        for geometry in [
            geometry(),
            geometry_with([-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0]),
            geometry_with([0.0, 1.0, 0.0], [0.0, 0.0, -1.0], [-1.0, 0.0, 0.0]),
            geometry_with([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ] {
            let labels = view_axes(&geometry).map(|axis| geometry.plane_label(axis));

            assert_eq!(labels, ["Axial", "Coronal", "Sagittal"]);
        }
    }

    /// Axial head first supine, prone, feet first, coronal and sagittal.
    fn acquisitions() -> [VolumeGeometry; 5] {
        [
            geometry(),
            geometry_with([-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0]),
            geometry_with([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]),
            geometry_with([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
            geometry_with([0.0, 1.0, 0.0], [0.0, 0.0, -1.0], [-1.0, 0.0, 0.0]),
        ]
    }

    /// The patient direction, DICOM's +x left, +y posterior, +z head, that
    /// volume `axis` increases towards, read straight from the cosines.
    fn cosines(geometry: &VolumeGeometry, axis: VolumeAxis) -> [f64; 3] {
        match axis {
            VolumeAxis::X => geometry.row_direction,
            VolumeAxis::Y => geometry.column_direction,
            VolumeAxis::Z => geometry.slice_direction,
        }
    }

    /// The texel a screen position over `image_rect` shows in the view of
    /// `axis`, as volume indices along the plane's column and row axes: the
    /// path the drawn texture takes, through the display transform.
    fn texel_under(
        geometry: &VolumeGeometry,
        volume: &Volume,
        axis: VolumeAxis,
        image_rect: egui::Rect,
        pointer: egui::Pos2,
    ) -> [usize; 2] {
        let screen = (pointer - image_rect.min) / image_rect.size();
        let (u, v) = transformed_image_uv(
            screen.x,
            screen.y,
            display_transform(geometry.plane_layout(axis)),
        );
        let (column_axis, row_axis) = axis.plane_axes();

        [
            index_from_fraction(u, volume.axis_len(column_axis)),
            index_from_fraction(v, volume.axis_len(row_axis)),
        ]
    }

    /// What each view shows across and down, in patient directions, followed
    /// from screen through the display transform and the plane's axes to the
    /// direction cosines. The edge letters come from the layout itself, so
    /// they cannot see a reformat drawn mirrored or upside down; this can.
    #[test]
    fn every_view_runs_left_or_posterior_across_and_footwards_down() {
        for geometry in acquisitions() {
            for axis in VolumeAxis::ALL {
                let transform = display_transform(geometry.plane_layout(axis));
                let (column_axis, row_axis) = axis.plane_axes();
                let towards = |from: (f32, f32), to: (f32, f32)| -> [i32; 3] {
                    let (from_u, from_v) = transformed_image_uv(from.0, from.1, transform);
                    let (to_u, to_v) = transformed_image_uv(to.0, to.1, transform);
                    std::array::from_fn(|component| {
                        let along = f64::from(to_u - from_u)
                            * cosines(&geometry, column_axis)[component]
                            + f64::from(to_v - from_v) * cosines(&geometry, row_axis)[component];
                        along.signum() as i32 * i32::from(along.abs() > 1.0e-6)
                    })
                };
                let across = towards((0.25, 0.5), (0.75, 0.5));
                let down = towards((0.5, 0.25), (0.5, 0.75));
                let normal = cosines(&geometry, axis).map(f64::abs);
                let expected = if normal[2] > 0.5 {
                    // Axial: patient left to the right, posterior down.
                    ([1, 0, 0], [0, 1, 0])
                } else if normal[1] > 0.5 {
                    // Coronal: patient left to the right, feet down.
                    ([1, 0, 0], [0, 0, -1])
                } else {
                    // Sagittal: posterior to the right, feet down.
                    ([0, 1, 0], [0, 0, -1])
                };

                assert_eq!((across, down), expected, "{axis:?} view of {geometry:?}");
            }
        }
    }

    /// A press picks the slices whose texel is drawn under the pointer, and
    /// each crosshair line is drawn over the texel of the slice it marks, in
    /// every view of every acquisition.
    #[test]
    fn a_press_picks_the_texel_drawn_under_it_and_the_lines_follow() {
        let image_rect =
            egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(120.0, 90.0));
        let pointers = [
            egui::pos2(112.0, 130.0),
            egui::pos2(205.0, 60.0),
            egui::pos2(160.0, 95.0),
        ];

        for geometry in acquisitions() {
            for axis in VolumeAxis::ALL {
                let layout = geometry.plane_layout(axis);
                let (column_axis, row_axis) = axis.plane_axes();

                for pointer in pointers {
                    let mut mpr = test_state();
                    move_crosshair(&mut mpr, layout, image_rect, pointer);

                    let texel = texel_under(&geometry, &mpr.volume, axis, image_rect, pointer);
                    assert_eq!(
                        [mpr.index(column_axis), mpr.index(row_axis)],
                        texel,
                        "{axis:?} view of {geometry:?} at {pointer:?}"
                    );

                    for line in view_lines(layout) {
                        let position = position_at_index(
                            mpr.index(line.axis),
                            line.range(image_rect),
                            mpr.volume.axis_len(line.axis),
                            line.flipped,
                        );
                        let on_line = if line.vertical {
                            egui::pos2(position, image_rect.center().y)
                        } else {
                            egui::pos2(image_rect.center().x, position)
                        };
                        let texel = texel_under(&geometry, &mpr.volume, axis, image_rect, on_line);
                        let drawn = if line.axis == column_axis {
                            texel[0]
                        } else {
                            texel[1]
                        };

                        assert_eq!(
                            drawn,
                            mpr.index(line.axis),
                            "{:?} line in the {axis:?} view of {geometry:?}",
                            line.axis
                        );
                    }
                }
            }
        }
    }

    /// A sagittal series of 60 x 240 x 320 mm keeps each view's millimetre
    /// aspect once its transposed views swap their axes.
    #[test]
    fn transposed_views_keep_their_millimetre_aspect() {
        let geometry = geometry_with([0.0, 1.0, 0.0], [0.0, 0.0, -1.0], [-1.0, 0.0, 0.0]);
        let mpr = MprState::new(
            Arc::new(Volume::new(6, 24, 32, [10.0; 3], vec![0.0; 6 * 24 * 32])),
            WindowLevel {
                center: 40.0,
                width: 350.0,
            },
        );
        // Left-right spans 320 mm, anterior-posterior 60 and head-foot 240.
        for (axis, expected_aspect) in [
            (VolumeAxis::Y, 320.0 / 60.0),
            (VolumeAxis::X, 320.0 / 240.0),
            (VolumeAxis::Z, 60.0 / 240.0),
        ] {
            let image_rect = view_image_rect(
                &mpr,
                &MprViewParams {
                    geometry: &geometry,
                    axis,
                    layout: geometry.plane_layout(axis),
                    mode: MprMode::Slice,
                    view_rect: egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    ),
                },
            )
            .unwrap();
            let aspect = image_rect.width() / image_rect.height();

            assert!(
                (aspect / expected_aspect - 1.0).abs() < 0.01,
                "{} view: {aspect} instead of {expected_aspect}",
                geometry.plane_label(axis)
            );
        }
    }

    /// A transposed view must read its texture with the plane's columns running
    /// down the screen; a plain one must leave them across it.
    #[test]
    fn the_display_transform_swaps_the_axes_of_a_transposed_view() {
        let plain = display_transform(geometry().plane_layout(VolumeAxis::Z));

        assert_eq!(transformed_image_uv(0.25, 0.75, plain), (0.25, 0.75));

        let sagittal_series = geometry_with([0.0, 1.0, 0.0], [0.0, 0.0, -1.0], [-1.0, 0.0, 0.0]);
        let layout = sagittal_series.plane_layout(VolumeAxis::Y);
        let transposed = display_transform(layout);
        let (u, v) = transformed_image_uv(0.25, 0.75, transposed);
        // Screen across reads the plane's rows, screen down reads its columns.
        let expected_u = if layout.vertical.flipped { 0.25 } else { 0.75 };
        let expected_v = if layout.horizontal.flipped {
            0.75
        } else {
            0.25
        };

        assert!((u - expected_u).abs() < 1.0e-6, "{u} {v}");
        assert!((v - expected_v).abs() < 1.0e-6, "{u} {v}");
    }

    #[test]
    fn the_slab_label_names_every_plane() {
        let mut mpr = test_state();
        // 3, 5 and 7 mm on 1, 1 and 2 mm slices: three, five and three slices,
        // so the 7 mm axial slab covers 6 mm.
        mpr.slab_mm = [3.0, 5.0, 7.0];

        assert_eq!(
            slab_thickness_label(&mpr, &geometry()),
            "Slab thickness  A: 6.0 mm   C: 5.0 mm   S: 3.0 mm"
        );

        // A slab running past the end of the volume names what it covers.
        mpr.slab_mm[VolumeAxis::X.index()] = 12.0;
        assert!(slab_thickness_label(&mpr, &geometry()).ends_with("S: 4.0 mm"));
    }

    #[test]
    fn a_wide_plane_fills_the_width_and_keeps_its_millimetre_aspect() {
        let available = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 200.0));

        let fitted = fit_physical_aspect(available, 100.0, 50.0);

        assert_eq!(fitted.size(), egui::vec2(200.0, 100.0));
        assert_eq!(fitted.center(), available.center());
    }

    #[test]
    fn a_tall_plane_fills_the_height() {
        let available = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0));

        let fitted = fit_physical_aspect(available, 50.0, 100.0);

        assert_eq!(fitted.size(), egui::vec2(50.0, 100.0));
        assert_eq!(fitted.center(), available.center());
    }

    #[test]
    fn an_empty_area_fits_nothing() {
        let empty = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(0.0, 40.0));

        assert_eq!(
            fit_physical_aspect(empty, 10.0, 10.0).size(),
            egui::Vec2::ZERO
        );
    }
}
