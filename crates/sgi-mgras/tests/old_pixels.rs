// SPDX-License-Identifier: BSD-3-Clause
// Ported from rust/sgi-mgras/tests/old_pixels.rs in jamesbraid/qemu-sgi-origami, branch gfx-engines.
//! The `pixel` lines of the recordings, which the replay parser skips, hold
//! what the old QEMU model's main plane held at that point. They are checked
//! here against the engine's pixel memory: the engine's framebuffer runs
//! bottom-up, so the old plane's row y is the engine's row 1023 - y.

mod support;

use sgi_mgras::dev::mgras::engine::Mode;
use sgi_mgras::dev::mgras::pixmem::{Buffer, Kind};
use support::*;

/// QEMU's own copies of the recordings, in data/qemu, which carry the fixes the
/// crate's copies from iris do not.
macro_rules! qemu_recordings {
    ($($n:literal),*) => { &[$(($n, include_str!(concat!("data/qemu/", $n, ".replay")))),*] };
}
const QEMU_REPLAYS: &[(&str, &str)] = qemu_recordings!("cmap-xmap-wid", "dma-image-transfer", "fill", "gl-eviction-i3dm", "gl-pixels-rectwrite", "pio-read-oglsnoop");

fn num(s: &str) -> u64 {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).unwrap(),
        None => s.parse().unwrap(),
    }
}

/// (checked, mismatched, first mismatch) for the recording's `pixel` lines.
fn check(name: &str, flip: bool) -> (usize, usize, String) {
    let text = QEMU_REPLAYS.iter().find(|r| r.0 == name).unwrap().1;
    let head = text.lines().find_map(|l| l.strip_prefix("head ")).unwrap().trim().to_string();
    let header = format!("sgi-replay 2\nhead {head}\n");
    let mut cfg = config(&head, Mode::Inline, None);
    cfg.entry_bytes = 8;
    let mut p = Player::new(cfg, Drive::Each);
    let buf = Buffer::new(576, Kind::Wide, 0);
    let (mut seg, mut checked, mut bad, mut first) = (header.clone(), 0, 0, String::new());
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        if w.first() == Some(&"pixel") {
            for c in &parse(&seg).cmds {
                p.apply(c);
            }
            drive(&mut p.e);
            seg = header.clone();
            let (x, y, want) = (num(w[1]) as u32, num(w[2]) as u32, num(w[3]));
            let got = p.e.pixel(&buf, x, if flip { 1023 - y } else { y });
            checked += 1;
            if got & 0xff_ffff != want {
                bad += 1;
                if first.is_empty() {
                    first = format!("{name} line `{line}`: engine {got:#x}");
                }
            }
        } else {
            seg.push_str(line);
            seg.push('\n');
        }
    }
    (checked, bad, first)
}

/// The recordings whose `pixel` lines the engine matches. The synthetic fill and
/// DMA recordings draw at the engine's own rows; real captures are top-down.
/// The overlay's lines name another buffer and the sphere's shaded pixels differ
/// in a colour channel, so neither is asserted.
#[test]
fn the_recordings_the_engine_matches_agree_with_the_old_planes() {
    for (name, at_least, flip) in [("gl-pixels-rectwrite", 9, true), ("cmap-xmap-wid", 2, true), ("gl-eviction-i3dm", 3, true), ("fill", 2, false), ("dma-image-transfer", 2, false), ("pio-read-oglsnoop", 2, true)] {
        let (checked, bad, first) = check(name, flip);
        assert!(checked >= at_least, "{name}: {checked} pixel lines");
        assert_eq!(bad, 0, "{first}");
    }
}
