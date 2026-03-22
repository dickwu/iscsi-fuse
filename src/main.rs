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
use crate::iscsi::config::CONFIG_TEMPLATE;
use crate::iscsi::recovery::RecoveryConfig;
use crate::iscsi::session::{IttPool, SessionState};
use crate::iscsi::transport::Transport;
use crate::iscsi::{
    CliArgs, Config, LoginManager, Pipeline, RecoveryManager, Session, TransportReader,
    TransportWriter,
};

fn main() -> Result<()> {
    // Initialize tracing with env filter defaulting to "iscsi_fuse=info"
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("iscsi_fuse=info")),
        )
        .init();

    let args = CliArgs::parse();
    let config_path = args.resolved_config();

    // On first run: if the config doesn't exist, write a template and exit.
    if !config_path.exists() {
        std::fs::write(&config_path, CONFIG_TEMPLATE).with_context(|| {
            format!(
                "Failed to write template config to {}",
                config_path.display()
            )
        })?;
        println!(
            "Created template config at {}.\n\
             Edit it with your iSCSI target details, then run iscsi-fuse again.",
            config_path.display()
        );
        return Ok(());
    }

    // Load Config from TOML
    let config = Config::load(&config_path).context("Failed to load configuration")?;

    // Resolve LUN: CLI arg overrides config file
    let lun = args.lun.unwrap_or(config.lun);
    let mount_point = args.resolved_mount_point();

    // Ensure mount point exists. Under /Volumes this requires root, but macFUSE's
    // mount helper will create it automatically, so a permission error is not fatal.
    if !mount_point.exists()
        && let Err(e) = std::fs::create_dir_all(&mount_point)
    {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            warn!(
                path = %mount_point.display(),
                "Cannot create mount point (permission denied) -- macFUSE will create it"
            );
        } else {
            return Err(e).context("Failed to create mount point directory");
        }
    }

    // Create tokio multi-thread runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to create Tokio runtime")?;

    // Connect, login, and set up the iSCSI session
    let (pipeline, block_size, total_blocks, total_bytes, _recovery) = rt.block_on(async {
        // a. Connect to iSCSI target
        let (mut writer, mut reader): (TransportWriter, TransportReader) =
            Transport::connect(&config.address)
                .await
                .context("Failed to connect to iSCSI target")?;

        // b. Create LoginManager
        let login_mgr = LoginManager::new(&config.initiator, &config.target);

        // c. Login
        let login_result = login_mgr
            .login(&mut writer, &mut reader, 0)
            .await
            .context("iSCSI login failed")?;

        let negotiated = login_result.negotiated;

        // d. Log session established
        info!(tsih = login_result.tsih, "iSCSI session established");

        // e. Enable digests on writer and reader based on negotiated params
        writer.enable_digests(negotiated.header_digest, negotiated.data_digest);
        reader.enable_digests(negotiated.header_digest, negotiated.data_digest);

        // f. Create IttPool and SessionState using values from login response
        let itt_pool = Arc::new(IttPool::new());
        let state = SessionState::new(
            login_result.initial_cmd_sn,
            login_result.initial_exp_stat_sn,
        );

        // g. Create Session wrapped in Arc
        let session = Arc::new(Session::new(
            writer,
            itt_pool.clone(),
            state,
            negotiated.clone(),
        ));

        // h. Spawn receiver task
        session.spawn_receiver(reader, itt_pool);

        // i. Create Pipeline
        let mut pipeline = Pipeline::new(session.clone(), lun, negotiated);

        // j. Read device capacity
        let (cap_total_blocks, cap_block_size) = pipeline
            .read_capacity()
            .await
            .context("Failed to read device capacity")?;

        // k. Set geometry
        pipeline.set_geometry(cap_block_size, cap_total_blocks);

        let total_bytes = cap_total_blocks * cap_block_size as u64;

        // l. Log device capacity
        info!(
            block_size = cap_block_size,
            total_blocks = cap_total_blocks,
            total_bytes,
            "Device capacity discovered"
        );

        // m. Create RecoveryManager and spawn keepalive
        let recovery_config: RecoveryConfig = config.recovery.clone().into();
        let login_mgr = Arc::new(LoginManager::new(&config.initiator, &config.target));
        let recovery =
            RecoveryManager::new(session, login_mgr, config.address.clone(), recovery_config);
        recovery.spawn_keepalive();

        let pipeline = Arc::new(pipeline);

        Ok::<_, anyhow::Error>((
            pipeline,
            cap_block_size,
            cap_total_blocks,
            total_bytes,
            recovery,
        ))
    })?;

    // Create BlockCache
    let cache_size_mb = args.cache_size_mb.unwrap_or(config.cache.size_mb);
    let cache = BlockCache::new(cache_size_mb, block_size, config.cache.readahead_max_kb);

    // Spawn BlockDevice worker (needs tokio runtime context for tokio::spawn)
    let _rt_guard = rt.enter();
    let coalesce_timeout = Duration::from_millis(config.cache.write_coalesce_ms);
    let coalesce_max_bytes = config.cache.write_coalesce_max_kb * 1024;
    let block_device = BlockDevice::spawn(
        pipeline.clone(),
        cache,
        block_size,
        total_bytes,
        coalesce_timeout,
        coalesce_max_bytes,
    );

    // Get uid/gid
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Create FUSE filesystem
    let fuse_fs = IscsiFuseFs::new(
        block_device,
        args.device_filename.clone(),
        args.read_only,
        uid,
        gid,
    );

    let fuse_config = IscsiFuseFs::fuse_config(args.read_only, &args.volume_name);

    info!(
        mount_point = %mount_point.display(),
        volume_name = %args.volume_name,
        device_filename = %args.device_filename,
        read_only = args.read_only,
        block_size,
        total_blocks,
        total_bytes,
        "Mounting FUSE filesystem"
    );

    // Mount -- this blocks until the filesystem is unmounted
    fuser::mount2(fuse_fs, &mount_point, &fuse_config).context("FUSE mount2 failed")?;

    // On return (unmount): send logout
    info!("FUSE session ended, sending iSCSI logout...");
    if let Err(e) = rt.block_on(pipeline.logout()) {
        error!("iSCSI logout failed: {e}");
    }

    info!("Shutdown complete");
    Ok(())
}
