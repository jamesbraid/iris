//! Interrupt pins and board wires, combined per host output.
//!
//! Every pin of every device number is a line (`device * 4 + pin - 1`) and
//! the board's wires follow them. The combiner keeps each line's level and
//! tells the host the OR of everything routed to an output, so on a shared
//! output one source cannot clear another's interrupt.
//!
//! No lock of ours is held while the host is driven. A level change is one
//! `fetch_or` or `fetch_and`, one `dirty` store and one `swap` on
//! `delivering`; whoever holds `delivering` reads the OR, calls the host and
//! looks again, so deliveries to one output are serialised and in order, and
//! the last one is the OR after the last change.
//!
//! A fall followed by a rise before the deliverer reads the OR would reach
//! the host as a steady 1. That is harmless to a level host and wrong for an
//! edge host (the Bridge sends a HEART packet on 0 then 1), so a change that
//! takes an output from 1 to 0 latches `fell` and the deliverer sends 0 then
//! 1. A rise followed by a fall that nobody saw is coalesced.
//!
//! `delivering` is released with a SeqCst store and taken with a SeqCst swap
//! because the protocol is a store-then-load handshake: a mutator stores
//! `dirty` and then looks at `delivering`; the holder clears `delivering`
//! and then looks at `dirty`. With a release store the holder's load of
//! `dirty` may be satisfied before the store of `delivering` is visible (a
//! store buffer does exactly that), and a change would sit undelivered.

use std::sync::{Arc, Weak};

use crate::irq::{InterruptController, IrqLine};
use crate::pci::PciDma;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};

/// A board wire that is not a slot pin, for example one chip's interrupt on
/// a bridge input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wire(pub u8);

/// How one root bus is wired, as data. No labels, no provenance: those are
/// comments beside the static that holds it.
pub struct PciWiring {
    /// Device numbers on the bus (at most 32).
    pub devices: u8,
    /// Host outputs (CRIME inputs on the O2, Bridge pins on the Octane).
    pub outputs: u8,
    /// Device to output for INTA..INTD; a pin not listed is not wired.
    pub pins: &'static [(u8, [Option<u8>; 4])],
    /// Output of `Wire(i)`.
    pub wires: &'static [u8],
}

/// What a bus asks of its host. Nothing else.
pub trait PciUpstream: Send + Sync {
    /// The DMA identity of root slot `requester`.
    fn dma_for(&self, requester: u8) -> Arc<dyn PciDma>;
    /// Output `output` is now `level`: the OR of every pin and wire routed
    /// to it. Calls are serialised and in order per output.
    fn set_output(&self, output: u8, level: bool);
}

/// Lines `0..PIN_LINES` are pins; wire `n` is line `PIN_LINES + n`.
const PIN_LINES: u32 = 32 * 4;

/// What drives one output, and its delivery state.
struct Out {
    /// Pin lines (two words) and wires routed here.
    pins: [u64; 2],
    wires: u32,
    /// A level changed since the deliverer last cleared this.
    dirty: AtomicBool,
    /// Someone is calling the host for this output.
    delivering: AtomicBool,
    /// The output went 1 to 0 at some point since the last delivery.
    fell: AtomicBool,
    /// Send the current level even though the host has it (redrive).
    force: AtomicBool,
    /// The level the host has; the deliverer's alone.
    last: AtomicBool,
}

/// Where a test hook runs: after a lower's `fetch_and` and before it works
/// out the output's level, or between the release of `delivering` and the
/// look at `dirty`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Point {
    Lowered,
    Released,
}

pub(super) struct Combiner {
    up: Weak<dyn PciUpstream>,
    pins: [AtomicU64; 2],
    wires: AtomicU32,
    /// Per device, the output of INTA..INTD.
    route: Box<[[Option<u8>; 4]]>,
    wire_out: &'static [u8],
    out: Box<[Out]>,
    /// Lets a test act at the two points a concurrent change can land.
    #[cfg(test)]
    hook: std::sync::Mutex<Option<Arc<dyn Fn(Point) + Send + Sync>>>,
}

