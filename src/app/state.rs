//! Persistent in-memory state for the running application.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;

use crate::dicom::{DicomIndex, DicomOverlayMetadata, MetadataItem, TextEncoding, Volume};
use crate::release_check::UpdateCheckOutcome;
use crate::settings::AppSettings;

use super::background_tasks::{ReleaseCheckJob, ScanController};
use super::frame_cache::DecodedCache;
use super::frame_loading::FrameLoadController;
use super::series_thumbnail_cache::SeriesThumbnailCache;
use super::three_d_state::{
    GpuBudget, MprMode, MprQuality, MprState, SavedMprView, VolumeRenderState,
};
use super::volume_job::{PreparedVolume, VolumeJobController, volume_is_at_target};

pub(super) type SeriesKey = (usize, usize, usize);

#[derive(Clone)]
pub(super) enum OpenSource {
    File(PathBuf),
    Folder(PathBuf),
    Inputs(Vec<PathBuf>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SliceSelection {
    pub(super) patient_index: usize,
    pub(super) study_index: usize,
    pub(super) series_index: usize,
    pub(super) slice_index: usize,
}

impl SliceSelection {
    pub(super) fn new(
        patient_index: usize,
        study_index: usize,
        series_index: usize,
        slice_index: usize,
    ) -> Self {
        Self {
            patient_index,
            study_index,
            series_index,
            slice_index,
        }
    }

    pub(super) fn series_key(self) -> SeriesKey {
        (self.patient_index, self.study_index, self.series_index)
    }
}

#[derive(Default)]
pub(super) struct MetadataPanelState {
    pub(super) search_text: String,
    pub(super) show_all: bool,
    pub(super) curated_items: Vec<MetadataItem>,
    pub(super) all_items: Vec<MetadataItem>,
    pub(super) expanded_paths: HashSet<String>,
    pub(super) overlay: Option<DicomOverlayMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlaybackLoopMode {
    StopAtEnd,
    Loop,
    PingPong,
}

pub(super) const PLAYBACK_MIN_FPS: f32 = 0.5;
pub(super) const PLAYBACK_MAX_FPS: f32 = 120.0;

pub(super) struct PlaybackState {
    pub(super) enabled: bool,
    pub(super) fps: f32,
    pub(super) loop_mode: PlaybackLoopMode,
    pub(super) direction: isize,
    pub(super) last_tick: Option<Instant>,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            enabled: false,
            fps: 15.0,
            loop_mode: PlaybackLoopMode::Loop,
            direction: 1,
            last_tick: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WindowLevel {
    pub(super) center: f64,
    pub(super) width: f64,
}

impl WindowLevel {
    /// The centres the window editors accept, in 2D and 3D alike.
    pub(super) const CENTER_RANGE: std::ops::RangeInclusive<f64> = -100_000.0..=100_000.0;
    /// The widths the window editors accept, in 2D and 3D alike.
    pub(super) const WIDTH_RANGE: std::ops::RangeInclusive<f64> = 1.0..=200_000.0;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WindowPreset {
    Default,
    FullDynamic,
    Skull,
    Lung,
    Abdomen,
    Mediastinum,
    Bone,
    Spine,
    Postmyelo,
    Felsenbein,
}

pub(super) const WINDOW_PRESETS: [WindowPreset; 10] = [
    WindowPreset::Default,
    WindowPreset::FullDynamic,
    WindowPreset::Skull,
    WindowPreset::Lung,
    WindowPreset::Abdomen,
    WindowPreset::Mediastinum,
    WindowPreset::Bone,
    WindowPreset::Spine,
    WindowPreset::Postmyelo,
    WindowPreset::Felsenbein,
];

impl WindowPreset {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::FullDynamic => "Full Dynamic",
            Self::Skull => "Skull",
            Self::Lung => "Lung",
            Self::Abdomen => "Abdomen",
            Self::Mediastinum => "Mediastinum",
            Self::Bone => "Bone",
            Self::Spine => "Spine",
            Self::Postmyelo => "Postmyelo",
            Self::Felsenbein => "Felsenbein",
        }
    }

    pub(super) fn shortcut(self) -> char {
        match self {
            Self::Default => '0',
            Self::FullDynamic => '1',
            Self::Skull => '2',
            Self::Lung => '3',
            Self::Abdomen => '4',
            Self::Mediastinum => '5',
            Self::Bone => '6',
            Self::Spine => '7',
            Self::Postmyelo => '8',
            Self::Felsenbein => '9',
        }
    }

    pub(super) fn fixed_window(self) -> Option<WindowLevel> {
        let (center, width) = match self {
            Self::Skull => (25.0, 95.0),
            Self::Lung => (-400.0, 1600.0),
            Self::Abdomen | Self::Mediastinum => (10.0, 400.0),
            Self::Bone => (300.0, 2500.0),
            Self::Spine => (20.0, 300.0),
            Self::Postmyelo => (200.0, 1000.0),
            Self::Felsenbein => (500.0, 4000.0),
            Self::Default | Self::FullDynamic => return None,
        };

        Some(WindowLevel { center, width })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SavedWindowLevel {
    pub(super) window: WindowLevel,
    pub(super) preset: Option<WindowPreset>,
}

#[derive(Clone, Copy)]
pub(super) struct ReopenViewState {
    pub(super) viewport_transform: ViewportTransform,
    pub(super) window_level: Option<SavedWindowLevel>,
}

pub(super) struct WindowLevelState {
    pub(super) current: WindowLevel,
    pub(super) default: WindowLevel,
    pub(super) value_range: (f64, f64),
    pub(super) customized: bool,
    pub(super) available: bool,
    pub(super) active_preset: Option<WindowPreset>,
    pub(super) by_series: HashMap<SeriesKey, SavedWindowLevel>,
}

impl Default for WindowLevelState {
    fn default() -> Self {
        let initial = WindowLevel {
            center: 128.0,
            width: 256.0,
        };

        Self {
            current: initial,
            default: initial,
            value_range: (-1024.0, 3071.0),
            customized: false,
            available: false,
            active_preset: Some(WindowPreset::Default),
            by_series: HashMap::new(),
        }
    }
}

pub(super) struct SidePanelLayout {
    pub(super) width: f32,
    pub(super) collapsed: bool,
    pub(super) resize_drag: Option<PanelResizeDrag>,
}

#[derive(Clone, Copy)]
pub(super) struct PanelResizeDrag {
    pub(super) start_width: f32,
}

impl SidePanelLayout {
    const fn expanded(width: f32) -> Self {
        Self {
            width,
            collapsed: false,
            resize_drag: None,
        }
    }
}

pub(super) struct EditWindowingDialogState {
    pub(super) open: bool,
    pub(super) center: f64,
    pub(super) width: f64,
}

impl Default for EditWindowingDialogState {
    fn default() -> Self {
        Self {
            open: false,
            center: 0.0,
            width: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ViewportTransform {
    pub(super) zoom: f32,
    pub(super) pan: egui::Vec2,
    pub(super) flip_horizontal: bool,
    pub(super) flip_vertical: bool,
    pub(super) rotation_quarters: u8,
}

impl Default for ViewportTransform {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            flip_horizontal: false,
            flip_vertical: false,
            rotation_quarters: 0,
        }
    }
}

pub(super) struct PanelLayout {
    pub(super) left: SidePanelLayout,
    pub(super) right: SidePanelLayout,
}

impl Default for PanelLayout {
    fn default() -> Self {
        Self {
            left: SidePanelLayout::expanded(450.0),
            right: SidePanelLayout::expanded(340.0),
        }
    }
}

pub(super) enum UpdateCheckStatus {
    NotChecked,
    Checking,
    Finished(UpdateCheckOutcome),
    Failed(String),
}

impl UpdateCheckStatus {
    pub(super) fn is_checking(&self) -> bool {
        matches!(self, Self::Checking)
    }
}

pub(super) struct AboutDialogState {
    pub(super) open: bool,
    pub(super) update_job: ReleaseCheckJob,
    pub(super) update_status: UpdateCheckStatus,
    pub(super) notify_when_finished: bool,
    pub(super) startup_check_pending: bool,
    pub(super) notification_visible: bool,
}

impl AboutDialogState {
    fn new(check_for_updates_on_startup: bool) -> Self {
        Self {
            open: false,
            update_job: ReleaseCheckJob::new(),
            update_status: UpdateCheckStatus::NotChecked,
            notify_when_finished: false,
            startup_check_pending: check_for_updates_on_startup,
            notification_visible: false,
        }
    }
}

/// The 3D workspace the central panel can switch to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ThreeDMode {
    Mpr,
    Mip,
    VolumeRendering,
}

impl ThreeDMode {
    pub(super) const ALL: [Self; 3] = [Self::Mpr, Self::Mip, Self::VolumeRendering];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Mpr => "MPR",
            Self::Mip => "MIP",
            Self::VolumeRendering => "Volume Rendering",
        }
    }

    /// How the MPR views sample the volume in this mode, if it shows them.
    pub(super) fn mpr_mode(self) -> Option<MprMode> {
        match self {
            Self::Mpr => Some(MprMode::Slice),
            Self::Mip => Some(MprMode::MipSlab),
            Self::VolumeRendering => None,
        }
    }
}

/// What the central panel and its control row currently show.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ViewMode {
    #[default]
    TwoD,
    ThreeD(ThreeDMode),
}

impl ViewMode {
    pub(super) fn three_d_mode(self) -> Option<ThreeDMode> {
        match self {
            Self::TwoD => None,
            Self::ThreeD(mode) => Some(mode),
        }
    }
}

/// What the wgpu renderer behind egui can do, read from the render state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct GpuInfo {
    pub(super) max_texture_dimension_3d: u32,
}

#[derive(Default)]
pub(super) struct ThreeDState {
    pub(super) view_mode: ViewMode,
    pub(super) job: VolumeJobController,
    /// The workspace on screen, if any.
    pub(super) active: Option<ActiveVolumeView>,
    /// A prepared volume the user did not open, kept to avoid rebuilding it.
    pub(super) cached: Option<(SeriesKey, u64, PreparedVolume)>,
    /// Series the user opened despite the low-resolution warning.
    pub(super) low_resolution_accepted: HashSet<(SeriesKey, u64)>,
    /// Series whose reconstruction failed, with the message it failed with.
    /// Selecting one inside 3D returns to it in 2D with that message instead
    /// of decoding it again; only an explicit 3D request retries it.
    pub(super) build_failures: HashMap<(SeriesKey, u64), String>,
    pub(super) low_resolution_prompt: Option<LowResolutionPrompt>,
    /// How each series' workspace was left when another replaced it, so a
    /// return to it opens where the user was: the planes, slabs, window and
    /// zoom, and the volume rendering camera and preset.
    pub(super) saved_views: HashMap<(SeriesKey, u64), (SavedMprView, VolumeRenderState)>,
    /// The renderer's limits, once a frame has read the wgpu render state.
    pub(super) gpu: Option<GpuInfo>,
    pub(super) gpu_available: bool,
    /// Why volume rendering gave up on this device, if it did.
    pub(super) render_failure: Option<String>,
    /// The volume whose texture the renderer was last told to keep.
    pub(super) wanted_gpu_volume: Option<u64>,
    /// Frames the tree selection has pointed at a series with no volume yet.
    pub(super) pending_selection_frames: u8,
}

impl ThreeDState {
    /// Whether the workspace on screen was built from exactly this series.
    pub(super) fn active_matches(&self, series_key: SeriesKey, fingerprint: u64) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.series_key == series_key && active.fingerprint == fingerprint
        })
    }

    /// Whether a volume for exactly this series is kept but not shown.
    pub(super) fn cached_matches(&self, series_key: SeriesKey, fingerprint: u64) -> bool {
        self.cached
            .as_ref()
            .is_some_and(|(cached_key, cached_fingerprint, _)| {
                *cached_key == series_key && *cached_fingerprint == fingerprint
            })
    }

    /// Whether a running job is already preparing exactly this series.
    pub(super) fn job_matches(&self, series_key: SeriesKey, fingerprint: u64) -> bool {
        self.job.request().is_some_and(|request| {
            request.series_key == series_key && request.fingerprint == fingerprint
        })
    }

    /// Why reconstructing this series failed, if it has been tried and failed.
    pub(super) fn build_failure(&self, series_key: SeriesKey, fingerprint: u64) -> Option<&str> {
        self.build_failures
            .get(&(series_key, fingerprint))
            .map(String::as_str)
    }
}

