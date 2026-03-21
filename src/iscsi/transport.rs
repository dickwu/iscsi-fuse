#![allow(dead_code)]

use std::time::Duration;

use anyhow::Result;
#[cfg(test)]
use bytes::Bytes;
use bytes::BytesMut;
use socket2::SockRef;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tracing::debug;

use super::digest::{data_digest, header_digest, verify_data_digest, verify_header_digest};
use super::pdu::{Bhs, Pdu, pad_to_4};

// ---------------------------------------------------------------------------
// DigestConfig
// ---------------------------------------------------------------------------

/// Controls whether CRC32C digests are computed and verified on the wire.
#[derive(Debug, Clone)]
pub struct DigestConfig {
    pub header: bool,
    pub data: bool,
}

// ---------------------------------------------------------------------------
// Buffer sizes
// ---------------------------------------------------------------------------

/// 4 MB socket buffer for 10G tuning.
const SOCK_BUF_SIZE: usize = 4 * 1024 * 1024;

/// 1 MB userspace buffer for BufReader / BufWriter.
const IO_BUF_SIZE: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Transport — static connection helpers
// ---------------------------------------------------------------------------

pub struct Transport;

impl Transport {
    /// Open a TCP connection to `addr` (e.g. "10.0.0.1:3260"), apply 10G
    /// socket tuning, and return a split writer/reader pair with digests
    /// disabled.
    pub async fn connect(addr: &str) -> Result<(TransportWriter, TransportReader)> {
        let stream = TcpStream::connect(addr).await?;

        // --- 10G socket tuning via socket2 ---
        let sock_ref = SockRef::from(&stream);
        sock_ref.set_send_buffer_size(SOCK_BUF_SIZE)?;
        sock_ref.set_recv_buffer_size(SOCK_BUF_SIZE)?;
        stream.set_nodelay(true)?;

        debug!(
            addr,
            send_buf = sock_ref.send_buffer_size()?,
            recv_buf = sock_ref.recv_buffer_size()?,
            "TCP connected with 10G tuning"
        );

        // Split and wrap in buffered I/O.
        let (read_half, write_half) = stream.into_split();
        let digest = DigestConfig {
            header: false,
            data: false,
        };

        let writer = TransportWriter::new(write_half, digest.clone());
        let reader = TransportReader::new(read_half, digest);

        Ok((writer, reader))
    }

