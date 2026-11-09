//! Fuel's XBridge revision 2 contains two independent PCI heads and their
//! shared crossbar. Each head has its own 1024-entry ATE, device windows and
//! interrupt state. Revision 2 places aliases differently from the Octane's
//! single-head Bridge, uses different PCI slot data-phase widths, and sends
//! host DMA through widget 0 with source-aware packets.
//!
//! This remains separate from `bridge.rs` because its two-head topology,
//! alias map and host packet path are externally visible parts of this chip.
//! Each head owns a shared `PciBus` for its functions, BAR decode and
//! interrupt pins, and supplies only what is its own: how a processor access
//! becomes configuration cycles and byte order, the ATE and DMA, and where an
//! interrupt pin goes. Whole-page BAR mappings of the device and alias windows
//! decode PCI PIO from the head's own table, without a `Physical` table; the
//! machine's RAM still uses one.
//!
//! # What is approximated here
//!
//! Slot widths, immediate link initialization, synchronous flushes and
//! revision-2 alias placement follow the project's BSD QEMU model, not a
//! silicon measurement. Timing, external SRAM, flash and diagnostic engines
//! are absent. Undefined accesses latch an error and remain observable.
//!
//! Ported from hw/pci-host/sgi-xbridge.c and hw/misc/sgi-xbridge-xbow.c
//! in jamesbraid/qemu-sgi-origami, revision
//! fe128635b6dfe2511f1bd451684cb30007686df1.

use crate::dev::w1;
use crate::pci::{BarDevice, BarMapping, ByteOrder, CpuWindow, DevFn, IntxRouting, PciBus, PciDma, PciDmaStatus, PciFunction, PciHost, Space};
use crate::physical::{read_bytes, write_bytes};
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, Resettable, BUS_OK};
use parking_lot::{Mutex, RwLock};
use std::sync::{Arc, Weak};

pub const HOST_WIDGET: u8 = 10;
pub const HEAD_E: u8 = 14;
pub const HEAD_F: u8 = 15;
pub const ATE_ENTRIES: usize = 1024;
pub const ATE_MAPPED: usize = 512;

/// Offsets of the PCI head register words, at eight-byte strides.
pub mod reg {
    pub const IDENTIFICATION: usize = 0x0;
    pub const STATUS: usize = 0x8;
    pub const ERR_UPPER: usize = 0x10;
    pub const ERR_LOWER: usize = 0x18;
    pub const CONTROL: usize = 0x20;
    pub const REQUEST_TIMEOUT: usize = 0x28;
    pub const INT_DEST_UPPER: usize = 0x30;
    pub const INT_DEST_LOWER: usize = 0x38;
    pub const LINK_CONFIG: usize = 0x48;
    pub const TEST_PIN_CONTROL: usize = 0x70;
    pub const DIRECT_MAP: usize = 0x80;
    pub const SSRAM_PARITY_ERR: usize = 0x90;
    pub const ARBITRATION: usize = 0xa0;
    pub const NIC: usize = 0xb0;
    pub const BUS_TIMEOUT: usize = 0xc0;
    pub const PCI_TYPE1_CONFIG: usize = 0xc8;
    pub const PCI_ERR_LOWER: usize = 0xd8;
    pub const INTERRUPT_STATUS: usize = 0x100;
    pub const INTERRUPT_ENABLE: usize = 0x108;
    pub const INTERRUPT_RESET: usize = 0x110;
    pub const INTERRUPT_MODE: usize = 0x118;
    pub const INTERRUPT_DEVICE: usize = 0x120;
    pub const INTERRUPT_ERROR_HOST: usize = 0x128;
    pub const INTERRUPT_HOST_BASE: usize = 0x130;
    pub const INTERRUPT_HOST_LAST: usize = 0x168;
    pub const INTERRUPT_MULTIPLE: usize = 0x170;
    pub const FORCE_INTERRUPT_BASE: usize = 0x180;
    pub const FORCE_PIN_BASE: usize = 0x1c0;
    pub const FORCE_PIN_LAST: usize = 0x1f8;
    pub const DEVICE_BASE: usize = 0x200;
    pub const DEVICE_LAST: usize = 0x238;
    pub const RRB_EVEN: usize = 0x280;
    pub const RRB_ODD: usize = 0x288;
    pub const RRB_CLEAR: usize = 0x298;
    pub const ATE_UPPER_BASE: u64 = 0x10000;
    pub const ATE_LOWER_BASE: u64 = 0x12000;
    pub const ATE_END: u64 = 0x14000;

}

/// The crossbar has the Crossbow's widget 0 register map, which `xbow.rs`
/// owns; the register offsets here are its constants. The access rules and
/// reset values below are this chip's own (the Octane's Crossbow model differs
/// in status, link state, wide accesses and where its interrupts go).
use crate::dev::xbow;

/// Fences run from widget 0's to link 15's.
const FENCE_LAST: u32 = xbow::FENCE_END - 8;
/// Read-only registers between the resource lock and the first link.
const READ_ONLY_BASE: u32 = xbow::LOCK_CLEAR + 8;
const READ_ONLY_LAST: u32 = xbow::LINK_BASE - 8;
fn ix(off: u32) -> usize {
    (off / 8) as usize
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptPacket {
    pub source_widget: u8,
    pub widget: u8,
    pub address: u64,
    pub vector: u8,
    pub level: bool,
}
pub type InterruptSender = Arc<dyn Fn(InterruptPacket) -> bool + Send + Sync>;

/// Source identity and placement hints retained for a host's inbound packet path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaPacket {
    pub source_widget: u8,
    pub slot: u8,
    pub widget: u8,
    pub address: u64,
    pub write: bool,
    pub half_write_requested: bool,
    pub half_read_requested: bool,
}
/// Bytes are in XIO address order after bridge placement. A false return
/// refuses that access. The host applies the requested exchange of doubleword halves.
pub type DmaSender = Arc<dyn Fn(DmaPacket, &mut [u8]) -> bool + Send + Sync>;

struct Routes {
    targets: RwLock<[Option<Arc<dyn BusDevice>>; 16]>,
    interrupts: RwLock<Option<InterruptSender>>,
    dma: RwLock<Option<DmaSender>>,
}
impl Routes {
    fn interrupt(&self, p: InterruptPacket) -> bool {
        let sender = self.interrupts.read().clone();
        let accepted = if let Some(send) = sender {
            send(p)
        } else {
            let target = self.targets.read()[p.widget as usize].clone();
            target.is_some_and(|t| {
                t.write64(
                    p.address,
                    p.vector as u64 | if p.level { 1 << 8 } else { 0 },
                ) == BUS_OK
            })
        };
        if !accepted {
            crate::dlog!(crate::devlog::LogModule::Bedrock, "xbridge: refused interrupt {p:?}");
        }
        accepted
    }
}

