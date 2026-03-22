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

use iscsi_lib::iscsi::login::{LoginManager, NegotiatedParams};
use iscsi_lib::iscsi::pipeline::Pipeline;
use iscsi_lib::iscsi::session::{IttPool, Session, SessionState};
use iscsi_lib::iscsi::transport::Transport;

/// Helper: connect, login, create pipeline, read capacity.
async fn connect_and_login() -> Result<(Arc<Pipeline>, Arc<Session>)> {
    connect_and_login_with_params(None).await
}

/// Helper with optional custom negotiation params.
async fn connect_and_login_with_params(
    custom_params: Option<&NegotiatedParams>,
) -> Result<(Arc<Pipeline>, Arc<Session>)> {
    let addr =
        std::env::var("ISCSI_TARGET_ADDR").unwrap_or_else(|_| "192.168.2.57:3260".to_string());
    let initiator = std::env::var("ISCSI_INITIATOR_IQN")
        .unwrap_or_else(|_| "iqn.2024-01.com.iscsi-fuse:initiator".to_string());
    let target = std::env::var("ISCSI_TARGET_IQN")
        .unwrap_or_else(|_| "iqn.2004-04.com.qnap:ts-873a:iscsi.target-1.6880d1".to_string());
    eprintln!("  target={target}, initiator={initiator}");
    let lun: u64 = std::env::var("ISCSI_LUN")
        .unwrap_or_else(|_| "1".to_string())
        .parse()?;

    // a. Connect
    let (mut writer, mut reader) = Transport::connect(&addr).await?;

    // b. Login
    let login_mgr = LoginManager::new(&initiator, &target);
    let login_result = login_mgr
        .login_with_params(&mut writer, &mut reader, 0, custom_params)
        .await?;
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

    // Send TEST UNIT READY to clear any Unit Attention condition
    eprintln!("  Sending TEST UNIT READY to clear Unit Attention");
    let tur_cdb = iscsi_lib::iscsi::command::build_test_unit_ready();
    let (_, tur_rx) = pipeline.session().submit_command(
        &tur_cdb, 0, 0, false, false, None,
    ).await?;
    let tur_resp = tur_rx.await?;
    eprintln!("  TUR status: {:?}", tur_resp.status);

    // Write a distinctive pattern to LBA 0 (one block)
    let pattern: u8 = 0xAB;
    let write_data = Bytes::from(vec![pattern; block_size as usize]);
    eprintln!("  block_size={block_size}, writing {pattern:#04X} x {block_size} to LBA 0");
    pipeline.scsi_write(0, write_data).await?;

    // Read back in SAME session (before sync cache)
    let same_session_read = pipeline.scsi_read(0, 1).await?;
    let same_ok = same_session_read.iter().all(|&b| b == pattern);
    eprintln!(
        "  same-session read: ok={same_ok}, first 16: {:02X?}",
        &same_session_read[..16.min(same_session_read.len())]
    );
    assert!(
        same_ok,
        "Same-session read failed! Write never reached target. First 16: {:02X?}",
        &same_session_read[..16]
    );

    // Flush target cache to persistent storage
    eprintln!("  sending SYNCHRONIZE CACHE");
    pipeline.scsi_synchronize_cache().await?;

    // Read again after sync cache (same session)
    let post_sync_read = pipeline.scsi_read(0, 1).await?;
    let post_sync_ok = post_sync_read.iter().all(|&b| b == pattern);
    eprintln!(
        "  post-sync read: ok={post_sync_ok}, first 16: {:02X?}",
        &post_sync_read[..16.min(post_sync_read.len())]
    );

    // Disconnect (logout + drop)
    eprintln!("  logging out session 1");
    pipeline.logout().await?;
    drop(pipeline);
    drop(_session);

    // Small delay to ensure target processes logout
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // -- Session 2: Read back --
    eprintln!("  reconnecting session 2");
    let (pipeline2, _session2) = connect_and_login().await?;

    let read_data = pipeline2.scsi_read(0, 1).await?;
    eprintln!(
        "  session-2 read: first 16: {:02X?}",
        &read_data[..16.min(read_data.len())]
    );
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

/// Test write with FUA bit set and also validate target rejects bad LBAs.
#[tokio::test]
#[ignore]
async fn test_write_fua_and_target_validation() -> Result<()> {
    let (pipeline, _session) = connect_and_login().await?;
    let block_size = pipeline.block_size();

    // Test 1: Write to an impossible LBA (way past end of disk)
    // If target returns Good for this, target isn't processing WRITEs at all
    eprintln!("  Test 1: Write to LBA 0xFFFFFFFE (should fail with CheckCondition)");
    let bad_data = Bytes::from(vec![0xEE; block_size as usize]);
    match pipeline.scsi_write(0xFFFFFFFE, bad_data).await {
        Ok(()) => eprintln!("  BAD: Target returned Good for out-of-range LBA!"),
        Err(e) => eprintln!("  GOOD: Target rejected invalid LBA: {e}"),
    }

    // Test 2: Write with FUA bit manually set in CDB
    eprintln!("  Test 2: Write to LBA 0 with FUA bit");
    let mut cdb = [0u8; 16];
    cdb[0] = 0x2A; // WRITE10
    cdb[1] = 0x08; // FUA bit
    // LBA 0 (bytes 2-5 already zero)
    cdb[7] = 0x00;
    cdb[8] = 0x01; // 1 block

    let pattern: u8 = 0xEF;
    let write_data = Bytes::from(vec![pattern; block_size as usize]);
    let edtl = block_size;

    let (itt, rx) = pipeline.session().submit_command(
        &cdb, 0, edtl, false, true, Some(write_data.clone()),
    ).await?;
    pipeline.session().itt_pool.register_write_data(itt, write_data);
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10), rx,
    ).await??;
    eprintln!("  FUA write status: {:?}", response.status);

    // Read back
    let read_data = pipeline.scsi_read(0, 1).await?;
    let ok = read_data.iter().all(|&b| b == pattern);
    eprintln!(
        "  Read back: ok={ok}, first 16: {:02X?}",
        &read_data[..16.min(read_data.len())]
    );

    pipeline.logout().await?;
    Ok(())
}