    /// Enable TCP keepalive on a `TcpStream` (call before splitting).
    pub fn set_tcp_keepalive(
        stream: &TcpStream,
        idle: Duration,
        interval: Duration,
        count: u32,
    ) -> Result<()> {
        let sock_ref = SockRef::from(stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(idle)
            .with_interval(interval)
            .with_retries(count);
        sock_ref.set_tcp_keepalive(&keepalive)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TransportWriter
// ---------------------------------------------------------------------------

pub struct TransportWriter {
    writer: BufWriter<OwnedWriteHalf>,
    digests: DigestConfig,
}

impl TransportWriter {
    pub fn new(write_half: OwnedWriteHalf, digests: DigestConfig) -> Self {
        Self {
            writer: BufWriter::with_capacity(IO_BUF_SIZE, write_half),
            digests,
        }
    }

    /// Enable header and/or data digests for subsequent writes.
    pub fn enable_digests(&mut self, header: bool, data: bool) {
        self.digests.header = header;
        self.digests.data = data;
    }

    /// Serialize and send a complete iSCSI PDU on the wire.
    pub async fn send_pdu(&mut self, pdu: &Pdu) -> Result<()> {
        // 1. Serialize BHS (48 bytes).
        let bhs_bytes = pdu.bhs.serialize();
        self.writer.write_all(&bhs_bytes).await?;

        // 2. Optional header digest (CRC32C, 4 bytes big-endian).
        if self.digests.header {
            let ahs = pdu.ahs.as_deref();
            let hd = header_digest(&bhs_bytes, ahs);
            self.writer.write_all(&hd).await?;
        }

        // 3. Data segment + padding + optional data digest.
        if let Some(ref data) = pdu.data {
            self.writer.write_all(data).await?;

            // Pad to 4-byte alignment.
            let pad_len = pad_to_4(data.len()) - data.len();
            if pad_len > 0 {
                let padding = [0u8; 3];
                self.writer.write_all(&padding[..pad_len]).await?;
            }

            if self.digests.data {
                let dd = data_digest(data);
                self.writer.write_all(&dd).await?;
            }
        }

        // 4. Flush to ensure the full PDU is on the wire.
        self.writer.flush().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TransportReader
// ---------------------------------------------------------------------------

pub struct TransportReader {
    reader: BufReader<OwnedReadHalf>,
    digests: DigestConfig,
    bhs_buf: [u8; 48],
}

impl TransportReader {
    pub fn new(read_half: OwnedReadHalf, digests: DigestConfig) -> Self {
        Self {
            reader: BufReader::with_capacity(IO_BUF_SIZE, read_half),
            digests,
            bhs_buf: [0u8; 48],
        }
    }

    /// Enable header and/or data digests for subsequent reads.
    pub fn enable_digests(&mut self, header: bool, data: bool) {
        self.digests.header = header;
        self.digests.data = data;
    }

    /// Read and reassemble a complete iSCSI PDU from the wire.
    pub async fn recv_pdu(&mut self) -> Result<Pdu> {
        // 1. Read BHS (48 bytes).
        self.reader.read_exact(&mut self.bhs_buf).await?;

        // 2. Optional header digest verification.
        if self.digests.header {
            let mut hd = [0u8; 4];
            self.reader.read_exact(&mut hd).await?;
            verify_header_digest(&self.bhs_buf, None, &hd)?;
        }

        // 3. Parse BHS.
        let bhs = Bhs::parse(&self.bhs_buf)?;

        // 4. Read optional data segment.
        let data = if bhs.data_segment_length > 0 {
            let raw_len = bhs.data_segment_length as usize;
            let padded_len = pad_to_4(raw_len);

            let mut buf = BytesMut::zeroed(padded_len);
            self.reader.read_exact(&mut buf).await?;

            // Optional data digest verification (on unpadded data).
            if self.digests.data {
                let mut dd = [0u8; 4];
                self.reader.read_exact(&mut dd).await?;
                verify_data_digest(&buf[..raw_len], &dd)?;
            }

            // Truncate to actual data length and freeze.
            buf.truncate(raw_len);
            Some(buf.freeze())
        } else {
            None
        };

        Ok(Pdu {
            bhs,
            ahs: None,
            data,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::pdu::{Bhs, Opcode, Pdu};
    use super::*;

    /// Create a loopback writer/reader pair with digests disabled.
    async fn loopback_pair() -> (TransportWriter, TransportReader) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let digest = DigestConfig {
            header: false,
            data: false,
        };
        let (_cr, cw) = client.into_split();
        let (sr, _sw) = server.into_split();
        let writer = TransportWriter::new(cw, digest.clone());
        let reader = TransportReader::new(sr, digest);
        (writer, reader)
    }

    /// Create a loopback writer/reader pair with both digests enabled.
    async fn loopback_pair_with_digests() -> (TransportWriter, TransportReader) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let digest = DigestConfig {
            header: true,
            data: true,
        };
        let (_cr, cw) = client.into_split();
        let (sr, _sw) = server.into_split();
        let writer = TransportWriter::new(cw, digest.clone());
        let reader = TransportReader::new(sr, digest);
        (writer, reader)
    }

    #[tokio::test]
    async fn test_send_recv_pdu_no_data_no_digest() {
        let (mut writer, mut reader) = loopback_pair().await;

        // Build a NOP-Out with a recognizable ITT.
        let bhs = Bhs::build_nop_out(0xDEAD_BEEF, 0xFFFF_FFFF, 1, 0);
        let pdu = Pdu {
            bhs,
            ahs: None,
            data: None,
        };

        writer.send_pdu(&pdu).await.unwrap();
        let received = reader.recv_pdu().await.unwrap();

        assert_eq!(received.bhs.opcode, Opcode::NopOut);
        assert_eq!(received.bhs.itt, 0xDEAD_BEEF);
        assert!(received.data.is_none());
    }

    #[tokio::test]
    async fn test_send_recv_pdu_with_data() {
        let (mut writer, mut reader) = loopback_pair().await;

        // 5 bytes of data — requires 3 bytes of padding to reach 8.
        let data = Bytes::from_static(b"hello");
        let mut bhs = Bhs::build_nop_out(42, 0xFFFF_FFFF, 1, 0);
        bhs.data_segment_length = data.len() as u32;
        // Re-serialize so the wire bytes reflect the updated length.
        let wire = bhs.serialize();
        let bhs = Bhs::parse(&wire).unwrap();

        let pdu = Pdu {
            bhs,
            ahs: None,
            data: Some(data.clone()),
        };

        writer.send_pdu(&pdu).await.unwrap();
        let received = reader.recv_pdu().await.unwrap();

        assert_eq!(received.bhs.opcode, Opcode::NopOut);
        assert_eq!(received.bhs.itt, 42);
        assert_eq!(received.bhs.data_segment_length, 5);
        assert_eq!(received.data.as_deref(), Some(b"hello".as_slice()));
    }

    #[tokio::test]
    async fn test_send_recv_with_digests() {
        let (mut writer, mut reader) = loopback_pair_with_digests().await;

        // PDU with 5-byte data payload + both digests.
        let data = Bytes::from_static(b"world");
        let mut bhs = Bhs::build_nop_out(99, 0xFFFF_FFFF, 2, 1);
        bhs.data_segment_length = data.len() as u32;
        let wire = bhs.serialize();
        let bhs = Bhs::parse(&wire).unwrap();

        let pdu = Pdu {
            bhs,
            ahs: None,
            data: Some(data.clone()),
        };

        writer.send_pdu(&pdu).await.unwrap();
        let received = reader.recv_pdu().await.unwrap();

        assert_eq!(received.bhs.opcode, Opcode::NopOut);
        assert_eq!(received.bhs.itt, 99);
        assert_eq!(received.bhs.data_segment_length, 5);
        assert_eq!(received.data.as_deref(), Some(b"world".as_slice()));
    }
}
