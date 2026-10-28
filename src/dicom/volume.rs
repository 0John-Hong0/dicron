//! Series volume reconstruction, plane extraction, and isotropic resampling.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use dicom_object::DefaultDicomObject;
use dicom_pixeldata::PhotometricInterpretation;

use super::elements::{element_text, first_parsed, parsed_values};
use super::geometry::{PATIENT_AXES, PatientAxis, patient_direction_label};
use super::model::SliceItem;
use super::pixels::{contain_decoder_panic, decode_pixel_frame};
use super::scan::open_dicom_file;

const MAX_WORKERS: usize = 8;
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(50);
/// Through-plane distances below this are treated as the same position.
const MINIMUM_SLICE_SEPARATION_MM: f64 = 1.0e-6;
/// Allow decimal rounding when the slices' own values are compared.
const DIRECTION_TOLERANCE: f64 = 1.0e-4;
const PIXEL_SPACING_TOLERANCE_MM: f64 = 1.0e-6;
const SLICE_THICKNESS_TOLERANCE_MM: f64 = 1.0e-3;
/// How far each ImageOrientationPatient direction may be from unit length:
/// cosines written with three decimals are about 0.02 % short.
const ORIENTATION_LENGTH_TOLERANCE: f64 = 1.0e-2;
/// How far the two directions may be from perpendicular, as the cosine of the
/// angle between them (0.06°).
const ORIENTATION_PERPENDICULAR_TOLERANCE: f64 = 1.0e-3;
/// How far a slice may sit from the regular grid fitted through all of them
/// and still be stacked as if it lay on it: a tenth of the spacing, which
/// covers positions written with one decimal or as single-precision numbers.
/// A stack further off, such as one missing a slice, is interpolated from its
/// actual positions instead.
const SLICE_POSITION_TOLERANCE: f64 = 0.1;
const SLICE_POSITION_TOLERANCE_MM: f64 = 0.01;
/// Sideways drift allowed per millimetre along the stack (a 0.06° tilt).
const SLICE_TILT_TOLERANCE: f64 = 1.0e-3;
/// The voxel sizes and extents a reconstructed patient volume can have.
const MIN_VOXEL_MM: f64 = 1.0e-3;
const MAX_EXTENT_MM: f64 = 10_000.0;
/// Slice spacing and thickness at or above this stair-step noticeably.
const LOW_RESOLUTION_MM: f64 = 3.0;
/// Voxels the MPR planes sample from at most: 128 M single-precision voxels
/// are 512 MB. A volume within the budget is shown as acquired.
const MAX_MPR_VOXELS: u64 = 128_000_000;
/// Voxels a series may be decoded into at most: one billion single-precision
/// voxels are 4 GB. A larger series is refused before its memory is asked
/// for, because an allocation that size can end the process instead of
/// failing.
const MAX_SOURCE_VOXELS: u64 = 1_000_000_000;

/// A reconstructed volume. Voxel `(x, y, z)` lives at
/// `(z * height + y) * width + x`, so `x` varies fastest.
pub(crate) struct Volume {
    data: Vec<f32>,
    /// Columns, along the image rows (patient `row_direction`).
    pub(crate) width: usize,
    /// Rows, along the image columns (patient `column_direction`).
    pub(crate) height: usize,
    /// Slices, along the patient `slice_direction`.
    pub(crate) depth: usize,
    /// Millimetres per voxel along x, y and z; every component finite and > 0.
    pub(crate) spacing: [f64; 3],
    /// Finite minimum and maximum of the voxel values.
    pub(crate) value_range: (f32, f32),
}

/// The three volume axes, named after the axis they step along.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VolumeAxis {
    X,
    Y,
    Z,
}

impl VolumeAxis {
    pub(crate) const ALL: [Self; 3] = [Self::X, Self::Y, Self::Z];

    pub(crate) fn index(self) -> usize {
        match self {
            Self::X => 0,
            Self::Y => 1,
            Self::Z => 2,
        }
    }

    /// The volume axes a plane through this axis runs its columns and rows
    /// along.
    pub(crate) fn plane_axes(self) -> (Self, Self) {
        match self {
            Self::X => (Self::Y, Self::Z),
            Self::Y => (Self::X, Self::Z),
            Self::Z => (Self::X, Self::Y),
        }
    }
}

/// One extracted plane, row-major over `columns` × `rows`, with rows in
/// increasing volume index order (no display flip applied). Its physical size
/// comes from the volume, through [`Volume::plane_extent_mm`].
pub(crate) struct PlaneImage {
    pub(crate) columns: usize,
    pub(crate) rows: usize,
    pub(crate) pixels: Vec<f32>,
}

/// The run of source cells one output cell reads along an axis, from `first`
/// on, with the weight of each.
struct AxisTaps {
    first: usize,
    weights: Vec<f32>,
}

impl Volume {
    pub(crate) fn new(
        width: usize,
        height: usize,
        depth: usize,
        spacing: [f64; 3],
        data: Vec<f32>,
    ) -> Self {
        debug_assert_eq!(width * height * depth, data.len());

        Self {
            value_range: finite_value_range(&data),
            data,
            width,
            height,
            depth,
            spacing: spacing.map(sanitized_spacing),
        }
    }

    pub(crate) fn axis_len(&self, axis: VolumeAxis) -> usize {
        match axis {
            VolumeAxis::X => self.width,
            VolumeAxis::Y => self.height,
            VolumeAxis::Z => self.depth,
        }
    }

    pub(crate) fn axis_spacing(&self, axis: VolumeAxis) -> f64 {
        self.spacing[axis.index()]
    }

    /// The voxel values themselves, `x` varying fastest.
    pub(crate) fn data(&self) -> &[f32] {
        &self.data
    }

    /// The millimetre extent of the whole volume along x, y and z.
    pub(crate) fn physical_size_mm(&self) -> [f64; 3] {
        [
            self.width as f64 * self.spacing[0],
            self.height as f64 * self.spacing[1],
            self.depth as f64 * self.spacing[2],
        ]
    }

    /// The millimetre size of a plane through `axis`: its columns, then its
    /// rows.
    pub(crate) fn plane_extent_mm(&self, axis: VolumeAxis) -> [f64; 2] {
        let (column_axis, row_axis) = axis.plane_axes();

        [
            self.axis_len(column_axis) as f64 * self.axis_spacing(column_axis),
            self.axis_len(row_axis) as f64 * self.axis_spacing(row_axis),
        ]
    }

    /// One plane through the volume. A Z plane has columns along x and rows
    /// along y, a Y plane columns along x and rows along z, and an X plane
    /// columns along y and rows along z.
    pub(crate) fn plane(&self, axis: VolumeAxis, index: usize) -> PlaneImage {
        self.project_slab(axis, index, 0)
    }

    /// The maximum over `index.saturating_sub(half_count) ..
    /// (index + half_count + 1).min(axis_len)`, laid out like [`Volume::plane`].
    pub(crate) fn slab_maximum(
        &self,
        axis: VolumeAxis,
        index: usize,
        half_count: usize,
    ) -> PlaneImage {
        self.project_slab(axis, index, half_count)
    }

    fn project_slab(&self, axis: VolumeAxis, index: usize, half_count: usize) -> PlaneImage {
        let (column_axis, row_axis) = axis.plane_axes();
        let columns = self.axis_len(column_axis);
        let rows = self.axis_len(row_axis);
        let axis_len = self.axis_len(axis);

        if axis_len == 0 || columns == 0 || rows == 0 {
            return PlaneImage {
                columns: 0,
                rows: 0,
                pixels: Vec::new(),
            };
        }

        let index = index.min(axis_len - 1);
        let first = index.saturating_sub(half_count);
        let last = (index + half_count + 1).min(axis_len);
        let mut pixels = vec![f32::NEG_INFINITY; columns * rows];

        match axis {
            // Across x, the slab of each output pixel is one contiguous run of
            // a row, read in place rather than revisiting every row per slice.
            VolumeAxis::X => {
                for (row_index, output_row) in pixels.chunks_exact_mut(columns).enumerate() {
                    for (output, column_index) in output_row.iter_mut().zip(0..) {
                        let run_start = (row_index * self.height + column_index) * self.width;
                        *output = self.data[run_start + first..run_start + last]
                            .iter()
                            .copied()
                            .fold(f32::NEG_INFINITY, f32::max);
                    }
                }
            }
            VolumeAxis::Y => {
                for slice_index in first..last {
                    for (row_index, output_row) in pixels.chunks_exact_mut(columns).enumerate() {
                        let row_start = (row_index * self.height + slice_index) * self.width;
                        let source_row = &self.data[row_start..row_start + self.width];
                        for (output, value) in output_row.iter_mut().zip(source_row) {
                            *output = output.max(*value);
                        }
                    }
                }
            }
            VolumeAxis::Z => {
                for slice_index in first..last {
                    let plane_start = slice_index * self.width * self.height;
                    let source_plane = &self.data[plane_start..plane_start + columns * rows];
                    for (output, value) in pixels.iter_mut().zip(source_plane) {
                        *output = output.max(*value);
                    }
                }
            }
        }

        PlaneImage {
            columns,
            rows,
            pixels,
        }
    }

    /// The voxel counts [`Volume::resample_isotropic`] would produce, so a
    /// caller with a size budget can choose its target before paying for the
    /// resample.
    pub(crate) fn resampled_dimensions(&self, target_mm: f64) -> [usize; 3] {
        if self.width == 0 || self.height == 0 || self.depth == 0 {
            return [0, 0, 0];
        }

        let target = self.sanitized_target_mm(target_mm);

        [
            resampled_len(self.width, self.spacing[0], target),
            resampled_len(self.height, self.spacing[1], target),
            resampled_len(self.depth, self.spacing[2], target),
        ]
    }

    fn voxel_count(&self) -> u64 {
        self.width as u64 * self.height as u64 * self.depth as u64
    }

    /// The isotropic voxel size a resample needs to stay within `max_voxels`
    /// voxels and `max_dimension` voxels per axis: as fine as those limits
    /// allow, but never finer than the finest source spacing, since that
    /// would only invent voxels.
    pub(crate) fn isotropic_target_within(&self, max_voxels: u64, max_dimension: usize) -> f64 {
        let size_mm = self.physical_size_mm();
        let finest_spacing = self.spacing.into_iter().fold(f64::INFINITY, f64::min);
        let longest_side = size_mm.into_iter().fold(0.0, f64::max);
        let budget_target =
            (size_mm[0] * size_mm[1] * size_mm[2] / max_voxels.max(1) as f64).cbrt();
        let dimension_target = longest_side / max_dimension.max(1) as f64;
        let mut target_mm = finest_spacing.max(budget_target).max(dimension_target);

        if !target_mm.is_finite() || target_mm <= 0.0 {
            target_mm = 1.0;
        }

        // Rounding to whole voxels can push a volume back over a limit, so grow
        // the target until the voxel counts themselves fit.
        while !dimensions_fit(
            self.resampled_dimensions(target_mm),
            max_voxels,
            max_dimension,
        ) {
            target_mm *= 1.05;
        }

        target_mm
    }

    /// Resample towards `target_mm`. An axis made finer, or kept, interpolates
    /// linearly between the voxels around each output centre, clamping at the
    /// edges; an axis made coarser averages every source voxel an output voxel
    /// covers, so a structure thinner than the new voxels fades instead of
    /// falling between two samples. Rounded voxel counts, including the
    /// minimum of two samples per axis, retain the source extent through their
    /// actual per-axis spacing rather than stretching the volume to fit the
    /// target.
    #[cfg(test)]
    pub(crate) fn resample_isotropic(&self, target_mm: f64) -> Self {
        let never_cancelled = AtomicBool::new(false);

        match self.resample_isotropic_unless_cancelled(target_mm, &never_cancelled) {
            Some(volume) => volume,
            None => unreachable!("a resample nobody can cancel was cancelled"),
        }
    }

    /// The resample of `resample_isotropic`, abandoned with `None` once
    /// `cancel` is set: every worker checks it between output planes, so a
    /// cancelled job releases its threads and its output buffer promptly.
    pub(crate) fn resample_isotropic_unless_cancelled(
        &self,
        target_mm: f64,
        cancel: &AtomicBool,
    ) -> Option<Self> {
        let target = self.sanitized_target_mm(target_mm);
        if self.width == 0 || self.height == 0 || self.depth == 0 {
            return Some(Self::new(0, 0, 0, [target; 3], Vec::new()));
        }

        let [output_width, output_height, output_depth] = self.resampled_dimensions(target);
        let source_size = self.physical_size_mm();
        let output_spacing = [
            source_size[0] / output_width as f64,
            source_size[1] / output_height as f64,
            source_size[2] / output_depth as f64,
        ];
        let column_samples = axis_taps(self.width, output_width);
        let row_samples = axis_taps(self.height, output_height);
        let plane_samples = axis_taps(self.depth, output_depth);

        let plane_len = output_width * output_height;
        let mut data = vec![0.0_f32; plane_len * output_depth];
        let planes_per_worker = output_depth.div_ceil(worker_count(output_depth));

        thread::scope(|scope| {
            for (chunk_index, chunk) in data.chunks_mut(planes_per_worker * plane_len).enumerate() {
                let column_samples = column_samples.as_slice();
                let row_samples = row_samples.as_slice();
                let plane_samples = plane_samples.as_slice();

                scope.spawn(move || {
                    let first_plane = chunk_index * planes_per_worker;

                    for (plane_offset, output_plane) in
                        chunk.chunks_exact_mut(plane_len).enumerate()
                    {
                        if cancel.load(Ordering::Relaxed) {
                            return;
                        }

                        let plane_sample = &plane_samples[first_plane + plane_offset];

                        for (row_sample, output_row) in row_samples
                            .iter()
                            .zip(output_plane.chunks_exact_mut(output_width))
                        {
                            for (output, column_sample) in output_row.iter_mut().zip(column_samples)
                            {
                                *output =
                                    self.sample_weighted(column_sample, row_sample, plane_sample);
                            }
                        }
                    }
                });
            }
        });

        if cancel.load(Ordering::Relaxed) {
            return None;
        }

        Some(Self::new(
            output_width,
            output_height,
            output_depth,
            output_spacing,
            data,
        ))
    }

