//! Save state as chunks: an engine stopped part way through a recording,
//! saved, and loaded into a fresh engine carries on to the same words as the
//! one that never stopped.

mod support;

use sgi_mgras::dev::mgras::engine::{Board, Mode};
use sgi_engine::{BoardEngine, Chunk, Refused, VecSource};
use support::*;

fn recording(name: &str) -> Replay {
    parse(REPLAYS.iter().find(|r| r.0 == name).unwrap().1)
}

fn source(c: &[Chunk]) -> VecSource {
    VecSource::new(c.to_vec())
}

/// Stop at `at`, save, finish on the same engine; load into another and
/// finish there from the same point.
fn split(rep: &Replay, at: usize, mode: Mode) {
    let cfg = config(&rep.head, mode, None);
    let mut a = Player::new(cfg, Drive::Lazy);
    rep.cmds[..at].iter().for_each(|c| a.apply(c));
    a.e.quiesce(u64::MAX).expect("quiesce");
    let mut chunks = Vec::new();
    a.e.save(&mut chunks).expect("save");
    let mut b = a.twin(cfg);
    b.e.quiesce(u64::MAX).expect("quiesce the new engine");
    b.e.load(&mut source(&chunks)).expect("load");
    b.e.resume();
    a.e.resume();
    rep.cmds[at..].iter().for_each(|c| a.apply(c));
    rep.cmds[at..].iter().for_each(|c| b.apply(c));
    let (oa, ob) = (a.finish(), b.finish());
    assert_eq!(oa.hash, ob.hash, "{} split at {at}", rep.head);
    assert_eq!((oa.flags, oa.reads), (ob.flags, ob.reads), "{} split at {at}", rep.head);
}

#[test]
fn a_saved_engine_carries_on_as_the_original_does() {
    for name in ["gl-swap-tmesh-sphere", "gl-texture-rgba4-quad", "gl-eviction-i3dm", "gl-deep-swap-clock-ssi", "root-icon"] {
        let rep = recording(name);
        let n = rep.cmds.len();
        for at in [n / 7, n / 2, n * 6 / 7] {
            split(&rep, at, Mode::Inline);
        }
        split(&rep, n / 2, Mode::Threaded);
    }
}

/// Some split falls inside a command: the parser holds words of a packet.
#[test]
fn a_split_inside_a_packet_resumes_the_packet() {
    let rep = recording("gl-swap-tmesh-sphere");
    let mut found = 0;
    let mut p = Player::new(config(&rep.head, Mode::Inline, None), Drive::Each);
    for (i, c) in rep.cmds.iter().enumerate() {
        p.apply(c);
        if p.e.describe().contains("needs") && i > 100 && found < 3 {
            found += 1;
            split(&rep, i + 1, Mode::Inline);
        }
    }
    assert!(found > 0, "no split fell inside a packet");
}

#[test]
fn a_state_that_does_not_fit_is_refused_and_changes_nothing() {
    let rep = recording("gl-swap-tmesh-sphere");
    let mut p = Player::new(config(&rep.head, Mode::Inline, None), Drive::Each);
    rep.cmds[..rep.cmds.len() / 2].iter().for_each(|c| p.apply(c));
    p.e.quiesce(u64::MAX).unwrap();
    let mut good = Vec::new();
    p.e.save(&mut good).unwrap();
    let before = p.e.state_hash().unwrap();

    let mismatch = |p: &mut Player, c: &[Chunk]| matches!(p.e.load(&mut source(c)), Err(Refused::Mismatch(_)));
    let mut bad = good.clone();
    bad.push(Chunk { name: "pixels".into(), index: u64::MAX, bytes: vec![0; 3072 * 8] });
    assert!(mismatch(&mut p, &bad), "a page past the memory");
    let mut bad = good.clone();
    bad.retain(|c| c.name != "rss");
    assert!(mismatch(&mut p, &bad), "no raster engine");
    let mut bad = good.clone();
    bad.iter_mut().find(|c| c.name == "hq3").unwrap().bytes.truncate(10);
    assert!(mismatch(&mut p, &bad), "a cut parser state");
    let mut bad = good.clone();
    bad.push(good.iter().find(|c| c.name == "te1").unwrap().clone());
    assert!(mismatch(&mut p, &bad), "a part twice");
    let mut bad = good.clone();
    bad.push(Chunk { name: "other".into(), index: 0, bytes: vec![] });
    assert!(mismatch(&mut p, &bad), "a part nobody knows");
    // Another board's state.
    let mut max = Player::new(config("sgi-mgras-ssi", Mode::Inline, None), Drive::Each);
    max.e.quiesce(u64::MAX).unwrap();
    assert!(mismatch(&mut max, &good), "{:?}", Board::Max);

    assert_eq!(p.e.state_hash().unwrap(), before, "a refused state changed the engine");
    p.e.load(&mut source(&good)).unwrap();
    assert_eq!(p.e.state_hash().unwrap(), before);
}
