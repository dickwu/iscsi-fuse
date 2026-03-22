use thiserror::Error;

#[derive(Debug, Error)]
pub enum DigestError {
    #[error("Header CRC32C mismatch: expected {expected:#010x}, got {received:#010x}")]
    HeaderMismatch { expected: u32, received: u32 },
    #[error("Data CRC32C mismatch: expected {expected:#010x}, got {received:#010x}")]
    DataMismatch { expected: u32, received: u32 },
}

/// Compute CRC32C of BHS (48 bytes) + optional AHS. Returns 4-byte big-endian digest.
pub fn header_digest(bhs: &[u8; 48], ahs: Option<&[u8]>) -> [u8; 4] {
    let mut crc = crc32c::crc32c(bhs);
    if let Some(ahs) = ahs {
        crc = crc32c::crc32c_append(crc, ahs);
    }
    crc.to_be_bytes()
}

/// Verify a received header digest.
pub fn verify_header_digest(
    bhs: &[u8; 48],
    ahs: Option<&[u8]>,
    received: &[u8; 4],
) -> Result<(), DigestError> {
    let expected = header_digest(bhs, ahs);
    if expected != *received {
        Err(DigestError::HeaderMismatch {
            expected: u32::from_be_bytes(expected),
            received: u32::from_be_bytes(*received),
        })
    } else {
        Ok(())
    }
}

/// Compute CRC32C of a data segment. Returns 4-byte big-endian digest.
pub fn data_digest(data: &[u8]) -> [u8; 4] {
    crc32c::crc32c(data).to_be_bytes()
}

/// Verify a received data digest.
pub fn verify_data_digest(data: &[u8], received: &[u8; 4]) -> Result<(), DigestError> {
    let expected = data_digest(data);
    if expected != *received {
        Err(DigestError::DataMismatch {
            expected: u32::from_be_bytes(expected),
            received: u32::from_be_bytes(*received),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_digest_deterministic() {
        let bhs = [0xABu8; 48];
        let d1 = header_digest(&bhs, None);
        let d2 = header_digest(&bhs, None);
        assert_eq!(d1, d2);
    }

    #[test]
    fn test_header_digest_with_ahs() {
        let bhs = [0u8; 48];
        let d_no_ahs = header_digest(&bhs, None);
        let d_with_ahs = header_digest(&bhs, Some(&[1, 2, 3, 4]));
        assert_ne!(d_no_ahs, d_with_ahs);
    }

    #[test]
    fn test_verify_header_digest_ok() {
        let bhs = [0x42u8; 48];
        let digest = header_digest(&bhs, None);
        assert!(verify_header_digest(&bhs, None, &digest).is_ok());
    }

    #[test]
    fn test_verify_header_digest_mismatch() {
        let bhs = [0x42u8; 48];
        let bad_digest = [0u8; 4];
        assert!(verify_header_digest(&bhs, None, &bad_digest).is_err());
    }

    #[test]
    fn test_data_digest_known_value() {
        let d = data_digest(&[]);
        assert_eq!(u32::from_be_bytes(d), 0x00000000);
    }

    #[test]
    fn test_verify_data_digest_ok() {
        let data = b"Hello iSCSI";
        let digest = data_digest(data);
        assert!(verify_data_digest(data, &digest).is_ok());
    }

    #[test]
    fn test_verify_data_digest_mismatch() {
        let data = b"Hello iSCSI";
        let bad = [0xFF; 4];
        assert!(verify_data_digest(data, &bad).is_err());
    }
}