/// One chip owns both heads and widget 0; views share routing and reset state.
pub struct XBridge {
    routes: Arc<Routes>,
    heads: [Arc<XBridgeHead>; 2],
    xbow: Arc<Crossbar>,
}
impl XBridge {
    pub fn new(memory: Arc<dyn BusDevice>) -> Arc<Self> {
        let routes = Arc::new(Routes {
            targets: RwLock::new(std::array::from_fn(|i| {
                if i == HOST_WIDGET as usize {
                    Some(memory.clone())
                } else {
                    None
                }
            })),
            interrupts: RwLock::new(None),
            dma: RwLock::new(None),
        });
        let heads = [
            XBridgeHead::new(HEAD_E, 1, routes.clone()),
            XBridgeHead::new(HEAD_F, 0, routes.clone()),
        ];
        let xbow = Arc::new(Crossbar {
            st: Mutex::new(XbowState::new()),
            order: Mutex::new(()),
            routes: routes.clone(),
            heads: heads.each_ref().map(Arc::downgrade),
            reset_output: RwLock::new(None),
        });
        Arc::new(Self {
            routes,
            heads,
            xbow,
        })
    }
    pub fn register_locks(self: &Arc<Self>) {
        use crate::locks::register_lock_fn;
        for head in &self.heads {
            for (what, probe) in [
                ("state", (|h: &XBridgeHead| h.st.is_locked()) as fn(&XBridgeHead) -> bool),
                ("irq_order", |h| h.order.is_locked()),
                ("microlan", |h| h.microlan.is_locked()),
            ] {
                let h = Arc::downgrade(head);
                let name = format!("xbridge::head{:x}::{what}", head.widget);
                register_lock_fn(name, move || h.upgrade().is_some_and(|h| probe(&h)));
            }
        }
        for (what, probe) in [
            ("state", (|x: &Crossbar| x.st.is_locked()) as fn(&Crossbar) -> bool),
            ("irq_order", |x| x.order.is_locked()),
        ] {
            let x = Arc::downgrade(&self.xbow);
            register_lock_fn(format!("xbridge::crossbar::{what}"), move || {
                x.upgrade().is_some_and(|x| probe(&x))
            });
        }
    }
    fn pci_head(&self, widget: u8) -> &Arc<XBridgeHead> {
        assert!((HEAD_E..=HEAD_F).contains(&widget));
        &self.heads[(widget - HEAD_E) as usize]
    }
    pub fn head(&self, widget: u8) -> Arc<dyn BusDevice> {
        self.pci_head(widget).clone()
    }
    pub fn crossbar(&self) -> Arc<dyn BusDevice> {
        self.xbow.clone()
    }
    /// A reader-specific widget-0 view; the host reads port 10 by default.
    pub fn crossbar_for_port(&self, port: u8) -> Arc<dyn BusDevice> {
        assert!((8..=15).contains(&port));
        Arc::new(CrossbarView {
            xbow: self.xbow.clone(),
            port,
        })
    }
    /// Host-port processor projection uses the XBridge widget-0 alias.
    pub fn widget(&self, widget: u8) -> Option<Arc<dyn BusDevice>> {
        match widget {
            0 | HOST_WIDGET => Some(self.crossbar()),
            HEAD_E | HEAD_F => Some(self.head(widget)),
            _ => self
                .routes
                .targets
                .read()
                .get(widget as usize)
                .and_then(Clone::clone),
        }
    }
    /// The PCI bus behind head `widget`: functions are fitted with
    /// [`PciBus::attach`] and the SuperIO interrupt is `bus.board_irq(BoardIrq(0))`.
    pub fn bus(&self, widget: u8) -> &PciBus {
        &self.pci_head(widget).bus
    }
    pub fn dma(&self, widget: u8, slot: u8) -> Arc<dyn PciDma> {
        assert!(slot < 8);
        self.pci_head(widget).dma_for(slot)
    }
    pub fn set_interrupt_sender(&self, send: InterruptSender) {
        *self.routes.interrupts.write() = Some(send);
    }
    pub fn set_dma_sender(&self, send: DmaSender) {
        *self.routes.dma.write() = Some(send);
    }
    pub fn set_dma_64bit(&self, widget: u8, slot: u8, wide: bool) {
        assert!(slot < 8);
        let head = self.pci_head(widget);
        {
            let mut st = head.st.lock();
            st.dma_64bit = (st.dma_64bit & !(1 << slot)) | ((wide as u8) << slot);
        }
        // A 64-bit slot changes the byte order of the 64-bit alias.
        head.bus.set_host_windows(&|| head.windows());
    }
    pub fn set_wide_slot_mask(&self, widget: u8, mask: u8) {
        self.pci_head(widget).st.lock().wide_slots = mask;
    }
    pub fn set_microlan(&self, widget: u8, bus: w1::Bus) {
        *self.pci_head(widget).microlan.lock() = Some(bus);
    }
    /// An interrupt packet from the widget attached at `p.source_widget`,
    /// routed as the heads' packets are. False when nothing takes it.
    pub fn send_interrupt(&self, p: InterruptPacket) -> bool {
        assert!((8..14).contains(&p.source_widget) && p.source_widget != HOST_WIDGET);
        self.routes.interrupt(p)
    }
    pub fn attach_widget(&self, widget: u8, target: Arc<dyn BusDevice>) {
        assert!((8..14).contains(&widget) && widget != HOST_WIDGET);
        self.routes.targets.write()[widget as usize] = Some(target);
        self.set_link_present(widget, true);
    }
    pub fn set_link_present(&self, widget: u8, present: bool) {
        assert!((8..=15).contains(&widget));
        let mut st = self.xbow.st.lock();
        st.present = (st.present & !(1 << (widget - 8))) | ((present as u8) << (widget - 8));
    }
    pub fn set_link_8bit(&self, widget: u8, enabled: bool) {
        assert!((8..=15).contains(&widget));
        let mut st = self.xbow.st.lock();
        let bit = 1 << (widget - 8);
        st.eight_bit = (st.eight_bit & !bit) | if enabled { bit } else { 0 };
    }
    /// A pulse for each reset port, outside chip locks. A head's registers,
    /// routed PCI lines and function headers are already reset. The board
    /// must reset that port's attached engines through this callback.
    pub fn set_reset_output(&self, output: Arc<dyn Fn(u8) + Send + Sync>) {
        *self.xbow.reset_output.write() = Some(output);
    }
    /// Time is supplied by the machine's virtual clock, in nanoseconds.
    pub fn warm_reset(&self, source_port: u8, now_ns: u64) {
        self.xbow.warm_reset(source_port, now_ns);
    }
    pub fn reset(&self) {
        for h in &self.heads {
            h.reset();
        }
        let mut st = self.xbow.st.lock();
        st.reset();
        st.quiet_until = 0;
    }
}
impl Resettable for XBridge {
    fn power_on(&self) {
        self.reset();
    }
}

