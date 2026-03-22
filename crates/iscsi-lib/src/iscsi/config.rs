#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

// ---------------------------------------------------------------------------
// Default value functions for serde
// ---------------------------------------------------------------------------

fn default_initiator() -> String {
    "iqn.2024-01.com.iscsi-fuse:initiator".to_string()
}

fn default_lun() -> u64 {
    0
}

fn default_max_recv_data_segment_length() -> u32 {
    1_048_576
}

fn default_max_burst_length() -> u32 {
    1_048_576
}

fn default_first_burst_length() -> u32 {
    262_144
}

fn default_max_outstanding_r2t() -> u32 {
    8
}

fn default_immediate_data() -> bool {
    true
}

fn default_initial_r2t() -> bool {
    false
}

fn default_header_digest() -> bool {
    true
}

fn default_data_digest() -> bool {
    true
}

fn default_noop_interval_secs() -> u64 {
    5
}

fn default_noop_timeout_secs() -> u64 {
    5
}

fn default_replacement_timeout_secs() -> u64 {
    30
}

fn default_max_login_retries() -> u32 {
    6
}

fn default_login_retry_delay_secs() -> u64 {
    5
}

fn default_cache_size_mb() -> usize {
    128
}

fn default_readahead_max_kb() -> usize {
    8192
}

fn default_write_coalesce_ms() -> u64 {
    5
}

fn default_write_coalesce_max_kb() -> usize {
    1024
}

// ---------------------------------------------------------------------------
// Config — top-level TOML configuration
// ---------------------------------------------------------------------------

/// Top-level iSCSI configuration parsed from a TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// iSCSI target IQN (required).
    pub target: String,

    /// Target portal address, e.g. "192.168.1.100:3260" (required).
    pub address: String,

    /// Initiator IQN.
    #[serde(default = "default_initiator")]
    pub initiator: String,

    /// Logical Unit Number.
    #[serde(default = "default_lun")]
    pub lun: u64,

    /// Performance tuning knobs.
    #[serde(default)]
    pub tuning: TuningConfig,

    /// Connection recovery parameters.
    #[serde(default)]
    pub recovery: RecoveryConfig,

    /// Block cache settings.
    #[serde(default)]
    pub cache: CacheConfig,
}

impl Config {
    /// Load a [`Config`] from a TOML file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse config file: {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// TuningConfig — 10G-optimized defaults
// ---------------------------------------------------------------------------

/// iSCSI session tuning parameters with 10-Gigabit-optimized defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct TuningConfig {
    /// Maximum data segment length the initiator can receive (bytes).
    #[serde(default = "default_max_recv_data_segment_length")]
    pub max_recv_data_segment_length: u32,

    /// Maximum burst length for solicited data (bytes).
    #[serde(default = "default_max_burst_length")]
    pub max_burst_length: u32,

    /// Maximum unsolicited data the initiator may send in the first burst (bytes).
    #[serde(default = "default_first_burst_length")]
    pub first_burst_length: u32,

    /// Maximum number of outstanding R2T PDUs per task.
    #[serde(default = "default_max_outstanding_r2t")]
    pub max_outstanding_r2t: u32,

    /// Allow unsolicited data to be sent immediately with the command PDU.
    #[serde(default = "default_immediate_data")]
    pub immediate_data: bool,

    /// When false, the initiator may send unsolicited data without waiting for R2T.
    #[serde(default = "default_initial_r2t")]
    pub initial_r2t: bool,

    /// Enable CRC32C header digests.
    #[serde(default = "default_header_digest")]
    pub header_digest: bool,

    /// Enable CRC32C data digests.
    #[serde(default = "default_data_digest")]
    pub data_digest: bool,
}

impl Default for TuningConfig {
    fn default() -> Self {
        Self {
            max_recv_data_segment_length: default_max_recv_data_segment_length(),
            max_burst_length: default_max_burst_length(),
            first_burst_length: default_first_burst_length(),
            max_outstanding_r2t: default_max_outstanding_r2t(),
            immediate_data: default_immediate_data(),
            initial_r2t: default_initial_r2t(),
            header_digest: default_header_digest(),
            data_digest: default_data_digest(),
        }
    }
}

