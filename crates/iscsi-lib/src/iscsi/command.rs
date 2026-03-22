#![allow(dead_code)]

use anyhow::{bail, ensure};

// ---------------------------------------------------------------------------
// ScsiStatus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScsiStatus {
    Good = 0x00,
    CheckCondition = 0x02,
    Busy = 0x08,
    ReservationConflict = 0x18,
    TaskSetFull = 0x28,
    TaskAborted = 0x40,
}

impl From<u8> for ScsiStatus {
    fn from(v: u8) -> Self {
        match v {
            0x00 => ScsiStatus::Good,
            0x02 => ScsiStatus::CheckCondition,
            0x08 => ScsiStatus::Busy,
            0x18 => ScsiStatus::ReservationConflict,
            0x28 => ScsiStatus::TaskSetFull,
            0x40 => ScsiStatus::TaskAborted,
            _ => ScsiStatus::CheckCondition, // unknown → treat as check condition
        }
    }
}

// ---------------------------------------------------------------------------
// SenseKey
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SenseKey {
    NoSense = 0x0,
    RecoveredError = 0x1,
    NotReady = 0x2,
    MediumError = 0x3,
    HardwareError = 0x4,
    IllegalRequest = 0x5,
    UnitAttention = 0x6,
    DataProtect = 0x7,
    AbortedCommand = 0xB,
}

impl From<u8> for SenseKey {
    fn from(v: u8) -> Self {
        match v {
            0x0 => SenseKey::NoSense,
            0x1 => SenseKey::RecoveredError,
            0x2 => SenseKey::NotReady,
            0x3 => SenseKey::MediumError,
            0x4 => SenseKey::HardwareError,
            0x5 => SenseKey::IllegalRequest,
            0x6 => SenseKey::UnitAttention,
            0x7 => SenseKey::DataProtect,
            0xB => SenseKey::AbortedCommand,
            _ => SenseKey::NoSense,
        }
    }
}

// ---------------------------------------------------------------------------
// SenseData
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SenseData {
    pub sense_key: SenseKey,
    pub asc: u8,
    pub ascq: u8,
    pub information: u32,
}

// ---------------------------------------------------------------------------
// CDB builders — all return [u8; 16]
// ---------------------------------------------------------------------------

pub fn build_test_unit_ready() -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x00;
    cdb
}

pub fn build_inquiry(alloc_len: u16) -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x12;
    let bytes = alloc_len.to_be_bytes();
    cdb[3] = bytes[0];
    cdb[4] = bytes[1];
    cdb
}

pub fn build_read_capacity10() -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x25;
    cdb
}

pub fn build_read_capacity16(alloc_len: u32) -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x9E;
    cdb[1] = 0x10; // service action
    let bytes = alloc_len.to_be_bytes();
    cdb[10] = bytes[0];
    cdb[11] = bytes[1];
    cdb[12] = bytes[2];
    cdb[13] = bytes[3];
    cdb
}

pub fn build_read10(lba: u32, block_count: u16) -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x28;
    let lba_bytes = lba.to_be_bytes();
    cdb[2] = lba_bytes[0];
    cdb[3] = lba_bytes[1];
    cdb[4] = lba_bytes[2];
    cdb[5] = lba_bytes[3];
    let count_bytes = block_count.to_be_bytes();
    cdb[7] = count_bytes[0];
    cdb[8] = count_bytes[1];
    cdb
}

pub fn build_read16(lba: u64, block_count: u32) -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x88;
    let lba_bytes = lba.to_be_bytes();
    cdb[2] = lba_bytes[0];
    cdb[3] = lba_bytes[1];
    cdb[4] = lba_bytes[2];
    cdb[5] = lba_bytes[3];
    cdb[6] = lba_bytes[4];
    cdb[7] = lba_bytes[5];
    cdb[8] = lba_bytes[6];
    cdb[9] = lba_bytes[7];
    let count_bytes = block_count.to_be_bytes();
    cdb[10] = count_bytes[0];
    cdb[11] = count_bytes[1];
    cdb[12] = count_bytes[2];
    cdb[13] = count_bytes[3];
    cdb
}

