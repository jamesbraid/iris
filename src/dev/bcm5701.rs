//! The Broadcom BCM5701 gigabit Ethernet controller (PCI 14e4:1645), as the
//! single-port SGI board carries it: a descriptor-ring MAC with an integrated
//! copper PHY, which the public Linux tg3 and OpenBSD bge drivers and the
//! IRIX tg driver all drive.
//!
//! BAR0 is 64 KiB: a register file at 0x0000-0x7fff (its first 256 bytes also
//! alias the PCI configuration registers) and a 32 KiB window onto the 32 KiB
//! on-chip SRAM at 0x8000-0xffff, whose base the memory-window base register
//! selects. The driver reaches chip state three ways: the BAR directly, an
//! indirect register window through configuration registers 0x78 and 0x80,
//! and an indirect memory window through 0x7c and 0x84. All three land on the
//! same state. The configuration side is the function's own `config_dword` and
//! `config_write`, so it works behind any bridge.
//!
//! Modelled: the PCI identity; the chip id in MISC_CTL; the global reset with
//! the on-chip firmware-ready handshake in SRAM and the firmware-progress
//! register; the self-clearing mode-register resets; the MII path to the
//! PHY ([`phy`]); the station address the bootcode leaves in SRAM and in
//! address filter slot 0; the interrupt mailbox, the MISC_CTL global mask and
//! PCISTATE.INTR_NOT_ACTIVE; the buffer manager's delayed enable; and the
//! serial EEPROM controller over a caller-supplied backing ([`eeprom`]); the
//! send ring and its doorbell, the standard, jumbo and return receive rings,
//! the status block DMA, host coalescing, the interrupt it drives, the
//! transmit checksum offload and segmentation, and the receive checksum
//! report ([`dma`]).
//!
//! # What is approximated
//!
//! - Iris has no virtual clock, so timing is wall-clock. Deadlines (the PHY's
//!   100 ms settle, the buffer manager's 8 ms startup, the coalescing budgets
//!   and the arrival latency) are evaluated when the device is touched and by
//!   the receive thread, each at its own expiry and earliest first, as the
//!   model's virtual clock would fire them.
//! - EEPROM transfers complete synchronously; the delay register only reads
//!   back, and the 10 ms write spacing is not enforced. Direct serial-pin
//!   control is refused and counted.
//! - The management interface ignores the PHY address field; one PHY answers.
//! - A write to a register with no modelled behaviour is stored and reads
//!   back, and is recorded in [`Bcm5701::refused`].
//! - The BCM5704 dual-port variants are not modelled.
//! - Frames reach the host through the shared network link; the chip's
//!   virtio-net offload path has no counterpart, so a large TCP buffer is
//!   always cut into segments in software.
//! - A transmit is carried out entirely inside the doorbell write, so there
//!   is no re-entrancy guard and no deferred flush: the receive thread offers
//!   a waiting frame again whenever the guest makes room.
//!
//! Ported from hw/net/bcm570x.c and hw/net/bcm570x_phy.c in
//! jamesbraid/qemu-sgi-origami.

mod dma;
pub mod eeprom;
pub mod phy;
mod state;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::irq::IrqLine;
use crate::pci::{Bar, PciConfig, PciConfigPolicy, PciFunction, PciFunctionState, PciRegisterPolicy, PCI_INTA};
use crate::traits::{Device, Resettable};
use eeprom::Eeprom;
pub use dma::{DmaWhat, Mailbox, Ring, RxOutcome, MAX_FRAME, RX_ARRIVAL_LATENCY_DEFAULT};
pub use state::Refused;
use state::{cfg, misc_ctl, pcistate, Header, State};

pub const VENDOR_ID: u16 = 0x14e4;
pub const DEVICE_ID: u16 = 0x1645;
/// The SGI board's subsystem identity.
pub const SUBSYSTEM_VENDOR_ID: u16 = 0x10a9;
pub const SUBSYSTEM_ID: u16 = 0x8010;
/// PCI network controller, Ethernet.
const CLASS_ETHERNET: u32 = 0x02_0000;

pub const BAR0_SIZE: u32 = 0x1_0000;
/// The low 256 bytes of BAR0 are the PCI configuration registers again.
const CFG_ALIAS: u32 = 0x100;

