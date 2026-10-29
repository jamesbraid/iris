//! Odyssey scanout: board planes composed into the image Iris displays.
//!
//! Display conversion reads the stored planes; it does not change the data
//! returned by board readback.
//!
//! Ported from hw/misc/sgi-odyssey.c and hw/display/sgi/scanout.c
//! in jamesbraid/qemu-sgi-origami.

use super::render::{Render, HEIGHT, WIDTH};
use crate::cpu::mips_core::CyclesPtr;
use crate::dev::ng1::rex3::Renderer;
use crate::disp::{Rex3Screen, FB_STRIDE};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Display {
    pub renderer: Mutex<Option<Box<dyn Renderer>>>,
    pub screen: Mutex<Rex3Screen>,
    pub cycles: Mutex<CyclesPtr>,
    pub screenshot: AtomicBool,
    local: Mutex<Option<Local>>,
}
struct Local {
    overlay: crate::debug_overlay::DebugOverlay,
    status: crate::disp::StatusBar,
    texture: crate::disp::StatusBarTexture,
    size: (usize, usize),
}
impl Display {
    pub fn new() -> Self {
        Self {
            renderer: Mutex::new(None),
            screen: Mutex::new(Rex3Screen::new()),
            cycles: Mutex::new(CyclesPtr::dangling()),
            screenshot: AtomicBool::new(false),
            local: Mutex::new(None),
        }
    }
    /// Runs on Odyssey's worker, including every host renderer call.
    pub fn frame(&self, r: &Render, backend: &[u32], displayed: u32, pending: usize) {
        let width = if backend[0x2802] == 0 {
            WIDTH
        } else {
            (backend[0x2802] as usize).min(WIDTH)
        };
        let height = if backend[0x2803] == 0 {
            HEIGHT
        } else {
            (backend[0x2803] as usize).min(HEIGHT)
        };
        let mut screen = self.screen.lock();
        screen.width = width;
        screen.height = height;
        screen.prebuilt = true;
        screen.status_bar_only = false;
        for y in 0..height {
            for x in 0..width {
                let rgb = resolve(r, backend, displayed, x, y);
                let rgba = 0xff000000 | ((rgb >> 16) & 255) | (rgb & 0xff00) | ((rgb & 255) << 16);
                screen.fb_rgb[y * FB_STRIDE + x] = rgba;
                screen.rgba[y * FB_STRIDE + x] = rgba;
            }
        }
        let mut renderer = self.renderer.lock();
        if let Some(renderer) = renderer.as_mut() {
            let mut local = self.local.lock();
            let local = local.get_or_insert_with(|| Local {
                overlay: crate::debug_overlay::DebugOverlay::new(),
                status: crate::disp::StatusBar::new(),
                texture: crate::disp::StatusBarTexture::new(),
                size: (0, 0),
            });
            if local.size != (width, height) {
                renderer.resize(width, height);
                local.size = (width, height);
            }
            let stats = crate::disp::BarStats {
                now: std::time::Instant::now(),
                cycles: self.cycles.lock().get(),
                fasttick: 0,
                decoded_delta: 0,
                l1i_hits: 0,
                l1i_fetches: 0,
                uncached: 0,
                hb: 0,
                count_hz: 0,
                gfifo_pending: pending,
            };
            renderer.present(
                &mut screen,
                &mut local.overlay,
                &mut local.status,
                &mut local.texture,
                &stats,
                false,
                None,
                None,
            );
        }
        self.screenshot.store(false, Ordering::Release);
    }
    pub fn stop(&self) {
        if let Some(renderer) = self.renderer.lock().as_mut() {
            renderer.stop();
        }
        self.local.lock().take();
    }
}
fn rgb(word: u32) -> u32 {
    ((word >> 22) & 255) << 16 | ((word >> 12) & 255) << 8 | ((word >> 2) & 255)
}
fn gamma(lut: &[u32], c: u32, shift: u32) -> u32 {
    let v = (c << 4) | (c >> 4);
    let t = if v < 0x200 {
        v
    } else if v < 0x400 {
        v / 2 + 0x100
    } else {
        v / 4 + 0x200
    };
    lut[0x1a00 + t as usize] >> shift & 255
}
fn resolve(r: &Render, b: &[u32], displayed: u32, x: usize, y: usize) -> u32 {
    let at = y * WIDTH + x;
    let did = r.did[at];
    let id = (did >> 3) as usize;
    let word = r.colour[((displayed >> id) & 1) as usize][at];
    let group = b[0x2900 + id] >> 4 & 31;
    let mode = b[0x2920 + (did & 7) as usize];
    let overlay = r.overlay[at] as u32;
    let mut colour = if mode & 1 != 0 && overlay != 0 {
        rgb(b[(256 * ((mode >> 3) & 31) + overlay) as usize])
    } else if group >= 26 {
        gamma(b, word >> 24, 22) << 16
            | gamma(b, (word >> 16) & 255, 12) << 8
            | gamma(b, (word >> 8) & 255, 2)
    } else {
        rgb(b[if group == 0 {
            (word & 0xfff) as usize
        } else {
            (256 * group + (word & 255)) as usize
        }])
    };
    let cursor = &b[0x2500..0x2600];
    let control = cursor[0x81];
    if control & 1 == 0 || control & (1 << 5) != 0 {
        return colour;
    }
    let cx = (cursor[0x80] >> 16) as i16 as i32;
    let cy = cursor[0x80] as i16 as i32;
    let row = y as i32 - cy;
    let col = x as i32 - cx;
    if control & (1 << 2) != 0 {
        if row == 0 || col == 0 {
            colour = rgb(cursor[0x91]);
        }
    } else if (0..32).contains(&row) && (0..32).contains(&col) {
        let row = row as usize;
        let col = col as usize;
        let plane = cursor[0xc0 + row * 2 + col / 16] >> ((col % 16) * 2) & 3;
        let glyph = cursor[row * 4 + col / 8] >> ((col % 8) * 4) & 15;
        if plane == 3 && glyph != 15 {
            colour = rgb(cursor[0x90 + glyph as usize]);
        }
    }
    colour
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn window_swap_overlay_and_cursor_resolve_in_order() {
        let mut r = Render::default();
        let mut b = vec![0; 0x4000];
        r.colour[0][0] = 1;
        r.colour[1][0] = 2;
        r.did[0] = 3 << 3;
        b[0x2903] = 4 << 4;
        b[4 * 256 + 1] = 0x3fc00000;
        b[4 * 256 + 2] = 0x000ff000;
        assert_eq!(resolve(&r, &b, 0, 0, 0), 0xff0000);
        assert_eq!(resolve(&r, &b, 1 << 3, 0, 0), 0x00ff00);
        r.overlay[0] = 3;
        b[0x2920] = 1 | (5 << 3);
        b[5 * 256 + 3] = 0x3fc;
        assert_eq!(resolve(&r, &b, 1 << 3, 0, 0), 0x0000ff);
        b[0x2581] = 1;
        b[0x25c0] = 3;
        b[0x2590] = 0x3fffffff;
        assert_eq!(resolve(&r, &b, 1 << 3, 0, 0), 0xffffff);
        b[0x2500] = 15;
        assert_eq!(resolve(&r, &b, 1 << 3, 0, 0), 0x0000ff);
    }
}
