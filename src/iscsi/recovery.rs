#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use bytes::Bytes;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::iscsi::login::LoginManager;
use crate::iscsi::session::{PduResponse, Session};

// ---------------------------------------------------------------------------
// RecoveryConfig
// ---------------------------------------------------------------------------

/// Configuration for NOP keepalive and session recovery behaviour.
#[derive(Clone, Debug)]
pub struct RecoveryConfig {
    /// Interval between NOP-Out keep-alive pings.
    pub noop_interval: Duration,
    /// How long to wait for a NOP-In reply before declaring timeout.
    pub noop_timeout: Duration,
    /// How long to attempt session recovery before giving up.
    pub replacement_timeout: Duration,
    /// Maximum number of login retries on connection failure.
    pub max_login_retries: u32,
    /// Delay between login retries.
    pub login_retry_delay: Duration,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            noop_interval: Duration::from_secs(5),
            noop_timeout: Duration::from_secs(5),
            replacement_timeout: Duration::from_secs(30),
            max_login_retries: 6,
            login_retry_delay: Duration::from_secs(5),
        }
    }
}

impl From<crate::iscsi::config::RecoveryConfig> for RecoveryConfig {
    fn from(cfg: crate::iscsi::config::RecoveryConfig) -> Self {
        Self {
            noop_interval: Duration::from_secs(cfg.noop_interval_secs),
            noop_timeout: Duration::from_secs(cfg.noop_timeout_secs),
            replacement_timeout: Duration::from_secs(cfg.replacement_timeout_secs),
            max_login_retries: cfg.max_login_retries,
            login_retry_delay: Duration::from_secs(cfg.login_retry_delay_secs),
        }
    }
}

// ---------------------------------------------------------------------------
// PendingCommand
// ---------------------------------------------------------------------------

/// A SCSI command that has been queued for (re-)submission after recovery.
pub struct PendingCommand {
    pub cdb: [u8; 16],
    pub lun: u64,
    pub edtl: u32,
    pub read: bool,
    pub write: bool,
    pub write_data: Option<Bytes>,
    pub reply: oneshot::Sender<Result<PduResponse>>,
    pub queued_at: Instant,
}

// ---------------------------------------------------------------------------
// PendingQueue
// ---------------------------------------------------------------------------

/// Queue of SCSI commands waiting to be re-submitted after session recovery.
pub struct PendingQueue {
    commands: Vec<PendingCommand>,
}

impl PendingQueue {
    /// Create an empty queue.
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// Add a command to the queue.
    pub fn push(&mut self, cmd: PendingCommand) {
        self.commands.push(cmd);
    }

    /// Returns true if the queue has no commands.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Take all queued commands, leaving the queue empty.
    pub fn drain(&mut self) -> Vec<PendingCommand> {
        std::mem::take(&mut self.commands)
    }

    /// Remove and fail commands that have been queued longer than `timeout`.
    ///
    /// Returns the number of expired commands.
    pub fn expire(&mut self, timeout: Duration) -> usize {
        let now = Instant::now();
        let mut expired = 0usize;
        let mut kept = Vec::new();

        for cmd in self.commands.drain(..) {
            let age = now.duration_since(cmd.queued_at);
            if age >= timeout {
                let _ = cmd.reply.send(Err(anyhow!(
                    "command expired after {:.1}s (timeout {:.1}s)",
                    age.as_secs_f64(),
                    timeout.as_secs_f64(),
                )));
                expired += 1;
            } else {
                kept.push(cmd);
            }
        }

        self.commands = kept;
        expired
    }

    /// Fail all queued commands with the given error message.
    pub fn fail_all(&mut self, err_msg: &str) {
        for cmd in self.commands.drain(..) {
            let _ = cmd.reply.send(Err(anyhow!("{}", err_msg)));
        }
    }
}

// ---------------------------------------------------------------------------
// RecoveryManager
// ---------------------------------------------------------------------------

/// Manages NOP keepalive probes and automatic session recovery.
pub struct RecoveryManager {
    session: Arc<Session>,
    login_mgr: Arc<LoginManager>,
    target_addr: String,
    config: RecoveryConfig,
    pending_queue: Mutex<PendingQueue>,
}

impl RecoveryManager {
    /// Construct a new RecoveryManager.
    pub fn new(
        session: Arc<Session>,
        login_mgr: Arc<LoginManager>,
        target_addr: String,
        config: RecoveryConfig,
    ) -> Self {
        Self {
            session,
            login_mgr,
            target_addr,
            config,
            pending_queue: Mutex::new(PendingQueue::new()),
        }
    }

