//! The part of iris's `disp` the display transport names: the screen a head
//! composes into, and the PNG writer.

pub const FB_STRIDE: usize = 2048;
pub const FB_MAX_H: usize = 1024;

/// The frame a head composes into: the words of the picture and its size.
pub struct Rex3Screen {
    pub width: usize,
    pub height: usize,
    pub fb_rgb: Vec<u32>,
    pub rgba: Vec<u32>,
    pub prebuilt: bool,
    pub frame_seq: u64,
}

impl Rex3Screen {
    pub fn new() -> Self {
        Rex3Screen { width: 0, height: 0, fb_rgb: vec![0; FB_STRIDE * FB_MAX_H], rgba: vec![0; FB_STRIDE * FB_MAX_H], prebuilt: false, frame_seq: 0 }
    }
}

/// Not available in the engine crate; callers get the error.
pub fn write_png_words(_path: impl AsRef<std::path::Path>, _px: &[u32], _stride: usize, _w: usize, _h: usize) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no PNG writer in the engine crate"))
}