// ---------------------------------------------------------------------------
// RecoveryConfig
// ---------------------------------------------------------------------------

/// Connection-recovery and keep-alive parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct RecoveryConfig {
    /// Interval between NOP-Out keep-alive pings (seconds).
    #[serde(default = "default_noop_interval_secs")]
    pub noop_interval_secs: u64,

    /// How long to wait for a NOP-In reply before declaring timeout (seconds).
    #[serde(default = "default_noop_timeout_secs")]
    pub noop_timeout_secs: u64,

    /// How long to attempt session recovery before giving up (seconds).
    #[serde(default = "default_replacement_timeout_secs")]
    pub replacement_timeout_secs: u64,

    /// Maximum number of login retries on connection failure.
    #[serde(default = "default_max_login_retries")]
    pub max_login_retries: u32,

    /// Delay between login retries (seconds).
    #[serde(default = "default_login_retry_delay_secs")]
    pub login_retry_delay_secs: u64,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            noop_interval_secs: default_noop_interval_secs(),
            noop_timeout_secs: default_noop_timeout_secs(),
            replacement_timeout_secs: default_replacement_timeout_secs(),
            max_login_retries: default_max_login_retries(),
            login_retry_delay_secs: default_login_retry_delay_secs(),
        }
    }
}

// ---------------------------------------------------------------------------
// CacheConfig
// ---------------------------------------------------------------------------

/// Block cache configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    /// Total cache size in megabytes.
    #[serde(default = "default_cache_size_mb")]
    pub size_mb: usize,

    /// Maximum readahead window in kilobytes.
    #[serde(default = "default_readahead_max_kb")]
    pub readahead_max_kb: usize,

    /// Write coalescing window in milliseconds.
    #[serde(default = "default_write_coalesce_ms")]
    pub write_coalesce_ms: u64,

    /// Maximum coalesced write size in kilobytes.
    #[serde(default = "default_write_coalesce_max_kb")]
    pub write_coalesce_max_kb: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            size_mb: default_cache_size_mb(),
            readahead_max_kb: default_readahead_max_kb(),
            write_coalesce_ms: default_write_coalesce_ms(),
            write_coalesce_max_kb: default_write_coalesce_max_kb(),
        }
    }
}

// ---------------------------------------------------------------------------
// CONFIG_TEMPLATE — commented TOML showing every field
// ---------------------------------------------------------------------------

/// A fully-commented TOML template that documents every configuration knob.
/// Written to disk on first run so users have a starting point.
pub const CONFIG_TEMPLATE: &str = r#"# iscsi-fuse configuration (TOML)
# ---------------------------------------------------------------
# Required: set these to match your iSCSI target.
# target = "iqn.2004-04.com.example:target"
# address = "192.168.1.100:3260"

# Optional initiator IQN (default shown).
# initiator = "iqn.2024-01.com.iscsi-fuse:initiator"

# Logical Unit Number (default 0).
# lun = 0

# [tuning]
# max_recv_data_segment_length = 1048576   # 1 MB
# max_burst_length             = 1048576   # 1 MB
# first_burst_length           = 262144    # 256 KB
# max_outstanding_r2t          = 8
# immediate_data               = true
# initial_r2t                  = false
# header_digest                = true      # CRC32C
# data_digest                  = true      # CRC32C

# [recovery]
# noop_interval_secs       = 5
# noop_timeout_secs        = 5
# replacement_timeout_secs = 30
# max_login_retries        = 6
# login_retry_delay_secs   = 5