    /// Spawn a background keepalive task.
    ///
    /// The task periodically checks `time_since_last_recv` on the session.
    /// If the session has been idle for longer than `noop_interval`, it sends
    /// a NOP-Out and waits up to `noop_timeout` for any incoming PDU activity.
    /// On timeout or send error, a warning is logged (full recovery integration
    /// will be added in a later task).
    pub fn spawn_keepalive(&self) -> JoinHandle<()> {
        let session = Arc::clone(&self.session);
        let interval = self.config.noop_interval;
        let timeout = self.config.noop_timeout;

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;

                let idle = session.time_since_last_recv();
                if idle < interval {
                    debug!(
                        idle_ms = idle.as_millis(),
                        "session active, skipping NOP-Out"
                    );
                    continue;
                }

                debug!("session idle for {:?}, sending NOP-Out", idle);
                if let Err(e) = session.send_nop_out().await {
                    warn!("failed to send NOP-Out: {e}");
                    continue;
                }

                // Wait for the receiver task to update last_recv via an
                // incoming NOP-In (or any other PDU). We poll the timestamp
                // rather than waiting on a specific ITT since unsolicited
                // NOP-Outs use ITT=0xFFFFFFFF and are answered with a
                // target-initiated NOP-In.
                let deadline = tokio::time::Instant::now() + timeout;
                let mut got_reply = false;
                while tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if session.time_since_last_recv() < timeout {
                        got_reply = true;
                        break;
                    }
                }

                if !got_reply {
                    warn!("NOP-Out timeout: no response within {:?}", timeout);
                }
            }
        })
    }

    /// Access the recovery configuration.
    pub fn config(&self) -> &RecoveryConfig {
        &self.config
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        let (tx, _rx) = oneshot::channel();
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

    #[test]
    fn test_pending_queue_fail_all() {
        let mut queue = PendingQueue::new();
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        queue.push(PendingCommand {
            cdb: [0u8; 16],
            lun: 0,
            edtl: 0,
            read: true,
            write: false,
            write_data: None,
            reply: tx1,
            queued_at: Instant::now(),
        });
        queue.push(PendingCommand {
            cdb: [0u8; 16],
            lun: 0,
            edtl: 0,
            read: false,
            write: true,
            write_data: Some(Bytes::from_static(b"data")),
            reply: tx2,
            queued_at: Instant::now(),
        });
        queue.fail_all("test failure");
        assert!(queue.is_empty());
        // Receivers should get errors
        assert!(rx1.blocking_recv().unwrap().is_err());
        assert!(rx2.blocking_recv().unwrap().is_err());
    }

    #[test]
    fn test_recovery_config_from_config_recovery() {
        let cfg = crate::iscsi::config::RecoveryConfig {
            noop_interval_secs: 10,
            noop_timeout_secs: 15,
            replacement_timeout_secs: 60,
            max_login_retries: 3,
            login_retry_delay_secs: 10,
        };
        let rc: RecoveryConfig = cfg.into();
        assert_eq!(rc.noop_interval, Duration::from_secs(10));
        assert_eq!(rc.noop_timeout, Duration::from_secs(15));
        assert_eq!(rc.replacement_timeout, Duration::from_secs(60));
        assert_eq!(rc.max_login_retries, 3);
        assert_eq!(rc.login_retry_delay, Duration::from_secs(10));
    }

    #[test]
    fn test_pending_queue_drain() {
        let mut queue = PendingQueue::new();
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        queue.push(PendingCommand {
            cdb: [0u8; 16],
            lun: 0,
            edtl: 0,
            read: true,
            write: false,
            write_data: None,
            reply: tx1,
            queued_at: Instant::now(),
        });
        queue.push(PendingCommand {
            cdb: [1u8; 16],
            lun: 1,
            edtl: 512,
            read: false,
            write: true,
            write_data: Some(Bytes::from_static(b"data")),
            reply: tx2,
            queued_at: Instant::now(),
        });
        assert!(!queue.is_empty());
        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_pending_queue_expire_keeps_fresh() {
        let mut queue = PendingQueue::new();
        // One expired command
        let (tx_old, _rx_old) = oneshot::channel();
        queue.push(PendingCommand {
            cdb: [0u8; 16],
            lun: 0,
            edtl: 0,
            read: true,
            write: false,
            write_data: None,
            reply: tx_old,
            queued_at: Instant::now() - Duration::from_secs(60),
        });
        // One fresh command
        let (tx_new, _rx_new) = oneshot::channel();
        queue.push(PendingCommand {
            cdb: [0u8; 16],
            lun: 0,
            edtl: 0,
            read: true,
            write: false,
            write_data: None,
            reply: tx_new,
            queued_at: Instant::now(),
        });
        let expired = queue.expire(Duration::from_secs(30));
        assert_eq!(expired, 1);
        assert!(!queue.is_empty()); // fresh one remains
        assert_eq!(queue.commands.len(), 1);
    }
}
