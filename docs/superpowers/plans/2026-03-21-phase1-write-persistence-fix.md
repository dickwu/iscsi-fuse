# Phase 1: Fix Write Persistence Bug — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the bug where SCSI WRITE returns `Good` but data doesn't survive unmount/remount, by adding the missing SYNCHRONIZE CACHE SCSI command.

**Architecture:** Add `build_synchronize_cache()` CDB builder → add `scsi_synchronize_cache()` to Pipeline → call it after `flush_dirty()` in BlockDeviceWorker. TDD throughout: write failing tests first, then implement.

**Tech Stack:** Rust, tokio, iSCSI (RFC 7143), SCSI SBC-4 (SYNCHRONIZE CACHE 10, opcode 0x35)

**Spec:** `docs/superpowers/specs/2026-03-21-iscsi-rs-driverkit-design.md` (Phase 1, lines 493-498)

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/iscsi/command.rs` | Modify (after line 202) | Add `build_synchronize_cache10()` CDB builder |
| `src/iscsi/pipeline.rs` | Modify (after line 185) | Add `scsi_synchronize_cache()` public method |
| `src/block_device.rs` | Modify (line 453-455) | Call `synchronize_cache()` after `flush_dirty()` writes complete |
| `tests/write_persistence.rs` | Create | Integration test: write → sync → disconnect → reconnect → read → assert |

---

### Task 1: Add `build_synchronize_cache10()` CDB Builder

**Files:**
- Modify: `src/iscsi/command.rs:196-202` (after `build_write()`, before tests)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/iscsi/command.rs`, after the existing write tests:

```rust
#[test]
fn test_build_synchronize_cache10() {
    let cdb = build_synchronize_cache10(0x1000, 128);
    assert_eq!(cdb[0], 0x35); // SYNCHRONIZE CACHE (10) opcode
    let parsed_lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
    assert_eq!(parsed_lba, 0x1000);
    let parsed_count = u16::from_be_bytes([cdb[7], cdb[8]]);
    assert_eq!(parsed_count, 128);
}

#[test]
fn test_build_synchronize_cache10_full_flush() {
    // lba=0, block_count=0 means "flush entire cache"
    let cdb = build_synchronize_cache10(0, 0);
    assert_eq!(cdb[0], 0x35);
    let parsed_lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
    assert_eq!(parsed_lba, 0);
    let parsed_count = u16::from_be_bytes([cdb[7], cdb[8]]);
    assert_eq!(parsed_count, 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib test_build_synchronize_cache`
Expected: FAIL — `cannot find function build_synchronize_cache10`

- [ ] **Step 3: Write minimal implementation**

Add after `build_write()` (after line 202) in `src/iscsi/command.rs`:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib test_build_synchronize_cache`
Expected: 2 tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/iscsi/command.rs
git commit -m "feat: add build_synchronize_cache10() CDB builder

Implements SCSI SYNCHRONIZE CACHE (10) command (opcode 0x35, SBC-4 5.35).
This command tells the iSCSI target to flush its volatile write cache
to persistent storage."
```

---

### Task 2: Add `scsi_synchronize_cache()` to Pipeline

**Files:**
- Modify: `src/iscsi/pipeline.rs:185` (after `scsi_write()` method)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/iscsi/pipeline.rs`:

```rust
#[test]
fn test_synchronize_cache_method_exists() {
    // Compile-time check: Pipeline has scsi_synchronize_cache method
    // (cannot test execution without a live session, but signature must exist)
    fn _assert_method(p: &Pipeline) {
        let _ = p.scsi_synchronize_cache();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib test_synchronize_cache_method_exists`
Expected: FAIL — `no method named scsi_synchronize_cache found`

- [ ] **Step 3: Write implementation**

Add after the `scsi_write_single()` method (after line 200) in `src/iscsi/pipeline.rs`:

```rust
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
```

Note on `submit_command` arguments:
- `&cdb` — the SYNCHRONIZE CACHE CDB
- `self.lun` — target LUN
- `0` — edtl is 0 (no data transfer)
- `false` — not a read
- `false` — not a write (no data out)
- `None` — no immediate data

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib test_synchronize_cache_method_exists`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo test --lib`
Expected: All existing tests still pass

- [ ] **Step 6: Commit**

```bash
git add src/iscsi/pipeline.rs
git commit -m "feat: add scsi_synchronize_cache() to Pipeline

Issues SYNCHRONIZE CACHE (10) with lba=0, block_count=0 to flush
the target's entire volatile write cache to persistent storage.
Uses WRITE_TIMEOUT (300s) since cache flushes can be slow on
large LUNs."
```

---

### Task 3: Call SYNCHRONIZE CACHE After flush_dirty()

**Files:**
- Modify: `src/block_device.rs:453-455` (end of `flush_dirty()`)

- [ ] **Step 1: Identify the insertion point**

Read `src/block_device.rs` lines 440-456. After all write handles are awaited successfully (line 453), add the SYNCHRONIZE CACHE call before the final `Ok(())`.

- [ ] **Step 2: Implement the change**

In `src/block_device.rs`, replace the end of `flush_dirty()`:

```rust
        // Await all.
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    error!("SCSI WRITE (flush) failed: {e}");
                    return Err(Errno::EIO);
                }
                Err(e) => {
                    error!("Flush task panicked: {e}");
                    return Err(Errno::EIO);
                }
            }
        }

        Ok(())
    }
