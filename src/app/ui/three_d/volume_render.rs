//! GPU volume rendering: the egui-wgpu callback, the resources it draws with,
//! and the camera interaction of the view itself.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use eframe::egui;
use eframe::egui_wgpu::{self, CallbackTrait, RenderState};
use eframe::wgpu;

use super::paint_centered_message;
use crate::app::DicronApp;
use crate::app::state::GpuInfo;
use crate::app::three_d_state::{
    GpuVolume, TRANSFER_FUNCTION_ENTRIES, TransferFunctionLut, VOLUME_WHEEL_ZOOM_SENSITIVITY,
    VolumeRenderState, VolumeUniforms,
};
use crate::app::ui::overlay::{
    OVERLAY_MARGIN, VIEWER_ZOOM_DRAG_SENSITIVITY, identification_overlay_text, paint_overlay_text,
};
use crate::dicom::{VolumeGeometry, dominant_patient_direction};

/// Shown when the device cannot build the rendering pipeline at all.
const FAILURE_MESSAGE: &str = "Volume rendering unavailable on this device.";
/// Shown when every smaller texture of this volume was refused as well.
const TOO_LARGE_MESSAGE: &str = "This volume needs more GPU memory than is available.";
/// The most one staging buffer of an upload holds. A volume is staged in
/// slabs of whole slices, so no buffer comes near the device's size limit.
const MAX_STAGING_BYTES: u64 = 32 << 20;

/// Lighting weights, from the source application's volume property.
const AMBIENT: f32 = 0.2;
const DIFFUSE: f32 = 0.7;
const SPECULAR: f32 = 0.2;
/// VTK's default specular power.
const SPECULAR_POWER: f32 = 10.0;
/// Opacity is authored per millimetre, like the source's scalar opacity unit
/// distance.
const OPACITY_UNIT_MM: f32 = 1.0;
/// Rays step half the finest voxel spacing, coarsened where needed so a ray
/// across the volume's diagonal takes at most `MAX_STEPS_PER_DIAGONAL` steps,
/// and doubled while the camera is dragged.
const STEPS_PER_VOXEL: f32 = 2.0;
const MAX_STEPS_PER_DIAGONAL: f32 = 1024.0;

const SHADER_SOURCE: &str = include_str!("volume_render.wgsl");
const BLIT_SHADER_SOURCE: &str = include_str!("volume_blit.wgsl");

/// The rays are marched into an image of at most this many pixels, which is
/// drawn onto the view scaled: a HiDPI view would otherwise cast a ray per
/// physical pixel, several times the work for little visible gain.
const MAX_RENDER_PIXELS: f32 = 1_500_000.0;
/// That image holds the colours as authored, in gamma space.
const RENDER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Read what the wgpu renderer offers, install the volume rendering resources
/// the first time a 3D mode is on screen, and pass what happened to the last
/// upload back to the app.
pub(in crate::app::ui) fn sync_render_state(
    app: &mut DicronApp,
    frame: &eframe::Frame,
    context: &egui::Context,
) {
    let Some(render_state) = frame.wgpu_render_state() else {
        app.three_d.gpu = None;
        app.three_d.gpu_available = false;
        return;
    };

    app.three_d.gpu_available = true;

    if app.three_d.gpu.is_none() {
        app.three_d.gpu = Some(GpuInfo {
            max_texture_dimension_3d: render_state.device.limits().max_texture_dimension_3d,
        });
    }

    // Only the active workspace's texture is worth keeping, in 2D as well:
    // another series' or a closed study's is released, whenever it changes.
    let wanted = app
        .three_d
        .active
        .as_ref()
        .and_then(|active| active.prepared.gpu.as_ref())
        .map(|gpu| gpu.id);
    if wanted != app.three_d.wanted_gpu_volume {
        app.three_d.wanted_gpu_volume = wanted;

        if let Some(resources) = render_state
            .renderer
            .write()
            .callback_resources
            .get_mut::<VolumeRenderResources>()
        {
            resources.release_unwanted(wanted);
        }
    }

    if app.three_d.view_mode.three_d_mode().is_none() {
        return;
    }

    install_resources(render_state, context);
    app.three_d.render_failure = render_failure(render_state);

    if let Some((volume_id, voxels)) = take_refused_upload(render_state) {
        app.receive_gpu_refusal(volume_id, voxels);
    }
}

fn install_resources(render_state: &RenderState, context: &egui::Context) {
    let mut renderer = render_state.renderer.write();

    if renderer
        .callback_resources
        .get::<VolumeRenderResources>()
        .is_none()
    {
        let resources = VolumeRenderResources::new(
            &render_state.device,
            render_state.target_format,
            context.clone(),
        );

        renderer.callback_resources.insert(resources);
    }
}

/// The upload the device refused since the last frame, if one was: the
/// volume and how many voxels it had.
fn take_refused_upload(render_state: &RenderState) -> Option<(u64, u64)> {
    // Most frames have nothing to take, and need only the shared lock.
    render_state
        .renderer
        .read()
        .callback_resources
        .get::<VolumeRenderResources>()?
        .refused_upload?;

    render_state
        .renderer
        .write()
        .callback_resources
        .get_mut::<VolumeRenderResources>()?
        .refused_upload
        .take()
}

fn render_failure(render_state: &RenderState) -> Option<String> {
    render_state
        .renderer
        .read()
        .callback_resources
        .get::<VolumeRenderResources>()
        .and_then(|resources| resources.failure.clone())
}

pub(super) fn show(app: &mut DicronApp, ui: &mut egui::Ui) {
    if !app.three_d.gpu_available {
        paint_centered_message(ui, super::GPU_UNAVAILABLE_MESSAGE);
        return;
    }

    if let Some(failure) = app.three_d.render_failure.as_deref() {
        paint_centered_message(ui, failure);
        return;
    }

    // The low-resolution warning owns the pointer while it is up.
    let interactive = app.three_d.low_resolution_prompt.is_none();
    let shows_selection = app.workspace_shows_selection();
    let Some(active) = app.three_d.active.as_mut().filter(|_| shows_selection) else {
        paint_centered_message(ui, super::PREPARING_VOLUME_MESSAGE);
        return;
    };
    if active.gpu_budget.exhausted() {
        paint_centered_message(ui, TOO_LARGE_MESSAGE);
        return;
    }
    let Some(volume) = active.prepared.gpu.clone() else {
        paint_centered_message(ui, crate::app::volume_job::PREPARING_GPU_MESSAGE);
        return;
    };

    let available_size = ui.available_size();
    let (view_rect, response) = ui.allocate_exact_size(
        egui::vec2(available_size.x.max(1.0), available_size.y.max(1.0)),
        egui::Sense::click_and_drag(),
    );

    ui.painter()
        .rect_filled(view_rect, 0.0, egui::Color32::BLACK);

    let extent_mm = volume.size_mm().into_iter().fold(0.0_f32, f32::max);

    if interactive {
        handle_camera_interaction(ui, &mut active.render, &response, view_rect, extent_mm);
    }

    let pixels_per_point = ui.ctx().pixels_per_point();
    let viewport_size_px = view_rect.size() * pixels_per_point;

    if viewport_size_px.x < 1.0 || viewport_size_px.y < 1.0 {
        return;
    }

    let transfer = TransferFunctionLut::bake(
        active.render.preset,
        active.mpr.window,
        active.prepared.series.geometry.inverted,
    );
    let uniforms = volume_uniforms(&CameraFrame {
        render: &active.render,
        volume: &volume,
        transfer: &transfer,
        geometry: &active.prepared.series.geometry,
        view_rect,
        pixels_per_point,
        // A drag trades detail for a responsive image; the idle frame after it
        // is drawn at full quality.
        moving: response.dragged(),
    });

    ui.painter().add(egui_wgpu::Callback::new_paint_callback(
        view_rect,
        VolumeRenderCallback {
            volume,
            transfer,
            uniforms,
            moving: response.dragged(),
        },
    ));

    let painter = ui.painter().with_clip_rect(view_rect);
    let max_width = (view_rect.width() - 2.0 * OVERLAY_MARGIN).max(0.0);

    if let Some(text) = identification_overlay_text(active.series_overlay.as_ref()) {
        paint_overlay_text(
            &painter,
            view_rect.left_top() + egui::vec2(OVERLAY_MARGIN, OVERLAY_MARGIN),
            egui::Align2::LEFT_TOP,
            &text,
            max_width,
            (view_rect.height() - 2.0 * OVERLAY_MARGIN).max(0.0),
        );
    }

    // Free rotation and an orthographic camera make a view from behind look
    // like a mirrored front view, so the edges name their patient direction.
    if let Some(letters) = edge_directions(&active.render, &active.prepared.series.geometry) {
        paint_edge_letters(&painter, view_rect, letters);
    }
}

