//! Persistent 3D workspace state, independent of any rendering pass.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use half::f16;

use crate::app::state::{ViewportTransform, WindowLevel};
use crate::app::volume_job::{same_target_mm, volume_is_at_target};
use crate::dicom::{
    PatientAxis, PlaneImage, Volume, VolumeAxis, VolumeGeometry, auto_mpr_target_mm,
    mpr_target_within_budget, slab_half_count,
};

/// Slabs start at least this thick, matching the source application.
const DEFAULT_SLAB_THICKNESS_MM: f64 = 10.0;

/// The fixed isotropic voxel sizes the quality menu offers.
pub(in crate::app) const MPR_ISOTROPIC_TARGETS_MM: [f64; 4] = [0.7, 1.0, 1.5, 2.0];

/// How an MPR view samples the volume along its own axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum MprMode {
    /// One plane at the current index.
    Slice,
    /// The maximum over a slab centred on the current index.
    MipSlab,
}

impl MprMode {
    /// Whether the views project a slab, which has bounds to draw, drag and
    /// read out.
    pub(in crate::app) fn has_slab(self) -> bool {
        match self {
            Self::Slice => false,
            Self::MipSlab => true,
        }
    }
}

/// Which volume the MPR views sample: whatever the voxel budget allows, the
/// reconstructed source as acquired, or a fixed isotropic voxel size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) enum MprQuality {
    Auto,
    Original,
    Iso(f64),
}

impl MprQuality {
    /// The isotropic voxel size to resample the source to, or `None` to sample
    /// the reconstructed volume itself.
    pub(in crate::app) fn target_mm(self, source: &Volume) -> Option<f64> {
        match self {
            Self::Auto => auto_mpr_target_mm(source),
            Self::Original => None,
            Self::Iso(target_mm) => Some(mpr_target_within_budget(source, target_mm)),
        }
    }

    /// The choice as the menu names it, with the voxel size actually used
    /// when that differs: a size finer than the series holds, or too fine for
    /// the voxel budget, is coarsened.
    pub(in crate::app) fn label(self, source: &Volume) -> String {
        match self {
            Self::Auto => match auto_mpr_target_mm(source) {
                None => "Auto (native)".to_owned(),
                Some(target_mm) => format!("Auto ({target_mm:.2} mm)"),
            },
            Self::Original => "Original".to_owned(),
            Self::Iso(target_mm) => {
                let used_mm = mpr_target_within_budget(source, target_mm);

                if same_target_mm(used_mm, target_mm) {
                    format!("{target_mm:.1} mm")
                } else {
                    format!("{target_mm:.1} mm ({used_mm:.2} mm)")
                }
            }
        }
    }
}

/// The view of a series' MPR workspace, kept for when the user returns to it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) struct SavedMprView {
    /// The crosshair as cell-centred fractions of each axis.
    pub(in crate::app) crosshair_fractions: [f64; 3],
    pub(in crate::app) slab_mm: [f64; 3],
    pub(in crate::app) window: WindowLevel,
    pub(in crate::app) view_transforms: [ViewportTransform; 3],
}

/// What a held pointer button moves in one MPR view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) enum MprDrag {
    /// The crosshair of the view along `view`.
    Crosshair { view: VolumeAxis },
    /// One slab boundary of `axis`, grabbed in the view along `view`,
    /// `grab_offset` points from where it is drawn, so the drag moves it by
    /// the pointer's motion rather than snapping it to the press.
    SlabBoundary {
        view: VolumeAxis,
        axis: VolumeAxis,
        grab_offset: f32,
    },
    /// The zoom of the view along `view`, about the point the drag began on.
    Zoom {
        view: VolumeAxis,
        anchor: egui::Pos2,
    },
    /// The pan of the view along `view`.
    Pan { view: VolumeAxis },
}

/// Everything the three MPR views need to keep between frames.
pub(in crate::app) struct MprState {
    /// The source volume, or a resampled grid with the same physical extent.
    pub(in crate::app) volume: Arc<Volume>,
    /// Bumped whenever `volume` is replaced, so cached textures are dropped.
    pub(in crate::app) volume_generation: u64,
    /// Current plane index per volume axis, indexed by [`VolumeAxis::index`].
    pub(in crate::app) indices: [usize; 3],
    /// Slab thickness in millimetres per volume axis.
    pub(in crate::app) slab_mm: [f64; 3],
    /// Which volume the views sample, driving the quality menu and resampling.
    pub(in crate::app) quality: MprQuality,
    pub(in crate::app) window: WindowLevel,
    /// The window `Reset WL` restores.
    pub(in crate::app) default_window: WindowLevel,
    /// Wheel motion not yet worth a whole slice, per volume axis.
    pub(in crate::app) wheel_remainder: [f32; 3],
    /// What the pointer is currently moving, latched when a drag starts.
    pub(in crate::app) drag: Option<MprDrag>,
    /// Zoom and pan of the view along each volume axis; flips stay derived
    /// from the geometry.
    pub(in crate::app) view_transforms: [ViewportTransform; 3],
    textures: [Option<PlaneTexture>; 3],
}

impl MprState {
    pub(in crate::app) fn new(volume: Arc<Volume>, window: WindowLevel) -> Self {
        let mut state = Self {
            volume,
            volume_generation: 0,
            indices: [0; 3],
            slab_mm: [DEFAULT_SLAB_THICKNESS_MM; 3],
            quality: MprQuality::Auto,
            window,
            default_window: window,
            wheel_remainder: [0.0; 3],
            drag: None,
            view_transforms: [ViewportTransform::default(); 3],
            textures: [None, None, None],
        };
        state.recenter();
        state
    }

    /// Re-centre every plane at its fitted size and restore the default slab
    /// thickness.
    pub(in crate::app) fn recenter(&mut self) {
        self.view_transforms = [ViewportTransform::default(); 3];

        for axis in VolumeAxis::ALL {
            self.indices[axis.index()] = self.volume.axis_len(axis) / 2;
            self.slab_mm[axis.index()] = self
                .volume
                .axis_spacing(axis)
                .max(DEFAULT_SLAB_THICKNESS_MM);
        }

        self.textures = [None, None, None];
    }

    /// Where the planes stand and how the views look, as fractions of the
    /// volume's extent, so another grid of the series can be put back the
    /// same way.
    pub(in crate::app) fn saved_view(&self) -> SavedMprView {
        SavedMprView {
            crosshair_fractions: VolumeAxis::ALL.map(|axis| {
                (self.index(axis) as f64 + 0.5) / self.volume.axis_len(axis).max(1) as f64
            }),
            slab_mm: self.slab_mm,
            window: self.window,
            view_transforms: self.view_transforms,
        }
    }

    /// Put a saved view back on this workspace.
    pub(in crate::app) fn restore_view(&mut self, saved: &SavedMprView) {
        for axis in VolumeAxis::ALL {
            let fraction = saved.crosshair_fractions[axis.index()];
            self.set_index(
                axis,
                (fraction * self.volume.axis_len(axis) as f64) as usize,
            );
            self.set_slab_thickness_mm(axis, saved.slab_mm[axis.index()]);
        }

        self.window = saved.window;
        self.view_transforms = saved.view_transforms;
        self.textures = [None, None, None];
    }