pub(super) struct ActiveVolumeView {
    pub(super) series_key: SeriesKey,
    pub(super) fingerprint: u64,
    /// Who and what the series is, taken when the workspace opened, so the
    /// views name the series they were built from.
    pub(super) series_overlay: Option<DicomOverlayMetadata>,
    pub(super) prepared: PreparedVolume,
    pub(super) low_resolution: bool,
    pub(super) mpr: MprState,
    pub(super) render: VolumeRenderState,
    /// How large a volume rendering texture this workspace may still try.
    pub(super) gpu_budget: GpuBudget,
}

impl ActiveVolumeView {
    /// Let go of the downsampled copy a series over the MPR budget was built
    /// with, once the planes show another grid: it can take 512 MB, and
    /// choosing Auto again resamples it anew.
    pub(super) fn release_unshown_planes(&mut self) {
        let source = &self.prepared.series.volume;

        if !Arc::ptr_eq(&self.prepared.mpr, source)
            && !Arc::ptr_eq(&self.prepared.mpr, &self.mpr.volume)
        {
            self.prepared.mpr = Arc::clone(source);
        }
    }

    /// The volume already in memory that shows `quality`, if one does: the
    /// planes' own, the one prepared with the series, or the source itself.
    pub(super) fn volume_for_quality(&self, quality: MprQuality) -> Option<Arc<Volume>> {
        let source = &self.prepared.series.volume;
        let Some(target_mm) = quality.target_mm(source) else {
            return Some(Arc::clone(source));
        };

        [&self.mpr.volume, &self.prepared.mpr, source]
            .into_iter()
            .find(|volume| volume_is_at_target(volume, source, target_mm))
            .cloned()
    }
}