    /// The voxel size a resample actually uses: the requested one when it is
    /// usable, else the coarsest source spacing.
    fn sanitized_target_mm(&self, target_mm: f64) -> f64 {
        if target_mm.is_finite() && target_mm > 0.0 {
            target_mm
        } else {
            self.spacing.into_iter().fold(0.0, f64::max)
        }
    }

    /// The weighted sum of the source voxels three axis tap runs select.
    fn sample_weighted(&self, x: &AxisTaps, y: &AxisTaps, z: &AxisTaps) -> f32 {
        let plane_len = self.width * self.height;
        let mut total = 0.0;

        for (plane, plane_weight) in (z.first..).zip(&z.weights) {
            let mut plane_total = 0.0;

            for (row, row_weight) in (y.first..).zip(&y.weights) {
                let row_start = plane * plane_len + row * self.width + x.first;
                let row_total: f32 = self.data[row_start..row_start + x.weights.len()]
                    .iter()
                    .zip(&x.weights)
                    .map(|(value, weight)| value * weight)
                    .sum();

                plane_total += row_weight * row_total;
            }

            total += plane_weight * plane_total;
        }

        total
    }
}

/// The isotropic voxel size the MPR planes should be resampled to, or `None`
/// to sample the volume as acquired.
///
/// The planes are drawn with their physical aspect and bilinear filtering, so
/// resampling to isotropic voxels adds no detail; it only costs memory and
/// blurs the in-plane resolution. The volume is therefore used as acquired
/// unless it exceeds the voxel budget, and then downsampled only as far as the
/// budget requires.
pub(crate) fn auto_mpr_target_mm(volume: &Volume) -> Option<f64> {
    auto_mpr_target_within(volume, MAX_MPR_VOXELS)
}

fn auto_mpr_target_within(volume: &Volume, max_voxels: u64) -> Option<f64> {
    (volume.voxel_count() > max_voxels)
        .then(|| volume.isotropic_target_within(max_voxels, usize::MAX))
}

/// `target_mm`, coarsened if a volume resampled to it would not fit the MPR
/// voxel budget.
///
/// A chosen voxel size is a request, not a promise: asking a thick-slice body
/// CT for 0.7 mm isotropic voxels would allocate several gigabytes on the job
/// thread, and Rust aborts rather than unwinds when that allocation fails.
pub(crate) fn mpr_target_within_budget(volume: &Volume, target_mm: f64) -> f64 {
    mpr_target_within(volume, target_mm, MAX_MPR_VOXELS)
}

fn mpr_target_within(volume: &Volume, target_mm: f64, max_voxels: u64) -> f64 {
    let coarsest = volume.isotropic_target_within(max_voxels, usize::MAX);

    if target_mm.is_finite() && target_mm > 0.0 {
        target_mm.max(coarsest)
    } else {
        coarsest
    }
}

/// Whether these voxel counts fit one volume within the budget and the per-axis
/// limit. The resample floors every axis at two voxels, so the limit does too.
fn dimensions_fit(dims: [usize; 3], max_voxels: u64, max_dimension: usize) -> bool {
    let dimension_limit = max_dimension.max(2);

    dims.iter().all(|length| *length <= dimension_limit)
        && dims
            .iter()
            .map(|length| *length as u64)
            .fold(1_u64, u64::saturating_mul)
            <= max_voxels
}

/// How many slices on each side of the centre a slab of `thickness_mm`
/// covers. A slab is an odd number of whole slices around the centre, and this
/// picks the count whose extent, `(2 * half + 1) * spacing`, is nearest the
/// thickness asked for, rounding a tie half-to-even.
pub(crate) fn slab_half_count(thickness_mm: f64, spacing_mm: f64) -> usize {
    if !thickness_mm.is_finite() || !spacing_mm.is_finite() || spacing_mm <= 0.0 {
        return 0;
    }

    let half_count = ((thickness_mm / spacing_mm - 1.0) / 2.0).round_ties_even();

    if half_count <= 0.0 {
        0
    } else {
        half_count as usize
    }
}

fn resampled_len(source_len: usize, source_spacing: f64, target: f64) -> usize {
    let length = (source_len as f64 * source_spacing / target).round_ties_even();

    if length <= 2.0 { 2 } else { length as usize }
}

/// What each output cell of an axis reads from the source cells.
fn axis_taps(source_len: usize, output_len: usize) -> Vec<AxisTaps> {
    let last_index = source_len - 1;
    let scale = source_len as f64 / output_len as f64;

    (0..output_len)
        .map(|output_index| {
            if scale <= 1.0 {
                // Finer or the same: interpolate at the output cell's centre.
                let source =
                    ((output_index as f64 + 0.5) * scale - 0.5).clamp(0.0, last_index as f64);
                let first = source as usize;
                let fraction = (source - first as f64) as f32;

                AxisTaps {
                    first,
                    weights: if first == last_index || fraction == 0.0 {
                        vec![1.0]
                    } else {
                        vec![1.0 - fraction, fraction]
                    },
                }
            } else {
                // Coarser: the output cell spans `scale` source cells, each
                // weighted by how much of it the output cell covers.
                let start = output_index as f64 * scale;
                let end = (start + scale).min(source_len as f64);
                let first = start.floor() as usize;
                let last = (end.ceil() as usize).clamp(first + 1, source_len) - 1;

                AxisTaps {
                    first,
                    weights: (first..=last)
                        .map(|cell| {
                            ((end.min(cell as f64 + 1.0) - start.max(cell as f64)) / scale) as f32
                        })
                        .collect(),
                }
            }
        })
        .collect()
}

fn finite_value_range(values: &[f32]) -> (f32, f32) {
    values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(None::<(f32, f32)>, |range, value| match range {
            Some((minimum, maximum)) => Some((minimum.min(value), maximum.max(value))),
            None => Some((value, value)),
        })
        .unwrap_or((0.0, 0.0))
}

fn sanitized_spacing(spacing: f64) -> f64 {
    if spacing.is_finite() && spacing > 0.0 {
        spacing
    } else {
        1.0
    }
}

/// One screen direction of a view: the volume axis it runs along, which patient
/// direction that axis points, and whether it must be reversed to reach the
/// standard presentation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlaneAxisLayout {
    pub(crate) axis: VolumeAxis,
    patient_axis: PatientAxis,
    /// `1.0` when the volume axis increases towards left, posterior or
    /// superior, `-1.0` when it increases the other way.
    sign: f64,
    /// Whether the volume axis runs backwards across or down the view.
    pub(crate) flipped: bool,
}

impl PlaneAxisLayout {
    fn new(axis: VolumeAxis, (patient_axis, sign): (PatientAxis, f64), vertical: bool) -> Self {
        // Screen-right shows patient left or posterior, and screen-down shows
        // inferior or posterior, so only the head-to-foot axis reverses the
        // sense of a positive volume index.
        let wanted_positive = !(vertical && patient_axis == PatientAxis::SuperiorInferior);

        Self {
            axis,
            patient_axis,
            sign,
            flipped: (sign > 0.0) != wanted_positive,
        }
    }

    /// Whether this direction, as displayed, runs towards left, posterior or
    /// superior rather than away from them.
    fn displayed_positive(self) -> bool {
        (self.sign > 0.0) != self.flipped
    }

    /// The one-letter marker for the far edge: the right of the view for a
    /// horizontal direction, its bottom for a vertical one.
    pub(crate) fn far_label(self) -> &'static str {
        patient_direction_label(self.patient_axis, self.displayed_positive())
    }

    /// The one-letter marker for the near edge: the left or the top.
    pub(crate) fn near_label(self) -> &'static str {
        patient_direction_label(self.patient_axis, !self.displayed_positive())
    }
}

/// How a view's plane is laid out on screen, in the standard radiological
/// presentation: patient left to the right, the head at the top, anterior at
/// the top of an axial view and at the left of a sagittal one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlaneLayout {
    /// The direction running across the view.
    pub(crate) horizontal: PlaneAxisLayout,
    /// The direction running down the view.
    pub(crate) vertical: PlaneAxisLayout,
    /// Whether the extracted plane's columns run down the view, so its two
    /// in-plane axes must be swapped before it is drawn.
    pub(crate) transposed: bool,
}

/// How a reconstructed volume sits in patient space.
#[derive(Clone, Debug)]
pub(crate) struct VolumeGeometry {
    /// Whether the series is MONOCHROME1, which shows its lowest values
    /// brightest. The voxels keep the stored values, so the window, the CT
    /// presets and the MIP all read them as the 2D viewer does; only the grey
    /// they are displayed in is inverted.
    pub(crate) inverted: bool,
    pub(crate) row_direction: [f64; 3],
    pub(crate) column_direction: [f64; 3],
    pub(crate) slice_direction: [f64; 3],
    /// `SliceThickness`, absent when the series does not state it.
    pub(crate) slice_thickness_mm: Option<f64>,
    /// The spacing of the volume's z axis.
    pub(crate) slice_spacing_mm: f64,
    /// The series' own slices, which are the volume's z planes unless they
    /// were unevenly spaced.
    pub(crate) slice_count: usize,
    /// Where each slice lies along z, in millimetres from the first, when the
    /// slices were unevenly spaced and the volume was interpolated between
    /// them onto `slice_spacing_mm`. `None` when slice `i` is z plane `i`.
    pub(crate) uneven_slice_offsets_mm: Option<Vec<f64>>,
}

impl VolumeGeometry {
    /// What makes the stack too coarse for clean reformats, if anything: its
    /// slices are thick, far apart, or both.
    pub(crate) fn low_resolution_cause(&self) -> Option<String> {
        let thick = self
            .slice_thickness_mm
            .is_some_and(|thickness| thickness >= LOW_RESOLUTION_MM);
        let far_apart = self.largest_slice_gap_mm() >= LOW_RESOLUTION_MM;

        match (thick, far_apart) {
            (true, true) => Some(format!(
                "slices {LOW_RESOLUTION_MM} mm or thicker and as far apart"
            )),
            (true, false) => Some(format!("slices {LOW_RESOLUTION_MM} mm or thicker")),
            (false, true) => Some(format!("slices {LOW_RESOLUTION_MM} mm or more apart")),
            (false, false) => None,
        }
    }

    /// The widest distance between neighbouring slices.
    fn largest_slice_gap_mm(&self) -> f64 {
        self.uneven_slice_offsets_mm
            .as_deref()
            .unwrap_or_default()
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .fold(self.slice_spacing_mm, f64::max)
    }

    /// The z spacing as the views state it, with the widest gap when the
    /// slices were uneven.
    pub(crate) fn slice_spacing_text(&self) -> String {
        if self.uneven_slice_offsets_mm.is_some() {
            format!(
                "{:.2} mm (uneven, up to {:.2} mm)",
                self.slice_spacing_mm,
                self.largest_slice_gap_mm()
            )
        } else {
            format!("{:.2} mm", self.slice_spacing_mm)
        }
    }

    /// Where slice `slice_index` of the series lies along the volume's z axis,
    /// as a fraction of its depth, so the slice a 2D view shows can be found
    /// in a volume that was interpolated or resampled.
    pub(crate) fn depth_fraction(&self, slice_index: usize) -> f64 {
        let slice_count = self.slice_count.max(1);
        let slice_index = slice_index.min(slice_count - 1);

        match self.uneven_slice_offsets_mm.as_deref() {
            // The planes are `slice_spacing_mm` apart, from the first slice to
            // the last, and each covers half a spacing on either side.
            Some(offsets @ [.., last]) => {
                let offset = offsets.get(slice_index).unwrap_or(last);

                (offset + 0.5 * self.slice_spacing_mm) / (last + self.slice_spacing_mm)
            }
            _ => (slice_index as f64 + 0.5) / slice_count as f64,
        }
    }

    /// The signed patient axis a volume axis runs along. The three volume axes
    /// always get three distinct patient axes: they are matched greedily by
    /// descending component magnitude.
    pub(crate) fn patient_axis(&self, axis: VolumeAxis) -> (PatientAxis, f64) {
        self.patient_axes()[axis.index()]
    }