    /// Show `volume`, another grid of the same series, instead of the current
    /// one. Both grids cover the same patient extent, so the crosshair keeps
    /// its patient position, each slab its thickness and each view its zoom
    /// and pan; only what was derived from the old voxels is dropped.
    pub(in crate::app) fn install_volume(&mut self, volume: Arc<Volume>) {
        if Arc::ptr_eq(&self.volume, &volume) {
            return;
        }

        let previous = std::mem::replace(&mut self.volume, volume);

        for axis in VolumeAxis::ALL {
            // Through the cell-centred fraction both grids share.
            let previous_len = previous.axis_len(axis).max(1);
            let fraction = (self.indices[axis.index()].min(previous_len - 1) as f64 + 0.5)
                / previous_len as f64;
            self.indices[axis.index()] = ((fraction * self.volume.axis_len(axis) as f64) as usize)
                .min(self.last_index(axis));
            self.set_slab_thickness_mm(axis, self.slab_mm[axis.index()]);
        }

        self.volume_generation = self.volume_generation.wrapping_add(1);
        self.drag = None;
        self.clear_wheel_remainders();
        self.textures = [None, None, None];
    }

    pub(in crate::app) fn reset_window(&mut self) {
        self.window = self.default_window;
    }

    pub(in crate::app) fn clear_wheel_remainders(&mut self) {
        self.wheel_remainder = [0.0; 3];
    }

    /// Move one plane to `index`, clamped to the volume.
    pub(in crate::app) fn set_index(&mut self, axis: VolumeAxis, index: usize) {
        self.indices[axis.index()] = index.min(self.last_index(axis));
    }

    /// Move one plane by whole slices, clamped to the volume.
    pub(in crate::app) fn step_index(&mut self, axis: VolumeAxis, steps: isize) {
        let stepped = (self.indices[axis.index()] as isize).saturating_add(steps);

        self.set_index(axis, stepped.max(0) as usize);
    }

    /// The current plane index of `axis`, clamped to the volume.
    pub(in crate::app) fn index(&self, axis: VolumeAxis) -> usize {
        self.indices[axis.index()].min(self.last_index(axis))
    }

    /// Set one axis' slab thickness, never thinner than a single slice.
    pub(in crate::app) fn set_slab_thickness_mm(&mut self, axis: VolumeAxis, thickness_mm: f64) {
        self.slab_mm[axis.index()] = self.volume.axis_spacing(axis).max(thickness_mm);
    }

    fn last_index(&self, axis: VolumeAxis) -> usize {
        self.volume.axis_len(axis).saturating_sub(1)
    }

    /// How many slices on each side of the current index a slab covers.
    pub(in crate::app) fn slab_half_count(&self, axis: VolumeAxis) -> usize {
        slab_half_count(self.slab_mm[axis.index()], self.volume.axis_spacing(axis))
    }

    /// The cached plane texture for `axis`, rebuilt through `build_texture`
    /// whenever the key it was built from no longer matches.
    ///
    /// The plane itself is kept with its texture. A change of window alone,
    /// as every frame of a WL drag is, re-windows that plane through `upload`
    /// instead of extracting or projecting it again through `build_plane`.
    pub(in crate::app) fn plane_texture(
        &mut self,
        axis: VolumeAxis,
        key: PlaneTextureKey,
        build_plane: impl FnOnce() -> PlaneImage,
        upload: impl FnOnce(&PlaneImage) -> egui::TextureHandle,
    ) -> egui::TextureId {
        let slot = &mut self.textures[axis.index()];

        match slot {
            Some(cached) if cached.key == key => {}
            Some(cached) if cached.key.same_plane(&key) => {
                cached.texture = upload(&cached.plane);
                cached.key = key;
            }
            _ => {
                let plane = build_plane();
                *slot = Some(PlaneTexture {
                    key,
                    texture: upload(&plane),
                    plane,
                });
            }
        }

        slot.as_ref()
            .map_or(egui::TextureId::default(), |cached| cached.texture.id())
    }
}

struct PlaneTexture {
    key: PlaneTextureKey,
    /// The extracted or projected plane the texture was windowed from.
    plane: PlaneImage,
    texture: egui::TextureHandle,
}

/// Everything a rendered plane texture depends on. Compared as a whole, so
/// window values are kept as bits instead of floats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) struct PlaneTextureKey {
    volume_generation: u64,
    index: usize,
    mode: MprMode,
    half_count: usize,
    window_bits: (u64, u64),
    /// The whole factors the plane's columns and rows were reduced by.
    reduction: [usize; 2],
}

impl PlaneTextureKey {
    pub(in crate::app) fn new(
        volume_generation: u64,
        index: usize,
        mode: MprMode,
        half_count: usize,
        window: WindowLevel,
        reduction: [usize; 2],
    ) -> Self {
        Self {
            volume_generation,
            index,
            mode,
            half_count,
            window_bits: (window.center.to_bits(), window.width.to_bits()),
            reduction,
        }
    }

    /// Whether both keys name the same plane, whatever its window.
    fn same_plane(&self, other: &Self) -> bool {
        Self {
            window_bits: other.window_bits,
            ..*self
        } == *other
    }
}

// The volume rendering half of the workspace: the GPU-ready volume, the
// transfer functions baked for it, and the camera the shader is driven from.

/// Voxels the GPU volume may hold. 48 M half-precision texels are 96 MB of
/// VRAM, and the upload stages as much again, with its rows padded, in
/// host-visible memory while it runs.
pub(in crate::app) const MAX_GPU_VOXELS: u64 = 48_000_000;

/// Degrees the camera turns per dragged pixel.
pub(in crate::app) const VOLUME_ROTATE_DEG_PER_PX: f32 = 0.4;
/// How much one scrolled point zooms the volume.
pub(in crate::app) const VOLUME_WHEEL_ZOOM_SENSITIVITY: f32 = 0.004;

const MIN_VOLUME_ZOOM: f32 = 0.1;
const MAX_VOLUME_ZOOM: f32 = 20.0;
/// Keeping the camera short of the poles keeps the up vector well defined.
const MAX_VOLUME_PITCH_DEG: f32 = 89.0;

static NEXT_GPU_VOLUME_ID: AtomicU64 = AtomicU64::new(1);

/// Attempts at a volume rendering texture, each at half the voxels of the one
/// the device refused, before the volume is reported as too large.
const MAX_GPU_ATTEMPTS: u8 = 3;

/// How many voxels a workspace's volume rendering texture may have, lowered
/// each time the device refuses one. It belongs to one workspace, so a series
/// that did not fit leaves the next one its full budget.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) struct GpuBudget {
    pub(in crate::app) max_voxels: u64,
    refusals: u8,
}

impl GpuBudget {
    pub(in crate::app) const FULL: Self = Self {
        max_voxels: MAX_GPU_VOXELS,
        refusals: 0,
    };

    /// The budget after the device refused a texture of `voxels`: half of
    /// them, so the next attempt fits where the last one failed.
    pub(in crate::app) fn after_refusal(self, voxels: u64) -> Self {
        Self {
            max_voxels: (voxels / 2).max(1),
            refusals: self.refusals.saturating_add(1),
        }
    }

    /// Whether the attempts are spent and the volume does not fit.
    pub(in crate::app) fn exhausted(self) -> bool {
        self.refusals >= MAX_GPU_ATTEMPTS
    }
}

