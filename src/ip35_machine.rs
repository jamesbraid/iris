//! The Fuel (IP34 board, IP35 software platform): an R14000 on Bedrock,
//! XBridge and the base I/O devices, with Iris's CPU thread, monitor and serial console.
//!
//! Selected with `[machine] profile = "fuel_ip34"`. Shared keys select the
//! PROM, NVRAM, memory and SCSI devices; `[fuel]` selects the serial console
//! and writable flash. `ip35::Ip35Bus` maps the processor views over the
//! same Physical memory banks. Graphics is not attached, and the IOC3 has
//! no keyboard or mouse ports.
//!
//! During PROM cache-as-memory bootstrap, cached startup bytes live in a
//! Fuel cache adapter until a successful cached fetch leaves the PROM homes.
//! This retirement rule is an approximation, not an established hardware event.
//!
//! The Bedrock node itself, and its approximations, are in `ip35::node`.
//!
//! Board composition ported from hw/mips/sgi/sn1-node.c
//! in jamesbraid/qemu-sgi-origami.

use crate::config::{scsi_label, FuelConsole, MachineConfig};
use crate::cpu::mips_exec::{CpuDevice, MipsCpu, MipsCpuConfig, MipsExecutor};
use crate::cpu::mips_tlb::MipsTlb;
use crate::dev::bedrock_flash::{Flash, FirmwareRecord, PART_SIZE};
use crate::dev::bedrock_io::{BedrockIo, SIZE as IO_SIZE};
use crate::dev::bedrock_lb::{BedrockLb, SIZE as LB_SIZE};
use crate::dev::bedrock_md::{BedrockMd, MemoryError};
use crate::dev::bedrock_ni::{BedrockNi, SIZE as NI_SIZE};
use crate::dev::bedrock_pi::{BedrockPi, BedrockPiConfig, Population, SIZE as PI_SIZE};
use crate::dev::bedrock_xb::{BedrockXb, SIZE as XB_SIZE};
use crate::dev::heart::{host_clock, Clock};
use crate::dev::ioc3::{Ioc3, Ioc3Config};
use crate::dev::ioc3_eth::Ioc3Eth;
use crate::dev::isp12160::Isp12160;
use crate::dev::l1::{L1, L1Config, L1Event};
use crate::dev::ohci::Ohci;
use crate::dev::usb_hid::{UsbKeyboard, UsbMouse};
use crate::dev::prom_image;
use crate::dev::ps2::Ps2Controller;
use crate::dev::serial_host::{null_serial_backend, open_log, tcp_serial_backend, CiSerialBackend, LogMode, SerialBackend, TeeBackend};
use crate::dev::timekeeper::Timekeeper;
use crate::dev::xbridge::XBridge;
use crate::devlog::{init_devlog, LogModule};
use crate::gdb_stub::CpuDebug;
use crate::gfx_display::GfxDisplay;
use crate::hptimer::TimerManager;
use crate::cpu::mips_cache_shadow::R14000ShadowCache;
use crate::cpu::mips_cache_v2::MipsCache;
use crate::ip35::node::{connect_cpu, connect_serial_ports, Node, NodeConfig, SerialPort};
#[cfg(test)]
pub(crate) use crate::ip35::node::PendingIrq;
use crate::ip35::{Ip35Bus, CPU_PHYS_MASK};
use crate::irq::{InterruptController, IrqLine};
use crate::machine_state::GuestState;
use crate::monitor::{Monitor, DEFAULT_PORT as DEFAULT_MONITOR_PORT};
use crate::net::{GatewayConfig, Link, NatControl};
use crate::perf_monitor::PerfMonitor;
use crate::ppmem::devmap::SLOT_SHIFT;
use crate::scsi::{attach_from_config, ScsiBus};
use crate::traits::{BusDevice, BusRead8, BusRead16, BusRead32, BusRead64, Device, MachineEvent, Resettable, Signal, BUS_ERR, BUS_OK};
use crate::workstation::{register_system_controller, DiscChanger, MachineRef, MachineSlot, Workstation};
use parking_lot::Mutex;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc};

pub struct FuelMachine {
    board: Arc<FuelBoard>,
    cpu: Arc<dyn CpuDevice>,
    timers: Arc<TimerManager>,
    monitor: Arc<Monitor>,
    monitor_port: u16,
    ci_serial: Option<Arc<CiSerialBackend>>,
    console: usize,
    ci: bool,
    scratch: Option<PathBuf>,
    timekeeper: Arc<Timekeeper>,
    nvram: String,
    frames: Option<Arc<AtomicU64>>,
    nat: Option<(
        Arc<NatControl>,
        (Ipv4Addr, Ipv4Addr, Ipv4Addr),
    )>,
    events: Option<Receiver<MachineEvent>>,
    slot: Option<MachineSlot>,
    lifecycle: GuestState,
}

