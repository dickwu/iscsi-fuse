use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "iscsi-fuse")]
#[command(about = "Mount an iSCSI target as a FUSE filesystem on macOS")]
pub struct CliArgs {
    /// Path to iSCSI YAML config file
    #[arg(short = 'c', long)]
    pub config: PathBuf,

    /// FUSE mount point directory
    #[arg(short = 'm', long)]
    pub mount_point: PathBuf,

    /// LUN number on the iSCSI target
    #[arg(short = 'l', long, default_value = "0")]
    pub lun: u64,

    /// Mount in read-only mode
    #[arg(long, default_value = "false")]
    pub read_only: bool,

    /// Cache size in number of blocks
    #[arg(long, default_value = "1024")]
    pub cache_blocks: usize,

    /// Name of the virtual device file in the mount
    #[arg(long, default_value = "disk.img")]
    pub device_filename: String,
}