impl Combiner {
    pub(super) fn new(w: &'static PciWiring, up: Weak<dyn PciUpstream>) -> Arc<Self> {
        assert!(w.devices <= 32 && w.wires.len() <= 32, "pci: wiring has too many devices or wires");
        let mut route = vec![[None; 4]; w.devices as usize];
        let mut out: Vec<Out> = (0..w.outputs)
            .map(|_| Out {
                pins: [0; 2],
                wires: 0,
                dirty: AtomicBool::new(false),
                delivering: AtomicBool::new(false),
                fell: AtomicBool::new(false),
                force: AtomicBool::new(false),
                last: AtomicBool::new(false),
            })
            .collect();
        let target = |o: u8| {
            assert!(o < w.outputs, "pci: wiring routes to output {o} of {}", w.outputs);
            o as usize
        };
        for &(dev, outs) in w.pins {
            assert!(dev < w.devices, "pci: wiring names device {dev} of {}", w.devices);
            route[dev as usize] = outs;
            for (p, o) in outs.iter().enumerate() {
                if let Some(o) = *o {
                    let line = dev as usize * 4 + p;
                    out[target(o)].pins[line / 64] |= 1 << (line % 64);
                }
            }
        }
        for (i, &o) in w.wires.iter().enumerate() {
            out[target(o)].wires |= 1 << i;
        }
        Arc::new(Self {
            up,
            pins: [AtomicU64::new(0), AtomicU64::new(0)],
            wires: AtomicU32::new(0),
            route: route.into(),
            wire_out: w.wires,
            out: out.into(),
            #[cfg(test)]
            hook: Default::default(),
        })
    }

    /// The line of device `dev`'s pin (1 = INTA), or None if the wiring
    /// does not route it.
    pub(super) fn pin_line(self: &Arc<Self>, dev: u8, pin: u8) -> Option<IrqLine> {
        let routed = self.route.get(dev as usize)?.get(pin as usize - 1)?.is_some();
        routed.then(|| IrqLine::new(self.clone(), dev as u32 * 4 + pin as u32 - 1))
    }

    pub(super) fn wire_line(self: &Arc<Self>, w: Wire) -> IrqLine {
        assert!((w.0 as usize) < self.wire_out.len(), "pci: no wire {}", w.0);
        IrqLine::new(self.clone(), PIN_LINES + w.0 as u32)
    }

    /// The OR of everything routed to output `o`.
    fn level(&self, o: usize) -> bool {
        let m = &self.out[o];
        self.pins[0].load(SeqCst) & m.pins[0] != 0
            || self.pins[1].load(SeqCst) & m.pins[1] != 0
            || self.wires.load(SeqCst) & m.wires != 0
    }

    fn set_pin(&self, line: u32, level: bool) {
        let (dev, pin) = ((line / 4) as usize, (line % 4) as usize);
        let Some(&Some(o)) = self.route.get(dev).map(|r| &r[pin]) else { return };
        let (w, bit) = ((line / 64) as usize, 1u64 << (line % 64));
        if level {
            self.pins[w].fetch_or(bit, SeqCst);
        } else {
            let old = self.pins[w].fetch_and(!bit, SeqCst);
            self.at(Point::Lowered);
            // The output just after this lower, from `old` and not from a
            // later read, so a rise that follows cannot hide the fall.
            if old & bit != 0 {
                let m = &self.out[o as usize];
                let after = (old & !bit & m.pins[w]) != 0
                    || self.pins[1 - w].load(SeqCst) & m.pins[1 - w] != 0
                    || self.wires.load(SeqCst) & m.wires != 0;
                if !after {
                    self.out[o as usize].fell.store(true, SeqCst);
                }
            }
        }
        self.kick(o as usize);
    }

    fn set_wire(&self, n: u32, level: bool) {
        let Some(&o) = self.wire_out.get(n as usize) else { return };
        let bit = 1u32 << n;
        if level {
            self.wires.fetch_or(bit, SeqCst);
        } else {
            let old = self.wires.fetch_and(!bit, SeqCst);
            self.at(Point::Lowered);
            if old & bit != 0 {
                let m = &self.out[o as usize];
                let after = (old & !bit & m.wires) != 0
                    || self.pins[0].load(SeqCst) & m.pins[0] != 0
                    || self.pins[1].load(SeqCst) & m.pins[1] != 0;
                if !after {
                    self.out[o as usize].fell.store(true, SeqCst);
                }
            }
        }
        self.kick(o as usize);
    }

