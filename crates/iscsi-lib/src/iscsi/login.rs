#![allow(dead_code)]

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use tracing::debug;

use super::pdu::{Bhs, Opcode, Pdu};
use super::transport::{TransportReader, TransportWriter};

// ---------------------------------------------------------------------------
// NegotiatedParams
// ---------------------------------------------------------------------------

/// Parameters negotiated during the iSCSI login operational phase.
#[derive(Debug, Clone)]
pub struct NegotiatedParams {
    /// Our receive limit (declared to target).
    pub max_recv_data_segment_length: u32,
    /// Target's receive limit (our send limit).
    pub max_send_data_segment_length: u32,
    pub max_burst_length: u32,
    pub first_burst_length: u32,
    pub initial_r2t: bool,
    pub immediate_data: bool,
    pub max_outstanding_r2t: u32,
    pub max_connections: u32,
    pub data_pdu_in_order: bool,
    pub data_sequence_in_order: bool,
    pub error_recovery_level: u8,
    pub header_digest: bool,
    pub data_digest: bool,
    pub default_time2wait: u32,
    pub default_time2retain: u32,
}

impl NegotiatedParams {
    /// Returns 10G-optimized default parameters.
    pub fn defaults_10g() -> Self {
        Self {
            max_recv_data_segment_length: 1_048_576,
            max_send_data_segment_length: 1_048_576,
            max_burst_length: 1_048_576,
            first_burst_length: 262_144,
            initial_r2t: false,
            immediate_data: true,
            max_outstanding_r2t: 8,
            max_connections: 1,
            data_pdu_in_order: true,
            data_sequence_in_order: true,
            error_recovery_level: 0,
            header_digest: true,
            data_digest: true,
            default_time2wait: 2,
            default_time2retain: 20,
        }
    }

    /// Build the operational negotiation text containing all keys with 10G
    /// default values. Each key=value pair is NUL-terminated.
    pub fn build_operational_text() -> String {
        Self::build_operational_text_from(&Self::defaults_10g())
    }

    /// Build the operational negotiation text from custom parameters.
    pub fn build_operational_text_from(defaults: &Self) -> String {
        let mut text = String::new();

        text.push_str(&format!(
            "HeaderDigest={}\0",
            if defaults.header_digest {
                "CRC32C"
            } else {
                "None"
            }
        ));
        text.push_str(&format!(
            "DataDigest={}\0",
            if defaults.data_digest {
                "CRC32C"
            } else {
                "None"
            }
        ));
        text.push_str(&format!(
            "MaxRecvDataSegmentLength={}\0",
            defaults.max_recv_data_segment_length
        ));
        text.push_str(&format!("MaxBurstLength={}\0", defaults.max_burst_length));
        text.push_str(&format!(
            "FirstBurstLength={}\0",
            defaults.first_burst_length
        ));
        text.push_str(&format!(
            "InitialR2T={}\0",
            if defaults.initial_r2t { "Yes" } else { "No" }
        ));
        text.push_str(&format!(
            "ImmediateData={}\0",
            if defaults.immediate_data { "Yes" } else { "No" }
        ));
        text.push_str(&format!(
            "MaxOutstandingR2T={}\0",
            defaults.max_outstanding_r2t
        ));
        text.push_str(&format!("MaxConnections={}\0", defaults.max_connections));
        text.push_str(&format!(
            "DataPDUInOrder={}\0",
            if defaults.data_pdu_in_order {
                "Yes"
            } else {
                "No"
            }
        ));
        text.push_str(&format!(
            "DataSequenceInOrder={}\0",
            if defaults.data_sequence_in_order {
                "Yes"
            } else {
                "No"
            }
        ));
        text.push_str(&format!(
            "ErrorRecoveryLevel={}\0",
            defaults.error_recovery_level
        ));
        text.push_str(&format!(
            "DefaultTime2Wait={}\0",
            defaults.default_time2wait
        ));
        text.push_str(&format!(
            "DefaultTime2Retain={}\0",
            defaults.default_time2retain
        ));

        text
    }