/// One volume prepared for the GPU: half-precision voxels, already resampled
/// to fit both the device's texture limit and the VRAM budget.
pub(in crate::app) struct GpuVolume {
    /// Identifies this volume to the renderer, which uploads again on a change.
    pub(in crate::app) id: u64,
    pub(in crate::app) dims: [u32; 3],
    pub(in crate::app) spacing_mm: [f32; 3],
    /// What one unit of texel stands for: each texel holds its value divided
    /// by this power of two, which is 1 unless the values exceed half
    /// precision.
    pub(in crate::app) texel_scale: f32,
    /// Texels waiting for their first upload, dropped once one succeeds.
    texels: Mutex<Option<Vec<u16>>>,
}

impl GpuVolume {
    #[cfg(test)]
    pub(in crate::app) fn from_volume(source: &Volume, max_texture_dimension_3d: u32) -> Self {
        Self::within_budget(source, max_texture_dimension_3d, MAX_GPU_VOXELS)
    }

    #[cfg(test)]
    fn within_budget(source: &Volume, max_texture_dimension_3d: u32, max_voxels: u64) -> Self {
        let never_cancelled = AtomicBool::new(false);

        match Self::prepare(
            source,
            max_texture_dimension_3d,
            max_voxels,
            &never_cancelled,
        ) {
            Some(volume) => volume,
            None => unreachable!("a preparation nobody can cancel was cancelled"),
        }
    }

    /// The texture for `source` within the device's texture limit and
    /// `max_voxels`, or `None` once `cancel` is set.
    pub(in crate::app) fn prepare(
        source: &Volume,
        max_texture_dimension_3d: u32,
        max_voxels: u64,
        cancel: &AtomicBool,
    ) -> Option<Self> {
        let target_mm =
            source.isotropic_target_within(max_voxels, max_texture_dimension_3d as usize);
        // A source already on the target grid is converted as it is, without
        // a full single-precision copy.
        let resampled;
        let volume = if volume_is_at_target(source, source, target_mm) {
            source
        } else {
            resampled = source.resample_isotropic_unless_cancelled(target_mm, cancel)?;
            &resampled
        };
        // Half precision holds whole numbers exactly up to 2048 and to within 2
        // up to 4096, so the values are stored as they are. Normalising them
        // over their range instead would let one padding value such as -32768
        // coarsen every tissue value to steps of 16 HU or more. Values beyond
        // half precision's range are divided by just enough of a power of two.
        let (minimum, maximum) = volume.value_range;
        let largest = minimum.abs().max(maximum.abs());
        let texel_scale = if largest > f16::MAX.to_f32() {
            (largest / f16::MAX.to_f32()).log2().ceil().exp2()
        } else {
            1.0
        };
        let texels = volume
            .data()
            .iter()
            .map(|value| f16::from_f32(value / texel_scale).to_bits())
            .collect();

        Some(Self {
            id: NEXT_GPU_VOLUME_ID.fetch_add(1, Ordering::Relaxed),
            dims: [
                volume.width as u32,
                volume.height as u32,
                volume.depth as u32,
            ],
            spacing_mm: volume.spacing.map(|spacing| spacing as f32),
            texel_scale,
            texels: Mutex::new(Some(texels)),
        })
    }

    /// The millimetre extent the texture covers.
    pub(in crate::app) fn size_mm(&self) -> [f32; 3] {
        std::array::from_fn(|axis| self.dims[axis] as f32 * self.spacing_mm[axis])
    }

    /// Take the staging texels for the upload. `None` once they are taken: a
    /// refused upload is followed by a smaller volume, not another attempt at
    /// this one.
    pub(in crate::app) fn take_texels(&self) -> Option<Vec<u16>> {
        lock(&self.texels).take()
    }
}

/// Lock a mutex this module only ever holds across plain data moves.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The fragment shader's uniform block. Every member is a `vec4<f32>`: WGSL
/// aligns `vec3<f32>` to 16 bytes while `#[repr(C)] [f32; 3]` packs to 4, so a
/// mixed layout would be smaller here than in the shader and the bind group
/// would be rejected.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(in crate::app) struct VolumeUniforms {
    /// Camera position in volume millimetres, then the orthographic half height.
    pub(in crate::app) eye_mm_ortho_half_height: [f32; 4],
    /// Screen-right direction, then the viewport's width / height ratio.
    pub(in crate::app) right_aspect: [f32; 4],
    /// Screen-up direction, then the ray marching step in millimetres.
    pub(in crate::app) up_step_mm: [f32; 4],
    /// View direction, then the distance one unit of opacity is defined over.
    pub(in crate::app) forward_opacity_unit_mm: [f32; 4],
    /// Volume extent in millimetres, then the value a texel of 0 stands for.
    pub(in crate::app) size_mm_value_min: [f32; 4],
    /// Texture size in voxels, then the value a texel of 1 stands for.
    pub(in crate::app) dims_value_max: [f32; 4],
    /// Viewport origin and size in physical pixels.
    pub(in crate::app) viewport_min_size_px: [f32; 4],
    /// Transfer function domain, the step limit, and shading on or off.
    pub(in crate::app) lut_range_steps_shading: [f32; 4],
    /// Ambient, diffuse and specular weights, then the specular power.
    pub(in crate::app) ambient_diffuse_spec_shine: [f32; 4],
    /// Background colour, then whether the target expects linear output.
    pub(in crate::app) background_srgb: [f32; 4],
}

/// How voxel values become colour and opacity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::app) enum TransferPreset {
    CtSoftTissue,
    CtBone,
    Grayscale,
}

impl TransferPreset {
    pub(in crate::app) const ALL: [Self; 3] = [Self::CtSoftTissue, Self::CtBone, Self::Grayscale];

    /// The preset a series opens with. The CT presets map Hounsfield units,
    /// which no other modality measures in: an MR or PET under them is a
    /// near-opaque box, so everything else opens in Grayscale on its window.
    pub(in crate::app) fn for_modality(modality: Option<&str>) -> Self {
        if modality.is_some_and(|modality| modality.trim().eq_ignore_ascii_case("CT")) {
            Self::CtSoftTissue
        } else {
            Self::Grayscale
        }
    }

    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::CtSoftTissue => "CT Soft Tissue",
            Self::CtBone => "CT Bone",
            Self::Grayscale => "Grayscale",
        }
    }

    /// The value range the baked table covers. The CT presets keep the source
    /// application's Hounsfield range; the grayscale preset follows the window
    /// the MPR planes are shown with.
    fn domain(self, window: WindowLevel) -> (f32, f32) {
        match self {
            Self::CtSoftTissue | Self::CtBone => (CT_DOMAIN_MIN_HU, CT_DOMAIN_MAX_HU),
            Self::Grayscale => {
                let half_width = window.width.max(1.0) / 2.0;

                (
                    (window.center - half_width) as f32,
                    (window.center + half_width) as f32,
                )
            }
        }
    }

    /// The colour and opacity one value maps to, clamped outside the domain.
    /// An inverted series, MONOCHROME1, takes its greys the other way round;
    /// the opacity still follows the values, so the same structures show.
    fn sample(self, value: f32, domain: (f32, f32), inverted: bool) -> [u8; 4] {
        let (color, opacity) = match self {
            Self::CtSoftTissue => (
                interpolate(&CT_COLOR_POINTS, value),
                interpolate(&CT_SOFT_TISSUE_OPACITY_POINTS, value)[0],
            ),
            Self::CtBone => (
                interpolate(&CT_COLOR_POINTS, value),
                interpolate(&CT_BONE_OPACITY_POINTS, value)[0],
            ),
            Self::Grayscale => {
                let span = domain.1 - domain.0;
                let level = if span > 0.0 {
                    ((value - domain.0) / span).clamp(0.0, 1.0)
                } else {
                    0.0
                };

                let gray = if inverted { 1.0 - level } else { level };

                ([gray; 3], level * GRAYSCALE_MAX_OPACITY)
            }
        };

        [
            to_byte(color[0]),
            to_byte(color[1]),
            to_byte(color[2]),
            to_byte(opacity),
        ]
    }
}

