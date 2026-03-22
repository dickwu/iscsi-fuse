#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use tracing::{debug, warn};

use crate::iscsi::command::{self, ScsiStatus};
use crate::iscsi::login::NegotiatedParams;
use crate::iscsi::session::{PduResponse, Session};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for individual SCSI read operations (single command).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for individual SCSI write operations (includes R2T round-trips).
const WRITE_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Command pipeline that splits large I/O requests into max_burst_length-sized
/// chunks and submits them concurrently through the session's 128-deep ITT pool.
pub struct Pipeline {
    session: Arc<Session>,
    lun: u64,
    block_size: u32,
    total_blocks: u64,
    negotiated: NegotiatedParams,
}

impl Pipeline {
    /// Create a new pipeline. Call `set_geometry` after `read_capacity`.
    pub fn new(session: Arc<Session>, lun: u64, negotiated: NegotiatedParams) -> Self {
        Self {
            session,
            lun,
            block_size: 0,
            total_blocks: 0,
            negotiated,
        }
    }

    /// Set the device geometry after a successful READ CAPACITY.
    pub fn set_geometry(&mut self, block_size: u32, total_blocks: u64) {
        self.block_size = block_size;
        self.total_blocks = total_blocks;
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_blocks * self.block_size as u64
    }

    /// Maximum number of blocks that fit within one burst for reads.
    pub fn max_read_blocks(&self) -> u32 {
        max_read_blocks_for(self.negotiated.max_burst_length, self.block_size)
    }

    /// Maximum number of blocks that fit within one burst for writes.
    pub fn max_write_blocks(&self) -> u32 {
        max_read_blocks_for(self.negotiated.max_burst_length, self.block_size)
    }

    // -----------------------------------------------------------------------
    // SCSI READ
    // -----------------------------------------------------------------------

    /// Issue a SCSI READ, automatically chunking and pipelining large requests.
    ///
    /// Chunks are submitted concurrently through the session's ITT pool and
    /// awaited in order to preserve correct byte ordering.
    pub async fn scsi_read(&self, lba: u64, block_count: u32) -> Result<Bytes> {
        if block_count == 0 {
            return Ok(Bytes::new());
        }

        let max_blocks = self.max_read_blocks();
        let chunks = compute_read_chunks(lba, block_count, max_blocks);

        if chunks.len() == 1 {
            return self.scsi_read_single(chunks[0].0, chunks[0].1).await;
        }

        // Spawn all chunk reads concurrently.
        let mut handles = Vec::with_capacity(chunks.len());
        for (chunk_lba, chunk_blocks) in chunks {
            let session = Arc::clone(&self.session);
            let lun = self.lun;
            let block_size = self.block_size;
            handles.push(tokio::spawn(async move {
                scsi_read_single_inner(&session, lun, block_size, chunk_lba, chunk_blocks).await
            }));
        }

        // Await in order and concatenate.
        let total_bytes = block_count as usize * self.block_size as usize;
        let mut result = BytesMut::with_capacity(total_bytes);
        for handle in handles {
            let chunk_data = handle
                .await
                .context("read chunk task panicked")?
                .context("read chunk failed")?;
            result.extend_from_slice(&chunk_data);
        }

        Ok(result.freeze())
    }

    /// Issue a single SCSI READ command (no chunking).
    async fn scsi_read_single(&self, lba: u64, block_count: u32) -> Result<Bytes> {
        scsi_read_single_inner(&self.session, self.lun, self.block_size, lba, block_count).await
    }

    // -----------------------------------------------------------------------
    // SCSI WRITE
    // -----------------------------------------------------------------------

    /// Issue a SCSI WRITE, automatically chunking and pipelining large requests.
    ///
    /// Uses zero-copy `Bytes::slice` to split the input data into chunks.
    pub async fn scsi_write(&self, lba: u64, data: Bytes) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let max_blocks = self.max_write_blocks();
        let block_count = data.len() as u32 / self.block_size;
        let chunks = compute_write_chunks(lba, block_count, self.block_size, max_blocks);

        if chunks.len() == 1 {
            return self.scsi_write_single(lba, block_count, data).await;
        }

        // Spawn all chunk writes concurrently.
        let mut handles = Vec::with_capacity(chunks.len());
        let mut offset = 0usize;
        for (chunk_lba, chunk_blocks) in chunks {
            let chunk_bytes = chunk_blocks as usize * self.block_size as usize;
            let chunk_data = data.slice(offset..offset + chunk_bytes);
            offset += chunk_bytes;

            let session = Arc::clone(&self.session);
            let lun = self.lun;
            let block_size = self.block_size;
            let negotiated_immediate = self.negotiated.immediate_data;
            let first_burst = self.negotiated.first_burst_length;
            handles.push(tokio::spawn(async move {
                scsi_write_single_inner(
                    &session,
                    lun,
                    block_size,
                    chunk_lba,
                    chunk_blocks,
                    chunk_data,
                    negotiated_immediate,
                    first_burst,
                )
                .await
            }));
        }

