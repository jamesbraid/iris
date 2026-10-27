//! The Octane (IP30) as a machine: an R10000 on HEART, the Crossbow, and
//! the widgets behind it, with iris's CPU thread, monitor and front ends.
//!
//! Selected with `[machine] profile = "octane_ip30"`; the shared keys
//! configure it (`prom`, `banks`, `[scsi.N]`, `[network]`, the serial
//! ports) and `[octane]` holds what only an Octane has. See
//! `docs/octane-ip30.md`.
//!
//! The processor's physical map, from OpenBSD's `ip30.h` and Linux's
//! `ip30-xtalk.c` (the Origami contract `ip30-machine.md` cites them):
//!
//! | Range | What |
//! |---|---|
//! | `0x0000_0000`, 16 KiB | low memory (below) |
//! | `0x0ff0_0000` | HEART's registers |
//! | `0x1000_0000 + (w << 24)` | widget `w`'s 16 MiB small window |
//! | `0x2000_0000` up | SDRAM through HEART's bank descriptors |
//! | `w << 36` | widget `w`'s 64 GiB large window |
//!
//! Widget 0 is the Crossbow, 8 HEART, 15 the onboard Bridge (whose window
//! holds the boot flash at 0x1fc0_0000, the reset vector), 9 to 12 the
//! graphics ports.
//!
//! Low memory follows the Origami model's functional approximation
//! (`ip30-machine.md`, "HEART low memory"): 16 KiB of distinct RAM at
//! physical 0, zero at reset, and the first 16 KiB above 0x2000_0000 a bus
//! error, with SDRAM starting at 0x2000_4000. The fault is enforced on the
//! bus path; a cached access through the CPU's direct RAM window does not
//! see it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::{CpuModel, MachineConfig};
use crate::cpu::mips_cache_shadow::R10000ShadowCache;
use crate::cpu::mips_exec::{CpuDevice, MipsCpu, MipsCpuConfig, MipsExecutor};
use crate::cpu::mips_tlb::MipsTlb;
use crate::hptimer::TimerManager;
use crate::ip30::heart::{Heart, HeartConfig, PIU_BASE, PIU_SIZE, RAM_BASE};
use crate::ip30::xbow::{Xbow, REG_SPAN};
use crate::ip30::xio::{widget_small_base, WIDGET_BRIDGE, WIDGET_HEART, WIDGET_SMALL_SIZE, WIDGET_XBOW};
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, Device, MachineEvent, Signal, BUS_ERR, BUS_OK};
use crate::workstation::{MachineRef, MachineSlot, Workstation};

/// Low memory at physical zero.
pub const LOW_RAM_SIZE: u64 = 0x4000;
/// The faulting start of SDRAM space.
const HIGH_GUARD: std::ops::Range<u64> = RAM_BASE as u64..RAM_BASE as u64 + 0x4000;
const SMALL_WINDOWS: std::ops::Range<u64> = 0x1000_0000..0x2000_0000;
const LARGE_SHIFT: u32 = 36;
/// The boot flash's offset in the Bridge's window: 0x1fc0_0000.
pub const FLASH_OFFSET: u64 = 0x00c0_0000;
pub const FLASH_SIZE: u64 = 0x0020_0000;

/// The Octane's 195 MHz R10000 (`ip30-machine.md`): Count at half that.
pub const OCTANE_COUNT_HZ: u64 = 97_500_000;
/// R10000 revision 2.7 (`ip30-r10000.md`, inferred from inventory text).
pub const OCTANE_PRID: u32 = 0x0927;
/// The Crossbow on the Octane reports revision 4.
const XBOW_REVISION: u32 = 4;

/// Where the CPU was at the end of its last batch, for the unmapped log.
static LAST_PC: AtomicU64 = AtomicU64::new(0);

/// The PROM image read-only in the flash window, 0xff past its end. Stands
/// in for the Bridge's flash until a machine has a Bridge.
pub struct RawFlash {
    bytes: Vec<u8>,
}