struct HeadState {
    reg: [u32; 0x300 / 8],
    ate: [u64; ATE_ENTRIES],
    pins: u8,
    dma_64bit: u8,
    wide_slots: u8,
}
impl HeadState {
    fn r(&self, off: usize) -> u32 {
        self.reg[off / 8]
    }
    fn set(&mut self, off: usize, value: u32) {
        self.reg[off / 8] = value;
    }
    fn reset(&mut self, speed: u8) {
        self.reg.fill(0);
        self.ate.fill(0);
        self.pins = 0;
        for (off, value) in [
            (0, 0x2d002049),
            (8, 0x20),
            (0x20, 0x0f00320f | ((speed as u32) << 4)),
            (0x28, 0xfffff),
            (reg::LINK_CONFIG, 0x03ff1810),
            (0x80, 0x20000),
            (0xa0, 0x20008),
            (0xc0, 0x10000),
        ] {
            self.set(off, value);
        }
        for slot in 0..8 {
            self.set(
                0x200 + slot * 8,
                0x18001000
                    | if slot < 2 {
                        slot as u32 * 2
                    } else {
                        slot as u32 + 2
                    },
            );
        }
    }
    fn packet(&self, pin: Option<usize>, level: bool) -> InterruptPacket {
        let upper = self.r(reg::INT_DEST_UPPER);
        let mut address = ((upper as u64 & 0xffff) << 32) | self.r(reg::INT_DEST_LOWER) as u64;
        let vector = if let Some(pin) = pin {
            let host = self.r(reg::INTERRUPT_HOST_BASE + pin * 8);
            address = (address & !(0x3ff << 38)) | (((host >> 8) as u64 & 0x3ff) << 38);
            host as u8
        } else {
            self.r(reg::INTERRUPT_ERROR_HOST) as u8
        };
        InterruptPacket {
            source_widget: (self.r(reg::CONTROL) & 15) as u8,
            widget: ((upper >> 16) & 15) as u8,
            address,
            vector,
            level,
        }
    }
}
// Lock order: order, then microlan (NIC only), then st.
struct XBridgeHead {
    widget: u8,
    speed: u8,
    st: Mutex<HeadState>,
    order: Mutex<()>,
    routes: Arc<Routes>,
    microlan: Mutex<Option<w1::Bus>>,
    /// The head's PCI bus: its functions, BAR windows and interrupt pins.
    bus: PciBus,
    /// Whole-page BAR mappings of the device and alias windows, which the
    /// processor reaches without a search of the bus.
    pio: PioDecode,
    /// This head, for the DMA a function is given: it must not keep the head alive.
    me: Weak<XBridgeHead>,
}

/// A run of the head's address space that one BAR covers whole, and the bus
/// device that answers it.
pub(crate) struct PioRun {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) dev: Arc<dyn BusDevice>,
}

/// Whole BAR-page runs are few and carry no RAM or native fault-handler state.
/// Readers clone one endpoint and release the table lock before calling a device.
#[derive(Default)]
pub(crate) struct PioDecode {
    runs: RwLock<Vec<PioRun>>,
}
impl PioDecode {
    pub(crate) fn publish(&self, mut runs: Vec<PioRun>) -> Vec<PioRun> {
        runs.sort_unstable_by_key(|run| run.start);
        debug_assert!(runs.windows(2).all(|pair| pair[0].end <= pair[1].start));
        std::mem::replace(&mut *self.runs.write(), runs)
    }
    pub(crate) fn target(&self, address: u64, size: usize) -> Option<Arc<dyn BusDevice>> {
        let runs = self.runs.read();
        let i = runs.partition_point(|run| run.start <= address).checked_sub(1)?;
        let run = &runs[i];
        (address.checked_add(size as u64)? <= run.end).then(|| run.dev.clone())
    }
}

/// The head's PCI interrupt routing. Device `d`'s INTA reaches interrupt pin `d`; the
/// pins are shared with the board's own IRQs (the IOC3's SuperIO interrupt
/// is `BoardIrq(0)`). The Fuel uses devices 1, 4 and 5; the others follow the
/// same rule, and INTB to INTD are not wired.
pub static PCI_ROUTING: IntxRouting = IntxRouting {
    devices: 8,
    irqs: 8,
    intx: &[
        (0, [Some(0), None, None, None]),
        (1, [Some(1), None, None, None]),
        (2, [Some(2), None, None, None]),
        (3, [Some(3), None, None, None]),
        (4, [Some(4), None, None, None]),
        (5, [Some(5), None, None, None]),
        (6, [Some(6), None, None, None]),
        (7, [Some(7), None, None, None]),
    ],
    board_irqs: &[0, 1, 2, 3, 4, 5, 6, 7],
};

/// The head's CPU windows, by [`CpuWindow`] index: the eight device windows,
/// then the 32-bit and 64-bit memory aliases and the I/O alias.
const DEVICE_WINDOWS: usize = 8;
const ALIAS_MEM32: usize = 8;
const ALIAS_MEM64: usize = 9;
const ALIAS_IO: usize = 10;
const WINDOWS: usize = 11;

