//! Persistent in-memory state for the running application.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use eframe::egui;

use crate::dicom::{DicomIndex, DicomOverlayMetadata, MetadataItem};
use crate::release_check::UpdateCheckOutcome;
use crate::settings::AppSettings;

use super::background_tasks::{ReleaseCheckJob, ScanController};
use super::frame_cache::DecodedCache;

pub(super) type SeriesKey = (usize, usize, usize);

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

pub(crate) struct DicronApp {
    pub(super) selected_dicom_path: Option<PathBuf>,
    pub(super) selected_dicom_frame_index: u32,
    pub(super) selected_dicom_frame_count: u32,
    pub(super) loaded_texture: Option<egui::TextureHandle>,
    pub(super) decoded_cache: DecodedCache,
    pub(super) current_frame_key: Option<(PathBuf, u32)>,
    pub(super) window_level: WindowLevelState,
    pub(super) edit_windowing_dialog: EditWindowingDialogState,
    pub(super) metadata: MetadataPanelState,
    pub(super) about_dialog: AboutDialogState,
    pub(super) error_message: Option<String>,
    pub(super) dicom_index: Option<DicomIndex>,
    pub(super) selected_slice: Option<SliceSelection>,
    pub(super) scan: ScanController,
    pub(super) viewer_scroll_accumulator: f32,
    pub(super) playback: PlaybackState,
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
            selected_dicom_path: None,
            selected_dicom_frame_index: 0,
            selected_dicom_frame_count: 1,
            loaded_texture: None,
            decoded_cache: DecodedCache::default(),
            current_frame_key: None,
            window_level: WindowLevelState::default(),
            edit_windowing_dialog: EditWindowingDialogState::default(),
            metadata: MetadataPanelState::default(),
            about_dialog,
            error_message: None,
            dicom_index: None,
            selected_slice: None,
            scan: ScanController::default(),
            viewer_scroll_accumulator: 0.0,
            playback: PlaybackState::default(),
            panel_layout: PanelLayout::default(),
            tree_view_generation: 0,
            settings,
        }
    }
}