impl RawFlash {
    pub fn new(prom: &[u8]) -> Self {
        let mut bytes = vec![0xffu8; FLASH_SIZE as usize];
        let n = prom.len().min(bytes.len());
        bytes[..n].copy_from_slice(&prom[..n]);
        Self { bytes }
    }

    fn get(&self, off: u64, n: usize) -> Option<u64> {
        let o = off.checked_sub(FLASH_OFFSET)? as usize;
        let b = self.bytes.get(o..o + n)?;
        Some(b.iter().fold(0u64, |v, &x| (v << 8) | x as u64))
    }
}

impl BusDevice for RawFlash {
    fn read8(&self, a: u64) -> BusRead8 {
        self.get(a & 0xff_ffff, 1).map_or(BusRead8::err(), |v| BusRead8::ok(v as u8))
    }
    fn read16(&self, a: u64) -> BusRead16 {
        self.get(a & 0xff_ffff, 2).map_or(BusRead16::err(), |v| BusRead16::ok(v as u16))
    }
    fn read32(&self, a: u64) -> BusRead32 {
        self.get(a & 0xff_ffff, 4).map_or(BusRead32::err(), |v| BusRead32::ok(v as u32))
    }
    fn read64(&self, a: u64) -> BusRead64 {
        self.get(a & 0xff_ffff, 8).map_or(BusRead64::err(), BusRead64::ok)
    }
}

/// What an address is, on the processor's side of HEART.
enum Target {
    LowRam(usize),
    Piu(u32),
    Xbow(u32),
    HeartWidget(u32),
    /// A widget's window, at the address as the processor issued it.
    Widget(u8),
    Ram,
    None,
}

/// The processor's physical bus.
pub struct Ip30Bus {
    pub heart: Arc<Heart>,
    pub xbow: Arc<Xbow>,
    low: Mutex<Vec<u8>>,
    /// Widgets 8 to 15 by number; HEART's own slot stays empty. Each takes
    /// the full physical address and masks it to its window.
    widgets: [Option<Arc<dyn BusDevice>>; 16],
    /// Large windows that reach the same device as the small one.
    large: [bool; 16],
    /// Accesses that hit nothing, first 64 distinct: `(addr, write, pc)`.
    unmapped: Mutex<Vec<(u64, bool, u64)>>,
}

impl Ip30Bus {
    pub fn new(heart: Arc<Heart>, xbow: Arc<Xbow>) -> Self {
        Self {
            heart,
            xbow,
            low: Mutex::new(vec![0; LOW_RAM_SIZE as usize]),
            widgets: Default::default(),
            large: [false; 16],
            unmapped: Mutex::new(Vec::new()),
        }
    }

    /// Put `dev` on widget `w`'s port. `large` also routes the widget's
    /// 64 GiB window to it, at the offset within that window.
    pub fn attach_widget(&mut self, w: u8, dev: Arc<dyn BusDevice>, large: bool) {
        self.widgets[w as usize] = Some(dev);
        self.large[w as usize] = large;
    }

    pub fn unmapped(&self) -> Vec<(u64, bool, u64)> {
        self.unmapped.lock().unwrap().clone()
    }

    fn miss(&self, addr: u64, write: bool) {
        let pc = LAST_PC.load(Ordering::Relaxed);
        let mut u = self.unmapped.lock().unwrap();
        if u.len() < 64 && !u.iter().any(|&(a, w, _)| a == addr && w == write) {
            if std::env::var_os("IRIS_IP30_TRACE").is_some() {
                eprintln!("ip30: unmapped {} at {addr:#x} (pc {pc:#x})", if write { "write" } else { "read" });
            }
            u.push((addr, write, pc));
        }
    }

