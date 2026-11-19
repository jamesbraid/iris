//! The engine run three ways over QEMU's MGRAS recordings: inline, running
//! its queue after every recorded command; inline, running it only when
//! forced; and on a worker thread. Pixels, texture memory, flags, the
//! values read back, the notes and the damage must be identical, so a
//! result never depends on host scheduling.

mod support;

use sgi_mgras::dev::mgras::engine::Mode;
use support::*;

/// The recordings that draw: the hash must differ from an engine that drew nothing.
const DRAWS: &[&str] = &[
    "char-modes",
    "gl-deep-swap-clock-ssi",
    "gl-eviction-i3dm",
    "gl-pixels-rectwrite",
    "gl-swap-tmesh-sphere",
    "gl-texture-rgba4-quad",
    "root-icon",
];

#[test]
fn inline_and_threaded_store_the_same_words() {
    for (name, text) in REPLAYS {
        let rep = parse(text);
        let each = play(&rep, Mode::Inline, None, Drive::Each);
        let lazy = play(&rep, Mode::Inline, None, Drive::Lazy);
        let threaded = play(&rep, Mode::Threaded, None, Drive::Lazy);
        assert_eq!(each, lazy, "{name}: inline, run each command against run when forced");
        assert_eq!(each, threaded, "{name}: inline against threaded");
        assert!(each.hq3_words > 0, "{name}: no word reached the parser");
        if DRAWS.contains(name) {
            assert_ne!(each.hash, empty_hash(&rep.head), "{name} drew nothing");
            assert!(!each.damage.is_empty(), "{name}: no damage was reported");
        }
    }
}

#[cfg(feature = "gr4-jit")]
#[test]
fn the_jit_stores_the_interpreters_words() {
    use sgi_mgras::dev::mgras::rss_jit::{MODE_OFF, MODE_SYNC};
    for name in ["gl-swap-tmesh-sphere", "gl-texture-rgba4-quad", "root-icon"] {
        let rep = parse(REPLAYS.iter().find(|r| r.0 == name).unwrap().1);
        let off = play(&rep, Mode::Inline, Some(MODE_OFF), Drive::Each);
        let jit = play(&rep, Mode::Inline, Some(MODE_SYNC), Drive::Each);
        assert_eq!(off.hash, jit.hash, "{name}");
    }
}

/// The texture quad on a Maximum Impact: its two raster engines meet at
/// every texture write, where they used to deadlock on one thread. Run on one
/// engine and on two they store the same words.
#[test]
fn two_stepped_engines_store_what_one_does_through_texture_writes() {
    let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-texture-rgba4-quad").unwrap().1);
    let one = {
        let mut p = Player::new(config("sgi-mgras-si-t", Mode::Inline, None), Drive::Each);
        rep.cmds.iter().for_each(|c| p.apply(c));
        p.finish()
    };
    let two = {
        let mut p = Player::new(config("sgi-mgras-ssi", Mode::Inline, None), Drive::Each);
        assert_eq!(p.e.raster_engines(), 2);
        rep.cmds.iter().for_each(|c| p.apply(c));
        p.finish()
    };
    assert_ne!(one.hash, empty_hash("sgi-mgras-si-t"));
    assert_eq!(one.hash, two.hash);
}