```

Replace with:

```rust
        // Await all.
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    error!("SCSI WRITE (flush) failed: {e}");
                    return Err(Errno::EIO);
                }
                Err(e) => {
                    error!("Flush task panicked: {e}");
                    return Err(Errno::EIO);
                }
            }
        }

        // Flush target's volatile write cache to persistent storage.
        if let Err(e) = self.pipeline.scsi_synchronize_cache().await {
            error!("SYNCHRONIZE CACHE failed: {e}");
            return Err(Errno::EIO);
        }

        Ok(())
    }
```

- [ ] **Step 3: Build to verify compilation**

Run: `PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo check`
Expected: Compiles with no errors

- [ ] **Step 4: Run full test suite**

Run: `PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo test --lib`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add src/block_device.rs
git commit -m "fix: issue SYNCHRONIZE CACHE after flushing dirty writes

After all dirty map entries are written to iSCSI via scsi_write(),
now issues SYNCHRONIZE CACHE (10) to tell the target to commit its
volatile write cache to persistent storage. This fixes the write
persistence bug where data was lost across unmount/remount cycles."
```

---

### Task 4: Write Integration Test for Write Persistence

**Files:**
- Create: `src/lib.rs` (expose modules for integration tests)
- Modify: `src/main.rs:1-5` (change `mod` to `use` imports from lib crate)
- Create: `tests/write_persistence.rs`

This test requires a live iSCSI target. It is an `#[ignore]` test by default, run manually with `cargo test --test write_persistence -- --ignored`.

- [ ] **Step 1: Create `src/lib.rs` to expose modules**

The project is currently a binary-only crate (`src/main.rs` declares all modules with `mod`). Integration tests need a library crate to import from. Create `src/lib.rs`:

```rust
pub mod block_device;
pub mod cache;
pub mod iscsi;
```

Note: `fuse_fs` and `auto_format` are intentionally NOT exported — they are binary-only modules.

- [ ] **Step 2: Update `src/main.rs` to use lib crate instead of declaring modules**

Replace the first 5 lines of `src/main.rs`:

```rust
mod auto_format;
mod block_device;
mod cache;
mod fuse_fs;
mod iscsi;
```

With:

```rust
mod auto_format;
mod fuse_fs;

use iscsi_fuse::block_device;
use iscsi_fuse::cache;
use iscsi_fuse::iscsi;
```

The `auto_format` and `fuse_fs` modules stay as `mod` in `main.rs` since they are binary-only (not needed by integration tests). The `block_device`, `cache`, and `iscsi` modules are now imported from the lib crate.

- [ ] **Step 3: Build to verify lib/bin split compiles**

Run: `PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo check`
Expected: Compiles. Both `src/lib.rs` (library) and `src/main.rs` (binary) work.

- [ ] **Step 4: Write the integration test**

Create `tests/write_persistence.rs`:

