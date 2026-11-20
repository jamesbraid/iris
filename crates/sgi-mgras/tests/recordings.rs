//! What the QEMU shell's recordings expect that the engine does not do, and
//! why. The engine follows the driver's documented sequences (gr4's
//! devices/re4/2d-transactions.md); a recording that goes another way is
//! the recording's artefact, and a flag read taken before the queued work
//! has run is a read that did not wait, as a guest's poll does.

mod support;

use sgi_engine::{BoardEngine, Read};
use sgi_mgras::dev::mgras::back::ImpactBack;
use sgi_mgras::dev::mgras::engine::Mode;
use sgi_mgras::dev::mgras::hq3::host;
use support::*;

/// The driver's rectangle fill, steps 1 to 6 of the page, over (10, 10)-(13, 13).
const FILL: [(u32, u32, bool); 6] = [(0x161, 0x0c006300, false), (0x110, 0, false), (0x05b, 0x332211, false), (0x046, 0x000a_000a, false), (0x047, 0x000d_000d, false), (0x045, 0x18, true)];

fn painted(regs: &[(u32, u32, bool)]) -> usize {
    let mut b = ImpactBack::new(None).unwrap();
    for (r, v, e) in regs {
        b.write(*r, *v, *e);
    }
    b.planes().mem.words.iter().filter(|&&w| w != 0).count()
}

#[test]
fn only_the_alias_execute_paints_a_block() {
    assert_eq!(painted(&FILL), 16);
    // The synthetic fill recordings execute BLOCKXYENDI and never write IR_ALIAS.
    let mut synthetic = FILL;
    synthetic[4].2 = true;
    assert_eq!(painted(&synthetic[..5]), 0, "an execute on BLOCKXYENDI with no primitive selected");
}

#[test]
fn a_flag_read_before_the_queued_query_ran_lacks_its_flag() {
    let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-texture-rgba4-quad").unwrap().1);
    let mut p = Player::new(config(&rep.head, Mode::Inline, None), Drive::Lazy);
    let mut seen = false;
    for c in &rep.cmds {
        if let Cmd::Read { off: 0x020010, .. } = c {
            let flags = |e: &mut sgi_mgras::dev::mgras::engine::MgrasEngine<Mem>| match e.read(host::SET_FLAGS, 32) {
                Read::Value(v) => v as u32,
                other => panic!("{other:?}"),
            };
            assert_eq!(flags(&mut p.e) & host::FLAG_GE_DATA, 0, "the query is still queued");
            drive(&mut p.e);
            assert_ne!(flags(&mut p.e) & host::FLAG_GE_DATA, 0, "a poll sees it once the work ran");
            seen = true;
        }
        p.apply(c);
    }
    assert!(seen);
}
