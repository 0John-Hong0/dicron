//! Asynchronous directory scanning and release-check coordination.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, atomic::AtomicBool};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::DicronApp;
use crate::dicom::{
    BuildProgress, DicomIndex, build_for_inputs_with_progress, build_from_folder_with_progress,
};
use crate::release_check::{self, UpdateCheckOutcome};

pub(in crate::app) struct ReleaseCheckJob {
    receiver: Option<Receiver<std::result::Result<UpdateCheckOutcome, String>>>,
}

impl ReleaseCheckJob {
    pub(in crate::app) fn new() -> Self {
        Self { receiver: None }
    }

    pub(in crate::app) fn start(&mut self, context: &egui::Context) -> bool {
        if self.receiver.is_some() {
            return false;
        }

        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        let context = context.clone();

        thread::spawn(move || {
            let result = release_check::check_latest_release(env!("CARGO_PKG_VERSION"))
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
            context.request_repaint();
        });

        true
    }

    pub(in crate::app) fn poll(
        &mut self,
        context: &egui::Context,
    ) -> Option<std::result::Result<UpdateCheckOutcome, String>> {
        let receiver = self.receiver.take()?;

        match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Disconnected) => {
                Some(Err("Update check stopped unexpectedly.".to_owned()))
            }
            Err(TryRecvError::Empty) => {
                self.receiver = Some(receiver);
                context.request_repaint_after(Duration::from_millis(100));
                None
            }
        }
    }
}

#[derive(Default)]
pub(in crate::app) struct ScanController {
    receiver: Option<Receiver<ScanMessage>>,
    state: Option<ScanProgress>,
    cancel: Option<Arc<AtomicBool>>,
}

impl ScanController {
    pub(in crate::app) fn is_active(&self) -> bool {
        self.state.is_some()
    }

    pub(in crate::app) fn progress(&self) -> Option<&ScanProgress> {
        self.state.as_ref()
    }
}

pub(in crate::app) struct ScanProgress {
    pub(in crate::app) source_label: String,
    pub(in crate::app) started_at: Instant,
    pub(in crate::app) processed_file_count: usize,
    pub(in crate::app) total_file_count: usize,
    pub(in crate::app) readable_dicom_count: usize,
}

enum ScanMessage {
    Progress(BuildProgress),
    Finished(std::result::Result<DicomIndex, String>),
}

impl DicronApp {
    pub(in crate::app) fn open_dicom_folder_path(
        &mut self,
        context: &egui::Context,
        selected_folder_path: PathBuf,
    ) {
        self.settings
            .remember_open_folder_path(&selected_folder_path);
        self.cancel_active_scan();

        let (scan_sender, scan_receiver) = mpsc::channel();
        let folder_path_for_thread = selected_folder_path.clone();
        let scan_cancel = Arc::new(AtomicBool::new(false));
        let scan_cancel_for_thread = Arc::clone(&scan_cancel);

        thread::spawn(move || {
            let scan_result = build_from_folder_with_progress(
                &folder_path_for_thread,
                &scan_cancel_for_thread,
                |progress| {
                    let _ = scan_sender.send(ScanMessage::Progress(progress));
                },
            )
            .map_err(|error| format!("{error:#}"));
            let _ = scan_sender.send(ScanMessage::Finished(scan_result));
        });

        self.clear_loaded_dicom_state();
        self.scan.cancel = Some(scan_cancel);
        self.scan.receiver = Some(scan_receiver);
        self.scan.state = Some(ScanProgress {
            source_label: selected_folder_path.display().to_string(),
            started_at: Instant::now(),
            processed_file_count: 0,
            total_file_count: 0,
            readable_dicom_count: 0,
        });
        self.error_message = None;
        context.request_repaint();
    }

    pub(in crate::app) fn open_dropped_dicom_inputs(
        &mut self,
        context: &egui::Context,
        dropped_paths: Vec<PathBuf>,
    ) {
        if let Some(first_file_path) = dropped_paths.iter().find(|path| path.is_file()) {
            self.settings.remember_open_dicom_path(first_file_path);
        }
        if let Some(first_folder_path) = dropped_paths.iter().find(|path| path.is_dir()) {
            self.settings.remember_open_folder_path(first_folder_path);
        }

        self.cancel_active_scan();
        let source_label = format!("{} dropped paths", dropped_paths.len());
        let (scan_sender, scan_receiver) = mpsc::channel();
        let scan_cancel = Arc::new(AtomicBool::new(false));
        let scan_cancel_for_thread = Arc::clone(&scan_cancel);

        thread::spawn(move || {
            let scan_result = build_for_inputs_with_progress(
                &dropped_paths,
                &scan_cancel_for_thread,
                |progress| {
                    let _ = scan_sender.send(ScanMessage::Progress(progress));
                },
            )
            .map_err(|error| format!("{error:#}"));
            let _ = scan_sender.send(ScanMessage::Finished(scan_result));
        });

        self.clear_loaded_dicom_state();
        self.scan.cancel = Some(scan_cancel);
        self.scan.receiver = Some(scan_receiver);
        self.scan.state = Some(ScanProgress {
            source_label,
            started_at: Instant::now(),
            processed_file_count: 0,
            total_file_count: 0,
            readable_dicom_count: 0,
        });
        self.error_message = None;
        context.request_repaint();
    }

    pub(in crate::app) fn cancel_active_scan(&mut self) {
        if let Some(scan_cancel) = self.scan.cancel.take() {
            scan_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(in crate::app) fn clear_scan(&mut self) {
        self.scan.receiver = None;
        self.scan.state = None;
        self.scan.cancel = None;
    }

    pub(in crate::app) fn receive_scan_messages(&mut self, context: &egui::Context) {
        let Some(scan_receiver) = self.scan.receiver.take() else {
            return;
        };
        let mut should_keep_receiver = true;

        loop {
            match scan_receiver.try_recv() {
                Ok(ScanMessage::Progress(progress)) => {
                    if let Some(scan_state) = &mut self.scan.state {
                        scan_state.processed_file_count = progress.processed_file_count;
                        scan_state.total_file_count = progress.total_file_count;
                        scan_state.readable_dicom_count = progress.readable_dicom_count;
                    }
                }
                Ok(ScanMessage::Finished(scan_result)) => {
                    should_keep_receiver = false;
                    self.scan.state = None;
                    self.scan.cancel = None;

                    match scan_result {
                        Ok(dicom_index) if dicom_index.total_file_count > 0 => {
                            self.dicom_index = Some(dicom_index);
                            self.error_message = None;
                            self.load_first_available_slice(context);
                        }
                        Ok(_) => {
                            self.error_message = Some("No readable DICOM files found.".to_owned());
                            self.dicom_index = None;
                        }
                        Err(error_message) => {
                            self.error_message =
                                Some(format!("Failed to scan folder: {error_message}"));
                            self.dicom_index = None;
                        }
                    }
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    should_keep_receiver = false;
                    self.scan.state = None;
                    self.scan.cancel = None;
                    self.error_message = Some("Folder scan stopped unexpectedly.".to_owned());
                    break;
                }
            }
        }

        if should_keep_receiver {
            self.scan.receiver = Some(scan_receiver);
            context.request_repaint_after(Duration::from_millis(100));
        }
    }
}
