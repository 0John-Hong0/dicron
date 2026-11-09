//! Most-recently-used cache of decoded DICOM frames and metadata.

use std::path::{Path, PathBuf};

use crate::dicom::{DecodedFrame, DicomMetadata, MetadataItem};

const DEFAULT_CACHE_CAPACITY: usize = 32;
/// Holds a full cache of 512 x 512 CT or MR slices (about 2.6 MB each, under
/// 90 MB for 32), yet only about five 3000 x 3000 radiographs or three
/// 4096 x 3328 mammograms, which keep about 10 bytes per pixel.
const DEFAULT_CACHE_BYTE_BUDGET: usize = 512 << 20;

pub(in crate::app) struct DecodedCacheEntry {
    pub(in crate::app) path: PathBuf,
    pub(in crate::app) frame_index: u32,
    pub(in crate::app) frame: DecodedFrame,
    pub(in crate::app) metadata: DicomMetadata,
}

/// Small most-recently-used cache of decoded frames + metadata, so navigating
/// back to a slice (or looping/ping-ponging in autoplay) skips the open+decode
/// entirely instead of re-reading and re-decompressing the file each time.
/// Bounded by entry count and by the bytes the entries keep; the newest entry,
/// the one on screen, is kept even when it alone exceeds the byte budget.
pub(in crate::app) struct DecodedCache {
    /// Each entry with its resident bytes, measured once on insert.
    entries: Vec<(DecodedCacheEntry, usize)>,
    capacity: usize,
    byte_budget: usize,
}

impl Default for DecodedCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY, DEFAULT_CACHE_BYTE_BUDGET)
    }
}

impl DecodedCache {
    fn new(capacity: usize, byte_budget: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity: capacity.max(1),
            byte_budget,
        }
    }

    fn resident_bytes(&self) -> usize {
        self.entries.iter().map(|(_, bytes)| bytes).sum()
    }

    fn position(&self, path: &Path, frame_index: u32) -> Option<usize> {
        self.entries
            .iter()
            .position(|(entry, _)| entry.frame_index == frame_index && entry.path == path)
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
        self.entries.last().map(|(entry, _)| entry)
    }

    pub(in crate::app) fn peek(&self, path: &Path, frame_index: u32) -> Option<&DecodedCacheEntry> {
        self.entries
            .get(self.position(path, frame_index)?)
            .map(|(entry, _)| entry)
    }

    /// Store the newly displayed frame and evict older entries to meet the cache budgets.
    pub(in crate::app) fn insert(&mut self, entry: DecodedCacheEntry) {
        if let Some(position) = self.position(&entry.path, entry.frame_index) {
            self.entries.remove(position);
        }

        let bytes = entry.frame.resident_bytes() + metadata_resident_bytes(&entry.metadata);
        self.entries.push((entry, bytes));

        while self.entries.len() > self.capacity
            || (self.entries.len() > 1 && self.resident_bytes() > self.byte_budget)
        {
            self.entries.remove(0);
        }
    }

    pub(in crate::app) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Approximate heap bytes of the metadata item trees: every item's slot in
