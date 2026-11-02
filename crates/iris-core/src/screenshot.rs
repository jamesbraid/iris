//! Writing frames to PNG files: screenshots and golden-image captures. Kept
//! out of disp.rs so a board that only saves frames does not depend on the
//! Newport display path.

/// Save `px` (R, G, B, A bytes, `w` x `h`, tightly packed) as a PNG, written
/// to a temporary name and renamed, so a reader never sees half a file.
pub fn save_png_rgba8(path: &str, px: &[u8], w: u32, h: u32) -> Result<(), String> {
    let tmp = format!("{path}.tmp");
    {
        let f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(px).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Save the composited rgba buffer as a PNG file.
/// `rgba` uses 0xFFBBGGRR (GL-native); swap R↔B when writing PNG.
pub fn save_screenshot(path: &str, rgba: &[u32], width: usize, height: usize) -> Result<(), String> {
    use std::io::BufWriter;
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(BufWriter::new(file), width as u32, height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    let mut rows: Vec<u8> = Vec::with_capacity(height * width * 3);
    for y in 0..height {
        for x in 0..width {
            let px = rgba[y * 2048 + x];
            rows.push(( px        & 0xFF) as u8);
            rows.push(((px >>  8) & 0xFF) as u8);
            rows.push(((px >> 16) & 0xFF) as u8);
        }
    }
    writer.write_image_data(&rows).map_err(|e| e.to_string())
}
