//! Background preparation of the volume a 3D workspace renders.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::state::{SeriesKey, ThreeDMode};
use crate::app::three_d_state::{GpuVolume, MprQuality};
use crate::dicom::{SeriesVolume, SliceItem, Volume, auto_mpr_target_mm, build_series_volume};

/// Spacing this close to the target counts as already resampled.
const SPACING_TOLERANCE_MM: f64 = 1.0e-6;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What a running job was started for.
pub(in crate::app) struct VolumeJobRequest {
    pub(in crate::app) series_key: SeriesKey,
    pub(in crate::app) fingerprint: u64,
    pub(in crate::app) requested_mode: ThreeDMode,
    pub(in crate::app) kind: VolumeJobKind,
}

/// The work one job performs. Cheap to clone, so the controller can keep the
/// request it started while the worker owns its own copy.
#[derive(Clone)]
pub(in crate::app) enum VolumeJobKind {
    /// Decode a series into a volume and, when it is over the MPR voxel
    /// budget, downsample it for the planes. The volume rendering texture is
    /// left to a job of its own on the first frame that shows it, so a volume
    /// that waits behind the low-resolution warning, or is kept for later,
    /// never holds a staging copy of it.
    Build { slices: Arc<[SliceItem]> },
    /// Resample an already reconstructed series to another voxel size, for the
    /// quality the user chose, which takes effect once the volume is in.
    Resample {
        series: Arc<SeriesVolume>,
        target_mm: f64,
        quality: MprQuality,
    },
    /// Prepare the volume rendering texture for an already built series,
    /// with at most `max_voxels`: fewer after the device refused a texture.
    GpuUpload {
        series: Arc<SeriesVolume>,
        max_texture_dimension_3d: u32,
        max_voxels: u64,
    },
}

/// The status of a texture being prepared, in the status bar and in the view
/// waiting for it.
pub(in crate::app) const PREPARING_GPU_MESSAGE: &str = "Preparing 3D  GPU…";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum VolumeJobStage {
    Decoding,
    Resampling,
    PreparingGpu,
}

pub(in crate::app) struct VolumeJobProgress {
    pub(in crate::app) stage: VolumeJobStage,
    pub(in crate::app) decoded_slice_count: usize,
    pub(in crate::app) total_slice_count: usize,
    pub(in crate::app) started_at: Instant,
}

/// A reconstructed series together with the volume the MPR views sample: the
/// source itself, unless it was over the voxel budget and had to be downsampled.
pub(in crate::app) struct PreparedVolume {
    pub(in crate::app) series: Arc<SeriesVolume>,
    pub(in crate::app) mpr: Arc<Volume>,
    /// The volume rendering texture, prepared on demand.
    pub(in crate::app) gpu: Option<Arc<GpuVolume>>,
}

/// What a finished job produced.
pub(in crate::app) enum VolumeJobOutput {
    Built(PreparedVolume),
    Resampled(Arc<Volume>),
    Gpu(Arc<GpuVolume>),
}

enum VolumeJobMessage {
    Progress {
        stage: VolumeJobStage,
        decoded_slice_count: usize,
        total_slice_count: usize,
    },
    Finished(Result<VolumeJobOutput, String>),
}

/// Owns the worker thread that prepares one volume at a time.
#[derive(Default)]
pub(in crate::app) struct VolumeJobController {
    receiver: Option<Receiver<VolumeJobMessage>>,
    progress: Option<VolumeJobProgress>,
    cancel: Option<Arc<AtomicBool>>,
    request: Option<VolumeJobRequest>,
}

impl VolumeJobController {
    pub(in crate::app) fn progress(&self) -> Option<&VolumeJobProgress> {
        self.progress.as_ref()
    }

    pub(in crate::app) fn request(&self) -> Option<&VolumeJobRequest> {
        self.request.as_ref()
    }

    /// Retarget a running job, so re-entering 3D in another mode while the
    /// volume is still building does not restart the work.
    pub(in crate::app) fn set_requested_mode(&mut self, requested_mode: ThreeDMode) {
        if let Some(request) = &mut self.request {
            request.requested_mode = requested_mode;
        }
    }