    #[cfg(test)]
    fn at(&self, p: Point) {
        // Not under the lock: a hook may change levels, which comes back here.
        let h = self.hook.lock().unwrap().clone();
        if let Some(h) = h {
            h(p);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn at(&self, _: Point) {}

    /// Deliver output `o` if nobody else is; whoever is will see `dirty`.
    fn kick(&self, o: usize) {
        let m = &self.out[o];
        m.dirty.store(true, SeqCst);
        loop {
            if m.delivering.swap(true, SeqCst) {
                return;
            }
            while m.dirty.swap(false, SeqCst) {
                let fell = m.fell.swap(false, SeqCst);
                let force = m.force.swap(false, SeqCst);
                let v = self.level(o);
                let mut last = m.last.load(Relaxed);
                if let Some(up) = self.up.upgrade() {
                    if fell && v && last {
                        up.set_output(o as u8, false);
                        last = false;
                    }
                    if force || v != last {
                        up.set_output(o as u8, v);
                    }
                }
                m.last.store(v, Relaxed);
            }
            m.delivering.store(false, SeqCst);
            self.at(Point::Released);
            // A change raced the release: go again.
            if !m.dirty.load(SeqCst) {
                return;
            }
        }
    }

    /// Send every output's current level to the host, whether or not it
    /// already has it.
    pub(super) fn redrive(&self) {
        for (o, m) in self.out.iter().enumerate() {
            m.force.store(true, SeqCst);
            self.kick(o);
        }
    }

    /// The pin words and the wire word, for a snapshot.
    pub(super) fn save(&self) -> (u64, u64, u32) {
        (self.pins[0].load(SeqCst), self.pins[1].load(SeqCst), self.wires.load(SeqCst))
    }

    /// Replace the levels outright and forget delivery state: what the host
    /// has is whatever the restore gave it, and `redrive` makes it agree.
    pub(super) fn load(&self, w: (u64, u64, u32)) {
        self.pins[0].store(w.0, SeqCst);
        self.pins[1].store(w.1, SeqCst);
        self.wires.store(w.2, SeqCst);
        for m in self.out.iter() {
            m.dirty.store(false, SeqCst);
            m.fell.store(false, SeqCst);
            m.force.store(false, SeqCst);
            m.last.store(false, Relaxed);
        }
    }
}

impl InterruptController for Combiner {
    fn set_line(&self, line: u32, level: bool) {
        if line < PIN_LINES {
            self.set_pin(line, level);
        } else {
            self.set_wire(line - PIN_LINES, level);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Mutex;

    struct Nothing;
    impl PciDma for Nothing {
        fn dma_read(&self, _: u64, _: &mut [u8]) -> crate::pci::PciDmaStatus { crate::pci::PciDmaStatus::Ok }
        fn dma_write(&self, _: u64, _: &[u8]) -> crate::pci::PciDmaStatus { crate::pci::PciDmaStatus::Ok }
    }

    /// Records each delivery. With `stall` armed the next delivery reports
    /// that it has started and then waits to be released, so a test can
    /// change levels while a delivery is in flight.
    struct Host {
        log: Mutex<Vec<(u8, bool)>>,
        in_flight: Mutex<u32>,
        overlapped: AtomicBool,
        stall: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    }

    impl Host {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                log: Mutex::new(Vec::new()),
                in_flight: Mutex::new(0),
                overlapped: AtomicBool::new(false),
                stall: Mutex::new(None),
            })
        }

        /// Arm one stall; returns the signal that it has begun and the release.
        fn arm(&self) -> (Receiver<()>, Sender<()>) {
            let (begun_tx, begun_rx) = channel();
            let (go_tx, go_rx) = channel();
            *self.stall.lock().unwrap() = Some((begun_tx, go_rx));
            (begun_rx, go_tx)
        }

        fn log(&self) -> Vec<(u8, bool)> {
            self.log.lock().unwrap().clone()
        }
    }

    impl PciUpstream for Host {
        fn dma_for(&self, _: u8) -> Arc<dyn PciDma> {
            Arc::new(Nothing)
        }
        fn set_output(&self, output: u8, level: bool) {
            {
                let mut n = self.in_flight.lock().unwrap();
                *n += 1;
                if *n > 1 {
                    self.overlapped.store(true, SeqCst);
                }
            }
            self.log.lock().unwrap().push((output, level));
            let stall = self.stall.lock().unwrap().take();
            if let Some((begun, go)) = stall {
                begun.send(()).unwrap();
                go.recv().unwrap();
            }
            *self.in_flight.lock().unwrap() -= 1;
        }
    }

    // Device 1 INTA and device 2 INTA share output 0; device 1 INTB is
    // output 1; device 3 has INTA..INTD on 2..5; wire 0 is output 0 and
    // wire 1 output 6 and wire 2 output 0.
    static W: PciWiring = PciWiring {
        devices: 8,
        outputs: 7,
        pins: &[
            (1, [Some(0), Some(1), None, None]),
            (2, [Some(0), None, None, None]),
            (3, [Some(2), Some(3), Some(4), Some(5)]),
        ],
        wires: &[0, 6, 0],
    };

    fn rig() -> (Arc<Host>, Arc<Combiner>) {
        let h = Host::new();
        let up: Weak<dyn PciUpstream> = Arc::downgrade(&(h.clone() as Arc<dyn PciUpstream>));
        (h, Combiner::new(&W, up))
    }

    #[test]
    fn every_device_pin_reaches_its_output() {
        let (h, c) = rig();
        for (dev, pin, out) in [(1, 1, 0), (1, 2, 1), (2, 1, 0), (3, 1, 2), (3, 2, 3), (3, 3, 4), (3, 4, 5)] {
            let l = c.pin_line(dev, pin).unwrap();
            l.set(true);
            assert_eq!(h.log().last(), Some(&(out, true)), "device {dev} pin {pin}");
            l.set(false);
            assert_eq!(h.log().last(), Some(&(out, false)), "device {dev} pin {pin}");
        }
    }

    #[test]
    fn unrouted_pin_line_is_none() {
        let (_, c) = rig();
        assert!(c.pin_line(1, 3).is_none(), "INTC of device 1");
        assert!(c.pin_line(4, 1).is_none(), "device 4 has nothing");
        assert!(c.pin_line(9, 1).is_none(), "past the last device");
    }

    #[test]
    fn shared_pins_or_and_edges_only_are_delivered() {
        let (h, c) = rig();
        let (a, b) = (c.pin_line(1, 1).unwrap(), c.pin_line(2, 1).unwrap());
        a.set(true);
        b.set(true);
        a.set(false);
        assert_eq!(h.log(), vec![(0, true)], "b still holds it, and a second rise is not an edge");
        b.set(false);
        assert_eq!(h.log(), vec![(0, true), (0, false)]);
        b.set(false);
        assert_eq!(h.log().len(), 2, "lowering a low pin delivers nothing");
    }

    #[test]
    fn a_wire_reaches_its_output_and_ors_with_pins() {
        let (h, c) = rig();
        let w = c.wire_line(Wire(0));
        let p = c.pin_line(1, 1).unwrap();
        w.set(true);
        p.set(true);
        w.set(false);
        assert_eq!(h.log(), vec![(0, true)], "the pin still holds output 0");
        p.set(false);
        assert_eq!(h.log(), vec![(0, true), (0, false)]);
        c.wire_line(Wire(1)).set(true);
        assert_eq!(h.log().last(), Some(&(6, true)));
    }

    /// Run `op` on a thread whose delivery stalls inside the host, change
    /// levels from the test thread meanwhile, then let the delivery go.
    fn stalled(h: &Arc<Host>, op: impl FnOnce() + Send + 'static, during: impl FnOnce()) {
        let (begun, go) = h.arm();
        let t = std::thread::spawn(op);
        begun.recv().unwrap();
        during();
        go.send(()).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn a_pin_fall_then_rise_before_delivery_gives_zero_then_one() {
        let (h, c) = rig();
        let p = c.pin_line(1, 1).unwrap();
        let raise = p.clone();
        stalled(&h, move || raise.set(true), || {
            p.set(false);
            p.set(true);
        });
        assert_eq!(h.log(), vec![(0, true), (0, false), (0, true)]);
    }

    #[test]
    fn a_shared_output_with_the_other_pin_high_gets_no_zero_then_one() {
        let (h, c) = rig();
        let (p, other) = (c.pin_line(1, 1).unwrap(), c.pin_line(2, 1).unwrap());
        let raise = p.clone();
        stalled(&h, move || raise.set(true), || {
            other.set(true);
            p.set(false);
            p.set(true);
        });
        assert_eq!(h.log(), vec![(0, true)], "the output never fell");
    }

    #[test]
    fn a_raise_lowered_before_delivery_is_coalesced() {
        let (h, c) = rig();
        let (p, q) = (c.pin_line(1, 1).unwrap(), c.pin_line(2, 1).unwrap());
        p.set(true);
        let lower = p.clone();
        stalled(&h, move || lower.set(false), || {
            q.set(true);
            q.set(false);
        });
        assert_eq!(h.log(), vec![(0, true), (0, false)], "the host never sees q's pulse");
    }

    #[test]
    fn deliveries_are_ordered_and_never_concurrent() {
        let (h, c) = rig();
        let pins: Vec<_> = [(1, 1), (2, 1)].iter().map(|&(d, p)| c.pin_line(d, p).unwrap()).collect();
        let ts: Vec<_> = pins
            .into_iter()
            .map(|l| {
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        l.set(true);
                        l.set(false);
                    }
                })
            })
            .collect();
        for t in ts {
            t.join().unwrap();
        }
        assert!(!h.overlapped.load(SeqCst));
        let log = h.log();
        assert_eq!(log.last().map(|e| e.1), Some(false), "ends low");
        assert!(log.windows(2).all(|w| w[0].1 != w[1].1), "levels alternate");
    }

    #[test]
    fn redrive_resends_a_high_output_equal_to_last() {
        let (h, c) = rig();
        c.pin_line(1, 1).unwrap().set(true);
        c.redrive();
        assert_eq!(&h.log()[..2], &[(0, true), (0, true)]);
        assert_eq!(h.log().iter().filter(|e| e.0 == 0).count(), 2);
        assert_eq!(h.log().len(), 2 + 6, "and every other output, low");
    }

    #[test]
    fn a_wire_fall_keeps_fell() {
        let (h, c) = rig();
        let w = c.wire_line(Wire(0));
        let raise = w.clone();
        stalled(&h, move || raise.set(true), || {
            w.set(false);
            w.set(true);
        });
        assert_eq!(h.log(), vec![(0, true), (0, false), (0, true)]);
    }

    #[test]
    fn load_resets_dirty_fell_and_last() {
        let (h, c) = rig();
        let p = c.pin_line(1, 1).unwrap();
        p.set(true);
        let saved = c.save();
        // A fall that nobody delivered: `fell` is latched and `dirty` set.
        c.out[0].fell.store(true, SeqCst);
        c.out[0].dirty.store(true, SeqCst);
        p.set(false);
        c.load(saved);
        assert_eq!(c.save(), saved);
        assert!(!c.out[0].fell.load(SeqCst) && !c.out[0].dirty.load(SeqCst) && !c.out[0].last.load(SeqCst));
        // After the load the host is told the saved level whatever it held.
        let n = h.log().len();
        c.redrive();
        assert_eq!(h.log()[n], (0, true));
    }

    /// A mutator that has stored `dirty` and found `delivering` held
    /// returns; the holder must look again after releasing. The hook plays
    /// that mutator in the window after the release.
    #[test]
    fn a_hook_lower_between_release_and_recheck_is_still_delivered() {
        let (h, c) = rig();
        let p = c.pin_line(1, 1).unwrap();
        let fired = Arc::new(AtomicBool::new(false));
        {
            let (fired, c2) = (fired.clone(), c.clone());
            *c.hook.lock().unwrap() = Some(Arc::new(move |at| {
                if at == Point::Released && !fired.swap(true, SeqCst) {
                    c2.pins[0].fetch_and(!(1 << 4), SeqCst);
                    c2.out[0].dirty.store(true, SeqCst);
                }
            }));
        }
        p.set(true);
        assert!(fired.load(SeqCst));
        assert_eq!(h.log(), vec![(0, true), (0, false)]);
    }

    /// Another source rises between a lower's `fetch_and` and its work on
    /// the output's level. The lower took the output to 0 (and the rise
    /// took it back), so the host must see 0 then 1; a level read after the
    /// lower would find the output high and see no fall. (A rise in the
    /// other word, or from the other kind of source, is read as it is when
    /// the lower works out the level; that is the limit of doing this
    /// without a lock.)
    #[test]
    fn a_rise_between_the_lower_and_its_level_still_latches_the_fall() {
        for wire in [false, true] {
            let (h, c) = rig();
            let (x, y) = if wire {
                (c.wire_line(Wire(0)), c.wire_line(Wire(2)))
            } else {
                (c.pin_line(1, 1).unwrap(), c.pin_line(2, 1).unwrap())
            };
            x.set(true);
            let fired = Arc::new(AtomicBool::new(false));
            let fired2 = fired.clone();
            *c.hook.lock().unwrap() = Some(Arc::new(move |at| {
                if at == Point::Lowered && !fired2.swap(true, SeqCst) {
                    y.set(true);
                }
            }));
            x.set(false);
            assert_eq!(h.log(), vec![(0, true), (0, false), (0, true)], "wire: {wire}");
        }
    }
}