/// What a board gives the chip: its station address and, optionally, the
/// serial EEPROM the board fits.
pub struct Bcm5701Config {
    pub mac: [u8; 6],
    pub eeprom: Option<Eeprom>,
    /// The minimum between a frame arriving and the driver being told, so a
    /// completion never beats the arrival it reports. Zero publishes at once.
    pub rx_arrival_latency: Duration,
    /// Whether the slot drives REQ64# as reset ends. The chip samples it to
    /// report a 64-bit bus in PCISTATE.
    pub req64: bool,
}

impl Bcm5701Config {
    pub fn new(mac: [u8; 6]) -> Self {
        Self { mac, eeprom: None, rx_arrival_latency: RX_ARRIVAL_LATENCY_DEFAULT, req64: false }
    }
}

pub struct Bcm5701 {
    pci: PciFunctionState,
    st: Mutex<State>,
    irq: Mutex<IrqLine>,
    /// Serialises delivery of level changes to the interrupt line.
    irq_order: Mutex<()>,
    link: Mutex<Option<crate::net::Rings>>,
    rx_wake: Mutex<Option<Arc<crate::net::Wake>>>,
    /// Between `start` and `stop`; the NAT engine watches it too.
    running: Arc<AtomicBool>,
    /// The flag the network backend watches, set and cleared with `running`.
    net_running: Mutex<Option<Arc<AtomicBool>>>,
    /// The network backend, between runs. A NAT engine keeps its flows across
    /// a stop.
    backend: Arc<Mutex<Option<Box<dyn crate::net::NetBackend>>>>,
    /// The NAT and receive threads, joined in `stop`.
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    me: Weak<Self>,
    /// Tests freeze the clock to step it exactly.
    #[cfg(test)]
    frozen: Mutex<Option<Instant>>,
}

impl Bcm5701 {
    pub fn new(config: Bcm5701Config) -> Result<Arc<Self>, String> {
        if config.mac[0] & 1 != 0 {
            return Err("bcm5701: station address must be unicast".into());
        }
        let policy = PciConfigPolicy {
            registers: &[
                // The chip id in the high half is read-only; the driver's
                // byte-order and interrupt-mask bits in the low half stick.
                PciRegisterPolicy {
                    reg_dword: cfg::MISC_CTL,
                    initial: state::CHIPID_BCM5701 << 16,
                    writable_mask: misc_ctl::WRITABLE,
                },
                // The chip drives PCISTATE, which `config_dword` reports from
                // its state; the guest cannot write it. It runs as
                // conventional PCI and starts with the interrupt line down.
                PciRegisterPolicy {
                    reg_dword: cfg::PCISTATE,
                    initial: State::pcistate_at_reset(config.req64),
                    writable_mask: 0,
                },
                // The indirect windows live in the chip, reached by whole
                // dwords through `config_dword` and `config_write`. The header
                // bytes behind them are what a narrower write reaches, and
                // nothing is stored there.
                PciRegisterPolicy { reg_dword: cfg::REG_BASEADDR, initial: 0, writable_mask: 0 },
                PciRegisterPolicy { reg_dword: cfg::MEMWIN_BASEADDR, initial: 0, writable_mask: 0 },
                PciRegisterPolicy { reg_dword: cfg::REG_DATA, initial: 0, writable_mask: 0 },
                PciRegisterPolicy { reg_dword: cfg::MEMWIN_DATA, initial: 0, writable_mask: 0 },
            ],
            ..Default::default()
        };
        let pci = PciConfig::new_with_policy(
            VENDOR_ID,
            DEVICE_ID,
            CLASS_ETHERNET,
            0,
            [Bar::Mem(BAR0_SIZE), Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused],
            PCI_INTA,
            policy,
        );
        pci.write(11, u32::from(SUBSYSTEM_ID) << 16 | u32::from(SUBSYSTEM_VENDOR_ID));
        let now = Instant::now();
        let dev = Arc::new_cyclic(|me| Self {
            st: Mutex::new(State::new(Header::new(me.clone()), config.req64, config.mac, config.eeprom, config.rx_arrival_latency, now)),
            pci: PciFunctionState::new(pci),
            irq: Mutex::new(IrqLine::unconnected()),
            irq_order: Mutex::new(()),
            link: Mutex::new(None),
            rx_wake: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            net_running: Mutex::new(None),
            backend: Arc::new(Mutex::new(None)),
            threads: Mutex::new(Vec::new()),
            me: me.clone(),
            #[cfg(test)]
            frozen: Mutex::new(None),
        });
        Ok(dev)
    }

    /// The header the bus keeps for the function.
    fn config_header(&self) -> &PciConfig {
        self.pci.config()
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(t) = *self.frozen.lock() {
            return t;
        }
        Instant::now()
    }

