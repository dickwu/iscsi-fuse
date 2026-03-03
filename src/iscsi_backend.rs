use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use iscsi_client_rs::cfg::config::Config;
use iscsi_client_rs::client::pool_sessions::Pool;
use iscsi_client_rs::control_block::read::{build_read10, build_read16};
use iscsi_client_rs::control_block::read_capacity::{
    build_read_capacity10, build_read_capacity16, parse_read_capacity10_zerocopy,
    parse_read_capacity16_zerocopy,
};
use iscsi_client_rs::control_block::write::{build_write10, build_write16};
use iscsi_client_rs::state_machine::read_states::ReadCtx;
use iscsi_client_rs::state_machine::write_states::WriteCtx;

/// Maximum number of blocks per single SCSI READ/WRITE command.
/// Conservative to stay within typical MaxBurstLength.
const MAX_BLOCKS_PER_IO: u32 = 128;

/// Timeout for individual SCSI operations.
const SCSI_TIMEOUT: Duration = Duration::from_secs(30);

pub struct IscsiBackend {
    pool: Arc<Pool>,
    _cancel: CancellationToken,
    tsih: u16,
    cid: u16,
    lun: u64,
    block_size: u32,
    total_blocks: u64,
}

impl IscsiBackend {
    /// Connect to the iSCSI target, login, and query device capacity.
    pub async fn connect(cfg: &Config, lun: u64) -> Result<Self> {
        let cancel = CancellationToken::new();
        let pool = Arc::new(Pool::with_cancel(cfg, cancel.clone()));
        pool.attach_self();

        info!("Logging in to iSCSI target...");
        let tsihs = pool
            .login_sessions_from_cfg(cfg)
            .await
            .context("iSCSI login failed")?;

        let tsih = *tsihs.first().context("No sessions returned from login")?;
        let cid: u16 = 0;

        info!(tsih, cid, "iSCSI session established");

        let mut backend = Self {
            pool,
            _cancel: cancel,
            tsih,
            cid,
            lun,
            block_size: 0,
            total_blocks: 0,
        };

        let (total_blocks, block_size) = backend.read_capacity().await?;
        backend.block_size = block_size;
        backend.total_blocks = total_blocks;

        info!(
            block_size,
            total_blocks,
            total_bytes = total_blocks as u128 * block_size as u128,
            "Device capacity queried"
        );

        Ok(backend)
    }

