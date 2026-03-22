pub mod cache;

use std::collections::BTreeMap;
use std::io::{Error as IoError, ErrorKind};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Interval, interval};
use tracing::{debug, error, warn};

use crate::block::cache::BlockCache;
use crate::iscsi::Pipeline;

/// Helper to create an I/O error (replaces fuser::Errno::EIO).
fn io_error(msg: &str) -> IoError {
    IoError::new(ErrorKind::Other, msg)
}

// ---------------------------------------------------------------------------
// BlockRequest -- the message type sent from FUSE threads to the async worker.
// ---------------------------------------------------------------------------

enum BlockRequest {
    Read {
        offset: u64,
        size: u32,
        reply: oneshot::Sender<Result<Bytes, IoError>>,
    },
    Write {
        offset: u64,
        data: Bytes,
        reply: oneshot::Sender<Result<u32, IoError>>,
    },
    Flush {
        reply: oneshot::Sender<Result<(), IoError>>,
    },
    SetSyncWrites {
        enabled: bool,
        reply: oneshot::Sender<()>,
    },
}

// ---------------------------------------------------------------------------
// BlockDevice -- the FUSE-facing handle (Clone + Send + Sync).
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BlockDevice {
    tx: mpsc::Sender<BlockRequest>,
    rt: Handle,
    block_size: u32,
    total_bytes: u64,
}

impl BlockDevice {
    /// Create the mpsc channel, spawn the [`BlockDeviceWorker`], and return
    /// the cloneable handle that FUSE threads use.
    pub fn spawn(
        pipeline: Arc<Pipeline>,
        cache: BlockCache,
        block_size: u32,
        total_bytes: u64,
        coalesce_timeout: Duration,
        coalesce_max_bytes: usize,
        sync_writes: bool,
    ) -> Self {
        let (tx, rx) = mpsc::channel(256);

        let worker = BlockDeviceWorker {
            pipeline,
            cache,
            block_size,
            total_bytes,
            dirty: DirtyMap::new(),
            coalesce_timeout,
            coalesce_max_bytes,
            sync_writes,
        };

        let rt = Handle::current();
        tokio::spawn(worker.run(rx));

        Self {
            tx,
            rt,
            block_size,
            total_bytes,
        }
    }

    /// Read `size` bytes starting at `offset`.
    /// Called from a synchronous FUSE thread — uses rt.block_on to bridge sync↔async.
    pub fn read_bytes(&self, offset: u64, size: u32) -> Result<Bytes, IoError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let tx = self.tx.clone();
        self.rt.block_on(async {
            tx.send(BlockRequest::Read {
                offset,
                size,
                reply: reply_tx,
            })
            .await
            .map_err(|_| io_error("block device channel closed"))?;
            reply_rx.await.map_err(|_| io_error("block device reply dropped"))?
        })
    }

    /// Write `data` starting at `offset`.
    /// Called from a synchronous FUSE thread.
    pub fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<u32, IoError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let tx = self.tx.clone();
        let data = Bytes::copy_from_slice(data);
        self.rt.block_on(async {
            tx.send(BlockRequest::Write {
                offset,
                data,
                reply: reply_tx,
            })
            .await
            .map_err(|_| io_error("block device channel closed"))?;
            reply_rx.await.map_err(|_| io_error("block device reply dropped"))?
        })
    }

    /// Flush all pending dirty writes to the target.
    pub fn flush(&self) -> Result<(), IoError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let tx = self.tx.clone();
        self.rt.block_on(async {
            tx.send(BlockRequest::Flush { reply: reply_tx })
                .await
                .map_err(|_| io_error("block device channel closed"))?;
            reply_rx.await.map_err(|_| io_error("block device reply dropped"))?
        })
    }

    /// Switch sync-write mode at runtime. When enabled, every write is
    /// flushed to iSCSI immediately instead of coalescing.
    pub fn set_sync_writes(&self, enabled: bool) {
        let (reply_tx, reply_rx) = oneshot::channel();
        let tx = self.tx.clone();
        self.rt.block_on(async {
            tx.send(BlockRequest::SetSyncWrites {
                enabled,
                reply: reply_tx,
            })
            .await
            .ok();
            reply_rx.await.ok();
        });
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }
}