/// Paint `[left, right, top, bottom]` at the middle of each edge of the view.
fn paint_edge_letters(painter: &egui::Painter, view_rect: egui::Rect, letters: [&str; 4]) {
    let [left, right, top, bottom] = letters;
    let max_width = (view_rect.width() - 2.0 * OVERLAY_MARGIN).max(0.0);
    let max_height = (view_rect.height() - 2.0 * OVERLAY_MARGIN).max(0.0);

    for (letter, anchor, offset, align) in [
        (
            left,
            view_rect.left_center(),
            egui::vec2(OVERLAY_MARGIN, 0.0),
            egui::Align2::LEFT_CENTER,
        ),
        (
            right,
            view_rect.right_center(),
            egui::vec2(-OVERLAY_MARGIN, 0.0),
            egui::Align2::RIGHT_CENTER,
        ),
        (
            top,
            view_rect.center_top(),
            egui::vec2(0.0, OVERLAY_MARGIN),
            egui::Align2::CENTER_TOP,
        ),
        (
            bottom,
            view_rect.center_bottom(),
            egui::vec2(0.0, -OVERLAY_MARGIN),
            egui::Align2::CENTER_BOTTOM,
        ),
    ] {
        paint_overlay_text(
            painter,
            anchor + offset,
            align,
            letter,
            max_width,
            max_height,
        );
    }
}

/// The patient directions the camera's left, right, top and bottom edges face.
fn edge_directions(
    render: &VolumeRenderState,
    geometry: &VolumeGeometry,
) -> Option<[&'static str; 4]> {
    let basis = render.camera_basis(geometry);
    // The camera works in volume millimetres, whose axes run along the image
    // rows, the image columns and the stack.
    let towards = |direction: [f32; 3], sign: f64| {
        dominant_patient_direction(std::array::from_fn(|axis| {
            sign * (f64::from(direction[0]) * geometry.row_direction[axis]
                + f64::from(direction[1]) * geometry.column_direction[axis]
                + f64::from(direction[2]) * geometry.slice_direction[axis])
        }))
    };

    Some([
        towards(basis.right, -1.0)?,
        towards(basis.right, 1.0)?,
        towards(basis.up, 1.0)?,
        towards(basis.up, -1.0)?,
    ])
}

/// Rotate, zoom and pan the camera like the 2D viewer moves its image.
fn handle_camera_interaction(
    ui: &egui::Ui,
    render: &mut VolumeRenderState,
    response: &egui::Response,
    view_rect: egui::Rect,
    extent_mm: f32,
) {
    if response.double_clicked_by(egui::PointerButton::Primary) {
        render.reset_view();
        return;
    }

    if response.dragged_by(egui::PointerButton::Primary) {
        render.rotate_by(response.drag_delta());
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    } else if response.dragged_by(egui::PointerButton::Secondary) {
        render.zoom_by((response.drag_delta().y * VIEWER_ZOOM_DRAG_SENSITIVITY).exp());
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    } else if response.dragged_by(egui::PointerButton::Middle) {
        let aspect = view_rect.width() / view_rect.height().max(1.0);
        let mm_per_point =
            2.0 * render.ortho_half_height_mm(extent_mm, aspect) / view_rect.height().max(1.0);

        render.pan_by(response.drag_delta(), mm_per_point);
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    } else if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }

    if response.hovered() {
        let scroll = ui.input(|input_state| input_state.smooth_scroll_delta.y);

        if scroll != 0.0 {
            render.zoom_by((scroll * VOLUME_WHEEL_ZOOM_SENSITIVITY).exp());
        }
    }
}

/// Everything one frame's uniforms are derived from.
struct CameraFrame<'a> {
    render: &'a VolumeRenderState,
    volume: &'a GpuVolume,
    transfer: &'a TransferFunctionLut,
    geometry: &'a VolumeGeometry,
    view_rect: egui::Rect,
    pixels_per_point: f32,
    moving: bool,
}

fn volume_uniforms(frame: &CameraFrame<'_>) -> VolumeUniforms {
    let size_mm = frame.volume.size_mm();
    let extent_mm = size_mm.into_iter().fold(0.0_f32, f32::max);
    let diagonal_mm = size_mm
        .into_iter()
        .map(|side| side * side)
        .sum::<f32>()
        .sqrt();
    let finest_spacing_mm = frame
        .volume
        .spacing_mm
        .into_iter()
        .fold(f32::INFINITY, f32::min);
    let basis = frame.render.camera_basis(frame.geometry);
    let viewport_min_px = frame.view_rect.min.to_vec2() * frame.pixels_per_point;
    let viewport_size_px = frame.view_rect.size() * frame.pixels_per_point;
    let aspect = viewport_size_px.x / viewport_size_px.y.max(1.0);
    let half_height_mm = frame.render.ortho_half_height_mm(extent_mm, aspect);
    // Far enough back that the whole box is in front of the camera, whatever
    // the orbit; the projection is orthographic, so distance costs nothing.
    let distance_mm = extent_mm * 2.0;
    let eye_mm: [f32; 3] = std::array::from_fn(|axis| {
        0.5 * size_mm[axis] - basis.forward[axis] * distance_mm
            + basis.right[axis] * frame.render.pan_mm[0]
            + basis.up[axis] * frame.render.pan_mm[1]
    });

    let mut step_mm =
        (finest_spacing_mm / STEPS_PER_VOXEL).max(diagonal_mm / MAX_STEPS_PER_DIAGONAL);
    if !step_mm.is_finite() || step_mm <= 0.0 {
        step_mm = 1.0;
    }
    let mut max_steps = (diagonal_mm / step_mm).ceil() + 2.0;

    if frame.moving {
        step_mm *= 2.0;
        max_steps = (max_steps / 2.0).ceil();
    }

    VolumeUniforms {
        eye_mm_ortho_half_height: [eye_mm[0], eye_mm[1], eye_mm[2], half_height_mm],
        right_aspect: [basis.right[0], basis.right[1], basis.right[2], aspect],
        up_step_mm: [basis.up[0], basis.up[1], basis.up[2], step_mm],
        forward_opacity_unit_mm: [
            basis.forward[0],
            basis.forward[1],
            basis.forward[2],
            OPACITY_UNIT_MM,
        ],
        size_mm_value_min: [size_mm[0], size_mm[1], size_mm[2], 0.0],
        dims_value_max: [
            frame.volume.dims[0] as f32,
            frame.volume.dims[1] as f32,
            frame.volume.dims[2] as f32,
            frame.volume.texel_scale,
        ],
        viewport_min_size_px: [
            viewport_min_px.x,
            viewport_min_px.y,
            viewport_size_px.x,
            viewport_size_px.y,
        ],
        lut_range_steps_shading: [
            frame.transfer.domain.0,
            frame.transfer.domain.1,
            max_steps,
            shader_flag(frame.render.shading),
        ],
        ambient_diffuse_spec_shine: [AMBIENT, DIFFUSE, SPECULAR, SPECULAR_POWER],
        // The viewer panel is black; the shader's own srgb flag is filled in
        // where the target format is known.
        background_srgb: [0.0, 0.0, 0.0, 0.0],
    }
}