/// A built volume waiting for the user to confirm its coarse geometry.
pub(super) struct LowResolutionPrompt {
    pub(super) prepared: PreparedVolume,
    pub(super) series_key: SeriesKey,
    pub(super) fingerprint: u64,
    pub(super) mode: ThreeDMode,
    /// What makes the series coarse, as the warning words it.
    pub(super) cause: String,
}

pub(crate) struct DicronApp {
    pub(super) open_source: Option<OpenSource>,
    pub(super) text_encoding: TextEncoding,
    pub(super) reopen_selection: Option<(PathBuf, u32)>,
    pub(super) reopen_view_state: Option<ReopenViewState>,
    pub(super) selected_dicom_path: Option<PathBuf>,
    pub(super) selected_dicom_frame_index: u32,
    pub(super) selected_dicom_frame_count: u32,
    pub(super) loaded_texture: Option<egui::TextureHandle>,
    pub(super) decoded_cache: DecodedCache,
    pub(super) series_thumbnails: SeriesThumbnailCache,
    pub(super) current_frame_key: Option<(PathBuf, u32)>,
    pub(super) window_level: WindowLevelState,
    pub(super) edit_windowing_dialog: EditWindowingDialogState,
    pub(super) metadata: MetadataPanelState,
    pub(super) about_dialog: AboutDialogState,
    pub(super) error_message: Option<String>,
    pub(super) dicom_index: Option<DicomIndex>,
    pub(super) selected_slice: Option<SliceSelection>,
    pub(super) last_slice_by_series: HashMap<SeriesKey, usize>,
    pub(super) scan: ScanController,
    pub(super) frame_load: FrameLoadController,
    pub(super) viewer_scroll_slice_remainder: f32,
    /// Whether a widget owned the keyboard when the last frame ended. egui
    /// releases a focused field as soon as Escape arrives, before the
    /// shortcuts are read, so that Escape is the field's and not a shortcut.
    pub(super) keyboard_owned_last_frame: bool,
    pub(super) viewport_transform: ViewportTransform,
    pub(super) viewport_zoom_anchor: Option<egui::Pos2>,
    pub(super) playback: PlaybackState,

