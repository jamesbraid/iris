// SPDX-License-Identifier: BSD-3-Clause
//! Fuel IP34 device composition and selected interpreter profile.
//!
//! Ported from hw/mips/sgi/sn1-node.c in jamesbraid/qemu-sgi-origami.

use crate::cpu::mips_cache_shadow::ShadowCache;
use crate::cpu::mips_exec::{MipsCpuConfig, MipsExecutor};
use crate::cpu::mips_tlb::MipsTlb;
use crate::ip35_bus::{Ip35Bus, CPU_PHYS_MASK};
use crate::traits::BusDevice;
use std::sync::Arc;

pub(crate) type FuelShadowCache = ShadowCache<
    32768,
    64,
    32768,
    32,
    2097152,
    128,
    true,
    0x0000_0f23,
    0x0000_0900,
    64,
    { crate::cpu::mips_cache_v2::model::R10000 },
>;
pub(crate) type FuelCpu = MipsExecutor<MipsTlb, FuelShadowCache>;

/// Select Fuel's population and identity over the existing R10000 policies.
/// This does not qualify the remaining R14000 execution or diagnostic behavior.
pub(crate) fn build_cpu(bus: Arc<Ip35Bus>) -> Result<Box<FuelCpu>, String> {
    if cfg!(feature = "opcodefusion") {
        return Err("Fuel currently requires an interpreter build without opcodefusion".into());
    }
    let cfg = MipsCpuConfig::for_model::<FuelShadowCache>();
    let mut cpu = Box::new(MipsExecutor::new(
        bus as Arc<dyn BusDevice>,
        MipsTlb::new(cfg.tlb_entries),
        &cfg,
    ));
    // The callback stores the core address: install it only after boxing.
    cpu.install_status_cb();
    cpu.phys_mask = CPU_PHYS_MASK;
    // Fuel's 500 MHz/2 MiB flash record selects these boot-mode inputs.
    // A mismatch enters the PROM's error routine before GP is initialized.
    cpu.core.cp0_config = 0x6c9a_a985;
    #[cfg(feature = "jitv2")]
    {
        cpu.jitv2_dispatch_enabled = false;
    }
    Ok(cpu)
}

/// Shared device composition for a single-processor Fuel.
/// Graphics and USB remain absent until their device implementations attach.
pub(crate) struct FuelBoard {
    pub bus: Arc<Ip35Bus>,
    pub startup: Arc<crate::ip35_bus::startup::StartupWindow>,
    pub pi: [Arc<crate::dev::bedrock_pi::BedrockPi>; 2],
    pub md: Arc<crate::dev::bedrock_md::BedrockMd>,
    pub l1: Arc<crate::dev::l1::L1>,
    pub flash: Arc<crate::dev::bedrock_flash::Flash>,
    pub lb: Arc<crate::dev::bedrock_lb::BedrockLb>,
    pub io: Arc<crate::dev::bedrock_io::BedrockIo>,
    pub ni: Arc<crate::dev::bedrock_ni::BedrockNi>,
    pub xb: Arc<crate::dev::bedrock_xb::BedrockXb>,
    pub registers: Arc<crate::physical::Physical>,
    pub xbridge: Arc<crate::dev::xbridge::XBridge>,
    pub ioc3: Arc<crate::dev::ioc3::Ioc3>,
    pub eth: Arc<crate::dev::ioc3_eth::Ioc3Eth>,
    pub scsi: Arc<crate::dev::isp12160::Isp12160>,
    pub usb: Arc<crate::dev::ohci::Ohci>,
    pub serial: [Arc<crate::dev::z85c30::CiSerialBackend>; 3],
    pub irq: Arc<PendingIrq>,
    clock: crate::dev::heart::Clock,
    last_clock: std::sync::atomic::AtomicU64,
    usb_elapsed_ns: std::sync::atomic::AtomicU64,
    last_md_clock: std::sync::atomic::AtomicU64,
    running: std::sync::atomic::AtomicBool,
}

impl FuelBoard {
    pub fn new(payload: &[u8], clock: crate::dev::heart::Clock) -> Result<Self, String> {
        Self::with_startup_home(payload, clock, payload.len() as u64)
    }

    /// Fixtures without executable payload bytes must name their execution home.
    pub fn with_startup_home(
        payload: &[u8],
        clock: crate::dev::heart::Clock,
        execution_size: u64,
    ) -> Result<Self, String> {
        Self::with_storage(payload, clock, execution_size, None)
    }

