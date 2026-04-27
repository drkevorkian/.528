pub fn demux_reader_skeleton(input: &[u8]) {
    if input.len() < 4 {
        return;
    }

    let header = &input[..4];
    let _magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);

    for chunk in input[4..].chunks(8) {
        let _ = chunk.len();
    }
}