```rust
//! Integration test for write persistence across disconnect/reconnect.
//!
//! Requires a live iSCSI target. Set environment variables:
//!   ISCSI_TARGET_ADDR=192.168.2.57:3260
//!   ISCSI_TARGET_IQN=iqn.2004-04.com.qnap:ts-873a:iscsi.lun1
//!   ISCSI_LUN=0
//!
//! Run: cargo test --test write_persistence -- --ignored

use std::sync::Arc;

use anyhow::Result;
use bytes::{Bytes, BytesMut};

use iscsi_fuse::iscsi::login::LoginManager;
use iscsi_fuse::iscsi::pipeline::Pipeline;
use iscsi_fuse::iscsi::session::{IttPool, Session, SessionState};
use iscsi_fuse::iscsi::transport::Transport;

/// Helper: connect, login, create pipeline, read capacity.
/// Mirrors the setup in main.rs lines 83-155.
async fn connect_and_login() -> Result<(Arc<Pipeline>, Arc<Session>)> {
    let addr = std::env::var("ISCSI_TARGET_ADDR")
        .unwrap_or_else(|_| "192.168.2.57:3260".to_string());
    let initiator = std::env::var("ISCSI_INITIATOR_IQN")
        .unwrap_or_else(|_| "iqn.2026-03.com.iscsi-rs:test".to_string());
    let target = std::env::var("ISCSI_TARGET_IQN")
        .unwrap_or_else(|_| "iqn.2004-04.com.qnap:ts-873a:iscsi.lun1".to_string());
    let lun: u64 = std::env::var("ISCSI_LUN")
        .unwrap_or_else(|_| "0".to_string())
        .parse()?;

    // a. Connect
    let (mut writer, mut reader) = Transport::connect(&addr).await?;

    // b. Login
    let login_mgr = LoginManager::new(&initiator, &target);
    let login_result = login_mgr.login(&mut writer, &mut reader, 0).await?;
    let negotiated = login_result.negotiated;

    // c. Enable digests
    writer.enable_digests(negotiated.header_digest, negotiated.data_digest);
    reader.enable_digests(negotiated.header_digest, negotiated.data_digest);

    // d. Create session
    let itt_pool = Arc::new(IttPool::new());
    let state = SessionState::new(
        login_result.initial_cmd_sn,
        login_result.initial_exp_stat_sn,
    );
    let session = Arc::new(Session::new(
        writer,
        itt_pool.clone(),
        state,
        negotiated.clone(),
    ));
    session.spawn_receiver(reader, itt_pool);

    // e. Create pipeline, read capacity, set geometry
    let mut pipeline = Pipeline::new(session.clone(), lun, negotiated);
    let (total_blocks, block_size) = pipeline.read_capacity().await?;
    pipeline.set_geometry(block_size, total_blocks);

    Ok((Arc::new(pipeline), session))
}

/// Write a known pattern, SYNCHRONIZE CACHE, disconnect, reconnect, read back.
#[tokio::test]
#[ignore] // Requires live iSCSI target
async fn test_write_persists_across_reconnect() -> Result<()> {
    // -- Session 1: Write --
    let (pipeline, _session) = connect_and_login().await?;
    let block_size = pipeline.block_size();

    // Write a distinctive pattern to LBA 0 (one block)
    let pattern: u8 = 0xAB;
    let write_data = Bytes::from(vec![pattern; block_size as usize]);
    pipeline.scsi_write(0, write_data).await?;

    // Flush target cache to persistent storage
    pipeline.scsi_synchronize_cache().await?;

    // Disconnect (logout + drop)
    pipeline.logout().await?;
    drop(pipeline);
    drop(_session);

    // Small delay to ensure target processes logout
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // -- Session 2: Read back --
    let (pipeline2, _session2) = connect_and_login().await?;

    let read_data = pipeline2.scsi_read(0, 1).await?;
    assert_eq!(read_data.len(), block_size as usize);
    assert!(
        read_data.iter().all(|&b| b == pattern),
        "Data did not persist! Expected all 0x{:02X}, got first 16 bytes: {:02X?}",
        pattern,
        &read_data[..16.min(read_data.len())]
    );

    pipeline2.logout().await?;
    Ok(())
}

/// Write multiple blocks, verify persistence.
#[tokio::test]
#[ignore]
async fn test_multi_block_write_persists() -> Result<()> {
    let (pipeline, _session) = connect_and_login().await?;
    let block_size = pipeline.block_size();

    // Write 8 blocks at LBA 100 with incrementing pattern
    let num_blocks = 8u32;
    let mut data = BytesMut::with_capacity((num_blocks * block_size) as usize);
    for i in 0..num_blocks {
        let pattern = (i & 0xFF) as u8;
        data.extend(std::iter::repeat(pattern).take(block_size as usize));
    }
    pipeline.scsi_write(100, data.freeze()).await?;
    pipeline.scsi_synchronize_cache().await?;
    pipeline.logout().await?;
    drop(pipeline);
    drop(_session);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Reconnect and verify
    let (pipeline2, _session2) = connect_and_login().await?;
    let read_data = pipeline2.scsi_read(100, num_blocks).await?;

    for i in 0..num_blocks {
        let offset = (i * block_size) as usize;
        let expected = (i & 0xFF) as u8;
        let actual = read_data[offset];
        assert_eq!(
            actual, expected,
            "Block {} at LBA {}: expected 0x{:02X}, got 0x{:02X}",
            i,
            100 + i,
            expected,
            actual
        );
    }

    pipeline2.logout().await?;
    Ok(())
}
```

