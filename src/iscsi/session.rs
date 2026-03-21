use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::{Mutex, oneshot};
use tracing::debug;

use super::login::NegotiatedParams;
use super::pdu::{Bhs, Pdu};
use super::transport::TransportWriter;
use crate::iscsi::command::ScsiStatus;

// ---------------------------------------------------------------------------
// PduResponse
// ---------------------------------------------------------------------------

/// What a completed SCSI command returns to the caller.
pub struct PduResponse {
    pub status: ScsiStatus,
    pub data: Option<Bytes>,
    pub sense: Option<Bytes>,
}

// ---------------------------------------------------------------------------
// IttPool — manages 128 ITT slots using two AtomicU64
// ---------------------------------------------------------------------------

/// Lock-free ITT (Initiator Task Tag) allocator with 128 slots.
///
/// Bits in `slots_lo` (ITTs 0-63) and `slots_hi` (ITTs 64-127) track which
/// ITTs are in-use. A set bit means the slot is occupied.
pub struct IttPool {
    slots_lo: AtomicU64,
    slots_hi: AtomicU64,
    completions: Mutex<Vec<Option<oneshot::Sender<PduResponse>>>>,
    write_data: Mutex<HashMap<u32, Bytes>>,
}

impl IttPool {
    /// Create a new pool with all 128 ITT slots free.
    pub fn new() -> Self {
        Self {
            slots_lo: AtomicU64::new(0),
            slots_hi: AtomicU64::new(0),
            completions: Mutex::new((0..128).map(|_| None).collect()),
            write_data: Mutex::new(HashMap::new()),
        }
    }

    /// Allocate a free ITT slot.
    ///
    /// Returns `Some((itt, receiver))` where `receiver` will deliver the
    /// `PduResponse` when the command completes, or `None` if all 128 slots
    /// are occupied.
    pub fn alloc(&self) -> Option<(u32, oneshot::Receiver<PduResponse>)> {
        // Try slots_lo first (ITTs 0-63).
        loop {
            let current = self.slots_lo.load(Ordering::Relaxed);
            let free_bit = (!current).trailing_zeros();
            if free_bit < 64 {
                let mask = 1u64 << free_bit;
                match self.slots_lo.compare_exchange_weak(
                    current,
                    current | mask,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let itt = free_bit;
                        let (tx, rx) = oneshot::channel();
                        // Store sender — blocking_lock is fine here since we only
                        // hold the lock very briefly and never across an await.
                        self.completions.blocking_lock()[itt as usize] = Some(tx);
                        return Some((itt, rx));
                    }
                    Err(_) => continue, // CAS failed, retry
                }
            } else {
                break; // slots_lo full, try slots_hi
            }
        }

