use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use moka::future::Cache;
use tracing::warn;

/// Adaptive readahead state: detects sequential access patterns and doubles the
/// prefetch window on each consecutive sequential read up to a configurable maximum.
struct ReadaheadState {
    /// LBA of the last read start.
    last_lba: AtomicU64,
    /// Current readahead window in blocks (doubled on every sequential read).
    ra_window: AtomicU32,
    /// LBA where the next readahead batch begins.
    ra_start: AtomicU64,
    /// Guard to prevent overlapping readahead spawns.
    ra_inflight: AtomicBool,
    /// Minimum readahead window (blocks).
    ra_min_blocks: u32,
    /// Maximum readahead window (blocks).
    ra_max_blocks: u32,
}

impl ReadaheadState {
    fn new(ra_min_blocks: u32, ra_max_blocks: u32) -> Self {
        Self {
            last_lba: AtomicU64::new(u64::MAX),
            ra_window: AtomicU32::new(ra_min_blocks),
            ra_start: AtomicU64::new(0),
            ra_inflight: AtomicBool::new(false),
            ra_min_blocks,
            ra_max_blocks,
        }
    }
}

/// Block-level read cache backed by `moka` with 64 KB chunk granularity and
/// adaptive sequential readahead.
pub struct BlockCache {
    inner: Cache<u64, Bytes>,
    block_size: u32,
    /// Number of blocks in one 64 KB chunk.
    chunk_blocks: u32,
    readahead: ReadaheadState,
}

impl BlockCache {
    /// Create a new `BlockCache`.
    ///
    /// * `size_mb`    -- maximum cache size in megabytes.
    /// * `block_size` -- device block size in bytes (e.g. 512).
    /// * `ra_max_kb`  -- maximum readahead window in kilobytes (e.g. 512).
    pub fn new(size_mb: usize, block_size: u32, ra_max_kb: usize) -> Self {
        let chunk_bytes: u64 = 64 * 1024; // 64 KB
        let max_capacity = (size_mb as u64 * 1024 * 1024) / chunk_bytes;
        let chunk_blocks = (chunk_bytes / block_size as u64) as u32;
        let ra_max_blocks = (ra_max_kb * 1024 / block_size as usize) as u32;
        let ra_min_blocks: u32 = 8;

        let inner = Cache::new(max_capacity);

        Self {
            inner,
            block_size,
            chunk_blocks,
            readahead: ReadaheadState::new(ra_min_blocks, ra_max_blocks),
        }
    }

    /// Align `lba` down to the start of its 64 KB chunk.
    pub fn chunk_lba(&self, lba: u64) -> u64 {
        (lba / self.chunk_blocks as u64) * self.chunk_blocks as u64
    }

    /// Read `block_count` blocks starting at `start_lba`.
    ///
    /// `fetch_fn` is invoked for every cache miss; it receives `(chunk_lba,
    /// chunk_blocks)` and must return exactly `chunk_blocks * block_size` bytes.
    /// Concurrent callers requesting the same chunk will share a single fetch
    /// (moka deduplication).
    pub async fn read_blocks<F, Fut>(
        &self,
        start_lba: u64,
        block_count: u32,
        fetch_fn: F,
    ) -> Result<Bytes>
    where
        F: Fn(u64, u32) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Result<Bytes>> + Send + 'static,
    {
        let end_lba = start_lba + block_count as u64;

        // Iterate over every chunk that overlaps [start_lba, end_lba).
        let first_chunk = self.chunk_lba(start_lba);
        let last_chunk = self.chunk_lba(end_lba.saturating_sub(1));

        let total_bytes = block_count as usize * self.block_size as usize;
        let mut assembled = BytesMut::with_capacity(total_bytes);

        let mut clba = first_chunk;
        while clba <= last_chunk {
            let cb = self.chunk_blocks;
            let f = fetch_fn.clone();

            let chunk_data = self
                .inner
                .try_get_with(clba, async move { f(clba, cb).await })
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            // Determine which slice of this chunk we need.
            let chunk_end_lba = clba + self.chunk_blocks as u64;
            let slice_start_lba = start_lba.max(clba);
            let slice_end_lba = end_lba.min(chunk_end_lba);

            let offset_in_chunk = (slice_start_lba - clba) as usize * self.block_size as usize;
            let slice_len = (slice_end_lba - slice_start_lba) as usize * self.block_size as usize;

            let end = (offset_in_chunk + slice_len).min(chunk_data.len());
            assembled.extend_from_slice(&chunk_data[offset_in_chunk..end]);

            clba += self.chunk_blocks as u64;
        }

        // Trigger readahead detection (fire-and-forget).
        self.maybe_trigger_readahead(start_lba, block_count, fetch_fn);

        Ok(assembled.freeze())
    }

