use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use rfd::FileDialog;

use crate::dialog_directories::DialogDirectories;
use crate::dicom::index::{
    DicomIndex, DicomIndexProgress, PatientGroup, SliceItem, StudyGroup,
    build_dicom_index_for_file, build_dicom_index_for_inputs_with_progress,
    build_dicom_index_with_progress,
};
use crate::dicom::loader::{DecodedFrame, DicomWindow, load_dicom_frame, render_frame};
use crate::metadata::{DicomMetadata, MetadataItem};
use crate::metadata_table;
use crate::texture::{fit_image_to_available_space, upload_color_image};

mod about_dialog;
mod cache;
mod io;
mod layout;
mod tree;
mod viewer;

use about_dialog::AboutDialog;
use cache::DecodedCache;

const LEFT_PANEL_DEFAULT_WIDTH: f32 = 450.0;
const RIGHT_PANEL_DEFAULT_WIDTH: f32 = 340.0;

const LEFT_PANEL_MIN_WIDTH: f32 = 220.0;
const RIGHT_PANEL_MIN_WIDTH: f32 = 260.0;

const LEFT_PANEL_MAX_WIDTH: f32 = 700.0;
const RIGHT_PANEL_MAX_WIDTH: f32 = 800.0;

const MIN_VIEWER_WIDTH: f32 = 300.0;
const RESIZE_HANDLE_WIDTH: f32 = 8.0;
const PANEL_CONTENT_MARGIN_X: i8 = 10;
const PANEL_CONTENT_MARGIN_Y: i8 = 6;
const SIDE_PANEL_MARGIN_X: i8 = 4;
const SIDE_PANEL_MARGIN_Y: i8 = 8;
// Series with at least this many slices get an inner virtualized scroll area
// (a safety net for pathologically large series); smaller ones render inline so
// they scroll with the tree and collapse normally.
const SLICE_LIST_VIRTUALIZE_THRESHOLD: usize = 1500;
// When "expand all by default" is off, series with at least this many slices
// start collapsed.
const SERIES_AUTO_COLLAPSE_SLICE_COUNT: usize = 200;
// Patients/studies whose subtree holds at least this many slices start collapsed,
// so a large index does not lay out tens of thousands of rows every frame.
const TREE_AUTO_COLLAPSE_SLICE_COUNT: usize = 1000;
// Number of decoded frames to keep cached for instant revisit / replay.
const DECODED_CACHE_CAPACITY: usize = 32;
const VIEWER_SCROLL_SLICE_STEP: f32 = 40.0;
const KEYBOARD_PAGE_SLICE_STEP: isize = 10;
const PLAYBACK_BAR_HORIZONTAL_PADDING: f32 = 12.0;
const PLAYBACK_BAR_VERTICAL_PADDING: f32 = 4.0;
const DEFAULT_AUTOPLAY_FPS: f32 = 15.0;
const MIN_AUTOPLAY_FPS: f32 = 0.5;
const MAX_AUTOPLAY_FPS: f32 = 120.0;

type SeriesKey = (usize, usize, usize);
type SliceKey = (usize, usize, usize, usize);

#[derive(Clone, Copy)]
enum ResizeSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutoplayLoopMode {
    StopAtEnd,
    Loop,
    PingPong,
}

impl AutoplayLoopMode {
    fn label(self) -> &'static str {
        match self {
            Self::StopAtEnd => "Stop at end",
            Self::Loop => "Loop",
            Self::PingPong => "Ping-pong",
        }
    }
}

pub struct DicronApp {
    selected_dicom_path: Option<PathBuf>,
    selected_dicom_frame_index: u32,
    selected_dicom_frame_count: u32,

    loaded_texture: Option<egui::TextureHandle>,
    // Decoded frames + metadata, kept so window/level changes re-run only the
    // cheap LUT pass and revisited slices skip decoding entirely.
    decoded_cache: DecodedCache,
    current_frame_key: Option<(PathBuf, u32)>,
    current_value_range: (f64, f64),
    // Whether the user has set a custom window (drag/saved series level). When
    // false, rendering defers to the file's own VOI (VoiLutOption::Default).
    window_customized: bool,
    curated_metadata_items: Vec<MetadataItem>,
    all_metadata_items: Vec<MetadataItem>,
    metadata_search_text: String,
    show_all_metadata: bool,
    about_dialog: AboutDialog,
    error_message: Option<String>,

    window_center: f64,
    window_width: f64,
    default_window_center: f64,
    default_window_width: f64,

    dicom_index: Option<DicomIndex>,
    window_level_by_series: HashMap<SeriesKey, WindowLevel>,

    selected_patient_index: Option<usize>,
    selected_study_index: Option<usize>,
    selected_series_index: Option<usize>,
    selected_slice_index: Option<usize>,

    scan_receiver: Option<Receiver<DicomFolderScanMessage>>,
    scan_state: Option<DicomFolderScanState>,
    scan_cancel: Option<Arc<AtomicBool>>,
    viewer_scroll_accumulator: f32,
    autoplay_enabled: bool,
    autoplay_fps: f32,
    autoplay_loop_mode: AutoplayLoopMode,
    autoplay_direction: isize,
    autoplay_last_tick: Option<Instant>,

    left_panel_width: f32,
    right_panel_width: f32,
    // Bumped when the "expand by default" preference changes so the tree's
    // CollapsingHeaders get fresh ids and re-apply their default open state.
    tree_view_generation: u64,
    dialog_directories: DialogDirectories,
}

struct DicomFolderScanState {
    source_label: String,
    started_at: Instant,
    processed_file_count: usize,
    total_file_count: usize,
    readable_dicom_count: usize,
}

enum DicomFolderScanMessage {
    Progress(DicomIndexProgress),
    Finished(std::result::Result<DicomIndex, String>),
}

#[derive(Clone, Copy)]
struct WindowLevel {
    center: f64,
    width: f64,
}

impl Default for DicronApp {
    fn default() -> Self {
        Self {
            selected_dicom_path: None,
            selected_dicom_frame_index: 0,
            selected_dicom_frame_count: 1,

            loaded_texture: None,
            decoded_cache: DecodedCache::new(DECODED_CACHE_CAPACITY),
            current_frame_key: None,
            current_value_range: (-1024.0, 3071.0),
            window_customized: false,
            curated_metadata_items: Vec::new(),
            all_metadata_items: Vec::new(),
            metadata_search_text: String::new(),
            show_all_metadata: false,
            about_dialog: AboutDialog::new(),
            error_message: None,

            window_center: 128.0,
            window_width: 256.0,
            default_window_center: 128.0,
            default_window_width: 256.0,

            dicom_index: None,
            window_level_by_series: HashMap::new(),

            selected_patient_index: None,
            selected_study_index: None,
            selected_series_index: None,
            selected_slice_index: None,

            scan_receiver: None,
            scan_state: None,
            scan_cancel: None,
            viewer_scroll_accumulator: 0.0,
            autoplay_enabled: false,
            autoplay_fps: DEFAULT_AUTOPLAY_FPS,
            autoplay_loop_mode: AutoplayLoopMode::Loop,
            autoplay_direction: 1,
            autoplay_last_tick: None,

            left_panel_width: LEFT_PANEL_DEFAULT_WIDTH,
            right_panel_width: RIGHT_PANEL_DEFAULT_WIDTH,
            tree_view_generation: 0,
            dialog_directories: DialogDirectories::load(),
        }
    }
}
