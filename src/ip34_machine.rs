//! The Fuel (IP34) as a machine: an R14000 on Bedrock, XBridge and the
//! base I/O devices, with Iris's CPU thread, monitor and serial console.
//!
//! Selected with `[machine] profile = "fuel_ip34"`. Shared keys select the
//! PROM, NVRAM, memory and SCSI devices; `[fuel]` selects the serial console
//! and writable flash. `ip35::Ip35Bus` maps the processor views over the
//! same Physical memory banks. Graphics is not attached; PS/2 ports are empty unless configured.
//!
//! Board composition ported from hw/mips/sgi/sn1-node.c
//! in jamesbraid/qemu-sgi-origami.

use crate::config::{FuelConsole, MachineConfig};
use crate::cpu::mips_cache_shadow::ShadowCache;
use crate::cpu::mips_exec::{CpuDevice, MipsCpu, MipsCpuConfig, MipsExecutor};
use crate::cpu::mips_tlb::MipsTlb;
use crate::hptimer::TimerManager;
use crate::ip35::{Ip35Bus, CPU_PHYS_MASK};
use crate::traits::{BusDevice, Device, MachineEvent, Resettable, Signal};
use crate::workstation::{MachineRef, MachineSlot, Workstation};
use std::sync::{atomic::AtomicU64, Arc};

pub struct FuelMachine {
    board: Arc<FuelBoard>,
    cpu: Arc<dyn CpuDevice>,
    timers: Arc<TimerManager>,
    monitor: Arc<crate::monitor::Monitor>,
    monitor_port: u16,
    ci_serial: Option<Arc<crate::dev::z85c30::CiSerialBackend>>,
    console: usize,
    ci: bool,
    scratch: Option<std::path::PathBuf>,
    timekeeper: Arc<crate::dev::timekeeper::Timekeeper>,
    nvram: String,
    nat: Option<(
        Arc<crate::net::NatControl>,
        (std::net::Ipv4Addr, std::net::Ipv4Addr, std::net::Ipv4Addr),
    )>,
    events: Option<std::sync::mpsc::Receiver<MachineEvent>>,
    slot: Option<MachineSlot>,
    lifecycle: crate::machine_state::GuestState,
}