pub fn build_write10(lba: u32, block_count: u16) -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x2A;
    let lba_bytes = lba.to_be_bytes();
    cdb[2] = lba_bytes[0];
    cdb[3] = lba_bytes[1];
    cdb[4] = lba_bytes[2];
    cdb[5] = lba_bytes[3];
    let count_bytes = block_count.to_be_bytes();
    cdb[7] = count_bytes[0];
    cdb[8] = count_bytes[1];
    cdb
}

pub fn build_write16(lba: u64, block_count: u32) -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x8A;
    let lba_bytes = lba.to_be_bytes();
    cdb[2] = lba_bytes[0];
    cdb[3] = lba_bytes[1];
    cdb[4] = lba_bytes[2];
    cdb[5] = lba_bytes[3];
    cdb[6] = lba_bytes[4];
    cdb[7] = lba_bytes[5];
    cdb[8] = lba_bytes[6];
    cdb[9] = lba_bytes[7];
    let count_bytes = block_count.to_be_bytes();
    cdb[10] = count_bytes[0];
    cdb[11] = count_bytes[1];
    cdb[12] = count_bytes[2];
    cdb[13] = count_bytes[3];
    cdb
}

/// Auto-select READ10 or READ16 based on addressing requirements.
pub fn build_read(lba: u64, block_count: u32) -> [u8; 16] {
    if lba <= u32::MAX as u64 && block_count <= u16::MAX as u32 {
        build_read10(lba as u32, block_count as u16)
    } else {
        build_read16(lba, block_count)
    }
}

/// Auto-select WRITE10 or WRITE16 based on addressing requirements.
pub fn build_write(lba: u64, block_count: u32) -> [u8; 16] {
    if lba <= u32::MAX as u64 && block_count <= u16::MAX as u32 {
        build_write10(lba as u32, block_count as u16)
    } else {
        build_write16(lba, block_count)
    }
}

/// Build a SYNCHRONIZE CACHE (10) CDB (opcode 0x35).
///
/// Tells the target to flush its volatile cache to persistent storage.
/// lba=0, block_count=0 means "flush the entire cache".
///
/// Reference: SBC-4 section 5.35
pub fn build_synchronize_cache10(lba: u32, block_count: u16) -> [u8; 16] {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x35;
    let lba_bytes = lba.to_be_bytes();
    cdb[2] = lba_bytes[0];
    cdb[3] = lba_bytes[1];
    cdb[4] = lba_bytes[2];
    cdb[5] = lba_bytes[3];
    let count_bytes = block_count.to_be_bytes();
    cdb[7] = count_bytes[0];
    cdb[8] = count_bytes[1];
    cdb
}

// ---------------------------------------------------------------------------
// Response parsers
// ---------------------------------------------------------------------------

pub fn parse_read_capacity10(data: &[u8]) -> anyhow::Result<(u32, u32)> {
    ensure!(
        data.len() >= 8,
        "READ CAPACITY(10) response too short: {} bytes",
        data.len()
    );
    let max_lba = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let block_len = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if block_len == 0 {
        bail!("READ CAPACITY(10) returned block length of 0");
    }
    Ok((max_lba, block_len))
}

pub fn parse_read_capacity16(data: &[u8]) -> anyhow::Result<(u64, u32)> {
    ensure!(
        data.len() >= 12,
        "READ CAPACITY(16) response too short: {} bytes",
        data.len()
    );
    let max_lba = u64::from_be_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    let block_len = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    if block_len == 0 {
        bail!("READ CAPACITY(16) returned block length of 0");
    }
    Ok((max_lba, block_len))
}