    /// Start the requested work, cancelling whatever was running before.
    pub(in crate::app) fn start(&mut self, context: &egui::Context, request: VolumeJobRequest) {
        self.cancel();

        let (stage, total_slice_count) = match &request.kind {
            VolumeJobKind::Build { slices, .. } => (VolumeJobStage::Decoding, slices.len()),
            VolumeJobKind::Resample { series, .. } => {
                (VolumeJobStage::Resampling, series.geometry.slice_count)
            }
            VolumeJobKind::GpuUpload { series, .. } => {
                (VolumeJobStage::PreparingGpu, series.geometry.slice_count)
            }
        };
        let kind = request.kind.clone();
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = Arc::clone(&cancel);
        let context_for_thread = context.clone();

        thread::spawn(move || {
            let output = run_job(kind, &cancel_for_thread, &sender);
            let _ = sender.send(VolumeJobMessage::Finished(output));
            context_for_thread.request_repaint();
        });

        self.receiver = Some(receiver);
        self.cancel = Some(cancel);
        self.request = Some(request);
        self.progress = Some(VolumeJobProgress {
            stage,
            decoded_slice_count: 0,
            total_slice_count,
            started_at: Instant::now(),
        });
        context.request_repaint();
    }

    /// Abandon a running job and forget its progress.
    pub(in crate::app) fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }

        self.receiver = None;
        self.progress = None;
        self.request = None;
    }

    /// Collect worker messages, returning the finished request and its result.
    pub(in crate::app) fn poll(
        &mut self,
        context: &egui::Context,
    ) -> Option<(VolumeJobRequest, Result<VolumeJobOutput, String>)> {
        let receiver = self.receiver.take()?;

        loop {
            match receiver.try_recv() {
                Ok(VolumeJobMessage::Progress {
                    stage,
                    decoded_slice_count,
                    total_slice_count,
                }) => {
                    if let Some(progress) = &mut self.progress {
                        progress.stage = stage;
                        progress.decoded_slice_count = decoded_slice_count;
                        progress.total_slice_count = total_slice_count;
                    }
                }
                Ok(VolumeJobMessage::Finished(prepared)) => {
                    return self.finish(prepared);
                }
                Err(TryRecvError::Empty) => {
                    self.receiver = Some(receiver);
                    context.request_repaint_after(POLL_INTERVAL);
                    return None;
                }
                Err(TryRecvError::Disconnected) => {
                    return self.finish(Err("3D preparation stopped unexpectedly.".to_owned()));
                }
            }
        }
    }

    fn finish(
        &mut self,
        output: Result<VolumeJobOutput, String>,
    ) -> Option<(VolumeJobRequest, Result<VolumeJobOutput, String>)> {
        self.progress = None;
        self.cancel = None;
        Some((self.request.take()?, output))
    }
}

/// Shown nowhere: a cancelled controller has already dropped the receiver.
const CANCELLED_MESSAGE: &str = "3D preparation cancelled";

/// Run one job on the worker thread. Every stage stops once the controller
/// cancels it, so an abandoned job releases its memory promptly.
fn run_job(
    kind: VolumeJobKind,
    cancel: &AtomicBool,
    sender: &Sender<VolumeJobMessage>,
) -> Result<VolumeJobOutput, String> {
    match kind {
        VolumeJobKind::Build { slices } => {
            prepare_volume(&slices, cancel, sender).map(VolumeJobOutput::Built)
        }
        VolumeJobKind::Resample {
            series, target_mm, ..
        } => series
            .volume
            .resample_isotropic_unless_cancelled(target_mm, cancel)
            .map(|volume| VolumeJobOutput::Resampled(Arc::new(volume)))
            .ok_or_else(|| CANCELLED_MESSAGE.to_owned()),
        VolumeJobKind::GpuUpload {
            series,
            max_texture_dimension_3d,
            max_voxels,
        } => GpuVolume::prepare(&series.volume, max_texture_dimension_3d, max_voxels, cancel)
            .map(|gpu| VolumeJobOutput::Gpu(Arc::new(gpu)))
            .ok_or_else(|| CANCELLED_MESSAGE.to_owned()),
    }
}