/// its list and its strings, counting the search copy as the same length as
/// the four strings it joins. The overlay fields are a few short strings and
/// are not counted.
fn metadata_resident_bytes(metadata: &DicomMetadata) -> usize {
    fn items_bytes(items: &[MetadataItem]) -> usize {
        items
            .iter()
            .map(|item| {
                let text_bytes =
                    item.tag.len() + item.description.len() + item.value.len() + item.path.len();
                size_of::<MetadataItem>() + 2 * text_bytes + items_bytes(&item.children)
            })
            .sum()
    }

    items_bytes(&metadata.curated_items) + items_bytes(&metadata.all_items)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        DEFAULT_CACHE_BYTE_BUDGET, DEFAULT_CACHE_CAPACITY, DecodedCache, DecodedCacheEntry,
    };
    use crate::dicom::{DicomMetadata, MetadataItem, MetadataItemKind, monochrome_test_frame};

    /// 10 x 10 16-bit pixels at 10 bytes each (sample plus f64 probe value).
    const SMALL_FRAME_BYTES: usize = 1_000;

    fn entry(name: &str, rows: u16, columns: u16) -> DecodedCacheEntry {
        DecodedCacheEntry {
            path: PathBuf::from(name),
            frame_index: 0,
            frame: monochrome_test_frame(rows, columns),
            metadata: DicomMetadata {
                curated_items: Vec::new(),
                all_items: Vec::new(),
                overlay: Default::default(),
            },
        }
    }

    fn small_entry(name: &str) -> DecodedCacheEntry {
        entry(name, 10, 10)
    }

    fn cached_names(cache: &DecodedCache, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .filter(|name| cache.peek(Path::new(name), 0).is_some())
            .map(|name| (*name).to_owned())
            .collect()
    }

    #[test]
    fn entries_over_the_byte_budget_evict_the_least_recently_used() {
        let mut cache = DecodedCache::new(32, 3 * SMALL_FRAME_BYTES + SMALL_FRAME_BYTES / 2);
        for name in ["a", "b", "c", "d"] {
            cache.insert(small_entry(name));
        }
        assert_eq!(cached_names(&cache, &["a", "b", "c", "d"]), ["b", "c", "d"]);

        // A frame twice the size pushes out the two oldest, never itself.
        cache.insert(entry("wide", 10, 20));
        assert_eq!(
            cached_names(&cache, &["b", "c", "d", "wide"]),
            ["d", "wide"]
        );
    }

    #[test]
    fn an_entry_larger_than_the_budget_is_kept_alone() {
        let mut cache = DecodedCache::new(32, 3 * SMALL_FRAME_BYTES);
        cache.insert(small_entry("a"));
        cache.insert(small_entry("b"));

        cache.insert(entry("huge", 100, 100));

        assert_eq!(cached_names(&cache, &["a", "b", "huge"]), ["huge"]);
    }

    #[test]
    fn the_entry_count_cap_still_applies_under_the_byte_budget() {
        let mut cache = DecodedCache::new(2, usize::MAX);
        for name in ["a", "b", "c"] {
            cache.insert(small_entry(name));
        }

        assert_eq!(cached_names(&cache, &["a", "b", "c"]), ["b", "c"]);
    }

    #[test]
    fn getting_an_entry_protects_it_from_byte_eviction() {
        let mut cache = DecodedCache::new(32, 3 * SMALL_FRAME_BYTES);
        for name in ["a", "b", "c"] {
            cache.insert(small_entry(name));
        }

        assert!(cache.get(Path::new("a"), 0).is_some());
        cache.insert(small_entry("d"));

        assert_eq!(cached_names(&cache, &["a", "b", "c", "d"]), ["a", "c", "d"]);
    }

    /// Decoding a cached frame again replaces its entry; the old copy is not
    /// counted against the budget, so the other entries stay.
    #[test]
    fn inserting_a_cached_frame_again_evicts_nothing() {
        let mut cache = DecodedCache::new(32, 3 * SMALL_FRAME_BYTES);
        for name in ["a", "b", "c", "b"] {
            cache.insert(small_entry(name));
        }

        assert_eq!(cached_names(&cache, &["a", "b", "c"]), ["a", "b", "c"]);
        cache.insert(small_entry("d"));
        // "a" is the least recently used.
        assert_eq!(cached_names(&cache, &["a", "b", "c", "d"]), ["b", "c", "d"]);
    }

    /// The entry on screen is the newest; when it alone is over the budget it
    /// stays, and the next frame that fits replaces it as the newest.
    #[test]
    fn an_oversized_entry_leaves_once_a_newer_one_arrives() {
        let mut cache = DecodedCache::new(32, 3 * SMALL_FRAME_BYTES);
        cache.insert(entry("huge", 100, 100));
        cache.insert(small_entry("a"));

        assert_eq!(cached_names(&cache, &["huge", "a"]), ["a"]);
    }

    /// An enhanced multi-frame object keeps a functional group item for every
    /// frame, so its metadata tree can outweigh the frame. The budget bounds
    /// the whole entry: entries with small frames but a large tree still evict
    /// the least recently used.
    #[test]
    fn large_metadata_trees_count_against_the_byte_budget() {
        let functional_groups = |name: &str| {
            let frames = (1..=2_000)
                .map(|frame| {
                    let item_path = format!("(5200,9230) / Item {frame}");
                    let leaf = MetadataItem::with_structure(
                        "(0020,9157)".to_owned(),
                        "DimensionIndexValues".to_owned(),
                        format!("{frame}\\1"),
                        format!("{item_path} / (0020,9111) / Item 1 / (0020,9157)"),
                        MetadataItemKind::Element,
                        Vec::new(),
                    );
                    MetadataItem::with_structure(
                        String::new(),
                        format!("Item {frame}"),
                        "1 element".to_owned(),
                        item_path,
                        MetadataItemKind::Item,
                        vec![leaf],
                    )
                })
                .collect();
            let mut entry = small_entry(name);
            entry.metadata.all_items = vec![MetadataItem::with_structure(
                "(5200,9230)".to_owned(),
                "PerFrameFunctionalGroupsSequence".to_owned(),
                "2000 items".to_owned(),
                "(5200,9230)".to_owned(),
                MetadataItemKind::Sequence,
                frames,
            )];
            entry
        };
        // Room for ten small entries, and well under 2,000 items.
        let mut cache = DecodedCache::new(32, 10 * SMALL_FRAME_BYTES);
        for name in ["a", "b", "c"] {
            cache.insert(functional_groups(name));
        }

        assert_eq!(cached_names(&cache, &["a", "b", "c"]), ["c"]);
    }

    /// Scrolling back and forth through a CT series must stay cached: a full
    /// cache of 512 x 512 16-bit slices fits the default budget.
    #[test]
    fn the_default_budget_holds_a_full_cache_of_ct_slices() {
        let ct_slice_bytes = monochrome_test_frame(512, 512).resident_bytes();

        assert!(DEFAULT_CACHE_CAPACITY * ct_slice_bytes <= DEFAULT_CACHE_BYTE_BUDGET);
    }
}
