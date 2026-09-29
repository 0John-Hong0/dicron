//! Prepare the initial/reopened image without blocking the UI loop.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use eframe::egui;

use super::DicronApp;
use super::actions::{PreparedFrame, prepare_frame, texture_name};
use super::frame_cache::DecodedCacheEntry;
use super::state::{ReopenViewState, SliceSelection};
use super::ui::upload_display_pixels;
use crate::dicom::{SliceItem, load_dicom_frame_with_encoding};

#[derive(Default)]
pub(in crate::app) struct FrameLoadController {
    receiver: Option<Receiver<FrameLoadMessage>>,
    request: Option<FrameLoadRequest>,
    cancel: Option<Arc<AtomicBool>>,
    phase: Option<FrameLoadPhase>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::app) enum FrameLoadPhase {
    Decoding,
    Rendering,
    PreparingDisplay,
}

impl FrameLoadPhase {
    pub(in crate::app) fn progress(self) -> f32 {
        match self {
            Self::Decoding => 0.0,
            Self::Rendering => 1.0 / 3.0,
            Self::PreparingDisplay => 2.0 / 3.0,
        }
    }

    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::Decoding => "Decoding image",
            Self::Rendering => "Rendering image",
            Self::PreparingDisplay => "Preparing display",
        }
    }
}

enum FrameLoadMessage {
    Progress(FrameLoadPhase),
    Finished(Result<Box<ReadyFrame>, String>),
}

struct FrameLoadRequest {
    path: PathBuf,
    frame_index: u32,
    selection: SliceSelection,
    reopen_view_state: Option<ReopenViewState>,
}

struct ReadyFrame {
    entry: DecodedCacheEntry,
    prepared: PreparedFrame,
    texture: egui::TextureHandle,
}

impl FrameLoadController {
    pub(in crate::app) fn is_active(&self) -> bool {
        self.receiver.is_some()
    }

    pub(in crate::app) fn progress(&self) -> Option<FrameLoadPhase> {
        self.phase
    }

    pub(in crate::app) fn clear(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.receiver = None;
        self.request = None;
        self.phase = None;
    }
}

impl DicronApp {
    pub(in crate::app) fn start_frame_load(
        &mut self,
        context: &egui::Context,
        slice: SliceItem,
        selection: SliceSelection,
        reopen_view_state: Option<ReopenViewState>,
    ) {
        self.frame_load.clear();
        let saved_window = self.window_level_for_selection(Some(selection));
        let text_encoding = self.text_encoding;
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.frame_load.receiver = Some(receiver);
        self.frame_load.phase = Some(FrameLoadPhase::Decoding);
        self.frame_load.cancel = Some(Arc::clone(&cancel));
        self.frame_load.request = Some(FrameLoadRequest {
            path: slice.path.clone(),
            frame_index: slice.frame_index,
            selection,
            reopen_view_state,
        });
        let context = context.clone();
        context.request_repaint();

        thread::spawn(move || {
            let result = (|| {
                let loaded =
                    load_dicom_frame_with_encoding(&slice.path, slice.frame_index, text_encoding)
                        .map_err(|error| format!("Failed to open DICOM: {error:#}"))?;
                if cancel.load(Ordering::Relaxed) {
                    return Err("Image load cancelled.".to_owned());
                }
                let entry = DecodedCacheEntry {
                    path: slice.path,
                    frame_index: slice.frame_index,
                    frame: loaded.frame,
                    metadata: loaded.metadata,
                };
                let _ = sender.send(FrameLoadMessage::Progress(FrameLoadPhase::Rendering));
                context.request_repaint();
                let (prepared, pixels) = prepare_frame(&entry, saved_window)
                    .map_err(|error| format!("Failed to render DICOM: {error:#}"))?;
                if cancel.load(Ordering::Relaxed) {
                    return Err("Image load cancelled.".to_owned());
                }
                let _ = sender.send(FrameLoadMessage::Progress(FrameLoadPhase::PreparingDisplay));
                context.request_repaint();
                // ColorImage conversion also scales with the image's pixel count.
                let texture = upload_display_pixels(&context, texture_name(&entry.path), pixels);
                Ok(Box::new(ReadyFrame {
                    entry,
                    prepared,
                    texture,
                }))
            })();
            if !cancel.load(Ordering::Relaxed) {
                let _ = sender.send(FrameLoadMessage::Finished(result));
                context.request_repaint();
            }
        });
    }

    pub(in crate::app) fn receive_frame_load(&mut self, context: &egui::Context) {
        let Some(receiver) = self.frame_load.receiver.take() else {
            return;
        };
        let result = loop {
            match receiver.try_recv() {
                Ok(FrameLoadMessage::Progress(phase)) => self.frame_load.phase = Some(phase),
                Ok(FrameLoadMessage::Finished(result)) => break result,
                Err(TryRecvError::Empty) => {
                    self.frame_load.receiver = Some(receiver);
                    context.request_repaint_after(Duration::from_millis(16));
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    break Err("Image load stopped unexpectedly.".to_owned());
                }
            }
        };
        let request = self.frame_load.request.take();
        self.frame_load.clear();
        let Some(request) = request else {
            return;
        };
        match result {
            Ok(ready) => {
                self.decoded_cache.insert(ready.entry);
                self.display_prepared_frame(
                    request.path,
                    request.frame_index,
                    Some(request.selection),
                    ready.prepared,
                    ready.texture,
                );
                if let Some(state) = request.reopen_view_state {
                    self.viewport_transform = state.viewport_transform;
                }
            }
            Err(error) => {
                self.window_level
                    .by_series
                    .remove(&request.selection.series_key());
                self.error_message = Some(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_image_load_keeps_the_ui_poll_nonblocking_and_cancels_stale_results() {
        let context = egui::Context::default();
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = DicronApp {
            frame_load: FrameLoadController {
                receiver: Some(receiver),
                request: Some(FrameLoadRequest {
                    path: PathBuf::from("pending.dcm"),
                    frame_index: 0,
                    selection: SliceSelection::new(0, 0, 0, 0),
                    reopen_view_state: None,
                }),
                cancel: Some(Arc::clone(&cancel)),
                phase: Some(FrameLoadPhase::Decoding),
            },
            ..Default::default()
        };

        // No completion is available: polling must return so another UI frame can paint.
        app.receive_frame_load(&context);
        assert!(app.frame_load.is_active());
        assert_eq!(app.frame_load.progress(), Some(FrameLoadPhase::Decoding));
        assert!(app.error_message.is_none());

        sender
            .send(FrameLoadMessage::Progress(FrameLoadPhase::Rendering))
            .unwrap();
        app.receive_frame_load(&context);
        assert!(app.frame_load.is_active());
        assert_eq!(app.frame_load.progress(), Some(FrameLoadPhase::Rendering));

        app.clear_loaded_dicom_state();
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!app.frame_load.is_active());
        assert_eq!(app.frame_load.progress(), None);
        assert!(
            sender
                .send(FrameLoadMessage::Finished(Err("stale failure".to_owned())))
                .is_err()
        );
        app.receive_frame_load(&context);
        assert!(app.error_message.is_none());
        assert!(app.selected_dicom_path.is_none());
    }
}