// ---------------------------------------------------------------------------
// DirtyMap -- BTreeMap-based write coalescing buffer.
// ---------------------------------------------------------------------------

struct DirtyEntry {
    data: Bytes,
    block_count: u32,
}

struct DirtyMap {
    entries: BTreeMap<u64, DirtyEntry>,
    total_bytes: usize,
}

impl DirtyMap {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            total_bytes: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert (or overwrite) a dirty entry for the given LBA range.
    fn insert(&mut self, start_lba: u64, block_count: u32, data: Bytes) {
        let len = data.len();
        if let Some(old) = self
            .entries
            .insert(start_lba, DirtyEntry { data, block_count })
        {
            self.total_bytes -= old.data.len();
        }
        self.total_bytes += len;
    }

    /// Drain all entries in LBA order. Resets total_bytes to 0.
    fn drain_sorted(&mut self) -> Vec<(u64, Bytes)> {
        let items: Vec<(u64, Bytes)> = self
            .entries
            .iter()
            .map(|(&lba, entry)| (lba, entry.data.clone()))
            .collect();
        self.entries.clear();
        self.total_bytes = 0;
        items
    }

    /// If the read range `[start_lba .. start_lba + block_count)` is fully
    /// contained within a single dirty entry, return that data. This provides
    /// read-your-writes semantics.
    fn read_overlap(&self, start_lba: u64, block_count: u32, block_size: u32) -> Option<Bytes> {
        // Find the entry whose start_lba is <= our start_lba.
        // BTreeMap::range(..=start_lba).last() gives the highest key <= start_lba.
        let (&entry_lba, entry) = self.entries.range(..=start_lba).next_back()?;

        let entry_end = entry_lba + entry.block_count as u64;
        let read_end = start_lba + block_count as u64;

        if start_lba >= entry_lba && read_end <= entry_end {
            let bs = block_size as usize;
            let offset_blocks = (start_lba - entry_lba) as usize;
            let byte_offset = offset_blocks * bs;
            let byte_len = block_count as usize * bs;
            let end = byte_offset + byte_len;
            if end <= entry.data.len() {
                return Some(entry.data.slice(byte_offset..end));
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// BlockDeviceWorker -- the async task that services BlockRequests.
// ---------------------------------------------------------------------------

struct BlockDeviceWorker {
    pipeline: Arc<Pipeline>,
    cache: BlockCache,
    block_size: u32,
    total_bytes: u64,
    dirty: DirtyMap,
    coalesce_timeout: Duration,
    coalesce_max_bytes: usize,
    /// When true, each write is flushed to iSCSI immediately (no coalescing).
    sync_writes: bool,
}

impl BlockDeviceWorker {
    async fn run(mut self, mut rx: mpsc::Receiver<BlockRequest>) {
        let mut coalesce_timer: Interval = interval(self.coalesce_timeout);
        // The first tick completes immediately -- consume it.
        coalesce_timer.tick().await;

        loop {
            tokio::select! {
                biased;

                msg = rx.recv() => {
                    match msg {
                        Some(BlockRequest::Read { offset, size, reply }) => {
                            let result = self.handle_read(offset, size).await;
                            let _ = reply.send(result);
                        }
                        Some(BlockRequest::Write { offset, data, reply }) => {
                            let result = self.handle_write(offset, data).await;
                            let _ = reply.send(result);
                        }
                        Some(BlockRequest::Flush { reply }) => {
                            let result = self.flush_dirty().await;
                            let _ = reply.send(result);
                        }
                        Some(BlockRequest::SetSyncWrites { enabled, reply }) => {
                            self.sync_writes = enabled;
                            debug!(sync_writes = enabled, "Sync-write mode changed");
                            let _ = reply.send(());
                        }
                        None => {
                            // Channel closed -- flush remaining dirty data and exit.
                            debug!("BlockDevice channel closed, flushing remaining dirty data");
                            if let Err(e) = self.flush_dirty().await {
                                error!("Final flush failed: {e:?}");
                            }
                            return;
                        }
                    }
                }

                _ = coalesce_timer.tick() => {
                    if !self.dirty.is_empty() {
                        debug!("Coalesce timer fired, flushing dirty data");
                        if let Err(e) = self.flush_dirty().await {
                            warn!("Periodic flush failed: {e:?}");
                        }
                    }
                }
            }
        }
    }

    // -- Read -----------------------------------------------------------------

    async fn handle_read(&mut self, offset: u64, size: u32) -> Result<Bytes, IoError> {
        if size == 0 || offset >= self.total_bytes {
            return Ok(Bytes::new());
        }

        // Clamp to device boundary.
        let actual_size = size.min((self.total_bytes - offset) as u32);
        let (start_lba, block_count, skip) =
            compute_alignment(offset, actual_size as u64, self.block_size);

        // 1. Check dirty map first (read-your-writes).
        if let Some(dirty_data) = self
            .dirty
            .read_overlap(start_lba, block_count, self.block_size)
        {
            let end = skip + actual_size as usize;
            if end <= dirty_data.len() {
                return Ok(dirty_data.slice(skip..end));
            }
        }

        // 2. Cache-aware read via the pipeline.
        let data = self.read_blocks_with_cache(start_lba, block_count).await?;

        let end = skip + actual_size as usize;
        if end > data.len() {
            error!(
                offset,
                size,
                data_len = data.len(),
                skip,
                end,
                "Read returned less data than expected"
            );
            return Err(io_error("SCSI read returned less data than expected"));
        }

        Ok(data.slice(skip..end))
    }

    /// Read blocks, consulting the LRU cache first and fetching misses from
    /// the pipeline.
    async fn read_blocks_with_cache(
        &self,
        start_lba: u64,
        block_count: u32,
    ) -> Result<Bytes, IoError> {
        let pipeline = self.pipeline.clone();
        self.cache
            .read_blocks(start_lba, block_count, move |lba, count| {
                let p = pipeline.clone();
                async move { p.scsi_read(lba, count).await }
            })
            .await
            .map_err(|e| {
                error!("SCSI READ failed: {e}");
                io_error("SCSI read failed")
            })
    }

    // -- Write ----------------------------------------------------------------

    async fn handle_write(&mut self, offset: u64, data: Bytes) -> Result<u32, IoError> {
        if data.is_empty() || offset >= self.total_bytes {
            return Ok(0);
        }

        // Clamp to device boundary.
        let actual_len = data.len().min((self.total_bytes - offset) as usize);
        let data = data.slice(..actual_len);

        let bs = self.block_size as u64;
        let (start_lba, block_count, skip) =
            compute_alignment(offset, actual_len as u64, self.block_size);
        let aligned = skip == 0 && (actual_len as u64).is_multiple_of(bs);

        let write_data = if aligned {
            data
        } else {
            // Read-modify-write: read existing blocks, overlay new data, write back.
            let existing = self
                .pipeline
                .scsi_read(start_lba, block_count)
                .await
                .map_err(|e| {
                    error!("SCSI READ (RMW) failed: {e}");
                    io_error("SCSI read failed during read-modify-write")
                })?;
            let mut buf = existing.to_vec();
            let end_in_buf = skip + actual_len;
            if end_in_buf > buf.len() {
                return Err(io_error("SCSI read-modify-write buffer overflow"));
            }
            buf[skip..end_in_buf].copy_from_slice(&data);
            Bytes::from(buf)
        };

        // Insert into dirty map.
        self.dirty.insert(start_lba, block_count, write_data);

        // Invalidate cache for written blocks.
        self.cache.invalidate_range(start_lba, block_count).await;

        // Flush immediately if sync mode or threshold exceeded.
        if self.sync_writes || self.dirty.total_bytes >= self.coalesce_max_bytes {
            debug!(
                sync = self.sync_writes,
                total_dirty = self.dirty.total_bytes,
                "Flushing dirty data"
            );
            self.flush_dirty().await?;
        }

        debug!(offset, len = actual_len, "Write completed");
        Ok(actual_len as u32)
    }

    // -- Flush ----------------------------------------------------------------

    async fn flush_dirty(&mut self) -> Result<(), IoError> {
        if self.dirty.is_empty() {
            return Ok(());
        }

        let entries = self.dirty.drain_sorted();
        debug!(count = entries.len(), "Flushing dirty entries");

        // Submit all dirty entries concurrently.
        let mut handles = Vec::with_capacity(entries.len());
        for (lba, data) in entries {
            let pipeline = self.pipeline.clone();
            handles.push(tokio::spawn(
                async move { pipeline.scsi_write(lba, data).await },
            ));
        }

        // Await all.
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    error!("SCSI WRITE (flush) failed: {e}");
                    return Err(io_error("SCSI write failed during flush"));
                }
                Err(e) => {
                    error!("Flush task panicked: {e}");
                    return Err(io_error("flush task panicked"));
                }
            }
        }

        // Flush target's volatile write cache to persistent storage.
        if let Err(e) = self.pipeline.scsi_synchronize_cache().await {
            error!("SYNCHRONIZE CACHE failed: {e}");
            return Err(io_error("SYNCHRONIZE CACHE failed"));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Alignment helper
// ---------------------------------------------------------------------------

/// Compute block-aligned LBA range and byte skip for a given byte offset/size.
pub fn compute_alignment(offset: u64, size: u64, block_size: u32) -> (u64, u32, usize) {
    let bs = block_size as u64;
    let start_lba = offset / bs;
    let end_lba = (offset + size).div_ceil(bs);
    let block_count = (end_lba - start_lba) as u32;
    let skip = (offset % bs) as usize;
    (start_lba, block_count, skip)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dirty_map_insert_and_drain() {
        let mut dm = DirtyMap::new();
        dm.insert(10, 2, Bytes::from(vec![0xAA; 8192]));
        dm.insert(5, 1, Bytes::from(vec![0xBB; 4096]));

        assert_eq!(dm.total_bytes, 8192 + 4096);

        let drained = dm.drain_sorted();
        assert_eq!(drained.len(), 2);
        // Sorted by LBA: 5 comes before 10.
        assert_eq!(drained[0].0, 5);
        assert_eq!(drained[1].0, 10);
        assert_eq!(dm.total_bytes, 0);
        assert!(dm.is_empty());
    }

    #[test]
    fn test_dirty_map_read_overlap_full() {
        let mut dm = DirtyMap::new();
        let block_size = 512u32;
        // Insert 4 blocks starting at LBA 10.
        let data = Bytes::from(vec![0xCC; 4 * 512]);
        dm.insert(10, 4, data.clone());

        // Read LBA 11..13 (2 blocks), fully within the dirty entry.
        let result = dm.read_overlap(11, 2, block_size);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.len(), 2 * 512);
        // Should be bytes from offset 512..1536 of the original data.
        assert_eq!(&result[..], &data[512..512 + 2 * 512]);
    }

    #[test]
    fn test_dirty_map_read_no_overlap() {
        let mut dm = DirtyMap::new();
        let block_size = 512u32;
        dm.insert(0, 2, Bytes::from(vec![0xDD; 1024]));

        // Read at LBA 100 -- no overlap.
        let result = dm.read_overlap(100, 1, block_size);
        assert!(result.is_none());
    }

    #[test]
    fn test_block_device_alignment() {
        // offset=100, size=200, bs=4096 => lba=0, count=1, skip=100
        let (lba, count, skip) = compute_alignment(100, 200, 4096);
        assert_eq!(lba, 0);
        assert_eq!(count, 1);
        assert_eq!(skip, 100);
    }

    #[test]
    fn test_block_device_alignment_spanning() {
        // offset=4000, size=200, bs=4096 => lba=0, count=2, skip=4000
        let (lba, count, skip) = compute_alignment(4000, 200, 4096);
        assert_eq!(lba, 0);
        assert_eq!(count, 2);
        assert_eq!(skip, 4000);
    }
}
