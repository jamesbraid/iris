//! What the engine answers before the guest has asked it anything: the GE
//! revision the kernel reads at 0x028004 ahead of its first GE query.

mod support;

use sgi_engine::{BoardEngine, Read};
use sgi_mgras::dev::mgras::engine::Mode;
use sgi_mgras::dev::mgras::hq3::host;
use support::*;

fn lo<E: BoardEngine>(e: &mut E) -> Read {
    e.read(host::GE_READBACK_LO, 32)
}

#[test]
fn the_configured_ge_revision_reads_until_the_first_query_replaces_it() {
    let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-new-context-slot-reuse").unwrap().1);
    assert!(rep.cmds.iter().any(|c| matches!(c, Cmd::Read { off: 0x028004, val: 0x3f800000, .. })), "the recording asks a GE query");
    let mut cfg = config(&rep.head, Mode::Inline, None);
    cfg.ge_revision = 3;
    let mut p = Player::new(cfg, Drive::Each);
    assert_eq!((lo(&mut p.e), p.e.read(host::GE_READBACK_HI, 32)), (Read::Value(3), Read::Value(0)));
    rep.cmds.iter().for_each(|c| p.apply(c));
    drive(&mut p.e);
    assert_ne!(lo(&mut p.e), Read::Value(3), "a query's answer replaced the revision");
    p.e.quiesce(u64::MAX).unwrap();
    p.e.reset().unwrap();
    assert_eq!(lo(&mut p.e), Read::Value(3), "power-on again");
}

#[test]
fn an_engine_not_told_a_revision_reads_zero() {
    let mut p = Player::new(config("sgi-mgras", Mode::Inline, None), Drive::Each);
    assert_eq!(lo(&mut p.e), Read::Value(0));
}