    fn decode(&self, addr: u64) -> Target {
        if addr < LOW_RAM_SIZE {
            return Target::LowRam(addr as usize);
        }
        if (PIU_BASE as u64..(PIU_BASE + PIU_SIZE) as u64).contains(&addr) {
            return Target::Piu((addr - PIU_BASE as u64) as u32);
        }
        if SMALL_WINDOWS.contains(&addr) {
            let w = ((addr >> 24) & 0xf) as u8;
            let off = (addr & (WIDGET_SMALL_SIZE as u64 - 1)) as u32;
            return match w {
                WIDGET_XBOW if off < REG_SPAN => Target::Xbow(off),
                // The processor reaches its own HEART without a trip
                // through the switch; the rest of the window has no route.
                WIDGET_HEART if off < crate::ip30::heart::WIDGET_REG_SIZE => Target::HeartWidget(off),
                w if self.widgets[w as usize].is_some() && self.xbow.carries(w) => Target::Widget(w),
                _ => Target::None,
            };
        }
        if HIGH_GUARD.contains(&addr) {
            return Target::None;
        }
        let w = (addr >> LARGE_SHIFT) as usize;
        if w >= 8 {
            return if w < 16 && self.large[w] && self.xbow.carries(w as u8) { Target::Widget(w as u8) } else { Target::None };
        }
        if addr >= RAM_BASE as u64 {
            return Target::Ram;
        }
        Target::None
    }

    fn read(&self, addr: u64, n: usize) -> Option<u64> {
        match self.decode(addr) {
            Target::LowRam(o) => {
                let l = self.low.lock().unwrap();
                let b = l.get(o..o + n)?;
                Some(b.iter().fold(0u64, |v, &x| (v << 8) | x as u64))
            }
            Target::Piu(o) => self.heart.piu_read(o, n),
            Target::Xbow(o) => self.xbow.read(o, n),
            Target::HeartWidget(o) => self.heart.widget_read(o, n),
            Target::Ram => self.heart.ram_read(addr, n),
            Target::Widget(_) | Target::None => None,
        }
    }

    fn write(&self, addr: u64, n: usize, val: u64) -> bool {
        match self.decode(addr) {
            Target::LowRam(o) => {
                let mut l = self.low.lock().unwrap();
                let Some(b) = l.get_mut(o..o + n) else { return false };
                for (k, x) in b.iter_mut().enumerate() {
                    *x = (val >> (8 * (n - 1 - k))) as u8;
                }
                true
            }
            Target::Piu(o) => self.heart.piu_write(o, n, val),
            Target::Xbow(o) => self.xbow.write(o, n, val),
            Target::HeartWidget(o) => self.heart.widget_write(o, n, val),
            Target::Ram => self.heart.ram_write(addr, n, val),
            Target::Widget(_) | Target::None => false,
        }
    }

    fn widget(&self, addr: u64) -> Option<&Arc<dyn BusDevice>> {
        match self.decode(addr) {
            Target::Widget(w) => self.widgets[w as usize].as_ref(),
            _ => None,
        }
    }

    /// Back to power-on: low memory zero (a synthetic choice, as in the
    /// model), the Crossbow and HEART reset.
    pub fn power_on(&self) {
        use crate::traits::Resettable;
        self.low.lock().unwrap().fill(0);
        self.xbow.power_on();
        self.heart.power_on();
    }
}

macro_rules! bus_access {
    ($read:ident, $write:ident, $r:ident, $t:ty, $n:expr) => {
        fn $read(&self, addr: u64) -> $r {
            if let Some(d) = self.widget(addr) {
                return d.$read(addr);
            }
            match self.read(addr, $n) {
                Some(v) => $r::ok(v as $t),
                None => {
                    self.miss(addr, false);
                    $r::err()
                }
            }
        }
        fn $write(&self, addr: u64, val: $t) -> u32 {
            if let Some(d) = self.widget(addr) {
                return d.$write(addr, val);
            }
            if self.write(addr, $n, val as u64) {
                BUS_OK
            } else {
                self.miss(addr, true);
                BUS_ERR
            }
        }
    };
}

