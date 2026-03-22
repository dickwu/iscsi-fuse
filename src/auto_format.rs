use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{debug, error, info, warn};

use iscsi_fuse::block_device::BlockDevice;

/// Tracks the attached disk device for cleanup.
#[derive(Clone)]
pub struct AutoFormatState {
    /// e.g. "/dev/disk4" -- set after hdiutil attach succeeds.
    attached_device: Arc<Mutex<Option<String>>>,
}

impl AutoFormatState {
    pub fn new() -> Self {
        Self {
            attached_device: Arc::new(Mutex::new(None)),
        }
    }

    /// Detach the disk image if one was attached. Safe to call multiple times.
    pub fn detach(&self) {
        let device = self.attached_device.lock().unwrap().take();
        if let Some(dev) = device {
            info!(device = %dev, "Detaching disk image");
            match Command::new("hdiutil")
                .args(["detach", &dev, "-force"])
                .output()
            {
                Ok(o) if o.status.success() => info!("Disk image detached"),
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    warn!(stderr = %stderr, "hdiutil detach returned non-zero (may already be detached)");
                }
                Err(e) => warn!(error = %e, "Failed to run hdiutil detach"),
            }
        }
    }
}

/// Run the auto-format sequence in a blocking thread.
/// Call this from `std::thread::spawn` BEFORE `fuser::mount2`.
///
/// Uses HFS+ (Mac OS Extended, Journaled) which works reliably through the
/// FUSE + DiskImages I/O path. APFS formatting fails because the macOS kernel's
/// APFS implementation uses internal I/O paths through the DiskImages driver
/// that don't cleanly round-trip through FUSE backing stores.
pub fn run_auto_format(
    mount_point: PathBuf,
    device_filename: String,
    volume_name: String,
    state: AutoFormatState,
    block_device: BlockDevice,
) {
    // 1. Wait for the FUSE mount to expose the device file.
    let device_path = mount_point.join(&device_filename);
    info!(path = %device_path.display(), "Waiting for FUSE device file");

    let mut appeared = false;
    for i in 0..100 {
        if device_path.exists() {
            appeared = true;
            break;
        }
        if i > 0 && i % 20 == 0 {
            debug!(elapsed_secs = i / 10, "Still waiting for device file");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !appeared {
        error!("Timed out waiting for {} to appear", device_path.display());
        return;
    }

    // Brief settle delay for FUSE to be fully ready.
    std::thread::sleep(Duration::from_millis(500));
    info!("FUSE device file ready, attaching disk image");

    // 2. Attach the raw disk image via hdiutil.
    let disk_dev = match hdiutil_attach(&device_path) {
        Some(dev) => dev,
        None => return,
    };

    // Store for cleanup.
    *state.attached_device.lock().unwrap() = Some(disk_dev.clone());

    // 3. Check if already formatted.
    if is_formatted(&disk_dev) {
        info!(device = %disk_dev, "Disk already formatted, skipping");
        mount_volume(&disk_dev);
        block_device.set_sync_writes(false);
        info!("Switched to async writes for normal operation");
        return;
    }

    // 4. Format with HFS+ (Journaled).
    // APFS cannot be used here because macOS's APFS kernel implementation
    // uses internal DiskImages I/O paths that bypass FUSE, causing data
    // written by newfs_apfs to not round-trip through the iSCSI target.
    // HFS+ works correctly through the FUSE + DiskImages stack.
    info!(device = %disk_dev, volume = %volume_name, "Formatting with HFS+ (Journaled)");
    let raw_dev = format!("/dev/r{}", &disk_dev[5..]); // /dev/disk4 -> /dev/rdisk4
    let status = Command::new("newfs_hfs")
        .args(["-v", &volume_name, &raw_dev])
        .status();

    match status {
        Ok(s) if s.success() => {
            info!("HFS+ format complete");
        }
        Ok(s) => {
            error!(code = ?s.code(), "newfs_hfs failed");
            return;
        }
        Err(e) => {
            error!(error = %e, "Failed to run newfs_hfs");
            return;
        }
    }

    // 5. Mount the volume.
    mount_volume(&disk_dev);

    // 6. Switch back to async writes for normal operation.
    block_device.set_sync_writes(false);
    info!("Switched to async writes for normal operation");
}

/// Attach a raw disk image and return the /dev/diskN path.
fn hdiutil_attach(image_path: &std::path::Path) -> Option<String> {
    let output = Command::new("hdiutil")
        .args([
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nomount",
        ])
        .arg(image_path)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            // Output format: "/dev/disk4  \t...\n"
            let dev = stdout
                .split_whitespace()
                .find(|s| s.starts_with("/dev/disk"))
                .map(|s| s.to_string());
            match dev {
                Some(d) => {
                    info!(device = %d, "Disk image attached");
                    Some(d)
                }
                None => {
                    error!(stdout = %stdout, "Could not parse device from hdiutil output");
                    None
                }
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            error!(stderr = %stderr, "hdiutil attach failed");
            None
        }
        Err(e) => {
            error!(error = %e, "Failed to run hdiutil");
            None
        }
    }
}

/// Check if the device already has a filesystem (APFS or HFS+).
fn is_formatted(device: &str) -> bool {
    let output = Command::new("diskutil").args(["info", device]).output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.contains("Apple_APFS")
                || stdout.contains("Apple_HFS")
                || stdout.contains("Type (Bundle):")
        }
        _ => false,
    }
}

/// Mount the volume on the device. Tries diskutil mountDisk for HFS+.
fn mount_volume(device: &str) {
    debug!(device = %device, "Mounting volume");
    let status = Command::new("diskutil")
        .args(["mountDisk", device])
        .status();

    match status {
        Ok(s) if s.success() => info!(device = %device, "Volume mounted"),
        Ok(s) => warn!(code = ?s.code(), device = %device, "diskutil mountDisk returned non-zero"),
        Err(e) => warn!(error = %e, "Failed to run diskutil mountDisk"),
    }
}