/// A failed build's message with its cause first and the file it happened in
/// after, so a long path can never push the cause out of sight.
fn cause_first(error: &anyhow::Error) -> String {
    let mut chain: Vec<String> = error.chain().map(ToString::to_string).collect();
    let cause = chain.pop().unwrap_or_default();

    if chain.is_empty() {
        cause
    } else {
        format!("{cause} ({})", chain.join(": "))
    }
}

fn prepare_volume(
    slices: &[SliceItem],
    cancel: &AtomicBool,
    sender: &Sender<VolumeJobMessage>,
) -> Result<PreparedVolume, String> {
    let total_slice_count = slices.len();
    let series = build_series_volume(slices, cancel, |progress| {
        let _ = sender.send(VolumeJobMessage::Progress {
            stage: VolumeJobStage::Decoding,
            decoded_slice_count: progress.decoded_slice_count,
            total_slice_count: progress.total_slice_count,
        });
    })
    .map_err(|error| cause_first(&error))?;

    // Auto names a target only for a volume over the MPR budget, which the
    // target then always fits.
    let mpr = match auto_mpr_target_mm(&series.volume) {
        Some(target_mm) => {
            let _ = sender.send(VolumeJobMessage::Progress {
                stage: VolumeJobStage::Resampling,
                decoded_slice_count: total_slice_count,
                total_slice_count,
            });
            Arc::new(
                series
                    .volume
                    .resample_isotropic_unless_cancelled(target_mm, cancel)
                    .ok_or_else(|| CANCELLED_MESSAGE.to_owned())?,
            )
        }
        None => Arc::clone(&series.volume),
    };

    Ok(PreparedVolume {
        series: Arc::new(series),
        mpr,
        gpu: None,
    })
}

/// Whether a volume has the requested grid while retaining the source extent.
/// Rounded dimensions can give each axis a slightly different actual spacing.
pub(in crate::app) fn volume_is_at_target(
    volume: &Volume,
    source: &Volume,
    target_mm: f64,
) -> bool {
    [volume.width, volume.height, volume.depth] == source.resampled_dimensions(target_mm)
        && volume
            .physical_size_mm()
            .into_iter()
            .zip(source.physical_size_mm())
            .all(|(extent, source_extent)| same_target_mm(extent, source_extent))
}

/// Whether two millimetre sizes name the same resample target.
pub(in crate::app) fn same_target_mm(one: f64, other: f64) -> bool {
    (one - other).abs() <= SPACING_TOLERANCE_MM
}

