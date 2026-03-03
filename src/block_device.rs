use std::sync::{Arc, Mutex};

use fuser::Errno;
use tokio::runtime::Handle;
use tracing::{debug, error};

use crate::cache::BlockCache;
use crate::iscsi_backend::IscsiBackend;

/// Translates arbitrary byte-offset I/O into block-aligned SCSI commands.
pub struct BlockDevice {
    backend: Arc<IscsiBackend>,
    cache: BlockCache,
    rt_handle: Handle,
    block_size: u32,
    total_bytes: u64,
    /// Mutex to serialize unaligned writes (prevents RMW races on the same block).
    write_lock: Mutex<()>,
}

impl BlockDevice {
    pub fn new(backend: Arc<IscsiBackend>, cache: BlockCache, rt_handle: Handle) -> Self {
        let block_size = backend.block_size();
        let total_bytes = backend.total_bytes();
        Self {
            backend,
            cache,
            rt_handle,
            block_size,
            total_bytes,
            write_lock: Mutex::new(()),
        }
    }

    /// Read `size` bytes starting at `offset`. Handles block alignment.
    pub fn read_bytes(&self, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        if size == 0 || offset >= self.total_bytes {
            return Ok(Vec::new());
        }

        // Clamp to device boundary
        let actual_size = size.min((self.total_bytes - offset) as u32);
        let bs = self.block_size as u64;

        let start_lba = offset / bs;
        let end_byte = offset + actual_size as u64;
        let end_lba = end_byte.div_ceil(bs);
        let block_count = (end_lba - start_lba) as u32;

        let skip = (offset % bs) as usize;

        let data = self.read_blocks_with_cache(start_lba, block_count)?;

        // Slice to requested range
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
            return Err(Errno::EIO);
        }

        Ok(data[skip..end].to_vec())
    }

    /// Write `data` starting at `offset`. Handles read-modify-write for unaligned boundaries.
    pub fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        if data.is_empty() || offset >= self.total_bytes {
            return Ok(0);
        }

        // Clamp to device boundary
        let actual_len = data.len().min((self.total_bytes - offset) as usize);
        let data = &data[..actual_len];

        let bs = self.block_size as u64;
        let start_lba = offset / bs;
        let end_byte = offset + actual_len as u64;
        let end_lba = end_byte.div_ceil(bs);
        let block_count = (end_lba - start_lba) as u32;

        let skip = (offset % bs) as usize;
        let aligned = skip == 0 && actual_len.is_multiple_of(bs as usize);

        // Serialize writes to prevent RMW races
        let _lock = self.write_lock.lock().unwrap();

        let write_data = if aligned {
            data.to_vec()
        } else {
            // Read-modify-write: read existing blocks, overlay new data, write back
            let existing = self.read_blocks_direct(start_lba, block_count)?;
            let mut buf = existing;
            let end_in_buf = skip + actual_len;
            if end_in_buf > buf.len() {
                return Err(Errno::EIO);
            }
            buf[skip..end_in_buf].copy_from_slice(data);
            buf
        };

        // Write to iSCSI target
        self.rt_handle
            .block_on(self.backend.scsi_write(start_lba, write_data))
            .map_err(|e| {
                error!("SCSI WRITE failed: {e}");
                Errno::EIO
            })?;

        // Invalidate cache for written blocks
        self.cache.invalidate_range(start_lba, block_count as u64);

        debug!(offset, len = actual_len, "Write completed");
        Ok(actual_len as u32)
    }

    /// Read blocks, using cache where possible.
    fn read_blocks_with_cache(&self, start_lba: u64, block_count: u32) -> Result<Vec<u8>, Errno> {
        let bs = self.block_size as usize;
        let mut result = Vec::with_capacity(block_count as usize * bs);

        let mut i = 0u32;
        while i < block_count {
            let lba = start_lba + i as u64;
            if let Some(cached) = self.cache.get(lba) {
                result.extend_from_slice(&cached);
                i += 1;
            } else {
                // Find contiguous miss run
                let miss_start = i;
                while i < block_count && self.cache.get(start_lba + i as u64).is_none() {
                    i += 1;
                }
                let miss_count = i - miss_start;
                let miss_lba = start_lba + miss_start as u64;

                // Read the entire miss run in one SCSI READ
                let data = self.read_blocks_direct(miss_lba, miss_count)?;

                // Populate cache with individual blocks
                for j in 0..miss_count {
                    let block_start = j as usize * bs;
                    let block_end = block_start + bs;
                    if block_end <= data.len() {
                        self.cache
                            .put(miss_lba + j as u64, data[block_start..block_end].to_vec());
                    }
                }

                result.extend_from_slice(&data);
            }
        }

        Ok(result)
    }

    /// Read blocks directly from iSCSI (no cache).
    fn read_blocks_direct(&self, start_lba: u64, block_count: u32) -> Result<Vec<u8>, Errno> {
        self.rt_handle
            .block_on(self.backend.scsi_read(start_lba, block_count))
            .map_err(|e| {
                error!("SCSI READ failed: {e}");
                Errno::EIO
            })
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }
}
