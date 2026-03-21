use anyhow::{Result, anyhow};
use bytes::Bytes;

// ---------------------------------------------------------------------------
// Opcode
// ---------------------------------------------------------------------------

/// iSCSI PDU opcodes per RFC 7143.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    // Initiator opcodes
    NopOut = 0x00,
    ScsiCommand = 0x01,
    TaskMgmt = 0x02,
    LoginRequest = 0x03,
    TextRequest = 0x04,
    ScsiDataOut = 0x05,
    LogoutRequest = 0x06,
    SnackRequest = 0x10,

    // Target opcodes
    NopIn = 0x20,
    ScsiResponse = 0x21,
    TaskMgmtResp = 0x22,
    LoginResponse = 0x23,
    TextResponse = 0x24,
    ScsiDataIn = 0x25,
    LogoutResponse = 0x26,
    R2t = 0x31,
    AsyncMessage = 0x32,
    Reject = 0x3f,
}

impl TryFrom<u8> for Opcode {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0x00 => Ok(Opcode::NopOut),
            0x01 => Ok(Opcode::ScsiCommand),
            0x02 => Ok(Opcode::TaskMgmt),
            0x03 => Ok(Opcode::LoginRequest),
            0x04 => Ok(Opcode::TextRequest),
            0x05 => Ok(Opcode::ScsiDataOut),
            0x06 => Ok(Opcode::LogoutRequest),
            0x10 => Ok(Opcode::SnackRequest),
            0x20 => Ok(Opcode::NopIn),
            0x21 => Ok(Opcode::ScsiResponse),
            0x22 => Ok(Opcode::TaskMgmtResp),
            0x23 => Ok(Opcode::LoginResponse),
            0x24 => Ok(Opcode::TextResponse),
            0x25 => Ok(Opcode::ScsiDataIn),
            0x26 => Ok(Opcode::LogoutResponse),
            0x31 => Ok(Opcode::R2t),
            0x32 => Ok(Opcode::AsyncMessage),
            0x3f => Ok(Opcode::Reject),
            other => Err(anyhow!("unknown iSCSI opcode: 0x{:02x}", other)),
        }
    }
}

// ---------------------------------------------------------------------------
// Bhs — Basic Header Segment (48 bytes)
// ---------------------------------------------------------------------------

/// The 48-byte Basic Header Segment of an iSCSI PDU.
///
/// We keep the raw wire-format bytes alongside parsed common fields so that
/// opcode-specific accessors can read directly from `raw` without ambiguity.
#[derive(Debug, Clone)]
pub struct Bhs {
    raw: [u8; 48],
    pub opcode: Opcode,
    pub immediate: bool,
    pub flags: u8,
    pub data_segment_length: u32,
    pub lun: u64,
    pub itt: u32,
}

impl Bhs {
    // -----------------------------------------------------------------------
    // Serialize / Parse
    // -----------------------------------------------------------------------

    /// Serialize the BHS to its 48-byte big-endian wire format.
    pub fn serialize(&self) -> [u8; 48] {
        let mut buf = [0u8; 48];

        // byte 0: immediate flag | opcode
        buf[0] = (if self.immediate { 0x40 } else { 0x00 }) | (self.opcode as u8);
        // byte 1: flags
        buf[1] = self.flags;
        // byte 2: AHS length (from raw)
        buf[2] = self.raw[2];
        // byte 3: reserved / status (from raw)
        buf[3] = self.raw[3];
        // bytes 4-7: DataSegmentLength (byte 4 reserved, bytes 5-7 = lower 24 bits)
        buf[4] = 0;
        buf[5] = (self.data_segment_length >> 16) as u8;
        buf[6] = (self.data_segment_length >> 8) as u8;
        buf[7] = self.data_segment_length as u8;
        // bytes 8-47: copy from raw (LUN/ISID area, ITT, and opcode-specific
        // fields are all maintained in raw by the builders and parse).
        buf[8..48].copy_from_slice(&self.raw[8..48]);

        buf
    }

