#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use std::sync::Mutex;
use tokio::sync::oneshot;
use tracing::debug;

use super::login::NegotiatedParams;
use super::pdu::{Bhs, Opcode, Pdu};
use super::transport::{TransportReader, TransportWriter};
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

impl Default for IttPool {
    fn default() -> Self {
        Self::new()
    }
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
                        self.completions.lock().unwrap()[itt as usize] = Some(tx);
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
                        self.completions.lock().unwrap()[itt as usize] = Some(tx);
                        return Some((itt, rx));
                    }
                    Err(_) => continue,
                }
            } else {
                return None; // All 128 slots occupied
            }
        }
    }

    /// Allocate a free ITT slot (async version for use from async contexts).
    pub async fn alloc_async(&self) -> Option<(u32, oneshot::Receiver<PduResponse>)> {
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
                        self.completions.lock().unwrap()[itt as usize] = Some(tx);
                        return Some((itt, rx));
                    }
                    Err(_) => continue,
                }
            } else {
                break;
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
                        self.completions.lock().unwrap()[itt as usize] = Some(tx);
                        return Some((itt, rx));
                    }
                    Err(_) => continue,
                }
            } else {
                return None;
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
        let sender = self.completions.lock().unwrap()[itt as usize].take();
        if let Some(tx) = sender {
            // Receiver may have been dropped (e.g. timeout), ignore send error.
            let _ = tx.send(response);
        }
        self.free(itt);
    }

    /// Register write data for a specific ITT (used for R2T handling).
    pub fn register_write_data(&self, itt: u32, data: Bytes) {
        self.write_data.lock().unwrap().insert(itt, data);
    }

    /// Get a clone of the write data registered for an ITT.
    pub fn get_write_data(&self, itt: u32) -> Option<Bytes> {
        self.write_data.lock().unwrap().get(&itt).cloned()
    }

    /// Remove and discard the write data for an ITT.
    pub fn remove_write_data(&self, itt: u32) {
        self.write_data.lock().unwrap().remove(&itt);
    }

    /// Complete a command asynchronously (for use from async contexts like
    /// the receiver task). Same as `complete` but uses `.lock().unwrap()`.
    pub async fn complete_async(&self, itt: u32, response: PduResponse) {
        debug_assert!(itt < 128, "ITT out of range: {itt}");
        let sender = self.completions.lock().unwrap()[itt as usize].take();
        if let Some(tx) = sender {
            let _ = tx.send(response);
        }
        self.free(itt);
    }

    /// Async version of `get_write_data` for use from async contexts.
    pub async fn get_write_data_async(&self, itt: u32) -> Option<Bytes> {
        self.write_data.lock().unwrap().get(&itt).cloned()
    }

    /// Async version of `remove_write_data` for use from async contexts.
    pub async fn remove_write_data_async(&self, itt: u32) {
        self.write_data.lock().unwrap().remove(&itt);
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
    /// Accumulated Data-In bytes keyed by ITT, used to reassemble multi-PDU
    /// read responses before the final status-bearing Data-In arrives.
    data_accumulator: Mutex<HashMap<u32, BytesMut>>,
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
            data_accumulator: Mutex::new(HashMap::new()),
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

    /// Update the last-received timestamp to now (sync version for non-async callers).
    pub fn update_last_recv(&self) {
        *self.last_recv.lock().unwrap() = Instant::now();
    }

    /// Update the last-received timestamp to now (async version for the receiver task).
    pub async fn update_last_recv_async(&self) {
        *self.last_recv.lock().unwrap() = Instant::now();
    }

    /// Return how long it has been since the last PDU was received.
    pub fn time_since_last_recv(&self) -> Duration {
        self.last_recv.lock().unwrap().elapsed()
    }

    // -----------------------------------------------------------------------
    // Receiver task
    // -----------------------------------------------------------------------

    /// Spawn the background receiver task that reads PDUs from the target and
    /// dispatches them to the appropriate handler.
    pub fn spawn_receiver(
        self: &Arc<Self>,
        mut reader: TransportReader,
        itt_pool: Arc<IttPool>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let session = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let pdu = reader.recv_pdu().await?;
                session.update_last_recv_async().await;
                session.update_sequence_numbers(&pdu.bhs);

                match pdu.bhs.opcode {
                    Opcode::ScsiResponse => {
                        session.handle_scsi_response(&itt_pool, &pdu).await;
                    }
                    Opcode::ScsiDataIn => {
                        session.handle_data_in(&itt_pool, &pdu).await;
                    }
                    Opcode::R2t => {
                        session.handle_r2t(&itt_pool, &pdu).await?;
                    }
                    Opcode::NopIn => {
                        session.handle_nop_in(&pdu).await?;
                    }
                    Opcode::AsyncMessage => {
                        tracing::warn!("Received AsyncMessage PDU");
                    }
                    Opcode::Reject => {
                        tracing::error!("Received Reject PDU");
                    }
                    _ => {
                        tracing::warn!(opcode = ?pdu.bhs.opcode, "Unexpected PDU");
                    }
                }
            }
        })
    }

    // -----------------------------------------------------------------------
    // Sequence number bookkeeping
    // -----------------------------------------------------------------------

    /// Update session-level sequence numbers from a target response BHS.
    ///
    /// - StatSN: store `stat_sn + 1` as our next ExpStatSN.
    /// - ExpCmdSN / MaxCmdSN: update the command window.
    fn update_sequence_numbers(&self, bhs: &Bhs) {
        let stat_sn = bhs.stat_sn();
        self.state
            .exp_stat_sn
            .store(stat_sn.wrapping_add(1), Ordering::Release);
        self.state
            .exp_cmd_sn
            .store(bhs.exp_cmd_sn(), Ordering::Release);
        self.state
            .max_cmd_sn
            .store(bhs.max_cmd_sn(), Ordering::Release);
    }

    // -----------------------------------------------------------------------
    // PDU handlers
    // -----------------------------------------------------------------------

    /// Handle a SCSI Response PDU (opcode 0x21).
    ///
    /// Extracts the SCSI status, takes any accumulated read data for this ITT,
    /// and completes the ITT oneshot channel.
    async fn handle_scsi_response(&self, itt_pool: &IttPool, pdu: &Pdu) {
        let itt = pdu.bhs.itt;
        let scsi_status = ScsiStatus::from(pdu.bhs.scsi_status());

        // Take accumulated Data-In bytes (if any) for this ITT.
        let data = self
            .data_accumulator
            .lock()
            .unwrap()
            .remove(&itt)
            .map(|b: BytesMut| b.freeze());

        let response = PduResponse {
            status: scsi_status,
            data,
            sense: pdu.data.clone(),
        };

        debug!(itt, ?scsi_status, "SCSI response received");
        itt_pool.complete_async(itt, response).await;
        itt_pool.remove_write_data_async(itt).await;
    }

    /// Handle a Data-In PDU (opcode 0x25).
    ///
    /// Accumulates read data at the specified buffer offset. If the S (status)
    /// bit is set, the transfer is complete: take all accumulated data, build
    /// a PduResponse, and complete the ITT.
    async fn handle_data_in(&self, itt_pool: &IttPool, pdu: &Pdu) {
        let itt = pdu.bhs.itt;
        let buffer_offset = pdu.bhs.buffer_offset() as usize;

        // Accumulate data if present.
        if let Some(ref data) = pdu.data {
            let mut acc = self.data_accumulator.lock().unwrap();
            let buf = acc.entry(itt).or_default();
            let needed = buffer_offset + data.len();
            if buf.len() < needed {
                buf.resize(needed, 0);
            }
            buf[buffer_offset..buffer_offset + data.len()].copy_from_slice(data);
        }

        // If the status flag (S bit) is set, the transfer is complete.
        if pdu.bhs.status_flag() {
            let scsi_status = ScsiStatus::from(pdu.bhs.scsi_status());
            let data = self
                .data_accumulator
                .lock()
                .unwrap()
                .remove(&itt)
                .map(|b: BytesMut| b.freeze());

            let response = PduResponse {
                status: scsi_status,
                data,
                sense: None,
            };

            debug!(itt, ?scsi_status, "Data-In complete (S bit set)");
            itt_pool.complete_async(itt, response).await;
        }
    }

    /// Handle an R2T (Ready To Transfer) PDU (opcode 0x31).
    ///
    /// The target is requesting write data. We slice the registered write data
    /// at the requested offset/length and send one or more Data-Out PDUs.
    async fn handle_r2t(&self, itt_pool: &IttPool, pdu: &Pdu) -> Result<()> {
        let itt = pdu.bhs.itt;
        let ttt = pdu.bhs.ttt();
        let buffer_offset = pdu.bhs.r2t_buffer_offset() as usize;
        let desired_length = pdu.bhs.r2t_desired_length() as usize;

        // The write data may not be registered yet if the target sent R2T
        // before our submit_command caller had a chance to call register_write_data.
        // Retry briefly to close this race window.
        let write_data = {
            let mut data = None;
            for _ in 0..50 {
                if let Some(d) = itt_pool.get_write_data_async(itt).await {
                    data = Some(d);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            match data {
                Some(d) => d,
                None => {
                    tracing::error!(itt, "R2T for ITT with no registered write data after retries");
                    return Ok(());
                }
            }
        };

        let max_segment = if self.negotiated.max_send_data_segment_length > 0 {
            self.negotiated.max_send_data_segment_length as usize
        } else if self.negotiated.max_recv_data_segment_length > 0 {
            self.negotiated.max_recv_data_segment_length as usize
        } else {
            262_144
        };

        let exp_stat_sn = self.state.exp_stat_sn.load(Ordering::Acquire);
        let mut sent = 0usize;
        let mut data_sn = 0u32;

        while sent < desired_length {
            let remaining = desired_length - sent;
            let chunk_len = remaining.min(max_segment);
            let offset = buffer_offset + sent;
            let end = (offset + chunk_len).min(write_data.len());
            let chunk = write_data.slice(offset..end);
            let is_final = sent + chunk_len >= desired_length;

            let mut bhs = Bhs::build_data_out(
                0, // LUN is 0 for Data-Out (target uses ITT/TTT to identify)
                itt,
                ttt,
                exp_stat_sn,
                data_sn,
                offset as u32,
            );
            if is_final {
                bhs.flags = 0x80; // F bit
            } else {
                bhs.flags = 0x00; // no F bit
            }
            bhs.data_segment_length = chunk.len() as u32;

            let data_out_pdu = Pdu {
                bhs,
                ahs: None,
                data: Some(chunk),
            };

            {
                let mut w = self.writer.lock().await;
                w.send_pdu(&data_out_pdu).await?;
            }

            debug!(
                itt,
                ttt, data_sn, offset, chunk_len, is_final, "Data-Out sent"
            );
            sent += chunk_len;
            data_sn += 1;
        }

        Ok(())
    }

    /// Handle a NOP-In PDU (opcode 0x20).
    ///
    /// If the target sent a ping (TTT != 0xFFFFFFFF), respond with a NOP-Out
    /// echoing the TTT. If TTT == 0xFFFFFFFF, it is an unsolicited NOP-In
    /// (response to our NOP-Out) and no reply is needed.
    async fn handle_nop_in(&self, pdu: &Pdu) -> Result<()> {
        let ttt = pdu.bhs.ttt();
        if ttt != 0xFFFF_FFFF {
            // Target-initiated ping — respond with NOP-Out echoing TTT.
            let cmd_sn = self.state.cmd_sn.load(Ordering::Relaxed);
            let exp_stat_sn = self.state.exp_stat_sn.load(Ordering::Acquire);
            let bhs = Bhs::build_nop_out(0xFFFF_FFFF, ttt, cmd_sn, exp_stat_sn);
            let nop_out = Pdu {
                bhs,
                ahs: None,
                data: None,
            };

            let mut w = self.writer.lock().await;
            w.send_pdu(&nop_out).await?;
            debug!(ttt, "NOP-Out response sent for target ping");
        } else {
            debug!("Unsolicited NOP-In received (no response needed)");
        }
        Ok(())
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

#[cfg(test)]
mod receiver_tests {
    use super::*;
    use crate::iscsi::pdu::Bhs;
    use crate::iscsi::transport::{DigestConfig, TransportReader, TransportWriter};

    /// Create a loopback TCP pair and return four halves:
    ///
    /// - `session_writer`: the TransportWriter that Session will own (client → server)
    /// - `receiver_reader`: the TransportReader fed to spawn_receiver (server → client)
    /// - `target_writer`: used by the test to inject fake target PDUs (server → client)
    /// - `_target_reader`: server-side read half (unused in most tests, but kept alive)
    async fn loopback_transport() -> (
        TransportWriter,
        TransportReader,
        TransportWriter,
        TransportReader,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let digest = DigestConfig {
            header: false,
            data: false,
        };

        let (client_read, client_write) = client.into_split();
        let (server_read, server_write) = server.into_split();

        let session_writer = TransportWriter::new(client_write, digest.clone());
        let receiver_reader = TransportReader::new(client_read, digest.clone());
        let target_writer = TransportWriter::new(server_write, digest.clone());
        let target_reader = TransportReader::new(server_read, digest);

        (
            session_writer,
            receiver_reader,
            target_writer,
            target_reader,
        )
    }

    /// Build a Session wrapped in Arc with default negotiated params.
    fn build_session(writer: TransportWriter, itt_pool: Arc<IttPool>) -> Arc<Session> {
        let state = SessionState::new(1, 0);
        let negotiated = NegotiatedParams::defaults_10g();
        Arc::new(Session::new(writer, itt_pool, state, negotiated))
    }

    #[tokio::test]
    async fn test_receiver_handles_scsi_response() {
        let (session_writer, receiver_reader, mut target_writer, _target_reader) =
            loopback_transport().await;

        let itt_pool = Arc::new(IttPool::new());
        let session = build_session(session_writer, Arc::clone(&itt_pool));

        // Allocate an ITT slot (simulating a submitted command).
        let (itt, rx) = itt_pool.alloc_async().await.expect("should allocate ITT");

        // Spawn the receiver.
        let handle = session.spawn_receiver(receiver_reader, Arc::clone(&itt_pool));

        // From the "target" side, send a SCSI Response for this ITT.
        let bhs = Bhs::build_scsi_response(itt, 0x00, 1, 1, 32);
        let pdu = Pdu {
            bhs,
            ahs: None,
            data: None,
        };
        target_writer.send_pdu(&pdu).await.unwrap();

        // Await the response on the oneshot channel.
        let response = rx.await.expect("should receive response");
        assert_eq!(response.status, ScsiStatus::Good);
        assert!(response.data.is_none());

        // Abort the receiver (it would block forever waiting for next PDU).
        handle.abort();
    }

    #[tokio::test]
    async fn test_receiver_handles_data_in_with_status() {
        let (session_writer, receiver_reader, mut target_writer, _target_reader) =
            loopback_transport().await;

        let itt_pool = Arc::new(IttPool::new());
        let session = build_session(session_writer, Arc::clone(&itt_pool));

        let (itt, rx) = itt_pool.alloc_async().await.expect("should allocate ITT");

        let handle = session.spawn_receiver(receiver_reader, Arc::clone(&itt_pool));

        // Send a Data-In PDU with S=1, carrying 4096 bytes of test data.
        let data_len = 4096u32;
        let payload = Bytes::from(vec![0xABu8; data_len as usize]);
        let bhs = Bhs::build_data_in(
            itt, 0, // data_sn
            0, // buffer_offset
            data_len, true, // status_flag (S bit)
            0x00, // GOOD
            1,    // stat_sn
            1,    // exp_cmd_sn
            32,   // max_cmd_sn
        );
        let pdu = Pdu {
            bhs,
            ahs: None,
            data: Some(payload),
        };
        target_writer.send_pdu(&pdu).await.unwrap();

        let response = rx.await.expect("should receive response");
        assert_eq!(response.status, ScsiStatus::Good);
        let data = response.data.expect("should have data");
        assert_eq!(data.len(), 4096);
        assert!(data.iter().all(|&b| b == 0xAB));

        handle.abort();
    }

    #[tokio::test]
    async fn test_receiver_handles_multi_pdu_data_in() {
        let (session_writer, receiver_reader, mut target_writer, _target_reader) =
            loopback_transport().await;

        let itt_pool = Arc::new(IttPool::new());
        let session = build_session(session_writer, Arc::clone(&itt_pool));

        let (itt, rx) = itt_pool.alloc_async().await.expect("should allocate ITT");

        let handle = session.spawn_receiver(receiver_reader, Arc::clone(&itt_pool));

        // First Data-In: 2048 bytes at offset 0, no S bit.
        let chunk1 = Bytes::from(vec![0x11u8; 2048]);
        let bhs1 = Bhs::build_data_in(itt, 0, 0, 2048, false, 0, 0, 1, 32);
        let pdu1 = Pdu {
            bhs: bhs1,
            ahs: None,
            data: Some(chunk1),
        };
        target_writer.send_pdu(&pdu1).await.unwrap();

        // Second Data-In: 2048 bytes at offset 2048, S bit set.
        let chunk2 = Bytes::from(vec![0x22u8; 2048]);
        let bhs2 = Bhs::build_data_in(itt, 1, 2048, 2048, true, 0x00, 1, 1, 32);
        let pdu2 = Pdu {
            bhs: bhs2,
            ahs: None,
            data: Some(chunk2),
        };
        target_writer.send_pdu(&pdu2).await.unwrap();

        let response = rx.await.expect("should receive response");
        assert_eq!(response.status, ScsiStatus::Good);
        let data = response.data.expect("should have data");
        assert_eq!(data.len(), 4096);
        assert!(data[..2048].iter().all(|&b| b == 0x11));
        assert!(data[2048..].iter().all(|&b| b == 0x22));

        handle.abort();
    }

    #[tokio::test]
    async fn test_receiver_handles_nop_in() {
        let (session_writer, receiver_reader, mut target_writer, mut target_reader) =
            loopback_transport().await;

        let itt_pool = Arc::new(IttPool::new());
        let session = build_session(session_writer, Arc::clone(&itt_pool));

        let handle = session.spawn_receiver(receiver_reader, Arc::clone(&itt_pool));

        // Send a target-initiated NOP-In with a non-0xFFFFFFFF TTT.
        let bhs = Bhs::build_nop_in(
            0xFFFF_FFFF, // ITT (reserved for target-initiated)
            42,          // TTT (non-0xFFFFFFFF = ping requiring response)
            1,           // stat_sn
            1,           // exp_cmd_sn
            32,          // max_cmd_sn
        );
        let pdu = Pdu {
            bhs,
            ahs: None,
            data: None,
        };
        target_writer.send_pdu(&pdu).await.unwrap();

        // The receiver should send a NOP-Out response. Read it from the
        // target side (server_read).
        let nop_out = target_reader.recv_pdu().await.unwrap();
        assert_eq!(nop_out.bhs.opcode, Opcode::NopOut);
        assert_eq!(nop_out.bhs.ttt(), 42, "NOP-Out should echo the TTT");
        assert_eq!(
            nop_out.bhs.itt, 0xFFFF_FFFF,
            "ITT should be 0xFFFFFFFF for response"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_receiver_updates_sequence_numbers() {
        let (session_writer, receiver_reader, mut target_writer, _target_reader) =
            loopback_transport().await;

        let itt_pool = Arc::new(IttPool::new());
        let session = build_session(session_writer, Arc::clone(&itt_pool));

        let (itt, _rx) = itt_pool.alloc_async().await.expect("should allocate ITT");

        let handle = session.spawn_receiver(receiver_reader, Arc::clone(&itt_pool));

        // Send a SCSI Response with specific sequence numbers.
        let bhs = Bhs::build_scsi_response(
            itt, 0x00, // GOOD
            10,   // stat_sn
            5,    // exp_cmd_sn
            50,   // max_cmd_sn
        );
        let pdu = Pdu {
            bhs,
            ahs: None,
            data: None,
        };
        target_writer.send_pdu(&pdu).await.unwrap();

        // Give the receiver a moment to process.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Verify updated sequence numbers.
        assert_eq!(
            session.state.exp_stat_sn.load(Ordering::Acquire),
            11,
            "exp_stat_sn should be stat_sn + 1"
        );
        assert_eq!(
            session.state.exp_cmd_sn.load(Ordering::Acquire),
            5,
            "exp_cmd_sn should be updated"
        );
        assert_eq!(
            session.state.max_cmd_sn.load(Ordering::Acquire),
            50,
            "max_cmd_sn should be updated"
        );

        handle.abort();
    }
}
