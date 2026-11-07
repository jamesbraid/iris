//! The O2 (IP32) as a machine: iris's CPU thread and window around the IP32
//! bus and devices (`ip32.rs`).
//!
//! Selected with `[machine] profile = "o2_ip32"` and configured by `[o2]`
//! (see [`crate::config::O2Section`]). Between instructions, two things run:
//!
//! - a service thread keeps the devices' interrupts current in CRIME and
//!   CRIME's in the CPU's IP2, runs the MACE clock from the host clock, and
//!   ends the run when the guest switches the machine off;
//! - a batch hook on the CPU thread lends the firmware the FPU, when
//!   `arcs_fpu` asks for it, with where the guest is running.
//!
//! Both CPUs run on the shadow cache from reset: the JIT reaches RAM through
//! the ppmem window with no tag probe, LRU or dirty bit on any load or store.
//! The stack POST keeps in cache tags is [`O2Memory`]'s business.
//!
//! The screen is GBE scanned out of RAM by [`O2Display`], presented through
//! the same renderer the other machines use; keys from the window go to the
//! MACE PS/2 keyboard.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::attach::{attach_devices, Ctx};
use crate::location::{Hop, Location};
use crate::scsi::ScsiPort;
use crate::topology::{Row, Topology};
use crate::config::{MachineConfig, MachineProfile};
use crate::cpu::parts::{self, Part};
use crate::cpu::mips_core::CyclesPtr;
use crate::cpu::mips_exec::{CpuDevice, MipsCpu, MipsCpuConfig, MipsExecutor};
use crate::cpu::mips_cache_v2::MipsCache;
use crate::cpu::mips_tlb::MipsTlb;
use crate::dev::crime_mem::O2Memory;
use crate::dev::ng1::rex3::Renderer;
use crate::disp::Rex3Screen;
use crate::hptimer::TimerManager;
use crate::ip32::{Ip32Bus, Ip32Options, FIRMWARE_LOAD_VA, FIRMWARE_SPAN, NO_ECC_BASE, RAM_BASE, RAM_WINDOW_SIZE};
use crate::traits::{BusDevice, Device, MachineEvent, Signal};
use crate::control::Control;
use crate::devices::{Devices, Phase};
use crate::disks::Disks;
use crate::guest_net::GuestNet;
use crate::workstation::Workstation;

const PROM_VA: std::ops::Range<u32> = 0xbfc0_0000..0xbfc8_0000;

/// Whether `pc` is in the firmware the PROM copies into RAM, at its KSEG0
/// address or its KSEG1 alias (0xa100_0000, uncached): it runs from both.
fn in_loaded_firmware(pc: u32) -> bool {
    let fw = FIRMWARE_LOAD_VA..FIRMWARE_LOAD_VA + FIRMWARE_SPAN;
    fw.contains(&pc) || fw.contains(&(pc.wrapping_sub(0x2000_0000)))
}

/// Whether `pc` is inside the PROM or its firmware copied into RAM.
fn in_firmware(pc: u32) -> bool {
    PROM_VA.contains(&pc) || in_loaded_firmware(pc)
}

/// Where the batch hook is in the machine's boot, shared with the machine so
/// a snapshot carries it: whether POST has finished, whether the CPU is in
/// the firmware, and the FPU-enable bit the firmware was lent (`arcs_fpu`).
struct BootState {
    post_done: AtomicBool,
    inside: AtomicBool,
    cu1_saved: std::sync::atomic::AtomicU32,
}

impl BootState {
    fn new() -> Self {
        Self { post_done: AtomicBool::new(false), inside: AtomicBool::new(true), cu1_saved: Default::default() }
    }

}

impl crate::traits::Device for BootState {}
impl crate::traits::Resettable for BootState {}

impl crate::traits::Saveable for BootState {
    fn save_state(&self) -> toml::Value {
        let mut t = toml::map::Map::new();
        t.insert("post_done".into(), toml::Value::Boolean(self.post_done.load(Ordering::Relaxed)));
        t.insert("inside".into(), toml::Value::Boolean(self.inside.load(Ordering::Relaxed)));
        t.insert("cu1_saved".into(), crate::snapshot::hex_u32(self.cu1_saved.load(Ordering::Relaxed)));
        toml::Value::Table(t)
    }

    fn load_state(&self, v: &toml::Value) -> Result<(), String> {
        use crate::snapshot::{get_field, toml_bool, toml_u32};
        self.post_done.store(get_field(v, "post_done").and_then(toml_bool).unwrap_or(true), Ordering::Relaxed);
        self.inside.store(get_field(v, "inside").and_then(toml_bool).unwrap_or(false), Ordering::Relaxed);
        self.cu1_saved.store(get_field(v, "cu1_saved").and_then(toml_u32).unwrap_or(0), Ordering::Relaxed);
        Ok(())
    }
}

/// The CPU's interrupt pins as a controller: line n is IPn.
struct CpuIrq(Arc<dyn crate::traits::Device>);

impl crate::irq::InterruptController for CpuIrq {
    fn set_line(&self, line: u32, level: bool) {
        self.0.signal(Signal::Interrupt(line, level));
    }
}

/// Memory into the snapshot chunk store, bank after bank. Banks are whole
/// chunks (32 or 128 MB), so the list is the one the memory as a single
/// store gave, and older snapshots load.
fn put_bank_words(store: &crate::chunk_store::ChunkStore, ram: &crate::dev::crime_mem::O2Memory) -> std::io::Result<Vec<crate::chunk_store::ChunkHash>> {
    let mut hashes = Vec::new();
    for words in ram.bank_words() {
        hashes.extend(crate::chunk_store::put_words_as_chunks(store, words)?);
    }
    Ok(hashes)
}

pub struct O2Machine {
    /// Where this machine has memory.
    pub memory: Arc<crate::memory_map::MemoryMap>,
    pub bus: Arc<Ip32Bus>,
    cpu: Arc<dyn CpuDevice>,
    timer_manager: Arc<TimerManager>,
    display: Arc<O2Display>,
    running: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    /// The shared monitor (telnet, `monitor_port`): the CPU's commands and
    /// every O2 device's, through the bus.
    monitor: Arc<crate::monitor::Monitor>,
    /// The console UART's in-process host end in CI mode (`--ci`), which the
    /// CI socket types into and reads.
    ci_serial: Option<Arc<crate::dev::serial_host::CiSerialBackend>>,
    /// The NAT handle, when the Ethernet is plugged in.
    net: Option<GuestNet>,
    /// Lifecycle state, the events channel (the guest switching the machine
    /// off) and the pointer slot the front ends share. Until the event thread
    /// runs the service loop exits the process itself on a power-off, as it
    /// always has.
    pub(crate) control: Control,
    devices: Devices,
    disks: Arc<Disks>,
    /// The places the machine has, with the SCSI controllers bound to theirs.
    topology: Arc<Topology>,
    /// Where snapshots live: `saves/<name>/` under the working directory.
    saves: std::path::PathBuf,
    /// The batch hook's place in the boot (see [`BootState`]).
    boot: Arc<BootState>,
    /// Set while the machine is stopped: the service thread leaves the
    /// devices alone, so nothing changes memory under a snapshot.
    paused: Arc<AtomicBool>,
    /// The flash file, recorded in a snapshot's manifest.
    flash_path: Option<String>,
}

/// What an O2 snapshot's manifest names as its machine.
const SNAPSHOT_MACHINE: &str = "o2_ip32";

/// The PROM image tried when `prom` is left at the shared default: the last
/// release's, named as SGI shipped it. The O2 has no embedded PROM.
pub const O2_PROM_FILE: &str = "ip32prom.rev4.18.bin";