const CT_DOMAIN_MIN_HU: f32 = -1000.0;
const CT_DOMAIN_MAX_HU: f32 = 1500.0;
const GRAYSCALE_MAX_OPACITY: f32 = 0.8;
/// Entries in a baked transfer function, and texels in the texture it is
/// uploaded to.
pub(in crate::app) const TRANSFER_FUNCTION_ENTRIES: usize = 256;

/// Hounsfield colour control points, from the source application.
const CT_COLOR_POINTS: [(f32, [f32; 3]); 6] = [
    (-1000.0, [0.0, 0.0, 0.0]),
    (-100.0, [0.1, 0.1, 0.1]),
    (40.0, [0.55, 0.35, 0.25]),
    (200.0, [0.9, 0.75, 0.55]),
    (500.0, [1.0, 0.95, 0.85]),
    (1500.0, [1.0, 1.0, 1.0]),
];
const CT_SOFT_TISSUE_OPACITY_POINTS: [(f32, [f32; 1]); 7] = [
    (-1000.0, [0.0]),
    (-100.0, [0.0]),
    (20.0, [0.02]),
    (100.0, [0.08]),
    (300.0, [0.2]),
    (800.0, [0.5]),
    (1500.0, [0.85]),
];
const CT_BONE_OPACITY_POINTS: [(f32, [f32; 1]); 5] = [
    (-1000.0, [0.0]),
    (150.0, [0.0]),
    (300.0, [0.15]),
    (700.0, [0.6]),
    (1500.0, [0.9]),
];

/// A baked transfer function, ready to upload as a one-row RGBA texture.
pub(in crate::app) struct TransferFunctionLut {
    /// Identifies the baked table, so the renderer uploads only on a change.
    pub(in crate::app) id: u64,
    pub(in crate::app) domain: (f32, f32),
    pub(in crate::app) rgba: [[u8; 4]; TRANSFER_FUNCTION_ENTRIES],
}

impl TransferFunctionLut {
    pub(in crate::app) fn bake(
        preset: TransferPreset,
        window: WindowLevel,
        inverted: bool,
    ) -> Self {
        let domain = preset.domain(window);
        let mut rgba = [[0_u8; 4]; TRANSFER_FUNCTION_ENTRIES];

        for (entry_index, entry) in rgba.iter_mut().enumerate() {
            *entry = preset.sample(transfer_entry_value(domain, entry_index), domain, inverted);
        }

        Self {
            id: transfer_function_id(preset, domain, inverted),
            domain,
            rgba,
        }
    }

    /// The entry nearest a value. The shader samples the table linearly, so it
    /// reads exactly this entry only at the entry's centre and blends towards
    /// the neighbour in between.
    #[cfg(test)]
    fn entry_for(&self, value: f32) -> [u8; 4] {
        let span = self.domain.1 - self.domain.0;
        let fraction = if span > 0.0 {
            ((value - self.domain.0) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let entry_index = ((fraction * TRANSFER_FUNCTION_ENTRIES as f32 - 0.5).round() as isize)
            .clamp(0, TRANSFER_FUNCTION_ENTRIES as isize - 1) as usize;

        self.rgba[entry_index]
    }
}

/// The value the centre of one table entry stands for.
fn transfer_entry_value(domain: (f32, f32), entry_index: usize) -> f32 {
    let fraction = (entry_index as f32 + 0.5) / TRANSFER_FUNCTION_ENTRIES as f32;

    domain.0 + (domain.1 - domain.0) * fraction
}

/// Identify a baked table by what went into it, so re-baking the same choice
/// keeps the uploaded texture.
fn transfer_function_id(preset: TransferPreset, domain: (f32, f32), inverted: bool) -> u64 {
    let mut hasher = DefaultHasher::new();

    preset.hash(&mut hasher);
    inverted.hash(&mut hasher);
    domain.0.to_bits().hash(&mut hasher);
    domain.1.to_bits().hash(&mut hasher);

    hasher.finish()
}

/// Piecewise linear interpolation over control points, clamped outside them.
fn interpolate<const N: usize>(points: &[(f32, [f32; N])], value: f32) -> [f32; N] {
    let Some((first_value, first)) = points.first().copied() else {
        return [0.0; N];
    };

    if value <= first_value {
        return first;
    }

    for pair in points.windows(2) {
        let (low_value, low) = pair[0];
        let (high_value, high) = pair[1];

        if value <= high_value {
            let span = high_value - low_value;
            let fraction = if span > 0.0 {
                (value - low_value) / span
            } else {
                0.0
            };

            return std::array::from_fn(|axis| low[axis] + (high[axis] - low[axis]) * fraction);
        }
    }

    points.last().map_or([0.0; N], |(_, last)| *last)
}

fn to_byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// An orthonormal camera frame in volume millimetres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) struct CameraBasis {
    pub(in crate::app) right: [f32; 3],
    pub(in crate::app) up: [f32; 3],
    pub(in crate::app) forward: [f32; 3],
}

/// The anterior view every volume starts from: standing in front of the
/// patient, looking towards their back, with the head up. Both directions come
/// from the patient axes rather than the volume's own, so the view is the same
/// whichever plane the series was acquired in, and `right` follows from them so
/// patient left always lands on the right of the screen.
pub(in crate::app) fn default_camera_basis(geometry: &VolumeGeometry) -> CameraBasis {
    // Every direction is read from the patient axes, never from a cross
    // product: a stack built in descending order maps the volume onto the
    // patient with a reflection, and `cross(forward, up)` would follow the
    // volume's handedness there instead of the patient's and put the
    // patient's right on the right of the screen. The index sorts slices
    // ascending, so the app never builds one, but the builder accepts it.
    CameraBasis {
        right: geometry.volume_direction_towards(PatientAxis::LeftRight),
        up: geometry.volume_direction_towards(PatientAxis::SuperiorInferior),
        forward: geometry.volume_direction_towards(PatientAxis::AnteriorPosterior),
    }
}

/// The camera and transfer function the volume rendering view keeps.
#[derive(Clone, Copy)]
pub(in crate::app) struct VolumeRenderState {
    /// Camera azimuth about the volume's up axis, in degrees.
    pub(in crate::app) yaw_deg: f32,
    /// Camera elevation, clamped short of the poles.
    pub(in crate::app) pitch_deg: f32,
    /// 1.0 fits the volume's longest side across the view's shorter side; a
    /// turned volume can still reach past the edges.
    pub(in crate::app) zoom: f32,
    /// Camera offset across the view, in millimetres.
    pub(in crate::app) pan_mm: [f32; 2],
    pub(in crate::app) preset: TransferPreset,
    pub(in crate::app) shading: bool,
}

impl Default for VolumeRenderState {
    fn default() -> Self {
        Self {
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            zoom: 1.0,
            pan_mm: [0.0; 2],
            preset: TransferPreset::CtSoftTissue,
            shading: true,
        }
    }
}

impl VolumeRenderState {
    /// Restore the default view, keeping the transfer function and shading the
    /// user chose.
    pub(in crate::app) fn reset_view(&mut self) {
        *self = Self {
            preset: self.preset,
            shading: self.shading,
            ..Self::default()
        };
    }