/// One frame's draw, handed to egui-wgpu.
struct VolumeRenderCallback {
    volume: Arc<GpuVolume>,
    transfer: TransferFunctionLut,
    uniforms: VolumeUniforms,
    /// Whether the camera is being dragged, which halves the image's size.
    moving: bool,
}

impl CallbackTrait for VolumeRenderCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(resources) = callback_resources.get_mut::<VolumeRenderResources>() {
            resources.prepare(
                device,
                queue,
                &self.volume,
                &self.transfer,
                self.uniforms,
                self.moving,
            );
        }

        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(resources) = callback_resources.get::<VolumeRenderResources>() {
            resources.paint(render_pass, self.volume.id);
        }
    }
}

/// The volume texture currently on the GPU, with the bind group that draws it.
struct UploadedVolume {
    volume_id: u64,
    bind_group: wgpu::BindGroup,
}

/// Work whose wgpu errors are still being caught.
enum PendingWork {
    Pipeline(RenderPipelines),
    Upload(PendingUpload),
}

/// A volume texture and its staging buffers, allocated but not yet filled: a
/// refused buffer cannot be written, and a copy into a refused texture would
/// fail outside any error scope.
struct PendingUpload {
    volume: Arc<GpuVolume>,
    /// Kept until the upload is known to have worked, so a smaller retry can
    /// still be made from it.
    texels: Vec<u16>,
    /// The texture's own voxel counts.
    dims: [u32; 3],
    texture: wgpu::Texture,
    slabs: Vec<StagingSlab>,
    bind_group: wgpu::BindGroup,
}

/// One staging buffer, mapped since its creation, for a run of whole slices.
struct StagingSlab {
    buffer: wgpu::Buffer,
    first_slice: u32,
    slice_count: u32,
}

impl PendingUpload {
    /// Fill the staging buffers and copy them into the texture, now that every
    /// allocation is known to have succeeded.
    fn finish(self, device: &wgpu::Device, queue: &wgpu::Queue) -> UploadedVolume {
        let texels = &self.texels;
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("volume_render_upload"),
        });

        copy_staging_slabs(&mut encoder, &self.slabs, texels, self.dims, &self.texture);
        // Submitted now rather than with egui's frame: egui drops a frame whose
        // surface it cannot acquire without submitting it, which would leave
        // the texture drawn empty.
        queue.submit([encoder.finish()]);

        UploadedVolume {
            volume_id: self.volume.id,
            bind_group: self.bind_group,
        }
    }
}

/// Staging buffers for a volume of `dims`, mapped from their creation.
fn create_staging_slabs(device: &wgpu::Device, dims: [u32; 3]) -> Vec<StagingSlab> {
    let max_bytes = device.limits().max_buffer_size.min(MAX_STAGING_BYTES);

    staging_slabs(dims, max_bytes)
        .map(|(first_slice, slice_count)| StagingSlab {
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("volume_render_staging"),
                size: staging_row_bytes(dims[0]) * u64::from(dims[1]) * u64::from(slice_count),
                usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: true,
            }),
            first_slice,
            slice_count,
        })
        .collect()
}

/// Write the tightly packed `texels` into their staging slabs, unmap them, and
/// record each slab's copy into its slices of the texture.
fn copy_staging_slabs(
    encoder: &mut wgpu::CommandEncoder,
    slabs: &[StagingSlab],
    texels: &[u16],
    dims: [u32; 3],
    texture: &wgpu::Texture,
) {
    let slice_len = dims[0] as usize * dims[1] as usize;

    for slab in slabs {
        let first = slab.first_slice as usize * slice_len;
        let slices = &texels[first..first + slab.slice_count as usize * slice_len];

        write_staging_rows(
            slab.buffer.slice(..).get_mapped_range_mut().slice(..),
            slices,
            dims[0],
        );
        slab.buffer.unmap();
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &slab.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(staging_row_bytes(dims[0]) as u32),
                    rows_per_image: Some(dims[1]),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: 0,
                    y: 0,
                    z: slab.first_slice,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: dims[0],
                height: dims[1],
                depth_or_array_layers: slab.slice_count,
            },
        );
    }
}

struct PendingErrorScope {
    work: PendingWork,
    /// A boxed future is `Send` but not `Sync`, and the callback resources must
    /// be both, so the mutex is here for its `Sync` alone.
    future: Mutex<Pin<Box<dyn Future<Output = Option<wgpu::Error>> + Send>>>,
}

impl PendingErrorScope {
    fn new(
        work: PendingWork,
        future: impl Future<Output = Option<wgpu::Error>> + Send + 'static,
    ) -> Self {
        Self {
            work,
            future: Mutex::new(Box::pin(future)),
        }
    }

    /// The error a popped scope caught, if its future has already resolved,
    /// as the wgpu-core backend's do at once.
    fn resolved(future: impl Future<Output = Option<wgpu::Error>>) -> Option<Option<wgpu::Error>> {
        match std::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(error) => Some(error),
            Poll::Pending => None,
        }
    }

    /// The wgpu-core backend resolves the scope at once; a backend that does
    /// not is checked again on the next frame.
    fn poll(&mut self) -> Poll<Option<wgpu::Error>> {
        self.future
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
    }
}

/// Everything the volume rendering pipeline keeps between frames, stored in
/// egui-wgpu's callback resources.
struct VolumeRenderResources {
    target_format: wgpu::TextureFormat,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buffer: wgpu::Buffer,
    transfer_texture: wgpu::Texture,
    transfer_view: wgpu::TextureView,
    transfer_id: Option<u64>,
    /// Built on first use, so a device that rejects the shader fails into a
    /// message instead of a panic during start-up.
    pipeline: Option<RenderPipelines>,
    blit_bind_group_layout: wgpu::BindGroupLayout,
    blit_uniform_buffer: wgpu::Buffer,
    /// The image the rays are marched into, sized for the view.
    render_target: Option<RenderTarget>,
    /// What the image in `render_target` was marched from. A repaint with
    /// the same inputs, as any pointer motion elsewhere in the window causes,
    /// draws that image again instead of marching every ray anew.
    marched: Option<MarchKey>,
    uploaded: Option<UploadedVolume>,
    pending_error_scope: Option<PendingErrorScope>,
    /// The volume whose texture the device refused, with its voxel count, for
    /// the app to prepare a smaller one.
    refused_upload: Option<(u64, u64)>,
    /// Asks for the frame that hands a refusal on to the app.
    repaint: egui::Context,
    /// Why the pipeline cannot be built on this device, if it cannot.
    failure: Option<String>,
}

