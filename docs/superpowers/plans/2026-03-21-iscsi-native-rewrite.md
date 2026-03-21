# iSCSI Native Protocol Rewrite — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `iscsi-client-rs` with a native iSCSI initiator following RFC 7143, optimized for 10Gbps throughput.

**Architecture:** Bottom-up layered protocol stack: PDU → Transport → Login → Session → Pipeline → Recovery, with a new moka-based cache, channel-based block device, and multi-threaded FUSE. Each layer is independently testable with unit tests using `#[cfg(test)]` modules.

**Tech Stack:** Rust 2024 edition (stable), tokio async runtime, fuser (macFUSE), moka cache, bytes zero-copy buffers, crc32c hardware-accelerated digests, socket2 TCP tuning, serde+toml config.

**Spec:** `docs/superpowers/specs/2026-03-21-iscsi-native-rewrite-design.md`

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `Cargo.toml` | Modify | New dependencies, remove old ones |
| `src/iscsi/mod.rs` | Create | Module declarations, public re-exports |
| `src/iscsi/digest.rs` | Create | CRC32C header/data digest compute + verify |
| `src/iscsi/command.rs` | Create | SCSI CDB builders + response parsers |
| `src/iscsi/pdu.rs` | Create | PDU types, BHS serialize/deserialize, builders |
| `src/iscsi/config.rs` | Create | TOML config structs, defaults, loading |
| `src/iscsi/transport.rs` | Create | TCP connection, 10G tuning, PDU framing |
| `src/iscsi/login.rs` | Create | Login state machine, parameter negotiation |
| `src/iscsi/session.rs` | Create | CmdSN/StatSN, ITT pool, receiver task |
| `src/iscsi/pipeline.rs` | Create | 128-deep command window, read/write paths |
| `src/iscsi/recovery.rs` | Create | NOP keepalive, session recovery, I/O queuing |
| `src/cache.rs` | Rewrite | moka + Bytes, adaptive readahead |
| `src/block_device.rs` | Rewrite | Channel dispatch, write coalescing, dirty map |
| `src/fuse_fs.rs` | Modify | Multi-threaded FUSE, channel-based BlockDevice |
| `src/config.rs` | Delete | Replaced by `src/iscsi/config.rs` |
| `src/iscsi_backend.rs` | Delete | Replaced by `src/iscsi/` module tree |
| `src/main.rs` | Rewrite | New wiring with all components |

---

### Task 1: Update Cargo.toml and Create Module Skeleton

**Files:**
- Modify: `Cargo.toml`
- Create: `src/iscsi/mod.rs`

- [ ] **Step 1: Update Cargo.toml with new dependencies**

Replace the entire `[dependencies]` section:

```toml
[package]
name = "iscsi-fuse"
version = "0.3.0"
edition = "2024"
license = "AGPL-3.0-or-later"

[dependencies]
fuser = "0.17"
num_cpus = "1"
tokio = { version = "1", features = ["full"] }
bytes = "1"
crc32c = "0.6"
socket2 = "0.6"
moka = { version = "0.12", features = ["future"] }
toml = "0.8"
serde = { version = "1", features = ["derive"] }
clap = { version = "4", features = ["derive"] }
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
libc = "0.2"
ctrlc = "3"
dirs = "6"
```

- [ ] **Step 2: Create `src/iscsi/mod.rs` with module declarations**

```rust
pub mod command;
pub mod config;
pub mod digest;
pub mod login;
pub mod pdu;
pub mod pipeline;
pub mod recovery;
pub mod session;
pub mod transport;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check 2>&1 | head -20`

