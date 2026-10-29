//! The Fuel (IP34) as a machine: an R14000 on Bedrock, XBridge and the
//! base I/O devices, with Iris's CPU thread, monitor and serial console.
//!
//! Selected with `[machine] profile = "fuel_ip34"`. The shared keys select
//! the PROM, NVRAM, memory and SCSI devices; `[fuel]` selects the L1 or IOC3
//! console and the writable flash file. Graphics is not attached.

use crate::config::{FuelConsole, MachineConfig};
use crate::cpu::mips_exec::{CpuDevice, MipsCpu};
use crate::hptimer::TimerManager;
use crate::ip34_machine::{FuelBoard, FuelShadowCache};
use crate::traits::{Device, MachineEvent, Resettable, Signal};
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
        let prom = std::fs::read(&cfg.prom).map_err(|e| format!("Fuel PROM {}: {e}", cfg.prom))?;
        let board = Arc::new(FuelBoard::with_storage(
            &prom,
            crate::dev::heart::host_clock(),
            prom.len() as u64,
            cfg.fuel.flash.as_deref().map(std::path::Path::new),
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
        let mut monitor = crate::monitor::Monitor::new();
        monitor.register_device(crate::devlog::init_devlog());
        monitor.register_device(cpu.clone().as_device());
        monitor.register_device(board.md.physical().clone());
        monitor.register_device(board.ioc3.ps2());
        monitor.register_device(board.l1.clone());
        monitor.register_device(board.scsi.clone());
        cpu.register_locks();
        board.scsi.register_locks();
        board.ioc3.register_locks();
        let nat = if cfg.network.mode != crate::config::NetMode::Off {
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
        self.board.startup.reset();
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
