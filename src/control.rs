//! How a front end (the monitor, the CI socket, the GUI) reaches a machine:
//! its lifecycle state, the channel its devices raise machine events on
//! (hard reset, power-off), and the pointer cell the monitor thread, the
//! event thread and the CI server share. Every machine carries one; none of
//! it touches a device, a bus or a CPU, and all of it runs on monitor, CI,
//! GUI or start/stop threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::machine_state::GuestState;
use crate::monitor::Monitor;
use crate::traits::MachineEvent;
use crate::workstation::{MachineRef, MachineSlot, Workstation};

pub struct Control {
    /// Never `Running`: `Workstation::status` derives that from the CPU.
    state: GuestState,
    /// The nullable pointer cell the monitor and CI thread share. Created on
    /// first use and nulled by [`Control::release`], so a handle that
    /// outlives the machine sees "machine is not running" instead of freed
    /// memory.
    slot: Option<MachineSlot>,
    tx: mpsc::Sender<MachineEvent>,
    /// Taken once by [`Control::register`].
    events: Option<mpsc::Receiver<MachineEvent>>,
    /// Set once the event thread runs. Until then a guest power-off ends the
    /// process itself, where the O2's service thread has only this flag.
    registered: Arc<AtomicBool>,
}

impl Control {
    pub fn new() -> Control {
        let (tx, events) = mpsc::channel();
        Control { state: GuestState::Idle, slot: None, tx, events: Some(events), registered: Arc::new(AtomicBool::new(false)) }
    }

    /// The sending end the devices raise `HardReset` and `PowerOff` on. The
    /// channel is unbounded: a bounded one would drop a guest power-off
    /// queued behind a few resets.
    pub fn sender(&self) -> mpsc::Sender<MachineEvent> {
        self.tx.clone()
    }

    /// Whether the event thread runs yet.
    pub fn registered(&self) -> Arc<AtomicBool> {
        self.registered.clone()
    }

    pub fn state(&self) -> GuestState {
        self.state
    }

    pub fn set_state(&mut self, state: GuestState) {
        debug_assert!(state != GuestState::Running, "Running is read from the CPU, never stored");
        self.state = state;
    }

    /// `Idle` or `Halted` becomes `Starting`; a state with no such transition
    /// stays as it is.
    pub fn started(&mut self) {
        if let Ok(starting) = self.state.transition(GuestState::Starting) {
            self.state = starting;
        }
    }

    /// Anything but a machine that was never started becomes `Halted`; there
    /// is nothing to halt in `Idle`.
    pub fn stopped(&mut self) {
        if self.state != GuestState::Idle {
            self.state = GuestState::Halted;
        }
    }

    /// The shared slot, created on first use. `me` must be the machine's
    /// final address (boxed, or on `main`'s stack): the slot records it.
    pub(crate) fn slot(&mut self, me: *mut dyn Workstation) -> MachineSlot {
        let slot = self.slot.get_or_insert_with(|| Arc::new(Mutex::new(MachineRef(me))));
        debug_assert!(std::ptr::addr_eq(slot.lock().0, me), "machine moved after its slot was handed out");
        slot.clone()
    }

    /// Register the monitor's system commands (`reset`, `save`, `load`, ...)
    /// and start the thread that carries out the devices' machine events.
    /// Panics if called twice.
    pub(crate) fn register(&mut self, me: *mut dyn Workstation, monitor: &Arc<Monitor>) {
        assert!(self.events.is_some(), "register_system_controller called twice");
        let slot = self.slot(me);
        crate::workstation::register_system_controller(monitor, slot, self.events.take());
        self.registered.store(true, Ordering::SeqCst);
    }

    /// Null the slot. Taking its lock waits for a monitor, event or CI command
    /// in flight, so none sees a half-destroyed machine. Every machine's
    /// `Drop` begins here; no controller command drops the machine, so this
    /// never waits on a lock its own thread holds.
    pub(crate) fn release(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.lock().0 = MachineRef::null();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_moves_idle_and_halted_to_starting_and_ignores_the_rest() {
        let mut c = Control::new();
        assert_eq!(c.state(), GuestState::Idle);
        c.started();
        assert_eq!(c.state(), GuestState::Starting);
        c.started();
        assert_eq!(c.state(), GuestState::Starting);
        c.set_state(GuestState::Halted);
        c.started();
        assert_eq!(c.state(), GuestState::Starting);
    }

    #[test]
    fn stopped_leaves_idle_alone_and_halts_anything_else() {
        let mut c = Control::new();
        c.stopped();
        assert_eq!(c.state(), GuestState::Idle);
        c.started();
        c.stopped();
        assert_eq!(c.state(), GuestState::Halted);
        c.stopped();
        assert_eq!(c.state(), GuestState::Halted);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "never stored")]
    fn set_state_refuses_running() {
        Control::new().set_state(GuestState::Running);
    }

    #[test]
    fn the_sender_reaches_the_receiver_register_hands_on() {
        let c = Control::new();
        assert!(!c.registered().load(Ordering::SeqCst));
        c.sender().send(MachineEvent::PowerOff).unwrap();
        assert!(c.events.as_ref().unwrap().try_recv().is_ok());
    }
}