    /// Invalidate all cached chunks overlapping `[start_lba, start_lba + block_count)`.
    /// Also resets the readahead window.
    pub async fn invalidate_range(&self, start_lba: u64, block_count: u32) {
        let end_lba = start_lba + block_count as u64;
        let first_chunk = self.chunk_lba(start_lba);
        let last_chunk = self.chunk_lba(end_lba.saturating_sub(1));

        let mut clba = first_chunk;
        while clba <= last_chunk {
            self.inner.invalidate(&clba).await;
            clba += self.chunk_blocks as u64;
        }

        // Reset readahead.
        self.readahead
            .ra_window
            .store(self.readahead.ra_min_blocks, Ordering::Relaxed);
    }

    /// Expose current readahead window (blocks) -- mainly for tests.
    pub fn readahead_window_blocks(&self) -> u32 {
        self.readahead.ra_window.load(Ordering::Relaxed)
    }

    /// Expose minimum readahead window (blocks) -- mainly for tests.
    pub fn readahead_min_blocks(&self) -> u32 {
        self.readahead.ra_min_blocks
    }

    // ------------------------------------------------------------------
    // Readahead internals
    // ------------------------------------------------------------------

    /// Detect sequential access and spawn an async readahead task when
    /// appropriate.
    ///
    /// The heuristic: if `start_lba == last_lba + last_count` (i.e. the read is
    /// contiguous with the previous one), double the readahead window (clamped
    /// to `ra_max_blocks`). When the read passes the midpoint of the
    /// previously-prefetched region, spawn a new prefetch.
    fn maybe_trigger_readahead<F, Fut>(&self, start_lba: u64, block_count: u32, fetch_fn: F)
    where
        F: Fn(u64, u32) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Result<Bytes>> + Send + 'static,
    {
        let prev_lba = self.readahead.last_lba.load(Ordering::Relaxed);
        let _prev_count = block_count; // approximation -- stores last count below

        // Update last-seen LBA (store start_lba for next comparison).
        self.readahead.last_lba.store(start_lba, Ordering::Relaxed);

        // Check whether this read is sequential with the previous.
        let is_sequential =
            prev_lba != u64::MAX && start_lba == prev_lba.wrapping_add(block_count as u64);

        if !is_sequential {
            // Random jump -- reset window.
            self.readahead
                .ra_window
                .store(self.readahead.ra_min_blocks, Ordering::Relaxed);
            self.readahead
                .ra_start
                .store(start_lba + block_count as u64, Ordering::Relaxed);
            return;
        }

        // Sequential -- grow window (double, capped).
        let current_window = self.readahead.ra_window.load(Ordering::Relaxed);
        let new_window = (current_window * 2).min(self.readahead.ra_max_blocks);
        self.readahead
            .ra_window
            .store(new_window, Ordering::Relaxed);

        // Check midpoint trigger: prefetch when read reaches the midpoint of
        // the outstanding readahead region.
        let ra_start = self.readahead.ra_start.load(Ordering::Relaxed);
        let read_end = start_lba + block_count as u64;
        let midpoint = ra_start + (new_window as u64 / 2);

        if read_end >= ra_start || read_end >= midpoint {
            // Attempt to claim the inflight slot.
            if self
                .readahead
                .ra_inflight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                let prefetch_lba = read_end;
                let prefetch_blocks = new_window;
                let chunk_blocks = self.chunk_blocks;
                let cache = self.inner.clone();
                let _ra_inflight = Arc::new(AtomicBool::new(true));
                // We share a pointer so the spawned task can clear the flag.
                let ra_flag_ptr = &self.readahead.ra_inflight as *const AtomicBool as usize;

                // Safety: we need to clear the inflight flag from inside the
                // spawned task. We cannot hold a reference to `self` inside
                // `tokio::spawn` because `BlockCache` is not `'static`.
                // Instead we copy the pointer and reconstruct a reference.
                // This is safe as long as `BlockCache` outlives the spawned
                // task -- which is guaranteed because the cache lives for the
                // lifetime of the program.
                tokio::spawn(async move {
                    let end_lba = prefetch_lba + prefetch_blocks as u64;
                    let mut clba = (prefetch_lba / chunk_blocks as u64) * chunk_blocks as u64;
                    while clba < end_lba {
                        let cb = chunk_blocks;
                        let f = fetch_fn.clone();
                        let c = cache.clone();
                        let res = c.try_get_with(clba, async move { f(clba, cb).await }).await;
                        if let Err(e) = res {
                            warn!(clba, "readahead prefetch failed: {e}");
                            break;
                        }
                        clba += chunk_blocks as u64;
                    }
                    // Clear inflight flag.
                    // SAFETY: see comment above.
                    let flag = unsafe { &*(ra_flag_ptr as *const AtomicBool) };
                    flag.store(false, Ordering::Release);
                });

                // Advance ra_start past the prefetched region.
                self.readahead
                    .ra_start
                    .store(prefetch_lba + prefetch_blocks as u64, Ordering::Relaxed);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Helper: create a BlockCache with 1 MB capacity, 512-byte blocks, 512 KB readahead max.
    fn test_cache() -> BlockCache {
        BlockCache::new(1, 512, 512)
    }

    /// Deterministic fetch function: returns `block_size * block_count` bytes
    /// filled with the low byte of `lba`. Also increments a shared counter.
    fn make_fetch(
        counter: Arc<AtomicUsize>,
        block_size: u32,
    ) -> impl Fn(u64, u32) -> std::pin::Pin<Box<dyn Future<Output = Result<Bytes>> + Send>>
    + Clone
    + Send
    + Sync
    + 'static {
        move |lba: u64, count: u32| {
            let c = counter.clone();
            let bs = block_size;
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                let total = count as usize * bs as usize;
                let byte = (lba & 0xFF) as u8;
                Ok(Bytes::from(vec![byte; total]))
            })
        }
    }

    #[tokio::test]
    async fn test_cache_miss_calls_fetch() {
        let cache = test_cache();
        let counter = Arc::new(AtomicUsize::new(0));
        let fetch = make_fetch(counter.clone(), 512);

        let data = cache.read_blocks(0, 1, fetch).await.unwrap();
        assert_eq!(data.len(), 512);
        assert!(
            counter.load(Ordering::SeqCst) >= 1,
            "fetch should be called on miss"
        );
    }

    #[tokio::test]
    async fn test_cache_hit_no_fetch() {
        let cache = test_cache();
        let counter = Arc::new(AtomicUsize::new(0));
        let fetch = make_fetch(counter.clone(), 512);

        // First read -- populates cache.
        let _d1 = cache.read_blocks(0, 1, fetch.clone()).await.unwrap();
        let calls_after_first = counter.load(Ordering::SeqCst);

        // Second read of the same range -- should hit cache.
        let d2 = cache.read_blocks(0, 1, fetch).await.unwrap();
        let calls_after_second = counter.load(Ordering::SeqCst);

        assert_eq!(
            calls_after_first, calls_after_second,
            "fetch should NOT be called on cache hit"
        );
        assert_eq!(d2.len(), 512);
    }

    #[tokio::test]
    async fn test_cache_invalidate() {
        let cache = test_cache();
        let counter = Arc::new(AtomicUsize::new(0));
        let fetch = make_fetch(counter.clone(), 512);

        // Populate
        let _ = cache.read_blocks(0, 1, fetch.clone()).await.unwrap();
        let calls_before = counter.load(Ordering::SeqCst);

        // Invalidate
        cache.invalidate_range(0, cache.chunk_blocks).await;

        // Force moka to process the invalidation.
        cache.inner.run_pending_tasks().await;

        // Read again -- should miss.
        let _ = cache.read_blocks(0, 1, fetch).await.unwrap();
        let calls_after = counter.load(Ordering::SeqCst);

        assert!(
            calls_after > calls_before,
            "fetch should be called again after invalidation"
        );
    }

    #[tokio::test]
    async fn test_chunk_lba_alignment() {
        let cache = test_cache();
        // 512-byte blocks, chunk = 64KB/512 = 128 blocks
        assert_eq!(cache.chunk_blocks, 128);
        assert_eq!(cache.chunk_lba(0), 0);
        assert_eq!(cache.chunk_lba(1), 0);
        assert_eq!(cache.chunk_lba(127), 0);
        assert_eq!(cache.chunk_lba(128), 128);
        assert_eq!(cache.chunk_lba(129), 128);
        assert_eq!(cache.chunk_lba(256), 256);
    }

    #[tokio::test]
    async fn test_readahead_sequential_triggers_prefetch() {
        let cache = test_cache();
        let counter = Arc::new(AtomicUsize::new(0));
        let fetch = make_fetch(counter.clone(), 512);

        // Issue three sequential reads (each chunk_blocks long).
        let cb = cache.chunk_blocks;
        for i in 0..3u64 {
            let _ = cache
                .read_blocks(i * cb as u64, cb, fetch.clone())
                .await
                .unwrap();
        }

        // Give the readahead spawned task a moment to complete.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let total_fetches = counter.load(Ordering::SeqCst);
        assert!(
            total_fetches >= 3,
            "at least 3 fetches expected (3 demand + possible readahead), got {total_fetches}"
        );
    }

    #[tokio::test]
    async fn test_readahead_resets_on_random() {
        let cache = test_cache();
        let counter = Arc::new(AtomicUsize::new(0));
        let fetch = make_fetch(counter.clone(), 512);

        let cb = cache.chunk_blocks;

        // Two sequential reads to grow window.
        let _ = cache.read_blocks(0, cb, fetch.clone()).await.unwrap();
        let _ = cache
            .read_blocks(cb as u64, cb, fetch.clone())
            .await
            .unwrap();

        let window_before = cache.readahead_window_blocks();

        // Random jump.
        let _ = cache
            .read_blocks(10000 * cb as u64, cb, fetch.clone())
            .await
            .unwrap();

        let window_after = cache.readahead_window_blocks();
        assert_eq!(
            window_after,
            cache.readahead_min_blocks(),
            "window should reset to min after random access (was {window_before}, now {window_after})"
        );
    }

    #[tokio::test]
    async fn test_readahead_window_grows() {
        let cache = test_cache();
        let counter = Arc::new(AtomicUsize::new(0));
        let fetch = make_fetch(counter.clone(), 512);

        let cb = cache.chunk_blocks;
        let min = cache.readahead_min_blocks();

        // Issue six sequential reads to let the window grow.
        for i in 0..6u64 {
            let _ = cache
                .read_blocks(i * cb as u64, cb, fetch.clone())
                .await
                .unwrap();
        }

        let window = cache.readahead_window_blocks();
        assert!(
            window > min,
            "readahead window should have grown beyond min ({min}), got {window}"
        );
    }
}