impl BusDevice for Ip30Bus {
    bus_access!(read8, write8, BusRead8, u8, 1);
    bus_access!(read16, write16, BusRead16, u16, 2);
    bus_access!(read32, write32, BusRead32, u32, 4);
    bus_access!(read64, write64, BusRead64, u64, 8);

    /// Memory pages have generations jitv2 can compile against; nothing
    /// else does.
    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, addr: u64) -> *const AtomicU64 {
        let m = self.heart.mem.lock().unwrap();
        match m.decode(addr) {
            crate::ip30::heart::Decode::Ram(o) if !HIGH_GUARD.contains(&addr) => m.gen_ptr(o),
            _ => std::ptr::null(),
        }
    }
}

/// The monitor's view of the board: HEART's command and the unmapped log.
impl Device for Ip30Bus {
    fn step(&self, _cycles: u64) {}
    fn stop(&self) {}
    fn start(&self) {}
    fn is_running(&self) -> bool {
        true
    }
    fn get_clock(&self) -> u64 {
        0
    }

    fn register_commands(&self) -> Vec<(String, String)> {
        let mut c = self.heart.register_commands();
        c.push(("ip30".into(), "Octane board: ip30 unmapped | xbow".into()));
        c
    }

    fn execute_command(&self, cmd: &str, args: &[&str], mut w: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        match cmd {
            "heart" => self.heart.execute_command(cmd, args, w),
            "ip30" => {
                let text: String = match args.first().copied().unwrap_or("unmapped") {
                    "unmapped" => self
                        .unmapped()
                        .iter()
                        .map(|(a, wr, pc)| format!("{} {a:#x} pc {pc:#x}\n", if *wr { "w" } else { "r" }))
                        .collect(),
                    "xbow" => self
                        .xbow
                        .unsupported()
                        .iter()
                        .map(|(o, s, wr)| format!("{} {s} bytes at {o:#x}\n", if *wr { "write" } else { "read" }))
                        .collect(),
                    other => return Err(format!("ip30: unknown subcommand {other}")),
                };
                w.write_all(text.as_bytes()).map_err(|e| e.to_string())
            }
            _ => Err("Command not found".into()),
        }
    }
}

/// The CPU's interrupt pins as a controller: line n is IPn.
struct CpuIrq(Arc<dyn Device>);

impl crate::irq::InterruptController for CpuIrq {
    fn set_line(&self, line: u32, level: bool) {
        self.0.signal(Signal::Interrupt(line, level));
    }
}

/// Bank sizes in bytes from `banks` (MB per HEART bank).
pub fn heart_banks(cfg: &MachineConfig) -> [u64; 4] {
    let mut b = [0u64; 4];
    for (i, mb) in cfg.banks[..4].iter().enumerate() {
        b[i] = (*mb as u64) << 20;
    }
    b
}

pub struct OctaneMachine {
    pub bus: Arc<Ip30Bus>,
    cpu: Arc<dyn CpuDevice>,
    timer_manager: Arc<TimerManager>,
    running: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    monitor: Arc<crate::monitor::Monitor>,
    monitor_port: u16,
    ci_serial: Option<Arc<crate::dev::z85c30::CiSerialBackend>>,
    #[allow(dead_code)]
    events: std::sync::mpsc::SyncSender<MachineEvent>,
    event_rx: Option<std::sync::mpsc::Receiver<MachineEvent>>,
    controller_up: Arc<AtomicBool>,
    controller_slot: Option<MachineSlot>,
    lifecycle: crate::machine_state::GuestState,
}