    /// Run `f` with the state at `now`, then deliver what it produced: any
    /// interrupt level it changed, frames to send, and a wake-up for the
    /// receive thread if the guest made room.
    fn enter<R>(&self, now: Instant, f: impl FnOnce(&mut State) -> R) -> R {
        let (result, wire, wake, purge) = {
            let mut st = self.st.lock();
            st.enter(now);
            let result = f(&mut st);
            (
                result,
                std::mem::take(&mut st.wire),
                std::mem::take(&mut st.wake_rx),
                std::mem::take(&mut st.purge_rx),
            )
        };
        if purge {
            // A reset drops what the backend was holding for the guest, so a
            // frame from before it cannot reach the new rings.
            if let Some(link) = self.link.lock().as_mut() {
                while link.from_net.pop().is_ok() {}
            }
        }
        self.irq_flush();
        self.send(wire);
        if wake {
            self.wake_rx();
        }
        result
    }

    /// Deliver queued interrupt level changes, in order, with no state lock
    /// held that the line could need.
    fn irq_flush(&self) {
        let _order = self.irq_order.lock();
        let events = std::mem::take(&mut self.st.lock().irq_events);
        if events.is_empty() {
            return;
        }
        let line = self.irq.lock();
        for level in events {
            line.set(level);
        }
    }

    /// A machine reset: registers, SRAM, PHY and the PCI header as at
    /// power-on, the interrupt dropped. The EEPROM keeps its contents.
    pub fn reset(&self) {
        self.pci.config().reset();
        PciFunction::reset(self);
    }

    /// A whole-dword write to configuration dword `reg`, by either path to
    /// the header: the chip's own registers act, anything else is the
    /// header's.
    fn config_write_dword(&self, reg: usize, val: u32) {
        if self.config_write(reg, val) {
            return;
        }
        self.pci.config().write(reg, val);
        if reg == cfg::MISC_CTL {
            self.enter(self.now(), State::misc_ctl_written);
        }
    }

    /// The backend's cable state; the PHY reports it as link.
    pub fn set_backend_link(&self, up: bool) {
        self.enter(self.now(), |st| st.phy.set_link_down(!up));
    }

    fn send(&self, frames: Vec<Vec<u8>>) {
        if frames.is_empty() {
            return;
        }
        if let Some(link) = self.link.lock().as_mut() {
            for frame in frames {
                let _ = link.to_net.push(frame);
            }
            link.tx_wake.notify();
        }
    }

    fn wake_rx(&self) {
        if let Some(wake) = self.rx_wake.lock().as_ref() {
            wake.notify();
        }
    }

    /// Connect to the backend `[network]` selected: frames sent go on
    /// `link.to_net`, frames on `link.from_net` are received, and
    /// `link.rx_wake` is notified when the guest does something that can
    /// unblock a waiting frame. The backend and receive threads run between
    /// `start` and `stop`; a NAT engine, which only trades frames with the
    /// host and never reaches the guest, keeps its flows across a stop.
    pub fn attach(&self, mut link: crate::net::Link) {
        let backend = link.backend();
        let running = link.running();
        let rings = link.into_rings();
        *self.rx_wake.lock() = Some(rings.rx_wake.clone());
        *self.link.lock() = Some(rings);
        *self.net_running.lock() = Some(running.clone());
        *self.backend.lock() = backend;
        if self.running.load(Ordering::SeqCst) {
            running.store(true, Ordering::SeqCst);
            self.spawn_nat();
        }
    }

    fn spawn_nat(&self) {
        let slot = self.backend.clone();
        let Some(mut engine) = slot.lock().take() else { return };
        let t = std::thread::Builder::new()
            .name("bcm5701-net".into())
            .spawn(move || {
                engine.run();
                *slot.lock() = Some(engine);
            })
            .expect("bcm5701 NAT thread");
        self.threads.lock().push(t);
    }

    /// Take frames into guest memory as they arrive and as the guest makes
    /// room, and run the device's timers, until it stops. The wait ends early
    /// for a wake from the guest or the NAT engine.
    fn rx_loop(me: Weak<Self>, running: Arc<AtomicBool>, wake: Arc<crate::net::Wake>) {
        while running.load(Ordering::SeqCst) {
            let timeout = {
                let Some(this) = me.upgrade() else { return };
                let now = this.now();
                this.enter(now, |_| ());
                let next = this.st.lock().deadline();
                next.and_then(|d| d.checked_duration_since(now)).unwrap_or(RX_IDLE)
            };
            wake.wait(timeout);
            if !running.load(Ordering::SeqCst) {
                return;
            }
            let Some(this) = me.upgrade() else { return };
            this.poll();
        }
    }