impl FuelMachine {
    pub fn new(cfg: &MachineConfig) -> Result<Self, String> {
        cfg.validate()?;
        let prom = prom_image::load(Path::new(&cfg.prom), 8 * 1024 * 1024)
            .map_err(|e| format!("Fuel {e}"))?;
        let timekeeper = Arc::new(Timekeeper::fuel_with_nvram_file(
            Path::new(&cfg.nvram),
        )?);
        let board = Arc::new(FuelBoard::with_config(FuelBoardConfig {
            payload: &prom,
            clock: host_clock(),
            flash_path: cfg.fuel.flash.as_deref().map(Path::new),
            station_mac: cfg.network().mac,
            timekeeper: timekeeper.clone(),
            ps2_input: cfg.fuel.ps2_input,
            usb_input: cfg.fuel.usb_input,
        })?);
        let timers = Arc::new(TimerManager::new());
        let mut exec = board.build_cpu();
        exec.core.set_timer_manager(timers.clone());
        exec.core.set_count_hz(
            cfg.clock
                .fixed_mhz
                .map_or(250_000_000, |mhz| (mhz * 1_000_000.0) as u64)
                .max(1),
        );
        exec.core.count_write_samples = true;
        let poll_board = board.clone();
        exec.batch_hook = Some(Box::new(move |_| poll_board.poll()));
        let cpu: Arc<dyn CpuDevice> = Arc::new(MipsCpu::<_, R14000ShadowCache>::new(*exec));
        let rx = connect_cpu("Fuel", &board.md, &board.pi[0], &board.lb, &board.ni, &board.l1, &cpu);
        // After the windows above: each compile worker takes the cache's
        // geometry and the CPU model's policies once, as it starts, and
        // without the windows the geometry has no inline load or store path.
        // Workers read plain physical code addresses, which the cached plane
        // serves; the uncached views would decode them as Hub registers.
        #[cfg(feature = "jitv2")]
        {
            let cpu_device: Arc<dyn Device> = cpu.clone().as_device();
            let jit = cpu.jitv2();
            let mut g = jit.lock();
            g.compile_queue.set_thread_count(cfg.jitv2.threads.max(1));
            g.compile_queue.set_cpu(Arc::downgrade(&cpu_device));
            g.compile_queue.set_owner(Arc::downgrade(&jit));
            // Not under lockstep, as on the other machines: lockstep compiles
            // inline on the CPU thread, and a running pool both owns the
            // Codegen inline needs and deadlocks the CPU thread's flush.
            #[cfg(not(feature = "jitv2_lockstep"))]
            {
                let stats = g.stats.clone();
                g.compile_queue.start(board.cached_bus(), stats);
            }
            // The compile pool keeps only a weak handle on the CPU.
            std::mem::forget(cpu_device);
        }
        let scratch = Self::attach_scsi(cfg, &board)?;
        let console = match cfg.fuel.console {
            FuelConsole::L1 => 0,
            FuelConsole::Ioc3A => 1,
            FuelConsole::Ioc3B => 2,
        };
        let ci_serial = cfg.ci.then(|| board.serial[console].clone());
        Self::connect_serial(cfg, &board, console)?;
        let monitor = Monitor::new();
        monitor.register_device(init_devlog());
        monitor.register_device(Arc::new(crate::cpu::cpu_monitor::CpuMonitor::new(vec![cpu.clone()])));
        monitor.register_device(PerfMonitor::new(
            cpu.running_flag(), cpu.cycles_ptr(), cpu.fasttick_count(), None, None,
        ));
        monitor.register_device(board.md.physical().clone());
        monitor.register_device(board.l1.clone());
        monitor.register_device(board.scsi.clone());
        // `ps2 type` and friends: the keyboard and mouse from the monitor.
        monitor.register_device(board.ioc3.ps2());
        cpu.register_locks();
        board.scsi.register_locks();
        board.ioc3.register_locks();
        board.xbridge.register_locks();
        board.flash.register_locks();
        board.md.register_locks();
        let (mut nat, mut frames) = (None, None);
        let net = GatewayConfig::from_network(&cfg.network());
        if let Some(link) = Link::with_gateway(net.clone())? {
            frames = Some(link.frames());
            nat = Some((link.nat(), (net.client_ip, net.gateway_ip, net.netmask)));
            board.eth.attach(link);
        }
        eprintln!(
            "iris: Fuel IP34, R14000 500 MHz, 512 MiB, serial console {:?}",
            cfg.fuel.console
        );
        Ok(Self {
            board,
            cpu,
            timers,
            monitor: Arc::new(monitor),
            monitor_port: cfg.monitor_port.unwrap_or(DEFAULT_MONITOR_PORT),
            ci_serial,
            console,
            ci: cfg.ci,
            scratch,
            timekeeper,
            nvram: cfg.nvram.clone(),
            nat,
            frames,
            events: Some(rx),
            slot: None,
            lifecycle: GuestState::Idle,
        })
    }

