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
    let addr =
        std::env::var("ISCSI_TARGET_ADDR").unwrap_or_else(|_| "192.168.2.57:3260".to_string());
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
        data.extend(std::iter::repeat_n(pattern, block_size as usize));
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
            actual,
            expected,
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