impl OctaneMachine {
    pub fn new(cfg: &MachineConfig) -> Result<Self, String> {
        if cfg.machine.cpu != CpuModel::R10000 {
            return Err("an Octane has an R10000 ([machine] cpu = \"r10000\")".into());
        }
        let prom = std::fs::read(&cfg.prom)
            .map_err(|e| format!("Octane PROM {}: {e} (set `prom` to the IP30 PROM image)", cfg.prom))?;
        if prom.len() as u64 > FLASH_SIZE {
            return Err(format!("Octane PROM {} is {} bytes; the flash holds {}", cfg.prom, prom.len(), FLASH_SIZE));
        }
        let banks = heart_banks(cfg);
        let heart = Arc::new(Heart::new(HeartConfig::octane(banks), crate::ip30::heart::host_clock())?);
        let xbow = Arc::new(Xbow::new(XBOW_REVISION, (1 << WIDGET_HEART) | (1 << WIDGET_BRIDGE)));
        xbow.connect_heart(heart.clone());
        let mut bus = Ip30Bus::new(heart.clone(), xbow.clone());
        bus.attach_widget(WIDGET_BRIDGE, Arc::new(RawFlash::new(&prom)), false);
        let bus = Arc::new(bus);
        eprintln!(
            "iris: Octane with {} MB ({}), serial console",
            banks.iter().sum::<u64>() >> 20,
            banks.iter().filter(|&&b| b != 0).map(|b| format!("{}", b >> 20)).collect::<Vec<_>>().join("+")
        );

        let timer_manager = Arc::new(TimerManager::new());
        let count_hz = cfg.clock.fixed_mhz.map_or(OCTANE_COUNT_HZ, |mhz| (mhz * 1_000_000.0) as u64);
        let cpu = Self::build_cpu(bus.clone(), &timer_manager, count_hz);

        #[cfg(feature = "jitv2")]
        {
            let cpu_device: Arc<dyn Device> = cpu.clone().as_device();
            let jit = cpu.jitv2();
            let mut g = jit.lock();
            g.compile_queue.set_thread_count(cfg.jitv2.threads.max(1));
            g.compile_queue.set_cpu(Arc::downgrade(&cpu_device));
            g.compile_queue.set_owner(Arc::downgrade(&jit));
            #[cfg(not(feature = "jitv2_lockstep"))]
            {
                let stats = g.stats.clone();
                g.compile_queue.start(bus.clone() as Arc<dyn BusDevice>, stats);
            }
            std::mem::forget(cpu_device);
        }

        // The shadow cache and the JIT read RAM straight from HEART's
        // window; descriptor writes remap it on the CPU thread.
        {
            use crate::ppmem::MappedMemory;
            let mem = heart.mem.lock().unwrap();
            let sp = mem.space();
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
        }

        // HEART's levels 0 to 4 are the CPU's IP2 to IP6.
        let pins: Arc<dyn crate::irq::InterruptController> = Arc::new(CpuIrq(cpu.clone().as_device()));
        heart.connect_cpu(std::array::from_fn(|i| crate::irq::IrqLine::new(pins.clone(), i as u32 + 2)));

        let mut monitor = crate::monitor::Monitor::new();
        monitor.register_device(crate::devlog::init_devlog());
        monitor.register_device(cpu.clone().as_device());
        monitor.register_device(bus.clone());
        let monitor_port = cfg.monitor_port.unwrap_or(crate::monitor::DEFAULT_PORT);
        let ci_serial = cfg.ci.then(|| Arc::new(crate::dev::z85c30::CiSerialBackend::new()));
        let (events, event_rx) = std::sync::mpsc::sync_channel(4);
        Ok(Self {
            bus,
            cpu,
            timer_manager,
            running: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            monitor: Arc::new(monitor),
            monitor_port,
            ci_serial,
            events,
            event_rx: Some(event_rx),
            controller_up: Arc::new(AtomicBool::new(false)),
            controller_slot: None,
            lifecycle: crate::machine_state::GuestState::Idle,
        })
    }

