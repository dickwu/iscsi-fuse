mod block_device;
mod cache;
mod config;
mod fuse_fs;
mod iscsi_backend;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::block_device::BlockDevice;
use crate::cache::BlockCache;
use crate::config::{CONFIG_TEMPLATE, CliArgs};
use crate::fuse_fs::IscsiFuseFs;
use crate::iscsi_backend::IscsiBackend;
use iscsi_client_rs::cfg::config::Config;

fn main() -> Result<()> {
    // Initialize logging
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
            "Created template config at {}.\nEdit it with your iSCSI target details, then run iscsi-fuse again.",
            config_path.display()
        );
        return Ok(());
    }

    let mount_point = args.resolved_mount_point();

    // Ensure mount point exists. Under /Volumes this requires root, but macFUSE's
    // mount helper will create it automatically, so a permission error is not fatal.
    if !mount_point.exists()
        && let Err(e) = std::fs::create_dir_all(&mount_point)
    {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            warn!(
                path = %mount_point.display(),
                "Cannot create mount point (permission denied) — macFUSE will create it"
            );
        } else {
            return Err(e).context("Failed to create mount point directory");
        }
    }

    // Load iSCSI config
    let iscsi_cfg =
        Config::load_from_file(&config_path).context("Failed to load iSCSI configuration")?;

    // Create Tokio runtime
    let rt = tokio::runtime::Runtime::new().context("Failed to create Tokio runtime")?;

    // Connect to iSCSI target
    let backend = rt.block_on(async { IscsiBackend::connect(&iscsi_cfg, args.lun).await })?;

    let backend = Arc::new(backend);

    info!(
        block_size = backend.block_size(),
        total_blocks = backend.total_blocks(),
        total_bytes = backend.total_bytes(),
        "Connected to iSCSI target"
    );

    // Build block device layer
    let rt_handle = rt.handle().clone();
    let cache = BlockCache::new(args.cache_blocks);
    let block_device = BlockDevice::new(backend.clone(), cache, rt_handle);

    // Get current user/group for file ownership
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Build FUSE filesystem
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
        "Mounting FUSE filesystem"
    );

    // Set up Ctrl+C handler to unmount cleanly
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .context("Failed to set Ctrl+C handler")?;

    // Use blocking mount2 — this runs the FUSE event loop on the current thread.
    // It returns when the filesystem is unmounted (e.g. via umount or Ctrl+C).
    info!(
        "FUSE filesystem mounting at {}. Press Ctrl+C to unmount.",
        mount_point.display()
    );

    fuser::mount2(fuse_fs, &mount_point, &fuse_config).context("FUSE mount2 failed")?;

    info!("FUSE session ended, disconnecting iSCSI...");

    // Disconnect iSCSI
    if let Err(e) = rt.block_on(async { backend.disconnect().await }) {
        error!("Error during iSCSI disconnect: {e}");
    }

    info!("Shutdown complete");
    Ok(())
}