/// The processors an O2 shipped with, the default first.
pub const CPUS: &[&Part] = &[&parts::R5000_180, &parts::R10000_250, &parts::R12000_400];

impl O2Machine {
    /// The places an O2 has: the PCI bus, its two onboard SCSI controllers
    /// (the internal one at device 1, the external connector at 2) with
    /// targets 1 to 7. The Ethernet is built in, so no target takes a
    /// DaynaPort. Touches no hardware.
    pub(crate) fn fill(_: &MachineConfig, t: &Topology) {
        let at = |hops: &[Hop]| hops.iter().fold(Location::root(), |l, h| l.join(*h));
        let add = |l: Location, row: Row| t.add(l, row).expect("rows are added once");
        add(Location::root(), Row::fixed("MACE"));
        add(at(&[Hop::pci(0)]), Row::fixed("PCI bus"));
        for (n, label) in ["SCSI internal", "SCSI external"].into_iter().enumerate() {
            let ctl = at(&[Hop::pci(0), Hop::pos(n as u8 + 1)]);
            add(ctl.clone(), Row::fixed(label).ctl(n as u8).onboard("aic7880"));
            t.scsi(&ctl, 1..=7, false);
        }
    }

    /// The hard disks as a snapshot records them, to refuse one taken on others.
    fn disk_refs(&self) -> Vec<crate::snapshot::DiskRef> {
        crate::snapshot::DiskRef::from_records(&self.disks, &self.topology, false)
    }

    pub fn new(cfg: &MachineConfig) -> Result<Self, String> {
        let part = MachineProfile::O2Ip32.part(cfg.machine.cpu.as_deref())?;
        let o2 = &cfg.o2;
        // `prom`, or the last release's file when `prom` is the shared default
        // and that file is not there: there is no embedded O2 PROM.
        let prom_path = MachineProfile::O2Ip32.prom_path(&cfg.prom);
        let prom = std::fs::read(&prom_path)
            .map_err(|e| format!("O2 PROM {prom_path}: {e} (set `prom` to the O2 PROM image; 4.18 is the last release)"))?;
        // `banks`: the kit in each DIMM pair from A (validate checked them).
        let kits = cfg.o2_kits();
        let mem = O2Memory::from_kits(&kits)?;
        // The PROM's flash, and so its environment, lives in `nvram`.
        // `[network] mac` is the address in the 1-Wire ID chip. `headless`:
        // no keyboard, so the PROM and the OS take the serial console, as on
        // an O2 booted without one. The GBE stays: it is part of every O2.
        let mac = match cfg.network.mac.as_deref() {
            Some(mac) => Some(crate::config::parse_mac(mac).map_err(|e| format!("network.mac: {e}"))?),
            None => None,
        };
        // The controllers own their buses; the devices are attached to them
        // below, once the machine is built.
        let topology = Arc::new(Topology::new());
        Self::fill(cfg, &topology);
        let scsi = [crate::dev::aic7880::Aic7880::new_bus(), crate::dev::aic7880::Aic7880::new_bus()];
        let opts = Ip32Options {
            scsi: Some(scsi),
            flash_file: Some(cfg.nvram.clone().into()),
            keyboard: (!cfg.headless).then_some(o2.mouse),
            mac,
        };
        let bus = Ip32Bus::with_options(mem, prom, opts);
        // MACE audio plays through the host unless audio is off.
        if !cfg.no_audio {
            bus.mace.audio.enable_output(cfg.audio.clone());
        }
        for (n, ctl) in bus.scsi.lock().unwrap().iter().enumerate() {
            let at = topology.controller(n as u8).expect("fill added a row for every controller");
            topology.bind(&at, Arc::new(ScsiPort::new(ctl.clone()))).expect("the controller row takes its handle once");
        }
        eprintln!(
            "iris: O2 with {} MB ({}), {}",
            kits.iter().sum::<u32>(),
            kits.iter().map(|k| format!("{k}")).collect::<Vec<_>>().join("+"),
            if cfg.headless { "serial console" } else { "graphics console" }
        );
        // The console UART's host end: in CI mode the in-process backend
        // the CI socket types into, otherwise TCP on `serial_port_a` (as the
        // Indy's channel A), either teed to `serial_log` when one is set. The
        // second port serves TCP on `serial_port_b`.
        let ci_serial = cfg.ci.then(|| Arc::new(crate::dev::serial_host::CiSerialBackend::new()));
        let (console, second) = crate::dev::serial_host::console_backends(cfg, "O2", ci_serial.as_ref());
        bus.mace.com0.set_backend(console);
        if let Some(b) = second {
            bus.mace.com1.set_backend(b);
        }
        // The Ethernet on the NAT gateway `[network]` describes (`mode =
        // "off"` leaves it unplugged; validate refuses pcap here for now).
        let net = match GuestNet::open(cfg, "O2")? {
            Some((net, link)) => {
                bus.mace.mec.attach(link);
                Some(net)
            }
            None => None,
        };

        let timer_manager = Arc::new(TimerManager::new());
        let sysad: Arc<dyn BusDevice> = bus.phys.clone();
        let arcs_fpu = o2.arcs_fpu;
        // The CPU clock the PROM measures and reports is twice Count.
        let count_hz = cfg.clock.count_hz(part);
        let cpu_mhz = (count_hz as f64 * 2.0 / 1e6).round().max(1.0) as u32;
        let boot = Arc::new(BootState::new());
        let memory = Arc::new(crate::ip32::memory_map());
        let cpu: Arc<dyn CpuDevice> = crate::with_cpu!(part, true, true, C =>
            Self::build_cpu::<C>(memory.clone(), sysad, &timer_manager, arcs_fpu, count_hz, boot.clone(), bus.ram.clone(), part.prid()));

        // ppmem: the CPU's cache and the JIT reach RAM through the memory's
        // window, publishing the mapped bitmap into the CPU's own fields, as
        // the Indigo2 does. Remaps happen on the CPU thread (a CRIME bank
        // control write), which is the window's contract.
        {
            use crate::ppmem::MappedMemory;
            let sp = bus.phys.ppmem_space().ok_or("ppmem: the O2 needs its memory window")?;
            unsafe {
                sp.set_bitmap_sink(cpu.ppmem_bitmap_ptr());
                cpu.set_tcache_window(sp.window_base());
                let bitmap = cpu.tcache_bitmap_ptr();
                if !bitmap.is_null() {
                    sp.set_bitmap_sink2(bitmap);
                }
                #[cfg(feature = "jitv2")]
                cpu.set_tcache_gen_window(sp.gen_window_base());
            }
            // The bus's direct paths read the same bitmap.
            bus.phys.resync_ppmem_bitmap();
        }

        // After the windows above: each compile worker takes the cache's
        // geometry once, as it starts, and without the windows that geometry
        // has no inline load or store path.
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
            // Codegen inline needs and deadlocks the CPU thread's flush (its
            // join waits on workers parked on the Jitv2 lock it holds).
            #[cfg(not(feature = "jitv2_lockstep"))]
            {
                drop(g);
                crate::cpu::jitv2::jitv2::start_compiling(&*cpu, &jit, cfg.jitv2.compile, bus.phys.clone() as Arc<dyn BusDevice>);
            }
        }

        // CRIME's output is the CPU's IP2, pushed the moment it changes.
        bus.mace.ust.use_host_clock();
        bus.connect_cpu_irq(crate::irq::IrqLine::new(Arc::new(CpuIrq(cpu.clone().as_device())), 2));

