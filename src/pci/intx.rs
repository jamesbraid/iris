//! Interrupt pins and board IRQs, combined per host output.
//!
//! Every pin of every device number is a line (`device * 4 + pin - 1`) and
//! the board IRQs follow them. The combiner keeps each line's level and
//! tells the host the OR of everything routed to an output, so on a shared
//! output one source cannot clear another's interrupt.
//!
//! A change sets its line and, when that changes its output's OR, drives the
//! host, all under the combiner's lock, as MACE's ISA block, the IOC and CRIME
//! drive the controller above them. So deliveries to the host are one at a
//! time and in the order the levels changed: the host's last level is the OR
//! after the last change, and a pin that falls and rises reaches it as a 0 and
//! then a 1, which an edge host needs (the Bridge sends HEART a packet only on
//! a rising edge). Interrupts only travel up: no host calls into the bus from
//! `set_irq`, or changes a line while holding a lock its `set_irq` takes, so
//! holding this lock across the call cannot deadlock.

use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use crate::irq::{InterruptController, IrqLine};
use crate::pci::PciHost;

/// A board interrupt that is not a PCI pin but shares a host input, for
/// example one chip's interrupt on a Bridge pin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BoardIrq(pub u8);

/// One root bus's interrupt routing as plain data. Labels and sources go
/// in comments beside the static.
pub struct IntxRouting {
    /// Device numbers on the bus (at most 32).
    pub devices: u8,
    /// Host IRQs (CRIME inputs on the O2, Bridge pins on the Octane).
    pub irqs: u8,
    /// Device to host IRQ for INTA..INTD; a pin not listed is not routed.
    pub intx: &'static [(u8, [Option<u8>; 4])],
    /// Host IRQ of `BoardIrq(i)`.
    pub board_irqs: &'static [u8],
}

/// Lines `0..PIN_LINES` are pins; board IRQ `n` is line `PIN_LINES + n`. Line
/// `l` is bit `l % 64` of word `l / 64`, so the board IRQs are the third word.
const PIN_LINES: u32 = 32 * 4;

#[derive(Default)]
struct Levels {
    /// Every line's level: pins in words 0 and 1, board IRQs in word 2.
    lines: [u64; 3],
    /// The level the host has, one bit per output.
    driven: u64,
}

pub(super) struct Combiner {
    up: Weak<dyn PciHost>,
    /// Per device, the output of INTA..INTD.
    route: Box<[[Option<u8>; 4]]>,
    board_irq_out: &'static [u8],
    /// Per output, the lines routed to it, by word.
    mask: Box<[[u64; 3]]>,
    levels: Mutex<Levels>,
}

impl Combiner {
    pub(super) fn new(w: &'static IntxRouting, up: Weak<dyn PciHost>) -> Arc<Self> {
        assert!(w.devices <= 32 && w.board_irqs.len() <= 32, "pci: routing has too many devices or board IRQs");
        assert!(w.irqs <= 64, "pci: routing has more than 64 outputs");
        let mut route = vec![[None; 4]; w.devices as usize];
        let mut mask = vec![[0u64; 3]; w.irqs as usize];
        let target = |o: u8| {
            assert!(o < w.irqs, "pci: routing names output {o} of {}", w.irqs);
            o as usize
        };
        for &(dev, outs) in w.intx {
            assert!(dev < w.devices, "pci: wiring names device {dev} of {}", w.devices);
            route[dev as usize] = outs;
            for (p, o) in outs.iter().enumerate() {
                if let Some(o) = *o {
                    let line = dev as usize * 4 + p;
                    mask[target(o)][line / 64] |= 1 << (line % 64);
                }
            }
        }
        for (i, &o) in w.board_irqs.iter().enumerate() {
            mask[target(o)][2] |= 1 << i;
        }
        Arc::new(Self { up, route: route.into(), board_irq_out: w.board_irqs, mask: mask.into(), levels: Default::default() })
    }

    /// The line of device `dev`'s pin (1 = INTA), or None if the wiring
    /// does not route it.
    pub(super) fn pin_line(self: &Arc<Self>, dev: u8, pin: u8) -> Option<IrqLine> {
        let routed = self.route.get(dev as usize)?.get(pin as usize - 1)?.is_some();
        routed.then(|| IrqLine::new(self.clone(), dev as u32 * 4 + pin as u32 - 1))
    }

    /// The output device `dev`'s pin (1 = INTA) is routed to.
    pub(super) fn output(&self, dev: u8, pin: u8) -> Option<u8> {
        *self.route.get(dev as usize)?.get(pin as usize - 1)?
    }

    pub(super) fn board_irq_line(self: &Arc<Self>, w: BoardIrq) -> IrqLine {
        assert!((w.0 as usize) < self.board_irq_out.len(), "pci: no board IRQ {}", w.0);
        IrqLine::new(self.clone(), PIN_LINES + w.0 as u32)
    }

    /// The output line `line` is routed to, if any.
    fn output_of(&self, line: u32) -> Option<usize> {
        let o = if line < PIN_LINES {
            (*self.route.get((line / 4) as usize)?.get((line % 4) as usize)?)?
        } else {
            *self.board_irq_out.get((line - PIN_LINES) as usize)?
        };
        Some(o as usize)
    }

    /// Tell the host output `o`'s level if it has the other one, or always
    /// with `force`.
    fn drive(&self, l: &mut Levels, o: usize, force: bool) {
        let level = (0..3).any(|i| l.lines[i] & self.mask[o][i] != 0);
        if !force && level == (l.driven >> o & 1 != 0) {
            return;
        }
        l.driven = l.driven & !(1 << o) | (level as u64) << o;
        if let Some(up) = self.up.upgrade() {
            up.set_irq(o as u8, level);
        }
    }