    /// The plane a volume axis cuts, named after its patient axis. The three
    /// labels are distinct by construction.
    pub(crate) fn plane_label(&self, axis: VolumeAxis) -> &'static str {
        match self.patient_axis(axis).0 {
            PatientAxis::LeftRight => "Sagittal",
            PatientAxis::AnteriorPosterior => "Coronal",
            PatientAxis::SuperiorInferior => "Axial",
        }
    }

    /// How the plane through `axis` is laid out on screen. Both in-plane axes
    /// are placed and signed from their patient directions, so every view comes
    /// out in the standard presentation whatever plane the series was acquired
    /// in.
    pub(crate) fn plane_layout(&self, axis: VolumeAxis) -> PlaneLayout {
        let (column_axis, row_axis) = axis.plane_axes();
        let patient_axes = self.patient_axes();
        let column = patient_axes[column_axis.index()];
        let row = patient_axes[row_axis.index()];

        // Head to foot always runs down the view; left to right always runs
        // across it. An axial plane has neither, and puts left-right across.
        let column_is_horizontal = match (column.0, row.0) {
            (PatientAxis::SuperiorInferior, _) => false,
            (_, PatientAxis::SuperiorInferior) => true,
            (PatientAxis::LeftRight, _) => true,
            (_, PatientAxis::LeftRight) => false,
            _ => true,
        };
        let (horizontal_axis, horizontal, vertical_axis, vertical) = if column_is_horizontal {
            (column_axis, column, row_axis, row)
        } else {
            (row_axis, row, column_axis, column)
        };

        PlaneLayout {
            horizontal: PlaneAxisLayout::new(horizontal_axis, horizontal, false),
            vertical: PlaneAxisLayout::new(vertical_axis, vertical, true),
            transposed: !column_is_horizontal,
        }
    }

    /// The volume-space unit vector pointing towards the patient's left,
    /// posterior or head, whichever `patient_axis` names. Used to place the
    /// volume rendering camera in the patient's frame rather than the volume's.
    pub(crate) fn volume_direction_towards(&self, patient_axis: PatientAxis) -> [f32; 3] {
        let patient_axes = self.patient_axes();
        let mut direction = [0.0; 3];

        // The three volume axes carry three distinct patient axes, so exactly
        // one of them matches.
        for axis in VolumeAxis::ALL {
            let (candidate, sign) = patient_axes[axis.index()];

            if candidate == patient_axis {
                direction[axis.index()] = if sign < 0.0 { -1.0 } else { 1.0 };
                return direction;
            }
        }

        direction[2] = 1.0;
        direction
    }

    /// The index step along `axis` that moves one slice in reading order:
    /// towards the feet, the back or the patient's left, the order radiology
    /// stacks axial, coronal and sagittal planes in. It is -1 when the volume's
    /// index runs the other way, so stepping never depends on how the series
    /// was acquired.
    pub(crate) fn reading_order_step(&self, axis: VolumeAxis) -> isize {
        let (patient_axis, sign) = self.patient_axis(axis);
        // Patient coordinates increase towards the left, the back and the head.
        let reading_order_sign = match patient_axis {
            PatientAxis::SuperiorInferior => -1.0,
            PatientAxis::AnteriorPosterior | PatientAxis::LeftRight => 1.0,
        };

        if sign * reading_order_sign > 0.0 {
            1
        } else {
            -1
        }
    }

    /// The volume axis running superior-inferior, whose planes stack head to
    /// foot.
    pub(crate) fn superior_inferior_axis(&self) -> VolumeAxis {
        let patient_axes = self.patient_axes();

        VolumeAxis::ALL
            .into_iter()
            .find(|axis| patient_axes[axis.index()].0 == PatientAxis::SuperiorInferior)
            .unwrap_or(VolumeAxis::Z)
    }

    fn patient_axes(&self) -> [(PatientAxis, f64); 3] {
        patient_axes_for_directions([
            self.row_direction,
            self.column_direction,
            self.slice_direction,
        ])
    }
}

fn patient_axes_for_directions(directions: [[f64; 3]; 3]) -> [(PatientAxis, f64); 3] {
    let mut candidates = Vec::with_capacity(9);

    for (volume_axis, direction) in directions.iter().enumerate() {
        for (patient_axis, component) in direction.iter().enumerate() {
            let magnitude = if component.is_finite() {
                component.abs()
            } else {
                0.0
            };
            candidates.push((magnitude, volume_axis, patient_axis));
        }
    }

    candidates.sort_by(|left, right| right.0.total_cmp(&left.0));

    let mut assigned: [Option<(PatientAxis, f64)>; 3] = [None; 3];
    let mut claimed = [false; 3];

    for (_, volume_axis, patient_axis) in candidates {
        if assigned[volume_axis].is_some() || claimed[patient_axis] {
            continue;
        }

        let component = directions[volume_axis][patient_axis];
        let sign = if component.is_finite() && component < 0.0 {
            -1.0
        } else {
            1.0
        };
        assigned[volume_axis] = Some((PATIENT_AXES[patient_axis], sign));
        claimed[patient_axis] = true;
    }

    // Every volume axis is matched: the candidate list holds all nine pairs.
    assigned.map(|entry| entry.unwrap_or((PatientAxis::LeftRight, 1.0)))
}