        // Await all in order.
        for handle in handles {
            handle
                .await
                .context("write chunk task panicked")?
                .context("write chunk failed")?;
        }

        Ok(())
    }

    /// Issue a single SCSI WRITE command (no chunking).
    async fn scsi_write_single(&self, lba: u64, block_count: u32, data: Bytes) -> Result<()> {
        scsi_write_single_inner(
            &self.session,
            self.lun,
            self.block_size,
            lba,
            block_count,
            data,
            self.negotiated.immediate_data,
            self.negotiated.first_burst_length,
        )
        .await
    }

    // -----------------------------------------------------------------------
    // SYNCHRONIZE CACHE
    // -----------------------------------------------------------------------

    /// Issue a SCSI SYNCHRONIZE CACHE (10) command to flush the target's
    /// volatile write cache to persistent storage.
    ///
    /// This MUST be called after flushing dirty writes to ensure data
    /// survives target power loss or session disconnect.
    pub async fn scsi_synchronize_cache(&self) -> Result<()> {
        let cdb = command::build_synchronize_cache10(0, 0); // flush entire cache

        debug!("SCSI SYNCHRONIZE CACHE");

        let (_, rx) = self
            .session
            .submit_command(&cdb, self.lun, 0, false, false, None)
            .await
            .context("submit SYNCHRONIZE CACHE")?;

        let response = tokio::time::timeout(WRITE_TIMEOUT, rx)
            .await
            .context("SYNCHRONIZE CACHE timed out")?
            .context("SYNCHRONIZE CACHE channel closed")?;

        check_scsi_status("SYNCHRONIZE CACHE", &response)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // READ CAPACITY
    // -----------------------------------------------------------------------

    /// Query device capacity. Tries READ CAPACITY(10) first, upgrading to
    /// READ CAPACITY(16) if the device is >= 2 TiB. Retries on UNIT ATTENTION.
    ///
    /// Returns `(total_blocks, block_size)`.
    pub async fn read_capacity(&self) -> Result<(u64, u32)> {
        let mut last_err = anyhow::anyhow!("no attempts");
        let mut rc10_overflow: Option<(u32, u32)> = None;

        for attempt in 0..3u32 {
            match self.read_capacity10().await {
                Ok((max_lba, block_len)) => {
                    if max_lba != 0xFFFF_FFFF {
                        // Fits in 32-bit LBA space (device < 2 TiB).
                        return Ok((max_lba as u64 + 1, block_len));
                    }
                    // Device is >= 2 TiB -- need READ CAPACITY(16).
                    rc10_overflow = Some((max_lba, block_len));
                    match self.read_capacity16().await {
                        Ok(result) => return Ok(result),
                        Err(e) => {
                            warn!("READ CAPACITY(16) attempt {}: {e}", attempt + 1);
                            last_err = e;
                        }
                    }
                }
                Err(e) => {
                    warn!("READ CAPACITY(10) attempt {}: {e}", attempt + 1);
                    last_err = e;
                }
            }
        }

        // RC(16) failed but RC(10) reported >= 2 TiB.
        if let Some((max_lba, block_len)) = rc10_overflow {
            warn!(
                "READ CAPACITY(16) unavailable; falling back to READ CAPACITY(10) \
                 result -- device reported as exactly 2 TiB"
            );
            return Ok((max_lba as u64 + 1, block_len));
        }

        Err(last_err.context("READ CAPACITY failed after retries"))
    }

    /// Issue READ CAPACITY(10). Returns `(max_lba, block_len)`.
    async fn read_capacity10(&self) -> Result<(u32, u32)> {
        let cdb = command::build_read_capacity10();
        let (_itt, rx) = self
            .session
            .submit_command(&cdb, self.lun, 8, true, false, None)
            .await
            .context("submit READ CAPACITY(10)")?;

        let response = tokio::time::timeout(READ_TIMEOUT, rx)
            .await
            .context("READ CAPACITY(10) timed out")?
            .context("READ CAPACITY(10) channel closed")?;

        check_scsi_status("READ CAPACITY(10)", &response)?;

        let data = response
            .data
            .as_ref()
            .context("READ CAPACITY(10) returned no data")?;

        command::parse_read_capacity10(data)
    }

    /// Issue READ CAPACITY(16). Returns `(total_blocks, block_len)`.
    async fn read_capacity16(&self) -> Result<(u64, u32)> {
        let alloc_len: u32 = 32;
        let cdb = command::build_read_capacity16(alloc_len);
        let (_itt, rx) = self
            .session
            .submit_command(&cdb, self.lun, alloc_len, true, false, None)
            .await
            .context("submit READ CAPACITY(16)")?;

        let response = tokio::time::timeout(READ_TIMEOUT, rx)
            .await
            .context("READ CAPACITY(16) timed out")?
            .context("READ CAPACITY(16) channel closed")?;

        check_scsi_status("READ CAPACITY(16)", &response)?;

        let data = response
            .data
            .as_ref()
            .context("READ CAPACITY(16) returned no data")?;

        let (max_lba, block_len) = command::parse_read_capacity16(data)?;

        let total_blocks = max_lba
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("READ CAPACITY(16) max_lba overflow"))?;

        Ok((total_blocks, block_len))
    }

    // -----------------------------------------------------------------------
    // Logout
    // -----------------------------------------------------------------------

    /// Send a Logout Request PDU to the target.
    pub async fn logout(&self) -> Result<()> {
        self.session.send_logout().await
    }
}