    /// Take frames from the backend into the buffers the driver has posted.
    /// A frame the engine cannot take yet stays at the head of the backend's
    /// queue until the guest makes room; one the filter rejects is discarded.
    /// Returns whether any frame was consumed.
    ///
    /// The link is held only to take one frame at a time, so a transmit on the
    /// CPU thread never waits behind a burst of deliveries.
    pub fn poll(&self) -> bool {
        let mut moved = false;
        loop {
            let now = self.now();
            {
                let mut link = self.link.lock();
                let Some(link) = link.as_mut() else { break };
                let outcome = {
                    let Ok(frame) = link.from_net.peek() else { break };
                    let mut st = self.st.lock();
                    st.enter(now);
                    st.receive(frame)
                };
                if outcome == RxOutcome::Deferred {
                    break;
                }
                let _ = link.from_net.pop();
                moved = true;
            }
            self.irq_flush();
        }
        moved
    }

    /// Everything the model was asked for and could not do, with counts.
    pub fn refused(&self) -> Vec<(Refused, u64)> {
        self.st.lock().refused()
    }

    /// The serial EEPROM backing's current contents, if one is fitted.
    pub fn eeprom_bytes(&self) -> Option<Vec<u8>> {
        self.st.lock().eeprom_bytes()
    }

    /// Name the long-lived locks in the lock registry, which holds only a
    /// weak reference to the device.
    pub fn register_locks(self: &Arc<Self>) {
        for (what, probe) in [
            ("st", (|d: &Bcm5701| d.st.is_locked()) as fn(&Bcm5701) -> bool),
            ("link", |d| d.link.is_locked()),
            ("irq_order", |d| d.irq_order.is_locked()),
        ] {
            let me = Arc::downgrade(self);
            crate::locks::register_lock(format!("bcm5701::{what}"), move || me.upgrade().is_some_and(|d| probe(&d)));
        }
    }

    /// BAR0 accepts aligned 32-bit accesses everywhere, and byte reads of the
    /// first station-address pair.
    fn accepts(off: u32, width: usize, write: bool) -> bool {
        match width {
            4 => off & 3 == 0,
            1 => !write && (state::reg::MAC_ADDR_0_HIGH..state::reg::MAC_ADDR_0_LOW + 4).contains(&off),
            _ => false,
        }
    }
}

impl PciFunction for Bcm5701 {
    fn state(&self) -> &PciFunctionState {
        &self.pci
    }

    /// The registers the chip computes: the four indirect-window registers
    /// and PCISTATE. Everything else is the header's.
    fn config_dword(&self, reg: usize) -> u32 {
        if reg == cfg::PCISTATE {
            return self.st.lock().pcistate;
        }
        if !(cfg::REG_BASEADDR..=cfg::MEMWIN_DATA).contains(&reg) {
            return self.pci.config().read(reg);
        }
        self.enter(self.now(), |st| match reg {
            cfg::REG_BASEADDR => st.reg_win_base,
            cfg::MEMWIN_BASEADDR => st.mem_win_base,
            cfg::REG_DATA => st.reg_read(st.reg_win_base),
            _ => st.sram_read(st.mem_win_base),
        })
    }

    /// A whole-dword write to the indirect-window registers acts on the chip
    /// every time; a write to MISC_CTL is stored by the header and then acted
    /// on, whether or not its value changed.
    fn config_write(&self, reg: usize, val: u32) -> bool {
        if reg == cfg::MISC_CTL {
            self.pci.config().write(reg, val);
            self.enter(self.now(), State::misc_ctl_written);
            return true;
        }
        if !(cfg::REG_BASEADDR..=cfg::MEMWIN_DATA).contains(&reg) {
            return false;
        }
        self.enter(self.now(), |st| match reg {
            cfg::REG_BASEADDR => st.reg_win_base = val,
            cfg::MEMWIN_BASEADDR => st.mem_win_base = val,
            cfg::REG_DATA => st.reg_write(st.reg_win_base, val),
            _ => st.sram_write(st.mem_win_base, val),
        });
        true
    }

    /// A narrower write to MISC_CTL changed it: act on it as a whole-dword
    /// write does.
    fn config_written(&self, reg: usize, _old: u32, _new: u32) {
        if reg == cfg::MISC_CTL {
            self.enter(self.now(), State::misc_ctl_written);
        }
    }