/// A reconstructed series: its voxels plus where they sit in the patient.
pub(crate) struct SeriesVolume {
    pub(crate) volume: Arc<Volume>,
    pub(crate) geometry: VolumeGeometry,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct VolumeBuildProgress {
    pub(crate) decoded_slice_count: usize,
    pub(crate) total_slice_count: usize,
}

/// Decode every slice of a series, in the given order, into one volume.
///
/// Slices are decoded in parallel and assembled in order, so the caller's
/// ordering decides the volume's z axis.
pub(crate) fn build_series_volume(
    slices: &[SliceItem],
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(VolumeBuildProgress),
) -> Result<SeriesVolume> {
    if slices.len() < 2 {
        bail!("3D reconstruction needs a series with at least two slices.");
    }

    let distinct_paths: HashSet<_> = slices.iter().map(|slice| slice.path.as_path()).collect();
    if distinct_paths.len() != slices.len() {
        bail!("3D reconstruction supports single-frame series only.");
    }

    // A series that repeats positions, such as several phases or two copies
    // of one acquisition, is refused before every slice is decoded to find
    // out. The index has already placed equal positions next to each other.
    if let Some(pair) = slices.windows(2).find(|pair| {
        matches!(
            (pair[0].sort_position, pair[1].sort_position),
            (Some(one), Some(other)) if (one - other).abs() <= MINIMUM_SLICE_SEPARATION_MM
        )
    }) {
        let file_name = |slice: &SliceItem| {
            slice.path.file_name().map_or_else(
                || slice.path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        };
        bail!(
            "3D reconstruction requires distinct slice positions in spatial order; {} and {} share one position, as several phases or a series held twice do.",
            file_name(&pair[0]),
            file_name(&pair[1])
        );
    }

    let assembler = decode_slices(slices, cancel, &mut on_progress)?;
    let geometry = assembler.geometry()?;
    let spacing = [
        assembler.pixel_spacing()[1],
        assembler.pixel_spacing()[0],
        geometry.slice_spacing_mm,
    ];
    let depth = match geometry.uneven_slice_offsets_mm.as_deref() {
        Some([.., last]) => (last / geometry.slice_spacing_mm).round_ties_even() as usize + 1,
        _ => assembler.slice_count,
    };
    let lengths = [assembler.columns, assembler.rows, depth];
    // Every later size, budget and camera computation assumes a plausible
    // physical volume; spacings like 1e-20 or 1e308 mm overflow them.
    if spacing.iter().zip(lengths).any(|(spacing, length)| {
        let extent = spacing * length as f64;
        *spacing < MIN_VOXEL_MM || !extent.is_finite() || extent > MAX_EXTENT_MM
    }) {
        bail!(
            "3D reconstruction does not support voxels of {} x {} x {} mm over {} x {} x {} voxels.",
            spacing[0],
            spacing[1],
            spacing[2],
            lengths[0],
            lengths[1],
            lengths[2]
        );
    }
    let data = match geometry.uneven_slice_offsets_mm.as_deref() {
        Some(offsets) => interpolate_slices(
            &assembler.data,
            assembler.columns * assembler.rows,
            offsets,
            geometry.slice_spacing_mm,
            depth,
            cancel,
        )?,
        None => assembler.data,
    };
    let volume = Volume::new(assembler.columns, assembler.rows, depth, spacing, data);

    Ok(SeriesVolume {
        volume: Arc::new(volume),
        geometry,
    })
}

/// Interpolate slices of `plane_len` voxels lying at `offsets_mm`, increasing
/// from 0, onto `depth` planes `spacing_mm` apart. Each plane blends the two
/// slices around it by how near each lies.
fn interpolate_slices(
    slices: &[f32],
    plane_len: usize,
    offsets_mm: &[f64],
    spacing_mm: f64,
    depth: usize,
    cancel: &AtomicBool,
) -> Result<Vec<f32>> {
    if plane_len == 0 {
        return Ok(Vec::new());
    }

    let plane_taps = position_taps(offsets_mm, spacing_mm, depth);
    let mut data = Vec::new();
    // The slices are still held while the planes are filled.
    reserve_source_voxels(
        &mut data,
        plane_len as u64 * depth as u64,
        MAX_SOURCE_VOXELS.saturating_sub(slices.len() as u64),
    )?;
    data.resize(plane_len * depth, 0.0);
    let planes_per_worker = depth.div_ceil(worker_count(depth));

    thread::scope(|scope| {
        for (chunk, plane_taps) in data
            .chunks_mut((planes_per_worker * plane_len).max(1))
            .zip(plane_taps.chunks(planes_per_worker))
        {
            scope.spawn(move || {
                for (output_plane, taps) in chunk.chunks_exact_mut(plane_len).zip(plane_taps) {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }

                    for (slice, weight) in (taps.first..).zip(&taps.weights) {
                        let slice_start = slice * plane_len;
                        let source_plane = &slices[slice_start..slice_start + plane_len];

                        for (output, value) in output_plane.iter_mut().zip(source_plane) {
                            *output += weight * value;
                        }
                    }
                }
            });
        }
    });

    if cancel.load(Ordering::Relaxed) {
        bail!("3D reconstruction cancelled");
    }

    Ok(data)
}

/// The slices around each of `depth` planes `spacing_mm` apart, for slices at
/// `offsets_mm`, increasing from 0, weighted by how near each lies.
fn position_taps(offsets_mm: &[f64], spacing_mm: f64, depth: usize) -> Vec<AxisTaps> {
    let last_pair = offsets_mm.len().saturating_sub(2);

    (0..depth)
        .map(|plane| {
            let offset = plane as f64 * spacing_mm;
            // The last slice at or before the plane, with a slice after it.
            let first = offsets_mm
                .partition_point(|slice_offset| *slice_offset <= offset)
                .saturating_sub(1)
                .min(last_pair);
            let fraction = ((offset - offsets_mm[first])
                / (offsets_mm[first + 1] - offsets_mm[first]))
                .clamp(0.0, 1.0) as f32;

            AxisTaps {
                first,
                weights: if fraction == 0.0 {
                    vec![1.0]
                } else {
                    vec![1.0 - fraction, fraction]
                },
            }
        })
        .collect()
}

fn decode_slices(
    slices: &[SliceItem],
    cancel: &AtomicBool,
    on_progress: &mut impl FnMut(VolumeBuildProgress),
) -> Result<VolumeAssembler> {
    let total_slice_count = slices.len();
    let next_slice_index = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (result_sender, result_receiver) = mpsc::channel();
    let mut assembler = VolumeAssembler {
        expected_slice_count: total_slice_count,
        ..Default::default()
    };
    let mut failure = None;

    thread::scope(|scope| {
        for _ in 0..worker_count(total_slice_count) {
            let result_sender = result_sender.clone();
            let next_slice_index = &next_slice_index;
            let stop = &stop;

            scope.spawn(move || {
                loop {
                    if cancel.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
                        break;
                    }

                    let slice_index = next_slice_index.fetch_add(1, Ordering::Relaxed);
                    let Some(slice) = slices.get(slice_index) else {
                        break;
                    };
                    let decoded = decode_slice(slice).map_err(|error| format!("{error:#}"));

                    if result_sender.send((slice_index, decoded)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(result_sender);

        let mut completed: Vec<Option<_>> = (0..total_slice_count).map(|_| None).collect();
        let mut decoded_slice_count = 0;
        let mut last_progress_report = Instant::now();

        for (slice_index, decoded) in result_receiver {
            decoded_slice_count += 1;
            completed[slice_index] = Some(decoded);

            while assembler.slice_count < total_slice_count {
                let Some(decoded) = completed[assembler.slice_count].take() else {
                    break;
                };

                match decoded
                    .map_err(anyhow::Error::msg)
                    .and_then(|decoded| assembler.push(decoded))
                {
                    Ok(()) => {}
                    Err(error) => {
                        failure = Some(error);
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }

            if failure.is_some() {
                break;
            }

            if decoded_slice_count == total_slice_count
                || last_progress_report.elapsed() >= PROGRESS_REPORT_INTERVAL
            {
                on_progress(VolumeBuildProgress {
                    decoded_slice_count,
                    total_slice_count,
                });
                last_progress_report = Instant::now();
            }
        }
    });

    if cancel.load(Ordering::Relaxed) {
        bail!("3D reconstruction cancelled");
    }

    if let Some(failure) = failure {
        return Err(failure);
    }

    if assembler.slice_count != total_slice_count {
        bail!("3D reconstruction cancelled");
    }

    Ok(assembler)
}

/// Threads for `work_items` independent pieces of decoding or resampling: one
/// per core, at most `MAX_WORKERS`, and never more than there are pieces.
fn worker_count(work_items: usize) -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(MAX_WORKERS)
        .min(work_items)
        .max(1)
}

/// Geometry attributes checked on every slice before sharing one voxel grid.
#[derive(Clone, Copy, Default)]
struct SliceHeader {
    /// `PixelSpacing`, in DICOM order: row spacing then column spacing.
    pixel_spacing: [f64; 2],
    slice_thickness_mm: Option<f64>,
    orientation: [f64; 6],
}

struct DecodedSlice {
    columns: usize,
    rows: usize,
    pixels: Vec<f32>,
    position: [f64; 3],
    monochrome1: bool,
    header: SliceHeader,
}

fn decode_slice(slice: &SliceItem) -> Result<DecodedSlice> {
    let dicom_object = open_dicom_file(&slice.path)
        .with_context(|| format!("could not open DICOM file {}", slice.path.display()))?;
    // A slice that cannot be placed in the patient is refused before its
    // pixels are decoded.
    let header = read_slice_header(&dicom_object, &slice.path)?;
    let position = patient_geometry(&dicom_object, "ImagePositionPatient", &slice.path)?;
    check_bits_stored(&dicom_object, &slice.path)?;

    // A decoder that panics on damaged pixel data fails this slice with its
    // file named, instead of ending the worker thread and the whole build.
    contain_decoder_panic(|| Ok(decode_slice_pixels(&dicom_object, slice, header, position)))
        .with_context(|| {
            format!(
                "could not decode the pixel data of {}",
                slice.path.display()
            )
        })?
}

fn decode_slice_pixels(
    dicom_object: &DefaultDicomObject,
    slice: &SliceItem,
    header: SliceHeader,
    position: [f64; 3],
) -> Result<DecodedSlice> {
    let decoded = decode_pixel_frame(dicom_object, slice.frame_index).with_context(|| {
        format!(
            "could not decode the pixel data of {}",
            slice.path.display()
        )
    })?;

    if !decoded.photometric_interpretation().is_monochrome() {
        bail!("3D reconstruction supports grayscale images only.");
    }

    let columns = decoded.columns() as usize;
    let rows = decoded.rows() as usize;
    let pixels = decoded.to_vec_frame::<f32>(0).with_context(|| {
        format!(
            "could not read the pixel values of {}",
            slice.path.display()
        )
    })?;

    if pixels.len() != columns * rows {
        bail!("3D reconstruction supports grayscale images only.");
    }

    Ok(DecodedSlice {
        columns,
        rows,
        pixels,
        position,
        monochrome1: matches!(
            decoded.photometric_interpretation(),
            PhotometricInterpretation::Monochrome1
        ),
        header,
    })
}

/// The decoders size their lookup tables from BitsStored, and panic or
/// mask the samples wrongly when it does not fit the allocated sample.
fn check_bits_stored(dicom_object: &DefaultDicomObject, path: &Path) -> Result<()> {
    let (Some(stored), Some(allocated)) = (
        first_parsed::<u16>(dicom_object, "BitsStored"),
        first_parsed::<u16>(dicom_object, "BitsAllocated"),
    ) else {
        return Ok(());
    };

    if stored == 0 || stored > allocated {
        bail!(
            "3D reconstruction cannot read the pixel data of {}: BitsStored {stored} does not fit BitsAllocated {allocated}.",
            path.display()
        );
    }

    Ok(())
}

fn read_slice_header(dicom_object: &DefaultDicomObject, path: &Path) -> Result<SliceHeader> {
    Ok(SliceHeader {
        orientation: patient_geometry(dicom_object, "ImageOrientationPatient", path)?,
        pixel_spacing: patient_geometry(dicom_object, "PixelSpacing", path)?,
        slice_thickness_mm: positive_measurement(first_parsed(dicom_object, "SliceThickness")),
    })
}

/// An attribute that places the slice in the patient. There is no default
/// for one: the views label their planes and edges with patient directions,
/// and a guessed frame would show a plausible image under the wrong labels.
/// A value that is present but unreadable is refused as well, rather than
/// being mistaken for an absent one.
fn patient_geometry<const N: usize>(
    dicom_object: &DefaultDicomObject,
    keyword: &str,
    path: &Path,
) -> Result<[f64; N]> {
    if element_text(dicom_object, keyword).is_none() {
        bail!("3D reconstruction needs {keyword} on every slice.");
    }

    parsed_values(dicom_object, keyword).with_context(|| {
        format!(
            "3D reconstruction cannot read the {keyword} of {}",
            path.display()
        )
    })
}

fn positive_measurement(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value > 0.0)
}

#[derive(Default)]
struct VolumeAssembler {
    data: Vec<f32>,
    /// Slices the series holds, so the whole voxel buffer is reserved at once.
    expected_slice_count: usize,
    columns: usize,
    rows: usize,
    slice_count: usize,
    monochrome1: bool,
    header: SliceHeader,
    positions: Vec<[f64; 3]>,
}

impl VolumeAssembler {
    fn push(&mut self, decoded: DecodedSlice) -> Result<()> {
        if self.slice_count == 0 {
            self.columns = decoded.columns;
            self.rows = decoded.rows;
            self.monochrome1 = decoded.monochrome1;
            self.header = decoded.header;
            let voxels = [decoded.columns, decoded.rows, self.expected_slice_count]
                .into_iter()
                .map(|length| length as u64)
                .fold(1_u64, u64::saturating_mul);
            reserve_source_voxels(&mut self.data, voxels, MAX_SOURCE_VOXELS)?;
        } else if decoded.columns != self.columns || decoded.rows != self.rows {
            bail!(
                "All slices must have the same matrix size for 3D reconstruction; this series mixes {} x {} and {} x {}.",
                self.columns,
                self.rows,
                decoded.columns,
                decoded.rows
            );
        } else if decoded.monochrome1 != self.monochrome1 {
            // Polarity is decided once for the whole volume, so a series that
            // disagrees with itself would leave some slices inverted and some
            // not, and the MIP maximum would mix the two.
            bail!("All slices must share one photometric interpretation for 3D reconstruction.");
        }

        if !matching_geometry_values(
            self.header.orientation,
            decoded.header.orientation,
            DIRECTION_TOLERANCE,
            0.0,
        ) {
            bail!("All slices must share one ImageOrientationPatient for 3D reconstruction.");
        }
        if !matching_geometry_values(
            self.header.pixel_spacing,
            decoded.header.pixel_spacing,
            PIXEL_SPACING_TOLERANCE_MM,
            DIRECTION_TOLERANCE,
        ) {
            bail!("All slices must share one PixelSpacing for 3D reconstruction.");
        }
        // Reconstructions of different thickness interleaved in one series
        // would fuse into one volume, and the low-resolution decision would
        // depend on which happened to sort first.
        match (
            self.header.slice_thickness_mm,
            decoded.header.slice_thickness_mm,
        ) {
            (Some(one), Some(other))
                if !matching_geometry_values(
                    [one],
                    [other],
                    SLICE_THICKNESS_TOLERANCE_MM,
                    DIRECTION_TOLERANCE,
                ) =>
            {
                bail!(
                    "All slices must share one SliceThickness for 3D reconstruction; this series mixes {one} mm and {other} mm."
                );
            }
            (None, Some(thickness)) => self.header.slice_thickness_mm = Some(thickness),
            _ => {}
        }

        self.data.extend_from_slice(&decoded.pixels);
        self.positions.push(decoded.position);
        self.slice_count += 1;
        Ok(())
    }

    fn pixel_spacing(&self) -> [f64; 2] {
        self.header.pixel_spacing.map(sanitized_spacing)
    }

    fn geometry(&self) -> Result<VolumeGeometry> {
        if self
            .header
            .pixel_spacing
            .into_iter()
            .any(|value| value <= 0.0)
        {
            bail!("3D reconstruction requires positive PixelSpacing values.");
        }
        let orientation = self.header.orientation;
        let stated = [
            [orientation[0], orientation[1], orientation[2]],
            [orientation[3], orientation[4], orientation[5]],
        ];
        // Lengths first, with room for rounded cosines, then the angle between
        // the directions once they are normalised.
        let (Some(row_direction), Some(column_direction)) =
            (normalized(stated[0]), normalized(stated[1]))
        else {
            bail!("3D reconstruction requires orthonormal ImageOrientationPatient directions.");
        };
        if stated.iter().any(|direction| {
            (dot(*direction, *direction).sqrt() - 1.0).abs() > ORIENTATION_LENGTH_TOLERANCE
        }) || dot(row_direction, column_direction).abs() > ORIENTATION_PERPENDICULAR_TOLERANCE
        {
            bail!("3D reconstruction requires orthonormal ImageOrientationPatient directions.");
        }
        let plane_normal = normalized(cross(row_direction, column_direction));
        let slice_direction = self
            .traversed_direction()
            .or(plane_normal)
            .unwrap_or([0.0, 0.0, 1.0]);
        let (slice_spacing_mm, uneven_slice_offsets_mm) =
            self.slice_spacing(plane_normal.unwrap_or([0.0, 0.0, 1.0]))?;

        Ok(VolumeGeometry {
            inverted: self.monochrome1,
            row_direction,
            column_direction,
            slice_direction,
            slice_thickness_mm: self.header.slice_thickness_mm,
            slice_spacing_mm,
            slice_count: self.slice_count,
            uneven_slice_offsets_mm,
        })
    }

    /// The direction the series actually travels, from the first slice position
    /// to the last.
    fn traversed_direction(&self) -> Option<[f64; 3]> {
        let first = self.positions.first()?;
        let last = self.positions.last()?;

        normalized([last[0] - first[0], last[1] - first[1], last[2] - first[2]])
    }

    /// Verify that the slice origins advance in one direction along a line
    /// perpendicular to the slices, and return the z spacing of the volume
    /// with, when the slices are too uneven to stack as they are, where each
    /// lies along that line from the first.
    fn slice_spacing(&self, plane_normal: [f64; 3]) -> Result<(f64, Option<Vec<f64>>)> {
        let positions = &self.positions;
        let first = positions[0];
        let projected: Vec<_> = positions
            .iter()
            .map(|position| {
                dot(
                    std::array::from_fn(|axis| position[axis] - first[axis]),
                    plane_normal,
                )
            })
            .collect();
        if projected.iter().any(|distance| !distance.is_finite()) {
            bail!("3D reconstruction requires finite distances between slice positions.");
        }
        let gaps: Vec<_> = projected.windows(2).map(|pair| pair[1] - pair[0]).collect();
        let direction = gaps[0].signum();
        if let Some(index) = gaps.iter().position(|gap| {
            !gap.is_finite()
                || gap.abs() <= MINIMUM_SLICE_SEPARATION_MM
                || gap.signum() != direction
        }) {
            bail!(
                "3D reconstruction requires distinct slice positions in spatial order; slices {} and {} of {} are {:.3} mm apart.",
                index + 1,
                index + 2,
                positions.len(),
                gaps[index]
            );
        }
        // The regular grid that best fits every position, by least squares, so
        // rounding in the written positions does not add up along the stack.
        let count = projected.len() as f64;
        let mean_index = (count - 1.0) / 2.0;
        let mean_distance = projected.iter().sum::<f64>() / count;
        let (covariance, variance) = projected.iter().enumerate().fold(
            (0.0, 0.0),
            |(covariance, variance), (index, distance)| {
                let offset = index as f64 - mean_index;
                (
                    covariance + offset * (distance - mean_distance),
                    variance + offset * offset,
                )
            },
        );
        let step = covariance / variance;
        let spacing = step.abs();
        let tolerance = SLICE_POSITION_TOLERANCE_MM.max(spacing * SLICE_POSITION_TOLERANCE);
        for (index, (position, distance)) in positions.iter().zip(&projected).enumerate() {
            let lateral: [f64; 3] = std::array::from_fn(|axis| {
                position[axis] - first[axis] - distance * plane_normal[axis]
            });
            let sideways = dot(lateral, lateral).sqrt();
            let allowed = tolerance + distance.abs() * SLICE_TILT_TOLERANCE;
            if sideways > allowed {
                bail!(
                    "3D reconstruction does not support tilted or shifted slice grids; slice {} of {} sits {sideways:.2} mm to the side of the first, more than the {allowed:.2} mm allowed.",
                    index + 1,
                    positions.len()
                );
            }
        }
        if projected.iter().enumerate().all(|(index, distance)| {
            let fitted = mean_distance + (index as f64 - mean_index) * step;
            (distance - fitted).abs() <= tolerance
        }) {
            return Ok((spacing, None));
        }

        // Uneven slices are interpolated onto planes as close together as
        // their usual gap, running from the first slice to the last.
        let offsets: Vec<_> = projected
            .iter()
            .map(|distance| distance * direction)
            .collect();
        let mut gap_sizes: Vec<_> = gaps.iter().map(|gap| gap.abs()).collect();
        let middle = gap_sizes.len() / 2;
        let usual_gap = *gap_sizes.select_nth_unstable_by(middle, f64::total_cmp).1;
        let length = offsets.last().copied().unwrap_or_default();
        // Bounded so an implausible stack is refused by the extent check
        // instead of overflowing the plane count.
        let intervals = (length / usual_gap)
            .round_ties_even()
            .clamp(1.0, MAX_EXTENT_MM / MIN_VOXEL_MM);

        Ok((length / intervals, Some(offsets)))
    }
}

/// Reserve exactly the voxels a series decodes into, refusing a series over
/// `max_voxels` with a message rather than letting the allocation abort.
fn reserve_source_voxels(data: &mut Vec<f32>, voxels: u64, max_voxels: u64) -> Result<()> {
    let gigabytes = voxels as f64 * size_of::<f32>() as f64 / 1e9;

    if voxels > max_voxels {
        bail!(
            "This series is too large for 3D reconstruction: its {voxels} voxels would need {gigabytes:.1} GB."
        );
    }

    usize::try_from(voxels)
        .ok()
        .and_then(|voxels| data.try_reserve_exact(voxels).ok())
        .ok_or_else(|| {
            anyhow!("Not enough memory for 3D reconstruction: this series needs {gigabytes:.1} GB.")
        })
}

fn matching_geometry_values<const N: usize>(
    one: [f64; N],
    other: [f64; N],
    tolerance: f64,
    relative_tolerance: f64,
) -> bool {
    one.into_iter().zip(other).all(|(one, other)| {
        (one - other).abs() <= tolerance + relative_tolerance * one.abs().max(other.abs())
    })
}

fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn normalized(vector: [f64; 3]) -> Option<[f64; 3]> {
    let length = dot(vector, vector).sqrt();

    (length.is_finite() && length > MINIMUM_SLICE_SEPARATION_MM)
        .then(|| vector.map(|component| component / length))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use dicom_core::{DataElement, PrimitiveValue, VR};
    use dicom_dictionary_std::{tags, uids};
    use dicom_object::{FileDicomObject, FileMetaTableBuilder};

    use super::*;

    static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

    fn temporary_file_path(label: &str) -> PathBuf {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "dicron-volume-{label}-{}-{sequence}.dcm",
            std::process::id()
        ))
    }

    struct SliceSpec {
        columns: u16,
        rows: u16,
        samples_per_pixel: u16,
        bits_stored: u16,
        photometric: &'static str,
        pixels: Vec<u8>,
        position: Option<&'static str>,
        orientation: Option<&'static str>,
        pixel_spacing: Option<&'static str>,
        slice_thickness: Option<&'static str>,
        rescale: Option<(&'static str, &'static str)>,
    }

    impl Default for SliceSpec {
        fn default() -> Self {
            Self {
                columns: 2,
                rows: 2,
                samples_per_pixel: 1,
                bits_stored: 8,
                photometric: "MONOCHROME2",
                pixels: vec![0, 1, 2, 3],
                position: Some("0\\0\\0"),
                orientation: Some("1\\0\\0\\0\\1\\0"),
                pixel_spacing: Some("1\\1"),
                slice_thickness: None,
                rescale: None,
            }
        }
    }

    fn write_slice_file(path: &Path, spec: &SliceSpec) {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
            .media_storage_sop_instance_uid("2.25.900")
            .build()
            .unwrap();
        let mut object = FileDicomObject::new_empty_with_meta(meta);

        for (tag, value) in [
            (tags::ROWS, spec.rows),
            (tags::COLUMNS, spec.columns),
            (tags::SAMPLES_PER_PIXEL, spec.samples_per_pixel),
            (tags::BITS_ALLOCATED, 8),
            (tags::BITS_STORED, spec.bits_stored),
            (tags::HIGH_BIT, spec.bits_stored.saturating_sub(1)),
            (tags::PIXEL_REPRESENTATION, 0),
        ] {
            object.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }

        if spec.samples_per_pixel > 1 {
            object.put_element(DataElement::new(
                tags::PLANAR_CONFIGURATION,
                VR::US,
                PrimitiveValue::from(0_u16),
            ));
        }

        object.put_element(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from(spec.photometric),
        ));

        for (tag, value) in [
            (tags::IMAGE_POSITION_PATIENT, spec.position),
            (tags::IMAGE_ORIENTATION_PATIENT, spec.orientation),
            (tags::PIXEL_SPACING, spec.pixel_spacing),
            (tags::SLICE_THICKNESS, spec.slice_thickness),
        ] {
            if let Some(value) = value {
                object.put_element(DataElement::new(tag, VR::DS, PrimitiveValue::from(value)));
            }
        }

        if let Some((slope, intercept)) = spec.rescale {
            object.put_element(DataElement::new(
                tags::RESCALE_SLOPE,
                VR::DS,
                PrimitiveValue::from(slope),
            ));
            object.put_element(DataElement::new(
                tags::RESCALE_INTERCEPT,
                VR::DS,
                PrimitiveValue::from(intercept),
            ));
        }

        object.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::from(spec.pixels.clone()),
        ));
        object.write_to_file(path).unwrap();
    }