// ---------------------------------------------------------------------------
// Inner helper functions (extracted so they can be used from spawned tasks)
// ---------------------------------------------------------------------------

/// Execute a single SCSI READ command and return the data.
///
/// Separated from `Pipeline` so it can be called from a `tokio::spawn` closure
/// that only captures `Arc<Session>` and scalar parameters.
async fn scsi_read_single_inner(
    session: &Session,
    lun: u64,
    block_size: u32,
    lba: u64,
    block_count: u32,
) -> Result<Bytes> {
    let cdb = command::build_read(lba, block_count);
    let edtl = block_count * block_size;

    debug!(lba, block_count, edtl, "SCSI READ");

    let (_itt, rx) = session
        .submit_command(&cdb, lun, edtl, true, false, None)
        .await
        .context("submit SCSI READ")?;

    let response = tokio::time::timeout(READ_TIMEOUT, rx)
        .await
        .context("SCSI READ timed out")?
        .context("SCSI READ channel closed")?;

    match response.status {
        ScsiStatus::Good => {
            let data = response.data.unwrap_or_else(Bytes::new);
            Ok(data)
        }
        ScsiStatus::CheckCondition => {
            // Check for UNIT ATTENTION and retry once.
            if let Some(ref sense_bytes) = response.sense
                && let Ok(sense) = command::parse_sense_data(sense_bytes)
                && command::is_unit_attention(&sense)
            {
                warn!(lba, block_count, "UNIT ATTENTION on read, retrying");
                let (_itt2, rx2) = session
                    .submit_command(&cdb, lun, edtl, true, false, None)
                    .await
                    .context("retry SCSI READ after UNIT ATTENTION")?;

                let response2 = tokio::time::timeout(READ_TIMEOUT, rx2)
                    .await
                    .context("SCSI READ retry timed out")?
                    .context("SCSI READ retry channel closed")?;

                check_scsi_status("SCSI READ retry", &response2)?;
                return Ok(response2.data.unwrap_or_else(Bytes::new));
            }
            bail!(
                "SCSI READ at LBA {lba} failed: CheckCondition (sense: {:?})",
                response.sense.as_deref()
            );
        }
        other => {
            bail!("SCSI READ at LBA {lba} failed: {other:?}");
        }
    }
}