    fn attach_scsi(cfg: &MachineConfig, board: &FuelBoard) -> Result<Option<PathBuf>, String> {
        let attached = attach_from_config(cfg, Some("fuel"), &|channel| {
            board.scsi.bus_for_channel(channel)
                .ok_or_else(|| format!("controller {channel} (a Fuel has channels 0 and 1)"))
        })?;
        if let Some(device) = attached.daynaports.first() {
            return Err(format!("{}: kind = \"daynaport\" is not available on the Fuel",
                scsi_label(device.controller, device.id)));
        }
        Ok(attached.scratch)
    }

    fn connect_serial(
        cfg: &MachineConfig,
        board: &FuelBoard,
        console: usize,
    ) -> Result<(), String> {
        let [a, b] = cfg.serial_ports();
        let ports = [
            SerialPort {
                label: "L1",
                port: cfg.fuel.l1_port.unwrap_or(8882),
                setting: "fuel.l1_port",
                ci: &board.serial[0],
                log: cfg.fuel.l1_log.clone(),
                connect: Box::new(|backend| board.l1.set_serial_backend(backend)),
            },
            SerialPort {
                label: "IOC3 A",
                port: a,
                setting: "serial_port_a",
                ci: &board.serial[1],
                log: None,
                connect: Box::new(|backend| board.ioc3.set_serial_backend(0, backend)),
            },
            SerialPort {
                label: "IOC3 B",
                port: b,
                setting: "serial_port_b",
                ci: &board.serial[2],
                log: None,
                connect: Box::new(|backend| board.ioc3.set_serial_backend(1, backend)),
            },
        ];
        connect_serial_ports(cfg, "Fuel", &ports, console)
    }