    /// Send every output's current level to the host, whether or not it
    /// already has it.
    pub(super) fn redrive(&self) {
        let mut l = self.levels.lock();
        for o in 0..self.mask.len() {
            self.drive(&mut l, o, true);
        }
    }

    /// Which outputs the host has high, one bit each, for a test to compare.
    #[cfg(test)]
    pub(super) fn driven(&self) -> u64 {
        self.levels.lock().driven
    }

    /// Take every output as high at the host, as a used bus might.
    #[cfg(test)]
    pub(super) fn drive_all_high(&self) {
        self.levels.lock().driven = u64::MAX;
    }

    /// The pin words and the wire word, for a snapshot.
    pub(super) fn save(&self) -> (u64, u64, u32) {
        let l = self.levels.lock();
        (l.lines[0], l.lines[1], l.lines[2] as u32)
    }

    /// Replace the levels outright and forget what the host has: that is
    /// whatever the restore gave it, and `redrive` makes it agree.
    pub(super) fn load(&self, w: (u64, u64, u32)) {
        *self.levels.lock() = Levels { lines: [w.0, w.1, w.2 as u64], driven: 0 };
    }
}

impl InterruptController for Combiner {
    fn set_line(&self, line: u32, level: bool) {
        let Some(o) = self.output_of(line) else { return };
        let (w, bit) = ((line / 64) as usize, 1u64 << (line % 64));
        let mut l = self.levels.lock();
        if level {
            l.lines[w] |= bit;
        } else {
            l.lines[w] &= !bit;
        }
        self.drive(&mut l, o, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::PciDma;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
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

    impl PciHost for Host {
        fn dma_for(&self, _: u8) -> Arc<dyn PciDma> {
            Arc::new(Nothing)
        }
        fn set_irq(&self, output: u8, level: bool) {
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
    static W: IntxRouting = IntxRouting {
        devices: 8,
        irqs: 7,
        intx: &[
            (1, [Some(0), Some(1), None, None]),
            (2, [Some(0), None, None, None]),
            (3, [Some(2), Some(3), Some(4), Some(5)]),
        ],
        board_irqs: &[0, 6, 0],
    };

    fn rig() -> (Arc<Host>, Arc<Combiner>) {
        let h = Host::new();
        let up: Weak<dyn PciHost> = Arc::downgrade(&(h.clone() as Arc<dyn PciHost>));
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
    fn a_board_irq_reaches_its_output_and_ors_with_pins() {
        let (h, c) = rig();
        let w = c.board_irq_line(BoardIrq(0));
        let p = c.pin_line(1, 1).unwrap();
        w.set(true);
        p.set(true);
        w.set(false);
        assert_eq!(h.log(), vec![(0, true)], "the pin still holds output 0");
        p.set(false);
        assert_eq!(h.log(), vec![(0, true), (0, false)]);
        c.board_irq_line(BoardIrq(1)).set(true);
        assert_eq!(h.log().last(), Some(&(6, true)));
    }

    /// Run `op` on a thread whose delivery stalls inside the host, start
    /// `during` on another meanwhile (its changes wait for the delivery),
    /// then let the delivery go.
    fn stalled(h: &Arc<Host>, op: impl FnOnce() + Send + 'static, during: impl FnOnce() + Send + 'static) {
        let (begun, go) = h.arm();
        let t = std::thread::spawn(op);
        begun.recv().unwrap();
        let d = std::thread::spawn(during);
        go.send(()).unwrap();
        t.join().unwrap();
        d.join().unwrap();
    }

    #[test]
    fn a_pin_fall_then_rise_during_a_delivery_gives_zero_then_one() {
        let (h, c) = rig();
        let p = c.pin_line(1, 1).unwrap();
        let raise = p.clone();
        stalled(&h, move || raise.set(true), move || {
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
        stalled(&h, move || raise.set(true), move || {
            other.set(true);
            p.set(false);
            p.set(true);
        });
        assert_eq!(h.log(), vec![(0, true)], "the output never fell");
    }

    #[test]
    fn a_pulse_during_a_delivery_reaches_the_host() {
        let (h, c) = rig();
        let (p, q) = (c.pin_line(1, 1).unwrap(), c.pin_line(2, 1).unwrap());
        p.set(true);
        let lower = p.clone();
        stalled(&h, move || lower.set(false), move || {
            q.set(true);
            q.set(false);
        });
        assert_eq!(h.log(), vec![(0, true), (0, false), (0, true), (0, false)]);
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
    fn a_board_irq_fall_then_rise_during_a_delivery_gives_zero_then_one() {
        let (h, c) = rig();
        let w = c.board_irq_line(BoardIrq(0));
        let raise = w.clone();
        stalled(&h, move || raise.set(true), move || {
            w.set(false);
            w.set(true);
        });
        assert_eq!(h.log(), vec![(0, true), (0, false), (0, true)]);
    }

    #[test]
    fn load_replaces_the_levels_and_forgets_what_the_host_has() {
        let (h, c) = rig();
        let p = c.pin_line(1, 1).unwrap();
        p.set(true);
        let saved = c.save();
        p.set(false);
        c.board_irq_line(BoardIrq(1)).set(true);
        c.load(saved);
        assert_eq!(c.save(), saved);
        assert_eq!(c.driven(), 0, "nothing is taken as delivered");
        // After the load the host is told the saved level whatever it held.
        let n = h.log().len();
        c.redrive();
        assert_eq!(h.log()[n], (0, true));
    }
}