    struct WrittenSeries {
        paths: Vec<PathBuf>,
        slices: Vec<SliceItem>,
    }

    impl Drop for WrittenSeries {
        fn drop(&mut self) {
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn write_series(label: &str, specs: &[SliceSpec]) -> WrittenSeries {
        let mut paths = Vec::with_capacity(specs.len());
        let mut slices = Vec::with_capacity(specs.len());

        for (index, spec) in specs.iter().enumerate() {
            let path = temporary_file_path(&format!("{label}-{index}"));
            write_slice_file(&path, spec);
            slices.push(SliceItem {
                path: path.clone(),
                display_name: format!("slice {index}"),
                frame_index: 0,
                instance_number: Some(index as i32 + 1),
                sort_position: None,
            });
            paths.push(path);
        }

        WrittenSeries { paths, slices }
    }

    fn build(series: &WrittenSeries) -> Result<SeriesVolume> {
        let cancel = AtomicBool::new(false);
        build_series_volume(&series.slices, &cancel, |_| {})
    }

    fn rejection_message(series: &WrittenSeries) -> String {
        build(series)
            .err()
            .expect("the series should be rejected")
            .to_string()
    }

    fn axial_series(label: &str, positions: &[&'static str]) -> WrittenSeries {
        let specs: Vec<SliceSpec> = positions
            .iter()
            .enumerate()
            .map(|(index, position)| SliceSpec {
                pixels: vec![index as u8; 4],
                position: Some(position),
                ..Default::default()
            })
            .collect();

        write_series(label, &specs)
    }

    fn test_volume(width: usize, height: usize, depth: usize, spacing: [f64; 3]) -> Volume {
        let data = (0..width * height * depth)
            .map(|index| index as f32)
            .collect();
        Volume::new(width, height, depth, spacing, data)
    }

    #[test]
    fn slices_are_stacked_in_the_given_order_with_rescale_applied() {
        let series = write_series(
            "order",
            &[
                SliceSpec {
                    pixels: vec![10, 10, 10, 10],
                    position: Some("0\\0\\0"),
                    rescale: Some(("2", "-100")),
                    ..Default::default()
                },
                SliceSpec {
                    pixels: vec![20, 20, 20, 20],
                    position: Some("0\\0\\2"),
                    rescale: Some(("2", "-100")),
                    ..Default::default()
                },
            ],
        );

        let built = build(&series).unwrap();

        assert_eq!(built.volume.depth, 2);
        assert_eq!(built.volume.width, 2);
        assert_eq!(built.volume.height, 2);
        assert_eq!(built.volume.plane(VolumeAxis::Z, 0).pixels, vec![-80.0; 4]);
        assert_eq!(built.volume.plane(VolumeAxis::Z, 1).pixels, vec![-60.0; 4]);
        assert_eq!(built.volume.value_range, (-80.0, -60.0));
    }

    /// The voxel buffer is reserved once at its final size, so a series never
    /// holds the spare capacity of a buffer grown by doubling.
    #[test]
    fn the_voxel_buffer_is_reserved_exactly() {
        let series = axial_series("exact", &["0\\0\\0", "0\\0\\1", "0\\0\\2"]);

        let built = build(&series).unwrap();

        assert_eq!(built.volume.data.len(), 2 * 2 * 3);
        assert_eq!(built.volume.data.capacity(), built.volume.data.len());
    }

    #[test]
    fn a_series_over_the_source_budget_is_refused_before_allocating() {
        let mut data = Vec::new();

        let over_budget = reserve_source_voxels(&mut data, 1_001, 1_000).unwrap_err();
        let impossible = reserve_source_voxels(&mut data, u64::MAX / 2, u64::MAX).unwrap_err();

        assert_eq!(
            over_budget.to_string(),
            "This series is too large for 3D reconstruction: its 1001 voxels would need 0.0 GB."
        );
        assert!(
            impossible
                .to_string()
                .starts_with("Not enough memory for 3D reconstruction")
        );
        assert_eq!(data.capacity(), 0);
        reserve_source_voxels(&mut data, 1_000, 1_000).unwrap();
        assert_eq!(data.capacity(), 1_000);
    }

    #[test]
    fn slice_spacing_is_the_median_of_regular_projected_positions() {
        let series = axial_series("median", &["0\\0\\0", "0\\0\\2", "0\\0\\4", "0\\0\\6"]);

        let built = build(&series).unwrap();

        assert_eq!(built.geometry.slice_spacing_mm, 2.0);
        assert_eq!(built.volume.spacing, [1.0, 1.0, 2.0]);
    }

    #[test]
    fn repeated_slice_positions_are_rejected() {
        let series = axial_series("repeated-position", &["0\\0\\0", "0\\0\\0", "0\\0\\3"]);

        assert!(rejection_message(&series).contains("distinct slice positions"));
    }

    /// A series that does not say where its slices sit in the patient would be
    /// reconstructed in a guessed frame and still labelled with patient
    /// directions, so it is refused, whatever spacing it states instead.
    #[test]
    fn series_without_patient_geometry_are_rejected() {
        let no_orientation = write_series(
            "no-orientation",
            &[
                SliceSpec {
                    orientation: None,
                    ..Default::default()
                },
                SliceSpec {
                    orientation: None,
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );
        let no_position = write_series(
            "no-position",
            &[
                SliceSpec {
                    position: None,
                    slice_thickness: Some("2.5"),
                    ..Default::default()
                },
                SliceSpec {
                    position: None,
                    slice_thickness: Some("2.5"),
                    ..Default::default()
                },
            ],
        );
        let no_pixel_spacing = write_series(
            "no-pixel-spacing",
            &[
                SliceSpec {
                    pixel_spacing: None,
                    ..Default::default()
                },
                SliceSpec {
                    pixel_spacing: None,
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );

        assert_eq!(
            rejection_message(&no_orientation),
            "3D reconstruction needs ImageOrientationPatient on every slice."
        );
        assert_eq!(
            rejection_message(&no_position),
            "3D reconstruction needs ImagePositionPatient on every slice."
        );
        assert_eq!(
            rejection_message(&no_pixel_spacing),
            "3D reconstruction needs PixelSpacing on every slice."
        );
    }

    /// A geometry value that is present but cannot be read is not taken for an
    /// absent one. Positions written with a trailing separator, for one, used
    /// to stack the repeated positions of a dynamic series into one stretched
    /// volume.
    #[test]
    fn unreadable_patient_geometry_is_rejected() {
        let trailing_separator = write_series(
            "unreadable-position",
            &[
                SliceSpec {
                    position: Some("0\\0\\0\\"),
                    ..Default::default()
                },
                SliceSpec {
                    position: Some("0\\0\\0\\"),
                    ..Default::default()
                },
            ],
        );
        let not_a_number = write_series(
            "unreadable-orientation",
            &[
                SliceSpec {
                    orientation: Some("NaN\\0\\0\\0\\1\\0"),
                    ..Default::default()
                },
                SliceSpec {
                    orientation: Some("NaN\\0\\0\\0\\1\\0"),
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );
        let one_value = write_series(
            "unreadable-pixel-spacing",
            &[
                SliceSpec {
                    pixel_spacing: Some("0.5"),
                    ..Default::default()
                },
                SliceSpec {
                    pixel_spacing: Some("0.5"),
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );

        for (series, keyword) in [
            (&trailing_separator, "ImagePositionPatient"),
            (&not_a_number, "ImageOrientationPatient"),
            (&one_value, "PixelSpacing"),
        ] {
            let message = rejection_message(series);
            assert!(
                message.starts_with(&format!("3D reconstruction cannot read the {keyword} of ")),
                "{message}"
            );
        }
    }

    #[test]
    fn zero_pixel_spacing_is_rejected() {
        let series = write_series(
            "zero-spacing",
            &[
                SliceSpec {
                    pixel_spacing: Some("0\\0"),
                    ..Default::default()
                },
                SliceSpec {
                    pixel_spacing: Some("0\\0"),
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );

        assert!(rejection_message(&series).contains("positive PixelSpacing"));
    }

    /// The quality menu offers fixed voxel sizes, and a fine one on a large
    /// volume must still be coarsened to fit the budget.
    #[test]
    fn a_chosen_voxel_size_is_coarsened_to_fit_the_budget() {
        // 4 x 4 x 4 voxels of 4 mm: 64 mm on a side.
        let volume = Volume::new(4, 4, 4, [4.0, 4.0, 4.0], vec![0.0; 64]);

        // 64 voxels of the 64 mm cube is 4 mm isotropic, and 8 voxels is 8 mm,
        // so a finer request is pulled back to whatever the budget allows.
        assert_eq!(mpr_target_within(&volume, 1.0, 64), 4.0);
        assert_eq!(mpr_target_within(&volume, 1.0, 8), 8.0);
        // A request coarser than the budget needs is left alone.
        assert_eq!(mpr_target_within(&volume, 32.0, 8), 32.0);
        // So is one the volume already fits.
        assert_eq!(mpr_target_within(&volume, 4.0, 1_000_000), 4.0);
    }

    #[test]
    fn a_head_first_series_runs_towards_the_head() {
        let ascending = axial_series("ascending", &["0\\0\\0", "0\\0\\1"]);
        let descending = axial_series("descending", &["0\\0\\1", "0\\0\\0"]);

        let ascending = build(&ascending).unwrap();
        let descending = build(&descending).unwrap();

        assert_eq!(
            ascending.geometry.patient_axis(VolumeAxis::Z),
            (PatientAxis::SuperiorInferior, 1.0)
        );
        assert_eq!(ascending.geometry.slice_direction, [0.0, 0.0, 1.0]);
        assert_eq!(
            descending.geometry.patient_axis(VolumeAxis::Z),
            (PatientAxis::SuperiorInferior, -1.0)
        );
        assert_eq!(descending.geometry.slice_direction, [0.0, 0.0, -1.0]);
        assert_eq!(descending.geometry.row_direction, [1.0, 0.0, 0.0]);
        assert_eq!(descending.geometry.column_direction, [0.0, 1.0, 0.0]);
    }

    #[test]
    fn monochrome1_series_keep_their_values_and_are_marked_inverted() {
        let series = write_series(
            "monochrome1",
            &[
                SliceSpec {
                    photometric: "MONOCHROME1",
                    pixels: vec![0, 0, 0, 0],
                    ..Default::default()
                },
                SliceSpec {
                    photometric: "MONOCHROME1",
                    pixels: vec![10, 10, 10, 10],
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );

        let built = build(&series).unwrap();

        assert!(built.geometry.inverted);
        assert_eq!(built.volume.plane(VolumeAxis::Z, 0).pixels, vec![0.0; 4]);
        assert_eq!(built.volume.plane(VolumeAxis::Z, 1).pixels, vec![10.0; 4]);
        assert_eq!(built.volume.value_range, (0.0, 10.0));
    }

    /// Polarity is decided once for the whole volume, so a series that mixes
    /// the two must be refused rather than reconstructed half inverted.
    #[test]
    fn a_series_that_mixes_photometric_interpretations_is_rejected() {
        let mixed = write_series(
            "mixed_photometric",
            &[
                SliceSpec {
                    photometric: "MONOCHROME2",
                    pixels: vec![0, 0, 0, 0],
                    ..Default::default()
                },
                SliceSpec {
                    photometric: "MONOCHROME1",
                    pixels: vec![10, 10, 10, 10],
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );

        assert_eq!(
            rejection_message(&mixed),
            "All slices must share one photometric interpretation for 3D reconstruction."
        );
    }

    #[test]
    fn unsupported_series_are_rejected_with_specific_messages() {
        let single = write_series("single", &[SliceSpec::default()]);
        let rgb = write_series(
            "rgb",
            &[
                SliceSpec {
                    samples_per_pixel: 3,
                    photometric: "RGB",
                    pixels: vec![0; 12],
                    ..Default::default()
                },
                SliceSpec {
                    samples_per_pixel: 3,
                    photometric: "RGB",
                    pixels: vec![0; 12],
                    ..Default::default()
                },
            ],
        );
        let palette = write_series(
            "palette",
            &[
                SliceSpec {
                    photometric: "PALETTE COLOR",
                    ..Default::default()
                },
                SliceSpec {
                    photometric: "PALETTE COLOR",
                    ..Default::default()
                },
            ],
        );
        let mismatched = write_series(
            "mismatched",
            &[
                SliceSpec::default(),
                SliceSpec {
                    columns: 3,
                    rows: 1,
                    pixels: vec![0, 1, 2],
                    ..Default::default()
                },
            ],
        );

        assert_eq!(
            rejection_message(&single),
            "3D reconstruction needs a series with at least two slices."
        );
        assert_eq!(
            rejection_message(&rgb),
            "3D reconstruction supports grayscale images only."
        );
        assert_eq!(
            rejection_message(&palette),
            "3D reconstruction supports grayscale images only."
        );
        assert!(
            rejection_message(&mismatched)
                .starts_with("All slices must have the same matrix size for 3D reconstruction;")
        );
    }

    /// A slice whose compressed pixel data was cut short fails the build with
    /// its file named, rather than a decoder panic ending the worker thread.
    #[test]
    fn a_damaged_slice_fails_the_build_with_its_file_named() {
        let paths = vec![
            temporary_file_path("rle-intact"),
            temporary_file_path("rle-truncated"),
        ];
        crate::dicom::pixels::write_rle_test_slice(&paths[0], "0\\0\\0", 16, 1.0);
        crate::dicom::pixels::write_rle_test_slice(&paths[1], "0\\0\\1", 16, 0.7);
        let series = WrittenSeries {
            slices: paths
                .iter()
                .map(|path| SliceItem {
                    path: path.clone(),
                    display_name: String::new(),
                    frame_index: 0,
                    instance_number: None,
                    sort_position: None,
                })
                .collect(),
            paths,
        };

        let message = rejection_message(&series);

        assert!(
            message.starts_with(&format!(
                "could not decode the pixel data of {}: ",
                series.paths[1].display()
            )),
            "{message}"
        );
    }

    /// 8-bit RLE slices reach the volume with every voxel in place.
    #[test]
    fn eight_bit_rle_slices_build_in_place() {
        let paths = vec![
            temporary_file_path("rle8-first"),
            temporary_file_path("rle8-second"),
        ];
        crate::dicom::pixels::write_rle_test_slice(&paths[0], "0\\0\\0", 8, 1.0);
        crate::dicom::pixels::write_rle_test_slice(&paths[1], "0\\0\\1", 8, 1.0);
        let series = WrittenSeries {
            slices: paths
                .iter()
                .map(|path| SliceItem {
                    path: path.clone(),
                    display_name: String::new(),
                    frame_index: 0,
                    instance_number: None,
                    sort_position: None,
                })
                .collect(),
            paths,
        };

        let built = build(&series).unwrap();

        let first_slice = built.volume.plane(VolumeAxis::Z, 0).pixels;
        let expected: Vec<f32> = (0..64 * 64).map(|index| (index % 251) as f32).collect();
        assert_eq!(first_slice, expected);
    }

    #[test]
    fn bits_stored_must_fit_the_allocated_sample() {
        for bits_stored in [0, 9] {
            let series = write_series(
                "bits-stored",
                &[
                    SliceSpec {
                        bits_stored,
                        ..Default::default()
                    },
                    SliceSpec {
                        bits_stored,
                        position: Some("0\\0\\1"),
                        ..Default::default()
                    },
                ],
            );

            let message = rejection_message(&series);

            assert!(
                message.ends_with(&format!(
                    "BitsStored {bits_stored} does not fit BitsAllocated 8."
                )),
                "{message}"
            );
        }
    }

    #[test]
    fn frames_of_one_file_are_rejected_as_multi_frame() {
        let series = axial_series("multi-frame", &["0\\0\\0", "0\\0\\1"]);
        let mut frames = series.slices.clone();
        frames[1].path = frames[0].path.clone();
        frames[1].frame_index = 1;
        let cancel = AtomicBool::new(false);

        let error = build_series_volume(&frames, &cancel, |_| {})
            .err()
            .expect("frames of one file should be rejected");

        assert_eq!(
            error.to_string(),
            "3D reconstruction supports single-frame series only."
        );
    }

    #[test]
    fn a_cancelled_build_reports_cancellation() {
        let series = axial_series("cancelled", &["0\\0\\0", "0\\0\\1"]);
        let cancel = AtomicBool::new(true);

        let error = build_series_volume(&series.slices, &cancel, |_| {})
            .err()
            .expect("a cancelled build should fail");

        assert_eq!(error.to_string(), "3D reconstruction cancelled");
    }

    /// Progress is reported at most every so often, so a short series may get
    /// a single report; the last one always covers every slice, and the count
    /// never runs backwards.
    #[test]
    fn final_progress_report_covers_every_slice() {
        let series = axial_series("progress", &["0\\0\\0", "0\\0\\1", "0\\0\\2"]);
        let cancel = AtomicBool::new(false);
        let mut updates = Vec::new();

        build_series_volume(&series.slices, &cancel, |progress| updates.push(progress)).unwrap();

        let last = updates.last().unwrap();
        assert_eq!(last.decoded_slice_count, 3);
        assert_eq!(last.total_slice_count, 3);
        assert!(
            updates
                .windows(2)
                .all(|pair| pair[0].decoded_slice_count <= pair[1].decoded_slice_count)
        );
    }

    /// Pixel Spacing lists the distance between rows first, then between
    /// columns, so a 3 x 2 image at 0.5 by 0.8 mm is 2.4 mm wide and 1 mm tall,
    /// and its pixels keep their row-major order.
    #[test]
    fn a_non_square_image_keeps_its_axes_and_spacings_apart() {
        let spec = |position| SliceSpec {
            columns: 3,
            rows: 2,
            pixels: vec![0, 1, 2, 3, 4, 5],
            position: Some(position),
            pixel_spacing: Some("0.5\\0.8"),
            ..Default::default()
        };
        let series = write_series("non-square", &[spec("0\\0\\0"), spec("0\\0\\2")]);

        let volume = build(&series).unwrap().volume;

        assert_eq!([volume.width, volume.height, volume.depth], [3, 2, 2]);
        assert_eq!(volume.spacing, [0.8, 0.5, 2.0]);
        assert_eq!(volume.data()[..6], [0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn low_resolution_follows_thickness_and_spacing() {
        let mut geometry = VolumeGeometry {
            inverted: false,
            row_direction: [1.0, 0.0, 0.0],
            column_direction: [0.0, 1.0, 0.0],
            slice_direction: [0.0, 0.0, 1.0],
            slice_thickness_mm: Some(1.0),
            slice_spacing_mm: 1.0,
            slice_count: 4,
            uneven_slice_offsets_mm: None,
        };

        assert_eq!(geometry.low_resolution_cause(), None);

        geometry.slice_thickness_mm = Some(3.0);
        assert_eq!(
            geometry.low_resolution_cause().as_deref(),
            Some("slices 3 mm or thicker")
        );

        geometry.slice_thickness_mm = None;
        assert_eq!(geometry.low_resolution_cause(), None);

        geometry.slice_spacing_mm = 5.0;
        assert_eq!(
            geometry.low_resolution_cause().as_deref(),
            Some("slices 3 mm or more apart")
        );

        geometry.slice_thickness_mm = Some(1.0);
        assert!(geometry.low_resolution_cause().is_some());
        geometry.slice_thickness_mm = Some(5.0);
        assert_eq!(
            geometry.low_resolution_cause().as_deref(),
            Some("slices 3 mm or thicker and as far apart")
        );

        // Uneven slices count by their widest gap, not the planes' spacing.
        geometry.slice_thickness_mm = Some(1.0);
        geometry.slice_spacing_mm = 1.0;
        geometry.uneven_slice_offsets_mm = Some(vec![0.0, 1.0, 2.0, 4.5]);
        assert_eq!(geometry.low_resolution_cause(), None);
        geometry.uneven_slice_offsets_mm = Some(vec![0.0, 1.0, 4.0, 5.0]);
        assert_eq!(
            geometry.low_resolution_cause().as_deref(),
            Some("slices 3 mm or more apart")
        );
    }

    /// A 2D slice of an evenly spaced stack is the plane of the same number,
    /// whatever index it is asked for.
    #[test]
    fn even_slices_sit_at_their_own_planes() {
        let geometry = VolumeGeometry {
            inverted: false,
            row_direction: [1.0, 0.0, 0.0],
            column_direction: [0.0, 1.0, 0.0],
            slice_direction: [0.0, 0.0, 1.0],
            slice_thickness_mm: Some(1.0),
            slice_spacing_mm: 2.0,
            slice_count: 4,
            uneven_slice_offsets_mm: None,
        };

        assert_eq!(geometry.depth_fraction(1), 0.375);
        assert_eq!(geometry.depth_fraction(9), 0.875);
        assert_eq!(geometry.slice_spacing_text(), "2.00 mm");
    }

    #[test]
    fn every_acquisition_orientation_labels_three_distinct_planes() {
        let orientations = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, -1.0], [1.0, 0.0, 0.0]),
        ];

        for (row_direction, column_direction, slice_direction) in orientations {
            let geometry = VolumeGeometry {
                inverted: false,
                row_direction,
                column_direction,
                slice_direction,
                slice_thickness_mm: None,
                slice_spacing_mm: 1.0,
                slice_count: 4,
                uneven_slice_offsets_mm: None,
            };
            let labels = VolumeAxis::ALL.map(|axis| geometry.plane_label(axis));

            assert!(labels.contains(&"Axial"), "{labels:?}");
            assert!(labels.contains(&"Coronal"), "{labels:?}");
            assert!(labels.contains(&"Sagittal"), "{labels:?}");
            assert_eq!(
                geometry.plane_label(geometry.superior_inferior_axis()),
                "Axial"
            );
        }
    }

    #[test]
    fn an_axial_acquisition_maps_volume_axes_to_patient_axes() {
        let geometry = VolumeGeometry {
            inverted: false,
            row_direction: [1.0, 0.0, 0.0],
            column_direction: [0.0, 1.0, 0.0],
            slice_direction: [0.0, 0.0, -1.0],
            slice_thickness_mm: None,
            slice_spacing_mm: 1.0,
            slice_count: 4,
            uneven_slice_offsets_mm: None,
        };

        assert_eq!(
            geometry.patient_axis(VolumeAxis::X),
            (PatientAxis::LeftRight, 1.0)
        );
        assert_eq!(
            geometry.patient_axis(VolumeAxis::Y),
            (PatientAxis::AnteriorPosterior, 1.0)
        );
        assert_eq!(
            geometry.patient_axis(VolumeAxis::Z),
            (PatientAxis::SuperiorInferior, -1.0)
        );
        assert_eq!(geometry.superior_inferior_axis(), VolumeAxis::Z);
    }

    #[test]
    fn oblique_directions_still_get_distinct_patient_axes() {
        let axes = patient_axes_for_directions([[0.9, 0.8, 0.0], [0.7, 0.6, 0.0], [0.0, 0.0, 1.0]]);

        assert_eq!(axes[0].0, PatientAxis::LeftRight);
        assert_eq!(axes[1].0, PatientAxis::AnteriorPosterior);
        assert_eq!(axes[2].0, PatientAxis::SuperiorInferior);
    }

    #[test]
    fn planes_use_the_axis_column_and_row_spacing() {
        let volume = test_volume(4, 3, 2, [0.5, 0.25, 0.75]);

        let axial = volume.plane(VolumeAxis::Z, 1);
        let coronal = volume.plane(VolumeAxis::Y, 1);
        let sagittal = volume.plane(VolumeAxis::X, 1);

        assert_eq!((axial.columns, axial.rows), (4, 3));
        assert_eq!(volume.plane_extent_mm(VolumeAxis::Z), [2.0, 0.75]);
        assert_eq!(axial.pixels[0], 12.0);

        assert_eq!((coronal.columns, coronal.rows), (4, 2));
        assert_eq!(volume.plane_extent_mm(VolumeAxis::Y), [2.0, 1.5]);
        assert_eq!(
            coronal.pixels,
            vec![4.0, 5.0, 6.0, 7.0, 16.0, 17.0, 18.0, 19.0]
        );

        assert_eq!((sagittal.columns, sagittal.rows), (3, 2));
        assert_eq!(volume.plane_extent_mm(VolumeAxis::X), [0.75, 1.5]);
        assert_eq!(sagittal.pixels, vec![1.0, 5.0, 9.0, 13.0, 17.0, 21.0]);
    }

    #[test]
    fn slabs_clamp_to_the_volume_and_take_the_maximum() {
        let volume = test_volume(2, 2, 3, [1.0, 1.0, 1.0]);

        let first = volume.slab_maximum(VolumeAxis::Z, 0, 5);
        let last = volume.slab_maximum(VolumeAxis::Z, volume.depth - 1, 5);
        let middle = volume.slab_maximum(VolumeAxis::Z, 1, 0);

        assert_eq!(first.pixels, vec![8.0, 9.0, 10.0, 11.0]);
        assert_eq!(last.pixels, first.pixels);
        assert_eq!(middle.pixels, vec![4.0, 5.0, 6.0, 7.0]);
    }

    /// Every slab, along every axis, is the maximum over exactly the slices
    /// from `index - half` to `index + half`, clamped to the volume: values
    /// that rise and fall in every direction pin both ends.
    #[test]
    fn slabs_take_the_maximum_over_exactly_their_slices() {
        let (width, height, depth) = (5, 4, 6);
        let values: Vec<f32> = (0..width * height * depth)
            .map(|index| ((index * 37 + 11) % 23) as f32)
            .collect();
        let volume = Volume::new(width, height, depth, [1.0; 3], values.clone());
        let at = |x: usize, y: usize, z: usize| values[(z * height + y) * width + x];

        for axis in VolumeAxis::ALL {
            let axis_len = volume.axis_len(axis);
            for index in 0..axis_len {
                for half in 0..3 {
                    let slab = volume.slab_maximum(axis, index, half);
                    let slices = index.saturating_sub(half)..(index + half + 1).min(axis_len);
                    for row in 0..slab.rows {
                        for column in 0..slab.columns {
                            let expected = slices
                                .clone()
                                .map(|slice| match axis {
                                    VolumeAxis::X => at(slice, column, row),
                                    VolumeAxis::Y => at(column, slice, row),
                                    VolumeAxis::Z => at(column, row, slice),
                                })
                                .fold(f32::NEG_INFINITY, f32::max);
                            assert_eq!(
                                slab.pixels[row * slab.columns + column],
                                expected,
                                "{axis:?} index {index} half {half} at ({column}, {row})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn resampling_to_the_current_spacing_keeps_the_voxels() {
        let volume = test_volume(3, 2, 2, [1.0, 1.0, 1.0]);

        let resampled = volume.resample_isotropic(1.0);

        assert_eq!(
            (resampled.width, resampled.height, resampled.depth),
            (3, 2, 2)
        );
        assert_eq!(resampled.spacing, [1.0, 1.0, 1.0]);
        assert_eq!(
            resampled.plane(VolumeAxis::Z, 0).pixels,
            vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]
        );
    }

    #[test]
    fn resampling_a_ramp_samples_voxel_centres() {
        let volume = Volume::new(4, 1, 1, [1.0, 1.0, 1.0], vec![0.0, 1.0, 2.0, 3.0]);

        let resampled = volume.resample_isotropic(0.5);
        let row = resampled.plane(VolumeAxis::Z, 0).pixels;

        assert_eq!(resampled.width, 8);
        // Output centres land at source 0.0, 0.25, 0.75, ... of a unit ramp.
        assert_eq!(row[0], 0.0);
        assert_eq!(row[1], 0.25);
        assert_eq!(row[2], 0.75);
        assert_eq!(row[7], 3.0);
    }

    /// A wall one voxel thick, wherever it lies, survives a resample to
    /// voxels 2.4 times as large: it fades but never falls between samples,
    /// and its total intensity is kept.
    #[test]
    fn downsampling_never_drops_a_thin_structure() {
        for position in 0..12 {
            let values = (0..12)
                .map(|index| if index == position { 1000.0 } else { 0.0 })
                .collect();
            let volume = Volume::new(12, 1, 1, [1.0; 3], values);

            let sampled = volume.resample_isotropic(2.4);
            let row = &sampled.plane(VolumeAxis::Z, 0).pixels[..sampled.width];

            assert_eq!(sampled.width, 5);
            let peak = row.iter().copied().fold(0.0, f32::max);
            assert!(peak >= 1000.0 / 2.4 / 2.0, "wall at {position}: {row:?}");
            let total: f64 = row
                .iter()
                .map(|value| f64::from(*value) * sampled.spacing[0])
                .sum();
            assert!(
                (total - 1000.0).abs() < 1.0e-2,
                "wall at {position}: {total}"
            );
        }
    }

    #[test]
    fn resampled_lengths_round_half_to_even() {
        assert_eq!(resampled_len(5, 1.0, 2.0), 2);
        assert_eq!(resampled_len(7, 1.0, 2.0), 4);
        assert_eq!(resampled_len(9, 1.0, 2.0), 4);
        assert_eq!(resampled_len(1, 1.0, 1.0), 2);
    }

    #[test]
    fn predicted_dimensions_match_the_resampled_volume() {
        let volume = Volume::new(5, 7, 9, [1.0, 1.0, 2.0], vec![0.0; 5 * 7 * 9]);

        assert_eq!(volume.physical_size_mm(), [5.0, 7.0, 18.0]);
        assert_eq!(volume.data().len(), 5 * 7 * 9);

        let dimensions = volume.resampled_dimensions(2.0);
        let resampled = volume.resample_isotropic(2.0);

        // 5, 7 and 18 mm at 2 mm round to 2, 4 and 9 voxels.
        assert_eq!(dimensions, [2, 4, 9]);
        assert_eq!(
            dimensions,
            [resampled.width, resampled.height, resampled.depth]
        );
        assert_eq!(resampled.physical_size_mm(), [5.0, 7.0, 18.0]);
    }

    /// Thick slices resampled finer take values between their neighbours,
    /// while the in-plane axes at the target already keep theirs.
    #[test]
    fn a_thick_slice_axis_is_interpolated_between_slice_centres() {
        // Slices 2 mm apart holding 0, 10 and 20, centred at 1, 3 and 5 mm.
        // Each slice holds 0 in its first column and 5 in its second.
        let values = (0..3)
            .flat_map(|z| (0..2).flat_map(move |_| [0.0, 5.0].map(|x| z as f32 * 10.0 + x)))
            .collect();
        let volume = Volume::new(2, 2, 3, [1.0, 1.0, 2.0], values);

        let resampled = volume.resample_isotropic(1.0);

        assert_eq!(
            [resampled.width, resampled.height, resampled.depth],
            [2, 2, 6]
        );
        let column =
            |x: usize| -> Vec<f32> { (0..6).map(|z| resampled.data()[z * 4 + x]).collect() };
        // Output centres at 0.5 to 5.5 mm; the ends clamp to the outer slices.
        assert_eq!(column(0), [0.0, 2.5, 7.5, 12.5, 17.5, 20.0]);
        assert_eq!(column(1), [5.0, 7.5, 12.5, 17.5, 22.5, 25.0]);
    }

    #[test]
    fn a_volume_within_the_budget_is_shown_as_acquired() {
        let anisotropic = test_volume(8, 8, 4, [0.4, 0.4, 0.9]);

        assert_eq!(anisotropic.voxel_count(), 256);
        assert_eq!(auto_mpr_target_within(&anisotropic, 256), None);
        assert_eq!(auto_mpr_target_mm(&anisotropic), None);
    }

    #[test]
    fn a_volume_over_the_budget_is_downsampled_just_enough() {
        let volume = test_volume(8, 8, 4, [0.4, 0.4, 0.9]);

        let target = auto_mpr_target_within(&volume, 100).expect("over budget");
        let dims = volume.resampled_dimensions(target);

        assert!(target >= 0.4, "{target}");
        assert!(
            dims.iter().map(|length| *length as u64).product::<u64>() <= 100,
            "{dims:?}"
        );
        // Just over the coarsest voxel that fits: shrinking it 5 % breaks the budget.
        let finer = volume.resampled_dimensions(target / 1.05);
        assert!(
            finer.iter().map(|length| *length as u64).product::<u64>() > 100,
            "{finer:?}"
        );
    }

    #[test]
    fn isotropic_targets_respect_the_per_axis_limit_and_the_source_detail() {
        let volume = test_volume(8, 8, 4, [1.0, 1.0, 1.0]);

        let limited = volume.isotropic_target_within(u64::MAX, 4);
        assert!(
            volume
                .resampled_dimensions(limited)
                .iter()
                .all(|length| *length <= 4)
        );

        // Nothing to gain from voxels finer than the source.
        assert_eq!(volume.isotropic_target_within(u64::MAX, usize::MAX), 1.0);
    }

    #[test]
    fn slab_half_counts_pick_the_nearest_odd_slice_count() {
        // 5 and 7 slices exactly.
        assert_eq!(slab_half_count(10.0, 2.0), 2);
        assert_eq!(slab_half_count(14.0, 2.0), 3);
        // Halfway between 9 and 11 slices, and between 11 and 13.
        assert_eq!(slab_half_count(10.0, 1.0), 4);
        assert_eq!(slab_half_count(12.0, 1.0), 6);
        assert_eq!(slab_half_count(1.0, 2.0), 0);
        assert_eq!(slab_half_count(10.0, 0.0), 0);
        assert_eq!(slab_half_count(f64::NAN, 1.0), 0);
    }

    #[test]
    fn inconsistent_slice_orientation_is_rejected() {
        let series = write_series(
            "review-mixed-orientation",
            &[
                SliceSpec {
                    position: Some("0\\0\\0"),
                    ..Default::default()
                },
                SliceSpec {
                    position: Some("0\\0\\1"),
                    orientation: Some("-1\\0\\0\\0\\-1\\0"),
                    ..Default::default()
                },
            ],
        );
        assert!(
            build(&series).is_err(),
            "oppositely oriented slices were accepted into one voxel basis"
        );
    }

    /// Unevenly spaced slices are placed at their own positions, read in
    /// either direction, with the planes between them interpolated.
    #[test]
    fn uneven_slices_are_interpolated_at_their_positions() {
        let ascending = axial_series(
            "uneven-ascending",
            &["0\\0\\0", "0\\0\\1", "0\\0\\2", "0\\0\\10"],
        );
        let descending = axial_series(
            "uneven-descending",
            &["0\\0\\10", "0\\0\\9", "0\\0\\8", "0\\0\\0"],
        );

        for (label, series) in [("ascending", ascending), ("descending", descending)] {
            let built = build(&series).unwrap_or_else(|error| panic!("{label}: {error}"));
            let volume = &built.volume;
            let geometry = &built.geometry;
            let plane_value = |plane| volume.plane(VolumeAxis::Z, plane).pixels[0];

            assert_eq!(volume.depth, 11, "{label}");
            assert_eq!(volume.spacing, [1.0, 1.0, 1.0], "{label}");
            assert_eq!(geometry.slice_count, 4, "{label}");
            assert_eq!(
                geometry.uneven_slice_offsets_mm.as_deref(),
                Some([0.0, 1.0, 2.0, 10.0].as_slice()),
                "{label}"
            );
            // Slices 0 to 3 hold the values 0 to 3.
            assert_eq!(plane_value(2), 2.0, "{label}");
            assert_eq!(
                volume.plane(VolumeAxis::Z, 6).pixels,
                vec![2.5; 4],
                "{label}"
            );
            assert_eq!(plane_value(10), 3.0, "{label}");
            // The 2D slices are found on the planes that hold them.
            for (slice, plane) in [(0, 0), (1, 1), (2, 2), (3, 10)] {
                assert_eq!(
                    (geometry.depth_fraction(slice) * volume.depth as f64) as usize,
                    plane,
                    "{label}: slice {slice}"
                );
            }
            assert_eq!(
                geometry.slice_spacing_text(),
                "1.00 mm (uneven, up to 8.00 mm)"
            );
            assert_eq!(
                geometry.low_resolution_cause().as_deref(),
                Some("slices 3 mm or more apart"),
                "{label}"
            );
        }
    }

    #[test]
    fn coarse_quality_preserves_a_short_stacks_physical_depth() {
        let source = Volume::new(8, 8, 4, [1.0, 1.0, 0.5], vec![0.0; 8 * 8 * 4]);
        let resampled = source.resample_isotropic(2.0);
        assert_eq!(
            resampled.physical_size_mm()[2],
            source.physical_size_mm()[2],
            "resampling doubled the physical depth of the stack"
        );
    }

    #[test]
    fn geometry_accepts_small_decimal_rounding_errors() {
        let series = write_series(
            "rounded-geometry",
            &[
                SliceSpec {
                    position: Some("0\\0\\0"),
                    pixel_spacing: Some("0.7\\0.7"),
                    ..Default::default()
                },
                SliceSpec {
                    position: Some("0\\0\\1.0004"),
                    pixel_spacing: Some("0.7000001\\0.7"),
                    orientation: Some("0.99999999\\0\\0\\0\\1\\0"),
                    ..Default::default()
                },
                SliceSpec {
                    position: Some("0\\0\\2"),
                    pixel_spacing: Some("0.7\\0.7"),
                    ..Default::default()
                },
                SliceSpec {
                    position: Some("0\\0\\3"),
                    pixel_spacing: Some("0.7\\0.7"),
                    ..Default::default()
                },
            ],
        );
        let built = build(&series).unwrap();
        assert_eq!(built.volume.spacing[..2], [0.7, 0.7]);
        // Fitted through every position, so the rounded one moves it slightly.
        assert!((built.volume.spacing[2] - 1.0).abs() < 1.0e-3);
    }

    /// Test positions are built at run time, while the fixture takes the
    /// static strings most tests write inline.
    fn leaked(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }

    /// An axial stack whose z positions are written by `format`.
    fn written_stack(label: &str, count: usize, z: impl Fn(usize) -> String) -> WrittenSeries {
        let positions: Vec<&'static str> = (0..count)
            .map(|index| leaked(format!("0\\0\\{}", z(index))))
            .collect();

        axial_series(label, &positions)
    }

    /// Positions are written rounded to a few decimals, or printed from single
    /// precision; neither may make a regular stack fail the spacing check,
    /// whatever its length or parity.
    #[test]
    fn rounded_positions_of_regular_stacks_are_accepted() {
        /// How a stack writes the millimetre position of each slice.
        type WritePosition = fn(f64) -> String;

        let cases: [(&str, usize, f64, WritePosition); 7] = [
            ("float32", 120, 0.9, |z| {
                format!("{:.6}", (z - 673.85) as f32)
            }),
            ("two-decimals-5", 5, 0.625, |z| format!("{z:.2}")),
            ("two-decimals-40", 40, 0.625, |z| format!("{z:.2}")),
            ("three-decimals-50", 50, 0.9375, |z| format!("{z:.3}")),
            ("three-decimals-51", 51, 0.9375, |z| format!("{z:.3}")),
            ("one-decimal", 20, 1.25, |z| format!("{z:.1}")),
            ("exact", 3, 2.0, |z| format!("{z}")),
        ];

        for (label, count, spacing, format) in cases {
            let series = written_stack(label, count, |index| format(index as f64 * spacing));

            let built = build(&series).unwrap_or_else(|error| panic!("{label}: {error}"));

            assert!(
                (built.geometry.slice_spacing_mm - spacing).abs() < spacing * 0.01,
                "{label}: {}",
                built.geometry.slice_spacing_mm
            );
            // Stacked as written, not blurred by interpolating the rounding.
            assert!(built.geometry.uneven_slice_offsets_mm.is_none(), "{label}");
            assert_eq!(built.volume.depth, count, "{label}");
        }
    }

    /// The tolerance for rounding still leaves a gap of one missing slice far
    /// outside it, wherever the gap falls, and the missing plane is filled in
    /// from the slices on either side.
    #[test]
    fn a_missing_slice_is_interpolated_anywhere_in_a_long_stack() {
        for missing in [50, 98] {
            let series = written_stack("missing-slice", 99, |index| {
                let index = if index >= missing { index + 1 } else { index };
                format!("{index}")
            });

            let built = build(&series).unwrap_or_else(|error| panic!("{missing}: {error}"));
            let plane_value = |plane| built.volume.plane(VolumeAxis::Z, plane).pixels[0];

            assert!(built.geometry.uneven_slice_offsets_mm.is_some());
            assert_eq!(built.volume.depth, 100, "slice {missing} missing");
            assert_eq!(built.volume.spacing[2], 1.0, "slice {missing} missing");
            // Slice `i` holds the value `i`, and the slice after the gap is
            // the one numbered `missing`.
            assert_eq!(plane_value(missing - 1), (missing - 1) as f32);
            assert_eq!(plane_value(missing), missing as f32 - 0.5);
            assert_eq!(plane_value(missing + 1), missing as f32);
            assert_eq!(built.geometry.low_resolution_cause(), None);
        }
    }

    #[test]
    fn orientations_written_with_three_decimals_are_accepted() {
        let rotated = write_series(
            "three-decimal-rotation",
            &[
                SliceSpec {
                    orientation: Some("0.707\\0.707\\0\\-0.707\\0.707\\0"),
                    ..Default::default()
                },
                SliceSpec {
                    orientation: Some("0.707\\0.707\\0\\-0.707\\0.707\\0"),
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );
        let tilted_axial = write_series(
            "three-decimal-tilt",
            &[
                SliceSpec {
                    orientation: Some("1\\0\\0\\0\\0.996\\-0.087"),
                    ..Default::default()
                },
                SliceSpec {
                    orientation: Some("1\\0\\0\\0\\0.996\\-0.087"),
                    position: Some("0\\0.087\\0.996"),
                    ..Default::default()
                },
            ],
        );

        let rotated = build(&rotated).unwrap();
        build(&tilted_axial).unwrap();

        // The directions are used normalised.
        assert!(
            (dot(
                rotated.geometry.row_direction,
                rotated.geometry.row_direction
            ) - 1.0)
                .abs()
                < 1.0e-12
        );
    }

    #[test]
    fn orientations_that_are_not_orthonormal_are_rejected() {
        for (label, orientation) in [
            ("zero-row", "0\\0\\0\\0\\1\\0"),
            ("double-length", "2\\0\\0\\0\\2\\0"),
            ("five-degrees-skewed", "1\\0\\0\\0.087\\0.996\\0"),
        ] {
            let series = write_series(
                label,
                &[
                    SliceSpec {
                        orientation: Some(orientation),
                        ..Default::default()
                    },
                    SliceSpec {
                        orientation: Some(orientation),
                        position: Some("0\\0\\1"),
                        ..Default::default()
                    },
                ],
            );

            assert!(
                rejection_message(&series).contains("orthonormal"),
                "{label}"
            );
        }
    }

    #[test]
    fn a_single_zero_pixel_spacing_value_is_rejected() {
        let series = write_series(
            "one-zero-spacing",
            &[
                SliceSpec {
                    pixel_spacing: Some("0.7\\0"),
                    ..Default::default()
                },
                SliceSpec {
                    pixel_spacing: Some("0.7\\0"),
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );

        assert!(rejection_message(&series).contains("positive PixelSpacing"));
    }

    /// A gantry tilt of a few degrees would shear the reconstruction.
    #[test]
    fn a_stack_tilted_by_three_degrees_is_rejected() {
        let (sine, cosine) = 3.0_f64.to_radians().sin_cos();
        let series = written_stack("three-degree-tilt", 10, |index| {
            let along = index as f64 * 2.0;
            format!("{}", along * cosine)
        });
        // The same stack, with each slice shifted sideways as a tilt does.
        let positions: Vec<&'static str> = (0..10)
            .map(|index| {
                let along = index as f64 * 2.0;
                leaked(format!("0\\{}\\{}", along * sine, along * cosine))
            })
            .collect();
        let tilted = axial_series("three-degree-tilt-shifted", &positions);

        build(&series).unwrap();
        assert!(rejection_message(&tilted).contains("tilted or shifted"));
    }

    /// Reconstructions of different thickness interleaved into one series
    /// would fuse, with the low-resolution decision left to the sort order.
    #[test]
    fn mixed_slice_thickness_is_rejected() {
        let mixed = write_series(
            "mixed-thickness",
            &[
                SliceSpec {
                    slice_thickness: Some("1"),
                    ..Default::default()
                },
                SliceSpec {
                    slice_thickness: Some("5"),
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );
        let stated_once = write_series(
            "thickness-stated-once",
            &[
                SliceSpec::default(),
                SliceSpec {
                    slice_thickness: Some("2.5"),
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );

        assert_eq!(
            rejection_message(&mixed),
            "All slices must share one SliceThickness for 3D reconstruction; this series mixes 1 mm and 5 mm."
        );
        assert_eq!(
            build(&stated_once).unwrap().geometry.slice_thickness_mm,
            Some(2.5)
        );
    }

    /// Repeated positions are known from the index, so the series is refused
    /// without decoding anything: these files do not even exist.
    #[test]
    fn repeated_positions_are_refused_before_decoding() {
        let slices: Vec<SliceItem> = (0..3)
            .map(|index| SliceItem {
                path: temporary_file_path(&format!("never-written-{index}")),
                display_name: String::new(),
                frame_index: 0,
                instance_number: None,
                sort_position: Some(if index < 2 { 0.0 } else { 1.0 }),
            })
            .collect();
        let cancel = AtomicBool::new(false);

        let error = build_series_volume(&slices, &cancel, |_| {}).err().unwrap();

        let message = error.to_string();
        assert!(
            message.starts_with("3D reconstruction requires distinct slice positions"),
            "{message}"
        );
        // The two files that share a position are named.
        for slice in &slices[..2] {
            let name = slice.path.file_name().unwrap().to_string_lossy();
            assert!(message.contains(name.as_ref()), "{message}");
        }
    }

    /// Spacings far outside any patient volume would overflow every later
    /// size and budget computation.
    #[test]
    fn implausible_voxel_sizes_are_rejected() {
        let huge_pixels = write_series(
            "huge-pixels",
            &[
                SliceSpec {
                    pixel_spacing: Some("1e308\\1e308"),
                    ..Default::default()
                },
                SliceSpec {
                    pixel_spacing: Some("1e308\\1e308"),
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );
        let tiny_pixels = write_series(
            "tiny-pixels",
            &[
                SliceSpec {
                    pixel_spacing: Some("1e-20\\1e-20"),
                    ..Default::default()
                },
                SliceSpec {
                    pixel_spacing: Some("1e-20\\1e-20"),
                    position: Some("0\\0\\1"),
                    ..Default::default()
                },
            ],
        );
        let distant_slices = axial_series("distant-slices", &["0\\0\\0", "0\\0\\1e300"]);

        for series in [&huge_pixels, &tiny_pixels, &distant_slices] {
            let message = rejection_message(series);
            assert!(
                message.starts_with("3D reconstruction does not support voxels of"),
                "{message}"
            );
        }
    }

    #[test]
    fn inconsistent_pixel_spacing_is_rejected() {
        let series = write_series(
            "mixed-pixel-spacing",
            &[
                SliceSpec::default(),
                SliceSpec {
                    position: Some("0\\0\\1"),
                    pixel_spacing: Some("2\\1"),
                    ..Default::default()
                },
            ],
        );
        assert!(rejection_message(&series).contains("PixelSpacing"));
    }

    #[test]
    fn partially_missing_geometry_is_rejected() {
        for (second, expected) in [
            (
                SliceSpec {
                    position: Some("0\\0\\1"),
                    orientation: None,
                    ..Default::default()
                },
                "ImageOrientationPatient",
            ),
            (
                SliceSpec {
                    position: Some("0\\0\\1"),
                    pixel_spacing: None,
                    ..Default::default()
                },
                "PixelSpacing",
            ),
            (
                SliceSpec {
                    position: None,
                    ..Default::default()
                },
                "ImagePositionPatient",
            ),
        ] {
            let series = write_series("partly-missing-geometry", &[SliceSpec::default(), second]);
            assert!(rejection_message(&series).contains(expected));
        }
    }

    #[test]
    fn tilted_slice_grids_are_rejected() {
        let series = axial_series("tilted-grid", &["0\\0\\0", "0.5\\0\\1", "1\\0\\2"]);
        assert!(rejection_message(&series).contains("tilted or shifted"));
    }

    #[test]
    fn nonmonotonic_slice_positions_are_rejected() {
        let series = axial_series("unordered-grid", &["0\\0\\0", "0\\0\\2", "0\\0\\1"]);
        assert!(rejection_message(&series).contains("spatial order"));
    }

    #[test]
    fn reversed_regular_stacks_keep_their_spacing_and_direction() {
        let series = axial_series(
            "reversed-grid",
            &["0\\0\\3", "0\\0\\2", "0\\0\\1", "0\\0\\0"],
        );
        let built = build(&series).unwrap();
        assert_eq!(built.volume.spacing, [1.0; 3]);
        assert_eq!(built.geometry.slice_direction, [0.0, 0.0, -1.0]);
        assert_eq!(built.volume.plane(VolumeAxis::Z, 0).pixels, vec![0.0; 4]);
        assert_eq!(built.volume.plane(VolumeAxis::Z, 3).pixels, vec![3.0; 4]);
    }

    #[test]
    fn resampling_rounding_preserves_extent_and_samples_cell_centres() {
        let source = Volume::new(5, 1, 1, [1.0; 3], vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        let sampled = source.resample_isotropic(2.0);
        assert_eq!(sampled.physical_size_mm(), [5.0, 1.0, 1.0]);
        assert_eq!(sampled.spacing, [2.5, 0.5, 0.5]);
        // Each output cell averages the two and a half source cells it covers.
        assert_eq!(&sampled.plane(VolumeAxis::Z, 0).pixels[..2], &[0.8, 3.2]);
    }
}