    #[cfg(test)]
    pub(crate) fn connect_serial_for_test(cfg: &MachineConfig, board: &FuelBoard, console: usize) -> Result<(), String> {
        Self::connect_serial(cfg, board, console)
    }

}
impl Workstation for FuelMachine {
    fn start(&mut self) {
        self.lifecycle = GuestState::Starting;
        self.monitor
            .clone()
            .start_server(format!("127.0.0.1:{}", self.monitor_port));
        self.restart_peripherals();
        if !self.ci {
            self.cpu_start();
        }
    }
    fn stop(&mut self) {
        self.cpu.clone().as_device().stop();
        self.board.stop();
        if let Err(e) = self.timekeeper.flush_nvram() {
            dlog!(LogModule::Bedrock, "Fuel NVRAM: {e}");
        }
        if self.lifecycle != GuestState::Idle {
            self.lifecycle = GuestState::Halted;
        }
    }
    fn reset(&mut self) {
        self.stop();
        self.cpu.power_on();
        for pi in &self.board.pi {
            pi.power_on();
        }
        self.board.md.power_on();
        self.board.lb.power_on();
        self.board.io.power_on();
        self.board.ni.power_on();
        self.board.xb.power_on();
        self.board.l1.power_on();
        self.board.flash.power_on();
        self.board.xbridge.power_on();
        self.board.ioc3.power_on();
        self.board.eth.power_on();
        self.board.scsi.power_on();
        self.board.usb.power_on();
        self.board.usb_elapsed_ns.store(0, Ordering::Relaxed);
        self.timekeeper.power_on();
        self.restart_peripherals();
    }
    fn restart_peripherals(&mut self) {
        self.board.start();
    }
    fn cpu_start(&self) {
        self.cpu.clone().as_device().start();
    }
    fn cpu_is_running(&self) -> bool {
        self.cpu.clone().as_device().is_running()
    }
    fn machine_slot(&mut self) -> MachineSlot {
        let ptr = self as *mut Self as *mut dyn Workstation;
        self.slot
            .get_or_insert_with(|| Arc::new(Mutex::new(MachineRef(ptr))))
            .clone()
    }
    fn register_system_controller(&mut self) {
        let slot = self.machine_slot();
        let events = self
            .events
            .take()
            .expect("Fuel system controller registered twice");
        register_system_controller(&self.monitor, slot, Some(events));
    }
    fn monitor_addr(&self) -> Option<SocketAddr> {
        self.monitor.bound_addr()
    }
    fn lifecycle(&self) -> GuestState {
        self.lifecycle
    }
    fn set_lifecycle(&mut self, state: GuestState) {
        self.lifecycle = state;
    }
    fn get_display(&self) -> Option<Arc<dyn GfxDisplay>> {
        None
    }
    fn register_monitor_device(&self, device: Arc<dyn Device>) {
        self.monitor.register_device(device);
    }
    fn get_ps2(&self) -> Arc<Ps2Controller> {
        self.board.ioc3.ps2()
    }
    fn get_timer_manager(&self) -> Arc<TimerManager> {
        self.timers.clone()
    }
    fn get_cpu_debug(&self) -> Arc<dyn CpuDebug> {
        self.cpu.clone().debug_adapter()
    }
    fn fasttick_count(&self) -> Arc<AtomicU64> {
        self.cpu.fasttick_count()
    }
    fn get_ci_serial(&self) -> Option<Arc<CiSerialBackend>> {
        self.ci_serial.clone()
    }
    fn inject_serial_console(&self, bytes: &[u8]) {
        self.board.serial[self.console].push_host(bytes);
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn save_nvram(&self, path: Option<&str>) -> Result<String, String> {
        if path.is_some_and(|p| p != self.nvram) {
            return Err("Fuel NVRAM save uses the configured path".into());
        }
        self.timekeeper.flush_nvram()?;
        Ok(self.nvram.clone())
    }
    fn scratch_path(&self) -> Option<&Path> {
        self.scratch.as_deref()
    }
    fn disc_changer(&self) -> Option<Arc<dyn DiscChanger>> {
        Some(self.board.scsi.clone() as Arc<dyn DiscChanger>)
    }
    fn pending_chd_sync_count(&self) -> usize {
        (0..2).map(|channel| self.board.scsi.bus_for_channel(channel).unwrap().pending_chd_sync_count()).sum()
    }
    fn sync_chd_disks(
        &self,
        only: Option<usize>,
        progress: &mut dyn FnMut(usize, usize, f32),
        cancel: &dyn Fn() -> bool,
    ) -> std::io::Result<usize> {
        let buses: Vec<_> = (0..2).map(|channel| self.board.scsi.bus_for_channel(channel).unwrap()).collect();
        ScsiBus::sync_chd_disks(&buses, only, progress, cancel)
    }
    fn net_frames(&self) -> Option<Arc<AtomicU64>> { self.frames.clone() }
    fn nat_control(&self) -> Option<Arc<NatControl>> {
        self.nat.as_ref().map(|(n, _)| n.clone())
    }
    fn nat_expected(&self) -> Option<(Ipv4Addr, Ipv4Addr, Ipv4Addr)> {
        self.nat.as_ref().map(|(_, a)| *a)
    }
}

impl Drop for FuelMachine {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.lock().0 = MachineRef::null();
        }
        self.stop();
        self.monitor.shutdown();
    }
}

pub(crate) type FuelCpu = MipsExecutor<MipsTlb, R14000ShadowCache>;

const FUEL_BOARD_ID: u64 = 0x4004;
/// Two 256 MiB banks, locational banks 0 and 1.
const FUEL_BANKS_MIB: [u32; 2] = [256, 256];


/// Select the named R14000 model and the Fuel PROM boot-mode inputs.
pub(crate) fn build_cpu(bus: Arc<Ip35Bus>) -> Box<FuelCpu> {
    let cfg = MipsCpuConfig::for_model::<R14000ShadowCache>();
    let mut cpu = Box::new(MipsExecutor::new(
        bus as Arc<dyn BusDevice>,
        MipsTlb::new(cfg.tlb_entries),
        &cfg,
    ));
    cpu.phys_mask = CPU_PHYS_MASK;
    // Fuel's 500 MHz/2 MiB flash record selects these boot-mode inputs.
    // A mismatch enters the PROM's error routine before GP is initialized.
    cpu.core.cp0_config = FirmwareRecord::FUEL.reset_config0();
    cpu
}