impl FuelMachine {
    pub fn new(cfg: &MachineConfig) -> Result<Self, String> {
        cfg.validate()?;
        let prom = crate::dev::prom_image::load(std::path::Path::new(&cfg.prom), 8 * 1024 * 1024)
            .map_err(|e| format!("Fuel {e}"))?;
        let board = Arc::new(FuelBoard::with_storage(
            &prom,
            crate::dev::heart::host_clock(),
            prom.len() as u64,
            cfg.fuel.flash.as_deref().map(std::path::Path::new),
            cfg.network().mac,
            [cfg.fuel.ps2_keyboard, cfg.fuel.ps2_mouse],
        )?);
        let timekeeper = Arc::new(crate::dev::timekeeper::Timekeeper::fuel_with_nvram_file(
            std::path::Path::new(&cfg.nvram),
        )?);
        board.ioc3.attach_timekeeper(timekeeper.clone());
        let timers = Arc::new(TimerManager::new());
        let mut exec = board.build_cpu()?;
        exec.core.set_timer_manager(timers.clone());
        exec.core.set_count_hz(
            cfg.clock
                .fixed_mhz
                .map_or(250_000_000, |mhz| (mhz * 1_000_000.0) as u64)
                .max(1),
        );
        exec.core.count_write_samples = true;
        let step_board = board.clone();
        exec.instruction_hook_enabled = Some(board.startup.enabled());
        exec.instruction_hook = Some(Arc::new(move |exec, step| {
            step_board.observe_cpu(exec);
            let _scope = step_board.startup.begin_access();
            step(exec)
        }));
        let poll_board = board.clone();
        exec.batch_hook = Some(Box::new(move |_| poll_board.poll()));
        let cpu: Arc<dyn CpuDevice> = Arc::new(MipsCpu::<_, FuelShadowCache>::new(*exec));
        board.pi[0].set_interrupt_controller(Arc::new(CpuIrq(Arc::downgrade(&cpu))));
        let (tx, rx) = std::sync::mpsc::channel();
        let reset_tx = tx.clone();
        board.lb.connect_soft_reset(Arc::new(move || {
            let _ = reset_tx.send(MachineEvent::HardReset);
        }));
        let reset_tx = tx.clone();
        board.ni.connect_local_reset(Arc::new(move || {
            let _ = reset_tx.send(MachineEvent::HardReset);
        }));
        let weak_cpu = Arc::downgrade(&cpu);
        board.l1.set_event_handler(Arc::new(move |event| {
            use crate::dev::l1::L1Event;
            match event {
                L1Event::Power(false) | L1Event::Shutdown => {
                    let _ = tx.send(MachineEvent::PowerOff);
                }
                L1Event::Power(true) | L1Event::Reset => {
                    let _ = tx.send(MachineEvent::HardReset);
                }
                L1Event::SoftReset => {
                    if let Some(cpu) = weak_cpu.upgrade() {
                        cpu.as_device().signal(Signal::Reset(true));
                    }
                }
                L1Event::Nmi => log::warn!("Fuel: NMI entry is not implemented by the CPU"),
            }
        }));
        let scratch = Self::attach_scsi(cfg, &board)?;
        let console = match cfg.fuel.console {
            FuelConsole::L1 => 0,
            FuelConsole::Ioc3A => 1,
            FuelConsole::Ioc3B => 2,
        };
        let ci_serial = cfg.ci.then(|| board.serial[console].clone());
        Self::connect_serial(cfg, &board, console)?;
        let monitor = crate::monitor::Monitor::new();
        monitor.register_device(crate::devlog::init_devlog());
        monitor.register_device(cpu.clone().as_device());
        monitor.register_device(crate::perf_monitor::PerfMonitor::new(
            cpu.running_flag(), cpu.cycles_ptr(), cpu.fasttick_count(), None, None,
        ));
        monitor.register_device(board.md.physical().clone());
        monitor.register_device(board.ioc3.ps2());
        monitor.register_device(board.l1.clone());
        monitor.register_device(board.scsi.clone());
        cpu.register_locks();
        board.scsi.register_locks();
        board.ioc3.register_locks();
        board.xbridge.register_locks();
        board.flash.register_locks();
        board.md.register_locks();
        board.startup.register_locks();
        let network_up = cfg.network.mode != crate::config::NetMode::Off;
        board.eth.set_backend_link(network_up);
        let nat = if network_up {
            let net = crate::net::GatewayConfig::from_network(&cfg.network());
            let addrs = (net.client_ip, net.gateway_ip, net.netmask);
            Some((board.eth.start_nat(net), addrs))
        } else {
            None
        };
        eprintln!(
            "iris: experimental Fuel IP34, R14000 500 MHz, 512 MiB, serial console {:?}",
            cfg.fuel.console
        );
        Ok(Self {
            board,
            cpu,
            timers,
            monitor: Arc::new(monitor),
            monitor_port: cfg.monitor_port.unwrap_or(crate::monitor::DEFAULT_PORT),
            ci_serial,
            console,
            ci: cfg.ci,
            scratch,
            timekeeper,
            nvram: cfg.nvram.clone(),
            nat,
            events: Some(rx),
            slot: None,
            lifecycle: crate::machine_state::GuestState::Idle,
        })
    }

    fn attach_scsi(
        cfg: &MachineConfig,
        board: &FuelBoard,
    ) -> Result<Option<std::path::PathBuf>, String> {
        let mut scratch = None;
        let mut keys: Vec<_> = cfg.scsi.keys().copied().collect();
        keys.sort();
        for key in keys {
            let dev = &cfg.scsi[&key];
            let (channel, id) = dev.placement(key);
            if dev.is_daynaport() {
                return Err("Fuel does not support DaynaPort targets".into());
            }
            if let Some(path) = dev.prepare_scratch(key) {
                scratch.get_or_insert(path);
            }
            let (path, discs) = dev.media();
            let cdrom = dev.is_cdrom();
            let overlay = (cfg.ci && dev.overlay && !cdrom)
                .then(|| crate::config::ScsiDeviceConfig::ci_overlay_path(key));
            let blocks = board
                .scsi
                .attach_channel_device(
                    channel,
                    id,
                    &path,
                    cdrom,
                    discs,
                    dev.overlay,
                    overlay.as_deref(),
                )
                .map_err(|e| {
                    format!(
                        "Fuel {}: {path}: {e}",
                        crate::config::scsi_label(channel, id)
                    )
                })?;
            eprintln!(
                "Fuel {}: {path} ({blocks} blocks)",
                crate::config::scsi_label(channel, id)
            );
        }
        Ok(scratch)
    }