struct SlotDma {
    head: Weak<XBridgeHead>,
    slot: u8,
}
impl PciDma for SlotDma {
    fn dma_read(&self, address: u64, bytes: &mut [u8]) -> PciDmaStatus {
        if let Some(h) = self.head.upgrade() {
            h.dma(self.slot, address, bytes, false)
        } else {
            bytes.fill(0);
            PciDmaStatus::MasterAbort
        }
    }
    fn dma_write(&self, address: u64, bytes: &[u8]) -> PciDmaStatus {
        self.head.upgrade().map_or(PciDmaStatus::MasterAbort, |h| {
            h.dma(self.slot, address, &mut bytes.to_vec(), true)
        })
    }
}
/// What the head's bus asks of it: the DMA of a requester, and an interrupt
/// pin's level, as the OR of every device line and board IRQ routed to it.
impl PciHost for XBridgeHead {
    fn dma_for(&self, requester: u8) -> Arc<dyn PciDma> {
        Arc::new(SlotDma { head: self.me.clone(), slot: requester })
    }
    fn set_irq(&self, pin: u8, level: bool) {
        self.pin(pin as usize, level);
    }
}
impl XBridgeHead {
    fn new(widget: u8, speed: u8, routes: Arc<Routes>) -> Arc<Self> {
        let mut st = HeadState {
            reg: [0; 96],
            ate: [0; ATE_ENTRIES],
            pins: 0,
            dma_64bit: 0,
            wide_slots: 0,
        };
        st.reset(speed);
        let this = Arc::new_cyclic(|me: &Weak<XBridgeHead>| Self {
            widget,
            speed,
            st: Mutex::new(st),
            order: Mutex::new(()),
            routes,
            microlan: Mutex::new(None),
            // iris's bus numbers count per bus tree; the heads are separate trees.
            bus: PciBus::new((widget - HEAD_E) * 16, &PCI_ROUTING, me.clone()),
            pio: PioDecode::default(),
            me: me.clone(),
        });
        this.bus.set_host_windows(&|| this.windows());
        let head = Arc::downgrade(&this);
        this.bus.on_bar_mappings(Box::new(move |runs| {
            if let Some(head) = head.upgrade() {
                head.publish_mappings(runs);
            }
        }));
        this
    }
    fn pin(&self, pin: usize, level: bool) {
        if pin >= 8 {
            return;
        }
        let _order = self.order.lock();
        let packet = {
            let mut st = self.st.lock();
            let bit = 1 << pin;
            let old = st.pins & bit != 0;
            st.pins = (st.pins & !bit) | if level { bit } else { 0 };
            let pins = st.pins as u32;
            let status = (st.r(reg::INTERRUPT_STATUS) & !255) | pins;
            st.set(reg::INTERRUPT_STATUS, status);
            (old != level
                && st.r(reg::INTERRUPT_ENABLE) & bit as u32 != 0
                && (level || st.r(reg::INTERRUPT_MODE) & bit as u32 != 0))
                .then(|| st.packet(Some(pin), level))
        };
        if let Some(packet) = packet {
            self.routes.interrupt(packet);
        }
    }
    fn reset(&self) {
        {
            let _order = self.order.lock();
            self.st.lock().reset(self.speed);
        }
        // RST#: headers, then functions, which drop their own lines. The
        // Device(x) registers are back at reset, so the windows move too.
        self.bus.reset();
        self.bus.set_host_windows(&|| self.windows());
    }
    fn invalid(&self, address: u64) {
        let _order = self.order.lock();
        let packet = {
            let mut st = self.st.lock();
            let first = st.r(reg::INTERRUPT_STATUS) & (1 << 24) == 0;
            let status = st.r(reg::INTERRUPT_STATUS) | (1 << 24);
            st.set(reg::INTERRUPT_STATUS, status);
            let multi = st.r(reg::INTERRUPT_MULTIPLE) | (1 << 24);
            st.set(reg::INTERRUPT_MULTIPLE, multi);
            if first {
                st.set(reg::ERR_UPPER, (address >> 32) as u32);
                st.set(reg::ERR_LOWER, address as u32);
            }
            (first && st.r(reg::INTERRUPT_ENABLE) & (1 << 24) != 0).then(|| st.packet(None, true))
        };
        if let Some(p) = packet {
            self.routes.interrupt(p);
        }
        crate::dlog!(crate::devlog::LogModule::Bedrock,
            "xbridge: head {} invalid access at {address:#x}",
            self.widget
        );
    }
    fn defined(off: usize) -> bool {
        matches!(off, reg::IDENTIFICATION..=reg::TEST_PIN_CONTROL | reg::DIRECT_MAP | reg::SSRAM_PARITY_ERR | reg::ARBITRATION | reg::NIC | reg::BUS_TIMEOUT..=reg::PCI_ERR_LOWER | reg::INTERRUPT_STATUS..=reg::FORCE_PIN_LAST | reg::DEVICE_BASE..=reg::RRB_CLEAR)
    }
    fn deferred(address: u64) -> bool {
        (0x300..0x600).contains(&address)
            || (0x30000..0x30008).contains(&address)
            || (0xc00000..0x1000000).contains(&address)
    }
    fn config(&self, address: u64, size: usize, value: Option<u64>) -> u64 {
        if size == 8 {
            let hi = self.config(address, 4, value.map(|v| v >> 32));
            return (hi << 32) | self.config(address + 4, 4, value.map(|v| v & 0xffffffff));
        }
        let reg = ((address & 255) as usize) ^ (4 - size);
        let function = ((address >> 8) & 7) as u8;
        // The type 0 aperture selects a device by address; the type 1
        // aperture takes it, and the bus, from the PCI Type 1 Configuration
        // register. Bus 0 there is this head's bus.
        let (bus, device) = if address < 0x28000 {
            (Some(self.bus.clone()), ((address - 0x20000) >> 12) as u8)
        } else {
            let sel = self.st.lock().r(reg::PCI_TYPE1_CONFIG);
            (self.bus.find_bus(((sel >> 16) & 255) as u8), ((sel >> 11) & 31) as u8)
        };
        let at = DevFn { device, function };
        let mask = u32::MAX >> (32 - size * 8);
        if let Some(value) = value {
            if let Some(bus) = bus {
                bus.config_write(at, reg, size, value as u32 & mask);
            }
            return 0;
        }
        match bus.and_then(|b| b.config_read(at, reg, size)) {
            Some(v) => v as u64,
            None => {
                let mut st = self.st.lock();
                let multi = st.r(reg::INTERRUPT_MULTIPLE) | (1 << 11);
                st.set(reg::INTERRUPT_MULTIPLE, multi);
                mask as u64
            }
        }
    }
    fn reg_write(&self, address: u64, value: u32) {
        let off = address as usize & !7;
        let _order = self.order.lock();
        let mut packets = Vec::new();
        if off == 0xb0 {
            let pulse = (value >> 10) & 0x3ff;
            let sample = (value >> 2) & 255;
            let mut bus = self.microlan.lock();
            if let Some(bus) = bus.as_mut() {
                let level = w1::slot::slot(bus, pulse, sample);
                self.st.lock().set(reg::NIC, 2 | level as u32);
            } else {
                crate::dlog!(crate::devlog::LogModule::Bedrock, "xbridge: head {} NIC has no board", self.widget);
            }
            return;
        }
        {
            let mut st = self.st.lock();
            let masked = match off {
                reg::CONTROL => (value & 0x0fe1f20f) | ((self.speed as u32) << 4),
                reg::REQUEST_TIMEOUT | reg::INT_DEST_UPPER => value & 0xfffff,
                reg::INT_DEST_LOWER | reg::RRB_EVEN | reg::RRB_ODD => value,
                reg::LINK_CONFIG => value & 0x03ffffff,
                reg::TEST_PIN_CONTROL => value & 0xffffff,
                reg::DIRECT_MAP => value & 0xf3ffff,
                reg::ARBITRATION => value & 0x3ff7f,
                reg::BUS_TIMEOUT => value & 0x1f03ff,
                reg::PCI_TYPE1_CONFIG => value & 0xfff800,
                reg::INTERRUPT_DEVICE => value & 0xffffff,
                reg::INTERRUPT_ERROR_HOST | reg::INTERRUPT_MODE => value & 255,
                reg::INTERRUPT_ENABLE => {
                    let value = value & 0x7fffffff;
                    let newly = value & !st.r(reg::INTERRUPT_ENABLE) & st.pins as u32;
                    st.set(off, value);
                    for pin in 0..8 {
                        if newly & (1 << pin) != 0 {
                            packets.push(st.packet(Some(pin), true));
                        }
                    }
                    value
                }
                reg::INTERRUPT_RESET => {
                    let clear = (if value & 2 != 0 { 1 << 30 } else { 0 })
                        | (if value & 8 != 0 { 1 << 24 } else { 0 });
                    let status = st.r(reg::INTERRUPT_STATUS) & !clear;
                    st.set(reg::INTERRUPT_STATUS, status);
                    let multi = if value & 64 != 0 {
                        0
                    } else {
                        st.r(reg::INTERRUPT_MULTIPLE) & !clear
                    };
                    st.set(reg::INTERRUPT_MULTIPLE, multi);
                    return;
                }
                reg::FORCE_INTERRUPT_BASE..=reg::FORCE_PIN_LAST => {
                    let pin = (off - reg::FORCE_INTERRUPT_BASE) / 8 & 7;
                    if st.r(reg::INTERRUPT_ENABLE) & (1 << pin) == 0 {
                        crate::dlog!(crate::devlog::LogModule::Bedrock, "xbridge: masked force semantics unresolved");
                    } else if off < reg::FORCE_PIN_BASE || st.pins & (1 << pin) != 0 {
                        packets.push(st.packet(Some(pin), true));
                    }
                    0
                }
                reg::INTERRUPT_HOST_BASE..=reg::INTERRUPT_HOST_LAST => value & 0x3ffff,
                reg::DEVICE_BASE..=reg::DEVICE_LAST => value & 0x1eefffff,
                _ => {
                    crate::dlog!(crate::devlog::LogModule::Bedrock,
                        "xbridge: head {} ignored register write {off:#x}",
                        self.widget
                    );
                    return;
                }
            };
            if !(reg::FORCE_INTERRUPT_BASE..=reg::FORCE_PIN_LAST).contains(&off) {
                st.set(off, masked);
            }
        }
        for p in packets {
            self.routes.interrupt(p);
        }
        drop(_order);
        if off == reg::CONTROL || (reg::DEVICE_BASE..=reg::DEVICE_LAST).contains(&off) {
            self.bus.set_host_windows(&|| self.windows());
        }
    }

