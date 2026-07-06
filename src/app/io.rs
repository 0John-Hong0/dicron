use super::cache::DecodedCacheEntry;
use super::*;

/// Owned values copied out of a cached frame so the cache borrow can be
/// released before `self` is mutated in `load_dicom_path`.
struct PreparedFrame {
    default_center: f64,
    default_width: f64,
    window_customized: bool,
    center: f64,
    width: f64,
    frame_count: u32,
    value_range: (f64, f64),
    metadata: DicomMetadata,
    render: anyhow::Result<egui::ColorImage>,
}

impl DicronApp {
    pub fn open_startup_paths(&mut self, context: &egui::Context, startup_paths: Vec<PathBuf>) {
        if startup_paths.is_empty() {
            return;
        }

        let accepted_paths = filter_accepted_dropped_paths(startup_paths);

        if accepted_paths.is_empty() {
            self.error_message =
                Some("No readable DICOM files or folders were provided.".to_owned());
            return;
        }

        if accepted_paths.len() == 1 {
            let startup_path = accepted_paths[0].clone();

            if startup_path.is_dir() {
                self.open_dicom_folder_path(context, startup_path);
                return;
            }

            if startup_path.is_file() {
                self.open_dicom_file_path(context, startup_path);
                return;
            }
        }

        self.open_dropped_dicom_inputs(context, accepted_paths);
    }

    pub(super) fn open_dicom_file(&mut self, context: &egui::Context) {
        let mut file_dialog = FileDialog::new()
            .set_title("Open DICOM")
            .add_filter("DICOM", &["dcm", "dicom"]);

        if let Some(open_dicom_directory) = &self.dialog_directories.open_dicom_directory {
            file_dialog = file_dialog.set_directory(open_dicom_directory);
        }

        let Some(selected_dicom_path) = file_dialog.pick_file() else {
            return;
        };

        self.open_dicom_file_path(context, selected_dicom_path);
    }

    pub(super) fn open_dicom_file_path(
        &mut self,
        context: &egui::Context,
        selected_dicom_path: PathBuf,
    ) {
        self.dialog_directories
            .remember_open_dicom_path(&selected_dicom_path);

        self.cancel_active_scan();
        self.clear_loaded_dicom_state();
        self.scan_receiver = None;
        self.scan_state = None;

        match build_dicom_index_for_file(&selected_dicom_path) {
            Ok(dicom_index) => {
                self.dicom_index = Some(dicom_index);
                self.error_message = None;

                self.load_first_available_slice(context);
            }
            Err(error) => {
                self.error_message = Some(format!("Failed to index DICOM: {error:#}"));
            }
        }
    }

    pub(super) fn open_dicom_folder(&mut self, context: &egui::Context) {
        let mut file_dialog = FileDialog::new().set_title("Open DICOM Folder");

        if let Some(open_folder_directory) = &self.dialog_directories.open_folder_directory {
            file_dialog = file_dialog.set_directory(open_folder_directory);
        }

        let Some(selected_folder_path) = file_dialog.pick_folder() else {
            return;
        };

        self.open_dicom_folder_path(context, selected_folder_path);
    }

    pub(super) fn open_dicom_folder_path(
        &mut self,
        context: &egui::Context,
        selected_folder_path: PathBuf,
    ) {
        self.dialog_directories
            .remember_open_folder_path(&selected_folder_path);

        self.cancel_active_scan();

        let (scan_sender, scan_receiver) = mpsc::channel();

        let folder_path_for_thread = selected_folder_path.clone();
        let scan_cancel = Arc::new(AtomicBool::new(false));
        let scan_cancel_for_thread = scan_cancel.clone();

        thread::spawn(move || {
            let scan_result = build_dicom_index_with_progress(
                &folder_path_for_thread,
                &scan_cancel_for_thread,
                |progress| {
                    let _ = scan_sender.send(DicomFolderScanMessage::Progress(progress));
                },
            )
            .map_err(|error| format!("{error:#}"));

            let _ = scan_sender.send(DicomFolderScanMessage::Finished(scan_result));
        });

        self.clear_loaded_dicom_state();

        self.scan_cancel = Some(scan_cancel);
        self.scan_receiver = Some(scan_receiver);
        self.scan_state = Some(DicomFolderScanState {
            source_label: selected_folder_path.display().to_string(),
            started_at: Instant::now(),
            processed_file_count: 0,
            total_file_count: 0,
            readable_dicom_count: 0,
        });

        self.error_message = None;

        context.request_repaint();
    }