    fn connect_serial(
        cfg: &MachineConfig,
        board: &FuelBoard,
        console: usize,
    ) -> Result<(), String> {
        use crate::dev::z85c30::{tcp_serial_backend, SerialBackend, TeeBackend};
        let [a, b] = cfg.serial_ports();
        let ports = [cfg.fuel.l1_port.unwrap_or(8882), a, b];
        if ports[0] == ports[1] || ports[0] == ports[2] || ports[1] == ports[2] {
            return Err("Fuel L1 and IOC3 serial ports must be distinct".into());
        }
        for (index, port) in ports.into_iter().enumerate() {
            let log_path = if index == console {
                cfg.serial_log.as_deref()
            } else if index == 0 {
                cfg.fuel.l1_log.as_deref()
            } else {
                None
            };
            let log = log_path
                .map(|path| {
                    crate::dev::serial_log::SerialLog::create(
                        std::path::Path::new(path),
                        cfg.serial_log_timestamps,
                    )
                    .map_err(|e| format!("Fuel serial log {path}: {e}"))
                })
                .transpose()?;
            let backend: Arc<dyn SerialBackend> = if cfg.ci && index == console {
                if let Some(log) = log {
                    board.serial[index].set_log(log);
                }
                board.serial[index].clone()
            } else {
                let (backend, actual_port) = tcp_serial_backend(port)
                    .ok_or_else(|| format!("Fuel serial port {port} unavailable"))?;
                eprintln!(
                    "iris: Fuel {} on 127.0.0.1:{actual_port}",
                    ["L1", "IOC3 A", "IOC3 B"][index]
                );
                match log {
                    Some(log) => Arc::new(TeeBackend::new(backend, log)),
                    None => backend,
                }
            };
            if index == 0 {
                board.l1.set_serial_backend(backend);
            } else {
                board.ioc3.set_serial_backend(index - 1, backend);
            }
        }
        Ok(())
    }
}

struct CpuIrq(std::sync::Weak<dyn CpuDevice>);
impl crate::irq::InterruptController for CpuIrq {
    fn set_line(&self, line: u32, level: bool) {
        if line < 5 {
            if let Some(cpu) = self.0.upgrade() {
                cpu.as_device().signal(Signal::Interrupt(line + 2, level));
            }
        }
    }
}