    pub(super) three_d: ThreeDState,

    pub(super) panel_layout: PanelLayout,
    // Bumped when the preference changes so CollapsingHeaders get fresh ids.
    pub(super) tree_view_generation: u64,
    pub(super) settings: AppSettings,
}

impl Default for DicronApp {
    fn default() -> Self {
        let settings = AppSettings::load();
        let about_dialog = AboutDialogState::new(settings.check_for_updates_on_startup);

        Self {
            open_source: None,
            text_encoding: TextEncoding::Auto,
            reopen_selection: None,
            reopen_view_state: None,
            selected_dicom_path: None,
            selected_dicom_frame_index: 0,
            selected_dicom_frame_count: 1,
            loaded_texture: None,
            decoded_cache: DecodedCache::default(),
            series_thumbnails: SeriesThumbnailCache::default(),
            current_frame_key: None,
            window_level: WindowLevelState::default(),
            edit_windowing_dialog: EditWindowingDialogState::default(),
            metadata: MetadataPanelState::default(),
            about_dialog,
            error_message: None,
            dicom_index: None,
            selected_slice: None,
            last_slice_by_series: HashMap::new(),
            scan: ScanController::default(),
            frame_load: FrameLoadController::default(),
            viewer_scroll_slice_remainder: 0.0,
            keyboard_owned_last_frame: false,
            viewport_transform: ViewportTransform::default(),
            viewport_zoom_anchor: None,
            playback: PlaybackState::default(),
            three_d: ThreeDState::default(),
            panel_layout: PanelLayout::default(),
            tree_view_generation: 0,
            settings,
        }
    }
}