    /// The CPU windows, from the Device(x) registers; see [`WINDOWS`].
    fn windows(&self) -> Vec<CpuWindow> {
        let st = self.st.lock();
        (0..WINDOWS)
            .map(|kind| {
                if kind < DEVICE_WINDOWS {
                    let (space, base, _) = Self::device(st.r(reg::DEVICE_BASE + kind * 8), kind);
                    let (cpu, len) = if kind < 2 {
                        (0x200000 + kind as u64 * 0x200000, 0x200000)
                    } else {
                        (0x600000 + (kind as u64 - 2) * 0x100000, 0x100000)
                    };
                    CpuWindow { space, cpu, pci: base << 20, len, mask: u64::MAX }
                } else {
                    let (cpu, len, space) = match kind {
                        ALIAS_MEM32 => (0x40000000, 0x40000000, Space::Mem),
                        ALIAS_MEM64 => (0x80000000, 0x40000000, Space::Mem),
                        _ => (0x100000000, 0x100000000, Space::Io),
                    };
                    CpuWindow { space, cpu, pci: 0, len, mask: u64::MAX }
                }
            })
            .collect()
    }

    /// Device(x) register `dev` of window `slot`: the space it opens onto,
    /// its PCI base in megabytes, and whether it is a 64-bit device.
    fn device(dev: u32, slot: usize) -> (Space, u32, bool) {
        let memory = dev & (1 << 12) != 0;
        let mut base = dev & if memory { 0x3ff } else { 0xfff };
        if slot < 2 {
            base &= !1;
        }
        (if memory { Space::Mem } else { Space::Io }, base, dev & (1 << 22) != 0)
    }

    /// The byte order of a CPU access to window `kind` reaching function `f`. A device
    /// window follows its Device(x) register: the swap bit gives a byte
    /// stream, otherwise the device's width gives word values. An alias
    /// follows Control's memory or I/O swap bit; the 64-bit memory alias is
    /// 64 bits wide for the slots set 64-bit.
    fn order(&self, kind: usize, f: &dyn PciFunction) -> ByteOrder {
        let st = self.st.lock();
        if kind < DEVICE_WINDOWS {
            let dev = st.r(reg::DEVICE_BASE + kind * 8);
            return if dev & (1 << 13) != 0 { ByteOrder::Stream } else { ByteOrder::values(if dev & (1 << 22) != 0 { 8 } else { 4 }) };
        }
        if st.r(reg::CONTROL) & (1 << if kind == ALIAS_IO { 23 } else { 22 }) != 0 {
            return ByteOrder::Stream;
        }
        let wide = kind == ALIAS_MEM64 && self.slot_of(f).is_some_and(|slot| st.dma_64bit & (1 << slot) != 0);
        ByteOrder::values(if wide { 8 } else { 4 })
    }

    /// The device number `f` sits at.
    fn slot_of(&self, f: &dyn PciFunction) -> Option<u8> {
        let same = |g: &Arc<dyn PciFunction>| std::ptr::addr_eq(Arc::as_ptr(g), f as *const dyn PciFunction);
        (0..DEVICE_WINDOWS as u8).find(|&device| (0..8).any(|function| self.bus.function(DevFn { device, function }).is_some_and(same)))
    }

    /// The bus reports the BAR mappings the head's windows hold: serve them
    /// from the head's own table.
    fn publish_mappings(&self, runs: Vec<BarMapping>) {
        let runs = runs
            .iter()
            .map(|r| {
                let order = self.order(r.window as usize, &*r.function);
                let base = r.cpu.wrapping_sub(r.off as u64);
                PioRun { start: r.cpu, end: r.cpu + r.len, dev: Arc::new(BarDevice::new(r.function.clone(), r.bar, base, u64::MAX, order)) }
            })
            .collect();
        // Replaced runs may run shutdown callbacks from their destructor.
        drop(self.pio.publish(runs));
    }

    /// The window kind, address space and PCI address that head address
    /// `address` reaches, for the slow path.
    fn pio_target(&self, address: u64) -> Option<(usize, Space, u32)> {
        if (0x200000..0xc00000).contains(&address) {
            let (slot, off) = if address < 0x600000 {
                (((address - 0x200000) >> 21) as usize, address & 0x1fffff)
            } else {
                (2 + ((address - 0x600000) >> 20) as usize, address & 0xfffff)
            };
            let (space, base, _) = Self::device(self.st.lock().r(reg::DEVICE_BASE + slot * 8), slot);
            return Some((slot, space, (base << 20).wrapping_add(off as u32)));
        }
        match address {
            0x40000000..=0x7fffffff => Some((ALIAS_MEM32, Space::Mem, (address - 0x40000000) as u32)),
            0x80000000..=0xbfffffff => Some((ALIAS_MEM64, Space::Mem, (address - 0x80000000) as u32)),
            0x100000000..=0x1ffffffff => Some((ALIAS_IO, Space::Io, (address - 0x100000000) as u32)),
            _ => None,
        }
    }