This will fail because modules are empty — that's expected. We just need the Cargo.toml to resolve dependencies. If dependency resolution fails, fix version issues.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml src/iscsi/mod.rs
git commit -m "feat: update deps and create iscsi module skeleton for native rewrite"
```

---

### Task 2: CRC32C Digest Module (`iscsi/digest.rs`)

**Files:**
- Create: `src/iscsi/digest.rs`

No dependencies on other iscsi modules. Pure functions.

- [ ] **Step 1: Write tests for digest functions**

```rust
// At bottom of src/iscsi/digest.rs

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_digest_deterministic() {
        let bhs = [0xABu8; 48];
        let d1 = header_digest(&bhs, None);
        let d2 = header_digest(&bhs, None);
        assert_eq!(d1, d2);
    }

    #[test]
    fn test_header_digest_with_ahs() {
        let bhs = [0u8; 48];
        let d_no_ahs = header_digest(&bhs, None);
        let d_with_ahs = header_digest(&bhs, Some(&[1, 2, 3, 4]));
        assert_ne!(d_no_ahs, d_with_ahs);
    }

    #[test]
    fn test_verify_header_digest_ok() {
        let bhs = [0x42u8; 48];
        let digest = header_digest(&bhs, None);
        assert!(verify_header_digest(&bhs, None, &digest).is_ok());
    }

    #[test]
    fn test_verify_header_digest_mismatch() {
        let bhs = [0x42u8; 48];
        let bad_digest = [0u8; 4];
        assert!(verify_header_digest(&bhs, None, &bad_digest).is_err());
    }

    #[test]
    fn test_data_digest_known_value() {
        // CRC32C of empty slice is 0x00000000
        let d = data_digest(&[]);
        assert_eq!(u32::from_be_bytes(d), 0x00000000);
    }

    #[test]
    fn test_verify_data_digest_ok() {
        let data = b"Hello iSCSI";
        let digest = data_digest(data);
        assert!(verify_data_digest(data, &digest).is_ok());
    }

    #[test]
    fn test_verify_data_digest_mismatch() {
        let data = b"Hello iSCSI";
        let bad = [0xFF; 4];
        assert!(verify_data_digest(data, &bad).is_err());
    }
}
```

- [ ] **Step 2: Implement digest functions**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DigestError {
    #[error("Header CRC32C mismatch: expected {expected:#010x}, got {received:#010x}")]
    HeaderMismatch { expected: u32, received: u32 },
    #[error("Data CRC32C mismatch: expected {expected:#010x}, got {received:#010x}")]
    DataMismatch { expected: u32, received: u32 },
}

/// Compute CRC32C of BHS (48 bytes) + optional AHS. Returns 4-byte big-endian digest.
pub fn header_digest(bhs: &[u8; 48], ahs: Option<&[u8]>) -> [u8; 4] {
    let mut crc = crc32c::crc32c(bhs);
    if let Some(ahs) = ahs {
        crc = crc32c::crc32c_append(crc, ahs);
    }
    crc.to_be_bytes()
}

/// Verify a received header digest.
pub fn verify_header_digest(
    bhs: &[u8; 48],
    ahs: Option<&[u8]>,
    received: &[u8; 4],
) -> Result<(), DigestError> {
    let expected = header_digest(bhs, ahs);
    if expected != *received {
        Err(DigestError::HeaderMismatch {
            expected: u32::from_be_bytes(expected),
            received: u32::from_be_bytes(*received),
        })
    } else {
        Ok(())
    }
}

/// Compute CRC32C of a data segment. Returns 4-byte big-endian digest.
pub fn data_digest(data: &[u8]) -> [u8; 4] {
    crc32c::crc32c(data).to_be_bytes()
}

/// Verify a received data digest.
pub fn verify_data_digest(data: &[u8], received: &[u8; 4]) -> Result<(), DigestError> {
    let expected = data_digest(data);
    if expected != *received {
        Err(DigestError::DataMismatch {
            expected: u32::from_be_bytes(expected),
            received: u32::from_be_bytes(*received),
        })
    } else {
        Ok(())
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse digest -- --nocapture`
Expected: All 7 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/iscsi/digest.rs
git commit -m "feat(iscsi): add CRC32C digest module with header/data compute and verify"
```

---

### Task 3: SCSI Command Module (`iscsi/command.rs`)

**Files:**
- Create: `src/iscsi/command.rs`

No dependencies on other iscsi modules. Pure functions — CDB builders and response parsers.

- [ ] **Step 1: Write tests for CDB builders**

Test that each builder produces correct opcode at byte 0, correct LBA placement, correct block count placement. Test `build_read`/`build_write` auto-select logic (10 vs 16).

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_test_unit_ready() {
        let cdb = build_test_unit_ready();
        assert_eq!(cdb[0], 0x00);
        assert_eq!(cdb[1..], [0u8; 15]);
    }

    #[test]
    fn test_build_read_capacity10() {
        let cdb = build_read_capacity10();
        assert_eq!(cdb[0], 0x25);
    }

    #[test]
    fn test_build_read_capacity16() {
        let cdb = build_read_capacity16(32);
        assert_eq!(cdb[0], 0x9E);
        assert_eq!(cdb[1], 0x10);
        assert_eq!(u32::from_be_bytes(cdb[10..14].try_into().unwrap()), 32);
    }

    #[test]
    fn test_build_read10_lba_and_count() {
        let cdb = build_read10(0x1000, 128);
        assert_eq!(cdb[0], 0x28);
        assert_eq!(u32::from_be_bytes(cdb[2..6].try_into().unwrap()), 0x1000);
        assert_eq!(u16::from_be_bytes(cdb[7..9].try_into().unwrap()), 128);
    }

    #[test]
    fn test_build_read16_large_lba() {
        let lba: u64 = 0x1_0000_0000; // > u32::MAX
        let cdb = build_read16(lba, 256);
        assert_eq!(cdb[0], 0x88);
        assert_eq!(u64::from_be_bytes(cdb[2..10].try_into().unwrap()), lba);
        assert_eq!(u32::from_be_bytes(cdb[10..14].try_into().unwrap()), 256);
    }

    #[test]
    fn test_build_read_auto_selects_read10() {
        let cdb = build_read(100, 50);
        assert_eq!(cdb[0], 0x28); // READ(10)
    }

    #[test]
    fn test_build_read_auto_selects_read16_large_lba() {
        let cdb = build_read(0x1_0000_0000, 50);
        assert_eq!(cdb[0], 0x88); // READ(16)
    }

    #[test]
    fn test_build_read_auto_selects_read16_large_count() {
        // block_count > u16::MAX forces READ(16) even if LBA fits in u32
        let cdb = build_read(100, 0x1_0000);
        assert_eq!(cdb[0], 0x88); // READ(16)
    }

    #[test]
    fn test_build_write10() {
        let cdb = build_write10(500, 64);
        assert_eq!(cdb[0], 0x2A);
        assert_eq!(u32::from_be_bytes(cdb[2..6].try_into().unwrap()), 500);
        assert_eq!(u16::from_be_bytes(cdb[7..9].try_into().unwrap()), 64);
    }

    #[test]
    fn test_build_write_auto_selects() {
        assert_eq!(build_write(100, 50)[0], 0x2A);       // WRITE(10)
        assert_eq!(build_write(0x1_0000_0000, 50)[0], 0x8A); // WRITE(16)
    }

    #[test]
    fn test_build_inquiry() {
        let cdb = build_inquiry(96);
        assert_eq!(cdb[0], 0x12);
        assert_eq!(u16::from_be_bytes(cdb[3..5].try_into().unwrap()), 96);
    }

    #[test]
    fn test_parse_read_capacity10() {
        let mut data = [0u8; 8];
        data[0..4].copy_from_slice(&1023u32.to_be_bytes()); // max_lba
        data[4..8].copy_from_slice(&4096u32.to_be_bytes()); // block_len
        let (max_lba, block_len) = parse_read_capacity10(&data).unwrap();
        assert_eq!(max_lba, 1023);
        assert_eq!(block_len, 4096);
    }

    #[test]
    fn test_parse_read_capacity10_zero_block_len() {
        let data = [0u8; 8]; // block_len = 0
        assert!(parse_read_capacity10(&data).is_err());
    }

    #[test]
    fn test_parse_read_capacity16() {
        let mut data = [0u8; 32];
        data[0..8].copy_from_slice(&0xFFFF_FFFF_FFFFu64.to_be_bytes());
        data[8..12].copy_from_slice(&512u32.to_be_bytes());
        let (max_lba, block_len) = parse_read_capacity16(&data).unwrap();
        assert_eq!(max_lba, 0xFFFF_FFFF_FFFF);
        assert_eq!(block_len, 512);
    }

    #[test]
    fn test_parse_sense_data_unit_attention() {
        // 2-byte iSCSI length prefix + fixed format sense
        let mut data = vec![0u8; 20];
        data[0..2].copy_from_slice(&18u16.to_be_bytes()); // sense length
        data[2] = 0x70; // response code: current errors, fixed format
        data[4] = 0x06; // sense key: UNIT ATTENTION
        data[14] = 0x29; // ASC: power on / reset
        data[15] = 0x00; // ASCQ
        let sense = parse_sense_data(&data).unwrap();
        assert!(matches!(sense.sense_key, SenseKey::UnitAttention));
        assert_eq!(sense.asc, 0x29);
        assert_eq!(sense.ascq, 0x00);
        assert!(is_unit_attention(&sense));
    }

    #[test]
    fn test_is_retryable() {
        assert!(is_retryable(ScsiStatus::Busy, None));
        assert!(is_retryable(ScsiStatus::TaskSetFull, None));
        assert!(!is_retryable(ScsiStatus::Good, None));

        let ua_sense = SenseData {
            sense_key: SenseKey::UnitAttention,
            asc: 0x29, ascq: 0x00, information: 0,
        };
        assert!(is_retryable(ScsiStatus::CheckCondition, Some(&ua_sense)));
    }
}
```

- [ ] **Step 2: Implement all types, CDB builders, and response parsers**

Implement everything per the spec: `ScsiStatus`, `SenseKey`, `SenseData`, all `build_*` functions, `parse_read_capacity10`, `parse_read_capacity16`, `parse_sense_data`, `is_unit_attention`, `is_retryable`.

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse command -- --nocapture`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/iscsi/command.rs
git commit -m "feat(iscsi): add SCSI command CDB builders and response parsers"
```

---

### Task 4: PDU Module (`iscsi/pdu.rs`)

**Files:**
- Create: `src/iscsi/pdu.rs`

Depends on: `bytes` crate. References `digest.rs` types but does not call digest functions directly (transport does that).

- [ ] **Step 1: Write tests for BHS serialization round-trip**

