//! Odyssey's integer drawing commands and the board planes they update.
//!
//! Drawing updates the planes used by scanout and host-side inspection.
//! Guest framebuffer readback is not implemented; display conversion lives
//! in `display`.
//!
//! Ported from hw/misc/sgi-odyssey.c and hw/display/sgi/board-memory.c
//! in jamesbraid/qemu-sgi-origami.

use std::collections::BTreeSet;

pub const WIDTH: usize = 1280;
pub const HEIGHT: usize = 1024;

/// Device-owned RGBA8 planes use the donor's red-high word convention.
/// These are model buffers, not an assertion about physical V12 packing.
pub struct Render {
    pub colour: [Vec<u32>; 2],
    pub did: Vec<u8>,
    pub overlay: Vec<u8>,
    pub depth: Vec<u32>,
    pub stencil: Vec<u8>,
    tag: u32,
    register: bool,
    index_word: u32,
    needed: usize,
    operands: Vec<u32>,
    colour_word: u32,
    index: bool,
    draw_mask: u32,
    destination: u32,
    plane_mask: u8,
    primitive: Option<u32>,
    vertices: Vec<(i32, i32)>,
    origin: (i32, i32),
    clip: [(u32, u32, u32, u32); 4],
    id_compare: u32,
    pattern: [u32; 32],
    pattern_shape: u32,
    pattern_mode: u32,
    constant: [u32; 2],
    logic: u8,
    pub unsupported: BTreeSet<u32>,
}
impl Default for Render {
    fn default() -> Self {
        Self {
            colour: [vec![0; WIDTH * HEIGHT], vec![0; WIDTH * HEIGHT]],
            did: vec![0; WIDTH * HEIGHT],
            overlay: vec![0; WIDTH * HEIGHT],
            depth: vec![0; WIDTH * HEIGHT],
            stencil: vec![0; WIDTH * HEIGHT],
            tag: 0,
            register: false,
            index_word: 0,
            needed: 0,
            operands: Vec::with_capacity(16),
            colour_word: 0,
            index: false,
            draw_mask: 1,
            destination: 0,
            plane_mask: 0,
            primitive: None,
            vertices: Vec::with_capacity(4),
            origin: (0, 0),
            clip: [(0, 0, 4095, 4095), (1, 1, 0, 0), (1, 1, 0, 0), (1, 1, 0, 0)],
            id_compare: 0,
            pattern: [0; 32],
            pattern_shape: 0,
            pattern_mode: 0,
            constant: [0; 2],
            logic: 3,
            unsupported: BTreeSet::new(),
        }
    }
}
impl Render {
    fn report(&mut self, word: u32) {
        if self.unsupported.len() < 64 && self.unsupported.insert(word) {
            eprintln!("odyssey: unsupported command {word:#010x}");
        }
    }
    pub fn word(&mut self, word: u32) {
        if self.needed != 0 {
            self.operands.push(word);
            self.needed -= 1;
            if self.needed == 0 {
                self.execute();
            }
            return;
        }
        self.operands.clear();
        self.register = word >> 16 == 1;
        self.tag = if self.register { word & 0xffff } else { word };
        self.needed = match self.tag {
            0xc580cc08 => 3,
            0x8080c800 => 2,
            0x2031 => 8,
            0xe01 | 0xe03 => 3,
            _ if word >> 24 != 0 && word & 0x800000 == 0 => 0,
            _ => arity(word),
        };
        if self.needed == 0 {
            self.execute();
        }
    }
    fn execute(&mut self) {
        let o = self.operands.clone();
        let n = o.len();
        match self.tag {
            0xc580cc08 if !self.register && n == 3 => {
                self.colour_word = (o[0] << 24) | (o[1] << 16) | (o[2] << 8) | 255;
                self.index = false;
            }
            0xc580c406 if !self.register && n == 1 => {
                self.colour_word = o[0];
                self.index_word = o[0];
                self.index = true;
            }
            0x0422 if self.register && n == 1 => self.logic = (o[0] & 15) as u8,
            0x0424 if self.register && n == 1 => {
                self.draw_mask = o[0];
                if o[0] & !5 != 0 {
                    self.report(self.tag);
                }
            }
            0x1017 if self.register && n == 4 => {
                self.destination = o[2];
            }
            0x1023 if self.register && n == 4 => self.plane_mask = (o[0] >> 12) as u8,
            0x4400 if self.register && n == 1 => {
                self.primitive = Some(o[0]);
                self.vertices.clear();
                if o[0] != 7 && o[0] != 0 {
                    self.report(self.tag);
                }
            }
            0x4001 if self.register && n == 0 => {
                self.primitive = None;
                self.vertices.clear();
            }
            0x8080c800 if !self.register && n == 2 => {
                if self.pattern_mode & (1 << 13) != 0 {
                    self.report(self.tag);
                    return;
                }
                let p = (
                    (o[0] as i32).wrapping_add(self.origin.0),
                    (o[1] as i32).wrapping_add(self.origin.1),
                );
                match self.primitive {
                    Some(0) => self.paint(p.0, p.1, self.colour_word),
                    Some(7) => {
                        self.vertices.push(p);
                        if self.vertices.len() == 4 {
                            self.quad();
                            self.vertices.clear();
                        }
                    }
                    _ => self.report(self.tag),
                }
            }
            0x0841 if self.register && n == 2 => self.origin = (sign13(o[0]), sign13(o[1])),
            0x0840 if self.register && n == 2 => self.id_compare = o[1],
            0x204b if self.register && n == 8 => {
                for i in 0..4 {
                    self.clip[i] = (
                        o[2 * i] >> 12 & 4095,
                        o[2 * i] & 4095,
                        o[2 * i + 1] >> 12 & 4095,
                        o[2 * i + 1] & 4095,
                    );
                }
            }
            0x3c4e | 0x3c4f if self.register && n == 16 => {
                let at = if self.tag == 0x3c4e { 0 } else { 16 };
                self.pattern[at..at + 16].copy_from_slice(&o);
            }
            0x043f if self.register && n == 1 => self.pattern_shape = o[0],
            0x0404 if self.register && n == 1 => {
                self.pattern_mode = o[0];
                if o[0] & (1 << 13) != 0 {
                    self.report(self.tag);
                }
            }
            0x0c21 if self.register && n == 3 => {
                self.constant = [o[1], o[2]];
                if o[0] != 0 {
                    self.report(self.tag);
                }
            }
            0x2031 if !self.register && n == 8 => self.copy(&o),
            // Flush and strobe words carry no visible rendering state.
            0x0443 | 0x0046 | 0x0019 | 0x1453 | 0x0658 if self.register => {}
            _ => self.report(self.tag),
        }
    }
    fn buffers(&self) -> [bool; 2] {
        [self.draw_mask & 3 != 0, self.draw_mask & 12 != 0]
    }
    fn allowed(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 || x >= WIDTH as i32 || y >= HEIGHT as i32 {
            return false;
        }
        let at = y as usize * WIDTH + x as usize;
        if self.id_compare & (1 << 16) != 0
            && (self.did[at] ^ self.id_compare as u8) & (self.id_compare >> 8) as u8 != 0
        {
            return false;
        }
        self.clip.iter().any(|&(x0, y0, x1, y1)| {
            x as u32 >= x0 && x as u32 <= x1 && y as u32 >= y0 && y as u32 <= y1
        })
    }
    fn paint(&mut self, x: i32, y: i32, value: u32) {
        if !self.allowed(x, y) {
            return;
        }
        let at = y as usize * WIDTH + x as usize;
        let buffers = self.buffers();
        let logic = self.pattern_mode & (1 << if self.index { 19 } else { 20 }) != 0;
        if self.destination & !0x20800000 == 0x00080080 {
            let out = if logic {
                let destination = if buffers[1] {
                    self.overlay[at]
                } else {
                    self.did[at]
                };
                logic_op(self.logic, value, destination as u32)
            } else {
                value
            };
            for (b, enabled) in buffers.into_iter().enumerate() {
                if !enabled {
                    continue;
                }
                let bytes = if b == 0 {
                    &mut self.did
                } else {
                    &mut self.overlay
                };
                bytes[at] = (bytes[at] & !self.plane_mask) | (out as u8 & self.plane_mask);
            }
        } else {
            let src = if self.index { value & 0xfff } else { value };
            for (b, enabled) in buffers.into_iter().enumerate() {
                if enabled {
                    self.colour[b][at] = if logic {
                        let p = logic_op(self.logic, src, self.colour[b][at]);
                        if self.index {
                            p & 0xfff
                        } else {
                            p | 255
                        }
                    } else {
                        src
                    };
                }
            }
        }
    }
    fn pattern_bit(&self, u: u32, v: u32) -> bool {
        let (word, bit) = match self.pattern_shape & !0x10 {
            4 if u < 32 && v < 32 => (v, 31 - u),
            5 if u < 64 && v < 16 => (2 * v + (u >> 5), 31 - (u & 31)),
            6 if u < 16 && v < 64 => (v >> 1, 31 - 16 * (v & 1) - u),
            _ => return false,
        };
        self.pattern[word as usize] >> bit & 1 != 0
    }
    fn quad(&mut self) {
        let minx = self.vertices.iter().map(|p| p.0).min().unwrap();
        let maxx = self.vertices.iter().map(|p| p.0).max().unwrap();
        let miny = self.vertices.iter().map(|p| p.1).min().unwrap();
        let maxy = self.vertices.iter().map(|p| p.1).max().unwrap();
        let patterned =
            self.pattern_mode & 0x18 == 0x18 && [4, 5, 6].contains(&(self.pattern_shape & !0x10));
        if self.destination & !0x20800000 == 0x00080080 && !patterned {
            for y in miny.max(0)..maxy.min(HEIGHT as i32) {
                for x in minx.max(0)..maxx.min(WIDTH as i32) {
                    if !self.allowed(x, y) {
                        continue;
                    }
                    let at = y as usize * WIDTH + x as usize;
                    let [ids, overlay] = self.buffers();
                    if ids {
                        self.did[at] = (self.did[at] & !self.plane_mask)
                            | (self.index_word as u8 & self.plane_mask);
                    }
                    if overlay {
                        self.overlay[at] = (self.overlay[at] & !self.plane_mask)
                            | (self.index_word as u8 & self.plane_mask);
                    }
                }
            }
            return;
        }
        for y in miny.max(0)..maxy.min(HEIGHT as i32) {
            for x in minx.max(0)..maxx.min(WIDTH as i32) {
                let value = if patterned
                    && !self.pattern_bit(
                        (x as i64 - minx as i64) as u32,
                        (y as i64 - miny as i64) as u32,
                    ) {
                    if self.pattern_shape & 0x10 == 0 {
                        continue;
                    }
                    let r = self.constant[0] >> 12 & 0xfff;
                    let g = self.constant[0] & 0xfff;
                    let b = self.constant[1] >> 12 & 0xfff;
                    if self.index {
                        r
                    } else {
                        (r >> 4) << 24 | (g >> 4) << 16 | (b >> 4) << 8 | 255
                    }
                } else {
                    self.colour_word
                };
                self.paint(x, y, value);
            }
        }
    }
    fn copy(&mut self, o: &[u32]) {
        if o[0] == 0x30000019 {
            self.report(self.tag);
            return;
        }
        let source = (o[2] >> 12) & 15;
        let dest = (o[7] >> 20) & 15;
        if source != ((o[4] >> 24) & 15) || dest != ((o[4] >> 20) & 15) {
            self.report(self.tag);
            return;
        }
        let pairs = if [0, 4].contains(&source) && [0, 4].contains(&dest) {
            vec![(source as usize >> 2, dest as usize >> 2)]
        } else if source == dest && [2, 3].contains(&source) {
            self.buffers()
                .into_iter()
                .enumerate()
                .filter(|p| p.1)
                .map(|p| (p.0, p.0))
                .collect()
        } else {
            self.report(self.tag);
            return;
        };
        let (sx, sy) = ((o[1] & 65535) as usize, (o[1] >> 16) as usize);
        let (w, h) = ((o[3] & 65535) as usize, (o[3] >> 16) as usize);
        let (dx, dy) = ((o[6] & 65535) as usize, (o[6] >> 16) as usize);
        let w = w.min(WIDTH.saturating_sub(dx));
        let h = h.min(HEIGHT.saturating_sub(dy));
        for (src, dst) in pairs {
            for row in 0..h {
                let row = if dy > sy { h - 1 - row } else { row };
                for col in 0..w {
                    let col = if dx > sx { w - 1 - col } else { col };
                    let p = if sx + col < WIDTH && sy + row < HEIGHT {
                        self.colour[src][(sy + row) * WIDTH + sx + col]
                    } else {
                        0
                    };
                    self.colour[dst][(dy + row) * WIDTH + dx + col] = p;
                }
            }
        }
    }
}
fn arity(word: u32) -> usize {
    let n = ((word >> 10) & 15) as usize;
    if n == 15 {
        16
    } else {
        n
    }
}
fn sign13(word: u32) -> i32 {
    ((word << 19) as i32) >> 19
}
pub fn logic_op(op: u8, s: u32, d: u32) -> u32 {
    match op & 15 {
        0 => 0,
        1 => s & d,
        2 => s & !d,
        3 => s,
        4 => !s & d,
        5 => d,
        6 => s ^ d,
        7 => s | d,
        8 => !(s | d),
        9 => !(s ^ d),
        10 => !d,
        11 => s | !d,
        12 => !s,
        13 => !s | d,
        14 => !(s & d),
        _ => u32::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn words(r: &mut Render, words: &[u32]) {
        for &w in words {
            r.word(w);
        }
    }
    fn rect(r: &mut Render, x: u32, y: u32, w: u32, h: u32) {
        words(r, &[0x00014400, 7]);
        for (x, y) in [(x, y), (x + w, y), (x + w, y + h), (x, y + h)] {
            words(r, &[0x8080c800, x, y]);
        }
        r.word(0x00014001);
    }
    #[test]
    fn glyph_mask_preserves_background() {
        let mut r = Render::default();
        words(&mut r, &[0xc580c406, 9]);
        rect(&mut r, 0, 0, 4, 1);
        let mut pattern = vec![0x00013c4e, 0xa0000000];
        pattern.extend([0; 15]);
        words(&mut r, &pattern);
        words(&mut r, &[0x0001043f, 4, 0x00010404, 0x18, 0xc580c406, 7]);
        rect(&mut r, 0, 0, 4, 1);
        assert_eq!(&r.colour[0][..4], &[7, 9, 7, 9]);
    }

    #[test]
    fn copy_overlaps_and_selects_each_buffer() {
        let mut r = Render::default();
        r.colour[0][..5].copy_from_slice(&[1, 2, 3, 4, 5]);
        words(&mut r, &[0x2031, 0, 0, 0, 0x10004, 0, 0, 1, 0]);
        assert_eq!(&r.colour[0][..5], &[1, 1, 2, 3, 4]);
        assert_eq!(&r.colour[1][..5], &[0; 5]);
    }

    #[test]
    fn auxiliary_logic_and_clear_keep_distinct_write_rules() {
        for (clear, expected) in [(false, (5, 5)), (true, (1, 1))] {
            let mut r = Render::default();
            r.did[0] = 2;
            r.overlay[0] = 4;
            words(
                &mut r,
                &[
                    0x00010424,
                    5,
                    0x00011017,
                    0,
                    80,
                    0x00080080,
                    0,
                    0x00011023,
                    0xff << 12,
                    0,
                    0,
                    0,
                    0x00010422,
                    6,
                    0x00010404,
                    1 << 19,
                    0xc580c406,
                    1,
                ],
            );
            if clear {
                rect(&mut r, 0, 0, 1, 1);
            } else {
                words(&mut r, &[0x00014400, 0, 0x8080c800, 0, 0, 0x00014001]);
            }
            assert_eq!((r.did[0], r.overlay[0]), expected, "clear={clear}");
        }
    }
}