    /// A processor access that no whole-page run answers: the bus finds the
    /// BAR and the window's byte order carries the bytes. Nothing claiming the
    /// address reads as ones and drops a write.
    fn slow_pio(&self, address: u64, size: usize, value: Option<u64>) -> u64 {
        let Some((kind, space, pci)) = self.pio_target(address) else { return 0 };
        let Some((f, bar, off)) = self.bus.decode(space, pci) else {
            crate::dlog!(crate::devlog::LogModule::Bedrock,
                "xbridge: head {} no PCI BAR for {space:?} {pci:#x}",
                self.widget
            );
            return if value.is_none() { u64::MAX >> (64 - size * 8) } else { 0 };
        };
        let order = self.order(kind, f);
        match value {
            None => order.read(f, bar, off, size),
            Some(v) => {
                order.write(f, bar, off, size, v);
                0
            }
        }
    }
    fn access(&self, address: u64, size: usize, value: Option<u64>) -> u64 {
        let write = value.is_some();
        if (0x20000..0x29000).contains(&address) {
            return self.config(address, size, value);
        }
        if (0x200000..0xc00000).contains(&address)
            || (0x40000000..0xc0000000).contains(&address)
            || (0x100000000..0x200000000).contains(&address)
        {
            // An unaligned access can straddle a BAR's final mapped page.
            if address & 0xfff > 0x1000 - size as u64 {
                return self.slow_pio(address, size, value);
            }
            let Some(endpoint) = self.pio.target(address, size) else {
                return self.slow_pio(address, size, value);
            };
            return if let Some(v) = value {
                match size {
                    1 => { endpoint.write8(address, v as u8); }
                    2 => { endpoint.write16(address, v as u16); }
                    4 => { endpoint.write32(address, v as u32); }
                    _ => { endpoint.write64(address, v); }
                }
                0
            } else {
                match size {
                    1 => endpoint.read8(address).data as u64,
                    2 => endpoint.read16(address).data as u64,
                    4 => endpoint.read32(address).data as u64,
                    _ => endpoint.read64(address).data,
                }
            };
        }
        if size == 8 && address & 7 == 0 && (reg::ATE_UPPER_BASE..reg::ATE_LOWER_BASE).contains(&address) && write {
            self.st.lock().ate[((address - reg::ATE_UPPER_BASE) / 8) as usize] =
                value.unwrap() & 0x0000ffffffffffff;
            return 0;
        }
        if size == 4 && address & 7 == 4 {
            if !write {
                if (reg::ATE_UPPER_BASE..reg::ATE_LOWER_BASE).contains(&address) {
                    return self.st.lock().ate[((address - reg::ATE_UPPER_BASE) / 8) as usize] >> 32;
                }
                if (reg::ATE_LOWER_BASE..reg::ATE_END).contains(&address) {
                    return self.st.lock().ate[((address - reg::ATE_LOWER_BASE) / 8) as usize] as u32 as u64;
                }
            }
            if address < 0x300 && Self::defined(address as usize & !7) {
                if let Some(value) = value {
                    self.reg_write(address, value as u32);
                    return 0;
                }
                return self.st.lock().r(address as usize & !7) as u64;
            }
        }
        if Self::deferred(address) {
            crate::dlog!(crate::devlog::LogModule::Bedrock,
                "xbridge: head {} deferred access at {address:#x}",
                self.widget
            );
        } else {
            self.invalid(address);
        }
        0
    }
    fn translate(
        &self,
        slot: u8,
        address: u64,
        write: bool,
        packets: bool,
    ) -> Option<(u8, u64, u64, bool)> {
        let mut st = self.st.lock();
        let dev = st.r(reg::DEVICE_BASE + slot as usize * 8);
        let wide = st.dma_64bit & (1 << slot) != 0;
        let device_swap = dev & (1 << 19) != 0;
        let (widget, target, swap, direct, page, wide64) = if (0x40000000..0x40800000)
            .contains(&address)
            || (0x60000000..0x60800000).contains(&address)
        {
            let shift = if st.r(reg::CONTROL) & (1 << 21) != 0 { 14 } else { 12 };
            let index = ((address & 0x1fffffff) >> shift) as usize;
            if index >= ATE_MAPPED || st.ate[index] & 1 == 0 {
                let v = st.r(reg::INTERRUPT_STATUS) | (1 << 30);
                st.set(reg::INTERRUPT_STATUS, v);
                let v = st.r(reg::INTERRUPT_MULTIPLE) | (1 << 30);
                st.set(reg::INTERRUPT_MULTIPLE, v);
                crate::dlog!(
                    crate::devlog::LogModule::Bedrock,
                    "xbridge: invalid page DMA {address:#x}, entry {index}"
                );
                return None;
            }
            let ate = st.ate[index];
            let mask = (1 << shift) - 1;
            (
                ((ate >> 8) & 15) as u8,
                (ate & 0x0000fffffffff000 & !mask) | (address & mask),
                ate & 32 != 0 || address & (1 << 29) != 0,
                false,
                true,
                false,
            )
        } else if address >= 0x1000000000000000 {
            (
                (address >> 60) as u8,
                address & 0x0000ffffffffffff,
                address & (1 << 55) != 0,
                false,
                false,
                true,
            )
        } else if (0x80000000..=0xffffffff).contains(&address) {
            let direct = st.r(reg::DIRECT_MAP);
            let mut target = ((direct as u64 & 0x1ffff) << 31) | (address & 0x7fffffff);
            if direct & 0x1ffff == 0 && direct & (1 << 17) != 0 {
                target += 0x20000000;
            }
            (
                ((direct >> 20) & 15) as u8,
                target,
                device_swap,
                true,
                false,
                false,
            )
        } else {
            crate::dlog!(crate::devlog::LogModule::Bedrock, "xbridge: unsupported DMA {address:#x}");
            return None;
        };
        let half = packets
            && swap
            && st.wide_slots & (1 << slot) != 0
            && if write {
                direct || page || (wide64 && device_swap)
            } else {
                direct || (wide64 && wide && device_swap) || (page && address & (1 << 29) != 0)
            };
        let mask = if swap || half {
            0
        } else if wide {
            7
        } else {
            3
        };
        Some((widget, target, mask, half))
    }
    // Retain the existing memory path's probe before issuing a chunk's writes.
    // The neutral helper alone can commit bytes before reporting a refusal.
    fn write_memory(mem: &dyn BusDevice, address: u64, data: &[u8]) -> bool {
        if data.is_empty() {
            return true;
        }
        let first = address & !7;
        let Some(end) = address
            .checked_add(data.len() as u64)
            .and_then(|a| a.checked_add(7))
        else {
            return false;
        };
        let mut probe = vec![0; ((end & !7) - first) as usize];
        read_bytes(mem, first, &mut probe) == BUS_OK && write_bytes(mem, address, data) == BUS_OK
    }
    fn dma(&self, slot: u8, address: u64, bytes: &mut [u8], write: bool) -> PciDmaStatus {
        let sender = self.routes.dma.read().clone();
        let mut good = true;
        let mut done = 0;
        while done < bytes.len() {
            let Some(at) = address.checked_add(done as u64) else {
                if !write {
                    bytes[done..].fill(0);
                }
                return PciDmaStatus::MasterAbort;
            };
            let n = (0x1000 - (at as usize & 0xfff)).min(bytes.len() - done);
            if (0x3fff0000..0x40000000).contains(&at) {
                if !write {
                    bytes[done..done + n].fill(0);
                }
                done += n;
                continue;
            }
            if !write {
                bytes[done..done + n].fill(0);
            }
            let translated = self.translate(slot, at, write, sender.is_some());
            let accepted = if let Some((widget, target, mask, half)) = translated {
                if let Some(send) = &sender {
                    // Packet legs use naturally aligned accesses, retaining source identity.
                    let mut offset = 0;
                    let mut ok = true;
                    while offset < n {
                        let p = target + offset as u64;
                        let mut size = 8usize;
                        while size > n - offset || p & (size as u64 - 1) != 0 {
                            size /= 2;
                        }
                        let mut data = [0u8; 8];
                        if write {
                            for i in 0..size {
                                data[i] = bytes[done
                                    + offset
                                    + if half {
                                        i
                                    } else {
                                        i ^ (mask as usize & (size - 1))
                                    }];
                            }
                        }
                        let placed = if half {
                            p
                        } else {
                            p ^ (mask & !(size as u64 - 1))
                        };
                        let packet = DmaPacket {
                            source_widget: (self.st.lock().r(reg::CONTROL) & 15) as u8,
                            slot,
                            widget,
                            address: placed,
                            write,
                            half_write_requested: half && write && size == 8,
                            half_read_requested: half && !write && size == 8,
                        };
                        let part = send(packet, &mut data[..size]);
                        ok &= part;
                        if !write {
                            for i in 0..size {
                                bytes[done + offset + i] = if part {
                                    data[if half {
                                        i
                                    } else {
                                        i ^ (mask as usize & (size - 1))
                                    }]
                                } else {
                                    0
                                };
                            }
                        }
                        offset += size;
                    }
                    ok
                } else {
                    let target_device = self.routes.targets.read()[widget as usize].clone();
                    target_device.is_some_and(|mem| {
                        let low = target & !mask;
                        let high = (target + n as u64 + mask) & !mask;
                        let mut data = vec![0; (high - low) as usize];
                        if write {
                            for i in 0..n {
                                data[(((target + i as u64) ^ mask) - low) as usize] =
                                    bytes[done + i];
                            }
                            if low == target && high == target + n as u64 {
                                Self::write_memory(&*mem, low, &data)
                            } else {
                                let mut ok = true;
                                for i in 0..n {
                                    let p = (target + i as u64) ^ mask;
                                    ok &= Self::write_memory(
                                        &*mem,
                                        p,
                                        &data[(p - low) as usize..(p - low) as usize + 1],
                                    );
                                }
                                ok
                            }
                        } else {
                            // Preserve the old helper's covering-doubleword probe
                            // and zero the whole failed physical chunk, even if
                            // the neutral helper returned part of its contents.
                            let first = low & !7;
                            let mut fetched = vec![0; (((high + 7) & !7) - first) as usize];
                            let ok = read_bytes(&*mem, first, &mut fetched) == BUS_OK;
                            for i in 0..n {
                                bytes[done + i] = if ok {
                                    fetched[(((target + i as u64) ^ mask) - first) as usize]
                                } else {
                                    0
                                };
                            }
                            ok
                        }
                    })
                }
            } else {
                false
            };
            if !accepted {
                good = false;
            }
            done += n;
        }
        if good {
            PciDmaStatus::Ok
        } else {
            PciDmaStatus::MasterAbort
        }
    }
}