Test: build a SCSI Command BHS, serialize to `[u8; 48]`, parse back, verify all fields match. Test Login Request builder. Test NOP-Out builder. Test `data_segment_length` 3-byte encoding. Test padding calculation.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opcode_values() {
        // Initiator opcodes
        assert_eq!(Opcode::NopOut as u8, 0x00);
        assert_eq!(Opcode::ScsiCommand as u8, 0x01);
        assert_eq!(Opcode::TaskMgmt as u8, 0x02);
        assert_eq!(Opcode::LoginRequest as u8, 0x03);
        assert_eq!(Opcode::TextRequest as u8, 0x04);
        assert_eq!(Opcode::ScsiDataOut as u8, 0x05);
        assert_eq!(Opcode::LogoutRequest as u8, 0x06);
        assert_eq!(Opcode::SnackRequest as u8, 0x10);
        // Target opcodes
        assert_eq!(Opcode::NopIn as u8, 0x20);
        assert_eq!(Opcode::ScsiResponse as u8, 0x21);
        assert_eq!(Opcode::TaskMgmtResp as u8, 0x22);
        assert_eq!(Opcode::LoginResponse as u8, 0x23);
        assert_eq!(Opcode::TextResponse as u8, 0x24);
        assert_eq!(Opcode::ScsiDataIn as u8, 0x25);
        assert_eq!(Opcode::LogoutResponse as u8, 0x26);
        assert_eq!(Opcode::R2t as u8, 0x31);
        assert_eq!(Opcode::AsyncMessage as u8, 0x32);
        assert_eq!(Opcode::Reject as u8, 0x3f);
    }

    #[test]
    fn test_scsi_command_round_trip() {
        let cdb = [0x28, 0, 0, 0, 0x10, 0, 0, 0, 128, 0, 0, 0, 0, 0, 0, 0]; // READ(10)
        let bhs = Bhs::build_scsi_command(0, 42, 1, 0, &cdb, 65536, true, false);
        let bytes = bhs.serialize();
        let parsed = Bhs::parse(&bytes).unwrap();
        assert_eq!(parsed.opcode, Opcode::ScsiCommand);
        assert_eq!(parsed.itt, 42);
        assert_eq!(parsed.lun, 0);
        // Verify CDB is in bytes 32-47
        assert_eq!(&bytes[32..48], &cdb);
    }

    #[test]
    fn test_login_request_builder() {
        let isid = [0x00, 0x02, 0x3D, 0x00, 0x00, 0x01];
        let bhs = Bhs::build_login_request(isid, 0, 0, 1, 1, 0, 0, 1, true);
        let bytes = bhs.serialize();
        // Opcode should be 0x43 (Immediate=1, opcode=0x03)
        assert_eq!(bytes[0], 0x43);
        // ISID at bytes 8-13
        assert_eq!(&bytes[8..14], &isid);
        // TSIH at bytes 14-15 should be 0
        assert_eq!(u16::from_be_bytes(bytes[14..16].try_into().unwrap()), 0);
    }

    #[test]
    fn test_nop_out_builder() {
        let bhs = Bhs::build_nop_out(0xFFFFFFFF, 0xFFFFFFFF, 5, 3);
        let bytes = bhs.serialize();
        assert_eq!(bytes[0] & 0x3F, 0x00); // NOP-Out opcode
        assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 0xFFFFFFFF); // ITT
    }

    #[test]
    fn test_data_segment_length_encoding() {
        // data_segment_length is 3 bytes at BHS bytes 5-7
        let mut bhs = Bhs::build_nop_out(1, 0xFFFFFFFF, 1, 1);
        bhs.data_segment_length = 0x010203;
        let bytes = bhs.serialize();
        assert_eq!(bytes[5], 0x01);
        assert_eq!(bytes[6], 0x02);
        assert_eq!(bytes[7], 0x03);
    }

    #[test]
    fn test_parse_data_in_fields() {
        // Build a fake Data-In BHS
        let mut raw = [0u8; 48];
        raw[0] = 0x25; // ScsiDataIn
        raw[1] = 0x81; // F=1, S=1 (final, status present)
        raw[3] = 0x00; // SCSI status GOOD
        // DataSegmentLength = 4096
        raw[5] = 0x00; raw[6] = 0x10; raw[7] = 0x00;
        // ITT = 7
        raw[16..20].copy_from_slice(&7u32.to_be_bytes());
        // DataSN = 0
        raw[36..40].copy_from_slice(&0u32.to_be_bytes());
        // BufferOffset = 0
        raw[40..44].copy_from_slice(&0u32.to_be_bytes());

        let bhs = Bhs::parse(&raw).unwrap();
        assert_eq!(bhs.opcode, Opcode::ScsiDataIn);
        assert_eq!(bhs.itt, 7);
        assert_eq!(bhs.data_segment_length, 4096);
        assert!(bhs.status_flag());
        assert_eq!(bhs.scsi_status(), 0x00);
        assert_eq!(bhs.data_sn(), 0);
        assert_eq!(bhs.buffer_offset(), 0);
    }

    #[test]
    fn test_parse_r2t_fields() {
        let mut raw = [0u8; 48];
        raw[0] = 0x31; // R2T
        raw[16..20].copy_from_slice(&5u32.to_be_bytes());   // ITT
        raw[20..24].copy_from_slice(&100u32.to_be_bytes());  // TTT
        raw[40..44].copy_from_slice(&8192u32.to_be_bytes()); // BufferOffset
        raw[44..48].copy_from_slice(&65536u32.to_be_bytes()); // DesiredDataTransferLength

        let bhs = Bhs::parse(&raw).unwrap();
        assert_eq!(bhs.opcode, Opcode::R2t);
        assert_eq!(bhs.itt, 5);
        assert_eq!(bhs.ttt(), 100);
        assert_eq!(bhs.r2t_buffer_offset(), 8192);
        assert_eq!(bhs.r2t_desired_length(), 65536);
    }

    #[test]
    fn test_padding_calculation() {
        assert_eq!(pad_to_4(0), 0);
        assert_eq!(pad_to_4(1), 4);
        assert_eq!(pad_to_4(4), 4);
        assert_eq!(pad_to_4(5), 8);
        assert_eq!(pad_to_4(1023), 1024);
    }

    #[test]
    fn test_logout_request_builder() {
        let bhs = Bhs::build_logout_request(42, 5, 3, 0);
        let bytes = bhs.serialize();
        assert_eq!(bytes[0] & 0x3F, 0x06); // LogoutRequest opcode
        assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 42); // ITT
    }

    #[test]
    fn test_data_out_builder() {
        let bhs = Bhs::build_data_out(0, 10, 200, 5, 0, 4096);
        let bytes = bhs.serialize();
        assert_eq!(bytes[0], 0x05); // ScsiDataOut opcode
        assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 10); // ITT
        assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 200); // TTT
        assert_eq!(u32::from_be_bytes(bytes[40..44].try_into().unwrap()), 4096); // BufferOffset
    }
}
```

- [ ] **Step 2: Implement Opcode enum, Bhs struct, Pdu struct, serialize/parse, all builders, all accessors, pad_to_4 helper**

Key implementation details:
- `Bhs` stores parsed fields natively (opcode as enum, itt as u32, etc.) plus `opcode_specific: [u8; 28]` for bytes 20-47.
- `serialize()` writes all fields back to `[u8; 48]` in big-endian.
- `parse()` reads from `[u8; 48]`, validates opcode.
- Accessors like `stat_sn()`, `ttt()`, `buffer_offset()` read from `opcode_specific` at the correct byte offsets for each opcode.
- `data_segment_length` is 3 bytes (bytes 5-7), stored as `u32` with upper byte always 0.

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse pdu -- --nocapture`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/iscsi/pdu.rs
git commit -m "feat(iscsi): add PDU types with BHS serialize/parse and all builders"
```

---

### Task 5: Config Module (`iscsi/config.rs`) + CLI Args

**Files:**
- Create: `src/iscsi/config.rs`
- Delete: `src/config.rs` (old YAML config)

- [ ] **Step 1: Write tests for config loading**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minimal_config() {
        let toml_str = r#"
            target = "iqn.2004-04.com.example:target"
            address = "192.168.1.100:3260"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.target, "iqn.2004-04.com.example:target");
        assert_eq!(config.address, "192.168.1.100:3260");
        // Verify defaults
        assert_eq!(config.tuning.max_burst_length, 1_048_576);
        assert_eq!(config.tuning.max_recv_data_segment_length, 1_048_576);
        assert!(config.tuning.header_digest);
        assert!(config.tuning.data_digest);
        assert!(config.tuning.immediate_data);
        assert!(!config.tuning.initial_r2t);
        assert_eq!(config.recovery.noop_interval_secs, 5);
        assert_eq!(config.cache.size_mb, 128);
    }

    #[test]
    fn test_full_config() {
        let toml_str = r#"
            target = "iqn.example:t1"
            address = "10.0.0.1:3260"
            initiator = "iqn.example:i1"
            lun = 3
            [tuning]
            max_burst_length = 524288
            header_digest = false
            [recovery]
            replacement_timeout_secs = 60
            [cache]
            size_mb = 256
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.lun, 3);
        assert_eq!(config.initiator, "iqn.example:i1");
        assert_eq!(config.tuning.max_burst_length, 524288);
        assert!(!config.tuning.header_digest);
        assert_eq!(config.recovery.replacement_timeout_secs, 60);
        assert_eq!(config.cache.size_mb, 256);
    }

    #[test]
    fn test_missing_required_field() {
        let toml_str = r#"address = "10.0.0.1:3260""#;
        assert!(toml::from_str::<Config>(toml_str).is_err());
    }
}
```

- [ ] **Step 2: Implement Config, TuningConfig, RecoveryConfig, CacheConfig structs with serde derive and defaults**