    fn build_cpu(sysad: Arc<dyn BusDevice>, tm: &Arc<TimerManager>, count_hz: u64) -> Arc<dyn CpuDevice> {
        let cfg = MipsCpuConfig::for_model::<R10000ShadowCache>();
        let tlb = MipsTlb::new(cfg.tlb_entries);
        let mut exec: MipsExecutor<MipsTlb, R10000ShadowCache> = MipsExecutor::new(sysad, tlb, &cfg);
        exec.core.set_timer_manager(tm.clone());
        exec.core.set_count_hz(count_hz);
        exec.core.reset_prid = OCTANE_PRID;
        exec.core.cp0_prid = OCTANE_PRID;
        // IRIX zeroes Count and arms Compare a few instructions later; see
        // `MipsCore::count_write_samples`.
        exec.core.count_write_samples = true;
        // The R10000 drives 40 physical address bits; HEART decodes them
        // all, and the attribute bits above fold away.
        exec.phys_mask = (1 << 40) - 1;
        exec.batch_hook = Some(Box::new(|exec: &mut MipsExecutor<MipsTlb, R10000ShadowCache>| {
            LAST_PC.store(exec.core.pc, Ordering::Relaxed);
        }));
        Arc::new(MipsCpu::new(exec))
    }

    pub fn start(&mut self) {
        if let Ok(starting) = self.lifecycle.transition(crate::machine_state::GuestState::Starting) {
            self.lifecycle = starting;
        }
        self.running.store(true, Ordering::SeqCst);
        self.monitor.clone().start_server(format!("127.0.0.1:{}", self.monitor_port));
        if self.ci_serial.is_none() {
            self.cpu.clone().as_device().start();
        }
    }

    pub fn stop(&mut self) {
        self.cpu.clone().as_device().stop();
        if self.lifecycle != crate::machine_state::GuestState::Idle {
            self.lifecycle = crate::machine_state::GuestState::Halted;
        }
    }

    pub fn reset(&mut self) {
        self.stop();
        crate::traits::Resettable::power_on(&*self.cpu);
        self.bus.power_on();
    }

    pub fn machine_slot(&mut self) -> MachineSlot {
        let ptr = self as *mut OctaneMachine as *mut dyn Workstation;
        let slot = self.controller_slot.get_or_insert_with(|| Arc::new(parking_lot::Mutex::new(MachineRef(ptr))));
        debug_assert!(std::ptr::addr_eq(slot.lock().0, ptr), "OctaneMachine moved after its slot was handed out");
        slot.clone()
    }
}

