use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct HondaRwdFirmwareInfo {
    pub start: u32,
    pub size: u32,
    pub data: Vec<u8>,
    pub decryption_key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct HondaRwdParseResult {
    pub can_address: u32,
    pub compatible_versions: HashMap<Vec<u8>, Vec<u8>>,
    pub firmware: HondaRwdFirmwareInfo,
    pub checksum: u32,
}

pub struct HondaRwdParser;

impl HondaRwdParser {
    pub fn new() -> Self {
        Self
    }

    /// Parses Honda RWD firmware file data, keeping certain fields as bytes.
    ///
    /// # Arguments
    /// * `data` - A bytes slice containing the Honda RWD file content.
    ///
    /// # Returns
    /// A `Result` containing `HondaRwdParseResult` if parsing succeeds, or an error string if it fails.
    ///
    /// # Errors
    /// Returns an error if:
    /// - The file doesn't have the expected indicator bytes (0x5a, 0x0d, 0x0a)
    /// - The file is truncated or malformed
    /// - Header structure is inconsistent
    pub fn parse(&self, data: &[u8]) -> Result<HondaRwdParseResult, String> {
        if data.len() < 3 {
            return Err("File too short".to_string());
        }

        let mut idx = 0;

        // Check indicator bytes
        let indicator_bytes = &data[0..3];
        idx += 3;

        // Only CAN flashing RWD files are supported
        if indicator_bytes != &[0x5a, 0x0d, 0x0a] {
            return Err("Not supported - invalid indicator bytes".to_string());
        }

        // Parse 6 headers
        let mut headers: Vec<Vec<Vec<u8>>> = Vec::new();
        for header_idx in 0..6 {
            if idx >= data.len() {
                return Err(format!("Unexpected end of file while reading header {}", header_idx));
            }

            let count = data[idx] as usize;
            idx += 1;

            let mut header: Vec<Vec<u8>> = Vec::new();
            for _ in 0..count {
                if idx >= data.len() {
                    return Err("Unexpected end of file while reading header entry count".to_string());
                }

                let length = data[idx] as usize;
                idx += 1;

                if idx + length > data.len() {
                    return Err("Unexpected end of file while reading header entry data".to_string());
                }

                let v = data[idx..idx + length].to_vec();
                idx += length;
                header.push(v);
            }
            headers.push(header);
        }

        // Parse start address (big-endian u32)
        if idx + 4 > data.len() {
            return Err("Unexpected end of file while reading start address".to_string());
        }
        let start = u32::from_be_bytes([data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]);
        idx += 4;

        // Parse size (big-endian u32)
        if idx + 4 > data.len() {
            return Err("Unexpected end of file while reading size".to_string());
        }
        let size = u32::from_be_bytes([data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]);
        idx += 4;

        // Parse firmware data
        if data.len() < 4 {
            return Err("File too short to contain checksum".to_string());
        }
        let firmware_end = data.len() - 4;
        if idx > firmware_end {
            return Err("Invalid firmware data section".to_string());
        }
        let firmware = data[idx..firmware_end].to_vec();
        idx = firmware_end;

        // Parse checksum (little-endian u32)
        let checksum = u32::from_le_bytes([data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]);
        idx += 4;

        // Verify we're at the end of the file
        if idx != data.len() {
            return Err("Not at end of file after unpacking".to_string());
        }

        // Validate header structure
        if headers.len() < 5 {
            return Err("Insufficient headers".to_string());
        }
        if headers[3].len() != headers[4].len() {
            return Err("Different number of versions and security access tokens".to_string());
        }

        // The header declares how long the firmware body is; the body itself is
        // whatever sits between the header block and the trailing checksum. If
        // those disagree the file is truncated or padded, and flashing a
        // partial image would leave the ECU unbootable — so refuse it here
        // rather than discovering it halfway through a TransferData loop.
        if firmware.len() != size as usize {
            return Err(format!(
                "Firmware body is {} bytes but the header declares {} bytes - file is truncated or corrupt",
                firmware.len(),
                size
            ));
        }

        // Build versions map
        let mut versions = HashMap::new();
        for i in 0..headers[3].len() {
            // Remove null bytes from version
            let version_bytes: Vec<u8> = headers[3][i]
                .iter()
                .filter(|&&b| b != 0)
                .copied()
                .collect();
            versions.insert(version_bytes, headers[4][i].clone());
        }

        // Calculate CAN address
        let can_address_offset_byte = if !headers[2].is_empty() && !headers[2][0].is_empty() {
            headers[2][0][0]
        } else {
            0
        };
        let can_address = 0x18DA00F1 | ((can_address_offset_byte as u32) << 8);

        // Get decryption key
        let decryption_key = if headers.len() > 5 && !headers[5].is_empty() {
            headers[5][0].clone()
        } else {
            Vec::new()
        };

        Ok(HondaRwdParseResult {
            can_address,
            compatible_versions: versions,
            firmware: HondaRwdFirmwareInfo {
                start,
                size,
                data: firmware,
                decryption_key,
            },
            checksum,
        })
    }
}

impl Default for HondaRwdParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes one header: a count byte followed by length-prefixed entries.
    fn header(entries: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![entries.len() as u8];
        for e in entries {
            out.push(e.len() as u8);
            out.extend_from_slice(e);
        }
        out
    }

    /// Builds a minimal but structurally complete `.rwd` image:
    /// magic, six headers, big-endian start/size, firmware, LE checksum.
    fn sample_rwd() -> Vec<u8> {
        let mut f = vec![0x5A, 0x0D, 0x0A];
        f.extend(header(&[b"header0"]));
        f.extend(header(&[]));
        // Header 2 byte 0 is the CAN address offset.
        f.extend(header(&[&[0x01, 0x02]]));
        // Header 3: calibration versions, NUL padded as they are in real files.
        f.extend(header(&[b"37805-AAA-A00\0\0", b"37805-BBB-B00\0"]));
        // Header 4: security tokens, paired with header 3 by index.
        f.extend(header(&[
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
            &[0xAA, 0xBB, 0xCC, 0xDD],
        ]));
        // Header 5: firmware decryption key.
        f.extend(header(&[&[0xDE, 0xAD, 0xBE]]));

        f.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // start
        f.extend_from_slice(&0x0000_0004u32.to_be_bytes()); // size
        f.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]); // firmware body
        f.extend_from_slice(&0x1234_5678u32.to_le_bytes()); // checksum
        f
    }

    #[test]
    fn parses_every_field_of_a_well_formed_file() {
        let parsed = HondaRwdParser::new().parse(&sample_rwd()).unwrap();

        // Header 2's first byte is OR'd into the diagnostic CAN address.
        assert_eq!(parsed.can_address, 0x18DA01F1);
        assert_eq!(parsed.firmware.start, 0x0001_0000);
        assert_eq!(parsed.firmware.size, 4);
        assert_eq!(parsed.firmware.data, vec![0xCA, 0xFE, 0xBA, 0xBE]);
        assert_eq!(parsed.firmware.decryption_key, vec![0xDE, 0xAD, 0xBE]);
        assert_eq!(parsed.checksum, 0x1234_5678, "checksum is little-endian");
    }

    /// Versions are keyed with their NUL padding stripped, because they are
    /// looked up against the calibration id read live from the ECU.
    #[test]
    fn pairs_calibration_versions_with_their_security_tokens() {
        let parsed = HondaRwdParser::new().parse(&sample_rwd()).unwrap();

        assert_eq!(parsed.compatible_versions.len(), 2);
        assert_eq!(
            parsed.compatible_versions.get(b"37805-AAA-A00".as_slice()),
            Some(&vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66])
        );
        assert_eq!(
            parsed.compatible_versions.get(b"37805-BBB-B00".as_slice()),
            Some(&vec![0xAA, 0xBB, 0xCC, 0xDD])
        );
        assert!(
            parsed.compatible_versions.keys().all(|k| !k.contains(&0)),
            "NUL padding must be stripped from version keys"
        );
    }

    #[test]
    fn rejects_a_file_that_is_not_a_can_rwd() {
        let mut bad = sample_rwd();
        bad[0] = 0x5B; // wrong indicator byte
        assert!(HondaRwdParser::new().parse(&bad).is_err());

        assert!(HondaRwdParser::new().parse(&[0x5A, 0x0D]).is_err());
    }

    /// Truncation must be reported, never silently produce a short firmware
    /// image — flashing one would brick the ECU.
    #[test]
    fn rejects_truncated_files() {
        let full = sample_rwd();
        // The last two cases lose only firmware bytes; they are caught by the
        // declared-size check rather than by running off the end of the buffer.
        for cut in [5, 20, full.len() - 5, full.len() - 1] {
            assert!(
                HondaRwdParser::new().parse(&full[..cut]).is_err(),
                "expected truncation at {cut} bytes to fail"
            );
        }
    }

    /// A body that does not match the declared size means bytes were lost or
    /// added somewhere; the resulting image must never reach an ECU.
    #[test]
    fn rejects_a_firmware_body_that_contradicts_the_declared_size() {
        let mut f = sample_rwd();
        // Splice in one extra firmware byte without touching the size field.
        let checksum_at = f.len() - 4;
        f.insert(checksum_at, 0xFF);

        let err = HondaRwdParser::new().parse(&f).unwrap_err();
        assert!(err.contains("header declares"), "got: {err}");
    }

    /// Versions and tokens are paired by index, so a length mismatch means the
    /// table is corrupt and the pairing cannot be trusted.
    #[test]
    fn rejects_a_version_token_count_mismatch() {
        let mut f = vec![0x5A, 0x0D, 0x0A];
        f.extend(header(&[]));
        f.extend(header(&[]));
        f.extend(header(&[&[0x01]]));
        f.extend(header(&[b"37805-AAA-A00", b"37805-BBB-B00"])); // 2 versions
        f.extend(header(&[&[0x11, 0x22, 0x33, 0x44]])); // 1 token
        f.extend(header(&[&[0xDE, 0xAD, 0xBE]]));
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());

        let err = HondaRwdParser::new().parse(&f).unwrap_err();
        assert!(err.contains("Different number of versions"), "got: {err}");
    }
}