    /// Parse a 48-byte buffer into a BHS.
    pub fn parse(buf: &[u8; 48]) -> Result<Self> {
        let immediate = buf[0] & 0x40 != 0;
        let opcode = Opcode::try_from(buf[0] & 0x3F)?;
        let flags = buf[1];
        let data_segment_length =
            ((buf[5] as u32) << 16) | ((buf[6] as u32) << 8) | (buf[7] as u32);
        let lun = u64::from_be_bytes(buf[8..16].try_into().unwrap());
        let itt = u32::from_be_bytes(buf[16..20].try_into().unwrap());

        let mut raw = [0u8; 48];
        raw.copy_from_slice(buf);

        Ok(Self {
            raw,
            opcode,
            immediate,
            flags,
            data_segment_length,
            lun,
            itt,
        })
    }

    // -----------------------------------------------------------------------
    // Opcode-specific accessors (read from self.raw)
    // -----------------------------------------------------------------------

    /// SCSI status byte (absolute byte 3). Valid for ScsiResponse and
    /// Data-In with S bit set.
    pub fn scsi_status(&self) -> u8 {
        self.raw[3]
    }

    /// S (status) bit — bit 0 of flags byte. Used by Data-In.
    pub fn status_flag(&self) -> bool {
        self.raw[1] & 0x01 != 0
    }

    /// F (final) bit — bit 7 of flags byte.
    pub fn final_flag(&self) -> bool {
        self.raw[1] & 0x80 != 0
    }