/// Shared device composition for a single-processor Fuel.
/// Graphics is absent; the serial workstation shares these base I/O devices.
pub(crate) struct FuelBoard {
    pub bus: Arc<Ip35Bus>,
    pub pi: [Arc<BedrockPi>; 2],
    pub md: Arc<BedrockMd>,
    pub l1: Arc<L1>,
    pub flash: Arc<Flash>,
    pub lb: Arc<BedrockLb>,
    pub io: Arc<BedrockIo>,
    pub ni: Arc<BedrockNi>,
    pub xb: Arc<BedrockXb>,
    pub xbridge: Arc<XBridge>,
    pub ioc3: Arc<Ioc3>,
    pub eth: Arc<Ioc3Eth>,
    pub scsi: Arc<Isp12160>,
    pub usb: Arc<Ohci>,
    pub serial: [Arc<CiSerialBackend>; 3],
    #[cfg(test)]
    pub irq: Arc<PendingIrq>,
    clock: Clock,
    last_clock: AtomicU64,
    usb_elapsed_ns: AtomicU64,
    running: AtomicBool,
}

pub(crate) struct FuelBoardConfig<'a> {
    pub payload: &'a [u8],
    pub clock: Clock,
    pub flash_path: Option<&'a Path>,
    pub station_mac: [u8; 6],
    pub timekeeper: Arc<Timekeeper>,
    /// The IOC3's PS/2 keyboard and mouse controller.
    pub ps2_input: bool,
    /// A USB keyboard and mouse on the OHCI root ports.
    pub usb_input: bool,
}

impl FuelBoard {
    #[cfg(test)]
    pub fn new(payload: &[u8], clock: Clock) -> Result<Self, String> {
        Self::with_config(FuelBoardConfig {
            payload, clock, flash_path: None,
            station_mac: L1Config::default().mac,
            timekeeper: Arc::new(Timekeeper::fuel()?),
            ps2_input: true,
            usb_input: false,
        })
    }