        // Try slots_hi (ITTs 64-127).
        loop {
            let current = self.slots_hi.load(Ordering::Relaxed);
            let free_bit = (!current).trailing_zeros();
            if free_bit < 64 {
                let mask = 1u64 << free_bit;
                match self.slots_hi.compare_exchange_weak(
                    current,
                    current | mask,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let itt = 64 + free_bit;
                        let (tx, rx) = oneshot::channel();
                        self.completions.blocking_lock()[itt as usize] = Some(tx);
                        return Some((itt, rx));
                    }
                    Err(_) => continue,
                }
            } else {
                return None; // All 128 slots occupied
            }
        }
    }

    /// Free an ITT slot, making it available for reuse.
    pub fn free(&self, itt: u32) {
        debug_assert!(itt < 128, "ITT out of range: {itt}");
        if itt < 64 {
            self.slots_lo.fetch_and(!(1u64 << itt), Ordering::Release);
        } else {
            self.slots_hi
                .fetch_and(!(1u64 << (itt - 64)), Ordering::Release);
        }
    }

    /// Complete a command by sending the response to the waiting caller,
    /// then free the ITT slot.
    pub fn complete(&self, itt: u32, response: PduResponse) {
        debug_assert!(itt < 128, "ITT out of range: {itt}");
        let sender = self.completions.blocking_lock()[itt as usize].take();
        if let Some(tx) = sender {
            // Receiver may have been dropped (e.g. timeout), ignore send error.
            let _ = tx.send(response);
        }
        self.free(itt);
    }

    /// Register write data for a specific ITT (used for R2T handling).
    pub fn register_write_data(&self, itt: u32, data: Bytes) {
        self.write_data.blocking_lock().insert(itt, data);
    }

    /// Get a clone of the write data registered for an ITT.
    pub fn get_write_data(&self, itt: u32) -> Option<Bytes> {
        self.write_data.blocking_lock().get(&itt).cloned()
    }

    /// Remove and discard the write data for an ITT.
    pub fn remove_write_data(&self, itt: u32) {
        self.write_data.blocking_lock().remove(&itt);
    }

    /// Return all ITT slot indices that are currently in-use.
    pub fn outstanding_itts(&self) -> Vec<u32> {
        let mut result = Vec::new();
        let lo = self.slots_lo.load(Ordering::Relaxed);
        let hi = self.slots_hi.load(Ordering::Relaxed);

        let mut bits = lo;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            result.push(bit);
            bits &= bits - 1; // clear lowest set bit
        }

        let mut bits = hi;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            result.push(64 + bit);
            bits &= bits - 1;
        }

        result
    }
}

// ---------------------------------------------------------------------------
// SessionState — atomic sequence numbers
// ---------------------------------------------------------------------------

/// Atomic session-level sequence numbers tracked by the initiator.
pub struct SessionState {
    pub cmd_sn: AtomicU32,
    pub exp_stat_sn: AtomicU32,
    /// Target's ExpCmdSN (from most recent response).
    pub exp_cmd_sn: AtomicU32,
    /// Target's MaxCmdSN (from most recent response).
    pub max_cmd_sn: AtomicU32,
}

impl SessionState {
    /// Initialize session state with the given starting sequence numbers.
    pub fn new(initial_cmd_sn: u32, initial_exp_stat_sn: u32) -> Self {
        Self {
            cmd_sn: AtomicU32::new(initial_cmd_sn),
            exp_stat_sn: AtomicU32::new(initial_exp_stat_sn),
            exp_cmd_sn: AtomicU32::new(initial_cmd_sn),
            max_cmd_sn: AtomicU32::new(initial_cmd_sn),
        }
    }

    /// Check if CmdSN is within the command window.
    ///
    /// The target advertises [ExpCmdSN, MaxCmdSN] as the valid window.
    /// We check `cmd_sn <= max_cmd_sn` using RFC 1982 serial comparison.
    pub fn cmd_sn_in_window(&self) -> bool {
        let cmd = self.cmd_sn.load(Ordering::Relaxed);
        let max = self.max_cmd_sn.load(Ordering::Relaxed);
        serial_le(cmd, max)
    }
}

// ---------------------------------------------------------------------------
// serial_le — RFC 1982 serial number arithmetic
// ---------------------------------------------------------------------------