impl Drop for OctaneMachine {
    fn drop(&mut self) {
        if let Some(slot) = self.controller_slot.take() {
            slot.lock().0 = MachineRef::null();
        }
        self.running.store(false, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        self.monitor.shutdown();
    }
}

impl Workstation for OctaneMachine {
    fn start(&mut self) {
        OctaneMachine::start(self)
    }
    fn stop(&mut self) {
        OctaneMachine::stop(self)
    }
    fn reset(&mut self) {
        OctaneMachine::reset(self)
    }
    fn restart_peripherals(&mut self) {}
    fn cpu_start(&self) {
        self.cpu.clone().as_device().start()
    }
    fn cpu_is_running(&self) -> bool {
        self.cpu.clone().as_device().is_running()
    }
    fn machine_slot(&mut self) -> MachineSlot {
        OctaneMachine::machine_slot(self)
    }
    fn register_system_controller(&mut self) {
        let slot = self.machine_slot();
        let events = self.event_rx.take();
        assert!(events.is_some(), "register_system_controller called twice");
        crate::workstation::register_system_controller(&self.monitor, slot, events);
        self.controller_up.store(true, Ordering::SeqCst);
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
        Arc::new(crate::dev::ps2::Ps2Controller::new(None))
    }
    fn get_timer_manager(&self) -> Arc<TimerManager> {
        self.timer_manager.clone()
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
        if let Some(ci) = &self.ci_serial {
            ci.push_host(bytes);
        }
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::BUS_ERR;

    fn bus(prom: &[u8]) -> Ip30Bus {
        let heart = Arc::new(Heart::new(HeartConfig::octane([128 << 20, 0, 0, 0]), crate::ip30::heart::host_clock()).unwrap());
        let xbow = Arc::new(Xbow::new(XBOW_REVISION, 0x8100));
        xbow.connect_heart(heart.clone());
        let mut b = Ip30Bus::new(heart, xbow);
        b.attach_widget(WIDGET_BRIDGE, Arc::new(RawFlash::new(prom)), false);
        b
    }

    #[test]
    fn the_reset_vector_reads_the_prom() {
        let b = bus(&[0x0b, 0xf0, 0x00, 0xf0, 0, 0, 0, 0]);
        assert_eq!(b.read32(0x1fc0_0000).data, 0x0bf0_00f0);
        assert_eq!(b.read64(0x1fc0_0000).data, 0x0bf0_00f0_0000_0000);
        assert_eq!(b.read8(0x1fc0_0001).data, 0xf0);
        assert_eq!(b.read32(0x1fc0_0008).data, 0xffff_ffff, "erased past the image");
        assert_eq!(b.read32(0x1fdf_fffc).data, 0xffff_ffff, "the whole first select");
        assert_eq!(b.write32(0x1fc0_0000, 0), BUS_ERR);
        assert_eq!(b.read32(0x1f00_0000).status, BUS_ERR, "nothing else of the Bridge yet");
    }

    #[test]
    fn low_memory_is_its_own_16_kib() {
        let b = bus(&[]);
        assert_eq!(b.read64(0).data, 0);
        assert_eq!(b.write32(0x614, 0x1234_5678), BUS_OK);
        assert_eq!(b.read16(0x616).data, 0x5678);
        assert_eq!(b.read8(0x614).data, 0x12);
        assert_eq!(b.write64(0x2000, 0xa5a5_a5a5_5a5a_5a5a), BUS_OK);
        assert_eq!(b.write64(0x0, 0x0f0f_0f0f_f0f0_f0f0), BUS_OK);
        assert_eq!(b.read64(0x2000).data, 0xa5a5_a5a5_5a5a_5a5a, "no mirror");
        assert_eq!(b.write8(0x3fff, 7), BUS_OK);
        assert_eq!(b.read8(0x3fff).data, 7);
        assert_eq!(b.read8(0x4000).status, BUS_ERR);
        b.power_on();
        assert_eq!(b.read32(0x614).data, 0);
    }

    #[test]
    fn sdram_starts_past_the_guard() {
        let b = bus(&[]);
        // Bank 0, 128 MiB at the start of SDRAM space.
        assert_eq!(b.write32(PIU_BASE as u64 + 0x20, 0x8003_0000), BUS_OK);
        assert_eq!(b.read64(0x2000_0000).status, BUS_ERR);
        assert_eq!(b.read8(0x2000_3fff).status, BUS_ERR);
        assert_eq!(b.write64(0x2000_4000, 0x0102_0304_0506_0708), BUS_OK);
        assert_eq!(b.read32(0x2000_4004).data, 0x0506_0708);
        assert_eq!(b.read64(0x2800_0000).status, BUS_ERR, "past the bank");
    }

    #[test]
    fn widgets_route_by_number() {
        let b = bus(&[]);
        assert_eq!(b.read32(0x1000_0004).data, 0x4000_0001, "Crossbow");
        assert_eq!(b.read32(0x1800_0004).data, 0x4c00_106d, "HEART");
        assert_eq!(b.read64(0x1800_0000).data, 0x4c00_106d);
        assert_eq!(b.read32(0x1800_1004).status, BUS_ERR, "no route back to HEART");
        assert_eq!(b.read32(0x1b00_0004).status, BUS_ERR, "an empty graphics port");
        assert_eq!(b.read64(0x0ff1_0030).data, 0, "HEART's pending bits");
        assert_eq!(b.read32(0x0ff0_0020).data, 0);
    }
}
