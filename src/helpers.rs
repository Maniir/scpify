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