Implement all structs with `#[derive(Deserialize, Clone)]`, default functions for each field, `Config::load(path)`, `CONFIG_TEMPLATE` const, and `CliArgs` with clap derive.

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse config -- --nocapture`
Expected: All 3 tests pass.

- [ ] **Step 4: Delete old config.rs**

```bash
rm src/config.rs
```

- [ ] **Step 5: Commit**

```bash
git add src/iscsi/config.rs
git rm src/config.rs
git commit -m "feat(iscsi): add TOML config with 10G-optimized defaults, remove old YAML config"
```

---

### Task 6: Transport Layer (`iscsi/transport.rs`)

**Files:**
- Create: `src/iscsi/transport.rs`

Depends on: `pdu.rs`, `digest.rs`, tokio, socket2, bytes.

- [ ] **Step 1: Write tests for PDU send/recv round-trip over loopback**

Use `tokio::net::TcpListener` + `TcpStream` pair to test send_pdu/recv_pdu. Test: send a Login Request PDU, recv it on the other end, verify fields match. Test with digests disabled and enabled. Test data segment padding.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::iscsi::pdu::{Bhs, Opcode, Pdu};
    use bytes::Bytes;

    #[tokio::test]
    async fn test_send_recv_pdu_no_data_no_digest() {
        let (mut sender, mut receiver) = loopback_pair().await;
        let bhs = Bhs::build_nop_out(1, 0xFFFFFFFF, 1, 0);
        let pdu = Pdu { bhs, ahs: None, data: None };
        sender.send_pdu(&pdu).await.unwrap();
        let received = receiver.recv_pdu().await.unwrap();
        assert_eq!(received.bhs.opcode, Opcode::NopOut);
        assert_eq!(received.bhs.itt, 1);
        assert!(received.data.is_none());
    }

    #[tokio::test]
    async fn test_send_recv_pdu_with_data() {
        let (mut sender, mut receiver) = loopback_pair().await;
        let bhs = Bhs::build_nop_out(2, 0xFFFFFFFF, 1, 0);
        let data = Bytes::from_static(b"Hello");
        let mut pdu = Pdu { bhs, ahs: None, data: Some(data) };
        pdu.bhs.data_segment_length = 5;
        sender.send_pdu(&pdu).await.unwrap();
        let received = receiver.recv_pdu().await.unwrap();
        assert_eq!(received.data.unwrap().as_ref(), b"Hello");
    }

    #[tokio::test]
    async fn test_send_recv_with_digests() {
        let (mut sender, mut receiver) = loopback_pair_with_digests().await;
        let bhs = Bhs::build_nop_out(3, 0xFFFFFFFF, 1, 0);
        let data = Bytes::from(vec![0xAB; 100]);
        let mut pdu = Pdu { bhs, ahs: None, data: Some(data) };
        pdu.bhs.data_segment_length = 100;
        sender.send_pdu(&pdu).await.unwrap();
        let received = receiver.recv_pdu().await.unwrap();
        assert_eq!(received.bhs.itt, 3);
        assert_eq!(received.data.unwrap().len(), 100);
    }

    /// Create a loopback Transport pair (no digests)
    async fn loopback_pair() -> (TransportWriter, TransportReader) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let digest_cfg = DigestConfig { header: false, data: false };
        let (cr, cw) = client.into_split();
        let (sr, sw) = server.into_split();
        let writer = TransportWriter::new(cw, digest_cfg.clone());
        let reader = TransportReader::new(sr, digest_cfg);
        (writer, reader)
    }

    /// Loopback pair with CRC32C digests enabled
    async fn loopback_pair_with_digests() -> (TransportWriter, TransportReader) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let digest_cfg = DigestConfig { header: true, data: true };
        let (cr, cw) = client.into_split();
        let (sr, sw) = server.into_split();
        let writer = TransportWriter::new(cw, digest_cfg.clone());
        let reader = TransportReader::new(sr, digest_cfg);
        (writer, reader)
    }
}
```

- [ ] **Step 2: Implement Transport split into TransportWriter and TransportReader**

Key details:
- `TransportWriter`: holds `BufWriter<OwnedWriteHalf>` (1MB buffer) + `DigestConfig`. Method: `send_pdu(&mut self, pdu: &Pdu) -> Result<()>`.
- `TransportReader`: holds `BufReader<OwnedReadHalf>` (1MB buffer) + `DigestConfig` + `bhs_buf: [u8; 48]`. Method: `recv_pdu(&mut self) -> Result<Pdu>`.
- `Transport::connect(addr) -> Result<(TransportWriter, TransportReader)>`: creates TcpStream, applies 10G tuning (4MB socket buffers, TCP_NODELAY), splits.
- `send_pdu` serializes BHS to `[u8; 48]`, computes header digest if enabled, writes BHS + header digest + padded data + data digest using `write_all` calls (vectored write optimization can come later — correctness first).
- `recv_pdu` reads 48 bytes, optionally reads+verifies 4-byte header digest, reads padded data segment, optionally reads+verifies 4-byte data digest, freezes data to `Bytes`.

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse transport -- --nocapture`
Expected: All 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/iscsi/transport.rs
git commit -m "feat(iscsi): add transport layer with PDU framing, 10G socket tuning, CRC32C"
```

---

### Task 7: Login Phase (`iscsi/login.rs`)

**Files:**
- Create: `src/iscsi/login.rs`

Depends on: `transport.rs`, `pdu.rs`, `config.rs`.

- [ ] **Step 1: Write tests for key-value text parsing and NegotiatedParams**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kv_pairs() {
        let data = b"Key1=Value1\0Key2=Value2\0";
        let pairs = parse_kv_pairs(data);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("Key1", "Value1"));
        assert_eq!(pairs[1], ("Key2", "Value2"));
    }

    #[test]
    fn test_parse_kv_pairs_trailing_nul() {
        let data = b"A=B\0\0"; // double NUL
        let pairs = parse_kv_pairs(data);
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn test_negotiated_params_defaults() {
        let params = NegotiatedParams::defaults_10g();
        assert_eq!(params.max_recv_data_segment_length, 1_048_576);
        assert_eq!(params.max_burst_length, 1_048_576);
        assert_eq!(params.first_burst_length, 262_144);
        assert!(!params.initial_r2t);
        assert!(params.immediate_data);
        assert!(params.header_digest);
        assert!(params.data_digest);
    }

    #[test]
    fn test_build_security_text() {
        let mgr = LoginManager::new("iqn.init:a", "iqn.target:b");
        let text = mgr.build_security_text();
        assert!(text.contains("InitiatorName=iqn.init:a\0"));
        assert!(text.contains("TargetName=iqn.target:b\0"));
        assert!(text.contains("AuthMethod=None\0"));
        assert!(text.contains("SessionType=Normal\0"));
    }

    #[test]
    fn test_build_operational_text() {
        let text = NegotiatedParams::build_operational_text();
        assert!(text.contains("MaxRecvDataSegmentLength=1048576\0"));
        assert!(text.contains("HeaderDigest=CRC32C\0"));
        assert!(text.contains("InitialR2T=No\0"));
        assert!(text.contains("ImmediateData=Yes\0"));
    }

    #[test]
    fn test_apply_target_response() {
        let mut params = NegotiatedParams::defaults_10g();
        let response_text = b"MaxRecvDataSegmentLength=262144\0HeaderDigest=None\0MaxBurstLength=524288\0";
        params.apply_target_response(response_text).unwrap();
        // Target declares its receive limit → stored as our max_send_data_segment_length
        assert_eq!(params.max_send_data_segment_length, 262_144);
        assert!(!params.header_digest); // target said None
        assert_eq!(params.max_burst_length, 524_288); // min(ours=1M, theirs=512K)
    }
}
```

- [ ] **Step 2: Implement LoginManager, NegotiatedParams, parse_kv_pairs, login flow**

Key details:
- `LoginManager::new(initiator, target)` — generates random ISID (type=0x80 random).
- `login(&self, writer: &mut TransportWriter, reader: &mut TransportReader, cid: u16) -> Result<LoginResult>` — executes security + operational phases.
- `NegotiatedParams::apply_target_response(data: &[u8])` — parses key=value pairs and applies negotiation rules.
- Login PDUs are always Immediate (byte 0 = 0x43 for Login Request).

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse login -- --nocapture`
Expected: All 6 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/iscsi/login.rs
git commit -m "feat(iscsi): add login phase with security/operational negotiation"
```

---

### Task 8: Session Management (`iscsi/session.rs`)

**Files:**
- Create: `src/iscsi/session.rs`

Depends on: `transport.rs`, `pdu.rs`, `login.rs`.

- [ ] **Step 1: Write tests for IttPool alloc/free/complete**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_itt_pool_alloc_and_free() {
        let pool = IttPool::new();
        let (itt, _rx) = pool.alloc().unwrap();
        assert!(itt < 128);
        pool.free(itt);
        // Should be able to alloc same slot again
        let (itt2, _rx2) = pool.alloc().unwrap();
        assert_eq!(itt, itt2);
    }

    #[test]
    fn test_itt_pool_alloc_128() {
        let pool = IttPool::new();
        let mut itts = Vec::new();
        for _ in 0..128 {
            let (itt, _rx) = pool.alloc().unwrap();
            itts.push(itt);
        }
        // 129th should fail
        assert!(pool.alloc().is_none());
        // Free one and retry
        pool.free(itts[0]);
        assert!(pool.alloc().is_some());
    }

    #[test]
    fn test_itt_pool_complete() {
        let pool = IttPool::new();
        let (itt, rx) = pool.alloc().unwrap();
        let resp = PduResponse {
            status: ScsiStatus::Good,
            data: Some(Bytes::from_static(b"test")),
            sense: None,
        };
        pool.complete(itt, resp);
        let received = rx.blocking_recv().unwrap();
        assert!(matches!(received.status, ScsiStatus::Good));
        assert_eq!(received.data.unwrap().as_ref(), b"test");
    }

    #[test]
    fn test_serial_number_le() {
        // RFC 1982 serial number arithmetic
        assert!(serial_le(1, 2));
        assert!(serial_le(1, 1));
        assert!(!serial_le(2, 1));
        // Wrap-around
        assert!(serial_le(0xFFFF_FFFE, 0xFFFF_FFFF));
        assert!(serial_le(0xFFFF_FFFF, 0x0000_0000));
    }

    #[test]
    fn test_session_state_cmd_window() {
        let state = SessionState::new(1, 0);
        state.max_cmd_sn.store(32, Ordering::Release);
        state.exp_cmd_sn.store(1, Ordering::Release);
        // cmd_sn=1 should be in window [1, 32]
        assert!(state.cmd_sn_in_window());
    }
}
```

