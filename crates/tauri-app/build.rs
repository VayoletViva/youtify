fn main() {
    // generate_context! and the Windows resource compiler require real raster
    // icons even when bundle.icon is empty. Generate deterministic assets using
    // only std, before tauri-build runs; no external image tool is required.
    generate_icons().expect("failed to generate application icons");
    tauri_build::build();
}

fn generate_icons() -> std::io::Result<()> {
    let directory = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("icons");
    std::fs::create_dir_all(&directory)?;
    let size = 128u32;
    let mut pixels = Vec::new();
    for y in 0..size {
        pixels.push(0); // PNG filter: None
        for x in 0..size {
            // Sage Y monogram with a lime play triangle on deep green.
            let branch = (32..64).contains(&y)
                && ((x as f64 - 54.0).abs() - (64 - y) as f64 * 0.7).abs() < 6.0;
            let stem = (48..60).contains(&x) && (60..96).contains(&y);
            let play = (78..106).contains(&x) && (y as i32 - 64).abs() * 2 <= (106 - x) as i32;
            pixels.extend_from_slice(if play { &[211, 240, 128, 255] }
                else if branch || stem { &[174, 194, 164, 255] }
                else { &[24, 30, 25, 255] });
        }
    }
    // A zlib stream with uncompressed DEFLATE blocks and Adler-32 checksum.
    let mut zlib = vec![0x78, 0x01];
    let count = pixels.len().div_ceil(65_535);
    for (index, block) in pixels.chunks(65_535).enumerate() {
        zlib.push(if index + 1 == count { 1 } else { 0 });
        let length = block.len() as u16;
        zlib.extend_from_slice(&length.to_le_bytes());
        zlib.extend_from_slice(&(!length).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &pixels { a = (a + u32::from(*byte)) % 65_521; b = (b + a) % 65_521; }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    write_changed(&directory.join("icon.png"), &png)?;

    // Vista+ ICO: one 128×128 PNG entry.
    let mut ico = vec![0, 0, 1, 0, 1, 0, 128, 128, 0, 0, 1, 0, 32, 0];
    ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    ico.extend_from_slice(&png);
    write_changed(&directory.join("icon.ico"), &ico)?;

    // macOS ic07: 128×128 PNG representation.
    let mut icns = b"icns".to_vec();
    icns.extend_from_slice(&((png.len() + 16) as u32).to_be_bytes());
    icns.extend_from_slice(b"ic07");
    icns.extend_from_slice(&((png.len() + 8) as u32).to_be_bytes());
    icns.extend_from_slice(&png);
    write_changed(&directory.join("icon.icns"), &icns)
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let mut crc = !0u32;
    for byte in kind.iter().chain(data) {
        crc ^= u32::from(*byte);
        for _ in 0..8 { crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1))); }
    }
    png.extend_from_slice(&(!crc).to_be_bytes());
}

fn write_changed(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if std::fs::read(path).ok().as_deref() != Some(bytes) { std::fs::write(path, bytes)?; }
    Ok(())
}