    /// Parse the target's response key-value pairs and apply negotiation
    /// rules to update our parameters.
    pub fn apply_target_response(&mut self, data: &[u8]) -> Result<()> {
        let pairs = parse_kv_pairs(data);

        for (key, value) in pairs {
            match key {
                "HeaderDigest" => {
                    self.header_digest = match value {
                        "CRC32C" => true,
                        "None" => false,
                        _ => bail!("unsupported HeaderDigest value: {}", value),
                    };
                }
                "DataDigest" => {
                    self.data_digest = match value {
                        "CRC32C" => true,
                        "None" => false,
                        _ => bail!("unsupported DataDigest value: {}", value),
                    };
                }
                "MaxRecvDataSegmentLength" => {
                    // Target's MaxRecvDataSegmentLength = our max send limit.
                    let v: u32 = value
                        .parse()
                        .map_err(|_| anyhow!("invalid MaxRecvDataSegmentLength: {}", value))?;
                    self.max_send_data_segment_length = v;
                }
                "MaxBurstLength" => {
                    let v: u32 = value
                        .parse()
                        .map_err(|_| anyhow!("invalid MaxBurstLength: {}", value))?;
                    self.max_burst_length = self.max_burst_length.min(v);
                }
                "FirstBurstLength" => {
                    let v: u32 = value
                        .parse()
                        .map_err(|_| anyhow!("invalid FirstBurstLength: {}", value))?;
                    self.first_burst_length = self.first_burst_length.min(v);
                }
                "InitialR2T" => {
                    // Boolean OR: if either says Yes, result is Yes.
                    let target_val = value == "Yes";
                    self.initial_r2t = self.initial_r2t || target_val;
                }
                "ImmediateData" => {
                    // Boolean AND: Yes only if both agree.
                    let target_val = value == "Yes";
                    self.immediate_data = self.immediate_data && target_val;
                }
                "MaxOutstandingR2T" => {
                    let v: u32 = value
                        .parse()
                        .map_err(|_| anyhow!("invalid MaxOutstandingR2T: {}", value))?;
                    self.max_outstanding_r2t = self.max_outstanding_r2t.min(v);
                }
                "MaxConnections" => {
                    let v: u32 = value
                        .parse()
                        .map_err(|_| anyhow!("invalid MaxConnections: {}", value))?;
                    self.max_connections = self.max_connections.min(v);
                }
                "DataPDUInOrder" => {
                    self.data_pdu_in_order = value == "Yes";
                }
                "DataSequenceInOrder" => {
                    self.data_sequence_in_order = value == "Yes";
                }
                "ErrorRecoveryLevel" => {
                    let v: u8 = value
                        .parse()
                        .map_err(|_| anyhow!("invalid ErrorRecoveryLevel: {}", value))?;
                    self.error_recovery_level = self.error_recovery_level.min(v);
                }
                "DefaultTime2Wait" => {
                    let v: u32 = value
                        .parse()
                        .map_err(|_| anyhow!("invalid DefaultTime2Wait: {}", value))?;
                    self.default_time2wait = v;
                }
                "DefaultTime2Retain" => {
                    let v: u32 = value
                        .parse()
                        .map_err(|_| anyhow!("invalid DefaultTime2Retain: {}", value))?;
                    self.default_time2retain = v;
                }
                _ => {
                    // Ignore unknown keys.
                    debug!(key, value, "ignoring unknown login key");
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// LoginResult
// ---------------------------------------------------------------------------

/// Result of a successful iSCSI login.
pub struct LoginResult {
    pub tsih: u16,
    pub negotiated: NegotiatedParams,
    /// The first CmdSN to use in Full Feature Phase (from target's ExpCmdSN)
    pub initial_cmd_sn: u32,
    /// The first StatSN we expect from the target (last login StatSN + 1)
    pub initial_exp_stat_sn: u32,
}

// ---------------------------------------------------------------------------
// LoginManager
// ---------------------------------------------------------------------------

/// Drives the iSCSI login state machine (security + operational phases).
pub struct LoginManager {
    isid: [u8; 6],
    initiator_name: String,
    target_name: String,
}

impl LoginManager {
    /// Create a new LoginManager with a deterministic ISID (type=0x80 random).
    pub fn new(initiator: &str, target: &str) -> Self {
        Self {
            isid: [0x80, 0x00, 0x00, 0x00, 0x00, 0x01],
            initiator_name: initiator.to_string(),
            target_name: target.to_string(),
        }
    }

    /// Build the security-phase negotiation text.
    pub fn build_security_text(&self) -> String {
        format!(
            "InitiatorName={}\0TargetName={}\0SessionType=Normal\0AuthMethod=None\0",
            self.initiator_name, self.target_name
        )
    }

    /// Perform the full login sequence: security phase then operational phase.
    pub async fn login(
        &self,
        writer: &mut TransportWriter,
        reader: &mut TransportReader,
        cid: u16,
    ) -> Result<LoginResult> {
        self.login_with_params(writer, reader, cid, None).await
    }

    /// Login with optional custom negotiation parameters.
    pub async fn login_with_params(
        &self,
        writer: &mut TransportWriter,
        reader: &mut TransportReader,
        cid: u16,
        custom_params: Option<&NegotiatedParams>,
    ) -> Result<LoginResult> {
        let tsih = self.security_phase(writer, reader, cid).await?;
        let (negotiated, initial_cmd_sn, initial_exp_stat_sn) = self
            .operational_phase_with_params(writer, reader, cid, tsih, custom_params)
            .await?;
        Ok(LoginResult {
            tsih,
            negotiated,
            initial_cmd_sn,
            initial_exp_stat_sn,
        })
    }

    /// Security phase: CSG=0 (SecurityNegotiation), NSG=1
    /// (LoginOperationalNegotiation), T=1 (transit).
    async fn security_phase(
        &self,
        writer: &mut TransportWriter,
        reader: &mut TransportReader,
        cid: u16,
    ) -> Result<u16> {
        let text = self.build_security_text();
        let data = Bytes::from(text.into_bytes());

        let mut bhs = Bhs::build_login_request(
            self.isid, 0, // TSIH=0 for new session
            cid, 1,    // ITT
            1,    // CmdSN
            0,    // ExpStatSN
            0,    // CSG=SecurityNegotiation
            1,    // NSG=LoginOperationalNegotiation
            true, // Transit
        );
        bhs.data_segment_length = data.len() as u32;

        let pdu = Pdu {
            bhs,
            ahs: None,
            data: Some(data),
        };

        writer.send_pdu(&pdu).await?;
        let resp = reader.recv_pdu().await?;

        if resp.bhs.opcode != Opcode::LoginResponse {
            bail!(
                "expected LoginResponse (0x23), got {:?} (0x{:02x})",
                resp.bhs.opcode,
                resp.bhs.opcode as u8
            );
        }

        let status_class = resp.bhs.login_status_class();
        if status_class != 0 {
            bail!(
                "login security phase failed: status-class={}, status-detail={}",
                status_class,
                resp.bhs.login_status_detail()
            );
        }

        let tsih = resp.bhs.tsih();
        debug!(tsih, "security phase complete");
        Ok(tsih)
    }

    /// Operational phase: CSG=1 (LoginOperationalNegotiation), NSG=3
    /// (FullFeaturePhase), T=1 (transit).
    async fn operational_phase(
        &self,
        writer: &mut TransportWriter,
        reader: &mut TransportReader,
        cid: u16,
        tsih: u16,
    ) -> Result<(NegotiatedParams, u32, u32)> {
        self.operational_phase_with_params(writer, reader, cid, tsih, None)
            .await
    }

    /// Operational phase with optional custom params override.
    async fn operational_phase_with_params(
        &self,
        writer: &mut TransportWriter,
        reader: &mut TransportReader,
        cid: u16,
        tsih: u16,
        custom_params: Option<&NegotiatedParams>,
    ) -> Result<(NegotiatedParams, u32, u32)> {
        let text = match custom_params {
            Some(p) => NegotiatedParams::build_operational_text_from(p),
            None => NegotiatedParams::build_operational_text(),
        };
        let data = Bytes::from(text.into_bytes());

        let mut bhs = Bhs::build_login_request(
            self.isid, tsih, cid, 2,    // ITT
            2,    // CmdSN
            1,    // ExpStatSN
            1,    // CSG=LoginOperationalNegotiation
            3,    // NSG=FullFeaturePhase
            true, // Transit
        );
        bhs.data_segment_length = data.len() as u32;

        let pdu = Pdu {
            bhs,
            ahs: None,
            data: Some(data),
        };

        writer.send_pdu(&pdu).await?;
        let resp = reader.recv_pdu().await?;

        if resp.bhs.opcode != Opcode::LoginResponse {
            bail!(
                "expected LoginResponse (0x23), got {:?} (0x{:02x})",
                resp.bhs.opcode,
                resp.bhs.opcode as u8
            );
        }

        let status_class = resp.bhs.login_status_class();
        if status_class != 0 {
            bail!(
                "login operational phase failed: status-class={}, status-detail={}",
                status_class,
                resp.bhs.login_status_detail()
            );
        }

        let mut params = match custom_params {
            Some(p) => p.clone(),
            None => NegotiatedParams::defaults_10g(),
        };

        if let Some(ref resp_data) = resp.data {
            let raw_text = String::from_utf8_lossy(resp_data);
            eprintln!("  [login] target response: {}", raw_text.replace('\0', " | "));
            params.apply_target_response(resp_data)?;
            eprintln!("  [login] negotiated: immediate_data={}, initial_r2t={}, max_send_dsl={}",
                params.immediate_data, params.initial_r2t, params.max_send_data_segment_length);
        }

        // Extract sequence numbers from the final login response for FFP init.
        let initial_cmd_sn = resp.bhs.exp_cmd_sn();
        let initial_exp_stat_sn = resp.bhs.stat_sn().wrapping_add(1);

        debug!(
            ?params,
            initial_cmd_sn, initial_exp_stat_sn, "operational phase complete"
        );
        Ok((params, initial_cmd_sn, initial_exp_stat_sn))
    }
}

// ---------------------------------------------------------------------------
// Key-value text helpers
// ---------------------------------------------------------------------------

/// Parse iSCSI text key=value pairs from NUL-separated data.
///
/// Each pair is terminated by a NUL byte. Returns references into the
/// input slice. Empty entries (e.g. from trailing NUL) are skipped.
pub fn parse_kv_pairs(data: &[u8]) -> Vec<(&str, &str)> {
    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };

    let mut pairs = Vec::new();

    for entry in text.split('\0') {
        if entry.is_empty() {
            continue;
        }
        if let Some((key, value)) = entry.split_once('=') {
            pairs.push((key, value));
        }
    }

    pairs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kv_pairs() {
        // Basic parsing.
        let data = b"Key1=Value1\0Key2=Value2\0";
        let pairs = parse_kv_pairs(data);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("Key1", "Value1"));
        assert_eq!(pairs[1], ("Key2", "Value2"));
    }

    #[test]
    fn test_parse_kv_pairs_trailing_nul() {
        // Trailing NUL should not produce an empty entry.
        let data = b"A=B\0";
        let pairs = parse_kv_pairs(data);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("A", "B"));
    }

    #[test]
    fn test_parse_kv_pairs_double_nul() {
        // Double NUL (empty entry between them) should be skipped.
        let data = b"X=1\0\0Y=2\0";
        let pairs = parse_kv_pairs(data);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("X", "1"));
        assert_eq!(pairs[1], ("Y", "2"));
    }

    #[test]
    fn test_parse_kv_pairs_empty() {
        let pairs = parse_kv_pairs(b"");
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_parse_kv_pairs_no_equals() {
        // Entries without '=' are skipped.
        let data = b"NoEquals\0Key=Val\0";
        let pairs = parse_kv_pairs(data);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("Key", "Val"));
    }

    #[test]
    fn test_negotiated_params_defaults() {
        let p = NegotiatedParams::defaults_10g();
        assert_eq!(p.max_recv_data_segment_length, 1_048_576);
        assert_eq!(p.max_send_data_segment_length, 1_048_576);
        assert_eq!(p.max_burst_length, 1_048_576);
        assert_eq!(p.first_burst_length, 262_144);
        assert!(!p.initial_r2t);
        assert!(p.immediate_data);
        assert_eq!(p.max_outstanding_r2t, 8);
        assert_eq!(p.max_connections, 1);
        assert!(p.data_pdu_in_order);
        assert!(p.data_sequence_in_order);
        assert_eq!(p.error_recovery_level, 0);
        assert!(p.header_digest);
        assert!(p.data_digest);
        assert_eq!(p.default_time2wait, 2);
        assert_eq!(p.default_time2retain, 20);
    }

    #[test]
    fn test_build_security_text() {
        let mgr = LoginManager::new(
            "iqn.2024.com.example:initiator",
            "iqn.2024.com.example:target",
        );
        let text = mgr.build_security_text();
        assert!(text.contains("InitiatorName=iqn.2024.com.example:initiator\0"));
        assert!(text.contains("TargetName=iqn.2024.com.example:target\0"));
        assert!(text.contains("SessionType=Normal\0"));
        assert!(text.contains("AuthMethod=None\0"));
    }

    #[test]
    fn test_build_operational_text() {
        let text = NegotiatedParams::build_operational_text();
        assert!(text.contains("MaxRecvDataSegmentLength=1048576\0"));
        assert!(text.contains("HeaderDigest=CRC32C\0"));
        assert!(text.contains("DataDigest=CRC32C\0"));
        assert!(text.contains("MaxBurstLength=1048576\0"));
        assert!(text.contains("FirstBurstLength=262144\0"));
        assert!(text.contains("InitialR2T=No\0"));
        assert!(text.contains("ImmediateData=Yes\0"));
        assert!(text.contains("MaxOutstandingR2T=8\0"));
        assert!(text.contains("MaxConnections=1\0"));
        assert!(text.contains("DataPDUInOrder=Yes\0"));
        assert!(text.contains("DataSequenceInOrder=Yes\0"));
        assert!(text.contains("ErrorRecoveryLevel=0\0"));
        assert!(text.contains("DefaultTime2Wait=2\0"));
        assert!(text.contains("DefaultTime2Retain=20\0"));
    }

    #[test]
    fn test_apply_target_response() {
        let mut params = NegotiatedParams::defaults_10g();

        // Target says: smaller MaxRecvDataSegmentLength (becomes our send limit),
        // no header digest, smaller max burst, target says InitialR2T=Yes.
        let resp = b"MaxRecvDataSegmentLength=262144\0\
                     HeaderDigest=None\0\
                     DataDigest=CRC32C\0\
                     MaxBurstLength=524288\0\
                     FirstBurstLength=131072\0\
                     InitialR2T=Yes\0\
                     ImmediateData=Yes\0\
                     MaxOutstandingR2T=4\0\
                     ErrorRecoveryLevel=0\0\
                     UnknownKey=Ignored\0";

        params.apply_target_response(resp).unwrap();

        // Target's MaxRecvDataSegmentLength becomes our max_send_data_segment_length.
        assert_eq!(params.max_send_data_segment_length, 262_144);
        // Our max_recv_data_segment_length stays at our declared value.
        assert_eq!(params.max_recv_data_segment_length, 1_048_576);
        // HeaderDigest negotiated to None.
        assert!(!params.header_digest);
        // DataDigest stays CRC32C.
        assert!(params.data_digest);
        // MaxBurstLength = min(1_048_576, 524_288).
        assert_eq!(params.max_burst_length, 524_288);
        // FirstBurstLength = min(262_144, 131_072).
        assert_eq!(params.first_burst_length, 131_072);
        // InitialR2T: boolean OR, either says Yes -> Yes.
        assert!(params.initial_r2t);
        // ImmediateData: boolean AND, both say Yes -> Yes.
        assert!(params.immediate_data);
        // MaxOutstandingR2T = min(8, 4).
        assert_eq!(params.max_outstanding_r2t, 4);
        // ErrorRecoveryLevel = min(0, 0).
        assert_eq!(params.error_recovery_level, 0);
    }

    #[test]
    fn test_apply_target_response_immediate_data_no() {
        // Test ImmediateData boolean AND: target says No -> result is No.
        let mut params = NegotiatedParams::defaults_10g();
        assert!(params.immediate_data); // We start with Yes.

        let resp = b"ImmediateData=No\0";
        params.apply_target_response(resp).unwrap();

        assert!(!params.immediate_data); // AND(Yes, No) = No.
    }

    #[test]
    fn test_apply_target_response_initial_r2t_no() {
        // Test InitialR2T boolean OR: both say No -> result is No.
        let mut params = NegotiatedParams::defaults_10g();
        assert!(!params.initial_r2t); // We start with No.

        let resp = b"InitialR2T=No\0";
        params.apply_target_response(resp).unwrap();

        assert!(!params.initial_r2t); // OR(No, No) = No.
    }

    #[test]
    fn test_login_manager_isid() {
        let mgr = LoginManager::new("iqn.init", "iqn.target");
        assert_eq!(mgr.isid[0], 0x80); // type byte = random
        assert_eq!(mgr.isid, [0x80, 0x00, 0x00, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn test_login_status_accessors() {
        // Build a login response BHS with known status-class/detail.
        let mut raw = [0u8; 48];
        raw[0] = 0x23; // LoginResponse opcode
        raw[36] = 0x02; // Status-Class = 2 (initiator error)
        raw[37] = 0x01; // Status-Detail = 1
        let bhs = Bhs::parse(&raw).unwrap();
        assert_eq!(bhs.login_status_class(), 0x02);
        assert_eq!(bhs.login_status_detail(), 0x01);
    }
}