struct XbowState {
    reg: [u32; 96],
    status: u32,
    present: u8,
    eight_bit: u8,
    quiet_until: u64,
}
impl XbowState {
    fn new() -> Self {
        let mut st = Self {
            reg: [0; 96],
            status: 0,
            present: (1 << 2) | (1 << 6) | (1 << 7),
            eight_bit: 0,
            quiet_until: 0,
        };
        st.reset();
        st
    }
    fn reset(&mut self) {
        self.reg.fill(0);
        self.status = 0;
        for (off, value) in [
            (xbow::IDENTIFICATION, 0x2d000001),
            (xbow::CONTROL, 0x80),
            (xbow::PACKET_TIMEOUT, 0xfffff),
            (xbow::INT_UPPER, 0x80000),
            (xbow::LLP_CONTROL, 0x03ff1810),
            (xbow::ARB_RELOAD, 2),
            (xbow::NIC, 2),
            (xbow::FENCE_BASE, 255),
        ] {
            self.reg[ix(off)] = value;
        }
        for i in 0..8 {
            self.reg[ix(xbow::FENCE_BASE + 8 + i * 8)] = 255;
            self.reg[ix(xbow::LINK_BASE + xbow::LINK_CONTROL + i * xbow::LINK_STRIDE)] = 0x80000;
        }
    }
    fn packet(&self, level: bool) -> InterruptPacket {
        let upper = self.reg[ix(xbow::INT_UPPER)];
        InterruptPacket {
            source_widget: 0,
            widget: ((upper >> 16) & 15) as u8,
            address: ((upper as u64 & 0xffff) << 32) | self.reg[ix(xbow::INT_LOWER)] as u64,
            vector: (upper >> 24) as u8,
            level,
        }
    }
}
// Lock order: order, then st. Reset outputs run with both released.
struct Crossbar {
    st: Mutex<XbowState>,
    order: Mutex<()>,
    routes: Arc<Routes>,
    heads: [Weak<XBridgeHead>; 2],
    reset_output: RwLock<Option<Arc<dyn Fn(u8) + Send + Sync>>>,
}
impl Crossbar {
    fn reset_port(&self, port: u8) {
        if (14..=15).contains(&port) {
            if let Some(h) = self.heads[(port - 14) as usize].upgrade() {
                h.reset();
            }
        }
        let out = self.reset_output.read().clone();
        if let Some(out) = out {
            out(port);
        }
    }
    fn warm_reset(&self, source: u8, now: u64) {
        assert!((8..=15).contains(&source));
        let ports = {
            let mut st = self.st.lock();
            if now < st.quiet_until {
                return;
            }
            st.quiet_until = now.saturating_add(10240);
            if st.reg[ix(xbow::FENCE_BASE)] & (1 << (source - 8)) == 0 {
                return;
            }
            let ports: Vec<_> = (0..8)
                .filter(|&i| st.reg[ix(xbow::FENCE_BASE + 8 + i * 8)] & (1 << (source - 8)) != 0)
                .map(|i| i as u8 + 8)
                .collect();
            st.reset();
            ports
        };
        for port in ports {
            self.reset_port(port);
        }
    }
    fn access(&self, port: u8, address: u64, size: usize, value: Option<u64>) -> u64 {
        let _order = self.order.lock();
        let mut packet = None;
        let mut reset = None;
        let mut access_error = false;
        let result = {
            let mut st = self.st.lock();
            let off = address as u32 & !7;
            if size != 4 || address & 7 != 4 || address >= xbow::LINK_END as u64 || (READ_ONLY_BASE..=READ_ONLY_LAST).contains(&off) {
                st.status |= 32;
                access_error = true;
                if st.reg[ix(xbow::CONTROL)] & 32 != 0 && st.status & (1 << 23) == 0 {
                    st.status |= 1 << 23;
                    packet = Some(st.packet(true));
                }
                crate::dlog!(
                    crate::devlog::LogModule::Bedrock,
                    "xbridge-xbow: invalid access {address:#x} width {size}"
                );
                0
            } else if let Some(v) = value {
                let v = v as u32;
                if off >= xbow::LINK_BASE {
                    match (off - xbow::LINK_BASE) % xbow::LINK_STRIDE {
                        xbow::LINK_CONTROL => st.reg[ix(off)] = v & 0xbf7f01ff,
                        xbow::LINK_ARB_UPPER | xbow::LINK_ARB_LOWER => st.reg[ix(off)] = v,
                        xbow::LINK_RESET => reset = Some((8 + (off - xbow::LINK_BASE) / xbow::LINK_STRIDE) as u8),
                        _ => {
                            st.status |= 32;
                            access_error = true;
                            crate::dlog!(
                                crate::devlog::LogModule::Bedrock,
                                "xbridge-xbow: read-only link register offset {off:#x}"
                            );
                        }
                    }
                } else {
                    match off {
                        xbow::CONTROL => st.reg[ix(off)] = v & 0xfe,
                        xbow::PACKET_TIMEOUT => st.reg[ix(off)] = v & 0xfffff,
                        xbow::INT_UPPER => st.reg[ix(off)] = (v & 0xff07ffff) | 0x80000,
                        xbow::INT_LOWER => st.reg[ix(off)] = v,
                        xbow::ERR_CMD => {
                            st.reg[ix(xbow::ERR_CMD)] = 0;
                            st.reg[ix(xbow::ERR_UPPER)] = 0;
                            st.reg[ix(xbow::ERR_LOWER)] = 0;
                        }
                        xbow::LLP_CONTROL => st.reg[ix(off)] = v & 0x3ffffff,
                        xbow::ARB_RELOAD => st.reg[ix(off)] = v & 0x3f,
                        xbow::PERF_A | xbow::PERF_B => st.reg[ix(off)] = v & 0x700000,
                        xbow::NIC => {
                            st.reg[ix(off)] = (v & 0xffffc) | 2;
                            if v & 0xffc00 != 0 {
                                crate::dlog!(
                                    crate::devlog::LogModule::Bedrock,
                                    "xbridge-xbow: one-wire pulse unimplemented at register offset 0x70"
                                );
                            }
                        }
                        xbow::FENCE_BASE..=FENCE_LAST => st.reg[ix(off)] = v & 255,
                        xbow::LOCK => {
                            if st.reg[ix(off)] == 0 {
                                st.reg[ix(off)] = v & 255;
                            }
                        }
                        xbow::LOCK_CLEAR => st.reg[ix(xbow::LOCK)] = 0,
                        _ => {
                            st.status |= 32;
                            access_error = true;
                            crate::dlog!(
                                crate::devlog::LogModule::Bedrock,
                                "xbridge-xbow: read-only register offset {off:#x}"
                            );
                        }
                    }
                }
                0
            } else if off >= xbow::LINK_BASE {
                let link = (off - xbow::LINK_BASE) / xbow::LINK_STRIDE;
                let present = st.present & (1 << link) != 0;
                match (off - xbow::LINK_BASE) % xbow::LINK_STRIDE {
                    xbow::LINK_FLUSH => 0,
                    xbow::LINK_STATUS | xbow::LINK_STATUS_CLEAR => {
                        if present {
                            1 << 31
                        } else {
                            1 << 5
                        }
                    }
                    xbow::LINK_AUX_STATUS => {
                        (if present { 1 << 5 } else { 1 << 6 })
                            | (if st.eight_bit & (1 << link) != 0 {
                                1 << 4
                            } else {
                                0
                            })
                    }
                    xbow::LINK_RESET => {
                        st.status |= 32;
                        access_error = true;
                        0
                    }
                    _ => st.reg[ix(off)] as u64,
                }
            } else {
                match off {
                    xbow::STATUS => (st.status | ((port as u32) << 6)) as u64,
                    xbow::STATUS_CLEAR => {
                        let v = st.status | ((port as u32) << 6);
                        if st.status & (1 << 23) != 0 {
                            packet = Some(st.packet(false));
                        }
                        st.status &= !((1 << 23) | 0x3f);
                        v as u64
                    }
                    xbow::LOCK_CLEAR => st.reg[ix(xbow::LOCK)] as u64,
                    _ => st.reg[ix(off)] as u64,
                }
            }
        };
        if access_error && packet.is_none() {
            let mut st = self.st.lock();
            if st.reg[ix(xbow::CONTROL)] & 32 != 0 && st.status & (1 << 23) == 0 {
                st.status |= 1 << 23;
                packet = Some(st.packet(true));
            }
        }
        if let Some(p) = packet {
            self.routes.interrupt(p);
        }
        // Reset outputs may call back into the chip, so release ordering too.
        drop(_order);
        if let Some(port) = reset {
            self.reset_port(port);
        }
        result
    }
}
struct CrossbarView {
    xbow: Arc<Crossbar>,
    port: u8,
}