/// RFC 1982 serial number less-than-or-equal comparison for 32-bit values.
///
/// Returns true if `a <= b` in the serial number space, i.e. a equals b or
/// b is "ahead" of a by less than 2^31.
pub fn serial_le(a: u32, b: u32) -> bool {
    a == b || {
        let diff = b.wrapping_sub(a);
        diff > 0 && diff < 0x8000_0000
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// An active iSCSI full-feature-phase session.
///
/// Holds the writer half of the transport (the reader is owned by the
/// receiver task, which is implemented separately in Task 8b).
pub struct Session {
    writer: tokio::sync::Mutex<TransportWriter>,
    pub itt_pool: Arc<IttPool>,
    pub state: SessionState,
    pub negotiated: NegotiatedParams,
    /// Monotonic instant of the last received PDU (for NOP keepalive).
    last_recv: Mutex<Instant>,
}

impl Session {
    /// Create a new session in full-feature phase.
    pub fn new(
        writer: TransportWriter,
        itt_pool: Arc<IttPool>,
        state: SessionState,
        negotiated: NegotiatedParams,
    ) -> Self {
        Self {
            writer: tokio::sync::Mutex::new(writer),
            itt_pool,
            state,
            negotiated,
            last_recv: Mutex::new(Instant::now()),
        }
    }

    /// Submit a SCSI command PDU and return the ITT + a receiver for the
    /// response.
    ///
    /// This method:
    /// 1. Waits for the CmdSN window to open
    /// 2. Allocates an ITT slot
    /// 3. Builds and sends the SCSI Command PDU
    /// 4. Returns (itt, receiver) for the caller to await the response
    pub async fn submit_command(
        &self,
        cdb: &[u8; 16],
        lun: u64,
        edtl: u32,
        read: bool,
        write: bool,
        immediate_data: Option<Bytes>,
    ) -> Result<(u32, oneshot::Receiver<PduResponse>)> {
        // 1. Wait for CmdSN window.
        loop {
            if self.state.cmd_sn_in_window() {
                break;
            }
            tokio::task::yield_now().await;
        }

        // 2. Allocate ITT.
        let (itt, rx) = loop {
            if let Some(pair) = self.itt_pool.alloc() {
                break pair;
            }
            tokio::task::yield_now().await;
        };

        // 3. Advance CmdSN and load ExpStatSN.
        let cmd_sn = self.state.cmd_sn.fetch_add(1, Ordering::AcqRel);
        let exp_stat_sn = self.state.exp_stat_sn.load(Ordering::Acquire);

        // 4. Build the SCSI Command BHS.
        let mut bhs =
            Bhs::build_scsi_command(lun, itt, cmd_sn, exp_stat_sn, cdb, edtl, read, write);

        // 5. Set data segment length if immediate data is provided.
        if let Some(ref data) = immediate_data {
            bhs.data_segment_length = data.len() as u32;
        }

        let pdu = Pdu {
            bhs,
            ahs: None,
            data: immediate_data,
        };

        // 6. Lock writer and send.
        {
            let mut w = self.writer.lock().await;
            w.send_pdu(&pdu).await?;
        }

        debug!(itt, cmd_sn, "SCSI command submitted");
        Ok((itt, rx))
    }

    /// Send a NOP-Out PDU (unsolicited ping for keepalive).
    pub async fn send_nop_out(&self) -> Result<()> {
        let cmd_sn = self.state.cmd_sn.load(Ordering::Relaxed);
        let exp_stat_sn = self.state.exp_stat_sn.load(Ordering::Acquire);

        let bhs = Bhs::build_nop_out(0xFFFF_FFFF, 0xFFFF_FFFF, cmd_sn, exp_stat_sn);
        let pdu = Pdu {
            bhs,
            ahs: None,
            data: None,
        };

        let mut w = self.writer.lock().await;
        w.send_pdu(&pdu).await?;
        debug!("NOP-Out sent");
        Ok(())
    }

    /// Send a Logout Request PDU.
    pub async fn send_logout(&self) -> Result<()> {
        let cmd_sn = self.state.cmd_sn.fetch_add(1, Ordering::AcqRel);
        let exp_stat_sn = self.state.exp_stat_sn.load(Ordering::Acquire);

        let bhs = Bhs::build_logout_request(0xFFFF_FFFF, cmd_sn, exp_stat_sn, 0);
        let pdu = Pdu {
            bhs,
            ahs: None,
            data: None,
        };

        let mut w = self.writer.lock().await;
        w.send_pdu(&pdu).await?;
        debug!("Logout request sent");
        Ok(())
    }

    /// Update the last-received timestamp to now.
    pub fn update_last_recv(&self) {
        *self.last_recv.blocking_lock() = Instant::now();
    }

    /// Return how long it has been since the last PDU was received.
    pub fn time_since_last_recv(&self) -> Duration {
        self.last_recv.blocking_lock().elapsed()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_itt_pool_alloc_and_free() {
        let pool = IttPool::new();

        // Allocate a slot.
        let (itt, _rx) = pool.alloc().expect("should allocate");
        assert!(itt < 128, "ITT should be in range [0, 128)");

        // Free it.
        pool.free(itt);

        // Re-allocate — should get the same slot back (it was the only
        // one ever used and is now the lowest free bit).
        let (itt2, _rx2) = pool.alloc().expect("should re-allocate");
        assert_eq!(itt, itt2, "should get the same ITT back after free");
    }

    #[test]
    fn test_itt_pool_alloc_128() {
        let pool = IttPool::new();
        let mut receivers = Vec::new();

        // Allocate all 128 slots.
        for i in 0..128 {
            let (itt, rx) = pool
                .alloc()
                .unwrap_or_else(|| panic!("alloc {i} should succeed"));
            assert!(itt < 128);
            receivers.push((itt, rx));
        }

        // 129th allocation should fail.
        assert!(pool.alloc().is_none(), "129th alloc should return None");

        // Free one slot and retry.
        let freed_itt = receivers[50].0;
        pool.free(freed_itt);

        let (itt, _rx) = pool.alloc().expect("should succeed after free");
        assert_eq!(itt, freed_itt, "should get the freed ITT back");
    }

    #[test]
    fn test_itt_pool_complete() {
        let pool = IttPool::new();

        let (itt, mut rx) = pool.alloc().expect("should allocate");

        // Complete with a response.
        let response = PduResponse {
            status: ScsiStatus::Good,
            data: Some(Bytes::from_static(b"test data")),
            sense: None,
        };
        pool.complete(itt, response);

        // The receiver should have the response.
        let received = rx.try_recv().expect("should receive response");
        assert_eq!(received.status, ScsiStatus::Good);
        assert_eq!(received.data.as_deref(), Some(b"test data".as_slice()));
        assert!(received.sense.is_none());
    }

    #[test]
    fn test_serial_number_le() {
        // Basic cases.
        assert!(serial_le(1, 2), "1 <= 2");
        assert!(serial_le(1, 1), "1 <= 1");
        assert!(!serial_le(2, 1), "!(2 <= 1)");

        // Wrap-around: a large number just before wrap should be <= a small
        // number just after wrap.
        assert!(serial_le(0xFFFF_FFFE, 0x0000_0001), "wrap-around case");
        assert!(serial_le(0xFFFF_FFFF, 0x0000_0000), "max <= 0 (wrap)");

        // The reverse should not hold.
        assert!(!serial_le(0x0000_0001, 0xFFFF_FFFE), "reverse wrap");

        // Boundary at exactly 2^31 apart — should be false (ambiguous in
        // RFC 1982, our implementation returns false).
        assert!(!serial_le(0, 0x8000_0000), "exactly 2^31 apart");
    }

    #[test]
    fn test_session_state_cmd_window() {
        let state = SessionState::new(1, 0);

        // Default: max_cmd_sn == initial_cmd_sn == 1, so cmd_sn=1 is in window.
        assert!(
            state.cmd_sn_in_window(),
            "initial cmd_sn should be in window"
        );

        // Widen the window.
        state.max_cmd_sn.store(32, Ordering::Relaxed);
        assert!(state.cmd_sn_in_window(), "cmd_sn=1 <= max_cmd_sn=32");

        // Advance cmd_sn past the window.
        state.cmd_sn.store(33, Ordering::Relaxed);
        assert!(
            !state.cmd_sn_in_window(),
            "cmd_sn=33 > max_cmd_sn=32, outside window"
        );

        // Exactly at the edge.
        state.cmd_sn.store(32, Ordering::Relaxed);
        assert!(state.cmd_sn_in_window(), "cmd_sn=32 == max_cmd_sn=32");
    }
}
