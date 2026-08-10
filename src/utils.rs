pub fn hex_to_bytes(hex_str: &str) -> Result<Vec<u8>, &'static str> {
    // A hex string must have an even number of characters.
    if hex_str.len() % 2 != 0 {
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

pub fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut hex_string = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex_string.push_str(&format!("{:02X}", byte));
    }
    hex_string
}

pub fn hexdump(data: &[u8]) {
    const WIDTH: usize = 16; // How many bytes to print per line

    // Iterate over the data in chunks of `WIDTH` bytes.
    // The `enumerate()` gives us the line number (i) as well as the chunk.
    for (i, chunk) in data.chunks(WIDTH).enumerate() {
        // 1. Print the offset (e.g., "00000000: ")
        // The offset is the line number (i) times the width (16).
        // {:08x} formats the number as an 8-digit, zero-padded hex value.
        print!("{:08x}: ", i * WIDTH);

        // 2. Print the hexadecimal representation of the bytes.
        let mut hex_part = String::new();
        for (j, byte) in chunk.iter().enumerate() {
            // Add an extra space in the middle for readability (after 8 bytes).
            if j == WIDTH / 2 {
                hex_part.push(' ');
            }
            hex_part.push_str(&format!(" {:02x}", byte));
        }
        // Pad with spaces if the last line is shorter than `WIDTH` bytes.
        // Each byte takes 3 chars (" 00"), so we calculate the padding needed.
        print!("{:<width$}", hex_part, width = WIDTH * 3 + 1);

        // 3. Print the ASCII representation of the bytes.
        print!(" |");
        for &byte in chunk {
            // If the byte is a printable ASCII character, print it.
            // Otherwise, print a dot ('.').
            let char = if (0x20..=0x7e).contains(&byte) {
                byte as char
            } else {
                '.'
            };
            print!("{}", char);
        }
        println!("|");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(hex_to_bytes("18daf101").unwrap(), vec![0x18, 0xDA, 0xF1, 0x01]);
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