macro_rules! bus_view {
    ($ty:ty, $read:expr) => {
        impl BusDevice for $ty {
            fn read8(&self, a: u64) -> BusRead8 {
                BusRead8::ok(($read)(self, a, 1, None) as u8)
            }
            fn read16(&self, a: u64) -> BusRead16 {
                BusRead16::ok(($read)(self, a, 2, None) as u16)
            }
            fn read32(&self, a: u64) -> BusRead32 {
                BusRead32::ok(($read)(self, a, 4, None) as u32)
            }
            fn read64(&self, a: u64) -> BusRead64 {
                BusRead64::ok(($read)(self, a, 8, None))
            }
            fn write8(&self, a: u64, v: u8) -> u32 {
                ($read)(self, a, 1, Some(v as u64));
                BUS_OK
            }
            fn write16(&self, a: u64, v: u16) -> u32 {
                ($read)(self, a, 2, Some(v as u64));
                BUS_OK
            }
            fn write32(&self, a: u64, v: u32) -> u32 {
                ($read)(self, a, 4, Some(v as u64));
                BUS_OK
            }
            fn write64(&self, a: u64, v: u64) -> u32 {
                ($read)(self, a, 8, Some(v));
                BUS_OK
            }
        }
    };
}
bus_view!(XBridgeHead, |s: &XBridgeHead, a, n, v| s.access(a, n, v));
bus_view!(Crossbar, |s: &Crossbar, a, n, v| s.access(
    HOST_WIDGET,
    a,
    n,
    v
));
bus_view!(CrossbarView, |s: &CrossbarView, a, n, v| s
    .xbow
    .access(s.port, a, n, v));

#[cfg(test)]
mod tests;