impl VolumeRenderResources {
    fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        repaint: egui::Context,
    ) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("volume_render_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: std::num::NonZeroU64::new(
                            size_of::<VolumeUniforms>() as u64
                        ),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("volume_render_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("volume_render_uniforms"),
            size: size_of::<VolumeUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let transfer_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("volume_render_transfer_function"),
            size: wgpu::Extent3d {
                width: TRANSFER_FUNCTION_ENTRIES as u32,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let transfer_view = transfer_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let blit_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("volume_render_blit_bind_group_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: std::num::NonZeroU64::new(
                                size_of::<BlitUniforms>() as u64
                            ),
                        },
                        count: None,
                    },
                ],
            });
        let blit_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("volume_render_blit_uniforms"),
            size: size_of::<BlitUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            target_format,
            bind_group_layout,
            sampler,
            uniform_buffer,
            transfer_texture,
            transfer_view,
            transfer_id: None,
            pipeline: None,
            blit_bind_group_layout,
            blit_uniform_buffer,
            render_target: None,
            marched: None,
            uploaded: None,
            pending_error_scope: None,
            refused_upload: None,
            repaint,
            failure: None,
        }
    }

    /// Release the texture, uploaded or on its way, of any volume but
    /// `wanted`: another series' after a switch, or a closed study's.
    fn release_unwanted(&mut self, wanted: Option<u64>) {
        if self
            .uploaded
            .as_ref()
            .is_some_and(|uploaded| Some(uploaded.volume_id) != wanted)
        {
            self.uploaded = None;
        }

        if let Some(PendingErrorScope {
            work: PendingWork::Upload(upload),
            ..
        }) = &self.pending_error_scope
            && Some(upload.volume.id) != wanted
        {
            self.pending_error_scope = None;
        }
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        volume: &Arc<GpuVolume>,
        transfer: &TransferFunctionLut,
        uniforms: VolumeUniforms,
        moving: bool,
    ) {
        if self.failure.is_some() {
            return;
        }

        // One guarded step per frame, so error scopes never nest.
        if self.pending_error_scope.is_some() {
            self.resolve_pending_scope(device, queue);
            return;
        }

        if self.pipeline.is_none() {
            self.start_pipeline(device);
            self.resolve_pending_scope(device, queue);

            if self.pipeline.is_none() {
                return;
            }
        }

        self.write_transfer_function(queue, transfer);

        if self
            .uploaded
            .as_ref()
            .is_none_or(|uploaded| uploaded.volume_id != volume.id)
        {
            self.start_upload(device, volume);
            self.resolve_pending_scope(device, queue);
        }

        self.march(device, queue, volume, transfer, uniforms, moving);
    }

    /// March the rays into the render target, unless it already holds the
    /// image these inputs make, and aim the blit at the view.
    fn march(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        volume: &GpuVolume,
        transfer: &TransferFunctionLut,
        uniforms: VolumeUniforms,
        moving: bool,
    ) {
        if self
            .uploaded
            .as_ref()
            .is_none_or(|uploaded| uploaded.volume_id != volume.id)
        {
            return;
        }

        let [_, _, view_width, view_height] = uniforms.viewport_min_size_px;
        let size = render_size([view_width, view_height], moving);
        if !self.ensure_render_target(device, size) {
            return;
        }
        let (Some(pipelines), Some(uploaded)) = (&self.pipeline, &self.uploaded) else {
            return;
        };

        // Within the image the view starts at its origin, and the image keeps
        // the authored colours for the blit to convert.
        let mut march_uniforms = uniforms;
        march_uniforms.viewport_min_size_px = [0.0, 0.0, size[0] as f32, size[1] as f32];
        march_uniforms.background_srgb[3] = shader_flag(false);
        let key = MarchKey {
            volume_id: volume.id,
            transfer_id: transfer.id,
            size,
            uniforms: bytemuck::cast(march_uniforms),
        };

        if self.marched != Some(key)
            && let Some(target) = &self.render_target
        {
            queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&march_uniforms));
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("volume_render_march"),
            });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("volume_render_march"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target.view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipelines.march);
                pass.set_bind_group(0, &uploaded.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            // Submitted now, like the upload, so a frame egui drops cannot
            // leave a key claiming an image that was never drawn.
            queue.submit([encoder.finish()]);
            self.marched = Some(key);
        }

        queue.write_buffer(
            &self.blit_uniform_buffer,
            0,
            bytemuck::bytes_of(&BlitUniforms {
                viewport_min_size_px: uniforms.viewport_min_size_px,
                flags: [shader_flag(self.target_format.is_srgb()), 0.0, 0.0, 0.0],
            }),
        );
    }

    /// Make the render target `size`, if it is not already. Its allocation is
    /// guarded like the upload's, and `false` means there is no target to
    /// march into this frame.
    fn ensure_render_target(&mut self, device: &wgpu::Device, size: [u32; 2]) -> bool {
        if self
            .render_target
            .as_ref()
            .is_some_and(|target| target.size == size)
        {
            return true;
        }

        self.render_target = None;
        self.marched = None;

        let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("volume_render_image"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: RENDER_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let blit_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("volume_render_blit_bind_group"),
            layout: &self.blit_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.blit_uniform_buffer.as_entire_binding(),
                },
            ],
        });
        let validation = PendingErrorScope::resolved(validation.pop());
        let out_of_memory = PendingErrorScope::resolved(out_of_memory.pop());

        // A scope that has not resolved yet is treated as refused: the next
        // frame tries again.
        if !matches!(validation, Some(None)) || !matches!(out_of_memory, Some(None)) {
            return false;
        }

        self.render_target = Some(RenderTarget {
            size,
            view,
            blit_bind_group,
        });
        true
    }

    fn paint(&self, render_pass: &mut wgpu::RenderPass<'static>, volume_id: u64) {
        let (Some(pipelines), Some(target)) = (&self.pipeline, &self.render_target) else {
            return;
        };

        if self
            .marched
            .is_none_or(|marched| marched.volume_id != volume_id)
        {
            return;
        }

        render_pass.set_pipeline(&pipelines.blit);
        render_pass.set_bind_group(0, &target.blit_bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }

    /// Install whatever the caught scope allowed, or record why it did not.
    fn resolve_pending_scope(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let Some(pending) = &mut self.pending_error_scope else {
            return;
        };
        let Poll::Ready(error) = pending.poll() else {
            return;
        };
        let Some(pending) = self.pending_error_scope.take() else {
            return;
        };

        match (pending.work, error) {
            (PendingWork::Pipeline(pipeline), None) => self.pipeline = Some(pipeline),
            (PendingWork::Upload(upload), None) => {
                self.uploaded = Some(upload.finish(device, queue));
            }
            (PendingWork::Pipeline(_), Some(_)) => self.failure = Some(FAILURE_MESSAGE.to_owned()),
            (PendingWork::Upload(upload), Some(_)) => {
                // The app prepares a smaller texture of the volume; this one's
                // texels go with the refused attempt.
                let voxels = upload.dims.into_iter().map(u64::from).product();
                self.refused_upload = Some((upload.volume.id, voxels));
                self.repaint.request_repaint();
            }
        }
    }

    /// Compile the shaders and build the pipelines, catching every kind of
    /// error a device with a different opinion would otherwise panic on: wgpu
    /// reports a driver that rejects a translated shader as an internal
    /// error, and a failed allocation as out of memory, and delivers each only
    /// to a scope of its own kind.
    fn start_pipeline(&mut self, device: &wgpu::Device) {
        let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        // The march draws into the render target, one sample per pixel.
        let march = fullscreen_pipeline(
            device,
            "volume_render_march",
            SHADER_SOURCE,
            &self.bind_group_layout,
            RENDER_FORMAT,
            1,
        );
        // The blit draws inside egui's own render pass, so both must agree on
        // the sample count and on having no depth attachment.
        let blit = fullscreen_pipeline(
            device,
            "volume_render_blit",
            BLIT_SHADER_SOURCE,
            &self.blit_bind_group_layout,
            self.target_format,
            u32::from(crate::EGUI_MSAA_SAMPLES).max(1),
        );

        // Popped innermost first, and any error fails the pipeline.
        let validation = validation.pop();
        let internal = internal.pop();
        let out_of_memory = out_of_memory.pop();

        self.pending_error_scope = Some(PendingErrorScope::new(
            PendingWork::Pipeline(RenderPipelines { march, blit }),
            async move { validation.await.or(internal.await).or(out_of_memory.await) },
        ));
    }

    fn write_transfer_function(&mut self, queue: &wgpu::Queue, transfer: &TransferFunctionLut) {
        if self.transfer_id == Some(transfer.id) {
            return;
        }

        let width = transfer.rgba.len() as u32;

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.transfer_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            transfer.rgba.as_flattened(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width,
                height: 1,
                depth_or_array_layers: 1,
            },
        );

        self.transfer_id = Some(transfer.id);
    }

    /// Allocate the 3-D texture and the staging buffers that will fill it,
    /// catching the out-of-memory errors that would otherwise reach wgpu's
    /// panicking default handler. Nothing is written until the allocations are
    /// known to have succeeded.
    ///
    /// `Queue::write_texture` is not used: it allocates its own staging copy of
    /// the whole volume, and wgpu treats running out of memory there as fatal
    /// and loses the device, which egui then panics on. A refused buffer or
    /// texture created here only becomes an error in the scopes below.
    fn start_upload(&mut self, device: &wgpu::Device, volume: &Arc<GpuVolume>) {
        let Some(texels) = volume.take_texels() else {
            return;
        };
        // The previous texture is no longer drawn; releasing it first leaves
        // its memory to the new one.
        self.uploaded = None;
        let dims = volume.dims;
        // A refused `create_texture` or `create_buffer` hands back an invalid
        // object, and every later use of it raises a validation error rather
        // than another out-of-memory one. Both scopes are open, so neither
        // reaches wgpu's panicking default handler.
        let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("volume_render_volume"),
            size: wgpu::Extent3d {
                width: dims[0],
                height: dims[1],
                depth_or_array_layers: dims[2],
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::R16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let slabs = create_staging_slabs(device, dims);

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("volume_render_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&self.transfer_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        // Popped innermost first, and either error refuses the upload.
        let validation = validation.pop();
        let out_of_memory = out_of_memory.pop();

        self.pending_error_scope = Some(PendingErrorScope::new(
            PendingWork::Upload(PendingUpload {
                volume: Arc::clone(volume),
                texels,
                dims,
                texture,
                slabs,
                bind_group,
            }),
            async move { validation.await.or(out_of_memory.await) },
        ));
    }
}

/// A pipeline that draws one triangle over the whole viewport with `source`'s
/// `vs_main` and `fs_main`.
fn fullscreen_pipeline(
    device: &wgpu::Device,
    label: &str,
    source: &'static str,
    bind_group_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
    sample_count: u32,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(source)),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });

    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: sample_count,
            ..Default::default()
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// The size of the image the rays are marched into for a view of `view_px`
/// physical pixels: the view's own, scaled down to `MAX_RENDER_PIXELS`, and
/// halved again on each axis while the camera is dragged.
fn render_size(view_px: [f32; 2], moving: bool) -> [u32; 2] {
    let [width, height] = view_px.map(|length| length.max(1.0));
    let budget_scale = (MAX_RENDER_PIXELS / (width * height)).sqrt().min(1.0);
    let scale = if moving {
        budget_scale * 0.5
    } else {
        budget_scale
    };

    [width, height].map(|length| (length * scale).ceil().max(1.0) as u32)
}

