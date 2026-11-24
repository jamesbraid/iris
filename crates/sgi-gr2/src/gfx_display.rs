//! The two display types the board's struct holds. iris's own are a window and its frame pacing; the
//! engine's picture goes out through `BoardEngine::scanout`.

#[derive(Default)]
pub struct Screen;

#[derive(Default)]
pub struct DisplayStop;

impl DisplayStop {
    pub fn request(&self) {}
}