impl Workstation for FuelMachine {
    fn start(&mut self) {
        self.lifecycle = crate::machine_state::GuestState::Starting;
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
            log::warn!("Fuel NVRAM: {e}");
        }
        if self.lifecycle != crate::machine_state::GuestState::Idle {
            self.lifecycle = crate::machine_state::GuestState::Halted;
        }
    }
    fn reset(&mut self) {
        self.stop();
        self.cpu.power_on();
        self.board.reset_startup();
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
            .get_or_insert_with(|| Arc::new(parking_lot::Mutex::new(MachineRef(ptr))))
            .clone()
    }
    fn register_system_controller(&mut self) {
        let slot = self.machine_slot();
        let events = self
            .events
            .take()
            .expect("Fuel system controller registered twice");
        crate::workstation::register_system_controller(&self.monitor, slot, Some(events));
    }
    fn monitor_addr(&self) -> Option<std::net::SocketAddr> {
        self.monitor.bound_addr()
    }
    fn lifecycle(&self) -> crate::machine_state::GuestState {
        self.lifecycle
    }
    fn set_lifecycle(&mut self, state: crate::machine_state::GuestState) {
        self.lifecycle = state;
    }
    fn get_display(&self) -> Option<Arc<dyn crate::gfx_display::GfxDisplay>> {
        None
    }
    fn register_monitor_device(&self, device: Arc<dyn crate::traits::Device>) {
        self.monitor.register_device(device);
    }
    fn get_ps2(&self) -> Arc<crate::dev::ps2::Ps2Controller> {
        self.board.ioc3.ps2()
    }
    fn get_timer_manager(&self) -> Arc<TimerManager> {
        self.timers.clone()
    }
    fn get_cpu_debug(&self) -> Arc<dyn crate::gdb_stub::CpuDebug> {
        self.cpu.clone().debug_adapter()
    }
    fn fasttick_count(&self) -> Arc<AtomicU64> {
        self.cpu.fasttick_count()
    }
    fn get_ci_serial(&self) -> Option<Arc<crate::dev::z85c30::CiSerialBackend>> {
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
    fn scratch_path(&self) -> Option<&std::path::Path> {
        self.scratch.as_deref()
    }
    fn disc_changer(&self) -> Option<Arc<dyn crate::workstation::DiscChanger>> {
        Some(Arc::new(FuelDiscs(self.board.scsi.clone())))
    }
    fn nat_control(&self) -> Option<Arc<crate::net::NatControl>> {
        self.nat.as_ref().map(|(n, _)| n.clone())
    }
    fn nat_expected(&self) -> Option<(std::net::Ipv4Addr, std::net::Ipv4Addr, std::net::Ipv4Addr)> {
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

struct FuelDiscs(Arc<crate::dev::isp12160::Isp12160>);
impl crate::workstation::DiscChanger for FuelDiscs {
    fn disc_status(&self) -> Vec<(usize, String, Vec<String>, u64, u64)> {
        let mut out = Vec::new();
        for bus in 0..2 {
            for (id, name, discs, pos, len) in self.0.channel(bus).unwrap().disc_status() {
                out.push((bus as usize * 16 + id, name, discs, pos, len));
            }
        }
        out
    }
    fn load_disc(&self, id: usize, path: String) -> Result<String, String> {
        self.0
            .channel((id / 16) as u8)
            .ok_or("invalid Fuel SCSI channel")?
            .load_disc(id % 16, path)
    }
    fn eject_disc(&self, id: usize) -> Result<String, String> {
        self.0
            .channel((id / 16) as u8)
            .ok_or("invalid Fuel SCSI channel")?
            .eject_disc(id % 16)
    }
    fn eject_to_empty(&self, id: usize) -> Result<(), String> {
        self.0
            .channel((id / 16) as u8)
            .ok_or("invalid Fuel SCSI channel")?
            .eject_to_empty(id % 16)
    }
}

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
/// Graphics is absent; the serial workstation shares these base I/O devices.
pub(crate) struct FuelBoard {
    pub bus: Arc<Ip35Bus>,
    pub startup: Arc<crate::ip35::startup::StartupWindow>,
    startup_view: Arc<dyn BusDevice>,
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
        Self::with_storage(payload, clock, execution_size, None, crate::dev::l1::L1Config::default().mac, [false; 2])
    }

    pub fn with_storage(
        payload: &[u8],
        clock: crate::dev::heart::Clock,
        execution_size: u64,
        flash_path: Option<&std::path::Path>,
        station_mac: [u8; 6],
        ps2_devices: [bool; 2],
    ) -> Result<Self, String> {
        use crate::dev::bedrock_pi::{BedrockPi, BedrockPiConfig};
        let md = crate::dev::bedrock_md::BedrockMd::with_clock(clock.clone());
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
        let l1 = Arc::new(crate::dev::l1::L1::new(crate::dev::l1::L1Config {
            mac: station_mac,
            ..Default::default()
        })?);
        let flash = Arc::new(match flash_path {
            Some(path) => crate::dev::bedrock_flash::Flash::with_file(payload, path),
            None => crate::dev::bedrock_flash::Flash::new(payload),
        }.map_err(|e| e.to_string())?);
        let lb = Arc::new(crate::dev::bedrock_lb::BedrockLb::new(clock.clone()));
        let io = Arc::new(crate::dev::bedrock_io::BedrockIo::new());
        let ni = Arc::new(crate::dev::bedrock_ni::BedrockNi::new());
        let xb = Arc::new(crate::dev::bedrock_xb::BedrockXb::new());
        let registers = crate::ip35::empty_view();
        crate::ip35::map_device(
            &registers,
            0,
            crate::dev::bedrock_pi::SIZE,
            pi[0].clone(),
            "Bedrock registers",
        );
        crate::ip35::map_device(
            &registers,
            0x20_0000,
            crate::dev::bedrock_pi::SIZE,
            pi[1].clone(),
            "Bedrock registers",
        );
        crate::ip35::map_device(
            &registers,
            0x40_0000,
            0x30010,
            io.clone(),
            "Bedrock registers",
        );
        crate::ip35::map_device(
            &registers,
            0x60_0000,
            0xc040,
            lb.clone(),
            "Bedrock registers",
        );
        crate::ip35::map_device(
            &registers,
            0x68_0000,
            0x2008,
            ni.clone(),
            "Bedrock registers",
        );
        crate::ip35::map_device(
            &registers,
            0x70_0000,
            0x198,
            xb.clone(),
            "Bedrock registers",
        );
        crate::ip35::map_device(
            &registers,
            0x78_0000,
            0x200000 - 0x180000,
            md.md_bus(),
            "Bedrock registers",
        );
        io.connect_hub(Arc::new(IoHub(pi.clone())));
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
        let p = pi.clone();
        md.on_error(Arc::new(move |kind| {
            for pi in &p {
                match kind {
                    crate::dev::bedrock_md::MemoryError::Correctable => pi.set_pending(1, 61),
                    crate::dev::bedrock_md::MemoryError::Uncorrectable => {
                        pi.set_memory_uncorrectable();
                    }
                }
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
                    ) == crate::traits::BUS_OK;
                }
                let value = io.inbound_read(
                    packet.source_widget, Some(packet.slot), packet.address, size as u8,
                );
                if value.status != crate::traits::BUS_OK {
                    return false;
                }
                bytes.copy_from_slice(&value.data.to_be_bytes()[8 - size..]);
                return true;
            }
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
        let ioc3 = Arc::new(crate::dev::ioc3::Ioc3::with_ps2_devices(
            crate::dev::ioc3::Ioc3Config::fuel(mac), ps2_devices[0], ps2_devices[1],
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
        bus.map(0, 0, 0x1000_0000, md.uncached_view());
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
            crate::ip35::map_device(
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
        let weak_phys = Arc::downgrade(&phys);
        let backing = md.memory_view();
        let startup = Arc::new(crate::ip35::startup::StartupWindow::with_release(
            execution_size,
            Arc::new(move || {
                if let Some(phys) = weak_phys.upgrade() {
                    phys.map(0x100000, 0x108000, backing.clone(), "Bedrock cached memory");
                }
            }),
        ));
        let startup_view: Arc<dyn BusDevice> = Arc::new(
            crate::ip35::startup::CachedView::new(md.memory_view(), startup.clone()),
        );
        phys.map(
            0x100000,
            0x108000,
            startup_view.clone(),
            "Fuel private startup window",
        );
        Ok(Self {
            bus: Arc::new(bus),
            startup,
            startup_view,
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
        if !self.startup.observing() {
            return;
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
        if !self.startup.observing() {
            return cpu.step_int();
        }
        self.observe_cpu(cpu);
        let _scope = self.startup.begin_access();
        cpu.step_int()
    }

    /// The CPU is stopped while restoring the startup decoder and observer.
    pub fn reset_startup(&self) {
        self.startup.reset();
        self.md.physical().map(
            0x100000,
            0x108000,
            self.startup_view.clone(),
            "Fuel private startup window",
        );
    }

    pub fn start(&self) {
        use crate::traits::Device;
        self.last_clock
            .store((self.clock)(), std::sync::atomic::Ordering::Release);
        self.md.start_clock();
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

// XIO register packets reach only the two PI interrupt-modify registers.
// The processor's register projection has a separate, wider interface.
struct IoHub([Arc<crate::dev::bedrock_pi::BedrockPi>; 2]);
impl BusDevice for IoHub {
    fn write64(&self, a: u64, v: u64) -> u32 {
        if matches!(a, 0x90 | 0x20_0090) && self.0[(a >> 21) as usize].io_modify(v, 0) {
            crate::traits::BUS_OK
        } else {
            crate::traits::BUS_ERR
        }
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