    /// Orbit the camera. Dragging moves the volume with the pointer, so the
    /// camera turns the other way.
    pub(in crate::app) fn rotate_by(&mut self, delta_px: egui::Vec2) {
        self.yaw_deg = (self.yaw_deg - delta_px.x * VOLUME_ROTATE_DEG_PER_PX).rem_euclid(360.0);
        self.pitch_deg = (self.pitch_deg - delta_px.y * VOLUME_ROTATE_DEG_PER_PX)
            .clamp(-MAX_VOLUME_PITCH_DEG, MAX_VOLUME_PITCH_DEG);
    }

    pub(in crate::app) fn zoom_by(&mut self, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }

        self.zoom = (self.zoom * factor).clamp(MIN_VOLUME_ZOOM, MAX_VOLUME_ZOOM);
    }

    /// Slide the camera across the view, so the volume follows the pointer.
    pub(in crate::app) fn pan_by(&mut self, delta_px: egui::Vec2, mm_per_px: f32) {
        self.pan_mm[0] -= delta_px.x * mm_per_px;
        self.pan_mm[1] += delta_px.y * mm_per_px;
    }

    /// The orthographic half height that shows a volume of `extent_mm` in a
    /// view `aspect` times as wide as it is tall. A view narrower than it is
    /// tall fits the extent across its width instead of its height.
    pub(in crate::app) fn ortho_half_height_mm(&self, extent_mm: f32, aspect: f32) -> f32 {
        let narrowness = if aspect.is_finite() && aspect > 0.0 {
            aspect.min(1.0)
        } else {
            1.0
        };

        0.5 * extent_mm / self.zoom.max(MIN_VOLUME_ZOOM) / narrowness
    }

    /// The camera frame the current orbit leaves, yaw about the volume's up
    /// axis and pitch about the turned right axis.
    ///
    /// Each turn mixes two vectors of the patient-derived frame, never a cross
    /// product, so it turns the patient the same way whether or not the
    /// volume maps onto the patient with a reflection.
    pub(in crate::app) fn camera_basis(&self, geometry: &VolumeGeometry) -> CameraBasis {
        let base = default_camera_basis(geometry);
        let (yaw_sine, yaw_cosine) = self.yaw_deg.to_radians().sin_cos();
        let right = normalize(mix(base.right, yaw_cosine, base.forward, yaw_sine));
        let forward = mix(base.forward, yaw_cosine, base.right, -yaw_sine);
        let (pitch_sine, pitch_cosine) = self.pitch_deg.to_radians().sin_cos();

        CameraBasis {
            right,
            up: normalize(mix(base.up, pitch_cosine, forward, -pitch_sine)),
            forward: normalize(mix(forward, pitch_cosine, base.up, pitch_sine)),
        }
    }
}

fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn normalize(vector: [f32; 3]) -> [f32; 3] {
    let length = dot(vector, vector).sqrt();

    if length > 0.0 {
        vector.map(|component| component / length)
    } else {
        vector
    }
}