    pub fn with_storage(
        payload: &[u8],
        clock: crate::dev::heart::Clock,
        execution_size: u64,
        flash_path: Option<&std::path::Path>,
    ) -> Result<Self, String> {
        use crate::dev::bedrock_pi::{BedrockPi, BedrockPiConfig};
        let md = crate::dev::bedrock_md::BedrockMd::new();
        let phys = md.physical().clone();
        let pi: [Arc<BedrockPi>; 2] = std::array::from_fn(|interface| {
            BedrockPi::new(
                BedrockPiConfig {
                    interface: interface as u8,
                    request_region: 0,
                },
                clock.clone(),
            )
        });
        // The selected QEMU Fuel population grants its fitted CPU A the bus.
        pi[0].write64(0x50, 1);
        let l1 = Arc::new(crate::dev::l1::L1::new(crate::dev::l1::L1Config::default())?);
        let flash = Arc::new(match flash_path {
            Some(path) => crate::dev::bedrock_flash::Flash::with_file(payload, path),
            None => crate::dev::bedrock_flash::Flash::new(payload),
        }.map_err(|e| e.to_string())?);
        let lb = Arc::new(crate::dev::bedrock_lb::BedrockLb::new(clock.clone()));
        let io = Arc::new(crate::dev::bedrock_io::BedrockIo::new());
        let ni = Arc::new(crate::dev::bedrock_ni::BedrockNi::new());
        let xb = Arc::new(crate::dev::bedrock_xb::BedrockXb::new());
        let registers = crate::ip35_bus::empty_view();
        crate::ip35_bus::map_device(
            &registers,
            0,
            crate::dev::bedrock_pi::SIZE,
            pi[0].clone(),
            "Bedrock registers",
        );
        crate::ip35_bus::map_device(
            &registers,
            0x20_0000,
            crate::dev::bedrock_pi::SIZE,
            pi[1].clone(),
            "Bedrock registers",
        );
        crate::ip35_bus::map_device(
            &registers,
            0x40_0000,
            0x30010,
            io.clone(),
            "Bedrock registers",
        );
        crate::ip35_bus::map_device(
            &registers,
            0x60_0000,
            0xc040,
            lb.clone(),
            "Bedrock registers",
        );
        crate::ip35_bus::map_device(
            &registers,
            0x68_0000,
            0x2008,
            ni.clone(),
            "Bedrock registers",
        );
        crate::ip35_bus::map_device(
            &registers,
            0x70_0000,
            0x198,
            xb.clone(),
            "Bedrock registers",
        );
        crate::ip35_bus::map_device(
            &registers,
            0x78_0000,
            0x200000 - 0x180000,
            md.md_bus(),
            "Bedrock registers",
        );
        io.connect_hub(Arc::new(IoHub(Arc::downgrade(&registers), pi.clone())));
        io.connect_memory(md.dma_view());
        let p = pi.clone();
        lb.connect_error(Arc::new(move |level| {
            for pi in &p {
                pi.set_lb_error(level);
            }
        }));
        let p = pi.clone();
        ni.connect_error(Arc::new(move |level| {
            for pi in &p {
                pi.set_ni_error(level);
            }
        }));
        let p = pi.clone();
        xb.connect_error(Arc::new(move |level| {
            for pi in &p {
                pi.set_xb_error(level);
            }
        }));
        let io_node = Arc::downgrade(&io);
        lb.connect_node_changed(Arc::new(move |node| {
            if let Some(io) = io_node.upgrade() {
                io.set_node_number(node);
            }
        }));
        let p = pi[0].clone();
        md.on_error(Arc::new(move |kind| {
            if matches!(kind, crate::dev::bedrock_md::MemoryError::Uncorrectable) {
                p.set_memory_uncorrectable();
            }
        }));
        let irq = Arc::new(PendingIrq::default());
        pi[0].set_interrupt_controller(irq.clone());
        l1.set_irq(crate::irq::IrqLine::new(Arc::new(UartIrq(pi.clone())), 0));
        let serial = std::array::from_fn(|_| Arc::new(crate::dev::z85c30::CiSerialBackend::new()));
        l1.set_serial_backend(serial[0].clone());
        let xbridge = crate::dev::xbridge::XBridge::new(md.dma_view());
        for widget in [0, 10, 14, 15] {
            io.connect_widget(widget, xbridge.widget(widget).unwrap());
        }
        let weak_io = Arc::downgrade(&io);
        xbridge.set_interrupt_sender(Arc::new(move |packet| {
            if packet.widget != 10 {
                return false;
            }
            weak_io.upgrade().is_some_and(|io| {
                io.inbound_write(
                    packet.source_widget,
                    None,
                    packet.address,
                    8,
                    packet.vector as u64 | if packet.level { 1 << 8 } else { 0 },
                ) == crate::traits::BUS_OK
            })
        }));
        let weak_io = Arc::downgrade(&io);
        xbridge.set_dma_sender(Arc::new(move |packet, bytes| {
            if packet.widget != 10 || packet.half_read_requested || packet.half_write_requested {
                log::warn!(
                    "Fuel: unsupported XBridge DMA destination or halfword placement {packet:?}"
                );
                return false;
            }
            let Some(io) = weak_io.upgrade() else {
                return false;
            };
            for (i, byte) in bytes.iter_mut().enumerate() {
                let a = packet.address + i as u64;
                if packet.write {
                    if io.inbound_write(packet.source_widget, Some(packet.slot), a, 1, *byte as u64)
                        != crate::traits::BUS_OK
                    {
                        return false;
                    }
                } else {
                    let value = io.inbound_read(packet.source_widget, Some(packet.slot), a, 1);
                    if value.status != crate::traits::BUS_OK {
                        return false;
                    }
                    *byte = value.data as u8;
                }
            }
            true
        }));
        let mac = l1.station_mac();
        let ioc3 = Arc::new(crate::dev::ioc3::Ioc3::new(
            crate::dev::ioc3::Ioc3Config::fuel(mac),
        ));
        let eth = crate::dev::ioc3_eth::Ioc3Eth::new_fuel(mac);
        ioc3.attach_eth(eth.clone());
        ioc3.attach_timekeeper(Arc::new(crate::dev::timekeeper::Timekeeper::fuel()?));
        ioc3.set_serial_backend(0, serial[1].clone());
        ioc3.set_serial_backend(1, serial[2].clone());
        let scsi = Arc::new(crate::dev::isp12160::Isp12160::new());
        xbridge.attach(15, 1, scsi.clone());
        xbridge.attach(15, 4, ioc3.clone());
        let usb = crate::dev::ohci::Ohci::new();
        xbridge.attach(15, 5, usb.clone());
        for slot in [1, 4, 5] {
            xbridge.set_pci_irq(15, slot, 0, xbridge.pin_line(15, slot));
        }
        ioc3.connect_superio_interrupt(xbridge.pin_line(15, 0));
        let mut bus = Ip35Bus::new(phys.clone());
        bus.set_fallback(1, 1 << 32, Arc::new(IoPio(io.clone())));
        // The two XIO register forms select the requester's PI0 interface.
        let protected_regs = Arc::new(Protected {
            md: md.clone(),
            device: registers.clone(),
        });
        for base in [0x0100_0000, 0x0180_0000] {
            bus.map(1, base, 0x80_0000, protected_regs.clone());
        }
        // The cached plane retains the complete MD decoder. SysAd UC0 and
        // UC3 have independent maps and never re-enter this overlay plane.
        phys.map(0, 1 << 33, md.memory_view(), "Bedrock cached memory");
        bus.map(3, 0, 1 << 33, md.uncached_view());
        // Hub-special low memory: its first 256 MiB is the node memory view.
        bus.map(0, 0, 0x1000_0000, md.memory_view());
        let prom = Arc::new(
            flash
                .array_view(0, crate::dev::bedrock_flash::PART_SIZE)
                .map_err(|e| e.to_string())?,
        );
        let boot = Arc::new(flash.boot_view());
        for base in [0x1800_0000, 0x3800_0000] {
            bus.map(0, base, 0x800_0000, boot.clone());
        }
        // Reset execution always sees stored bytes, independently of commands.
        bus.map(0, 0x1fc0_0000, 0x40_0000, prom.clone());
        if !payload.is_empty() {
            crate::ip35_bus::map_device(
                &phys,
                0x1fc0_0000,
                payload.len() as u64,
                prom.clone(),
                "Fuel cached reset PROM",
            );
        }
        let leds = Arc::new(Leds {
            md: md.clone(),
            values: parking_lot::Mutex::new([0; 4]),
        });
        let uart = Arc::new(Protected {
            md: md.clone(),
            device: l1.clone(),
        });
        let head = Arc::new(HubHead(md.clone()));
        for base in [0x1000_0000, 0x3000_0000] {
            bus.map(
                0,
                base,
                1 << crate::ppmem::devmap::SLOT_SHIFT,
                Arc::new(HubSpecialPage {
                    head: head.clone(),
                    uart: uart.clone(),
                    leds: leds.clone(),
                }),
            );
            bus.map(0, base + 0x400_0000, 8, Arc::new(BoardId(md.clone())));
        }
        bus.map(0, 0x1_c000_0000, 1 << 30, md.directory_bus());
        bus.map(0, 0x1_4000_0000, 1 << 30, md.ecc_bus());
        bus.map(0, 0x1_0000_0000, 1 << 30, md.ecc_scrub_bus());
        bus.map(2, 0, 1 << 33, md.fetchop_bus());
        let startup = Arc::new(crate::ip35_bus::startup::StartupWindow::new(execution_size));
        phys.map(
            0x100000,
            0x108000,
            Arc::new(crate::ip35_bus::startup::CachedView::new(
                md.memory_view(),
                startup.clone(),
            )),
            "Fuel private startup window",
        );
        Ok(Self {
            bus: Arc::new(bus),
            startup,
            pi,
            md,
            l1,
            flash,
            lb,
            io,
            ni,
            xb,
            registers,
            xbridge,
            ioc3,
            eth,
            scsi,
            usb,
            serial,
            irq,
            last_clock: std::sync::atomic::AtomicU64::new(clock()),
            last_md_clock: std::sync::atomic::AtomicU64::new(clock()),
            usb_elapsed_ns: std::sync::atomic::AtomicU64::new(0),
            clock,
            running: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn cached_bus(&self) -> Arc<dyn BusDevice> {
        self.bus.cached_view(self.md.memory_view())
    }

    /// Construct the cached view once. Every instruction must subsequently
    /// execute through step_cpu, which publishes phase state and scopes access.
    pub fn build_cpu(&self) -> Result<Box<FuelCpu>, String> {
        let mut cpu = build_cpu(self.bus.clone())?;
        cpu.cache = FuelShadowCache::new(self.cached_bus());
        Ok(cpu)
    }

    /// CPU-thread observer. Debugger reads must never call this method.
    pub fn observe_cpu(&self, cpu: &mut FuelCpu) {
        // MBIST can complete between two consecutive register polls.
        if self.running.load(std::sync::atomic::Ordering::Acquire) {
            self.poll_md((self.clock)());
        }
        let pc = cpu.debug_translate(cpu.core.pc);
        self.startup.observe(
            cpu.core.fpr[0],
            (!pc.is_exception()).then_some(pc.phys),
            self.lb.node_number(),
        );
    }

    /// One non-fused interpreter instruction with private-window observation.
    pub fn step_cpu(&self, cpu: &mut FuelCpu) -> u32 {
        self.observe_cpu(cpu);
        let _scope = self.startup.begin_access();
        cpu.step_int()
    }

    fn poll_md(&self, now: u64) {
        let previous = self.last_md_clock.swap(now, std::sync::atomic::Ordering::AcqRel);
        self.md.advance_ns(now.saturating_sub(previous));
    }

    pub fn start(&self) {
        use crate::traits::Device;
        self.last_clock
            .store((self.clock)(), std::sync::atomic::Ordering::Release);
        self.last_md_clock.store((self.clock)(), std::sync::atomic::Ordering::Release);
        self.running
            .store(true, std::sync::atomic::Ordering::Release);
        for pi in &self.pi {
            pi.start();
        }
        self.l1.start();
        self.ioc3.start();
        self.eth.start();
        self.scsi.start();
        self.usb.start();
    }
    pub fn poll(&self) {
        if self.running.load(std::sync::atomic::Ordering::Acquire) {
            let now = (self.clock)();
            let previous = self
                .last_clock
                .swap(now, std::sync::atomic::Ordering::AcqRel);
            self.poll_md(now);
            self.flash.advance_ns(now.saturating_sub(previous));
            let delta = now.saturating_sub(previous);
            let old = self.usb_elapsed_ns.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
            let frames = (old + delta) / 1_000_000 - old / 1_000_000;
            if frames != 0 { self.usb.advance_frames(frames); }
            for pi in &self.pi {
                pi.poll();
            }
        }
    }
    pub fn stop(&self) {
        use crate::traits::Device;
        self.poll();
        self.running
            .store(false, std::sync::atomic::Ordering::Release);
        self.usb.stop();
        self.scsi.stop();
        self.eth.stop();
        self.ioc3.stop();
        self.l1.stop();
        for pi in &self.pi {
            pi.stop();
        }
    }
}

/// Break the I/O-to-registers ownership cycle while keeping the native bus
/// interface. A detached endpoint refuses accesses.
struct IoHub(
    std::sync::Weak<crate::physical::Physical>,
    [Arc<crate::dev::bedrock_pi::BedrockPi>; 2],
);
macro_rules! weak_access {
    ($r:ident,$w:ident,$ty:ty,$result:ident) => {
        fn $r(&self, a: u64) -> crate::traits::$result {
            self.0
                .upgrade()
                .map(|b| b.$r(a))
                .unwrap_or_else(crate::traits::$result::err)
        }
        fn $w(&self, a: u64, v: $ty) -> u32 {
            self.0
                .upgrade()
                .map(|b| b.$w(a, v))
                .unwrap_or(crate::traits::BUS_ERR)
        }
    };
}
impl BusDevice for IoHub {
    weak_access!(read8, write8, u8, BusRead8);
    weak_access!(read16, write16, u16, BusRead16);
    weak_access!(read32, write32, u32, BusRead32);
    fn read64(&self, a: u64) -> crate::traits::BusRead64 {
        self.0
            .upgrade()
            .map(|b| b.read64(a))
            .unwrap_or_else(crate::traits::BusRead64::err)
    }
    fn write64(&self, a: u64, v: u64) -> u32 {
        if matches!(a, 0x90 | 0x20_0090) {
            return if self.1[(a >> 21) as usize].io_modify(v, 0) {
                crate::traits::BUS_OK
            } else {
                crate::traits::BUS_ERR
            };
        }
        self.0
            .upgrade()
            .map(|b| b.write64(a, v))
            .unwrap_or(crate::traits::BUS_ERR)
    }
}

struct HubSpecialPage {
    head: Arc<HubHead>,
    uart: Arc<Protected>,
    leds: Arc<Leds>,
}
impl HubSpecialPage {
    fn target(&self, a: u64, n: u64) -> Option<(&dyn BusDevice, u64)> {
        let end = a.checked_add(n)?;
        match a {
            0..=0x7f if end <= 0x80 => Some((self.head.as_ref(), a)),
            0x80..=0xbf if end <= 0xc0 => Some((self.uart.as_ref(), a - 0x80)),
            0xc0..=0xdf if end <= 0xe0 => Some((self.leds.as_ref(), a - 0xc0)),
            _ => None,
        }
    }
}
macro_rules! hub_page_access {
    ($r:ident,$w:ident,$t:ty,$out:ident,$n:expr) => {
        fn $r(&self, a: u64) -> crate::traits::$out {
            self.target(a, $n)
                .map(|(d, at)| d.$r(at))
                .unwrap_or_else(crate::traits::$out::err)
        }
        fn $w(&self, a: u64, v: $t) -> u32 {
            self.target(a, $n)
                .map(|(d, at)| d.$w(at, v))
                .unwrap_or(crate::traits::BUS_ERR)
        }
    };
}
impl BusDevice for HubSpecialPage {
    hub_page_access!(read8, write8, u8, BusRead8, 1);
    hub_page_access!(read16, write16, u16, BusRead16, 2);
    hub_page_access!(read32, write32, u32, BusRead32, 4);
    hub_page_access!(read64, write64, u64, BusRead64, 8);
}

struct BoardId(Arc<crate::dev::bedrock_md::BedrockMd>);
impl BusDevice for BoardId {
    fn read64(&self, a: u64) -> crate::traits::BusRead64 {
        if a == 0 && self.0.current_cell() & 1 != 0 {
            crate::traits::BusRead64::ok(0x4004)
        } else {
            crate::traits::BusRead64::err()
        }
    }
}
struct HubHead(Arc<crate::dev::bedrock_md::BedrockMd>);
macro_rules! head_access {
    ($r:ident,$w:ident,$ty:ty,$result:ident) => {
        fn $r(&self, a: u64) -> crate::traits::$result {
            log::warn!(
                "Fuel: unidentified Hub-special read {} bytes at {a:#x}",
                std::mem::size_of::<$ty>()
            );
            if self.0.current_cell() & 1 != 0 {
                crate::traits::$result::ok(0)
            } else {
                crate::traits::$result::err()
            }
        }
        fn $w(&self, a: u64, v: $ty) -> u32 {
            log::warn!(
                "Fuel: unidentified Hub-special write {} bytes at {a:#x} value {v:#x}",
                std::mem::size_of::<$ty>()
            );
            if self.0.current_cell() & 1 != 0 {
                crate::traits::BUS_OK
            } else {
                crate::traits::BUS_ERR
            }
        }
    };
}
impl BusDevice for HubHead {
    head_access!(read8, write8, u8, BusRead8);
    head_access!(read16, write16, u16, BusRead16);
    head_access!(read32, write32, u32, BusRead32);
    head_access!(read64, write64, u64, BusRead64);
}
struct Leds {
    md: Arc<crate::dev::bedrock_md::BedrockMd>,
    values: parking_lot::Mutex<[u8; 4]>,
}
impl BusDevice for Leds {
    fn read64(&self, a: u64) -> crate::traits::BusRead64 {
        if a < 32 && a & 7 == 0 && self.md.current_cell() & 1 != 0 {
            crate::traits::BusRead64::ok(self.values.lock()[a as usize / 8] as u64)
        } else {
            crate::traits::BusRead64::err()
        }
    }
    fn write64(&self, a: u64, v: u64) -> u32 {
        if a < 32 && a & 7 == 0 && self.md.current_cell() & 1 != 0 {
            self.values.lock()[a as usize / 8] = v as u8;
            crate::traits::BUS_OK
        } else {
            crate::traits::BUS_ERR
        }
    }
}

struct IoPio(Arc<crate::dev::bedrock_io::BedrockIo>);
macro_rules! pio_access {
    ($r:ident,$w:ident,$ty:ty,$result:ident,$size:expr) => {
        fn $r(&self, a: u64) -> crate::traits::$result {
            let r = self.0.pio_read(a, $size);
            crate::traits::$result {
                data: r.data as $ty,
                status: r.status,
            }
        }
        fn $w(&self, a: u64, v: $ty) -> u32 {
            self.0.pio_write(a, $size, v as u64)
        }
    };
}
impl BusDevice for IoPio {
    pio_access!(read8, write8, u8, BusRead8, 1);
    pio_access!(read16, write16, u16, BusRead16, 2);
    pio_access!(read32, write32, u32, BusRead32, 4);
    pio_access!(read64, write64, u64, BusRead64, 8);
}
impl Drop for FuelBoard {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Protected {
    md: Arc<crate::dev::bedrock_md::BedrockMd>,
    device: Arc<dyn BusDevice>,
}
macro_rules! protected_access {
    ($r:ident,$w:ident,$ty:ty,$result:ident) => {
        fn $r(&self, a: u64) -> crate::traits::$result {
            if self.md.current_cell() & 1 != 0 {
                self.device.$r(a)
            } else {
                crate::traits::$result::err()
            }
        }
        fn $w(&self, a: u64, v: $ty) -> u32 {
            if self.md.current_cell() & 1 != 0 {
                self.device.$w(a, v)
            } else {
                crate::traits::BUS_ERR
            }
        }
    };
}
impl BusDevice for Protected {
    protected_access!(read8, write8, u8, BusRead8);
    protected_access!(read16, write16, u16, BusRead16);
    protected_access!(read32, write32, u32, BusRead32);
    protected_access!(read64, write64, u64, BusRead64);
}

#[derive(Default)]
pub(crate) struct PendingIrq(std::sync::atomic::AtomicU64);
impl PendingIrq {
    pub fn bits(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}
impl crate::irq::InterruptController for PendingIrq {
    fn set_line(&self, line: u32, level: bool) {
        if line >= 5 {
            return;
        }
        let mask = 1 << (10 + line);
        if level {
            self.0.fetch_or(mask, std::sync::atomic::Ordering::Release);
        } else {
            self.0
                .fetch_and(!mask, std::sync::atomic::Ordering::Release);
        }
    }
}
struct UartIrq([Arc<crate::dev::bedrock_pi::BedrockPi>; 2]);
impl crate::irq::InterruptController for UartIrq {
    fn set_line(&self, _: u32, level: bool) {
        for pi in &self.0 {
            pi.set_uart_irq(level);
        }
    }
}
