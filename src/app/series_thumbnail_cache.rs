//! Asynchronous, bounded thumbnail cache for the compact series browser.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui;

use crate::app::state::SeriesKey;
use crate::app::ui::upload_display_pixels;
use crate::dicom::{DisplayPixels, SliceItem, load_dicom_thumbnail};

const MAX_CONCURRENT_THUMBNAILS: usize = 4;
const MAX_CACHED_THUMBNAILS: usize = 128;
const MAX_THUMBNAIL_EDGE: u32 = 192;

struct ThumbnailResult {
    generation: u64,
    key: SeriesKey,
    pixels: Result<DisplayPixels, String>,
}

struct ThumbnailJob {
    generation: u64,
    cancellation: Arc<AtomicBool>,
    context: egui::Context,
    key: SeriesKey,
    path: PathBuf,
    frame_index: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ThumbnailSource {
    path: PathBuf,
    frame_index: u32,
}

impl From<&SliceItem> for ThumbnailSource {
    fn from(slice: &SliceItem) -> Self {
        Self {
            path: slice.path.clone(),
            frame_index: slice.frame_index,
        }
    }
}

fn thumbnail_sources(slices: &[SliceItem]) -> Option<(ThumbnailSource, Option<ThumbnailSource>)> {
    let primary = ThumbnailSource::from(slices.get(slices.len() / 2)?);
    let fallback = slices
        .first()
        .map(ThumbnailSource::from)
        .filter(|fallback| fallback != &primary);
    Some((primary, fallback))
}

struct PendingThumbnail {
    fallback: Option<ThumbnailSource>,
}

struct ThumbnailWorkers {
    sender: Sender<ThumbnailJob>,
    receiver: Receiver<ThumbnailResult>,
}

impl Default for ThumbnailWorkers {
    fn default() -> Self {
        let (job_sender, job_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let job_receiver = Arc::new(Mutex::new(job_receiver));

        for worker_index in 0..MAX_CONCURRENT_THUMBNAILS {
            let job_receiver = Arc::clone(&job_receiver);
            let result_sender = result_sender.clone();
            thread::Builder::new()
                .name(format!("series-thumbnail-{worker_index}"))
                .spawn(move || thumbnail_worker(job_receiver, result_sender))
                .expect("failed to spawn a series thumbnail worker");
        }

        Self {
            sender: job_sender,
            receiver: result_receiver,
        }
    }
}

fn thumbnail_worker(receiver: Arc<Mutex<Receiver<ThumbnailJob>>>, sender: Sender<ThumbnailResult>) {
    loop {
        let job = {
            let Ok(receiver) = receiver.lock() else {
                return;
            };
            receiver.recv()
        };
        let Ok(job) = job else {
            return;
        };

        if job.cancellation.load(Ordering::Acquire) {
            continue;
        }

        let pixels = load_dicom_thumbnail(&job.path, job.frame_index, MAX_THUMBNAIL_EDGE)
            .map_err(|error| format!("{error:#}"));

        if job.cancellation.load(Ordering::Acquire) {
            continue;
        }

        if sender
            .send(ThumbnailResult {
                generation: job.generation,
                key: job.key,
                pixels,
            })
            .is_err()
        {
            return;
        }
        job.context.request_repaint();
    }
}

pub(in crate::app) struct SeriesThumbnailCache {
    textures: HashMap<SeriesKey, egui::TextureHandle>,
    insertion_order: Vec<SeriesKey>,
    pending: HashMap<SeriesKey, PendingThumbnail>,
    failed: HashSet<SeriesKey>,
    workers: ThumbnailWorkers,
    generation: u64,
    cancellation: Arc<AtomicBool>,
}

impl Default for SeriesThumbnailCache {
    fn default() -> Self {
        Self {
            textures: HashMap::new(),
            insertion_order: Vec::new(),
            pending: HashMap::new(),
            failed: HashSet::new(),
            workers: ThumbnailWorkers::default(),
            generation: 0,
            cancellation: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl SeriesThumbnailCache {
    pub(in crate::app) fn poll(&mut self, context: &egui::Context) {
        while let Ok(result) = self.workers.receiver.try_recv() {
            self.accept_result(context, result);
        }
    }

    fn accept_result(&mut self, context: &egui::Context, result: ThumbnailResult) {
        if result.generation != self.generation {
            return;
        }

        match result.pixels {
            Ok(pixels) => {
                self.pending.remove(&result.key);
                let texture_name = format!(
                    "series-thumbnail-{}-{}-{}",
                    result.key.0, result.key.1, result.key.2
                );
                let texture = upload_display_pixels(context, &texture_name, pixels);
                self.textures.insert(result.key, texture);
                self.insertion_order.push(result.key);
                self.evict_oldest_textures();
            }
            Err(_) => {
                let fallback = self
                    .pending
                    .get_mut(&result.key)
                    .and_then(|pending| pending.fallback.take());
                if fallback.is_none_or(|source| !self.enqueue(context, result.key, source)) {
                    self.pending.remove(&result.key);
                    self.failed.insert(result.key);
                }
            }
        }
    }

    pub(in crate::app) fn request(
        &mut self,
        context: &egui::Context,
        key: SeriesKey,
        slices: &[SliceItem],
    ) {
        if self.textures.contains_key(&key)
            || self.pending.contains_key(&key)
            || self.failed.contains(&key)
            || self.pending.len() >= MAX_CONCURRENT_THUMBNAILS
        {
            return;
        }

        let Some((primary, fallback)) = thumbnail_sources(slices) else {
            return;
        };
        self.pending.insert(key, PendingThumbnail { fallback });
        if !self.enqueue(context, key, primary) {
            self.pending.remove(&key);
            self.failed.insert(key);
        }
    }

    fn enqueue(&self, context: &egui::Context, key: SeriesKey, source: ThumbnailSource) -> bool {
        let job = ThumbnailJob {
            generation: self.generation,
            cancellation: Arc::clone(&self.cancellation),
            context: context.clone(),
            key,
            path: source.path,
            frame_index: source.frame_index,
        };
        self.workers.sender.send(job).is_ok()
    }

    pub(in crate::app) fn texture(&self, key: SeriesKey) -> Option<&egui::TextureHandle> {
        self.textures.get(&key)
    }

    pub(in crate::app) fn failed(&self, key: SeriesKey) -> bool {
        self.failed.contains(&key)
    }

    pub(in crate::app) fn clear(&mut self) {
        self.cancellation.store(true, Ordering::Release);
        self.generation = self.generation.wrapping_add(1);
        self.cancellation = Arc::new(AtomicBool::new(false));
        self.textures.clear();
        self.insertion_order.clear();
        self.pending.clear();
        self.failed.clear();

        while self.workers.receiver.try_recv().is_ok() {}
    }

    fn evict_oldest_textures(&mut self) {
        while self.insertion_order.len() > MAX_CACHED_THUMBNAILS {
            let oldest = self.insertion_order.remove(0);
            self.textures.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn thumbnail_sources_use_a_stable_middle_then_first_policy() {
        let slices = [
            slice("first.dcm", 0),
            slice("middle.dcm", 1),
            slice("last.dcm", 2),
        ];

        let (primary, fallback) = thumbnail_sources(&slices).unwrap();

        assert_eq!(primary, ThumbnailSource::from(&slices[1]));
        assert_eq!(fallback, Some(ThumbnailSource::from(&slices[0])));
        assert!(thumbnail_sources(&[]).is_none());
    }

    #[test]
    fn requests_stop_at_the_concurrent_job_limit() {
        let mut cache = SeriesThumbnailCache::default();
        let context = egui::Context::default();
        let slices = [slice("missing.dcm", 0)];

        for series_index in 0..=MAX_CONCURRENT_THUMBNAILS {
            cache.request(&context, (0, 0, series_index), &slices);
        }

        assert_eq!(cache.pending.len(), MAX_CONCURRENT_THUMBNAILS);
        assert!(
            !cache
                .pending
                .contains_key(&(0, 0, MAX_CONCURRENT_THUMBNAILS))
        );
    }

    #[test]
    fn completed_thumbnails_evict_the_oldest_texture_at_the_cache_limit() {
        let mut cache = SeriesThumbnailCache::default();
        let context = egui::Context::default();

        for series_index in 0..=MAX_CACHED_THUMBNAILS {
            cache.accept_result(
                &context,
                ThumbnailResult {
                    generation: cache.generation,
                    key: (0, 0, series_index),
                    pixels: Ok(DisplayPixels {
                        width: 1,
                        height: 1,
                        rgba: vec![0, 0, 0, 255],
                    }),
                },
            );
        }

        assert_eq!(cache.textures.len(), MAX_CACHED_THUMBNAILS);
        assert_eq!(cache.insertion_order.len(), MAX_CACHED_THUMBNAILS);
        assert!(!cache.textures.contains_key(&(0, 0, 0)));
        assert!(cache.textures.contains_key(&(0, 0, MAX_CACHED_THUMBNAILS)));
    }

    #[test]
    fn clearing_the_cache_cancels_old_jobs_and_resets_generation_state() {
        let mut cache = SeriesThumbnailCache::default();
        let old_cancellation = Arc::clone(&cache.cancellation);
        let old_generation = cache.generation;
        cache
            .pending
            .insert((0, 0, 0), PendingThumbnail { fallback: None });
        cache.failed.insert((0, 0, 1));

        cache.clear();

        assert!(old_cancellation.load(Ordering::Acquire));
        assert_eq!(cache.generation, old_generation + 1);
        assert!(!cache.cancellation.load(Ordering::Acquire));
        assert!(cache.pending.is_empty());
        assert!(cache.failed.is_empty());
    }

    #[test]
    fn stale_results_cannot_change_the_current_cache_generation() {
        let mut cache = SeriesThumbnailCache::default();
        let stale_generation = cache.generation;
        cache.clear();
        let key = (1, 2, 3);
        cache
            .pending
            .insert(key, PendingThumbnail { fallback: None });

        cache.accept_result(
            &egui::Context::default(),
            ThumbnailResult {
                generation: stale_generation,
                key,
                pixels: Err("stale failure".to_owned()),
            },
        );

        assert!(cache.pending.contains_key(&key));
        assert!(!cache.failed.contains(&key));
    }

    #[test]
    fn a_failed_middle_slice_retries_the_first_slice_once() {
        let mut cache = SeriesThumbnailCache::default();
        let key = (1, 2, 3);
        let fallback = ThumbnailSource {
            path: PathBuf::from("first.dcm"),
            frame_index: 0,
        };
        cache.pending.insert(
            key,
            PendingThumbnail {
                fallback: Some(fallback),
            },
        );

        cache.accept_result(
            &egui::Context::default(),
            ThumbnailResult {
                generation: cache.generation,
                key,
                pixels: Err("middle slice failed".to_owned()),
            },
        );

        assert!(cache.pending.contains_key(&key));
        assert!(cache.pending[&key].fallback.is_none());
        assert!(!cache.failed.contains(&key));

        cache.accept_result(
            &egui::Context::default(),
            ThumbnailResult {
                generation: cache.generation,
                key,
                pixels: Err("first slice failed".to_owned()),
            },
        );

        assert!(!cache.pending.contains_key(&key));
        assert!(cache.failed.contains(&key));
    }
}