/// `first * first_weight + second * second_weight`.
fn mix(first: [f32; 3], first_weight: f32, second: [f32; 3], second_weight: f32) -> [f32; 3] {
    std::array::from_fn(|component| {
        first[component] * first_weight + second[component] * second_weight
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> MprState {
        let volume = Arc::new(Volume::new(4, 6, 8, [1.0, 1.0, 2.0], vec![0.0; 4 * 6 * 8]));

        MprState::new(
            volume,
            WindowLevel {
                center: 40.0,
                width: 350.0,
            },
        )
    }

    #[test]
    fn a_new_state_centers_every_plane() {
        let state = test_state();

        assert_eq!(state.indices, [2, 3, 4]);
        assert_eq!(state.slab_mm, [10.0, 10.0, 10.0]);
    }

    #[test]
    fn recentering_restores_indices_slabs_and_view_transforms() {
        let mut state = test_state();
        state.indices = [0, 0, 0];
        state.slab_mm = [1.0, 1.0, 1.0];
        state.view_transforms[0].zoom = 3.0;
        state.view_transforms[2].pan = egui::vec2(12.0, -4.0);

        state.recenter();

        assert_eq!(state.indices, [2, 3, 4]);
        assert_eq!(state.slab_mm, [10.0, 10.0, 10.0]);
        assert_eq!(state.view_transforms, [ViewportTransform::default(); 3]);
    }

    #[test]
    fn slab_half_counts_follow_the_axis_spacing() {
        let state = test_state();

        // 10 mm slabs: nine 1 mm slices across x, five 2 mm ones along z.
        assert_eq!(state.slab_half_count(VolumeAxis::X), 4);
        assert_eq!(state.slab_half_count(VolumeAxis::Z), 2);
    }

    #[test]
    /// Another grid of the same extent keeps the crosshair at its patient
    /// position, every slab at least one of its slices thick, and the views
    /// as zoomed and panned.
    fn installing_a_volume_keeps_the_view_where_it_was() {
        let mut state = test_state();
        state.indices = [3, 0, 4];
        state.slab_mm = [1.0, 1.0, 1.0];
        state.view_transforms[VolumeAxis::Z.index()].zoom = 3.0;

        // 4 x 6 x 16 mm, as the 4 x 6 x 8 source.
        state.install_volume(Arc::new(Volume::new(
            2,
            3,
            4,
            [2.0, 2.0, 4.0],
            vec![0.0; 24],
        )));

        assert_eq!(state.volume_generation, 1);
        assert_eq!(state.indices, [1, 0, 2]);
        assert_eq!(state.slab_mm, [2.0, 2.0, 4.0]);
        assert_eq!(state.view_transforms[VolumeAxis::Z.index()].zoom, 3.0);
    }

    #[test]
    fn plane_indices_stay_inside_the_volume() {
        let mut state = test_state();

        state.set_index(VolumeAxis::X, 99);
        assert_eq!(state.index(VolumeAxis::X), 3);

        state.step_index(VolumeAxis::X, -10);
        assert_eq!(state.index(VolumeAxis::X), 0);

        state.step_index(VolumeAxis::X, 2);
        assert_eq!(state.index(VolumeAxis::X), 2);
    }

    #[test]
    fn a_slab_is_never_thinner_than_one_slice() {
        let mut state = test_state();

        state.set_slab_thickness_mm(VolumeAxis::Z, 0.0);

        assert_eq!(state.slab_mm[VolumeAxis::Z.index()], 2.0);
    }

    #[test]
    fn resetting_the_window_restores_the_series_default() {
        let mut state = test_state();
        state.window = WindowLevel {
            center: 300.0,
            width: 2000.0,
        };

        state.reset_window();

        assert_eq!(state.window, state.default_window);
        assert_eq!(state.window.center, 40.0);
    }

    #[test]
    fn quality_choices_name_their_own_voxel_size() {
        let source = Volume::new(4, 4, 2, [0.5, 0.5, 2.0], vec![0.0; 32]);

        // A small anisotropic volume is within budget, so Auto keeps it as is.
        assert_eq!(MprQuality::Auto.target_mm(&source), None);
        assert_eq!(MprQuality::Auto.label(&source), "Auto (native)");
        assert_eq!(MprQuality::Original.target_mm(&source), None);
        assert_eq!(MprQuality::Original.label(&source), "Original");
        assert_eq!(MprQuality::Iso(1.5).target_mm(&source), Some(1.5));
        assert_eq!(MprQuality::Iso(1.5).label(&source), "1.5 mm");
    }

    #[test]
    fn texture_keys_differ_in_every_component() {
        let window = WindowLevel {
            center: 40.0,
            width: 350.0,
        };
        let key = PlaneTextureKey::new(1, 2, MprMode::Slice, 3, window, [1, 1]);
        let windowed = |center, width| {
            PlaneTextureKey::new(
                1,
                2,
                MprMode::Slice,
                3,
                WindowLevel { center, width },
                [1, 1],
            )
        };

        assert_eq!(
            key,
            PlaneTextureKey::new(1, 2, MprMode::Slice, 3, window, [1, 1])
        );
        assert_ne!(
            key,
            PlaneTextureKey::new(2, 2, MprMode::Slice, 3, window, [1, 1])
        );
        assert_ne!(
            key,
            PlaneTextureKey::new(1, 3, MprMode::Slice, 3, window, [1, 1])
        );
        assert_ne!(
            key,
            PlaneTextureKey::new(1, 2, MprMode::MipSlab, 3, window, [1, 1])
        );
        assert_ne!(
            key,
            PlaneTextureKey::new(1, 2, MprMode::Slice, 4, window, [1, 1])
        );
        assert_ne!(key, windowed(41.0, 350.0));
        assert_ne!(key, windowed(40.0, 351.0));
        assert_ne!(
            key,
            PlaneTextureKey::new(1, 2, MprMode::Slice, 3, window, [2, 1])
        );
    }

    /// Dragging WL changes only the window, which re-windows the plane kept
    /// with the texture; a new slice projects the plane again.
    #[test]
    fn a_window_change_rewindows_without_projecting_again() {
        let context = egui::Context::default();
        let mut state = test_state();
        let mut projections = 0;
        let mut uploads = 0;
        let window = |center| WindowLevel {
            center,
            width: 350.0,
        };
        let mut show = |state: &mut MprState, index, center| {
            let key = PlaneTextureKey::new(0, index, MprMode::MipSlab, 2, window(center), [1, 1]);
            state.plane_texture(
                VolumeAxis::Z,
                key,
                || {
                    projections += 1;
                    PlaneImage {
                        columns: 1,
                        rows: 1,
                        pixels: vec![0.0],
                    }
                },
                |_| {
                    uploads += 1;
                    context.load_texture(
                        "plane",
                        egui::ColorImage::new([1, 1], vec![egui::Color32::BLACK]),
                        egui::TextureOptions::LINEAR,
                    )
                },
            );
        };

        show(&mut state, 3, 40.0);
        show(&mut state, 3, 40.0);
        show(&mut state, 3, 60.0);
        show(&mut state, 3, 80.0);
        show(&mut state, 4, 80.0);

        assert_eq!(projections, 2);
        assert_eq!(uploads, 4);
    }

    fn ct_window() -> WindowLevel {
        WindowLevel {
            center: 40.0,
            width: 400.0,
        }
    }

    /// Half-open colour comparison: the baked entry a value falls in is one
    /// step away from the control point itself.
    fn assert_channels_close(actual: [u8; 4], expected: [u8; 4]) {
        for (channel, (actual, expected)) in actual.into_iter().zip(expected).enumerate() {
            assert!(
                actual.abs_diff(expected) <= 2,
                "channel {channel}: {actual} is not within 2 of {expected}"
            );
        }
    }

    fn assert_unit_length(vector: [f32; 3]) {
        let length = dot(vector, vector).sqrt();

        assert!(
            (length - 1.0).abs() < 1.0e-5,
            "{vector:?} is not a unit vector"
        );
    }

    #[test]
    fn the_uniform_block_matches_the_shader_layout() {
        assert_eq!(size_of::<VolumeUniforms>(), 160);
        assert_eq!(
            std::mem::offset_of!(VolumeUniforms, eye_mm_ortho_half_height),
            0
        );
        assert_eq!(std::mem::offset_of!(VolumeUniforms, right_aspect), 16);
        assert_eq!(std::mem::offset_of!(VolumeUniforms, up_step_mm), 32);
        assert_eq!(
            std::mem::offset_of!(VolumeUniforms, forward_opacity_unit_mm),
            48
        );
        assert_eq!(std::mem::offset_of!(VolumeUniforms, size_mm_value_min), 64);
        assert_eq!(std::mem::offset_of!(VolumeUniforms, dims_value_max), 80);
        assert_eq!(
            std::mem::offset_of!(VolumeUniforms, viewport_min_size_px),
            96
        );
        assert_eq!(
            std::mem::offset_of!(VolumeUniforms, lut_range_steps_shading),
            112
        );
        assert_eq!(
            std::mem::offset_of!(VolumeUniforms, ambient_diffuse_spec_shine),
            128
        );
        assert_eq!(std::mem::offset_of!(VolumeUniforms, background_srgb), 144);
    }

    fn test_geometry(
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

    /// The camera must start in front of the patient looking at their back,
    /// head up and patient left on the right of the screen, whatever plane the
    /// series was acquired in.
    ///
    /// Each camera vector is taken from volume axes into the patient's own
    /// coordinates, DICOM's +x left, +y posterior, +z head, through the
    /// direction cosines alone, so the check shares no code with the camera.
    fn assert_anterior_head_up(geometry: &VolumeGeometry) {
        let basis = default_camera_basis(geometry);
        let in_patient = |vector: [f32; 3]| -> [f64; 3] {
            let axes = [
                geometry.row_direction,
                geometry.column_direction,
                geometry.slice_direction,
            ];
            std::array::from_fn(|component| {
                (0..3)
                    .map(|axis| f64::from(vector[axis]) * axes[axis][component])
                    .sum()
            })
        };

        assert_eq!(in_patient(basis.forward), [0.0, 1.0, 0.0], "{geometry:?}");
        assert_eq!(in_patient(basis.up), [0.0, 0.0, 1.0], "{geometry:?}");
        assert_eq!(in_patient(basis.right), [1.0, 0.0, 0.0], "{geometry:?}");
    }

    #[test]
    fn the_default_view_faces_the_patient_with_the_head_up() {
        let axial = test_geometry([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]);

        assert_anterior_head_up(&axial);

        let basis = default_camera_basis(&axial);
        assert_eq!(basis.forward, [0.0, 1.0, 0.0]);
        assert_eq!(basis.up, [0.0, 0.0, 1.0]);
        assert_eq!(basis.right, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn the_default_view_follows_the_patient_not_the_volume_axes() {
        // Slices stacking towards the feet, a prone acquisition, and coronal
        // and sagittal acquisitions all start from the same view of the
        // patient.
        for geometry in [
            test_geometry([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]),
            test_geometry([-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0]),
            test_geometry([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
            test_geometry([0.0, 1.0, 0.0], [0.0, 0.0, -1.0], [-1.0, 0.0, 0.0]),
            // Prone and feet first together, and a coronal stack towards the
            // front.
            test_geometry([-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, -1.0]),
            test_geometry([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, -1.0, 0.0]),
        ] {
            assert_anterior_head_up(&geometry);

            let basis = default_camera_basis(&geometry);
            assert_unit_length(basis.right);
            assert_unit_length(basis.up);
            assert_unit_length(basis.forward);
            assert!(dot(basis.right, basis.up).abs() < 1.0e-6, "{geometry:?}");
            assert!(
                dot(basis.right, basis.forward).abs() < 1.0e-6,
                "{geometry:?}"
            );
        }
    }

    #[test]
    fn orbiting_keeps_the_camera_frame_orthonormal() {
        let mut render = VolumeRenderState::default();
        render.rotate_by(egui::vec2(75.0, -40.0));

        let basis = render.camera_basis(&test_geometry(
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
        ));

        assert_unit_length(basis.right);
        assert_unit_length(basis.up);
        assert_unit_length(basis.forward);
        assert!(dot(basis.right, basis.up).abs() < 1.0e-5);
        assert!(dot(basis.right, basis.forward).abs() < 1.0e-5);
        assert!(dot(basis.up, basis.forward).abs() < 1.0e-5);
    }

    #[test]
    fn dragging_turns_the_volume_towards_the_pointer() {
        let mut render = VolumeRenderState::default();
        // Dragging a quarter turn to the right turns the volume with the
        // pointer, so the patient's right side swings round to face the camera
        // and the view direction becomes the volume's +x axis.
        render.rotate_by(egui::vec2(90.0 / VOLUME_ROTATE_DEG_PER_PX, 0.0));

        let basis = render.camera_basis(&test_geometry(
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
        ));

        assert!((basis.forward[0] - 1.0).abs() < 1.0e-5, "{basis:?}");
        assert!(basis.forward[1].abs() < 1.0e-5, "{basis:?}");
    }

    /// The orbit turns the patient the same way under the pointer whether or
    /// not the volume maps onto the patient with a reflection, as a stack
    /// running towards the feet does.
    #[test]
    fn orbiting_turns_the_patient_the_same_way_in_a_reflected_volume() {
        let in_patient = |geometry: &VolumeGeometry, vector: [f32; 3]| -> [i32; 3] {
            let axes = [
                geometry.row_direction,
                geometry.column_direction,
                geometry.slice_direction,
            ];
            std::array::from_fn(|component| {
                let value: f64 = (0..3)
                    .map(|axis| f64::from(vector[axis]) * axes[axis][component])
                    .sum();
                value.round() as i32
            })
        };
        let head_first = test_geometry([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]);
        let reflected = test_geometry([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]);
        let mut render = VolumeRenderState::default();
        render.rotate_by(egui::vec2(
            90.0 / VOLUME_ROTATE_DEG_PER_PX,
            30.0 / VOLUME_ROTATE_DEG_PER_PX,
        ));

        for geometry in [&head_first, &reflected] {
            let basis = render.camera_basis(geometry);
            let reference = render.camera_basis(&head_first);
            for (vector, expected) in [
                (basis.right, reference.right),
                (basis.up, reference.up),
                (basis.forward, reference.forward),
            ] {
                assert_eq!(
                    in_patient(geometry, vector),
                    in_patient(&head_first, expected),
                    "{geometry:?}"
                );
            }
        }
    }

    #[test]
    fn the_camera_clamps_its_pitch_zoom_and_keeps_its_preset() {
        let mut render = VolumeRenderState::default();

        render.rotate_by(egui::vec2(0.0, -10_000.0));
        assert_eq!(render.pitch_deg, 89.0);

        render.zoom_by(1000.0);
        assert_eq!(render.zoom, 20.0);
        render.zoom_by(0.0);
        assert_eq!(render.zoom, 20.0);
        render.zoom_by(1.0e-6);
        assert_eq!(render.zoom, 0.1);

        // Panning moves the camera against the drag, so the volume follows it.
        render.pan_by(egui::vec2(4.0, -3.0), 2.0);
        assert_eq!(render.pan_mm, [-8.0, -6.0]);

        render.preset = TransferPreset::CtBone;
        render.shading = false;
        render.reset_view();

        assert_eq!(render.pitch_deg, 0.0);
        assert_eq!(render.zoom, 1.0);
        assert_eq!(render.pan_mm, [0.0, 0.0]);
        assert_eq!(render.preset, TransferPreset::CtBone);
        assert!(!render.shading);
    }

    #[test]
    fn the_orthographic_half_height_fits_the_volume_at_unit_zoom() {
        let mut render = VolumeRenderState::default();

        assert_eq!(render.ortho_half_height_mm(200.0, 1.5), 100.0);
        // Half as wide as tall: the 200 mm fit across the width.
        assert_eq!(render.ortho_half_height_mm(200.0, 0.5), 200.0);

        render.zoom_by(2.0);

        assert_eq!(render.ortho_half_height_mm(200.0, 1.5), 50.0);
    }

    #[test]
    fn gpu_volumes_fit_the_voxel_budget_and_the_texture_limit() {
        let volume = Volume::new(64, 64, 64, [1.0, 1.0, 1.0], vec![0.0; 64 * 64 * 64]);

        let within_budget = GpuVolume::within_budget(&volume, 2048, 4096);
        assert_eq!(within_budget.dims, [16, 16, 16]);
        assert!(
            within_budget
                .dims
                .into_iter()
                .map(u64::from)
                .product::<u64>()
                <= 4096
        );

        let within_texture_limit = GpuVolume::within_budget(&volume, 8, 4096);
        assert!(
            within_texture_limit.dims.iter().all(|length| *length <= 8),
            "{:?}",
            within_texture_limit.dims
        );

        // A volume that already fits is never resolved finer than its source.
        let coarse = Volume::new(8, 8, 8, [2.0, 2.0, 2.0], vec![0.0; 8 * 8 * 8]);
        assert_eq!(GpuVolume::from_volume(&coarse, 2048).spacing_mm, [2.0; 3]);
    }

    /// Texels hold the values themselves, so a padding value far below the
    /// tissue leaves every tissue value exact, as the MPR planes show it.
    #[test]
    fn gpu_texels_hold_the_values_themselves() {
        let mut values: Vec<f32> = (0..4 * 4 * 4).map(|index| index as f32 - 20.0).collect();
        values[0] = -32768.0;
        let volume = Volume::new(4, 4, 4, [1.0, 1.0, 1.0], values.clone());

        let gpu_volume = GpuVolume::from_volume(&volume, 2048);
        assert_eq!(gpu_volume.texel_scale, 1.0);

        let texels = gpu_volume.take_texels().expect("staging texels");
        let decoded: Vec<f32> = texels
            .iter()
            .map(|bits| f16::from_bits(*bits).to_f32())
            .collect();
        assert_eq!(decoded, values);

        // The staging copy goes with the one upload it is taken for.
        assert!(gpu_volume.take_texels().is_none());
    }

    /// Values beyond half precision, such as PET in Bq/ml, are scaled by a
    /// power of two instead of overflowing.
    #[test]
    fn gpu_texels_beyond_half_precision_are_scaled_down() {
        let volume = Volume::new(2, 1, 1, [1.0; 3], vec![0.0, 600_000.0]);

        let gpu_volume = GpuVolume::from_volume(&volume, 2048);
        let texels = gpu_volume.take_texels().unwrap();

        assert_eq!(gpu_volume.texel_scale, 16.0);
        // Half precision keeps about 0.05 % of a value this large.
        let decoded = f16::from_bits(texels[1]).to_f32() * gpu_volume.texel_scale;
        assert!(
            (decoded - 600_000.0).abs() < 600_000.0 * 5.0e-4,
            "{decoded}"
        );
    }

    /// Each refusal halves the voxels of the texture refused, not of a budget
    /// that may never have been reached, and three refusals end the attempts.
    #[test]
    fn a_gpu_budget_halves_what_was_refused() {
        let budget = GpuBudget::FULL.after_refusal(10_000_000);
        assert_eq!(budget.max_voxels, 5_000_000);
        assert!(!budget.exhausted());

        let budget = budget.after_refusal(4_000_000).after_refusal(1_500_000);
        assert_eq!(budget.max_voxels, 750_000);
        assert!(budget.exhausted());
    }

    #[test]
    fn a_cancelled_preparation_stops_without_a_volume() {
        let source = Volume::new(8, 8, 8, [0.5; 3], vec![0.0; 8 * 8 * 8]);
        let cancelled = AtomicBool::new(true);

        assert!(
            source
                .resample_isotropic_unless_cancelled(1.0, &cancelled)
                .is_none()
        );
        assert!(GpuVolume::prepare(&source, 2048, 64, &cancelled).is_none());
    }

    #[test]
    fn gpu_downsampling_preserves_rounded_and_short_axis_extents() {
        let source = Volume::new(5, 7, 4, [1.0, 1.0, 0.5], vec![0.0; 5 * 7 * 4]);
        let gpu = GpuVolume::within_budget(&source, 2048, 16);
        assert!(gpu.dims.into_iter().map(u64::from).product::<u64>() <= 16);
        assert_eq!(gpu.size_mm(), [5.0, 7.0, 2.0]);
        assert_eq!(gpu.spacing_mm[2], 1.0);
    }

    #[test]
    fn the_ct_presets_follow_their_control_points_and_clamp() {
        let soft_tissue =
            TransferFunctionLut::bake(TransferPreset::CtSoftTissue, ct_window(), false);

        assert_eq!(soft_tissue.domain, (-1000.0, 1500.0));
        assert_channels_close(soft_tissue.entry_for(-4000.0), [0, 0, 0, 0]);
        assert_channels_close(soft_tissue.entry_for(-1000.0), [0, 0, 0, 0]);
        assert_channels_close(soft_tissue.entry_for(-100.0), [28, 27, 26, 0]);
        assert_channels_close(soft_tissue.entry_for(40.0), [140, 89, 64, 9]);
        assert_channels_close(soft_tissue.entry_for(200.0), [227, 189, 138, 35]);
        assert_channels_close(soft_tissue.entry_for(500.0), [255, 242, 217, 81]);
        assert_channels_close(soft_tissue.entry_for(4000.0), [255, 255, 255, 216]);

        let bone = TransferFunctionLut::bake(TransferPreset::CtBone, ct_window(), false);

        // Bone shares the colours and only lifts opacity above 150 HU.
        assert_channels_close(bone.entry_for(150.0), [200, 158, 115, 0]);
        assert_channels_close(bone.entry_for(300.0), [238, 209, 167, 39]);
        assert_channels_close(bone.entry_for(700.0), [255, 245, 225, 153]);
        assert_channels_close(bone.entry_for(4000.0), [255, 255, 255, 229]);
    }

    /// A chosen voxel size names the size actually used when the series or
    /// the voxel budget overrules it.
    #[test]
    fn the_quality_label_names_the_voxel_size_used() {
        let fine = Volume::new(8, 8, 8, [0.5; 3], vec![0.0; 8 * 8 * 8]);
        let coarse = Volume::new(8, 8, 8, [1.0; 3], vec![0.0; 8 * 8 * 8]);
        // 800 mm on a side: 0.7 mm voxels would be far over the budget.
        let huge = Volume::new(4, 4, 4, [200.0; 3], vec![0.0; 4 * 4 * 4]);

        assert_eq!(MprQuality::Iso(0.7).label(&fine), "0.7 mm");
        assert_eq!(MprQuality::Iso(0.7).label(&coarse), "0.7 mm (1.00 mm)");

        // Coarsened to fit the MPR budget of 128 M voxels.
        let used_mm = MprQuality::Iso(0.7).target_mm(&huge).unwrap();
        assert!(used_mm > 0.7);
        assert!(
            huge.resampled_dimensions(used_mm)
                .iter()
                .map(|length| *length as u64)
                .product::<u64>()
                <= 128_000_000
        );
        assert_eq!(
            MprQuality::Iso(0.7).label(&huge),
            format!("0.7 mm ({used_mm:.2} mm)")
        );
    }

    #[test]
    fn the_grayscale_preset_spans_the_window() {
        let lut = TransferFunctionLut::bake(TransferPreset::Grayscale, ct_window(), false);

        assert_eq!(lut.domain, (-160.0, 240.0));
        assert_channels_close(lut.entry_for(-400.0), [0, 0, 0, 0]);
        assert_channels_close(lut.entry_for(40.0), [128, 128, 128, 102]);
        assert_channels_close(lut.entry_for(400.0), [255, 255, 255, 204]);
    }

    /// MONOCHROME1 shows low values brightest, so its greys run the other way;
    /// the opacity still rises with the value, so the same structures show.
    #[test]
    fn an_inverted_series_takes_its_greys_the_other_way_round() {
        let lut = TransferFunctionLut::bake(TransferPreset::Grayscale, ct_window(), true);

        assert_channels_close(lut.entry_for(-400.0), [255, 255, 255, 0]);
        assert_channels_close(lut.entry_for(400.0), [0, 0, 0, 204]);
    }

    #[test]
    fn baking_the_same_choice_keeps_the_table_identity() {
        let soft_tissue =
            TransferFunctionLut::bake(TransferPreset::CtSoftTissue, ct_window(), false);
        let again = TransferFunctionLut::bake(TransferPreset::CtSoftTissue, ct_window(), false);
        let bone = TransferFunctionLut::bake(TransferPreset::CtBone, ct_window(), false);
        let wider = TransferFunctionLut::bake(
            TransferPreset::Grayscale,
            WindowLevel {
                center: 40.0,
                width: 800.0,
            },
            false,
        );
        let narrower = TransferFunctionLut::bake(TransferPreset::Grayscale, ct_window(), false);

        let inverted = TransferFunctionLut::bake(TransferPreset::Grayscale, ct_window(), true);

        assert_eq!(soft_tissue.id, again.id);
        assert_ne!(soft_tissue.id, bone.id);
        assert_ne!(narrower.id, inverted.id);
        assert_ne!(wider.id, narrower.id);
    }
}
