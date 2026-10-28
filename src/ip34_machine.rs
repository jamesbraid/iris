// SPDX-License-Identifier: BSD-3-Clause
//! Fuel interpreter construction. The machine is not yet a public selector.
//!
//! Ported from hw/mips/sgi/sn1-node.c in jamesbraid/qemu-sgi-origami.

use crate::cpu::mips_cache_shadow::ShadowCache;
use crate::cpu::mips_exec::{MipsCpuConfig, MipsExecutor};
use crate::cpu::mips_tlb::MipsTlb;
use crate::ip34::{Ip34Bus, CPU_PHYS_MASK};
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
pub(crate) fn build_cpu(bus: Arc<Ip34Bus>) -> Result<Box<FuelCpu>, String> {
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

/// Private bring-up composition for a single-processor Fuel.
/// Graphics and USB remain absent until their device implementations attach.
pub(crate) struct FuelBoard {
    pub bus: Arc<Ip34Bus>,
    pub pi: [Arc<crate::dev::bedrock_pi::BedrockPi>; 2],
    pub md: Arc<crate::dev::bedrock_md::BedrockMd>,
    pub l1: Arc<crate::dev::l1::L1>,
    pub flash: Arc<crate::dev::bedrock_flash::Flash>,
    pub lb: Arc<crate::dev::bedrock_lb::BedrockLb>,
    pub io: Arc<crate::dev::bedrock_io::BedrockIo>,
    pub ni: Arc<crate::dev::bedrock_ni::BedrockNi>,
    pub xb: Arc<crate::dev::bedrock_xb::BedrockXb>,
    pub registers: Arc<Ip34Bus>,
    pub xbridge: Arc<crate::dev::xbridge::XBridge>,
    pub ioc3: Arc<crate::dev::ioc3::Ioc3>,
    pub eth: Arc<crate::dev::ioc3_eth::Ioc3Eth>,
    pub scsi: Arc<crate::dev::isp12160::Isp12160>,
    pub serial: [Arc<crate::dev::z85c30::CiSerialBackend>; 3],
    pub irq: Arc<PendingIrq>,
    clock: crate::dev::heart::Clock,
    last_clock: std::sync::atomic::AtomicU64,
    running: std::sync::atomic::AtomicBool,
}

impl FuelBoard {
    pub fn new(payload: &[u8], clock: crate::dev::heart::Clock) -> Result<Self, String> {
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
        let l1 = Arc::new(crate::dev::l1::L1::new(crate::dev::l1::L1Config::default())?);
        let flash =
            Arc::new(crate::dev::bedrock_flash::Flash::new(payload).map_err(|e| e.to_string())?);
        let lb = Arc::new(crate::dev::bedrock_lb::BedrockLb::new(clock.clone()));
        let io = Arc::new(crate::dev::bedrock_io::BedrockIo::new());
        let ni = Arc::new(crate::dev::bedrock_ni::BedrockNi::new());
        let xb = Arc::new(crate::dev::bedrock_xb::BedrockXb::new());
        let mut registers = Ip34Bus::new(phys.clone());
        registers.map(0, 0, crate::dev::bedrock_pi::SIZE, pi[0].clone());
        registers.map(0, 0x20_0000, crate::dev::bedrock_pi::SIZE, pi[1].clone());
        registers.map(0, 0x40_0000, 0x30010, io.clone());
        registers.map(0, 0x60_0000, 0xc040, lb.clone());
        registers.map(0, 0x68_0000, 0x2008, ni.clone());
        registers.map(0, 0x70_0000, 0x198, xb.clone());
        registers.map(0, 0x78_0000, 0x200000 - 0x180000, md.md_bus());
        let registers = Arc::new(registers);
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
        ioc3.connect_superio_interrupt(xbridge.pin_line(15, 0));
        let mut bus = Ip34Bus::new(phys.clone());
        bus.set_fallback(1, 1 << 32, Arc::new(IoPio(io.clone())));
        // The two XIO register forms select the requester's PI0 interface.
        let protected_regs = Arc::new(Protected {
            md: md.clone(),
            device: registers.clone(),
        });
        for base in [0x0100_0000, 0x0180_0000] {
            bus.map(1, base, 0x80_0000, protected_regs.clone());
        }
        let ram: Arc<dyn BusDevice> = phys;
        bus.map(3, 0, 1 << 33, md.uncached_view());
        // Hub-special low memory: its first 256 MiB is the node memory view.
        bus.map(0, 0, 0x1000_0000, ram);
        let prom = Arc::new(
            flash
                .array_view(0, crate::dev::bedrock_flash::PART_SIZE)
                .map_err(|e| e.to_string())?,
        );
        for base in [0x1800_0000, 0x1fc0_0000, 0x3800_0000] {
            let len = if base == 0x1fc0_0000 {
                0x40_0000
            } else {
                0x80_0000
            };
            bus.map(0, base, len, prom.clone());
        }
        let uart = Arc::new(Protected {
            md: md.clone(),
            device: l1.clone(),
        });
        for base in [0x1000_0080, 0x3000_0080] {
            bus.map(0, base, 0x40, uart.clone());
        }
        let board_id = Arc::new(BoardId(md.clone()));
        let head = Arc::new(HubHead(md.clone()));
        let leds = Arc::new(Leds {
            md: md.clone(),
            values: parking_lot::Mutex::new([0; 4]),
        });
        for base in [0x1000_0000, 0x3000_0000] {
            bus.map(0, base, 0x80, head.clone());
            bus.map(0, base + 0xc0, 0x20, leds.clone());
            bus.map(0, base + 0x400_0000, 8, board_id.clone());
        }
        let second = Arc::new(
            flash
                .array_view(
                    crate::dev::bedrock_flash::PART_SIZE,
                    crate::dev::bedrock_flash::PART_SIZE,
                )
                .map_err(|e| e.to_string())?,
        );
        for base in [0x1a00_0000, 0x3a00_0000] {
            bus.map(0, base, 0x80_0000, second.clone());
        }
        bus.map(0, 0x1_c000_0000, 1 << 29, md.directory_bus());
        bus.map(0, 0x1_4000_0000, 1 << 30, md.ecc_bus());
        bus.map(0, 0x1_0000_0000, 1 << 30, md.ecc_scrub_bus());
        bus.map(2, 0, 1 << 33, md.fetchop_bus());
        Ok(Self {
            bus: Arc::new(bus),
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
            serial,
            irq,
            last_clock: std::sync::atomic::AtomicU64::new(clock()),
            clock,
            running: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn start(&self) {
        use crate::traits::Device;
        self.last_clock
            .store((self.clock)(), std::sync::atomic::Ordering::Release);
        self.running
            .store(true, std::sync::atomic::Ordering::Release);
        for pi in &self.pi {
            pi.start();
        }
        self.l1.start();
        self.ioc3.start();
        self.eth.start();
        self.scsi.start();
    }
    pub fn poll(&self) {
        if self.running.load(std::sync::atomic::Ordering::Acquire) {
            let now = (self.clock)();
            let previous = self
                .last_clock
                .swap(now, std::sync::atomic::Ordering::AcqRel);
            self.md.advance_ns(now.saturating_sub(previous));
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
    std::sync::Weak<Ip34Bus>,
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