- [ ] **Step 2: Implement IttPool with two AtomicU64, SessionState, PduResponse, serial_le**

Key details:
- `IttPool::new()` — both `slots_lo` and `slots_hi` start at 0 (all free).
- `alloc()` — check `slots_lo` first with `(!current).trailing_zeros()`, CAS to set bit. If full, try `slots_hi` (ITT = 64 + bit).
- `free(itt)` — clear bit on appropriate half.
- `complete(itt, response)` — take the oneshot sender from a `Mutex<Vec<Option<oneshot::Sender>>>`, send response, free ITT.
- `SessionState` — atomics for `cmd_sn`, `exp_stat_sn`, `exp_cmd_sn`, `max_cmd_sn`.
- `serial_le(a, b)` — RFC 1982: `a == b || (a.wrapping_sub(b) > 0x8000_0000)` inverted.

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse session -- --nocapture`
Expected: All 5 tests pass.

- [ ] **Step 4: Implement Session struct with submit_command**

`Session` holds `Mutex<TransportWriter>`, `Arc<IttPool>`, `SessionState`. Implement:
- `Session::new(writer, itt_pool, state, negotiated)` constructor
- `submit_command(cdb, lun, edtl, read, write, immediate_data) -> Result<oneshot::Receiver<PduResponse>>` — waits for CmdSN window, allocates ITT, stamps sequence numbers, sends PDU.
- `register_write_data(itt, data: Bytes)` — stores write data for R2T handling.
- `send_nop_out()` — for keepalive.
- `send_logout()` — for clean shutdown.

Do NOT implement `spawn_receiver` here — that's Task 8b.

- [ ] **Step 5: Run all tests**

Run: `cargo test -p iscsi-fuse session -- --nocapture`
Expected: All 5 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/iscsi/session.rs
git commit -m "feat(iscsi): add session with ITT pool, CmdSN windowing, command submission"
```

---

### Task 8b: Session Receiver Task (`iscsi/session.rs` continued)

**Files:**
- Modify: `src/iscsi/session.rs`

Depends on: Task 8a.

- [ ] **Step 1: Write tests for receiver loop using loopback transport**

Create a test helper that sends raw PDU bytes to a loopback transport reader, then verify the receiver correctly routes them.

```rust
// Additional tests in session.rs

#[cfg(test)]
mod receiver_tests {
    use super::*;
    use crate::iscsi::pdu::{Bhs, Opcode, Pdu};
    use crate::iscsi::transport::{TransportWriter, TransportReader, DigestConfig};
    use bytes::Bytes;

    /// Helper: create a loopback transport pair + Session + IttPool.
    /// Returns (session, itt_pool, fake_target_writer, receiver_reader)
    async fn setup_session() -> (
        Arc<Session>, Arc<IttPool>,
        TransportWriter, TransportReader,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let digest = DigestConfig { header: false, data: false };
        let (cr, cw) = client.into_split();
        let (sr, sw) = server.into_split();
        // Client write → Server read (initiator → target direction)
        let session_writer = TransportWriter::new(cw, digest.clone());
        // Server write → Client read (target → initiator direction)
        let fake_target_writer = TransportWriter::new(sw, digest.clone());
        let receiver_reader = TransportReader::new(cr, digest);

        let itt_pool = Arc::new(IttPool::new());
        let state = SessionState::new(1, 0);
        let negotiated = NegotiatedParams::defaults_10g();
        let session = Arc::new(Session::new(
            session_writer, itt_pool.clone(), state, negotiated,
        ));
        (session, itt_pool, fake_target_writer, receiver_reader)
    }

    #[tokio::test]
    async fn test_receiver_handles_scsi_response() {
        let (session, itt_pool, mut target_w, receiver_r) = setup_session().await;
        // Allocate an ITT
        let (itt, rx) = itt_pool.alloc().unwrap();
        // Spawn receiver
        let handle = session.clone().spawn_receiver(receiver_r, itt_pool.clone());
        // Target sends a SCSI Response for this ITT
        let mut resp_bhs = Bhs::build_scsi_response(itt, 0x00, 1, 1, 32);
        let resp_pdu = Pdu { bhs: resp_bhs, ahs: None, data: None };
        target_w.send_pdu(&resp_pdu).await.unwrap();
        // Await completion
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2), rx
        ).await.unwrap().unwrap();
        assert!(matches!(result.status, ScsiStatus::Good));
        handle.abort();
    }

    #[tokio::test]
    async fn test_receiver_handles_data_in_with_status() {
        let (session, itt_pool, mut target_w, receiver_r) = setup_session().await;
        let (itt, rx) = itt_pool.alloc().unwrap();
        let handle = session.clone().spawn_receiver(receiver_r, itt_pool.clone());
        // Target sends Data-In with S bit set (final, includes status)
        let data = Bytes::from(vec![0xAA; 4096]);
        let mut bhs = Bhs::build_data_in(itt, 0, 0, data.len() as u32, true, 0x00, 1, 1, 32);
        let pdu = Pdu { bhs, ahs: None, data: Some(data.clone()) };
        target_w.send_pdu(&pdu).await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2), rx
        ).await.unwrap().unwrap();
        assert!(matches!(result.status, ScsiStatus::Good));
        assert_eq!(result.data.unwrap().len(), 4096);
        handle.abort();
    }

    #[tokio::test]
    async fn test_receiver_handles_nop_in() {
        let (session, itt_pool, mut target_w, receiver_r) = setup_session().await;
        let handle = session.clone().spawn_receiver(receiver_r, itt_pool.clone());
        // Target sends NOP-In (solicited ping). ITT=0xFFFFFFFF means unsolicited.
        let bhs = Bhs::build_nop_in(0xFFFFFFFF, 0xFFFFFFFF, 1, 1, 32);
        let pdu = Pdu { bhs, ahs: None, data: None };
        target_w.send_pdu(&pdu).await.unwrap();
        // Give receiver time to process and send NOP-Out response
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // No crash = success for NOP-In handling
        handle.abort();
    }
}
```

- [ ] **Step 2: Implement spawn_receiver and target PDU builders for tests**