/// The march and blit pipelines, built together.
struct RenderPipelines {
    march: wgpu::RenderPipeline,
    blit: wgpu::RenderPipeline,
}

/// The image the rays are marched into, with the bind group that draws it.
struct RenderTarget {
    size: [u32; 2],
    view: wgpu::TextureView,
    blit_bind_group: wgpu::BindGroup,
}

/// Everything a marched image depends on.
#[derive(Clone, Copy, PartialEq)]
struct MarchKey {
    volume_id: u64,
    transfer_id: u64,
    size: [u32; 2],
    uniforms: [u8; size_of::<VolumeUniforms>()],
}

/// The blit's uniform block, mirrored in `volume_blit.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BlitUniforms {
    viewport_min_size_px: [f32; 4],
    flags: [f32; 4],
}

/// The shader has no booleans in its uniform block.
fn shader_flag(value: bool) -> f32 {
    if value { 1.0 } else { 0.0 }
}

/// The bytes one staged row of R16Float texels takes: a buffer-to-texture copy
/// needs every row to start on a 256 byte boundary.
fn staging_row_bytes(width: u32) -> u64 {
    (u64::from(width) * 2).next_multiple_of(u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT))
}

/// The runs of whole slices that share one staging buffer, as the first slice
/// and the count: as many as fit `max_bytes`, and at least one.
fn staging_slabs(dims: [u32; 3], max_bytes: u64) -> impl Iterator<Item = (u32, u32)> {
    let slice_bytes = staging_row_bytes(dims[0]) * u64::from(dims[1]);
    let slices_per_slab = u32::try_from(max_bytes / slice_bytes.max(1))
        .unwrap_or(u32::MAX)
        .max(1);

    (0..dims[2])
        .step_by(slices_per_slab as usize)
        .map(move |first_slice| (first_slice, slices_per_slab.min(dims[2] - first_slice)))
}