        bus.gbe.use_cpu_clock(cpu.cycles_ptr(), cpu_mhz);
        // CRIME's rendering engine draws on its own thread, fed through
        // its interface buffer.
        bus.crime.re.start(bus.clone(), |b: &Ip32Bus| &b.crime.re);
        let running = Arc::new(AtomicBool::new(false));
        let status = crate::disp::StatusSource::new(bus.mace.id.heartbeat.clone(), cpu.fasttick_count());
        status.set_cpu_cycles(cpu.cycles_ptr());
        status.set_count_hz(cpu.count_hz_atomic());
        let display = Arc::new(O2Display::new(bus.clone(), status, running.clone()));
        if let Some(port) = o2.control_port {
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(l) => {
                    eprintln!("iris: O2 control on 127.0.0.1:{port} (the monitor's `o2` commands; `help o2` there lists them)");
                    let bus = bus.clone();
                    std::thread::Builder::new().name("O2-Control".into()).spawn(move || control_server(l, bus)).unwrap();
                }
                Err(e) => eprintln!("iris: O2 control port {port}: {e}"),
            }
        }
        // The shared monitor, as the other machines have it: the CPU's
        // commands (registers, breakpoints, memory) and the O2's devices'.
        let mut devices = Devices::new();
        devices.add("o2", Phase::Chipset, bus.clone());
        devices.add("boot", Phase::Chipset, boot.clone());
        let monitor_port = cfg.monitor_port.unwrap_or(crate::monitor::DEFAULT_PORT);
        let monitor = crate::monitor::Monitor::new(monitor_port);
        monitor.register_device(crate::devlog::init_devlog());
        monitor.register_device(Arc::new(crate::cpu::cpu_monitor::CpuMonitor::new(vec![cpu.clone()])));
        // The bus brings its devices' and the SCSI controllers' commands.
        devices.register_monitor(&monitor);
        // `phys`: the physical bus's decode table, memory and traces.
        monitor.register_device(bus.phys.clone());
        monitor.register_device(Arc::new(O2Control::new(bus.clone())));
        #[cfg(feature = "perfetto")]
        {
            crate::perfetto::set_guest_cycles(Some(cpu.cycles_ptr()));
            monitor.register_device(Arc::new(crate::perfetto::PerfettoMonitor));
        }

        let control = Control::new();
        let disks = Arc::new(Disks::new().with_ci(cfg.ci));
        attach_devices(&cfg.placements()?, cfg, Some(&mut Ctx { devices: &mut devices, disks: &disks }), &topology)?;
        Ok(Self {
            memory,
            bus,
            cpu,
            timer_manager,
            display,
            running,
            threads: Vec::new(),
            monitor: Arc::new(monitor),
            ci_serial,
            net,
            control,
            devices,
            disks,
            topology,
            saves: "saves".into(),
            boot,
            paused: Arc::new(AtomicBool::new(false)),
            flash_path: Some(cfg.nvram.clone()),
        })
    }

    fn build_cpu<C>(memory: Arc<crate::memory_map::MemoryMap>, sysad: Arc<dyn BusDevice>, tm: &Arc<TimerManager>, arcs_fpu: bool, count_hz: u64, boot: Arc<BootState>,
                    ram: Arc<O2Memory>, prid: u32) -> Arc<dyn CpuDevice>
    where
        C: crate::cpu::mips_cache_v2::CpuModel + MipsCache + From<Arc<dyn BusDevice>> + 'static,
        MipsCpu<MipsTlb, C>: CpuDevice,
    {
        let cfg = MipsCpuConfig::for_model::<C>(memory).with_prid(prid);
        let tlb = MipsTlb::new(cfg.tlb_entries);
        let mut exec: MipsExecutor<MipsTlb, C> = MipsExecutor::new(sysad, tlb, &cfg);
        exec.cache.set_pd_tag_store_hook(Box::new(move |tag| ram.stack_tag_store(tag)));
        exec.core.set_timer_manager(tm.clone());
        // Count runs at half the pipeline clock on both CPUs.
        exec.core.set_count_hz(count_hz.max(1));
        // The no-ECC alias of memory reaches memory, not the PCI window the
        // bus decodes at the same low 32 bits.
        exec.phys_alias = Some((NO_ECC_BASE, RAM_WINDOW_SIZE as u64, RAM_BASE as u64));
        // CRIME's decode, like the MC's, is modelled on the low 32 bits.
        exec.phys_mask = u32::MAX as u64;
        exec.batch_hook = Some(Box::new(move |exec: &mut MipsExecutor<MipsTlb, C>| {
            let pc = exec.core.pc as u32;
            LAST_PC.store(pc, Ordering::Relaxed);
            BATCHES.fetch_add(1, Ordering::Relaxed);
            for (slot, v) in LAST_REGS.iter().zip(exec.core.gpr.iter()) {
                slot.store(*v, Ordering::Relaxed);
            }
            LAST_REGS[32].store(exec.core.cp0_status as u64, Ordering::Relaxed);
            LAST_REGS[33].store(exec.core.cp0_cause as u64, Ordering::Relaxed);
            LAST_REGS[34].store(exec.core.cp0_epc, Ordering::Relaxed);
            LAST_REGS[35].store(exec.core.cp0_badvaddr, Ordering::Relaxed);
            // The timer, for the input stall: Count as last materialized,
            // Compare, the pending-interrupt word and the Compare-match count.
            LAST_REGS[36].store(exec.core.cp0_count, Ordering::Relaxed);
            LAST_REGS[37].store(exec.core.cp0_compare, Ordering::Relaxed);
            LAST_REGS[38].store(exec.core.hot.interrupts.load(Ordering::Relaxed), Ordering::Relaxed);
            LAST_REGS[39].store(exec.core.fasttick_count.load(Ordering::Relaxed), Ordering::Relaxed);
            // Instructions retired, for an idle-MIPS reading over the control port.
            LAST_REGS[40].store(exec.core.hot.cycles, Ordering::Relaxed);
            LAST_REGS[41].store(exec.core.cp0_config as u64, Ordering::Relaxed);
            if !boot.post_done.load(Ordering::Relaxed) {
                // POST is over once the firmware it loaded into RAM runs.
                if in_loaded_firmware(pc) {
                    boot.post_done.store(true, Ordering::Relaxed);
                } else {
                    return;
                }
            }
            let fw = in_firmware(pc);
            if fw != boot.inside.load(Ordering::Relaxed) {
                boot.inside.store(fw, Ordering::Relaxed);
                if arcs_fpu {
                    const CU1: u32 = 1 << 29;
                    if fw {
                        boot.cu1_saved.store(exec.core.cp0_status & CU1, Ordering::Relaxed);
                        exec.core.cp0_status |= CU1;
                    } else {
                        exec.core.cp0_status = (exec.core.cp0_status & !CU1) | boot.cu1_saved.load(Ordering::Relaxed);
                    }
                    exec.resync_privilege_state();
                }
            }
        }));
        Arc::new(MipsCpu::new(exec))
    }

    pub fn start(&mut self) {
        // Publish "starting" before the monitor binds, so a caller polling
        // readiness can tell "not started" from "startup under way".
        self.control.started();
        self.running.store(true, Ordering::SeqCst);
        self.monitor.serve();
        let d = self.display.clone();
        self.threads.push(std::thread::Builder::new().name("O2-Display".into()).spawn(move || d.display_loop()).unwrap());
        let bus = self.bus.clone();
        let cpu = self.cpu.clone();
        let running = self.running.clone();
        let events = (self.control.sender(), self.control.registered());
        let disks = (self.disks.clone(), self.topology.clone());
        let paused = self.paused.clone();
        self.devices.start();
        self.threads.push(
            std::thread::Builder::new()
                .name("O2-Service".into())
                .spawn(move || Self::service_loop(bus, cpu, running, paused, events, disks))
                .unwrap(),
        );
        // CI mode: the harness starts the CPU (`iris-ci start`).
        if self.ci_serial.is_none() {
            self.cpu.clone().as_device().start();
        }
    }

    /// Device interrupts the guest did not cause by an access (network
    /// frames, keys), typing scripts, and the guest's power switch.
    fn service_loop(
        bus: Arc<Ip32Bus>,
        cpu: Arc<dyn CpuDevice>,
        running: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
        (events, controller_up): (std::sync::mpsc::SyncSender<MachineEvent>, Arc<AtomicBool>),
        (disks, topology): (Arc<Disks>, Arc<Topology>),
    ) {
        let dev = cpu.clone().as_device();
        // The PROM's environment lives in its flash: save it once the guest
        // has written there (setenv) and the writing has stopped.
        let mut flash_seen = bus.flash_writes();
        let mut flash_check = Instant::now();
        // How long to sleep between service passes. While CRIME is
        // interrupting the machine is busy, so keep the tight 100 µs poll;
        // while it is quiet back off to 2 ms so an idle machine costs ~500
        // wakeups/s instead of 10 000. The backoff only adds latency to
        // asynchronous input (a typed key, a frame from the network) —
        // and while any of that is flowing, its interrupts keep the poll
        // at 100 µs anyway.
        const SERVICE_POLL: Duration = Duration::from_micros(100);
        const SERVICE_POLL_IDLE: Duration = Duration::from_millis(2);
        let mut poll = SERVICE_POLL;
        while running.load(Ordering::Relaxed) {
            if paused.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            if flash_check.elapsed() >= Duration::from_secs(2) {
                flash_check = Instant::now();
                let n = bus.flash_writes();
                if n != flash_seen {
                    flash_seen = n;
                    bus.save_flash();
                }
            }
            // Frames from the network, and any line that changed without a
            // register access (a disk finishing, a key from the window).
            let interrupting = bus.service();
            poll = if interrupting { SERVICE_POLL } else {
                (poll * 2).min(SERVICE_POLL_IDLE)
            };
            // A cold reset through CRIME (NetBSD's reboot): the CPU starts
            // again at the reset vector with every device as at power-on.
            if bus.crime.regs.take_reset_request() {
                eprintln!("iris: the O2 reset itself (CRIME cold reset)");
                dev.stop();
                crate::traits::Resettable::power_on(&*cpu);
                crate::traits::Resettable::power_on(&*bus);
                dev.start();
            }
            if bus.mace.rtc.powered_off() {
                eprintln!("iris: the O2 switched itself off");
                dev.stop();
                bus.save_flash();
                // The machine-events thread stops the machine and exits,
                // unless the host embeds iris (IRIS_NO_EXIT_ON_POWEROFF).
                if controller_up.load(Ordering::SeqCst) && events.try_send(MachineEvent::PowerOff).is_ok() {
                    return;
                }
                running.store(false, Ordering::SeqCst);
                crate::workstation::exit_process("power-off", 0, || {
                    crate::workstation::flushed(disks.flush(&topology), "power-off")
                });
            }
            std::thread::sleep(poll);
        }
    }

    /// Stop the CPU, hold the service thread off the devices, let the
    /// rendering engine finish what it was given, and save the PROM's flash.
    /// The display thread and the monitor run on until the machine is
    /// dropped; [`Self::restart_peripherals`] lets the service thread go.
    pub fn stop(&mut self) {
        self.cpu.clone().as_device().stop();
        self.paused.store(true, Ordering::SeqCst);
        // The guest clock points into this machine's CPU core.
        #[cfg(feature = "perfetto")]
        crate::perfetto::set_guest_cycles(None);
        // After the CPU, so nothing queues more; each finishes the command
        // it is running, so a snapshot finds none half done.
        self.devices.stop();
        if !self.bus.crime.re.drain(Duration::from_secs(2)) {
            eprintln!("iris: O2 stopped with rendering engine writes still queued");
        }
        self.bus.save_flash();
        // Not through the display: this stop leaves the guest's writes durable
        // whatever follows it.
        if let Err(e) = self.disks.flush(&self.topology) {
            eprintln!("iris: disk flush at stop failed: {e} (guest writes may be lost)");
        }
        self.control.stopped();
    }

    /// Let the service thread look after the devices again.
    pub fn restart_peripherals(&mut self) {
        self.paused.store(false, Ordering::SeqCst);
        self.devices.start();
    }

    /// The CPU and every part back at power-on.
    fn power_on_devices(&self) {
        crate::traits::Resettable::power_on(&*self.cpu);
        self.devices.power_on();
    }

    /// The hardware as at power-on, with the CPU stopped: what a CRIME cold
    /// reset does, less the restart.
    pub fn reset(&mut self) {
        self.stop();
        self.power_on_devices();
        self.restart_peripherals();
    }

    /// Save the machine to `saves/<name>/`, then run on, as the Indy does.
    /// The format is the Indy's: device state, memory in the shared chunk
    /// store (all of it as bank 0), the copy-on-write disks' changes; plus
    /// the flash and the batch hook's place in the boot. Disks without
    /// `overlay` are not in it: restoring then pairs saved memory with
    /// whatever the disk holds by then.
    pub fn save_snapshot(&mut self, name: &str) -> Result<(), String> {
        self.stop();
        let saved = self.write_snapshot(name);
        self.restart_peripherals();
        self.cpu.clone().as_device().start();
        saved?;
        println!("Snapshot saved to saves/{name}");
        Ok(())
    }

    fn write_snapshot(&self, name: &str) -> Result<(), String> {
        use crate::chunk_store::{put_words_as_chunks, ChunkStore};
        use crate::snapshot::{ChunksManifest, Manifest, Snapshot};
        use crate::traits::Saveable;
        let snap = Snapshot::new(self.saves.join(name));
        snap.ensure_dir().map_err(|e| e.to_string())?;
        let mut manifest = Manifest::for_current_save();
        manifest.machine = Some(SNAPSHOT_MACHINE.into());
        manifest.disks = self.disk_refs();
        manifest.nvram = self.flash_path.clone();
        manifest.cpu_model = Some(self.cpu.model_name().to_string());
        manifest.cpu_cache = Some(self.cpu.cache_kind().to_string());
        snap.write_manifest(&manifest).map_err(|e| e.to_string())?;
        let sv = manifest.schema_version;
        snap.write_state("cpu", &self.cpu.save_state(), sv).map_err(|e| e.to_string())?;
        // `o2` and `boot`, one file each.
        if let Some(table) = self.devices.save().as_table() {
            for (key, state) in table {
                snap.write_state(key, state, sv).map_err(|e| e.to_string())?;
            }
        }
        snap.write_bin("flash.bin", &self.bus.flash_bytes()).map_err(|e| e.to_string())?;
        let store = ChunkStore::new(&self.saves);
        let mut chunks = ChunksManifest::default();
        chunks.bank_chunks[0] = put_bank_words(&store, &self.bus.ram)
            .map_err(|e| format!("CAS memory put: {e}"))?;
        snap.write_chunks_manifest(&chunks).map_err(|e| e.to_string())?;
        for (i, ctl) in self.bus.scsi.lock().unwrap().iter().enumerate() {
            let dir = snap.dir.join(format!("ctrl{i}"));
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let overlays = ctl.export_overlays(&dir).map_err(|e| format!("COW overlay export (controller {i}): {e}"))?;
            snap.write_toml(&format!("cow{i}.toml"), &crate::snapshot::build_cow_table(overlays))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Restore the machine from `saves/<name>/` and run it. Everything is
    /// read and checked before the machine is touched, so a snapshot that
    /// does not fit (another machine, CPU, build, memory or disk) leaves it
    /// as it was.
    pub fn load_snapshot(&mut self, name: &str) -> Result<(), String> {
        let snap = self.read_snapshot(name)?;
        self.stop();
        let applied = self.apply_snapshot(snap);
        self.restart_peripherals();
        applied?;
        self.cpu.clone().as_device().start();
        println!("Snapshot loaded from saves/{name}");
        Ok(())
    }

    fn read_snapshot(&self, name: &str) -> Result<O2Snapshot, String> {
        use crate::chunk_store::{get_chunks_as_words, ChunkStore};
        use crate::snapshot::{Snapshot, SCHEMA_VERSION};
        let snap = Snapshot::new(self.saves.join(name));
        let m = snap.read_manifest()?.ok_or_else(|| format!("saves/{name}: no snapshot.toml"))?;
        if m.machine.as_deref() != Some(SNAPSHOT_MACHINE) {
            return Err(format!("snapshot {name} is not of an O2"));
        }
        if m.host_arch != std::env::consts::ARCH {
            return Err(format!("snapshot host_arch '{}' does not match current host '{}'", m.host_arch, std::env::consts::ARCH));
        }
        if m.schema_version > SCHEMA_VERSION {
            return Err(format!("snapshot schema_version {} is newer than this iris build supports ({SCHEMA_VERSION})", m.schema_version));
        }
        if m.schema_version < SCHEMA_VERSION {
            return Err(format!(
                "snapshot {name} is schema {}; the O2's UART state changed shape in schema 5, so it cannot be restored",
                m.schema_version
            ));
        }
        // As the Indy: build, CPU and disks must match.
        crate::snapshot::check_provenance(&m, self.cpu.model_name(), self.cpu.cache_kind(), &self.disk_refs())?;
        let sv = m.schema_version;
        let state = |base: &str| snap.read_state(base, sv).map_err(|e| format!("{base}: {e}"));
        let chunks = snap.read_chunks_manifest().map_err(|e| format!("chunks.bin: {e}"))?;
        let memory = get_chunks_as_words(&ChunkStore::new(&self.saves), &chunks.bank_chunks[0])
            .map_err(|e| format!("CAS memory get: {e}"))?;
        if memory.len() * 4 != self.bus.ram.len() {
            return Err(format!("snapshot memory is {} MB, this O2 has {} MB",
                memory.len() >> 18, self.bus.ram.len() >> 20));
        }
        let ctrls = self.bus.scsi.lock().unwrap().len();
        let overlays = (0..ctrls)
            .map(|i| snap.read_toml(&format!("cow{i}.toml")).ok().map(|t| crate::snapshot::parse_cow_table(&t)))
            .collect();
        let mut board = toml::map::Map::new();
        for key in self.devices.keys() {
            board.insert(key.clone(), state(&key)?);
        }
        Ok(O2Snapshot {
            cpu: state("cpu")?,
            board: toml::Value::Table(board),
            flash: snap.read_bin("flash.bin").map_err(|e| format!("flash.bin: {e}"))?,
            memory,
            overlays,
            dir: snap.dir,
        })
    }

    /// Put a read snapshot into the stopped machine: the hardware as at
    /// power-on first, so nothing the snapshot does not hold is left over.
    fn apply_snapshot(&mut self, s: O2Snapshot) -> Result<(), String> {
        use crate::traits::Saveable;
        self.power_on_devices();
        self.cpu.load_state(&s.cpu)?;
        self.devices.load(&s.board)?;
        self.bus.set_flash(&s.flash)?;
        self.bus.ram.restore_words(&s.memory)?;
        for (i, ctl) in self.bus.scsi.lock().unwrap().iter().enumerate() {
            if let Some(Some(sets)) = s.overlays.get(i) {
                ctl.import_overlays(&s.dir.join(format!("ctrl{i}")), sets)
                    .map_err(|e| format!("COW overlay import (controller {i}): {e}"))?;
            }
        }
        Ok(())
    }

    pub fn get_ps2(&self) -> Arc<crate::dev::ps2::Ps2Controller> {
        match self.bus.mace.ps2.devices() {
            Some(d) => d.clone(),
            // Serial console: a keyboard nothing reads.
            None => Arc::new(crate::dev::ps2::Ps2Controller::new(None)),
        }
    }

    pub fn get_display(&self) -> Arc<dyn crate::gfx_display::GfxDisplay> {
        self.display.clone()
    }

    pub fn get_timer_manager(&self) -> Arc<TimerManager> {
        self.timer_manager.clone()
    }
}

/// A snapshot read and checked, ready to put into the machine.
struct O2Snapshot {
    cpu: toml::Value,
    /// The parts' states, as `Devices::save` writes them.
    board: toml::Value,
    flash: Vec<u8>,
    memory: Vec<u32>,
    /// Each controller's copy-on-write sets, when the snapshot has them.
    overlays: Vec<Option<Vec<(usize, Vec<u64>)>>>,
    dir: std::path::PathBuf,
}

impl Drop for O2Machine {
    fn drop(&mut self) {
        // First wait out any monitor, event or CI command in flight, so none
        // sees a half-destroyed machine (see `Drop for Machine`).
        self.control.release();
        // The CPU and its compile pool first: they reach the bus, which is
        // freed once this returns.
        self.cpu.clone().as_device().stop();
        #[cfg(feature = "jitv2")]
        crate::cpu::jitv2::Jitv2::stop_for_drop(&self.cpu.jitv2(), || self.cpu.clone().as_device().stop());
        self.display.display_stop.request();
        self.running.store(false, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        crate::workstation::flush_on_drop(self);
        self.monitor.shutdown();
        // The rendering engine's thread runs from construction and holds the bus.
        self.bus.crime.re.stop();
    }
}

/// The O2 as the front ends see it. The CI rollback and `--load-elf` are not
/// there yet; the trait's defaults say so.
impl Workstation for O2Machine {
    fn start(&mut self) { O2Machine::start(self) }
    fn stop(&mut self) { O2Machine::stop(self) }
    fn reset(&mut self) { O2Machine::reset(self) }
    fn restart_peripherals(&mut self) { O2Machine::restart_peripherals(self) }
    fn cpu_start(&self) { self.cpu.clone().as_device().start() }
    fn cpu_is_running(&self) -> bool { self.cpu.clone().as_device().is_running() }
    fn devices(&self) -> &Devices { &self.devices }
    fn disks(&self) -> &Disks { &self.disks }
    fn topology(&self) -> &Topology { &self.topology }
    fn control(&self) -> &Control { &self.control }
    fn control_mut(&mut self) -> &mut Control { &mut self.control }
    fn net(&self) -> Option<&GuestNet> { self.net.as_ref() }
    fn monitor(&self) -> &Arc<crate::monitor::Monitor> { &self.monitor }
    fn as_ws_ptr(&mut self) -> *mut dyn Workstation { self as *mut Self as *mut dyn Workstation }

    fn get_display(&self) -> Option<Arc<dyn crate::gfx_display::GfxDisplay>> { Some(O2Machine::get_display(self)) }
    fn get_ps2(&self) -> Arc<crate::dev::ps2::Ps2Controller> { O2Machine::get_ps2(self) }
    fn get_timer_manager(&self) -> Arc<TimerManager> { O2Machine::get_timer_manager(self) }
    fn get_cpu_debug(&self) -> Arc<dyn crate::gdb_stub::CpuDebug> { self.cpu.clone().debug_adapter() }
    fn fasttick_count(&self) -> Arc<std::sync::atomic::AtomicU64> { self.cpu.fasttick_count() }
    fn get_ci_serial(&self) -> Option<Arc<crate::dev::serial_host::CiSerialBackend>> { self.ci_serial.clone() }
    /// Typed at serial port 1, the console with `graphics = false`.
    fn inject_serial_console(&self, bytes: &[u8]) {
        match &self.ci_serial {
            Some(ci) => ci.push_host(bytes),
            None => self.bus.mace.com0.feed(bytes),
        }
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }

    fn save_snapshot(&mut self, name: &str) -> Result<(), String> { O2Machine::save_snapshot(self, name) }
    fn load_snapshot(&mut self, name: &str) -> Result<(), String> { O2Machine::load_snapshot(self, name) }

}

/// GBE's picture, scanned out of RAM about sixty times a second and handed
/// to whatever renderer the front-end installs.
pub struct O2Display {
    bus: Arc<Ip32Bus>,
    renderer: Mutex<Option<Box<dyn Renderer>>>,
    display_stop: crate::gfx_display::DisplayStop,
    screen: Mutex<Rex3Screen>,
    screenshot_pending: AtomicBool,
    /// The status bar's inputs; the heartbeat is the MACE ID chip's, and the
    /// Hz is the CPU's Compare-match count (IRIX's clock tick on the O2,
    /// which has no 8254).
    status: Arc<crate::disp::StatusSource>,
    running: Arc<AtomicBool>,
}

impl O2Display {
    fn new(bus: Arc<Ip32Bus>, status: Arc<crate::disp::StatusSource>, running: Arc<AtomicBool>) -> Self {
        Self {
            bus,
            renderer: Mutex::new(None),
            display_stop: Default::default(),
            screen: Mutex::new(Rex3Screen::new()),
            screenshot_pending: AtomicBool::new(false),
            status,
            running,
        }
    }

    fn display_loop(&self) {
        let frame_time = Duration::from_micros(16_667);
        let mut status_bar = crate::disp::StatusBar::new();
        let mut overlay = crate::debug_overlay::DebugOverlay::new();
        let mut sbtex = crate::disp::StatusBarTexture::new();
        let mut rgba = vec![0u8; 2048 * 2048 * 4];
        let mut size = (0usize, 0usize);
        let mut shots = 0u32;
        while self.running.load(Ordering::Relaxed) {
            let start = Instant::now();
            let shown = self.bus.render_screen(&mut rgba);
            // Off: keep the last size, black.
            let (w, h) = shown.map(|(w, h)| (w as usize, h as usize)).unwrap_or(if size.0 == 0 { (1280, 1024) } else { size });
            {
                let mut screen = self.screen.lock();
                let stride = crate::disp::FB_STRIDE;
                for y in 0..h {
                    for x in 0..w {
                        let v = if shown.is_some() {
                            let o = (y * w + x) * 4;
                            u32::from_le_bytes([rgba[o], rgba[o + 1], rgba[o + 2], 0xff])
                        } else {
                            0xff00_0000
                        };
                        screen.fb_rgb[y * stride + x] = v;
                        screen.rgba[y * stride + x] = v;
                    }
                }
                screen.frame_seq = screen.frame_seq.wrapping_add(1);
                screen.prebuilt = true;
                screen.width = w;
                screen.height = h;
                screen.status_bar_only = false;
                let take_shot = self.screenshot_pending.swap(false, Ordering::Relaxed);
                let stats = self.status.snapshot(start, 0);
                if let Some(r) = self.renderer.lock().as_mut() {
                    if (w, h) != size {
                        r.resize(w, h);
                    }
                    r.present(&mut screen, &mut overlay, &mut status_bar, &mut sbtex, &stats, take_shot, None, None);
                }
                size = (w, h);
                if take_shot {
                    let pixels = screen.rgba.clone();
                    let path = format!("screenshot_{shots:04}.png");
                    shots += 1;
                    std::thread::spawn(move || match crate::disp::save_screenshot(&path, &pixels, w, h) {
                        Ok(()) => println!("iris: screenshot saved to {path}"),
                        Err(e) => println!("iris: screenshot failed: {e}"),
                    });
                }
            }
            let spent = start.elapsed();
            if spent < frame_time {
                std::thread::sleep(frame_time - spent);
            }
        }
        self.display_stop.finish(&self.renderer);
    }
}

impl crate::gfx_display::GfxDisplay for O2Display {
    fn renderer_slot(&self) -> &Mutex<Option<Box<dyn Renderer>>> {
        &self.renderer
    }
    fn display_stop(&self) -> &crate::gfx_display::DisplayStop {
        &self.display_stop
    }
    fn screen(&self) -> &Mutex<Rex3Screen> {
        &self.screen
    }
    fn request_screenshot(&self) {
        self.screenshot_pending.store(true, Ordering::Relaxed);
    }
    fn cycles(&self) -> CyclesPtr {
        self.status.cycles()
    }
    fn hw_cursor(&self) -> Option<crate::gfx_display::HwCursor> {
        let (x, y, mask) = self.bus.gbe.cursor()?;
        Some(crate::gfx_display::HwCursor { x, y, size: 32, mask })
    }
}

/// The control port: one command per line, one reply line each (`ok ...` or
/// `err ...`). For scripts and for an assistant driving the machine while a
/// person watches its window.
///
/// - `shot PATH`: the screen as a PNG.
/// - `type TEXT`: type it on the PS/2 keyboard (`\r` Enter, `\e` Escape,
///   `\b` Backspace, `\t` Tab).
/// - `key NAME`, `keydown NAME`, `keyup NAME`: a key by its winit name
///   (`Enter`, `KeyA`, `F1`, `ShiftLeft`, `ArrowUp`...).
/// - `mouse DX DY [BUTTONS]`: move by DX, DY (screen pixels, y down) with
///   BUTTONS held (1 left, 2 right, 4 middle).
/// - `click [BUTTON]`: press and release (default 1, left).
/// - `status`: what the display is showing, and the GBE registers behind it.
/// Where the CPU was at the end of its last batch, and how many batches it
/// has run: enough to tell a guest spinning in one place from one that is
/// still moving, without stopping it.
pub(crate) static LAST_PC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static BATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The general registers at the same moment, then Status, Cause, EPC and
/// BadVAddr: what a polling loop is polling.
static LAST_REGS: [std::sync::atomic::AtomicU64; 42] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 42];

/// The control commands on the shared monitor, as `o2 <command> [args]`:
/// the same commands, and replies, as the control port's.
struct O2Control {
    bus: Arc<Ip32Bus>,
    rgba: Mutex<Vec<u8>>,
}

impl O2Control {
    fn new(bus: Arc<Ip32Bus>) -> Self {
        Self { bus, rgba: Mutex::new(vec![0u8; 2048 * 2048 * 4]) }
    }
}

impl Device for O2Control {
    fn step(&self, _cycles: u64) {}
    fn stop(&self) {}
    fn start(&self) {}
    fn is_running(&self) -> bool { true }
    fn get_clock(&self) -> u64 { 0 }

    fn register_commands(&self) -> Vec<(String, String)> {
        vec![("o2".to_string(), "O2 control: o2 shot PATH | status | pixel X Y | type TEXT | key|keydown|keyup NAME | \
            mouse DX DY [BUTTONS] | click [BUTTON] | pc | regs | peek ADDR LEN PATH | \
            retrace PATH | scsi PATH | unmapped | ps2log PATH | crimelog PATH".to_string())]
    }

    fn execute_command(&self, cmd: &str, args: &[&str], mut w: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        if cmd != "o2" {
            return Err("Command not found".to_string());
        }
        let Some((sub, rest)) = args.split_first() else {
            return Err("Usage: o2 <command> [args] (help o2)".to_string());
        };
        let reply = control_command(&self.bus, sub, &rest.join(" "), &mut self.rgba.lock());
        writeln!(w, "{reply}").map_err(|e| e.to_string())
    }
}

fn control_server(listener: std::net::TcpListener, bus: Arc<Ip32Bus>) {
    use std::io::{BufRead, BufReader, Write};
    for conn in listener.incoming().flatten() {
        let bus = bus.clone();
        let _ = std::thread::Builder::new().name("O2-Control-Client".into()).spawn(move || {
            let mut out = match conn.try_clone() {
                Ok(o) => o,
                Err(_) => return,
            };
            let mut rgba = vec![0u8; 2048 * 2048 * 4];
            for line in BufReader::new(conn).lines().map_while(Result::ok) {
                let (cmd, arg) = line.trim_end().split_once(' ').unwrap_or((line.trim_end(), ""));
                let reply = control_command(&bus, cmd, arg, &mut rgba);
                if writeln!(out, "{reply}").is_err() {
                    break;
                }
            }
        });
    }
}

fn control_command(bus: &Ip32Bus, cmd: &str, arg: &str, rgba: &mut [u8]) -> String {
    let kb = bus.mace.ps2.devices();
    match cmd {
        "shot" => match bus.render_screen(rgba) {
            Some((w, h)) => match crate::disp::save_png_rgba8(arg, &rgba[..(w * h * 4) as usize], w, h) {
                Ok(()) => format!("ok {w}x{h}"),
                Err(e) => format!("err {e}"),
            },
            None => "err display off".into(),
        },
        "pc" => format!(
            "ok pc 0x{:08x} after {} batches",
            LAST_PC.load(Ordering::Relaxed),
            BATCHES.load(Ordering::Relaxed)
        ),
        // `crimelog PATH`: the guest's recent CRIME interrupt-register
        // accesses, oldest first.
        "crimelog" => match std::fs::write(arg.trim(), bus.crime.regs.irqlog_text()) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err {e}"),
        },
        "regs" => {
            let r: Vec<u64> = LAST_REGS.iter().map(|v| v.load(Ordering::Relaxed)).collect();
            let mut s = format!("ok pc 0x{:08x}", LAST_PC.load(Ordering::Relaxed));
            for (i, v) in r[..32].iter().enumerate() {
                s.push_str(&format!(" r{i}=0x{v:x}"));
            }
            s.push_str(&format!(" sr=0x{:x} cause=0x{:x} epc=0x{:x} badva=0x{:x}", r[32], r[33], r[34], r[35]));
            s.push_str(&format!(" count=0x{:x} compare=0x{:x} irq=0x{:x} ticks={} cycles={} config=0x{:x}", r[36], r[37], r[38], r[39], r[40], r[41]));
            s
        }
        // `peek ADDR LEN PATH`: LEN bytes of the physical address space from
        // ADDR (hex), read as the CPU would, written to PATH as raw big-endian
        // bytes.
        "peek" => {
            let mut it = arg.split_whitespace();
            let addr = it.next().and_then(|a| u32::from_str_radix(a.trim_start_matches("0x"), 16).ok());
            let len = it.next().and_then(|l| l.parse::<u32>().ok());
            match (addr, len, it.next()) {
                (Some(a), Some(n), Some(path)) => {
                    let mut out = Vec::with_capacity(n as usize);
                    for i in (0..n.min(1 << 20)).step_by(4) {
                        out.extend_from_slice(&bus.read32(a.wrapping_add(i) as u64).data.to_be_bytes());
                    }
                    match std::fs::write(path, out) {
                        Ok(()) => "ok".into(),
                        Err(e) => format!("err {e}"),
                    }
                }
                _ => "err usage: peek ADDR LEN PATH".into(),
            }
        }
        // `ps2log PATH`: the PS/2 ports' recent traffic, `1>f4` a byte to
        // port 1's device, `1<fa` one from it.
        "ps2log" => match std::fs::write(arg, bus.mace.ps2.traffic_timed()) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err {e}"),
        },
        "retrace" => match std::fs::write(arg, bus.crime.re.trace_text()) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err {e}"),
        },
        "scsi" => {
            let mut text = String::new();
            for (n, c) in bus.scsi.lock().unwrap().iter().enumerate() {
                for line in c.notes() {
                    text.push_str(&format!("{n}: {line}\n"));
                }
            }
            match std::fs::write(arg, text) {
                Ok(()) => "ok".into(),
                Err(e) => format!("err {e}"),
            }
        }
        "unmapped" => {
            // Bus accesses that hit nothing (the first 64 distinct ones):
            // what a guest saw as a bus error, with the PC that made it.
            let u = bus.unmapped();
            let list: Vec<String> = u.iter().map(|(a, w, pc)| format!("{} {a:#010x} pc {pc:#010x}", if *w { "w" } else { "r" })).collect();
            format!("ok {} {}", u.len(), list.join("; "))
        }
        "pixel" => {
            // pixel X Y: what scan-out makes of one point (frame-buffer word,
            // DID, mode-table entry, colour map, overlay).
            let v: Vec<usize> = arg.split_whitespace().filter_map(|t| t.parse().ok()).collect();
            if v.len() != 2 {
                return "err pixel X Y".into();
            }
            *bus.gbe.probe.lock().unwrap() = (Some((v[0], v[1])), "not on screen".into());
            bus.render_screen(rgba);
            let mut p = bus.gbe.probe.lock().unwrap();
            p.0 = None;
            format!("ok {}", p.1)
        }
        "status" => {
            let shown = bus.render_screen(rgba);
            format!("ok display {shown:?}; {}", bus.gbe.summary())
        }
        _ if kb.is_none() => "err no keyboard or mouse (graphics = false)".into(),
        "type" => {
            let kb = kb.unwrap();
            let text = arg.replace("\\r", "\r").replace("\\e", "\x1b").replace("\\b", "\x08").replace("\\t", "\t");
            let mut n = 0;
            for c in text.chars() {
                if c == '\t' {
                    kb.push_kb(winit::keyboard::KeyCode::Tab, true);
                    kb.push_kb(winit::keyboard::KeyCode::Tab, false);
                    n += 1;
                } else if kb.type_char(c) {
                    n += 1;
                }
            }
            format!("ok {n}")
        }
        "key" | "keydown" | "keyup" => match crate::ip32_screen::keycode(arg.trim()) {
            Some(k) => {
                let kb = kb.unwrap();
                if cmd != "keyup" {
                    kb.push_kb(k, true);
                }
                if cmd != "keydown" {
                    kb.push_kb(k, false);
                }
                "ok".into()
            }
            None => format!("err no key {arg:?}"),
        },
        "mouse" => {
            let v: Vec<i32> = arg.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() < 2 {
                return "err mouse DX DY [BUTTONS]".into();
            }
            let buttons = v.get(2).copied().unwrap_or(0) as u8;
            let kb = kb.unwrap();
            // A PS/2 packet carries at most 255 either way: send big moves in steps.
            let (mut dx, mut dy) = (v[0], v[1]);
            loop {
                let (sx, sy) = (dx.clamp(-200, 200), dy.clamp(-200, 200));
                kb.push_mouse_input(buttons, sx, sy, 0);
                dx -= sx;
                dy -= sy;
                if dx == 0 && dy == 0 {
                    break;
                }
            }
            "ok".into()
        }
        "click" => {
            let b: u8 = arg.trim().parse().unwrap_or(1);
            let kb = kb.unwrap();
            kb.push_mouse_input(b, 0, 0, 0);
            kb.push_mouse_input(0, 0, 0, 0);
            "ok".into()
        }
        _ => format!("err unknown command {cmd:?}"),
    }
}