/// Execute a single SCSI WRITE command.
#[allow(clippy::too_many_arguments)]
async fn scsi_write_single_inner(
    session: &Session,
    lun: u64,
    block_size: u32,
    lba: u64,
    block_count: u32,
    data: Bytes,
    immediate_data_enabled: bool,
    first_burst_length: u32,
) -> Result<()> {
    let cdb = command::build_write(lba, block_count);
    let edtl = block_count * block_size;

    debug!(lba, block_count, edtl, "SCSI WRITE");

    // Compute immediate data: the first burst that can be sent with the command PDU.
    let immediate_data = if immediate_data_enabled && !data.is_empty() {
        let imm_len = (data.len() as u32).min(first_burst_length) as usize;
        if imm_len > 0 {
            Some(data.slice(..imm_len))
        } else {
            None
        }
    } else {
        None
    };

    let (itt, rx) = session
        .submit_command(&cdb, lun, edtl, false, true, immediate_data)
        .await
        .context("submit SCSI WRITE")?;

    // Register the full write data for R2T handling by the receiver task.
    session.itt_pool.register_write_data(itt, data);

    let response = tokio::time::timeout(WRITE_TIMEOUT, rx)
        .await
        .context("SCSI WRITE timed out")?
        .context("SCSI WRITE channel closed")?;

    check_scsi_status("SCSI WRITE", &response)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Status checking helper
// ---------------------------------------------------------------------------

/// Check a PDU response status and bail on non-Good status.
fn check_scsi_status(op: &str, response: &PduResponse) -> Result<()> {
    match response.status {
        ScsiStatus::Good => Ok(()),
        ScsiStatus::CheckCondition => {
            if let Some(ref sense_bytes) = response.sense
                && let Ok(sense) = command::parse_sense_data(sense_bytes)
            {
                bail!(
                    "{op}: CheckCondition, sense_key={:?} ASC={:#04x} ASCQ={:#04x}",
                    sense.sense_key,
                    sense.asc,
                    sense.ascq
                );
            }
            bail!("{op}: CheckCondition (no parseable sense data)");
        }
        other => {
            bail!("{op}: failed with status {other:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helper functions (for testing)
// ---------------------------------------------------------------------------

/// Split a read request into max_blocks-sized chunks.
///
/// Returns a vec of `(lba, block_count)` pairs.
pub fn compute_read_chunks(start_lba: u64, block_count: u32, max_blocks: u32) -> Vec<(u64, u32)> {
    let mut chunks = Vec::new();
    let mut remaining = block_count;
    let mut current_lba = start_lba;

    while remaining > 0 {
        let chunk = remaining.min(max_blocks);
        chunks.push((current_lba, chunk));
        current_lba += chunk as u64;
        remaining -= chunk;
    }

    chunks
}

/// Split a write request into max_blocks-sized chunks.
///
/// Returns a vec of `(lba, block_count)` pairs.
pub fn compute_write_chunks(
    start_lba: u64,
    block_count: u32,
    _block_size: u32,
    max_blocks: u32,
) -> Vec<(u64, u32)> {
    compute_read_chunks(start_lba, block_count, max_blocks)
}

/// Compute the maximum number of blocks that fit within a given burst length.
pub fn max_read_blocks_for(max_burst_length: u32, block_size: u32) -> u32 {
    if block_size == 0 {
        return 0;
    }
    max_burst_length / block_size
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_read_small() {
        // 10 blocks with max 256 should produce a single chunk.
        let chunks = compute_read_chunks(0, 10, 256);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], (0, 10));
    }

    #[test]
    fn test_chunk_read_large() {
        // 1000 blocks with max 256 should produce 4 chunks:
        // 256 + 256 + 256 + 232 = 1000
        let chunks = compute_read_chunks(0, 1000, 256);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0], (0, 256));
        assert_eq!(chunks[1], (256, 256));
        assert_eq!(chunks[2], (512, 256));
        assert_eq!(chunks[3], (768, 232));

        // Total block count should match.
        let total: u32 = chunks.iter().map(|(_, c)| c).sum();
        assert_eq!(total, 1000);
    }

    #[test]
    fn test_chunk_write() {
        // 500 blocks with max 256 should produce 2 chunks:
        // 256 + 244 = 500
        let chunks = compute_write_chunks(100, 500, 4096, 256);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], (100, 256));
        assert_eq!(chunks[1], (356, 244));

        let total: u32 = chunks.iter().map(|(_, c)| c).sum();
        assert_eq!(total, 500);
    }

    #[test]
    fn test_max_read_blocks() {
        // 1 MiB burst with 4096 byte blocks = 256 blocks.
        assert_eq!(max_read_blocks_for(1_048_576, 4096), 256);
        // 1 MiB burst with 512 byte blocks = 2048 blocks.
        assert_eq!(max_read_blocks_for(1_048_576, 512), 2048);
    }

    #[test]
    fn test_max_read_blocks_zero_block_size() {
        // Should not panic, returns 0.
        assert_eq!(max_read_blocks_for(1_048_576, 0), 0);
    }

    #[test]
    fn test_chunk_read_exact_multiple() {
        // 512 blocks with max 256 = exactly 2 chunks.
        let chunks = compute_read_chunks(0, 512, 256);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], (0, 256));
        assert_eq!(chunks[1], (256, 256));
    }

    #[test]
    fn test_chunk_read_zero() {
        // 0 blocks should produce no chunks.
        let chunks = compute_read_chunks(0, 0, 256);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_read_nonzero_start_lba() {
        // Verify that start LBA is carried through correctly.
        let chunks = compute_read_chunks(1000, 600, 256);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], (1000, 256));
        assert_eq!(chunks[1], (1256, 256));
        assert_eq!(chunks[2], (1512, 88));
    }
}
