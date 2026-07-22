//! Most-recently-used cache of synchronously decoded DICOM frames and metadata.

use std::path::{Path, PathBuf};

use crate::dicom::{DecodedFrame, DicomMetadata};

const DEFAULT_CACHE_CAPACITY: usize = 32;

pub(in crate::app) struct DecodedCacheEntry {
    pub(in crate::app) path: PathBuf,
    pub(in crate::app) frame_index: u32,
    pub(in crate::app) frame: DecodedFrame,
    pub(in crate::app) metadata: DicomMetadata,
}

/// Small most-recently-used cache of decoded frames + metadata, so navigating
/// back to a slice (or looping/ping-ponging in autoplay) skips the open+decode
/// entirely instead of re-reading and re-decompressing the file each time.
pub(in crate::app) struct DecodedCache {
    entries: Vec<DecodedCacheEntry>,
    capacity: usize,
}

impl Default for DecodedCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY)
    }
}

impl DecodedCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    fn position(&self, path: &Path, frame_index: u32) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.frame_index == frame_index && entry.path == path)
    }

    /// Fetch an entry, promoting it to most-recently-used.
    pub(in crate::app) fn get(
        &mut self,
        path: &Path,
        frame_index: u32,
    ) -> Option<&DecodedCacheEntry> {
        let position = self.position(path, frame_index)?;
        let entry = self.entries.remove(position);
        self.entries.push(entry);
        self.entries.last()
    }

    pub(in crate::app) fn insert(&mut self, entry: DecodedCacheEntry) {
        if let Some(position) = self.position(&entry.path, entry.frame_index) {
            self.entries.remove(position);
        }

        self.entries.push(entry);

        while self.entries.len() > self.capacity {
            self.entries.remove(0);
        }
    }

    pub(in crate::app) fn clear(&mut self) {
        self.entries.clear();
    }
}