    pub fn with_config(config: FuelBoardConfig<'_>) -> Result<Self, String> {
        let FuelBoardConfig { payload, clock, flash_path, station_mac, timekeeper, ps2_input, usb_input } = config;
        let node = Node::build(NodeConfig {
            name: "Fuel",
            payload,
            clock: clock.clone(),
            flash_path,
            banks_mib: &FUEL_BANKS_MIB,
            population: Population::SINGLE,
            firmware: FirmwareRecord::FUEL,
            board_id: FUEL_BOARD_ID,
            l1: L1Config { mac: station_mac, ..Default::default() },
        })?;
        let (md, io) = (&node.md, &node.io);
        let serial = std::array::from_fn(|_| Arc::new(CiSerialBackend::new()));
        node.l1.set_serial_backend(serial[0].clone());
        let xbridge = XBridge::new(md.dma_view());
        for widget in [0, 10, 14, 15] {
            io.connect_widget(widget, xbridge.widget(widget).unwrap());
        }
        let weak_io = Arc::downgrade(io);
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
                ) == BUS_OK
            })
        }));
        let weak_io = Arc::downgrade(io);
        xbridge.set_dma_sender(Arc::new(move |packet, bytes| {
            if packet.widget != 10 || packet.half_read_requested || packet.half_write_requested {
                dlog!(LogModule::Bedrock,
                    "Fuel: unsupported XBridge DMA destination or halfword placement {packet:?}"
                );
                return false;
            }
            let Some(io) = weak_io.upgrade() else {
                return false;
            };
            // Preserve the width of XBridge's naturally aligned packet legs,
            // including Hub registers whose narrower accesses are refused.
            let size = bytes.len();
            if matches!(size, 1 | 2 | 4 | 8)
                && packet.address & (size as u64 - 1) == 0
            {
                if packet.write {
                    let value = bytes.iter().fold(0u64, |value, byte| (value << 8) | *byte as u64);
                    return io.inbound_write(
                        packet.source_widget, Some(packet.slot), packet.address, size as u8, value,
                    ) == BUS_OK;
                }
                let value = io.inbound_read(
                    packet.source_widget, Some(packet.slot), packet.address, size as u8,
                );
                if value.status != BUS_OK {
                    return false;
                }
                bytes.copy_from_slice(&value.data.to_be_bytes()[8 - size..]);
                return true;
            }
            for (i, byte) in bytes.iter_mut().enumerate() {
                let a = packet.address + i as u64;
                if packet.write {
                    if io.inbound_write(packet.source_widget, Some(packet.slot), a, 1, *byte as u64)
                        != BUS_OK
                    {
                        return false;
                    }
                } else {
                    let value = io.inbound_read(packet.source_widget, Some(packet.slot), a, 1);
                    if value.status != BUS_OK {
                        return false;
                    }
                    *byte = value.data as u8;
                }
            }
            true
        }));
        let mac = node.l1.station_mac();
        let mut ioc3_config = Ioc3Config::fuel(mac);
        ioc3_config.keyboard_mouse = ps2_input;
        let ioc3 = Arc::new(Ioc3::new(ioc3_config));
        let eth = Ioc3Eth::new_fuel(mac);
        ioc3.attach_eth(eth.clone());
        ioc3.attach_timekeeper(timekeeper);
        ioc3.set_serial_backend(0, serial[1].clone());
        ioc3.set_serial_backend(1, serial[2].clone());
        let scsi = Arc::new(Isp12160::new(
            Isp12160::new_bus(),
            Isp12160::new_bus(),
        ));
        xbridge.attach(15, 1, scsi.clone());
        xbridge.attach(15, 4, ioc3.clone());
        let usb = Ohci::new();
        if usb_input {
            // Host input reaches the USB devices through the same controller
            // object the GUI, VNC and CI already drive; each device drops
            // what its guest has not enabled.
            let keyboard = UsbKeyboard::new();
            let mouse = UsbMouse::new();
            usb.attach(0, keyboard.clone())?;
            usb.attach(1, mouse.clone())?;
            ioc3.ps2().add_sink(keyboard);
            ioc3.ps2().add_sink(mouse);
        }
        xbridge.attach(15, 5, usb.clone());
        for slot in [1, 4, 5] {
            xbridge.set_pci_irq(15, slot, 0, xbridge.pin_line(15, slot));
        }
        ioc3.connect_superio_interrupt(xbridge.pin_line(15, 0));
        let reset_ioc3 = ioc3.clone();
        let reset_eth = eth.clone();
        let reset_scsi = scsi.clone();
        let reset_usb = usb.clone();
        xbridge.set_reset_output(Arc::new(move |port| {
            if port == 15 {
                reset_ioc3.power_on();
                reset_eth.power_on();
                reset_scsi.power_on();
                reset_usb.power_on();
            }
        }));
        Ok(Self {
            bus: node.bus,
            pi: node.pi,
            md: node.md,
            l1: node.l1,
            flash: node.flash,
            lb: node.lb,
            io: node.io,
            ni: node.ni,
            xb: node.xb,
            xbridge,
            ioc3,
            eth,
            scsi,
            usb,
            serial,
            #[cfg(test)]
            irq: node.irq,
            last_clock: AtomicU64::new(clock()),
            usb_elapsed_ns: AtomicU64::new(0),
            clock,
            running: AtomicBool::new(false),
        })
    }

    pub fn cached_bus(&self) -> Arc<dyn BusDevice> {
        self.bus.cached_view(self.md.dma_view(), self.md.ram_extent())
    }

    pub fn build_cpu(&self) -> Box<FuelCpu> {
        let mut cpu = build_cpu(self.bus.clone());
        cpu.cache = R14000ShadowCache::new(self.cached_bus());
        // The PROM keeps its stack in cache lines it tags for a window with no
        // memory behind it yet; the controller answers that window while they stand.
        let memory = self.md.clone();
        cpu.cache.set_pd_tag_store_hook(Box::new(move |tag| memory.startup_tag_store(tag)));
        cpu
    }

    pub fn start(&self) {
        self.last_clock
            .store((self.clock)(), Ordering::Release);
        self.md.start_clock();
        self.running
            .store(true, Ordering::Release);
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
        if self.running.load(Ordering::Acquire) {
            let now = (self.clock)();
            let previous = self
                .last_clock
                .swap(now, Ordering::AcqRel);
            self.flash.advance_ns(now.saturating_sub(previous));
            let delta = now.saturating_sub(previous);
            let old = self.usb_elapsed_ns.fetch_add(delta, Ordering::Relaxed);
            let frames = (old + delta) / 1_000_000 - old / 1_000_000;
            if frames != 0 { self.usb.advance_frames(frames); }
            for pi in &self.pi {
                pi.poll();
            }
        }
    }
    pub fn stop(&self) {
        self.poll();
        self.running
            .store(false, Ordering::Release);
        self.md.stop_clock();
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

impl Drop for FuelBoard {
    fn drop(&mut self) {
        self.stop();
    }
}

