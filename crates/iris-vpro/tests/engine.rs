//! The engine handle: identity, lent memory, inline and threaded modes.

use std::sync::Arc;

use iris_vpro::dev::odyssey::engine::{memory_bytes, Config, Engine, Lent, Mode};
use iris_vpro::traits::BusDevice;

struct NoMem;
impl BusDevice for NoMem {}

fn config(mode: Mode) -> Config {
    Config { product: "v12".into(), widget: 11, mode, pool: (0, 0), gpu: None }
}

/// The first reads of every recorded V12 boot (QEMU's capture of an Octane2).
#[test]
fn v12_identity_and_widget() {
    // SAFETY: nothing is lent.
    let e = unsafe { Engine::new(config(Mode::Inline), None, Arc::new(NoMem)) }.unwrap();
    e.reset();
    assert_eq!(e.read(0x4, 4), Some(0xac01_3047));
    assert_eq!(e.read(0x24, 4), Some(0xb));
}

#[test]
fn lent_memory_is_zeroed_by_reset_and_size_checked() {
    let bytes = memory_bytes("v12").unwrap();
    assert_eq!(bytes, 128 << 20);
    let mut mem = vec![!0u64; (bytes / 8) as usize];
    let lent = Lent { ptr: mem.as_mut_ptr(), bytes };
    // SAFETY: `mem` outlives the engine and nothing else touches it.
    let e = unsafe { Engine::new(config(Mode::Inline), Some(lent), Arc::new(NoMem)) }.unwrap();
    e.reset();
    drop(e);
    assert!(mem.iter().all(|&w| w == 0));

    let short = Lent { ptr: mem.as_mut_ptr(), bytes: 32 << 20 };
    // SAFETY: as above.
    let refused = unsafe { Engine::new(config(Mode::Inline), Some(short), Arc::new(NoMem)) };
    assert!(refused.is_err());
}

/// Threaded mode runs a store on the board's thread; quiesce waits for it.
#[test]
fn threaded_mode_quiesces() {
    // SAFETY: nothing is lent.
    let e = unsafe { Engine::new(config(Mode::Threaded), None, Arc::new(NoMem)) }.unwrap();
    e.reset();
    // A command-FIFO word with no stream behind it: decoded and dropped.
    while !e.write(0x400000, 8, 0) {}
    assert!(e.quiesce(std::time::Duration::from_secs(5)));
    assert!(e.idle());
    let mut ev = Vec::new();
    e.drain_events(&mut ev);
    assert!(ev.is_empty());
}