# [cache]
# size_mb             = 128
# readahead_max_kb    = 8192   # 8 MB
# write_coalesce_ms   = 5
# write_coalesce_max_kb = 1024 # 1 MB
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minimal_config() {
        let toml_str = r#"
            target = "iqn.2004-04.com.example:target"
            address = "192.168.1.100:3260"
        "#;

        let cfg: Config = toml::from_str(toml_str).expect("should parse minimal config");

        // Required fields
        assert_eq!(cfg.target, "iqn.2004-04.com.example:target");
        assert_eq!(cfg.address, "192.168.1.100:3260");

        // Defaults
        assert_eq!(cfg.initiator, "iqn.2024-01.com.iscsi-fuse:initiator");
        assert_eq!(cfg.lun, 0);

        // Tuning defaults (10G-optimized)
        assert_eq!(cfg.tuning.max_recv_data_segment_length, 1_048_576);
        assert_eq!(cfg.tuning.max_burst_length, 1_048_576);
        assert_eq!(cfg.tuning.first_burst_length, 262_144);
        assert_eq!(cfg.tuning.max_outstanding_r2t, 8);
        assert!(cfg.tuning.immediate_data);
        assert!(!cfg.tuning.initial_r2t);
        assert!(cfg.tuning.header_digest);
        assert!(cfg.tuning.data_digest);

        // Recovery defaults
        assert_eq!(cfg.recovery.noop_interval_secs, 5);
        assert_eq!(cfg.recovery.noop_timeout_secs, 5);
        assert_eq!(cfg.recovery.replacement_timeout_secs, 30);
        assert_eq!(cfg.recovery.max_login_retries, 6);
        assert_eq!(cfg.recovery.login_retry_delay_secs, 5);

        // Cache defaults
        assert_eq!(cfg.cache.size_mb, 128);
        assert_eq!(cfg.cache.readahead_max_kb, 8192);
        assert_eq!(cfg.cache.write_coalesce_ms, 5);
        assert_eq!(cfg.cache.write_coalesce_max_kb, 1024);
    }

    #[test]
    fn test_full_config() {
        let toml_str = r#"
            target = "iqn.2025-01.com.mysan:vol1"
            address = "10.0.0.1:3260"
            initiator = "iqn.2025-01.com.client:node1"
            lun = 3

            [tuning]
            max_recv_data_segment_length = 524288
            max_burst_length = 524288
            first_burst_length = 131072
            max_outstanding_r2t = 4
            immediate_data = false
            initial_r2t = true
            header_digest = false
            data_digest = false

            [recovery]
            noop_interval_secs = 10
            noop_timeout_secs = 15
            replacement_timeout_secs = 60
            max_login_retries = 3
            login_retry_delay_secs = 10

            [cache]
            size_mb = 256
            readahead_max_kb = 4096
            write_coalesce_ms = 10
            write_coalesce_max_kb = 2048
        "#;

        let cfg: Config = toml::from_str(toml_str).expect("should parse full config");

        assert_eq!(cfg.target, "iqn.2025-01.com.mysan:vol1");
        assert_eq!(cfg.address, "10.0.0.1:3260");
        assert_eq!(cfg.initiator, "iqn.2025-01.com.client:node1");
        assert_eq!(cfg.lun, 3);

        assert_eq!(cfg.tuning.max_recv_data_segment_length, 524_288);
        assert_eq!(cfg.tuning.max_burst_length, 524_288);
        assert_eq!(cfg.tuning.first_burst_length, 131_072);
        assert_eq!(cfg.tuning.max_outstanding_r2t, 4);
        assert!(!cfg.tuning.immediate_data);
        assert!(cfg.tuning.initial_r2t);
        assert!(!cfg.tuning.header_digest);
        assert!(!cfg.tuning.data_digest);

        assert_eq!(cfg.recovery.noop_interval_secs, 10);
        assert_eq!(cfg.recovery.noop_timeout_secs, 15);
        assert_eq!(cfg.recovery.replacement_timeout_secs, 60);
        assert_eq!(cfg.recovery.max_login_retries, 3);
        assert_eq!(cfg.recovery.login_retry_delay_secs, 10);

        assert_eq!(cfg.cache.size_mb, 256);
        assert_eq!(cfg.cache.readahead_max_kb, 4096);
        assert_eq!(cfg.cache.write_coalesce_ms, 10);
        assert_eq!(cfg.cache.write_coalesce_max_kb, 2048);
    }

    #[test]
    fn test_missing_required_field() {
        // Missing `target` — should fail.
        let toml_str = r#"
            address = "192.168.1.100:3260"
        "#;

        let result: std::result::Result<Config, _> = toml::from_str(toml_str);
        assert!(result.is_err(), "missing target should produce an error");
    }
}