/// Reconstruct in patient-space order without changing the 2D series order.
pub(in crate::app) fn spatially_ordered_slices(slices: &[SliceItem]) -> Vec<SliceItem> {
    let mut ordered = slices.to_vec();
    ordered.sort_by(|left, right| {
        match (
            left.sort_position.filter(|value| value.is_finite()),
            right.sort_position.filter(|value| value.is_finite()),
        ) {
            (Some(left), Some(right)) => left.total_cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    ordered
}

/// Identifies the exact frames a volume was built from, so a re-scanned or
/// edited series is rebuilt instead of reused.
pub(in crate::app) fn series_fingerprint(slices: &[SliceItem]) -> u64 {
    let mut hasher = DefaultHasher::new();

    for slice in slices {
        slice.path.hash(&mut hasher);
        slice.frame_index.hash(&mut hasher);
    }

    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The cause leads a failure's message and the file follows, so a long
    /// path cannot hide it.
    #[test]
    fn a_failure_names_its_cause_before_the_file() {
        let error = anyhow::anyhow!("the pixel data is damaged")
            .context("could not decode the pixel data of /a/very/long/path.dcm");

        assert_eq!(
            cause_first(&error),
            "the pixel data is damaged (could not decode the pixel data of /a/very/long/path.dcm)"
        );
        assert_eq!(
            cause_first(&anyhow::anyhow!("needs two slices")),
            "needs two slices"
        );
    }

    fn slice(path: &str, frame_index: u32) -> SliceItem {
        SliceItem {
            path: PathBuf::from(path),
            display_name: path.to_owned(),
            frame_index,
            instance_number: None,
            sort_position: None,
        }
    }

    #[test]
    fn volume_build_orders_rle_slices_spatially_and_preserves_modality_values_with_empty_voi() {
        use dicom_core::{DataElement, PrimitiveValue, VR};
        use dicom_dictionary_std::tags;
        struct TemporarySeries(PathBuf);
        impl Drop for TemporarySeries {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = TemporarySeries(
            std::env::temp_dir().join(format!("dicron-volume-merge-{}", std::process::id())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        let mut slices = Vec::new();
        for (instance, z) in [2, 0, 1].into_iter().enumerate() {
            let path = directory.0.join(format!("{instance}.dcm"));
            crate::dicom::write_rle_test_slice(&path, &format!("0\\0\\{z}"), 8, 1.0);
            let mut object = dicom_object::open_file(&path).unwrap();
            object.put_element(DataElement::new(tags::RESCALE_SLOPE, VR::DS, "1"));
            object.put_element(DataElement::new(
                tags::RESCALE_INTERCEPT,
                VR::DS,
                (z * 100).to_string(),
            ));
            object.put_element(DataElement::new(
                tags::VOILUT_FUNCTION,
                VR::CS,
                PrimitiveValue::Empty,
            ));
            object.write_to_file(&path).unwrap();
            slices.push(SliceItem {
                path,
                display_name: String::new(),
                frame_index: 0,
                instance_number: Some(instance as i32),
                sort_position: Some(f64::from(z)),
            });
        }
        let original: Vec<_> = slices.iter().map(|slice| slice.path.clone()).collect();
        let (sender, _receiver) = mpsc::channel();
        let prepared = prepare_volume(
            &spatially_ordered_slices(&slices),
            &AtomicBool::new(false),
            &sender,
        )
        .unwrap();
        assert_eq!(prepared.series.volume.depth, 3);
        assert_eq!(prepared.series.volume.spacing, [1.0; 3]);
        for z in 0..3 {
            assert_eq!(
                prepared.series.volume.data()[z * 64 * 64 + 65],
                65.0 + z as f32 * 100.0
            );
        }
        assert_eq!(
            slices
                .iter()
                .map(|slice| slice.path.clone())
                .collect::<Vec<_>>(),
            original
        );
    }

    #[test]
    fn fingerprints_follow_the_frames_and_their_order() {
        let series = [slice("a.dcm", 0), slice("b.dcm", 0)];
        let reordered = [slice("b.dcm", 0), slice("a.dcm", 0)];
        let other_frame = [slice("a.dcm", 0), slice("b.dcm", 1)];

        assert_eq!(
            series_fingerprint(&series),
            series_fingerprint(&[slice("a.dcm", 0), slice("b.dcm", 0)])
        );
        assert_ne!(series_fingerprint(&series), series_fingerprint(&reordered));
        assert_ne!(
            series_fingerprint(&series),
            series_fingerprint(&other_frame)
        );
    }

    #[test]
    fn only_the_requested_grid_with_the_source_extent_skips_resampling() {
        let source = Volume::new(5, 7, 4, [1.0, 1.0, 0.5], vec![0.0; 5 * 7 * 4]);
        let resampled = source.resample_isotropic(2.0);
        let stretched = Volume::new(2, 4, 2, [2.0; 3], vec![0.0; 2 * 4 * 2]);
        assert!(!volume_is_at_target(&source, &source, 1.0));
        assert!(volume_is_at_target(&resampled, &source, 2.0));
        assert!(!volume_is_at_target(&resampled, &source, 1.0));
        assert!(!volume_is_at_target(&stretched, &source, 2.0));
        assert!(same_target_mm(1.0, 1.0));
        assert!(!same_target_mm(1.0, 1.5));
    }
}