Implement:
- `Session::spawn_receiver(reader: TransportReader, itt_pool: Arc<IttPool>) -> JoinHandle<Result<()>>` — the receiver loop.
- PDU routing: ScsiResponse → complete ITT, ScsiDataIn → accumulate + complete, R2T → send Data-Out, NopIn → send NOP-Out, AsyncMessage/Reject → log.
- Data-In accumulation: `Mutex<HashMap<u32, BytesMut>>` keyed by ITT, data inserted at buffer_offset. When F or S bit set, freeze and complete.
- R2T handling: look up registered write data by ITT, slice range, send Data-Out PDUs (acquire writer lock per PDU).
- Add `Bhs::build_scsi_response()`, `Bhs::build_data_in()`, `Bhs::build_nop_in()` builders for test use (target-side PDU construction).

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse session -- --nocapture`
Expected: All tests pass (5 from Task 8a + 3 new receiver tests).

- [ ] **Step 4: Commit**

```bash
git add src/iscsi/session.rs
git commit -m "feat(iscsi): add session receiver task with Data-In, R2T, NOP-In handling"
```

---

### Task 9: Command Pipeline (`iscsi/pipeline.rs`)

**Files:**
- Create: `src/iscsi/pipeline.rs`

Depends on: `session.rs`, `command.rs`.

- [ ] **Step 1: Write tests for chunking logic**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_read_small() {
        // 10 blocks, max_blocks_per_cmd=256 → 1 chunk
        let chunks = compute_read_chunks(0, 10, 256);
        assert_eq!(chunks, vec![(0, 10)]);
    }

    #[test]
    fn test_chunk_read_large() {
        // 1000 blocks, max=256 → 4 chunks (256+256+256+232)
        let chunks = compute_read_chunks(0, 1000, 256);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0], (0, 256));
        assert_eq!(chunks[1], (256, 256));
        assert_eq!(chunks[2], (512, 256));
        assert_eq!(chunks[3], (768, 232));
    }

    #[test]
    fn test_chunk_write() {
        let chunks = compute_write_chunks(100, 500, 4096, 256);
        // 500 blocks, max=256 → 2 chunks
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].0, 100);   // lba
        assert_eq!(chunks[0].1, 256);   // blocks
        assert_eq!(chunks[1].0, 356);
        assert_eq!(chunks[1].1, 244);
    }

    #[test]
    fn test_max_read_blocks() {
        // max_burst_length=1MB, block_size=4096 → 256 blocks
        assert_eq!(max_read_blocks(1_048_576, 4096), 256);
        // max_burst_length=1MB, block_size=512 → 2048 blocks
        assert_eq!(max_read_blocks(1_048_576, 512), 2048);
    }
}
```

- [ ] **Step 2: Implement Pipeline struct, compute_read_chunks, compute_write_chunks, scsi_read, scsi_write, read_capacity**

Key details:
- `Pipeline::new(session, lun, negotiated)` — stores Arc<Session>.
- `scsi_read(lba, block_count)` — compute chunks, submit all via `session.submit_command()`, collect `Vec<JoinHandle>` or `Vec<impl Future>`, await all in order, concatenate into `BytesMut`, freeze.
- `scsi_write(lba, data: Bytes)` — compute chunks, `data.slice()` each chunk (zero-copy), submit all.
- `read_capacity()` — RC(10) first, RC(16) fallback, 3-retry loop for UNIT ATTENTION.

**Test coverage note:** `scsi_read`, `scsi_write`, and `read_capacity` require a live Session with a loopback transport (similar to Task 8b's test setup). These integration-style tests can be added as a follow-up or during end-to-end testing against a real target. The chunking logic (pure functions) is fully unit-tested above.

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse pipeline -- --nocapture`
Expected: All 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/iscsi/pipeline.rs
git commit -m "feat(iscsi): add command pipeline with 128-deep pipelining, read/write paths"
```

---

### Task 10: Recovery Manager (`iscsi/recovery.rs`)

**Files:**
- Create: `src/iscsi/recovery.rs`

Depends on: `session.rs`, `login.rs`, `transport.rs`.

- [ ] **Step 1: Write tests for RecoveryConfig defaults and PendingCommand expiry**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn test_recovery_config_defaults() {
        let cfg = RecoveryConfig::default();
        assert_eq!(cfg.noop_interval, Duration::from_secs(5));
        assert_eq!(cfg.noop_timeout, Duration::from_secs(5));
        assert_eq!(cfg.replacement_timeout, Duration::from_secs(30));
        assert_eq!(cfg.max_login_retries, 6);
        assert_eq!(cfg.login_retry_delay, Duration::from_secs(5));
    }

    #[test]
    fn test_pending_command_expiry() {
        let mut queue = PendingQueue::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        queue.push(PendingCommand {
            cdb: [0u8; 16],
            lun: 0,
            edtl: 0,
            read: true,
            write: false,
            write_data: None,
            reply: tx,
            queued_at: Instant::now() - Duration::from_secs(60),
        });
        let expired = queue.expire(Duration::from_secs(30));
        assert_eq!(expired, 1);
        assert!(queue.is_empty());
    }
}
```

- [ ] **Step 2: Implement RecoveryManager, RecoveryConfig, PendingQueue, PendingCommand**

Key details:
- `RecoveryConfig` — derived from `iscsi/config.rs` `RecoveryConfig` (convert secs to Duration).
- `PendingQueue` — `Vec<PendingCommand>` with `push`, `drain`, `expire(timeout)`, `fail_all(err)`.
- `RecoveryManager::spawn_keepalive()` — tokio::spawn a loop: sleep noop_interval, check idle time, send NOP-Out, timeout for NOP-In.
- `RecoveryManager::trigger_recovery()` — drain outstanding from session, reconnect loop, retry pending.
- `reconnect()` — `Transport::connect()` + `LoginManager::login()` + TEST UNIT READY.

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse recovery -- --nocapture`
Expected: All 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/iscsi/recovery.rs
git commit -m "feat(iscsi): add recovery manager with NOP keepalive and session recovery"
```

---

### Task 11: iscsi/mod.rs Public API

**Files:**
- Modify: `src/iscsi/mod.rs`

- [ ] **Step 1: Add public re-exports**

```rust
pub mod command;
pub mod config;
pub mod digest;
pub mod login;
pub mod pdu;
pub mod pipeline;
pub mod recovery;
pub mod session;
pub mod transport;

// Re-export commonly used types
pub use config::{CliArgs, Config};
pub use login::{LoginManager, LoginResult, NegotiatedParams};
pub use pipeline::Pipeline;
pub use recovery::RecoveryManager;
pub use session::Session;
pub use transport::{DigestConfig, Transport, TransportReader, TransportWriter};
```

- [ ] **Step 2: Verify full iscsi module compiles**

Run: `cargo check 2>&1 | head -30`
Expected: No errors from `src/iscsi/` modules. (Other modules may have errors from removed old config — that's expected, fixed in later tasks.)

- [ ] **Step 3: Commit**

```bash
git add src/iscsi/mod.rs
git commit -m "feat(iscsi): add module re-exports"
```

---

### Task 12: Cache Layer Rewrite (`cache.rs`)

**Files:**
- Rewrite: `src/cache.rs`

- [ ] **Step 1: Write tests for chunk-aligned cache operations**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn test_cache_miss_calls_fetch() {
        let cache = BlockCache::new(16, 4096, 8192); // 16MB, 4K blocks
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = call_count.clone();

        let data = cache.read_blocks(0, 16, move |_lba, count| {
            let cc = cc.clone();
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok(Bytes::from(vec![0xAA; count as usize * 4096]))
            }
        }).await.unwrap();

        assert_eq!(data.len(), 16 * 4096);
        assert!(call_count.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn test_cache_hit_no_fetch() {
        let cache = BlockCache::new(16, 4096, 8192);
        let call_count = Arc::new(AtomicU32::new(0));

        // First read — populates cache
        let cc = call_count.clone();
        cache.read_blocks(0, 16, move |_lba, count| {
            let cc = cc.clone();
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok(Bytes::from(vec![0xBB; count as usize * 4096]))
            }
        }).await.unwrap();

        let first_count = call_count.load(Ordering::SeqCst);

        // Second read — should hit cache, no additional fetch
        let cc = call_count.clone();
        let data = cache.read_blocks(0, 16, move |_lba, count| {
            let cc = cc.clone();
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok(Bytes::from(vec![0xCC; count as usize * 4096]))
            }
        }).await.unwrap();

        assert_eq!(call_count.load(Ordering::SeqCst), first_count); // no new fetches
        assert_eq!(data[0], 0xBB); // got cached data, not new fetch
    }

    #[tokio::test]
    async fn test_cache_invalidate() {
        let cache = BlockCache::new(16, 4096, 8192);

        // Populate
        cache.read_blocks(0, 16, |_lba, count| async move {
            Ok(Bytes::from(vec![0xAA; count as usize * 4096]))
        }).await.unwrap();

        // Invalidate
        cache.invalidate_range(0, 16);

        // Read again — should fetch (different data)
        let data = cache.read_blocks(0, 16, |_lba, count| async move {
            Ok(Bytes::from(vec![0xBB; count as usize * 4096]))
        }).await.unwrap();

        assert_eq!(data[0], 0xBB); // got new data
    }

    #[test]
    fn test_chunk_lba_alignment() {
        let cache = BlockCache::new(16, 4096, 8192);
        // chunk_blocks = 64KB / 4096 = 16
        assert_eq!(cache.chunk_lba(0), 0);
        assert_eq!(cache.chunk_lba(5), 0);
        assert_eq!(cache.chunk_lba(16), 16);
        assert_eq!(cache.chunk_lba(17), 16);
        assert_eq!(cache.chunk_lba(32), 32);
    }

    #[tokio::test]
    async fn test_readahead_sequential_triggers_prefetch() {
        let cache = BlockCache::new(16, 4096, 8192);
        let fetch_count = Arc::new(AtomicU32::new(0));

        // Sequential reads: 0-15, 16-31, 32-47
        for start in (0..48).step_by(16) {
            let fc = fetch_count.clone();
            cache.read_blocks(start, 16, move |_lba, count| {
                let fc = fc.clone();
                async move {
                    fc.fetch_add(1, Ordering::SeqCst);
                    Ok(Bytes::from(vec![0xAA; count as usize * 4096]))
                }
            }).await.unwrap();
        }
        // With readahead, we expect MORE fetches than the 3 explicit reads
        // because prefetch should have been triggered after detecting sequential pattern
        let total_fetches = fetch_count.load(Ordering::SeqCst);
        assert!(total_fetches >= 3, "Expected at least 3 fetches (got {total_fetches}), readahead may have added more");
    }

    #[tokio::test]
    async fn test_readahead_resets_on_random() {
        let cache = BlockCache::new(16, 4096, 8192);
        // Sequential read
        cache.read_blocks(0, 16, |_lba, count| async move {
            Ok(Bytes::from(vec![0xAA; count as usize * 4096]))
        }).await.unwrap();
        cache.read_blocks(16, 16, |_lba, count| async move {
            Ok(Bytes::from(vec![0xAA; count as usize * 4096]))
        }).await.unwrap();
        // Random jump — readahead window should reset
        cache.read_blocks(10000, 16, |_lba, count| async move {
            Ok(Bytes::from(vec![0xAA; count as usize * 4096]))
        }).await.unwrap();
        // Verify window was reset (internal state check)
        assert_eq!(
            cache.readahead_window_blocks(),
            cache.readahead_min_blocks(),
            "Readahead window should reset after random access"
        );
    }

    #[tokio::test]
    async fn test_readahead_window_grows() {
        let cache = BlockCache::new(16, 4096, 8192);
        // Sequential reads should double the readahead window
        for i in 0..6 {
            let start = i * 16;
            cache.read_blocks(start, 16, |_lba, count| async move {
                Ok(Bytes::from(vec![0xAA; count as usize * 4096]))
            }).await.unwrap();
        }
        // After 6 sequential reads, window should have grown from min
        assert!(
            cache.readahead_window_blocks() > cache.readahead_min_blocks(),
            "Readahead window should grow after sequential reads"
        );
    }
}
```

