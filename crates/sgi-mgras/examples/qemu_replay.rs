//! Replays QEMU's MGRAS recordings (`tests/data/mgras/*.replay`, the
//! `sgi-replay 2` format) into the engine the way an Octane's HQ4 front
//! would feed it, and reports what it cost and a hash of what it stored.
//!
//! Usage: qemu_replay [--jit off|sync] [--threaded] [--runs N] FILE...

#[path = "../tests/support/mod.rs"]
mod support;

use std::time::Instant;

use sgi_mgras::dev::mgras::engine::Mode;
use support::*;

fn main() {
    let mut jit = None;
    let mut mode = Mode::Inline;
    let mut runs = 1;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            #[cfg(feature = "gr4-jit")]
            "--jit" => {
                use sgi_mgras::dev::mgras::rss_jit::{MODE_OFF, MODE_SYNC};
                jit = Some(if args.next().unwrap() == "sync" { MODE_SYNC } else { MODE_OFF });
            }
            "--threaded" => mode = Mode::Threaded,
            "--runs" => runs = args.next().unwrap().parse().unwrap(),
            f => files.push(f.to_string()),
        }
    }
    for f in &files {
        let rep = parse(&std::fs::read_to_string(f).unwrap());
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..runs {
            let mut p = Player::new(config(&rep.head, mode, jit), Drive::Lazy);
            let t = Instant::now();
            rep.cmds.iter().for_each(|c| p.apply(c));
            drive(&mut p.e);
            times.push(t.elapsed().as_secs_f64());
            last = Some(p.finish());
        }
        let o = last.unwrap();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let words = o.fifo_words.max(1) as f64;
        println!(
            "{f}: fifo_words {} hq3_words {} min {:.3} ms median {:.3} ms ({:.0} / {:.0} ns a word) hash {:016x} flags {:#x} notes {}",
            o.fifo_words,
            o.hq3_words,
            times[0] * 1e3,
            times[times.len() / 2] * 1e3,
            times[0] * 1e9 / words,
            times[times.len() / 2] * 1e9 / words,
            o.hash,
            o.flags,
            o.notes.len()
        );
    }
}