    /// StatSN (raw[24..28]).
    pub fn stat_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[24..28].try_into().unwrap())
    }

    /// ExpCmdSN (raw[28..32]).
    pub fn exp_cmd_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[28..32].try_into().unwrap())
    }

    /// MaxCmdSN (raw[32..36]).
    pub fn max_cmd_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[32..36].try_into().unwrap())
    }

    /// DataSN (raw[36..40]).
    pub fn data_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[36..40].try_into().unwrap())
    }

    /// Buffer Offset (raw[40..44]).
    pub fn buffer_offset(&self) -> u32 {
        u32::from_be_bytes(self.raw[40..44].try_into().unwrap())
    }

    /// Target Transfer Tag (raw[20..24]).
    pub fn ttt(&self) -> u32 {
        u32::from_be_bytes(self.raw[20..24].try_into().unwrap())
    }

    /// R2T Buffer Offset (raw[40..44]).
    pub fn r2t_buffer_offset(&self) -> u32 {
        u32::from_be_bytes(self.raw[40..44].try_into().unwrap())
    }

    /// R2T Desired Data Transfer Length (raw[44..48]).
    pub fn r2t_desired_length(&self) -> u32 {
        u32::from_be_bytes(self.raw[44..48].try_into().unwrap())
    }

    /// R2TSN (raw[36..40]).
    pub fn r2t_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[36..40].try_into().unwrap())
    }

    /// TSIH (raw[14..16]) — used by Login Request/Response.
    pub fn tsih(&self) -> u16 {
        u16::from_be_bytes(self.raw[14..16].try_into().unwrap())
    }

    /// CmdSN (raw[24..28]) — for initiator PDUs (same offset as StatSN).
    pub fn cmd_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[24..28].try_into().unwrap())
    }

    /// ExpStatSN (raw[28..32]) — for initiator PDUs.
    pub fn exp_stat_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[28..32].try_into().unwrap())
    }

    // -----------------------------------------------------------------------
    // Builder helpers (private)
    // -----------------------------------------------------------------------

    /// Create a zeroed BHS and fill in the raw bytes from it.
    fn new_zeroed() -> Self {
        Self {
            raw: [0u8; 48],
            opcode: Opcode::NopOut,
            immediate: false,
            flags: 0,
            data_segment_length: 0,
            lun: 0,
            itt: 0,
        }
    }

    /// Sync the `raw` bytes from the parsed fields.  Call after setting
    /// common fields and before setting opcode-specific raw bytes.
    fn sync_raw_from_fields(&mut self) {
        self.raw[0] = (if self.immediate { 0x40 } else { 0x00 }) | (self.opcode as u8);
        self.raw[1] = self.flags;
        // bytes 5-7: data_segment_length lower 24 bits
        self.raw[5] = (self.data_segment_length >> 16) as u8;
        self.raw[6] = (self.data_segment_length >> 8) as u8;
        self.raw[7] = self.data_segment_length as u8;
        self.raw[8..16].copy_from_slice(&self.lun.to_be_bytes());
        self.raw[16..20].copy_from_slice(&self.itt.to_be_bytes());
    }

    /// Write a big-endian u32 into raw at the given offset.
    fn set_raw_u32(&mut self, offset: usize, value: u32) {
        self.raw[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    /// Write a big-endian u16 into raw at the given offset.
    fn set_raw_u16(&mut self, offset: usize, value: u16) {
        self.raw[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }

    // -----------------------------------------------------------------------
    // Initiator-side builders
    // -----------------------------------------------------------------------

    /// Build a SCSI Command PDU (opcode 0x01).
    pub fn build_scsi_command(
        lun: u64,
        itt: u32,
        cmd_sn: u32,
        exp_stat_sn: u32,
        cdb: &[u8; 16],
        edtl: u32,
        read: bool,
        write: bool,
    ) -> Self {
        let mut bhs = Self::new_zeroed();
        bhs.opcode = Opcode::ScsiCommand;
        bhs.immediate = false;
        bhs.flags = 0x80 | if read { 0x40 } else { 0 } | if write { 0x20 } else { 0 };
        bhs.lun = lun;
        bhs.itt = itt;
        bhs.sync_raw_from_fields();

        // bytes 20-23: Expected Data Transfer Length
        bhs.set_raw_u32(20, edtl);
        // bytes 24-27: CmdSN
        bhs.set_raw_u32(24, cmd_sn);
        // bytes 28-31: ExpStatSN
        bhs.set_raw_u32(28, exp_stat_sn);
        // bytes 32-47: CDB (16 bytes)
        bhs.raw[32..48].copy_from_slice(cdb);

        bhs
    }

    /// Build a SCSI Data-Out PDU (opcode 0x05).
    pub fn build_data_out(
        lun: u64,
        itt: u32,
        ttt: u32,
        exp_stat_sn: u32,
        data_sn: u32,
        buffer_offset: u32,
    ) -> Self {
        let mut bhs = Self::new_zeroed();
        bhs.opcode = Opcode::ScsiDataOut;
        bhs.immediate = false;
        bhs.flags = 0x80; // F bit set
        bhs.lun = lun;
        bhs.itt = itt;
        bhs.sync_raw_from_fields();

        // bytes 20-23: TTT
        bhs.set_raw_u32(20, ttt);
        // bytes 28-31: ExpStatSN
        bhs.set_raw_u32(28, exp_stat_sn);
        // bytes 36-39: DataSN
        bhs.set_raw_u32(36, data_sn);
        // bytes 40-43: Buffer Offset
        bhs.set_raw_u32(40, buffer_offset);

        bhs
    }

    /// Build a Login Request PDU (opcode 0x03, always immediate).
    pub fn build_login_request(
        isid: [u8; 6],
        tsih: u16,
        cid: u16,
        itt: u32,
        cmd_sn: u32,
        exp_stat_sn: u32,
        csg: u8,
        nsg: u8,
        transit: bool,
    ) -> Self {
        let mut bhs = Self::new_zeroed();
        bhs.opcode = Opcode::LoginRequest;
        bhs.immediate = true;
        bhs.flags = ((transit as u8) << 7) | ((csg & 0x03) << 2) | (nsg & 0x03);
        bhs.itt = itt;
        bhs.sync_raw_from_fields();

        // byte 2: VersionMax = 0x00
        bhs.raw[2] = 0x00;
        // byte 3: VersionMin = 0x00
        bhs.raw[3] = 0x00;
        // bytes 8-13: ISID
        bhs.raw[8..14].copy_from_slice(&isid);
        // bytes 14-15: TSIH
        bhs.set_raw_u16(14, tsih);
        // bytes 16-19: ITT (already set via sync_raw_from_fields)
        // bytes 20-21: CID
        bhs.set_raw_u16(20, cid);
        // bytes 24-27: CmdSN
        bhs.set_raw_u32(24, cmd_sn);
        // bytes 28-31: ExpStatSN
        bhs.set_raw_u32(28, exp_stat_sn);

        bhs
    }

    /// Build a NOP-Out PDU (opcode 0x00, always immediate).
    pub fn build_nop_out(itt: u32, ttt: u32, cmd_sn: u32, exp_stat_sn: u32) -> Self {
        let mut bhs = Self::new_zeroed();
        bhs.opcode = Opcode::NopOut;
        bhs.immediate = true;
        bhs.flags = 0x80; // F bit
        bhs.itt = itt;
        bhs.sync_raw_from_fields();

        // bytes 20-23: TTT
        bhs.set_raw_u32(20, ttt);
        // bytes 24-27: CmdSN
        bhs.set_raw_u32(24, cmd_sn);
        // bytes 28-31: ExpStatSN
        bhs.set_raw_u32(28, exp_stat_sn);

        bhs
    }

    /// Build a Logout Request PDU (opcode 0x06, always immediate).
    pub fn build_logout_request(itt: u32, cmd_sn: u32, exp_stat_sn: u32, cid: u16) -> Self {
        let mut bhs = Self::new_zeroed();
        bhs.opcode = Opcode::LogoutRequest;
        bhs.immediate = true;
        bhs.flags = 0x80; // F=1, reason=0x00 (close session)
        bhs.itt = itt;
        bhs.sync_raw_from_fields();

        // bytes 20-21: CID
        bhs.set_raw_u16(20, cid);
        // bytes 24-27: CmdSN
        bhs.set_raw_u32(24, cmd_sn);
        // bytes 28-31: ExpStatSN
        bhs.set_raw_u32(28, exp_stat_sn);

        bhs
    }

    // -----------------------------------------------------------------------
    // Target-side builders (for testing)
    // -----------------------------------------------------------------------

    /// Build a SCSI Response PDU (opcode 0x21).
    pub fn build_scsi_response(
        itt: u32,
        scsi_status: u8,
        stat_sn: u32,
        exp_cmd_sn: u32,
        max_cmd_sn: u32,
    ) -> Self {
        let mut bhs = Self::new_zeroed();
        bhs.opcode = Opcode::ScsiResponse;
        bhs.immediate = false;
        bhs.flags = 0x80; // F=1
        bhs.itt = itt;
        bhs.sync_raw_from_fields();

        // byte 3: SCSI status
        bhs.raw[3] = scsi_status;
        // bytes 24-27: StatSN
        bhs.set_raw_u32(24, stat_sn);
        // bytes 28-31: ExpCmdSN
        bhs.set_raw_u32(28, exp_cmd_sn);
        // bytes 32-35: MaxCmdSN
        bhs.set_raw_u32(32, max_cmd_sn);

        bhs
    }

    /// Build a Data-In PDU (opcode 0x25).
    pub fn build_data_in(
        itt: u32,
        data_sn: u32,
        buffer_offset: u32,
        data_len: u32,
        status_flag: bool,
        scsi_status: u8,
        stat_sn: u32,
        exp_cmd_sn: u32,
        max_cmd_sn: u32,
    ) -> Self {
        let mut bhs = Self::new_zeroed();
        bhs.opcode = Opcode::ScsiDataIn;
        bhs.immediate = false;
        bhs.flags = 0x80 | if status_flag { 0x01 } else { 0x00 };
        bhs.data_segment_length = data_len;
        bhs.itt = itt;
        bhs.sync_raw_from_fields();

        // byte 3: SCSI status (if status flag set)
        if status_flag {
            bhs.raw[3] = scsi_status;
        }
        // bytes 24-27: StatSN (if status flag set)
        if status_flag {
            bhs.set_raw_u32(24, stat_sn);
        }
        // bytes 28-31: ExpCmdSN
        bhs.set_raw_u32(28, exp_cmd_sn);
        // bytes 32-35: MaxCmdSN
        bhs.set_raw_u32(32, max_cmd_sn);
        // bytes 36-39: DataSN
        bhs.set_raw_u32(36, data_sn);
        // bytes 40-43: Buffer Offset
        bhs.set_raw_u32(40, buffer_offset);

        bhs
    }

    /// Build a NOP-In PDU (opcode 0x20).
    pub fn build_nop_in(
        itt: u32,
        ttt: u32,
        stat_sn: u32,
        exp_cmd_sn: u32,
        max_cmd_sn: u32,
    ) -> Self {
        let mut bhs = Self::new_zeroed();
        bhs.opcode = Opcode::NopIn;
        bhs.immediate = false;
        bhs.flags = 0x80; // F=1
        bhs.itt = itt;
        bhs.sync_raw_from_fields();

        // bytes 20-23: TTT
        bhs.set_raw_u32(20, ttt);
        // bytes 24-27: StatSN
        bhs.set_raw_u32(24, stat_sn);
        // bytes 28-31: ExpCmdSN
        bhs.set_raw_u32(28, exp_cmd_sn);
        // bytes 32-35: MaxCmdSN
        bhs.set_raw_u32(32, max_cmd_sn);

        bhs
    }

    /// Build an R2T PDU (opcode 0x31).
    pub fn build_r2t(
        itt: u32,
        ttt: u32,
        stat_sn: u32,
        exp_cmd_sn: u32,
        max_cmd_sn: u32,
        r2t_sn: u32,
        buffer_offset: u32,
        desired_length: u32,
    ) -> Self {
        let mut bhs = Self::new_zeroed();
        bhs.opcode = Opcode::R2t;
        bhs.immediate = false;
        bhs.flags = 0x80; // F=1
        bhs.itt = itt;
        bhs.sync_raw_from_fields();

        // bytes 20-23: TTT
        bhs.set_raw_u32(20, ttt);
        // bytes 24-27: StatSN
        bhs.set_raw_u32(24, stat_sn);
        // bytes 28-31: ExpCmdSN
        bhs.set_raw_u32(28, exp_cmd_sn);
        // bytes 32-35: MaxCmdSN
        bhs.set_raw_u32(32, max_cmd_sn);
        // bytes 36-39: R2TSN
        bhs.set_raw_u32(36, r2t_sn);
        // bytes 40-43: Buffer Offset
        bhs.set_raw_u32(40, buffer_offset);
        // bytes 44-47: Desired Data Transfer Length
        bhs.set_raw_u32(44, desired_length);

        bhs
    }
}

// ---------------------------------------------------------------------------
// Pdu
// ---------------------------------------------------------------------------

/// A complete iSCSI Protocol Data Unit: BHS + optional AHS + optional data.
#[derive(Debug, Clone)]
pub struct Pdu {
    pub bhs: Bhs,
    pub ahs: Option<Bytes>,
    pub data: Option<Bytes>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Round `len` up to the nearest multiple of 4 (iSCSI padding).
pub fn pad_to_4(len: usize) -> usize {
    (len + 3) & !3
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opcode_values() {
        // Initiator opcodes
        assert_eq!(Opcode::NopOut as u8, 0x00);
        assert_eq!(Opcode::ScsiCommand as u8, 0x01);
        assert_eq!(Opcode::TaskMgmt as u8, 0x02);
        assert_eq!(Opcode::LoginRequest as u8, 0x03);
        assert_eq!(Opcode::TextRequest as u8, 0x04);
        assert_eq!(Opcode::ScsiDataOut as u8, 0x05);
        assert_eq!(Opcode::LogoutRequest as u8, 0x06);
        assert_eq!(Opcode::SnackRequest as u8, 0x10);
        // Target opcodes
        assert_eq!(Opcode::NopIn as u8, 0x20);
        assert_eq!(Opcode::ScsiResponse as u8, 0x21);
        assert_eq!(Opcode::TaskMgmtResp as u8, 0x22);
        assert_eq!(Opcode::LoginResponse as u8, 0x23);
        assert_eq!(Opcode::TextResponse as u8, 0x24);
        assert_eq!(Opcode::ScsiDataIn as u8, 0x25);
        assert_eq!(Opcode::LogoutResponse as u8, 0x26);
        assert_eq!(Opcode::R2t as u8, 0x31);
        assert_eq!(Opcode::AsyncMessage as u8, 0x32);
        assert_eq!(Opcode::Reject as u8, 0x3f);

        // Round-trip through TryFrom
        for &val in &[
            0x00u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x10, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25,
            0x26, 0x31, 0x32, 0x3f,
        ] {
            let op = Opcode::try_from(val).unwrap();
            assert_eq!(op as u8, val);
        }

        // Unknown opcode must fail
        assert!(Opcode::try_from(0xFF).is_err());
        assert!(Opcode::try_from(0x07).is_err());
    }

    #[test]
    fn test_scsi_command_round_trip() {
        let cdb = [
            0x28, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        let lun = 0x0001_0000_0000_0000u64;
        let itt = 42;
        let cmd_sn = 100;
        let exp_stat_sn = 200;
        let edtl = 512;

        let bhs = Bhs::build_scsi_command(lun, itt, cmd_sn, exp_stat_sn, &cdb, edtl, true, false);

        assert_eq!(bhs.opcode, Opcode::ScsiCommand);
        assert!(!bhs.immediate);
        assert!(bhs.final_flag());

        // Serialize and parse back
        let wire = bhs.serialize();
        let parsed = Bhs::parse(&wire).unwrap();

        assert_eq!(parsed.opcode, Opcode::ScsiCommand);
        assert!(!parsed.immediate);
        assert_eq!(parsed.lun, lun);
        assert_eq!(parsed.itt, itt);
        assert_eq!(parsed.cmd_sn(), cmd_sn);
        assert_eq!(parsed.exp_stat_sn(), exp_stat_sn);
        // CDB is at raw[32..48]
        assert_eq!(&parsed.raw[32..48], &cdb);
        // EDTL at raw[20..24]
        assert_eq!(
            u32::from_be_bytes(parsed.raw[20..24].try_into().unwrap()),
            edtl
        );
    }

    #[test]
    fn test_login_request_builder() {
        let isid = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let tsih = 0x1234u16;
        let cid = 0x0001u16;
        let itt = 1;
        let cmd_sn = 1;
        let exp_stat_sn = 0;
        let csg = 0; // SecurityNegotiation
        let nsg = 1; // LoginOperationalNegotiation
        let transit = true;

        let bhs =
            Bhs::build_login_request(isid, tsih, cid, itt, cmd_sn, exp_stat_sn, csg, nsg, transit);

        let wire = bhs.serialize();

        // byte 0: immediate (0x40) | LoginRequest (0x03) = 0x43
        assert_eq!(wire[0], 0x43);
        assert_eq!(bhs.opcode, Opcode::LoginRequest);
        assert!(bhs.immediate);

        // ISID at bytes 8-13
        assert_eq!(&wire[8..14], &isid);

        // TSIH at bytes 14-15
        assert_eq!(bhs.tsih(), tsih);
        assert_eq!(u16::from_be_bytes(wire[14..16].try_into().unwrap()), tsih);

        // flags: transit=1 -> bit 7, csg=0 << 2, nsg=1
        assert_eq!(wire[1], 0x81); // 1000_0001

        // CID at bytes 20-21
        assert_eq!(u16::from_be_bytes(wire[20..22].try_into().unwrap()), cid);
    }

    #[test]
    fn test_nop_out_builder() {
        let itt = 0xFFFF_FFFFu32;
        let ttt = 0xFFFF_FFFFu32;
        let cmd_sn = 50;
        let exp_stat_sn = 60;

        let bhs = Bhs::build_nop_out(itt, ttt, cmd_sn, exp_stat_sn);

        assert_eq!(bhs.opcode, Opcode::NopOut);
        assert!(bhs.immediate);
        assert_eq!(bhs.itt, itt);
        assert_eq!(bhs.ttt(), ttt);

        let wire = bhs.serialize();
        // byte 0: immediate (0x40) | NopOut (0x00) = 0x40
        assert_eq!(wire[0], 0x40);

        // Round-trip
        let parsed = Bhs::parse(&wire).unwrap();
        assert_eq!(parsed.opcode, Opcode::NopOut);
        assert!(parsed.immediate);
        assert_eq!(parsed.itt, itt);
        assert_eq!(parsed.ttt(), ttt);
        assert_eq!(parsed.cmd_sn(), cmd_sn);
        assert_eq!(parsed.exp_stat_sn(), exp_stat_sn);
    }

    #[test]
    fn test_data_segment_length_encoding() {
        // Build a BHS with a specific data_segment_length and check the 3-byte
        // encoding at wire bytes 5-7.
        let mut bhs = Bhs::build_nop_out(0, 0, 0, 0);
        bhs.data_segment_length = 0x123456;
        // Re-sync raw for the new data_segment_length
        bhs.sync_raw_from_fields();

        let wire = bhs.serialize();
        assert_eq!(wire[4], 0x00); // reserved
        assert_eq!(wire[5], 0x12);
        assert_eq!(wire[6], 0x34);
        assert_eq!(wire[7], 0x56);

        // Parse back
        let parsed = Bhs::parse(&wire).unwrap();
        assert_eq!(parsed.data_segment_length, 0x123456);

        // Edge case: 0
        bhs.data_segment_length = 0;
        bhs.sync_raw_from_fields();
        let wire = bhs.serialize();
        assert_eq!(wire[5], 0);
        assert_eq!(wire[6], 0);
        assert_eq!(wire[7], 0);
        let parsed = Bhs::parse(&wire).unwrap();
        assert_eq!(parsed.data_segment_length, 0);

        // Edge case: max 24-bit
        bhs.data_segment_length = 0xFFFFFF;
        bhs.sync_raw_from_fields();
        let wire = bhs.serialize();
        assert_eq!(wire[5], 0xFF);
        assert_eq!(wire[6], 0xFF);
        assert_eq!(wire[7], 0xFF);
        let parsed = Bhs::parse(&wire).unwrap();
        assert_eq!(parsed.data_segment_length, 0xFFFFFF);
    }

    #[test]
    fn test_parse_data_in_fields() {
        let itt = 7;
        let data_sn = 3;
        let buffer_offset = 4096;
        let data_len = 512;
        let stat_sn = 10;
        let exp_cmd_sn = 20;
        let max_cmd_sn = 30;
        let scsi_status = 0x00; // GOOD

        let bhs = Bhs::build_data_in(
            itt,
            data_sn,
            buffer_offset,
            data_len,
            true, // status_flag
            scsi_status,
            stat_sn,
            exp_cmd_sn,
            max_cmd_sn,
        );

        assert_eq!(bhs.opcode, Opcode::ScsiDataIn);
        assert_eq!(bhs.data_segment_length, data_len);

        let wire = bhs.serialize();
        let parsed = Bhs::parse(&wire).unwrap();

        assert_eq!(parsed.opcode, Opcode::ScsiDataIn);
        assert_eq!(parsed.data_segment_length, data_len);
        assert_eq!(parsed.data_sn(), data_sn);
        assert_eq!(parsed.buffer_offset(), buffer_offset);
        assert!(parsed.status_flag());
        assert!(parsed.final_flag());
        assert_eq!(parsed.scsi_status(), scsi_status);
        assert_eq!(parsed.stat_sn(), stat_sn);
        assert_eq!(parsed.exp_cmd_sn(), exp_cmd_sn);
        assert_eq!(parsed.max_cmd_sn(), max_cmd_sn);
    }

    #[test]
    fn test_parse_r2t_fields() {
        let itt = 5;
        let ttt = 99;
        let stat_sn = 10;
        let exp_cmd_sn = 11;
        let max_cmd_sn = 12;
        let r2t_sn = 0;
        let buffer_offset = 8192;
        let desired_length = 65536;

        let bhs = Bhs::build_r2t(
            itt,
            ttt,
            stat_sn,
            exp_cmd_sn,
            max_cmd_sn,
            r2t_sn,
            buffer_offset,
            desired_length,
        );

        assert_eq!(bhs.opcode, Opcode::R2t);

        let wire = bhs.serialize();
        let parsed = Bhs::parse(&wire).unwrap();

        assert_eq!(parsed.opcode, Opcode::R2t);
        assert_eq!(parsed.itt, itt);
        assert_eq!(parsed.ttt(), ttt);
        assert_eq!(parsed.stat_sn(), stat_sn);
        assert_eq!(parsed.exp_cmd_sn(), exp_cmd_sn);
        assert_eq!(parsed.max_cmd_sn(), max_cmd_sn);
        assert_eq!(parsed.r2t_sn(), r2t_sn);
        assert_eq!(parsed.r2t_buffer_offset(), buffer_offset);
        assert_eq!(parsed.r2t_desired_length(), desired_length);
    }

    #[test]
    fn test_padding_calculation() {
        assert_eq!(pad_to_4(0), 0);
        assert_eq!(pad_to_4(1), 4);
        assert_eq!(pad_to_4(2), 4);
        assert_eq!(pad_to_4(3), 4);
        assert_eq!(pad_to_4(4), 4);
        assert_eq!(pad_to_4(5), 8);
        assert_eq!(pad_to_4(100), 100);
        assert_eq!(pad_to_4(101), 104);
        assert_eq!(pad_to_4(1023), 1024);
        assert_eq!(pad_to_4(1024), 1024);
    }

    #[test]
    fn test_logout_request_builder() {
        let itt = 77;
        let cmd_sn = 500;
        let exp_stat_sn = 501;
        let cid = 1;

        let bhs = Bhs::build_logout_request(itt, cmd_sn, exp_stat_sn, cid);

        assert_eq!(bhs.opcode, Opcode::LogoutRequest);
        assert!(bhs.immediate);
        assert_eq!(bhs.itt, itt);

        let wire = bhs.serialize();
        // byte 0: immediate (0x40) | LogoutRequest (0x06) = 0x46
        assert_eq!(wire[0], 0x46);
        // F bit should be set
        assert!(bhs.final_flag());

        // CID at bytes 20-21
        assert_eq!(u16::from_be_bytes(wire[20..22].try_into().unwrap()), cid);

        // Round-trip
        let parsed = Bhs::parse(&wire).unwrap();
        assert_eq!(parsed.opcode, Opcode::LogoutRequest);
        assert!(parsed.immediate);
        assert_eq!(parsed.itt, itt);
        assert_eq!(parsed.cmd_sn(), cmd_sn);
        assert_eq!(parsed.exp_stat_sn(), exp_stat_sn);
    }

    #[test]
    fn test_data_out_builder() {
        let lun = 0x0001_0000_0000_0000u64;
        let itt = 33;
        let ttt = 44;
        let exp_stat_sn = 55;
        let data_sn = 0;
        let buffer_offset = 0;

        let bhs = Bhs::build_data_out(lun, itt, ttt, exp_stat_sn, data_sn, buffer_offset);

        assert_eq!(bhs.opcode, Opcode::ScsiDataOut);
        assert!(!bhs.immediate);
        assert_eq!(bhs.itt, itt);
        assert_eq!(bhs.lun, lun);

        let wire = bhs.serialize();
        // byte 0: ScsiDataOut (0x05) — no immediate bit
        assert_eq!(wire[0], 0x05);

        let parsed = Bhs::parse(&wire).unwrap();
        assert_eq!(parsed.opcode, Opcode::ScsiDataOut);
        assert_eq!(parsed.itt, itt);
        assert_eq!(parsed.ttt(), ttt);
        assert_eq!(parsed.exp_stat_sn(), exp_stat_sn);
        assert_eq!(parsed.data_sn(), data_sn);
        assert_eq!(parsed.buffer_offset(), buffer_offset);
    }
}