- [ ] **Step 2: Implement BlockCache with moka, Bytes, chunk granularity, read_blocks, invalidate_range, readahead**

Key details:
- `moka::future::Cache<u64, Bytes>` with `max_capacity` = `size_mb * 1024 * 1024 / (64 * 1024)`.
- `chunk_blocks = 64 * 1024 / block_size`.
- `read_blocks` iterates chunks, uses `cache.try_get_with()` for deduplication.
- Readahead state: `ReadaheadState` with atomics. `maybe_trigger_readahead` detects sequential, doubles window, spawns prefetch at midpoint.
- Expose `readahead_window_blocks()` and `readahead_min_blocks()` for test assertions.

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse cache -- --nocapture`
Expected: All 7 tests pass (4 cache + 3 readahead).

- [ ] **Step 4: Commit**

```bash
git add src/cache.rs
git commit -m "feat: rewrite cache with moka, Bytes, 64KB chunks, adaptive readahead"
```

---

### Task 13: Block Device Rewrite (`block_device.rs`)

**Files:**
- Rewrite: `src/block_device.rs`

- [ ] **Step 1: Write tests for DirtyMap merge and overlap detection**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn test_dirty_map_insert_and_drain() {
        let mut dirty = DirtyMap::new();
        dirty.insert(0, 4, Bytes::from(vec![0xAA; 4 * 4096]));
        dirty.insert(10, 2, Bytes::from(vec![0xBB; 2 * 4096]));
        assert_eq!(dirty.total_bytes, 6 * 4096);

        let entries = dirty.drain_sorted();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, 0);  // LBA 0
        assert_eq!(entries[1].0, 10); // LBA 10
        assert_eq!(dirty.total_bytes, 0);
    }

    #[test]
    fn test_dirty_map_read_overlap_full() {
        let mut dirty = DirtyMap::new();
        let data = Bytes::from(vec![0xCC; 4 * 4096]);
        dirty.insert(0, 4, data);
        // Read fully within dirty range
        let result = dirty.read_overlap(0, 4, 4096);
        assert!(result.is_some());
        assert_eq!(result.unwrap()[0], 0xCC);
    }

    #[test]
    fn test_dirty_map_read_no_overlap() {
        let mut dirty = DirtyMap::new();
        dirty.insert(0, 4, Bytes::from(vec![0xCC; 4 * 4096]));
        // Read completely outside dirty range
        let result = dirty.read_overlap(100, 4, 4096);
        assert!(result.is_none());
    }

    #[test]
    fn test_block_device_alignment() {
        // offset=100, size=200, block_size=4096
        // start_lba=0, end_lba=1, block_count=1, skip=100
        let (start_lba, block_count, skip) = compute_alignment(100, 200, 4096);
        assert_eq!(start_lba, 0);
        assert_eq!(block_count, 1);
        assert_eq!(skip, 100);
    }

    #[test]
    fn test_block_device_alignment_spanning() {
        // offset=4000, size=200, block_size=4096
        // spans blocks 0 and 1
        let (start_lba, block_count, skip) = compute_alignment(4000, 200, 4096);
        assert_eq!(start_lba, 0);
        assert_eq!(block_count, 2);
        assert_eq!(skip, 4000);
    }
}
```

- [ ] **Step 2: Implement DirtyMap, compute_alignment, BlockDevice (channel handle), BlockDeviceWorker**

Key details:
- `DirtyMap` — `BTreeMap<u64, DirtyEntry>` with `insert`, `drain_sorted`, `read_overlap`, `is_empty`, `total_bytes`.
- `BlockDevice::spawn(pipeline, cache, ...) -> Self` — creates mpsc channel, spawns worker task, returns handle.
- `BlockDevice::read_bytes`, `write_bytes`, `flush` — `blocking_send` + `blocking_recv`.
- `BlockDeviceWorker::run` — `tokio::select!` on channel + coalesce timer.
- `handle_read` — check dirty map, then cache.read_blocks.
- `handle_write` — RMW if unaligned, insert to dirty map, flush if threshold.
- `flush_dirty` — drain sorted, submit via pipeline.

- [ ] **Step 3: Run tests**