    pub(super) fn handle_dropped_paths(&mut self, context: &egui::Context) {
        let dropped_paths: Vec<PathBuf> = context.input(|input_state| {
            input_state
                .raw
                .dropped_files
                .iter()
                .filter_map(|dropped_file| dropped_file.path.clone())
                .collect()
        });

        if dropped_paths.is_empty() {
            return;
        }

        let accepted_paths = filter_accepted_dropped_paths(dropped_paths);

        if accepted_paths.is_empty() {
            self.error_message = Some("Drop DICOM files or folders only.".to_owned());
            return;
        }

        if accepted_paths.len() == 1 {
            let dropped_path = accepted_paths[0].clone();

            if dropped_path.is_dir() {
                self.open_dicom_folder_path(context, dropped_path);
                return;
            }

            if dropped_path.is_file() {
                self.open_dicom_file_path(context, dropped_path);
                return;
            }
        }

        self.open_dropped_dicom_inputs(context, accepted_paths);
    }

    pub(super) fn open_dropped_dicom_inputs(
        &mut self,
        context: &egui::Context,
        dropped_paths: Vec<PathBuf>,
    ) {
        if let Some(first_file_path) = dropped_paths.iter().find(|path| path.is_file()) {
            self.dialog_directories
                .remember_open_dicom_path(first_file_path);
        }

        if let Some(first_folder_path) = dropped_paths.iter().find(|path| path.is_dir()) {
            self.dialog_directories
                .remember_open_folder_path(first_folder_path);
        }

        self.cancel_active_scan();

        let source_label = format!("{} dropped paths", dropped_paths.len());
        let (scan_sender, scan_receiver) = mpsc::channel();
        let input_paths_for_thread = dropped_paths;
        let scan_cancel = Arc::new(AtomicBool::new(false));
        let scan_cancel_for_thread = scan_cancel.clone();

        thread::spawn(move || {
            let scan_result = build_dicom_index_for_inputs_with_progress(
                &input_paths_for_thread,
                &scan_cancel_for_thread,
                |progress| {
                    let _ = scan_sender.send(DicomFolderScanMessage::Progress(progress));
                },
            )
            .map_err(|error| format!("{error:#}"));

            let _ = scan_sender.send(DicomFolderScanMessage::Finished(scan_result));
        });

        self.clear_loaded_dicom_state();

        self.scan_cancel = Some(scan_cancel);
        self.scan_receiver = Some(scan_receiver);
        self.scan_state = Some(DicomFolderScanState {
            source_label,
            started_at: Instant::now(),
            processed_file_count: 0,
            total_file_count: 0,
            readable_dicom_count: 0,
        });

        self.error_message = None;

        context.request_repaint();
    }

