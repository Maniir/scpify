/// Returns a compact, single-line hex string of `data`, truncated to `max_len` bytes.
///
/// If the data is truncated, a suffix indicating the total byte count is appended,
/// e.g. `"DE AD BE EF ... (256 bytes total)"`.
///
/// # Examples
///
/// ```
/// # use scpify::helpers::hex_dump;
/// // Short data — no truncation
/// assert_eq!(hex_dump(&[0xDE, 0xAD, 0xBE, 0xEF], 16), "DE AD BE EF");
///
/// // Empty slice
/// assert_eq!(hex_dump(&[], 16), "");
///
/// // Data longer than max_len is truncated with a total-byte annotation
/// let data: Vec<u8> = (0u8..=255).collect();
/// let result = hex_dump(&data, 4);
/// assert!(result.starts_with("00 01 02 03"));
/// assert!(result.ends_with("(256 bytes total)"));
/// ```
pub fn hex_dump(data: &[u8], max_len: usize) -> String {
    let len = data.len().min(max_len);

    let hex = data[..len]
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ");

    if data.len() > max_len {
        format!("{} ... ({} bytes total)", hex, data.len())
    } else {
        hex
    }
}

/// Returns a formatted, multi-line hex dump of `data`, truncated to `max_len` bytes.
///
/// Each line displays 16 bytes in hex on the left and their ASCII representation
/// on the right, with non-printable characters replaced by `.`. The hex column is
/// padded to 48 characters so the ASCII column is consistently aligned.
///
/// # Example output
/// ```text
/// 48 65 6C 6C 6F 2C 20 77 6F 72 6C 64 21 0A      Hello, world!.
/// ```
///
/// # Examples
///
/// ```
/// # use scpify::helpers::hex_dump_pretty;
/// // Short data fits on a single line
/// let result = hex_dump_pretty(b"Hello", 64);
/// assert_eq!(result, "48 65 6C 6C 6F                                    Hello");
///
/// // Non-printable bytes are shown as '.'
/// let result = hex_dump_pretty(&[0x00, 0x41, 0x0A], 64);
/// assert_eq!(result, "00 41 0A                                          .A.");
///
/// // Data spanning two 16-byte rows produces two lines
/// let data: Vec<u8> = (0x41u8..0x51).collect(); // 'A'..'Q' (16 bytes)
/// let long = data.repeat(2);                     // 32 bytes
/// let result = hex_dump_pretty(&long, 64);
/// assert_eq!(result.lines().count(), 2);
///
/// // Empty slice produces an empty string
/// assert_eq!(hex_dump_pretty(&[], 64), "");
/// ```
pub fn hex_dump_pretty(data: &[u8], max_len: usize) -> String {
    let len = data.len().min(max_len);

    data[..len]
        .chunks(16)
        .map(|chunk| {
            let hex = chunk
                .iter()
                .map(|b| format!("{:02X}", b))
                .collect::<Vec<_>>()
                .join(" ");

            // Replace non-printable bytes with '.' for the ASCII column
            let ascii = chunk
                .iter()
                .map(|b| {
                    if b.is_ascii_graphic() {
                        *b as char
                    } else {
                        '.'
                    }
                })
                .collect::<String>();

            format!("{:<48}  {}", hex, ascii)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
