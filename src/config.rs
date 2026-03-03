use clap::Parser;
use std::path::PathBuf;

/// Default config path: ~/.iscsi-fuse
pub fn default_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".iscsi-fuse")
}

/// Template written to the default config path on first run.
pub const CONFIG_TEMPLATE: &str = r#"# iscsi-fuse configuration
# Edit TargetName and TargetAddress to match your iSCSI target, then re-run iscsi-fuse.

login:
  identity:
    SessionType: Normal
    InitiatorName: "iqn.2024-01.com.iscsi-fuse:initiator"
    InitiatorAlias: "iscsi-fuse-client"
    TargetName: "iqn.2004-04.com.example:target"   # <-- change this
    IsX86: false
  auth:
    AuthMethod: None
  integrity:
    HeaderDigest: None
    DataDigest: None
  flow:
    MaxRecvDataSegmentLength: 262144
    MaxBurstLength: 262144
    FirstBurstLength: 65536
  write_flow:
    InitialR2T: true
    ImmediateData: true
    MaxOutstandingR2T: 1
  ordering:
    DataPDUInOrder: true
    DataSequenceInOrder: true
  recovery:
    ErrorRecoveryLevel: 0
  timers:
    DefaultTime2Wait: 0
    DefaultTime2Retain: 0
  limits:
    MaxConnections: 1
  extensions: {}
  transport:
    TargetAddress: "192.168.1.100:3260"             # <-- change this
    TargetPortalGroupTag: 1
runtime:
  MaxSessions: 1
  TimeoutConnection: 30
"#;

#[derive(Parser, Debug)]
#[command(name = "iscsi-fuse")]
#[command(about = "Mount an iSCSI target as a FUSE filesystem on macOS")]
pub struct CliArgs {
    /// Path to iSCSI YAML config file [default: ~/.iscsi-fuse]
    #[arg(short = 'c', long)]
    pub config: Option<PathBuf>,

    /// FUSE mount point directory (defaults to /Volumes/<volume-name>)
    #[arg(short = 'm', long)]
    pub mount_point: Option<PathBuf>,

    /// Volume name shown in Finder sidebar
    #[arg(short = 'n', long, default_value = "iscsi")]
    pub volume_name: String,

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

impl CliArgs {
    /// Resolve the config path, defaulting to ~/.iscsi-fuse
    pub fn resolved_config(&self) -> PathBuf {
        self.config.clone().unwrap_or_else(default_config_path)
    }

    /// Resolve the mount point, defaulting to /Volumes/<volume_name>
    pub fn resolved_mount_point(&self) -> PathBuf {
        self.mount_point
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("/Volumes/{}", self.volume_name)))
    }
}