    /// Query READ CAPACITY.
    /// First issues READ CAPACITY(10). If the device is too large (>2TB, max_lba
    /// = 0xFFFFFFFF), retries with READ CAPACITY(16). On first command after a
    /// new session, SCSI targets may respond with UNIT ATTENTION — we retry once
    /// to clear it. If READ CAPACITY(16) fails, falls back to the READ CAPACITY(10)
    /// result (device may be exactly 2TB).
    async fn read_capacity(&self) -> Result<(u64, u32)> {
        // UNIT ATTENTION clears after first occurrence; retry up to 2 times.
        let mut last_err = anyhow::anyhow!("no attempts");
        // Saved RC(10) overflow result: max_lba=0xFFFFFFFF means device is ≥ 2TB.
        let mut rc10_overflow: Option<(u32, u32)> = None;

        for attempt in 0..2u32 {
            match self.read_capacity10().await {
                Ok((max_lba, block_len)) => {
                    if max_lba != 0xFFFF_FFFF {
                        // Fits in 32-bit LBA space (device < 2TB)
                        return Ok((max_lba as u64 + 1, block_len));
                    }
                    // Device is ≥ 2TB — need READ CAPACITY(16) for exact size.
                    // Save this in case RC(16) fails (e.g. target quirk).
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

        // RC(16) failed but RC(10) told us the device is ≥ 2TB.
        // Fall back: treat 0xFFFFFFFF as the actual last LBA (device is exactly 2TB).
        if let Some((max_lba, block_len)) = rc10_overflow {
            warn!(
                "READ CAPACITY(16) unavailable; falling back to READ CAPACITY(10) \
                 result — device reported as exactly 2 TiB"
            );
            return Ok((max_lba as u64 + 1, block_len));
        }

        Err(last_err.context("READ CAPACITY failed after retries"))
    }

    async fn read_capacity16(&self) -> Result<(u64, u32)> {
        let tsih = self.tsih;
        let cid = self.cid;
        let lun = self.lun;

        let outcome = tokio::time::timeout(SCSI_TIMEOUT, async {
            self.pool
                .execute_with(tsih, cid, |conn, itt, cmd_sn, exp_stat_sn| {
                    let mut cdb = [0u8; 16];
                    build_read_capacity16(&mut cdb, 0, false, 32, 0);
                    ReadCtx::new(conn, lun, itt, cmd_sn, exp_stat_sn, 32, cdb)
                })
                .await
        })
        .await
        .context("READ CAPACITY(16) timed out")?
        .context("READ CAPACITY(16) failed")?;

        debug!(
            data_len = outcome.data.len(),
            "READ CAPACITY(16) raw response bytes: {:02x?}", &outcome.data
        );

        let parsed = parse_read_capacity16_zerocopy(&outcome.data)?;
        let max_lba = parsed.max_lba.get();
        let block_len = parsed.block_len.get();

        if block_len == 0 {
            anyhow::bail!("READ CAPACITY(16) returned block_len=0");
        }

        let total_blocks = max_lba
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("READ CAPACITY(16) max_lba=0xFFFFFFFFFFFFFFFF — target returned invalid/all-FF response"))?;

        Ok((total_blocks, block_len))
    }

    async fn read_capacity10(&self) -> Result<(u32, u32)> {
        let tsih = self.tsih;
        let cid = self.cid;
        let lun = self.lun;

        let outcome = tokio::time::timeout(SCSI_TIMEOUT, async {
            self.pool
                .execute_with(tsih, cid, |conn, itt, cmd_sn, exp_stat_sn| {
                    let mut cdb = [0u8; 16];
                    build_read_capacity10(&mut cdb, 0, false, 0);
                    ReadCtx::new(conn, lun, itt, cmd_sn, exp_stat_sn, 8, cdb)
                })
                .await
        })
        .await
        .context("READ CAPACITY(10) timed out")?
        .context("READ CAPACITY(10) failed")?;

        let parsed = parse_read_capacity10_zerocopy(&outcome.data)?;
        let max_lba = parsed.max_lba.get();
        let block_len = parsed.block_len.get();

        if block_len == 0 {
            anyhow::bail!("READ CAPACITY(10) returned block_len=0");
        }

        Ok((max_lba, block_len))
    }

    /// Issue a SCSI READ command. Returns exactly `block_count * block_size` bytes.
    /// Automatically chunks large requests.
    pub async fn scsi_read(&self, lba: u64, block_count: u32) -> Result<Vec<u8>> {
        if block_count == 0 {
            return Ok(Vec::new());
        }

        let mut result = Vec::with_capacity(block_count as usize * self.block_size as usize);
        let mut remaining = block_count;
        let mut current_lba = lba;

        while remaining > 0 {
            let chunk_blocks = remaining.min(MAX_BLOCKS_PER_IO);
            let chunk_data = self.scsi_read_single(current_lba, chunk_blocks).await?;
            result.extend_from_slice(&chunk_data);
            current_lba += chunk_blocks as u64;
            remaining -= chunk_blocks;
        }

        Ok(result)
    }

    /// Single SCSI READ command (no chunking).
    async fn scsi_read_single(&self, lba: u64, block_count: u32) -> Result<Vec<u8>> {
        let tsih = self.tsih;
        let cid = self.cid;
        let lun = self.lun;
        let read_len = block_count * self.block_size;

        debug!(lba, block_count, read_len, "SCSI READ");

        let outcome = tokio::time::timeout(SCSI_TIMEOUT, async {
            self.pool
                .execute_with(tsih, cid, move |conn, itt, cmd_sn, exp_stat_sn| {
                    let mut cdb = [0u8; 16];
                    if lba <= u32::MAX as u64 && block_count <= u16::MAX as u32 {
                        build_read10(&mut cdb, lba as u32, block_count as u16, 0, 0);
                    } else {
                        build_read16(&mut cdb, lba, block_count, 0, 0);
                    }
                    ReadCtx::new(conn, lun, itt, cmd_sn, exp_stat_sn, read_len, cdb)
                })
                .await
        })
        .await
        .context("SCSI READ timed out")?
        .context("SCSI READ failed")?;

        Ok(outcome.data)
    }

    /// Issue a SCSI WRITE command. `data` length must be a multiple of block_size.
    /// Automatically chunks large requests.
    pub async fn scsi_write(&self, lba: u64, data: Vec<u8>) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let total_blocks = data.len() as u32 / self.block_size;
        let mut offset = 0usize;
        let mut current_lba = lba;
        let mut remaining = total_blocks;

        while remaining > 0 {
            let chunk_blocks = remaining.min(MAX_BLOCKS_PER_IO);
            let chunk_bytes = chunk_blocks as usize * self.block_size as usize;
            let chunk_data = data[offset..offset + chunk_bytes].to_vec();
            self.scsi_write_single(current_lba, chunk_data).await?;
            current_lba += chunk_blocks as u64;
            offset += chunk_bytes;
            remaining -= chunk_blocks;
        }

        Ok(())
    }

    /// Single SCSI WRITE command (no chunking).
    async fn scsi_write_single(&self, lba: u64, data: Vec<u8>) -> Result<()> {
        let tsih = self.tsih;
        let cid = self.cid;
        let lun = self.lun;
        let block_count = data.len() as u32 / self.block_size;

        debug!(lba, block_count, bytes = data.len(), "SCSI WRITE");

        tokio::time::timeout(SCSI_TIMEOUT, async {
            self.pool
                .execute_with(tsih, cid, move |conn, itt, cmd_sn, exp_stat_sn| {
                    let mut cdb = [0u8; 16];
                    if lba <= u32::MAX as u64 && block_count <= u16::MAX as u32 {
                        build_write10(&mut cdb, lba as u32, block_count as u16, 0, 0);
                    } else {
                        build_write16(&mut cdb, lba, block_count, 0, 0);
                    }
                    WriteCtx::new(conn, lun, itt, cmd_sn, exp_stat_sn, cdb, data)
                })
                .await
        })
        .await
        .context("SCSI WRITE timed out")?
        .context("SCSI WRITE failed")?;

        Ok(())
    }

    /// Graceful logout and disconnect.
    pub async fn disconnect(&self) -> Result<()> {
        info!("Disconnecting iSCSI session...");
        self.pool
            .shutdown_gracefully(Duration::from_secs(10))
            .await
            .context("iSCSI shutdown failed")?;
        info!("iSCSI session disconnected");
        Ok(())
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
}
