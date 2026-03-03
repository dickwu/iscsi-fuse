use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

/// LBA-indexed LRU cache of block data.
pub struct BlockCache {
    inner: Mutex<LruCache<u64, Vec<u8>>>,
}

impl BlockCache {
    pub fn new(capacity_blocks: usize) -> Self {
        let cap = NonZeroUsize::new(capacity_blocks.max(1))
            .expect("capacity must be > 0");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Get cached block data for a given LBA. Returns None on miss.
    pub fn get(&self, lba: u64) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().get(&lba).cloned()
    }

    /// Insert block data into cache.
    pub fn put(&self, lba: u64, data: Vec<u8>) {
        self.inner.lock().unwrap().put(lba, data);
    }

    /// Invalidate a range of LBAs (used after writes).
    pub fn invalidate_range(&self, start_lba: u64, count: u64) {
        let mut cache = self.inner.lock().unwrap();
        for lba in start_lba..start_lba + count {
            cache.pop(&lba);
        }
    }

}