/// Test write with ImmediateData=No, InitialR2T=Yes (forces R2T/DataOut path).
/// This tests the theory that the QNAP target ignores immediate data.
#[tokio::test]
#[ignore]
async fn test_write_with_r2t_no_immediate_data() -> Result<()> {
    let mut params = NegotiatedParams::defaults_10g();
    params.immediate_data = false;
    params.initial_r2t = true;

    eprintln!("  Connecting with ImmediateData=No, InitialR2T=Yes");
    let (pipeline, _session) = connect_and_login_with_params(Some(&params)).await?;
    let block_size = pipeline.block_size();
    let negotiated = pipeline.negotiated();

    eprintln!(
        "  Negotiated: immediate_data={}, initial_r2t={}, first_burst={}",
        negotiated.immediate_data, negotiated.initial_r2t, negotiated.first_burst_length
    );

    let pattern: u8 = 0xCD;
    let write_data = Bytes::from(vec![pattern; block_size as usize]);
    eprintln!("  Writing 0x{pattern:02X} x {block_size} to LBA 0 (via R2T)");
    pipeline.scsi_write(0, write_data).await?;

    let read_data = pipeline.scsi_read(0, 1).await?;
    let ok = read_data.iter().all(|&b| b == pattern);
    eprintln!(
        "  Same-session read: ok={ok}, first 16: {:02X?}",
        &read_data[..16.min(read_data.len())]
    );

    if ok {
        eprintln!("  SUCCESS: R2T path works! Target ignores immediate data.");
    } else {
        eprintln!("  FAIL: R2T path also fails. Issue is NOT immediate data.");
    }

    pipeline.scsi_synchronize_cache().await?;
    pipeline.logout().await?;
    Ok(())
}
