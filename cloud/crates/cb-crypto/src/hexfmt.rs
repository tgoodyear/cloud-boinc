//! The hex block format used for BOINC keys and signatures.
//!
//! Upstream: `sprint_hex_data` and `sscan_hex_data` in `lib/crypt.cpp`.

/// Bytes rendered per line by upstream `sprint_hex_data`.
const BYTES_PER_LINE: usize = 32;

/// Render bytes as upstream `sprint_hex_data` does: lowercase hex, a newline
/// after every 32 bytes (and after a final partial line), then `".\n"`.
///
/// The trailing `.` exists because a reader may not know the data length; it
/// is the non-hex character that stops `sscan_hex_data`.
///
/// Upstream reports an error for empty input; this function returns `".\n"`,
/// which [`scan_hex_data`] reads back as empty.
pub fn print_hex_data(data: &[u8]) -> String {
    let lines = data.len().div_ceil(BYTES_PER_LINE);
    let mut out = String::with_capacity(data.len() * 2 + lines + 2);
    for chunk in data.chunks(BYTES_PER_LINE) {
        out.push_str(&hex::encode(chunk));
        out.push('\n');
    }
    out.push_str(".\n");
    out
}

/// Parse a hex block as upstream `sscan_hex_data` does.
///
/// Newlines (`\n` only) are skipped before the first digit of a byte. Parsing
/// stops at the first other non-hex character, including a space, a tab or a
/// carriage return, and returns the bytes read so far. Upper and lower case
/// digits are accepted. A trailing unpaired digit is dropped.
///
/// Note that this means leading whitespace other than `\n` yields no data,
/// exactly as in the stock client and upload handler.
pub fn scan_hex_data(text: impl AsRef<[u8]>) -> Vec<u8> {
    let buf = text.as_ref();
    let mut data = Vec::with_capacity(buf.len() / 2);
    let mut i = 0;
    while i < buf.len() {
        let c1 = buf[i];
        i += 1;
        if c1 == b'\n' {
            continue;
        }
        let Some(hi) = hex_value(c1) else { break };
        let Some(&c2) = buf.get(i) else { break };
        i += 1;
        let Some(lo) = hex_value(c2) else { break };
        data.push(hi << 4 | lo);
    }
    data
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_like_upstream() {
        assert_eq!(print_hex_data(&[0xab]), "ab\n.\n");
        assert_eq!(print_hex_data(&[]), ".\n");
        let full = print_hex_data(&[0x01; 32]);
        assert_eq!(full, format!("{}\n.\n", "01".repeat(32)));
        let more = print_hex_data(&[0xff; 33]);
        assert_eq!(more, format!("{}\nff\n.\n", "ff".repeat(32)));
    }

    #[test]
    fn scans_like_upstream() {
        assert_eq!(scan_hex_data("0aFf\n10\n.\n"), vec![0x0a, 0xff, 0x10]);
        assert_eq!(scan_hex_data("\n\n0a"), vec![0x0a]);
        // stops at any non-hex character other than a newline
        assert_eq!(scan_hex_data(" 0a"), Vec::<u8>::new());
        assert_eq!(scan_hex_data("0a\r\n0b"), vec![0x0a]);
        // a newline between the two digits of a byte stops the scan
        assert_eq!(scan_hex_data("0a0\nb"), vec![0x0a]);
        // unpaired trailing digit is dropped
        assert_eq!(scan_hex_data("0a0"), vec![0x0a]);
        assert_eq!(scan_hex_data(""), Vec::<u8>::new());
    }

    #[test]
    fn round_trip() {
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(scan_hex_data(print_hex_data(&data)), data);
    }
}