#[cfg(test)]
mod drop_tests {
    use super::*;
    use crate::config::{GraphicsBoard, MachineProfile};
    use crate::display_stop_tests::{big_stack, config, scratch};

    /// A dropped O2 frees its bus and with it the decode table: a process may
    /// have only 64, and a test binary builds many machines.
    #[test]
    fn a_dropped_o2_frees_its_bus_and_decode_table() {
        let dir = scratch("o2-drop");
        let cfg = config(&dir, MachineProfile::O2Ip32, GraphicsBoard::Newport);
        let machine = big_stack(move || O2Machine::new(&cfg).expect("O2"));
        let phys = Arc::downgrade(&machine.bus.phys);
        drop(machine);
        assert_eq!(phys.strong_count(), 0, "the bus outlives its machine");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;
    use crate::exit_persistence_tests::Machine as Kind;
    use crate::traits::{Resettable, Saveable};
    use crate::topology::Family;
    use crate::workstation_tests::with_shared;

    fn o2<R>(f: impl FnOnce(&mut O2Machine) -> R) -> R {
        with_shared(Kind::O2, |m| f(m.as_any_mut().downcast_mut::<O2Machine>().expect("an O2")))
    }

    /// Both SCSI controllers start and stop with the bus, which is the only
    /// entry the machine registers for them.
    #[test]
    fn o2_lifecycle_reaches_both_controllers() {
        o2(|m| {
            let running = |m: &O2Machine| m.bus.scsi.lock().unwrap().iter().map(|c| c.is_running()).collect::<Vec<_>>();
            assert_eq!(running(m), [false, false]);
            m.restart_peripherals();
            assert_eq!(running(m), [true, true]);
            m.stop();
            assert_eq!(running(m), [false, false]);
        });
    }

    /// The files a snapshot has always been: the CPU, the bus (`o2`), the boot
    /// hook (`boot`), memory in the chunk store, the flash, and each
    /// controller's overlays.
    #[test]
    fn o2_saved_files_keep_their_names() {
        o2(|m| {
            let dir = std::env::temp_dir().join(format!("iris-o2-names-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let before = std::mem::replace(&mut m.saves, dir.clone());
            let written = m.write_snapshot("s");
            m.saves = before;
            written.unwrap();
            let mut names: Vec<String> = std::fs::read_dir(dir.join("s")).unwrap().flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            let _ = std::fs::remove_dir_all(&dir);
            assert_eq!(names, ["boot.bin", "chunks.bin", "cow0.toml", "cow1.toml", "cpu.bin", "ctrl0", "ctrl1", "flash.bin", "o2.bin", "snapshot.toml"]);
        });
    }

    /// A cold reset puts back the devices with a power-on state and nothing
    /// else: CRIME's registers and a MACE UART return to their reset values,
    /// while memory and the flash keep what they hold.
    #[test]
    fn power_on_resets_what_the_bus_resets() {
        o2(|m| {
            let table = |m: &O2Machine| m.devices.save()["o2"].clone();
            let clean = table(m);
            let (word, flash) = (RAM_BASE as u64 + 0x100, m.bus.flash_bytes());
            let old = m.bus.phys.read32(word).data;
            assert_eq!(m.bus.phys.write32(word, 0x600d_f00d), crate::traits::BUS_OK);

            // Dirty the CRIME registers and a MACE UART.
            let mut dirty = clean.clone();
            let crime = crate::devices::testing::dirtied(&clean["crime"]["crime"], &[]).expect("a CRIME register");
            dirty["crime"].as_table_mut().unwrap().insert("crime".into(), crime);
            let com0 = crate::devices::testing::dirtied(&clean["mace"]["com0"], &[]).expect("a UART register");
            dirty["mace"].as_table_mut().unwrap().insert("com0".into(), com0);
            m.bus.load_state(&dirty).unwrap();
            assert_ne!(table(m)["crime"]["crime"], clean["crime"]["crime"]);
            assert_ne!(table(m)["mace"]["com0"], clean["mace"]["com0"]);

            Resettable::power_on(&*m.bus);
            let after = table(m);
            assert_eq!(after["crime"]["crime"], clean["crime"]["crime"], "CRIME's registers are back");
            assert_eq!(after["mace"]["com0"], clean["mace"]["com0"], "the UART is back");
            assert_eq!(m.bus.phys.read32(word).data, 0x600d_f00d, "memory is as the guest left it");
            assert_eq!(m.bus.flash_bytes(), flash, "the flash is as it was");
            // Put the shared machine back.
            m.bus.phys.write32(word, old);
            m.bus.load_state(&clean).unwrap();
        });
    }

    #[test]
    fn boot_state_round_trips() {
        let a = BootState::new();
        a.post_done.store(false, Ordering::Relaxed);
        a.inside.store(true, Ordering::Relaxed);
        a.cu1_saved.store(0x1234_5000, Ordering::Relaxed);
        let b = BootState::new();
        b.load_state(&a.save_state()).unwrap();
        assert!(!b.post_done.load(Ordering::Relaxed) && b.inside.load(Ordering::Relaxed));
        assert_eq!(b.cu1_saved.load(Ordering::Relaxed), 0x1234_5000);
        assert_eq!(b.save_state(), a.save_state());
    }
}

/// Disks are placed through the socket map.
#[cfg(test)]
mod placement_tests {
    use super::*;
    use crate::attach::attach_device;
    use crate::config::Placement;
    use crate::exit_persistence_tests::Machine;
    use crate::topology::Family;
    use crate::workstation_tests::with_shared;

    fn loc(s: &str) -> Location {
        s.parse().unwrap()
    }

    fn filled() -> Topology {
        let t = Topology::new();
        O2Machine::fill(&MachineConfig::default(), &t);
        t
    }

    /// Every controller the bus made is on its row, and the shared O2's
    /// disks sit on the internal controller.
    #[test]
    fn o2_rows_bind_every_handle() {
        with_shared(Machine::O2, |m| {
            let bound = m.topology().buses::<ScsiPort>();
            let at: Vec<String> = bound.iter().map(|b| b.0.to_string()).collect();
            assert_eq!(at, ["pci0.1", "pci0.2"]);
            let disks: Vec<String> = m.disks().records().iter().map(|r| r.at.to_string()).collect();
            assert_eq!(disks, ["pci0.1.t1", "pci0.1.t2", "pci0.1.t3", "pci0.1.t4", "pci0.1.t5"]);
            assert!(bound[0].1.bus().has_target(5));
        });
    }

    /// Option slots belong to the PCI core; until then the map has the bus
    /// and the two onboard controllers only.
    #[test]
    fn o2_fill_has_no_option_slot_yet() {
        let t = filled();
        let e = t.claim(&loc("pci0.3"), "aic7880", Family::Pci, false, "").unwrap_err();
        assert!(e.contains("no such location"), "{e}");
        assert!(t.claim(&loc("pci0.2.t7"), "hdd", Family::Scsi, false, "").is_ok());
        assert!(t.claim(&loc("pci0.2.t8"), "hdd", Family::Scsi, false, "").is_err(), "the limit stays 7");
    }

    /// The Ethernet is built in: the claim says which row refused.
    #[test]
    fn a_daynaport_on_the_o2_is_refused_by_the_claim_with_the_row_named() {
        let t = filled();
        let p = Placement { device: "daynaport".into(), at: loc("pci0.1.t3"), opts: Default::default() };
        let e = attach_device(&p, &MachineConfig::default(), None, &t).unwrap_err();
        assert_eq!(e, "error: pci0.1.t3: daynaport is not supported on this SCSI bus");
    }

    /// `[scsi.20]` names controller 1, target 4; `controller = 1` moves a low key.
    #[test]
    fn legacy_controller_one_lands_on_pci0_2() {
        let t = filled();
        let mut cfg: MachineConfig = toml::from_str("[scsi.20]\npath = \"x.img\"\n[scsi.1]\ncontroller = 1\npath = \"y.img\"\n").unwrap();
        cfg.migrate(&t).unwrap();
        let ats: Vec<String> = cfg.placements().unwrap().iter().map(|p| p.at.to_string()).collect();
        assert_eq!(ats, ["pci0.2.t1", "pci0.2.t4"]);
    }
}