/// Lay tightly packed rows of `width` texels out in staging memory, one row
/// every `staging_row_bytes(width)` bytes.
fn write_staging_rows(mut staging: wgpu::WriteOnly<'_, [u8]>, texels: &[u16], width: u32) {
    let row_bytes = staging_row_bytes(width) as usize;
    let width = width as usize;

    for (index, row) in texels.chunks_exact(width).enumerate() {
        let start = index * row_bytes;

        staging
            .slice(start..start + width * 2)
            .copy_from_slice(bytemuck::cast_slice(row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse and validate `source`, and check that its uniform block named
    /// `block` matches a Rust struct of `size` bytes member for member.
    fn assert_shader_matches(
        source: &str,
        block: &str,
        size: usize,
        rust_members: &[(&str, usize)],
    ) {
        use wgpu::naga;

        let module = naga::front::wgsl::parse_str(source).unwrap();
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        )
        .validate(&module)
        .unwrap();

        let uniforms = module
            .global_variables
            .iter()
            .find(|(_, variable)| variable.name.as_deref() == Some(block))
            .map(|(_, variable)| &module.types[variable.ty].inner)
            .unwrap();
        let naga::TypeInner::Struct { members, span } = uniforms else {
            panic!("the uniform block is not a struct");
        };

        assert_eq!(*span as usize, size);
        let shader_members: Vec<_> = members
            .iter()
            .map(|member| (member.name.as_deref().unwrap(), member.offset as usize))
            .collect();
        assert_eq!(shader_members, rust_members);
    }

    /// The shaders are only compiled by the device at run time, where an
    /// error shows as "unavailable" on every machine. Parsing and validating
    /// them here catches that in any build, and so does a uniform block that
    /// no longer matches its Rust struct member for member.
    #[test]
    fn the_shaders_are_valid_and_match_their_uniform_structs() {
        let rust_members = [
            (
                "eye_mm_ortho_half_height",
                std::mem::offset_of!(VolumeUniforms, eye_mm_ortho_half_height),
            ),
            (
                "right_aspect",
                std::mem::offset_of!(VolumeUniforms, right_aspect),
            ),
            (
                "up_step_mm",
                std::mem::offset_of!(VolumeUniforms, up_step_mm),
            ),
            (
                "forward_opacity_unit_mm",
                std::mem::offset_of!(VolumeUniforms, forward_opacity_unit_mm),
            ),
            (
                "size_mm_value_min",
                std::mem::offset_of!(VolumeUniforms, size_mm_value_min),
            ),
            (
                "dims_value_max",
                std::mem::offset_of!(VolumeUniforms, dims_value_max),
            ),
            (
                "viewport_min_size_px",
                std::mem::offset_of!(VolumeUniforms, viewport_min_size_px),
            ),
            (
                "lut_range_steps_shading",
                std::mem::offset_of!(VolumeUniforms, lut_range_steps_shading),
            ),
            (
                "ambient_diffuse_spec_shine",
                std::mem::offset_of!(VolumeUniforms, ambient_diffuse_spec_shine),
            ),
            (
                "background_srgb",
                std::mem::offset_of!(VolumeUniforms, background_srgb),
            ),
        ];
        assert_shader_matches(
            SHADER_SOURCE,
            "uniforms",
            size_of::<VolumeUniforms>(),
            &rust_members,
        );

        assert_shader_matches(
            BLIT_SHADER_SOURCE,
            "blit",
            size_of::<BlitUniforms>(),
            &[
                (
                    "viewport_min_size_px",
                    std::mem::offset_of!(BlitUniforms, viewport_min_size_px),
                ),
                ("flags", std::mem::offset_of!(BlitUniforms, flags)),
            ],
        );
    }

    /// The camera the shader is handed: the view's aspect in pixels, an eye
    /// in front of the volume's centre moved by the pan along the screen
    /// axes, half the finest spacing per step, and twice that while dragging.
    #[test]
    fn the_uniforms_place_the_camera_and_size_the_steps() {
        use crate::app::state::WindowLevel;
        use crate::app::three_d_state::TransferPreset;

        // 10 x 20 x 40 mm, axial, so the default camera looks along +y with
        // +x to the right and +z up.
        let source = crate::dicom::Volume::new(10, 20, 40, [1.0; 3], vec![0.0; 10 * 20 * 40]);
        let volume = GpuVolume::from_volume(&source, 64);
        let geometry = VolumeGeometry {
            inverted: false,
            row_direction: [1.0, 0.0, 0.0],
            column_direction: [0.0, 1.0, 0.0],
            slice_direction: [0.0, 0.0, 1.0],
            slice_thickness_mm: Some(1.0),
            slice_spacing_mm: 1.0,
            slice_count: 40,
            uneven_slice_offsets_mm: None,
        };
        let transfer = TransferFunctionLut::bake(
            TransferPreset::Grayscale,
            WindowLevel {
                center: 0.0,
                width: 100.0,
            },
            false,
        );
        let render = VolumeRenderState {
            pan_mm: [3.0, -2.0],
            zoom: 2.0,
            ..VolumeRenderState::default()
        };
        let frame = |moving| CameraFrame {
            render: &render,
            volume: &volume,
            transfer: &transfer,
            geometry: &geometry,
            view_rect: egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(800.0, 400.0)),
            pixels_per_point: 2.0,
            moving,
        };

        let still = volume_uniforms(&frame(false));
        // Twice the largest side back from the centre, plus the pan.
        assert_eq!(still.eye_mm_ortho_half_height, [8.0, -70.0, 18.0, 10.0]);
        assert_eq!(still.right_aspect, [1.0, 0.0, 0.0, 2.0]);
        assert_eq!(still.viewport_min_size_px, [20.0, 40.0, 1600.0, 800.0]);
        assert_eq!(still.up_step_mm, [0.0, 0.0, 1.0, 0.5]);
        assert_eq!(still.forward_opacity_unit_mm[..3], [0.0, 1.0, 0.0]);
        // The diagonal, 45.8 mm, in half millimetre steps, and two to spare.
        assert_eq!(still.lut_range_steps_shading[2], 94.0);

        let moving = volume_uniforms(&frame(true));
        assert_eq!(moving.up_step_mm[3], 1.0);
        assert_eq!(moving.lut_range_steps_shading[2], 47.0);
    }

    /// The image is marched at the view's own resolution up to the pixel
    /// budget, and at half that on each axis while the camera moves.
    #[test]
    fn the_render_size_follows_the_view_within_the_pixel_budget() {
        assert_eq!(render_size([640.0, 480.0], false), [640, 480]);
        assert_eq!(render_size([640.0, 480.0], true), [320, 240]);
        assert_eq!(render_size([0.0, 0.0], false), [1, 1]);

        let [width, height] = render_size([4000.0, 2000.0], false);
        let pixels = f64::from(width) * f64::from(height);
        assert!(
            pixels <= f64::from(MAX_RENDER_PIXELS) * 1.01,
            "{width}x{height}"
        );
        assert!(width.abs_diff(height * 2) <= 1, "{width}x{height}");
        assert_eq!(
            render_size([4000.0, 2000.0], true),
            [width.div_ceil(2), height.div_ceil(2)]
        );
    }

    /// The default view looks at the patient from the front, head up; turned
    /// half way round it looks from behind, with left and right swapped.
    /// Each edge letter is painted, not just chosen, in the app's own fonts,
    /// whose rows are taller than egui's defaults.
    #[test]
    fn the_edge_letters_are_painted() {
        let context = egui::Context::default();
        crate::configure_fonts(&context);
        let view_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0));
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(view_rect),
                ..Default::default()
            },
            |ui| paint_edge_letters(ui.painter(), view_rect, ["R", "L", "H", "F"]),
        );
        let texts = super::super::painted_texts(&output);

        for letter in ["R", "L", "H", "F"] {
            assert!(
                texts.iter().any(|text| text == letter),
                "{letter}: {texts:?}"
            );
        }
    }

    #[test]
    fn the_edges_name_the_patient_directions_they_face() {
        let geometry = |slice_direction| VolumeGeometry {
            inverted: false,
            row_direction: [1.0, 0.0, 0.0],
            column_direction: [0.0, 1.0, 0.0],
            slice_direction,
            slice_thickness_mm: Some(1.0),
            slice_spacing_mm: 1.0,
            slice_count: 2,
            uneven_slice_offsets_mm: None,
        };
        let head_first = geometry([0.0, 0.0, 1.0]);
        let feet_first = geometry([0.0, 0.0, -1.0]);
        let mut render = VolumeRenderState::default();

        assert_eq!(
            edge_directions(&render, &head_first),
            Some(["R", "L", "H", "F"])
        );
        assert_eq!(
            edge_directions(&render, &feet_first),
            Some(["R", "L", "H", "F"])
        );

        // Dragging 450 points turns the camera by 180 degrees.
        render.rotate_by(egui::vec2(-450.0, 0.0));
        assert_eq!(
            edge_directions(&render, &head_first),
            Some(["L", "R", "H", "F"])
        );
    }

    #[test]
    fn staged_rows_start_on_the_copy_alignment() {
        let mut staging = vec![0xAA_u8; 2 * 256];

        write_staging_rows(
            wgpu::WriteOnly::from_mut(staging.as_mut_slice()),
            &[1, 2, 3, 4, 5, 6],
            3,
        );

        assert_eq!(staging_row_bytes(3), 256);
        assert_eq!(staging_row_bytes(128), 256);
        assert_eq!(staging_row_bytes(129), 512);
        assert_eq!(&staging[..6], bytemuck::cast_slice::<u16, u8>(&[1, 2, 3]));
        assert_eq!(
            &staging[256..262],
            bytemuck::cast_slice::<u16, u8>(&[4, 5, 6])
        );
        // The padding between rows is left as it was.
        assert!(staging[6..256].iter().all(|&byte| byte == 0xAA));
    }

    #[test]
    fn staging_slabs_cover_every_slice_within_the_size_limit() {
        // One slice of 3 x 5 texels stages as five 256 byte rows.
        let dims = [3, 5, 7];

        assert_eq!(
            staging_slabs(dims, 2 * 1280).collect::<Vec<_>>(),
            [(0, 2), (2, 2), (4, 2), (6, 1)]
        );
        assert_eq!(staging_slabs(dims, u64::MAX).collect::<Vec<_>>(), [(0, 7)]);
        // A single slice over the limit still gets a slab of its own.
        assert_eq!(staging_slabs(dims, 100).count(), 7);
    }

    /// The output of a wgpu future that the native backends complete at once.
    fn ready<T>(future: impl Future<Output = T>) -> Option<T> {
        match std::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(output) => Some(output),
            Poll::Pending => None,
        }
    }

    /// A device on the platform's native backend: Metal on macOS and Vulkan
    /// elsewhere. GPU tests report when no device is available.
    fn native_test_device(max_buffer_size: u64) -> Option<(wgpu::Device, wgpu::Queue)> {
        #[cfg(target_os = "macos")]
        let backends = wgpu::Backends::METAL;
        #[cfg(not(target_os = "macos"))]
        let backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = ready(instance.enumerate_adapters(backends))
            .and_then(|adapters| adapters.into_iter().next());
        let device = adapter.and_then(|adapter| {
            ready(adapter.request_device(&wgpu::DeviceDescriptor {
                required_limits: wgpu::Limits {
                    max_buffer_size,
                    ..wgpu::Limits::downlevel_defaults()
                },
                ..Default::default()
            }))?
            .ok()
        });

        if device.is_none() {
            eprintln!("skipped: no native GPU device");
        }
        device
    }

    /// Records what reaches wgpu's handlers instead of panicking, so a test can
    /// assert that nothing escaped the error scopes and the device survived.
    fn record_device_trouble(device: &wgpu::Device) -> Arc<Mutex<Vec<String>>> {
        let trouble: Arc<Mutex<Vec<String>>> = Arc::default();
        let lost = Arc::clone(&trouble);
        device.set_device_lost_callback(move |reason, message| {
            lost.lock()
                .unwrap()
                .push(format!("lost ({reason:?}): {message}"));
        });
        let uncaptured = Arc::clone(&trouble);
        device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
            uncaptured
                .lock()
                .unwrap()
                .push(format!("uncaptured: {error}"));
        }));

        trouble
    }

    /// A 3 x 5 x 7 volume whose voxels all differ.
    fn numbered_volume() -> Arc<GpuVolume> {
        let source = crate::dicom::Volume::new(
            3,
            5,
            7,
            [1.0; 3],
            (0..3 * 5 * 7).map(|value| value as f32).collect(),
        );

        Arc::new(GpuVolume::from_volume(&source, 64))
    }

    /// Every texel reaches the texture through the staging slabs, with the rows
    /// padded for the copy and each slab at its own depth.
    #[test]
    fn staging_slabs_reach_every_texel() {
        // Three padded slices of 1280 bytes fit one 4096 byte buffer, so the
        // seven slices take three slabs.
        let Some((device, queue)) = native_test_device(4096) else {
            return;
        };
        let trouble = record_device_trouble(&device);
        let dims = [3, 5, 7];
        let texels: Vec<u16> = (0..3 * 5 * 7).map(|index| 0x3C00 + index).collect();
        // Unlike the volume rendering texture, this one can be read back.
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: 3,
                height: 5,
                depth_or_array_layers: 7,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::R16Float,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let slabs = create_staging_slabs(&device, dims);
        assert_eq!(slabs.len(), 3);

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        copy_staging_slabs(&mut encoder, &slabs, &texels, dims, &texture);
        let row_bytes = staging_row_bytes(3);
        let readbacks: Vec<_> = (0..7)
            .map(|slice| {
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: row_bytes * 5,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: 0,
                            z: slice,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(row_bytes as u32),
                            rows_per_image: Some(5),
                        },
                    },
                    wgpu::Extent3d {
                        width: 3,
                        height: 5,
                        depth_or_array_layers: 1,
                    },
                );
                buffer
            })
            .collect();
        queue.submit([encoder.finish()]);
        for buffer in &readbacks {
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, |result| result.unwrap());
        }
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .unwrap();

        let mut copied = Vec::new();
        for buffer in &readbacks {
            let bytes = buffer.slice(..).get_mapped_range();
            for row in bytes.chunks_exact(row_bytes as usize) {
                copied.extend_from_slice(bytemuck::cast_slice::<u8, u16>(&row[..6]));
            }
        }
        assert!(trouble.lock().unwrap().is_empty(), "{trouble:?}");
        assert_eq!(copied, texels);
    }

    #[test]
    fn the_pipeline_builds_without_errors() {
        let Some((device, queue)) = native_test_device(4096) else {
            return;
        };
        let trouble = record_device_trouble(&device);
        let mut resources = VolumeRenderResources::new(
            &device,
            wgpu::TextureFormat::Rgba8Unorm,
            egui::Context::default(),
        );

        resources.start_pipeline(&device);
        resources.resolve_pending_scope(&device, &queue);

        assert!(resources.pipeline.is_some());
        assert!(resources.failure.is_none());
        assert!(trouble.lock().unwrap().is_empty(), "{trouble:?}");
    }

    #[test]
    fn a_volume_that_fits_is_uploaded_without_errors() {
        let Some((device, queue)) = native_test_device(4096) else {
            return;
        };
        let trouble = record_device_trouble(&device);
        let mut resources = VolumeRenderResources::new(
            &device,
            wgpu::TextureFormat::Rgba8Unorm,
            egui::Context::default(),
        );
        let volume = numbered_volume();

        resources.start_upload(&device, &volume);
        resources.resolve_pending_scope(&device, &queue);
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .unwrap();

        assert_eq!(
            resources
                .uploaded
                .as_ref()
                .map(|uploaded| uploaded.volume_id),
            Some(volume.id)
        );
        assert_eq!(resources.refused_upload, None);
        // The staging copy of the texels is dropped once the upload worked.
        assert!(volume.take_texels().is_none());
        assert!(trouble.lock().unwrap().is_empty(), "{trouble:?}");

        // The texture stays while its volume is wanted, and goes once not.
        resources.release_unwanted(Some(volume.id));
        assert!(resources.uploaded.is_some());
        resources.release_unwanted(None);
        assert!(resources.uploaded.is_none());
    }

    /// Blit what `resources` last marched into a `view` square of their
    /// target format, and read its pixels back, row by row from the top.
    fn paint_and_read_back(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        resources: &VolumeRenderResources,
        volume_id: u64,
        view: u32,
    ) -> Vec<[u8; 3]> {
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: view,
                height: view,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: resources.target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(view * view * 4),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let view = target.create_view(&wgpu::TextureViewDescriptor::default());
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLUE),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            resources.paint(&mut pass.forget_lifetime(), volume_id);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(view * 4),
                    rows_per_image: Some(view),
                },
            },
            wgpu::Extent3d {
                width: view,
                height: view,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, |result| result.unwrap());
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .unwrap();

        let pixels = readback.slice(..).get_mapped_range();
        pixels
            .chunks_exact(4)
            .map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect()
    }

    /// The whole path, from texels to pixels: a sphere of bone density in air
    /// renders lit in the middle of the view and leaves the corners empty.
    #[test]
    fn a_bone_sphere_renders_in_the_middle_of_the_view() {
        use crate::app::state::WindowLevel;
        use crate::app::three_d_state::TransferPreset;

        let Some((device, queue)) = native_test_device(1 << 20) else {
            return;
        };
        let trouble = record_device_trouble(&device);
        const SIDE: usize = 16;
        const VIEW: u32 = 64;
        let values = (0..SIDE * SIDE * SIDE)
            .map(|index| {
                let [x, y, z] = [index % SIDE, index / SIDE % SIDE, index / (SIDE * SIDE)]
                    .map(|coordinate| coordinate as f32 + 0.5 - SIDE as f32 / 2.0);

                if x * x + y * y + z * z < 25.0 {
                    1000.0
                } else {
                    -1000.0
                }
            })
            .collect();
        let source = crate::dicom::Volume::new(SIDE, SIDE, SIDE, [1.0; 3], values);
        let volume = Arc::new(GpuVolume::from_volume(&source, 64));
        let geometry = VolumeGeometry {
            inverted: false,
            row_direction: [1.0, 0.0, 0.0],
            column_direction: [0.0, 1.0, 0.0],
            slice_direction: [0.0, 0.0, 1.0],
            slice_thickness_mm: Some(1.0),
            slice_spacing_mm: 1.0,
            slice_count: SIDE,
            uneven_slice_offsets_mm: None,
        };
        let transfer = TransferFunctionLut::bake(
            TransferPreset::CtBone,
            WindowLevel {
                center: 40.0,
                width: 400.0,
            },
            false,
        );
        let render = VolumeRenderState::default();
        let uniforms = volume_uniforms(&CameraFrame {
            render: &render,
            volume: &volume,
            transfer: &transfer,
            geometry: &geometry,
            view_rect: egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(VIEW as f32, VIEW as f32),
            ),
            pixels_per_point: 1.0,
            moving: false,
        });
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut resources = VolumeRenderResources::new(&device, format, egui::Context::default());
        resources.prepare(&device, &queue, &volume, &transfer, uniforms, false);
        assert!(resources.pipeline.is_some() && resources.uploaded.is_some());
        let marched = resources.marched;
        assert!(marched.is_some_and(|key| key.size == [VIEW, VIEW]));

        // The same view moved on screen reuses the marched image, and a moving
        // camera marches a smaller one.
        let mut moved = uniforms;
        moved.viewport_min_size_px[0] += 10.0;
        resources.prepare(&device, &queue, &volume, &transfer, moved, false);
        assert!(resources.marched == marched);
        resources.prepare(&device, &queue, &volume, &transfer, uniforms, true);
        assert!(
            resources
                .marched
                .is_some_and(|key| key.size == [VIEW / 2, VIEW / 2])
        );
        resources.prepare(&device, &queue, &volume, &transfer, uniforms, false);
        assert!(resources.marched == marched);

        let pixels = paint_and_read_back(&device, &queue, &resources, volume.id, VIEW);
        let pixel = |x: u32, y: u32| pixels[(y * VIEW + x) as usize];
        let centre = pixel(VIEW / 2, VIEW / 2);
        assert!(
            centre.iter().all(|channel| *channel > 100),
            "centre {centre:?}"
        );
        assert_eq!(pixel(1, 1), [0, 0, 0]);
        assert_eq!(pixel(VIEW - 2, VIEW - 2), [0, 0, 0]);
        assert!(trouble.lock().unwrap().is_empty(), "{trouble:?}");
    }

    /// The default view shows the patient from the front, head up: a blob on
    /// the patient's left, towards the head, renders at the top right of the
    /// image, whichever way the series was stacked or the patient lay.
    #[test]
    fn the_default_render_puts_the_patients_left_on_the_right_and_head_up() {
        use crate::app::state::WindowLevel;
        use crate::app::three_d_state::TransferPreset;

        let Some((device, queue)) = native_test_device(1 << 20) else {
            return;
        };
        let trouble = record_device_trouble(&device);
        const SIDE: usize = 16;
        const VIEW: u32 = 64;
        let transfer = TransferFunctionLut::bake(
            TransferPreset::CtBone,
            WindowLevel {
                center: 40.0,
                width: 400.0,
            },
            false,
        );

        for (row_direction, column_direction, slice_direction) in [
            // Head first supine, and feet first prone.
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, -1.0]),
        ] {
            let geometry = VolumeGeometry {
                inverted: false,
                row_direction,
                column_direction,
                slice_direction,
                slice_thickness_mm: Some(1.0),
                slice_spacing_mm: 1.0,
                slice_count: SIDE,
                uneven_slice_offsets_mm: None,
            };
            // Each voxel's patient position from the volume's centre, through
            // the direction cosines: +x is the patient's left, +z the head.
            let values = (0..SIDE * SIDE * SIDE)
                .map(|index| {
                    let voxel = [index % SIDE, index / SIDE % SIDE, index / (SIDE * SIDE)]
                        .map(|coordinate| coordinate as f64 + 0.5 - SIDE as f64 / 2.0);
                    let axes = [row_direction, column_direction, slice_direction];
                    let [x, y, z]: [f64; 3] = std::array::from_fn(|component| {
                        (0..3).map(|axis| voxel[axis] * axes[axis][component]).sum()
                    });
                    let [dx, dz] = [x - 4.0, z - 4.0];

                    if dx * dx + y * y + dz * dz < 6.0 {
                        1000.0
                    } else {
                        -1000.0
                    }
                })
                .collect();
            let source = crate::dicom::Volume::new(SIDE, SIDE, SIDE, [1.0; 3], values);
            let volume = Arc::new(GpuVolume::from_volume(&source, 64));
            let render = VolumeRenderState::default();
            let uniforms = volume_uniforms(&CameraFrame {
                render: &render,
                volume: &volume,
                transfer: &transfer,
                geometry: &geometry,
                view_rect: egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(VIEW as f32, VIEW as f32),
                ),
                pixels_per_point: 1.0,
                moving: false,
            });
            let mut resources = VolumeRenderResources::new(
                &device,
                wgpu::TextureFormat::Rgba8Unorm,
                egui::Context::default(),
            );
            resources.prepare(&device, &queue, &volume, &transfer, uniforms, false);
            let pixels = paint_and_read_back(&device, &queue, &resources, volume.id, VIEW);

            let lit: Vec<[u32; 2]> = (0..VIEW)
                .flat_map(|y| (0..VIEW).map(move |x| [x, y]))
                .filter(|[x, y]| pixels[(y * VIEW + x) as usize][0] > 100)
                .collect();
            assert!(!lit.is_empty(), "{geometry:?}");
            let centroid = [0, 1]
                .map(|axis| lit.iter().map(|pixel| pixel[axis]).sum::<u32>() / lit.len() as u32);
            assert!(centroid[0] > VIEW / 2 + 4, "{centroid:?} {geometry:?}");
            assert!(centroid[1] < VIEW / 2 - 4, "{centroid:?} {geometry:?}");
        }
        assert!(trouble.lock().unwrap().is_empty(), "{trouble:?}");
    }

    /// A staging buffer the device refuses, while the texture itself fits,
    /// is an upload refusal like any other: the device is kept, nothing is
    /// written to the refused buffer, and the refusal names the volume and its
    /// size for the app to prepare a smaller one.
    #[test]
    fn a_refused_staging_buffer_leaves_the_device_usable() {
        // Each 1280 byte slice is over the device's 256 byte buffer limit.
        let Some((device, queue)) = native_test_device(256) else {
            return;
        };
        let trouble = record_device_trouble(&device);
        let mut resources = VolumeRenderResources::new(
            &device,
            wgpu::TextureFormat::Rgba8Unorm,
            egui::Context::default(),
        );
        let volume = numbered_volume();

        resources.start_upload(&device, &volume);
        resources.resolve_pending_scope(&device, &queue);

        assert!(resources.uploaded.is_none());
        assert_eq!(resources.refused_upload, Some((volume.id, 3 * 5 * 7)));
        assert!(resources.failure.is_none());
        assert!(trouble.lock().unwrap().is_empty(), "{trouble:?}");
    }
}