    /// Signal the currently running folder scan (if any) to stop. The detached
    /// scan thread checks this flag between files and returns early, so opening
    /// a new folder/file does not leave the old scan churning over the disk.
    pub(super) fn cancel_active_scan(&mut self) {
        if let Some(scan_cancel) = self.scan_cancel.take() {
            scan_cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn clear_loaded_dicom_state(&mut self) {
        self.selected_dicom_path = None;
        self.loaded_texture = None;
        self.decoded_cache.clear();
        self.current_frame_key = None;
        self.window_customized = false;
        self.clear_metadata_items();

        self.dicom_index = None;
        self.window_level_by_series.clear();
        self.clear_selected_indices();
    }

    pub(super) fn receive_scan_messages(&mut self, context: &egui::Context) {
        let Some(scan_receiver) = self.scan_receiver.take() else {
            return;
        };

        let mut should_keep_receiver = true;

        loop {
            match scan_receiver.try_recv() {
                Ok(DicomFolderScanMessage::Progress(progress)) => {
                    if let Some(scan_state) = &mut self.scan_state {
                        scan_state.processed_file_count = progress.processed_file_count;
                        scan_state.total_file_count = progress.total_file_count;
                        scan_state.readable_dicom_count = progress.readable_dicom_count;
                    }
                }
                Ok(DicomFolderScanMessage::Finished(scan_result)) => {
                    should_keep_receiver = false;
                    self.scan_state = None;
                    self.scan_cancel = None;

                    match scan_result {
                        Ok(dicom_index) => {
                            if dicom_index.total_file_count == 0 {
                                self.error_message =
                                    Some("No readable DICOM files found.".to_owned());
                                self.dicom_index = None;
                            } else {
                                self.dicom_index = Some(dicom_index);
                                self.error_message = None;
                                self.load_first_available_slice(context);
                            }
                        }
                        Err(error_message) => {
                            self.error_message =
                                Some(format!("Failed to scan folder: {error_message}"));
                            self.dicom_index = None;
                        }
                    }

                    break;
                }
                Err(TryRecvError::Empty) => {
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    should_keep_receiver = false;
                    self.scan_state = None;
                    self.scan_cancel = None;
                    self.error_message = Some("Folder scan stopped unexpectedly.".to_owned());
                    break;
                }
            }
        }

        if should_keep_receiver {
            self.scan_receiver = Some(scan_receiver);
            context.request_repaint_after(Duration::from_millis(100));
        }
    }

    pub(super) fn load_dicom_path(
        &mut self,
        context: &egui::Context,
        dicom_path: PathBuf,
        frame_index: u32,
    ) {
        if self.decoded_cache.get(&dicom_path, frame_index).is_none() {
            match load_dicom_frame(&dicom_path, frame_index) {
                Ok(loaded) => self.decoded_cache.insert(DecodedCacheEntry {
                    path: dicom_path.clone(),
                    frame_index,
                    frame: loaded.frame,
                    metadata: loaded.metadata,
                }),
                Err(error) => {
                    self.error_message = Some(format!("Failed to open DICOM: {error:#}"));
                    return;
                }
            }
        }

        let saved_window_level = self.current_series_window_level();

        // Borrow the cached entry just long enough to render and copy out the
        // scalars we need; everything returned here is owned so the borrow ends
        // before we mutate `self`.
        let prepared = {
            let entry = self
                .decoded_cache
                .get(&dicom_path, frame_index)
                .expect("frame is present in cache");

            let (default_center, default_width) = entry.frame.default_center_width();
            let window_customized = saved_window_level.is_some();
            let center =
                saved_window_level.map_or(default_center, |window_level| window_level.center);
            let width = saved_window_level.map_or(default_width, |window_level| window_level.width);

            let effective_window = if window_customized {
                Some(DicomWindow { center, width })
            } else {
                None
            };

            PreparedFrame {
                default_center,
                default_width,
                window_customized,
                center,
                width,
                frame_count: entry.frame.frame_count,
                value_range: entry.frame.value_range,
                metadata: entry.metadata.clone(),
                render: render_frame(&entry.frame, effective_window),
            }
        };

        let color_image = match prepared.render {
            Ok(color_image) => color_image,
            Err(error) => {
                self.error_message = Some(format!("Failed to render DICOM: {error:#}"));
                return;
            }
        };

        self.default_window_center = prepared.default_center;
        self.default_window_width = prepared.default_width;
        self.window_customized = prepared.window_customized;
        self.window_center = prepared.center;
        self.window_width = prepared.width;
        self.current_value_range = prepared.value_range;
        self.selected_dicom_frame_index = frame_index;
        self.selected_dicom_frame_count = prepared.frame_count;
        self.loaded_texture = Some(upload_color_image(
            context,
            texture_name(&dicom_path),
            color_image,
        ));
        self.curated_metadata_items = prepared.metadata.curated_items;
        self.all_metadata_items = prepared.metadata.all_items;
        self.current_frame_key = Some((dicom_path.clone(), frame_index));
        self.selected_dicom_path = Some(dicom_path);
        self.error_message = None;
    }

    /// Re-render the current frame after a window/level change. This reuses the
    /// cached decoded pixels, so it is a cheap LUT pass with no disk access.
    pub(super) fn refresh_dicom_texture(&mut self, context: &egui::Context) {
        let Some((path, frame_index)) = self.current_frame_key.clone() else {
            return;
        };

        let effective_window = self.effective_window();

        let rendered = {
            let Some(entry) = self.decoded_cache.get(&path, frame_index) else {
                return;
            };

            render_frame(&entry.frame, effective_window)
        };

        match rendered {
            Ok(color_image) => {
                self.loaded_texture = Some(upload_color_image(
                    context,
                    texture_name(&path),
                    color_image,
                ));
                self.error_message = None;
            }
            Err(error) => {
                self.error_message = Some(format!("Failed to refresh DICOM: {error:#}"));
            }
        }
    }
}

fn texture_name(dicom_path: &Path) -> &str {
    dicom_path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or("loaded-dicom-image")
}

fn filter_accepted_dropped_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|path| path.is_dir() || looks_like_dicom_file(path))
        .collect()
}

/// Cheap accept-filter for dropped / CLI paths. The previous implementation
/// fully parsed every file (up to pixel data) on the UI thread, which froze the
/// app when many loose files were dropped. Here we only check the DICOM Part 10
/// `DICM` preamble or a `.dcm`/`.dicom` extension; the background scan still does
/// the authoritative parse, so a false positive is harmless.
fn looks_like_dicom_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    if has_dicom_preamble(path) {
        return true;
    }

    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("dcm") | Some("dicom")
    )
}

fn has_dicom_preamble(path: &Path) -> bool {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };

    let mut header = [0u8; 132];

    if file.read_exact(&mut header).is_err() {
        return false;
    }

    &header[128..132] == b"DICM"
}