/// Parse sense data with a 2-byte iSCSI sense length prefix.
///
/// Layout: bytes 0-1 = sense data length (big-endian), bytes 2+ = fixed-format sense.
/// Fixed-format sense (response code 0x70 or 0x71):
///   byte 0 (offset 2) = response code
///   byte 2 (offset 4) = sense key (bits 3-0)
///   bytes 3-6 (offset 5-8) = information field
///   byte 12 (offset 14) = ASC
///   byte 13 (offset 15) = ASCQ
pub fn parse_sense_data(data: &[u8]) -> anyhow::Result<SenseData> {
    ensure!(
        data.len() >= 14,
        "sense data too short: {} bytes",
        data.len()
    );

    let response_code = data[2] & 0x7F;
    ensure!(
        response_code == 0x70 || response_code == 0x71,
        "unsupported sense response code: {:#04x}",
        response_code
    );

    let sense_key = SenseKey::from(data[4] & 0x0F);
    let information = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);

    // ASC and ASCQ are at fixed-format offsets 12 and 13, plus the 2-byte prefix → bytes 14, 15
    ensure!(
        data.len() >= 16,
        "sense data too short for ASC/ASCQ: {} bytes",
        data.len()
    );
    let asc = data[14];
    let ascq = data[15];

    Ok(SenseData {
        sense_key,
        asc,
        ascq,
        information,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn is_unit_attention(sense: &SenseData) -> bool {
    sense.sense_key == SenseKey::UnitAttention
}

pub fn is_retryable(status: ScsiStatus, sense: Option<&SenseData>) -> bool {
    match status {
        ScsiStatus::Busy | ScsiStatus::TaskSetFull => true,
        ScsiStatus::CheckCondition => sense.is_some_and(is_unit_attention),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_test_unit_ready() {
        let cdb = build_test_unit_ready();
        assert_eq!(cdb[0], 0x00);
        assert!(cdb[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_build_read_capacity10() {
        let cdb = build_read_capacity10();
        assert_eq!(cdb[0], 0x25);
    }

    #[test]
    fn test_build_read_capacity16() {
        let alloc_len: u32 = 32;
        let cdb = build_read_capacity16(alloc_len);
        assert_eq!(cdb[0], 0x9E);
        assert_eq!(cdb[1], 0x10);
        let parsed = u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]);
        assert_eq!(parsed, alloc_len);
    }

    #[test]
    fn test_build_read10_lba_and_count() {
        let lba: u32 = 0x1000;
        let count: u16 = 128;
        let cdb = build_read10(lba, count);
        assert_eq!(cdb[0], 0x28);
        let parsed_lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
        assert_eq!(parsed_lba, lba);
        let parsed_count = u16::from_be_bytes([cdb[7], cdb[8]]);
        assert_eq!(parsed_count, count);
    }

    #[test]
    fn test_build_read16_large_lba() {
        let lba: u64 = (u32::MAX as u64) + 1;
        let count: u32 = 1;
        let cdb = build_read16(lba, count);
        assert_eq!(cdb[0], 0x88);
        let parsed_lba = u64::from_be_bytes([
            cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9],
        ]);
        assert_eq!(parsed_lba, lba);
    }

    #[test]
    fn test_build_read_auto_selects_read10() {
        let cdb = build_read(100, 64);
        assert_eq!(cdb[0], 0x28);
    }

    #[test]
    fn test_build_read_auto_selects_read16_large_lba() {
        let lba = (u32::MAX as u64) + 1;
        let cdb = build_read(lba, 1);
        assert_eq!(cdb[0], 0x88);
    }

    #[test]
    fn test_build_read_auto_selects_read16_large_count() {
        let count = (u16::MAX as u32) + 1;
        let cdb = build_read(0, count);
        assert_eq!(cdb[0], 0x88);
    }

    #[test]
    fn test_build_write10() {
        let cdb = build_write10(0x2000, 256);
        assert_eq!(cdb[0], 0x2A);
        let parsed_lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
        assert_eq!(parsed_lba, 0x2000);
        let parsed_count = u16::from_be_bytes([cdb[7], cdb[8]]);
        assert_eq!(parsed_count, 256);
    }

    #[test]
    fn test_build_write_auto_selects() {
        let small = build_write(100, 64);
        assert_eq!(small[0], 0x2A);
        let large = build_write((u32::MAX as u64) + 1, 1);
        assert_eq!(large[0], 0x8A);
    }

    #[test]
    fn test_build_synchronize_cache10() {
        let cdb = build_synchronize_cache10(0x1000, 128);
        assert_eq!(cdb[0], 0x35);
        let parsed_lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
        assert_eq!(parsed_lba, 0x1000);
        let parsed_count = u16::from_be_bytes([cdb[7], cdb[8]]);
        assert_eq!(parsed_count, 128);
    }

    #[test]
    fn test_build_synchronize_cache10_full_flush() {
        let cdb = build_synchronize_cache10(0, 0);
        assert_eq!(cdb[0], 0x35);
        let parsed_lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
        assert_eq!(parsed_lba, 0);
        let parsed_count = u16::from_be_bytes([cdb[7], cdb[8]]);
        assert_eq!(parsed_count, 0);
    }

    #[test]
    fn test_build_inquiry() {
        let alloc_len: u16 = 96;
        let cdb = build_inquiry(alloc_len);
        assert_eq!(cdb[0], 0x12);
        let parsed = u16::from_be_bytes([cdb[3], cdb[4]]);
        assert_eq!(parsed, alloc_len);
    }

    #[test]
    fn test_parse_read_capacity10() {
        let max_lba: u32 = 1023;
        let block_len: u32 = 4096;
        let mut data = [0u8; 8];
        data[0..4].copy_from_slice(&max_lba.to_be_bytes());
        data[4..8].copy_from_slice(&block_len.to_be_bytes());
        let (parsed_lba, parsed_bl) = parse_read_capacity10(&data).unwrap();
        assert_eq!(parsed_lba, max_lba);
        assert_eq!(parsed_bl, block_len);
    }

    #[test]
    fn test_parse_read_capacity10_zero_block_len() {
        let mut data = [0u8; 8];
        data[0..4].copy_from_slice(&1023u32.to_be_bytes());
        // block_len stays 0
        assert!(parse_read_capacity10(&data).is_err());
    }

    #[test]
    fn test_parse_read_capacity16() {
        let max_lba: u64 = 0x0001_0000_0000_FFFF;
        let block_len: u32 = 512;
        let mut data = [0u8; 12];
        data[0..8].copy_from_slice(&max_lba.to_be_bytes());
        data[8..12].copy_from_slice(&block_len.to_be_bytes());
        let (parsed_lba, parsed_bl) = parse_read_capacity16(&data).unwrap();
        assert_eq!(parsed_lba, max_lba);
        assert_eq!(parsed_bl, block_len);
    }

    #[test]
    fn test_parse_sense_data_unit_attention() {
        // Build a sense data buffer with 2-byte iSCSI prefix + fixed-format sense.
        // Total layout: [len_hi, len_lo, response_code, segment, sense_key, info0..info3, ...]
        let mut data = [0u8; 20];
        // bytes 0-1: sense data length (we just put a large enough value)
        data[0] = 0x00;
        data[1] = 0x12; // 18 bytes of sense
        // byte 2: response code 0x70 (current, fixed format)
        data[2] = 0x70;
        // byte 3: segment number (0)
        data[3] = 0x00;
        // byte 4: sense key = 0x06 (UNIT ATTENTION)
        data[4] = 0x06;
        // bytes 5-8: information field
        data[5] = 0x00;
        data[6] = 0x00;
        data[7] = 0x00;
        data[8] = 0x01;
        // bytes 9-13: additional sense length + filler
        data[9] = 0x0A; // additional sense length
        // byte 14: ASC
        data[14] = 0x28;
        // byte 15: ASCQ
        data[15] = 0x00;

        let sense = parse_sense_data(&data).unwrap();
        assert_eq!(sense.sense_key, SenseKey::UnitAttention);
        assert_eq!(sense.asc, 0x28);
        assert_eq!(sense.ascq, 0x00);
        assert_eq!(sense.information, 1);
        assert!(is_unit_attention(&sense));
    }

    #[test]
    fn test_is_retryable() {
        // Busy is retryable regardless of sense
        assert!(is_retryable(ScsiStatus::Busy, None));

        // TaskSetFull is retryable regardless of sense
        assert!(is_retryable(ScsiStatus::TaskSetFull, None));

        // CheckCondition with UnitAttention sense is retryable
        let ua_sense = SenseData {
            sense_key: SenseKey::UnitAttention,
            asc: 0x28,
            ascq: 0x00,
            information: 0,
        };
        assert!(is_retryable(ScsiStatus::CheckCondition, Some(&ua_sense)));

        // CheckCondition without sense is not retryable
        assert!(!is_retryable(ScsiStatus::CheckCondition, None));

        // CheckCondition with non-UA sense is not retryable
        let other_sense = SenseData {
            sense_key: SenseKey::MediumError,
            asc: 0x00,
            ascq: 0x00,
            information: 0,
        };
        assert!(!is_retryable(
            ScsiStatus::CheckCondition,
            Some(&other_sense)
        ));

        // Good is not retryable
        assert!(!is_retryable(ScsiStatus::Good, None));
    }
}
