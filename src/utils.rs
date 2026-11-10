/// Byte that makes an eight-bit wrapping sum equal zero.
pub(crate) fn checksum8(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        .wrapping_neg()
}

/// Removes the NUL and space padding ECUs put around fixed-width text fields.
pub fn trim_padding(text: &str) -> &str {
    text.trim_matches(['\0', ' '])
}

/// Formats whole seconds with explicit units for duration estimates.
pub(crate) fn format_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{} h {} min {} s",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{} min {} s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds} s")
    }
}

pub fn hex_to_bytes(hex_str: &str) -> Result<Vec<u8>, &'static str> {
    if !hex_str.is_ascii() {
        return Err("Invalid hex character found.");
    }
    // A hex string must have an even number of characters.
    if !hex_str.len().is_multiple_of(2) {
        return Err("Hex string must have an even number of characters.");
    }

    // Iterate over the string in chunks of two characters.
    (0..hex_str.len())
        .step_by(2)
        .map(|i| {
            // `from_str_radix` parses a string slice in a given base (16 for hex).
            u8::from_str_radix(&hex_str[i..i + 2], 16)
        })
        .collect::<Result<Vec<u8>, _>>() // Collect the results into a `Vec<u8>`.
        .map_err(|_| "Invalid hex character found.") // Map the parsing error to a simple string.
}

/// All numeric input uses the same hexadecimal/decimal convention.
pub fn parse_hex_u32(s: &str) -> Result<u32, String> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u32>().map_err(|e| e.to_string())
    }
}

pub fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut hex_string = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex_string.push_str(&format!("{byte:02X}"));
    }
    hex_string
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_padding_removes_nul_and_space_from_both_ends_only() {
        assert_eq!(trim_padding("37805-RAD-3350"), "37805-RAD-3350");
        assert_eq!(trim_padding("37805-RAD-3350  "), "37805-RAD-3350");
        assert_eq!(trim_padding("37805-RAD-3350\0\0"), "37805-RAD-3350");
        assert_eq!(trim_padding("\0 37805 RAD \0 "), "37805 RAD");
        assert_eq!(trim_padding(" \0 "), "");
    }

    #[test]
    fn hex_round_trips_through_bytes() {
        let bytes = [0x00u8, 0x0F, 0x18, 0xDA, 0xF1, 0xFF];
        let hex = bytes_to_hex(&bytes);
        assert_eq!(hex, "000F18DAF1FF", "must be uppercase and zero padded");
        assert_eq!(hex_to_bytes(&hex).unwrap(), bytes);
    }

    #[test]
    fn empty_input_round_trips() {
        assert_eq!(bytes_to_hex(&[]), "");
        assert_eq!(hex_to_bytes("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_to_bytes_accepts_lowercase() {
        assert_eq!(
            hex_to_bytes("18daf101").unwrap(),
            vec![0x18, 0xDA, 0xF1, 0x01]
        );
    }

    /// `parse_frame_response` leans on these rejections to drop adapter chatter
    /// (`SEARCHING...`, `OK`, `NO DATA`) out of a response buffer.
    #[test]
    fn hex_to_bytes_rejects_odd_length_and_non_hex() {
        assert!(hex_to_bytes("ABC").is_err());
        assert!(hex_to_bytes("OK").is_err());
        assert!(hex_to_bytes("NODATA?").is_err());
    }
}
