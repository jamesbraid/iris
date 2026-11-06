//! What every machine gets from `Workstation`'s provided methods, checked on
//! each machine type with the fixture the exit-persistence tests use.
//!
//! The Indy-family, O2 and Octane machines the tests need are built once and
//! shared through [`with_shared`], which hands each test the same machine and
//! expects it back as found; anything that has to change or drop a machine
//! uses an Octane. (Sharing matters most for the O2: its rendering-engine
//! thread holds the bus for the life of the process, so each O2 a test builds
//! keeps one of the process's 64 device-map tables.)

use std::sync::{Mutex, MutexGuard};

use crate::config::NetMode;
use crate::exit_persistence_tests::{build, config, make_images, on_big_stack, serial, Dir, Machine};
use crate::machine_state::GuestState;
use crate::workstation::Workstation;

static SHARED: [Mutex<Option<Box<dyn Workstation>>>; 5] = [const { Mutex::new(None) }; 5];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Build `machine` over fresh images on a thread with the stack it wants,
/// with the network in `mode`.
fn built(machine: Machine, mode: NetMode) -> Box<dyn Workstation> {
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let dir = Dir::new(&format!("ws-{machine:?}-{mode:?}"));
            make_images(&dir);
            let mut cfg = config(machine, &dir, false);
            cfg.network.mode = mode;
            // The images stay for the life of the process.
            std::mem::forget(dir);
            build(cfg)
        })
        .unwrap();
    handle.join().expect("the machine builds")
}

/// Run `f` on the one machine of this type that the tests share. The machine
/// is never started and never dropped; `f` hands it back as it found it.
pub(crate) fn with_shared<R>(machine: Machine, f: impl FnOnce(&mut dyn Workstation) -> R) -> R {
    let _serial = serial();
    let mut slot = lock(&SHARED[machine as usize]);
    let m = slot.get_or_insert_with(|| built(machine, NetMode::Nat));
    f(&mut **m)
}

/// The NAT accessors and lifecycle on the shared Indy, IP28 and O2: the NAT
/// hands the gateway's addresses and the machine starts `Idle` with no monitor
/// up.
#[test]
fn the_provided_methods_answer_on_every_machine_type() {
    for machine in [Machine::Indy, Machine::Indigo2Ip28, Machine::O2] {
        with_shared(machine, |m| {
            assert_eq!(m.lifecycle(), GuestState::Idle, "{machine:?}");
            assert_eq!(m.monitor_addr(), None, "{machine:?}: no monitor before start");

            let mut cfg = crate::config::MachineConfig::default();
            cfg.network.mode = NetMode::Nat;
            let want = crate::net::GatewayConfig::from_network(&cfg.network());
            assert_eq!(m.nat_expected(), Some((want.client_ip, want.gateway_ip, want.netmask)), "{machine:?}");
            assert!(m.nat_control().is_some() && m.net_frames().is_some(), "{machine:?}");
        });
    }
}

/// On an Octane, which a test can build and drop freely: the lifecycle state
/// round-trips through `set_lifecycle`, a second `register_system_controller`
/// panics, dropping the machine nulls the slot a holder kept (which
/// `with_machine_slot` reports as "not running"), and the NAT accessors
/// follow the link, answering only while it is plugged in.
#[test]
fn a_fresh_octane_provides_the_lifecycle_slot_and_nat_methods() {
    let _serial = serial();
    let mut m = built(Machine::Octane, NetMode::Nat);
    assert_eq!(m.lifecycle(), GuestState::Idle);
    m.set_lifecycle(GuestState::Halted);
    assert_eq!(m.lifecycle(), GuestState::Halted);
    assert_eq!(m.status().state, GuestState::Halted);
    assert_eq!(m.control().state(), GuestState::Halted);
    assert!(m.nat_control().is_some() && m.nat_expected().is_some() && m.net_frames().is_some());

    m.register_system_controller();
    let second = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| m.register_system_controller()));
    let msg = second.expect_err("a second register must panic");
    let msg = msg.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| msg.downcast_ref::<String>().cloned()).unwrap_or_default();
    assert!(msg.contains("called twice"), "{msg}");

    let slot = m.machine_slot();
    assert!(!slot.lock().0.is_null());
    // Dropping a machine needs a big stack as well.
    on_big_stack(move || drop(m));
    assert!(slot.lock().0.is_null());

    let off = built(Machine::Octane, NetMode::Off);
    assert!(off.nat_control().is_none() && off.nat_expected().is_none() && off.net_frames().is_none());
    on_big_stack(move || drop(off));
}