    fn bar_read(&self, bar: usize, off: u32, width: usize) -> u32 {
        // A rejected read returns zero, as the model it is ported from does
        // behind its bridge; the refusal is counted.
        if bar != 0 {
            return 0;
        }
        let valid = Self::accepts(off, width, false) && off < BAR0_SIZE;
        // The configuration alias goes through the function's own dwords,
        // which take the state lock themselves.
        if valid && width == 4 && off < CFG_ALIAS {
            return self.config_dword((off / 4) as usize);
        }
        self.enter(self.now(), |st| {
            if !valid {
                st.refuse(Refused::BarAccess { offset: off, width: width as u8, write: false });
                0
            } else if width == 1 {
                (st.reg_read(off & !3) >> ((off & 3) * 8)) & 0xff
            } else if off < state::REG_FILE_SIZE {
                st.reg_read(off)
            } else {
                st.sram_read((st.mem_win_base & !(state::SRAM_SIZE - 1)) + (off - state::REG_FILE_SIZE))
            }
        })
    }

    fn bar_write(&self, bar: usize, off: u32, val: u32, width: usize) {
        if bar != 0 {
            return;
        }
        let valid = Self::accepts(off, width, true) && off < BAR0_SIZE;
        if valid && off < CFG_ALIAS {
            // Not the registers the bus built its BAR decode from: they change
            // only through its configuration cycles.
            let reg = (off / 4) as usize;
            if !matches!(reg, 1 | 4..=9) {
                self.config_write_dword(reg, val);
            }
            return;
        }
        self.enter(self.now(), |st| {
            if !valid {
                st.refuse(Refused::BarAccess { offset: off, width: width as u8, write: true });
            } else if off < state::REG_FILE_SIZE {
                st.reg_write(off, val);
            } else {
                st.sram_write((st.mem_win_base & !(state::SRAM_SIZE - 1)) + (off - state::REG_FILE_SIZE), val);
            }
        });
    }

    /// RST# has reset the header: the chip returns to power-on state and
    /// drops its interrupt. It samples REQ64# again as reset ends.
    fn reset(&self) {
        self.enter(self.now(), State::reset);
    }

    /// The chip's DMA, and its interrupt line, which carries the level the
    /// chip already has.
    fn attached(&self) {
        if let Some(dma) = self.pci.dma() {
            self.st.lock().dma = dma.clone();
            self.wake_rx();
        }
        if let Some(line) = self.pci.irq() {
            *self.irq.lock() = line.clone();
            // Present the current level to the new line.
            self.st.lock().update_irq(true);
            self.irq_flush();
        }
    }
}

/// How long the receive thread sleeps with no deadline to wait for: it is
/// woken by the guest and the NAT engine, so this is only a bound.
const RX_IDLE: Duration = Duration::from_secs(60);

impl Device for Bcm5701 {
    fn step(&self, _cycles: u64) {}

    /// Start the receive thread, and the NAT engine's if one is attached.
    fn start(&self) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let wake = self.rx_wake.lock().get_or_insert_with(crate::net::Wake::new).clone();
        let (me, running) = (self.me.clone(), self.running.clone());
        let rx = std::thread::Builder::new()
            .name("bcm5701-rx".into())
            .spawn(move || Self::rx_loop(me, running, wake))
            .expect("bcm5701 rx thread");
        self.threads.lock().push(rx);
        if let Some(r) = self.net_running.lock().as_ref() {
            r.store(true, Ordering::SeqCst);
        }
        self.spawn_nat();
        // Frames queued while stopped.
        self.wake_rx();
    }

    /// Join the threads: no frame reaches guest memory and no line moves
    /// afterwards. Frames the NAT engine takes meanwhile queue for `start`.
    fn stop(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(r) = self.net_running.lock().as_ref() {
            r.store(false, Ordering::SeqCst);
        }
        self.wake_rx();
        if let Some(l) = self.link.lock().as_ref() {
            l.tx_wake.notify();
        }
        // The receive thread can hold the last reference for a moment, and
        // then this runs on it; it cannot join itself.
        let me = std::thread::current().id();
        let threads: Vec<_> = self.threads.lock().drain(..).collect();
        for t in threads {
            if t.thread().id() != me {
                let _ = t.join();
            }
        }
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn get_clock(&self) -> u64 {
        0
    }
}

impl Resettable for Bcm5701 {
    fn power_on(&self) {
        self.reset();
    }
}

impl Drop for Bcm5701 {
    fn drop(&mut self) {
        Device::stop(&*self);
    }
}