- [ ] **Step 5: Build the test (compile-only)**

Run: `PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo test --test write_persistence --no-run`
Expected: Compiles (test is `#[ignore]` so won't run)

- [ ] **Step 6: Run against live target**

Run: `ISCSI_TARGET_ADDR=192.168.2.57:3260 cargo test --test write_persistence -- --ignored --nocapture`
Expected: Both tests PASS — data survives disconnect/reconnect

If tests FAIL (data doesn't persist), debug:
1. Check if SYNCHRONIZE CACHE returns Good status (add `--nocapture` for logs)
2. Capture wire traffic: `sudo tcpdump -i en0 port 3260 -w /tmp/iscsi.pcap`
3. Open in Wireshark with iSCSI dissector, check Data-Out PDUs have correct offsets
4. Check if SCSI WRITE responses show Good status but target returns stale data

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs src/main.rs tests/write_persistence.rs
git commit -m "test: add write persistence integration test

Converts project to lib+bin crate structure so integration tests can
import modules. Verifies that data written via scsi_write() +
scsi_synchronize_cache() survives a full disconnect/reconnect cycle.

Requires a live iSCSI target (set ISCSI_TARGET_ADDR env var).
Tests are #[ignore] by default.
Run: cargo test --test write_persistence -- --ignored"
```

**CI note:** For automated CI testing, set up an open-source iSCSI target
(tgt or LIO) in a Linux container. This is deferred to Phase 7 (CI/CD setup)
but the test infrastructure is ready for it — just set the env vars to point
at the containerized target.

---

### Task 5: Validate and Polish

- [ ] **Step 1: Run full test suite**

Run: `PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo test --lib`
Expected: All unit tests pass

- [ ] **Step 2: Run clippy**

Run: `PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo clippy --all-targets -- -D warnings`
Expected: No warnings

- [ ] **Step 3: Run fmt**

Run: `cargo fmt --all --check`
Expected: No formatting issues

- [ ] **Step 4: Run integration test against QNAP**

Run: `ISCSI_TARGET_ADDR=192.168.2.57:3260 cargo test --test write_persistence -- --ignored --nocapture`
Expected: Both tests PASS

- [ ] **Step 5: Manual end-to-end test via FUSE**

```bash
# Build and run iscsi-fuse with sync writes
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo build --release
sudo ./target/release/iscsi-fuse 192.168.2.57:3260 --sync-writes

# In another terminal:
# Write through FUSE
dd if=/dev/urandom of=/Volumes/iscsi/disk.img bs=4096 count=1 seek=0

# Unmount
sudo umount /Volumes/iscsi

# Remount
sudo ./target/release/iscsi-fuse 192.168.2.57:3260 --sync-writes

# Read back and check non-zero
dd if=/Volumes/iscsi/disk.img bs=4096 count=1 | xxd | head
```

Expected: Data persists (non-zero bytes after remount)

- [ ] **Step 6: Final commit (if any fixups needed)**

```bash
git add -A
git commit -m "fix: address clippy/fmt issues from write persistence work"
```

---

## Phase 1 Gate

All of these must be true before proceeding to Phase 2:

- [ ] `build_synchronize_cache10()` exists and is tested
- [ ] `scsi_synchronize_cache()` exists on Pipeline
- [ ] `flush_dirty()` calls SYNCHRONIZE CACHE after all writes complete
- [ ] Integration test passes against QNAP: write → sync cache → disconnect → reconnect → read → data matches
- [ ] All unit tests pass
- [ ] clippy clean, fmt clean