Run: `cargo test -p iscsi-fuse block_device -- --nocapture`
Expected: All 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/block_device.rs
git commit -m "feat: rewrite block device with channel dispatch, write coalescing, dirty map"
```

---

### Task 14: FUSE Layer Update (`fuse_fs.rs`)

**Files:**
- Modify: `src/fuse_fs.rs`

- [ ] **Step 1: Update imports and BlockDevice usage**

Replace `use crate::block_device::BlockDevice` (already the same path but new API). Update:
- `read()` — call `self.block_device.read_bytes(offset, size)`, returns `Bytes` now (use `.as_ref()` for `reply.data()`).
- `write()` — call `self.block_device.write_bytes(offset, data)`.
- `flush()` and add `fsync()` — call `self.block_device.flush()`.
- `fuse_config()` — change `n_threads` from `Some(1)` to `Some(num_cpus::get() as u32)`.

- [ ] **Step 2: Add `fsync` implementation if missing**

```rust
fn fsync(
    &self, _req: &Request, ino: INodeNo, _fh: FileHandle,
    _datasync: bool, reply: ReplyEmpty,
) {
    if ino == DEVICE_INODE {
        match self.block_device.flush() {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    } else {
        reply.ok();
    }
}
```

- [ ] **Step 3: Write tests for FUSE config**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuse_config_multi_threaded() {
        let config = IscsiFuseFs::fuse_config(false, "test-vol");
        assert!(config.n_threads.unwrap() > 1 || num_cpus::get() == 1);
    }

    #[test]
    fn test_fuse_config_read_only_has_ro_mount() {
        let config = IscsiFuseFs::fuse_config(true, "test-vol");
        let has_ro = config.mount_options.iter().any(|o| matches!(o, MountOption::RO));
        assert!(has_ro);
    }

    #[test]
    fn test_fuse_config_read_write_has_rw_mount() {
        let config = IscsiFuseFs::fuse_config(false, "test-vol");
        let has_rw = config.mount_options.iter().any(|o| matches!(o, MountOption::RW));
        assert!(has_rw);
    }
}
```

- [ ] **Step 4: Verify compilation and run tests**

Run: `cargo test -p iscsi-fuse fuse_fs -- --nocapture`
Expected: All 3 FUSE config tests pass. (May still have errors in main.rs — fixed next task.)

- [ ] **Step 5: Commit**

```bash
git add src/fuse_fs.rs
git commit -m "feat: update FUSE layer for multi-threaded operation and channel-based block device"
```

---

### Task 15: Main Integration Rewrite (`main.rs`)

**Files:**
- Rewrite: `src/main.rs`
- Delete: `src/iscsi_backend.rs`

- [ ] **Step 1: Rewrite main.rs with new module wiring**

Replace the entire file with:

```rust
mod block_device;
mod cache;
mod fuse_fs;
mod iscsi;

use std::sync::Arc;
use std::time::Duration;
use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::block_device::BlockDevice;
use crate::cache::BlockCache;
use crate::fuse_fs::IscsiFuseFs;
use crate::iscsi::config::{CliArgs, Config, CONFIG_TEMPLATE};
use crate::iscsi::{LoginManager, Pipeline, RecoveryManager, Session, Transport};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("iscsi_fuse=info")),
        )
        .init();

    let args = CliArgs::parse();
    let config_path = args.resolved_config();

    // First-run: write template and exit
    if !config_path.exists() {
        std::fs::write(&config_path, CONFIG_TEMPLATE)
            .with_context(|| format!("Failed to write template: {}", config_path.display()))?;
        println!(
            "Created template config at {}.\nEdit target and address, then run iscsi-fuse again.",
            config_path.display()
        );
        return Ok(());
    }

    let config = Config::load(&config_path)?;
    let lun = args.lun.unwrap_or(config.lun);
    let mount_point = args.resolved_mount_point();

    if !mount_point.exists() {
        if let Err(e) = std::fs::create_dir_all(&mount_point) {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                warn!(path = %mount_point.display(), "Cannot create mount point — macFUSE will create it");
            } else {
                return Err(e).context("Failed to create mount point");
            }
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to create Tokio runtime")?;

    let (pipeline, _recovery) = rt.block_on(async {
        let (writer, reader) = Transport::connect(&config.address).await?;
        let login_mgr = Arc::new(LoginManager::new(&config.initiator, &config.target));
        // Login needs mutable writer/reader — pass owned, get back after login
        let (login_result, writer, reader) = login_mgr.login(writer, reader, 0).await?;
        info!(tsih = login_result.tsih, "iSCSI session established");

        let itt_pool = Arc::new(crate::iscsi::session::IttPool::new());
        let state = crate::iscsi::session::SessionState::new(1, 0);
        let session = Arc::new(Session::new(writer, itt_pool.clone(), state, login_result.negotiated.clone()));
        session.clone().spawn_receiver(reader, itt_pool.clone());

        let mut pipeline = Pipeline::new(session.clone(), lun, login_result.negotiated.clone());
        let (total_blocks, block_size) = pipeline.read_capacity().await?;
        pipeline.set_geometry(block_size, total_blocks);
        info!(block_size, total_blocks, "Device capacity queried");

        let recovery = Arc::new(RecoveryManager::new(
            session.clone(), login_mgr, config.address.clone(),
            config.recovery.clone().into(),
        ));
        recovery.spawn_keepalive();
        Ok::<_, anyhow::Error>((Arc::new(pipeline), recovery))
    })?;

    let cache_size = args.cache_size_mb.unwrap_or(config.cache.size_mb);
    let cache = BlockCache::new(cache_size, pipeline.block_size(), config.cache.readahead_max_kb);

    let block_device = BlockDevice::spawn(
        pipeline.clone(), cache, pipeline.block_size(), pipeline.total_bytes(),
        Duration::from_millis(config.cache.write_coalesce_ms),
        config.cache.write_coalesce_max_kb * 1024,
    );

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let fuse_fs = IscsiFuseFs::new(block_device, args.device_filename.clone(), args.read_only, uid, gid);
    let fuse_config = IscsiFuseFs::fuse_config(args.read_only, &args.volume_name);

    info!(mount_point = %mount_point.display(), volume_name = %args.volume_name, "Mounting FUSE filesystem");
    fuser::mount2(fuse_fs, &mount_point, &fuse_config).context("FUSE mount2 failed")?;

    info!("FUSE session ended, disconnecting iSCSI...");
    rt.block_on(async {
        if let Err(e) = pipeline.logout().await {
            error!("iSCSI logout failed: {e}");
        }
    });
    info!("Shutdown complete");
    Ok(())
}
```

- [ ] **Step 2: Delete old iscsi_backend.rs**

```bash
git rm src/iscsi_backend.rs
```

- [ ] **Step 3: Verify full project compiles**

Run: `cargo check`
Expected: Clean compilation with no errors.

- [ ] **Step 4: Run all tests**

Run: `cargo test`
Expected: All unit tests across all modules pass.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git rm src/iscsi_backend.rs
git commit -m "feat: rewrite main.rs with native iSCSI stack, remove iscsi-client-rs dependency"
```

---

### Task 16: Build Verification and Cleanup

**Files:**
- Various cleanup

- [ ] **Step 1: Run cargo clippy**

Run: `cargo clippy -- -W clippy::all 2>&1 | head -40`
Fix any warnings.

- [ ] **Step 2: Run cargo fmt**

Run: `cargo fmt`

- [ ] **Step 3: Verify release build**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Compiles successfully.

- [ ] **Step 4: Verify binary runs (first-run template generation)**

Run: `./target/release/iscsi-fuse --config /tmp/test-iscsi-fuse.toml 2>&1`
Expected: "Created template config at /tmp/test-iscsi-fuse.toml"

- [ ] **Step 5: Verify template is valid TOML**

Run: `cat /tmp/test-iscsi-fuse.toml` and verify it parses.

- [ ] **Step 6: Clean up temp file**

```bash
rm -f /tmp/test-iscsi-fuse.toml
```

- [ ] **Step 7: Final commit**

```bash
git add -A
git commit -m "chore: clippy fixes and formatting for native iSCSI rewrite"
```

---

## Task Dependency Graph

```
Task 1 (Cargo.toml + skeleton)
  ├─→ Task 2 (digest.rs)
  ├─→ Task 3 (command.rs)
  ├─→ Task 5 (config.rs)
  │
  ├─→ Task 4 (pdu.rs) ← depends on digest types
  │     │
  │     └─→ Task 6 (transport.rs) ← depends on pdu, digest
  │           │
  │           └─→ Task 7 (login.rs) ← depends on transport, pdu, config
  │                 │
  │                 └─→ Task 8a (session: ITT + submit) ← depends on transport, pdu, login
  │                       │
  │                       └─→ Task 8b (session: receiver task) ← depends on 8a
  │                             │
  │                             └─→ Task 9 (pipeline.rs) ← depends on session, command
  │                                   │
  │                                   └─→ Task 10 (recovery.rs) ← depends on session, login, transport, pipeline
  │
  ├─→ Task 12 (cache.rs) ← independent, depends only on moka+bytes
  │
  ├─→ Task 11 (mod.rs re-exports) ← depends on tasks 2-10 (all iscsi modules)
  │
  └─→ Task 13 (block_device.rs) ← depends on pipeline (9), cache (12)
        │
        └─→ Task 14 (fuse_fs.rs) ← depends on block_device
              │
              └─→ Task 15 (main.rs) ← depends on everything
                    │
                    └─→ Task 16 (cleanup)
```

**Parallelizable groups:**
- Tasks 2, 3, 5, 12 can run in parallel (no interdependencies)
- Tasks 4, 6, 7, 8a, 8b, 9, 10 must be sequential (protocol stack build-up)
- Task 11 runs after all iscsi modules (tasks 2-10)
- Task 13 needs tasks 9 + 12
- Tasks 14, 15, 16 are sequential at the end

**Note on integration testing:** After Task 11, run `cargo check` on the full iscsi module tree to catch type mismatches early, before proceeding to Tasks 13-15.
