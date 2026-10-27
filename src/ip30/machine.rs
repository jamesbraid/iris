//! The Octane (IP30) as a machine: an R10000 on HEART, the Crossbow, and
//! the widgets behind it, with iris's CPU thread, monitor and front ends.
//!
//! Ported from hw/mips/sgi/octane.c in
//! https://github.com/jamesbraid/qemu-sgi-origami (sgi-origami branch),
//! BSD-3-Clause.
//!
//! Selected with `[machine] profile = "octane_ip30"`; the shared keys
//! configure it (`prom`, `banks`, `[scsi.N]`, `[network]`, the serial
//! ports) and `[octane]` holds what only an Octane has. See
//! `docs/octane-ip30.md`.
//!
//! The processor's physical map, from OpenBSD's `ip30.h` and Linux's
//! `ip30-xtalk.c`:
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
//! graphics ports. On the Bridge's PCI bus, as on the board: the two
//! ISP1040 SCSI controllers in slots 0 and 1, IOC3 (serial, keyboard and
//! mouse, the clock, Ethernet) in slot 2, RAD1 audio in slot 3.
//!
//! Low memory is an approximation: 16 KiB of distinct RAM at physical 0,
//! zero at reset, and the first 16 KiB above 0x2000_0000 a bus error, with
//! SDRAM starting at 0x2000_4000. Public software uses physical 0x400 (the
//! GDA) and 0x600 (MPCONF) but no published source gives the backing, and
//! the stock diagnostics expect the upper 8 KiB to be distinct from the
//! lower. The fault is enforced on the
//! bus path; a cached access through the CPU's direct RAM window does not
//! see it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::{CpuModel, MachineConfig};
use crate::cpu::mips_cache_shadow::R10000ShadowCache;
use crate::cpu::mips_exec::{CpuDevice, MipsCpu, MipsCpuConfig, MipsExecutor};
use crate::cpu::mips_tlb::MipsTlb;
use crate::hptimer::TimerManager;
use crate::dev::ioc3::{board_record_part, onewire, BoardRecord, Ioc3};
use crate::dev::ioc3_eth::Ioc3Eth;
use crate::dev::isp1040::Isp1040;
use crate::ip30::bridge::{Bridge, RawFlash};
use crate::ip30::heart::{Heart, HeartConfig, OneWireResponder, PIU_BASE, PIU_SIZE, RAM_BASE};
use crate::ip30::impact::ImpactXio;
use crate::ip30::xio::{XioInterruptSink, XioMemory};
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
/// The DS1685 NVRAM file when `nvram` is left at the shared default: the
/// Octane's image is a different size and layout from the other machines'.
pub const OCTANE_NVRAM_FILE: &str = "nvram-ip30.bin";
/// The boot flash: one 1 MiB part on the Bridge's first chip select.
pub const FLASH_SIZE: u64 = 1 << 20;
/// The Ethernet address when `[network] mac` does not set one, a
/// placeholder in SGI's range.
pub const DEFAULT_MAC: [u8; 6] = [0x08, 0x00, 0x69, 0x12, 0x34, 0x56];

/// The 195 MHz R10000 of SGI's standard 128 MB Octane configurations (the
/// OCTANE Technical Report): Count at half that.
pub const OCTANE_COUNT_HZ: u64 = 97_500_000;
/// R10000 revision 2.7, as an Octane's hinv reports it; the raw PRId is
/// inferred from the R10000 manual's encoding.
pub const OCTANE_PRID: u32 = 0x0927;
/// The Crossbow on the Octane reports revision 4.
const XBOW_REVISION: u32 = 4;
/// The graphics port the one IMPACT board sits in. A choice: it is where a
/// measured Octane2 has its graphics board, not a measured SI placement.
pub const WIDGET_GFX: u8 = 11;

/// Where the CPU was at the end of its last batch, for the unmapped log.
static LAST_PC: AtomicU64 = AtomicU64::new(0);

/// The boot flash window, both chip selects, as the processor addresses it.
const BOOT_FLASH: std::ops::Range<u64> = 0x1fc0_0000..0x2000_0000;
/// An R10000 secondary cache line.
const CACHE_LINE: u64 = 128;

/// The CPU's cached view of the bus: what the cache model reaches, while
/// uncached accesses go to the bus directly.
///
/// The PROM runs cached from the boot flash and keeps its initialised data
/// there, storing to its own variables at their flash addresses. On an
/// R10000 a cached store allocates the line in the write-back cache, dirty
/// exclusive, and later cached loads hit it; the PROM never has the line
/// written back, the same pattern as firmware that keeps its working data
/// in the cache before memory exists. Nothing reaches the Bridge, whose
/// flash port refuses anything but a 16-bit write.
///
/// Approximation: iris's R10000 cache model passes every cached access to
/// memory, so the dirty lines live here instead. The first cached store to a
/// flash line copies the line in; from then on cached loads and stores of it
/// use the copy, which is never written back or evicted. Uncached accesses
/// always reach the flash. Reset empties it. What the hardware does if such
/// a line is ever written back is not modelled.
pub struct CachedFlashLines {
    bus: Arc<dyn BusDevice>,
    lines: Mutex<std::collections::HashMap<u64, [u8; CACHE_LINE as usize]>>,
}

impl CachedFlashLines {
    pub fn new(bus: Arc<dyn BusDevice>) -> Self {
        Self { bus, lines: Mutex::new(std::collections::HashMap::new()) }
    }

    /// Forget every line, as a reset loses the cache's contents.
    pub fn clear(&self) {
        self.lines.lock().unwrap().clear();
    }

    /// Bytes `addr..addr + n` of a held line, big-endian, or `None` if the
    /// line is not held.
    fn load(&self, addr: u64, n: usize) -> Option<u64> {
        if !BOOT_FLASH.contains(&addr) {
            return None;
        }
        let lines = self.lines.lock().unwrap();
        let line = lines.get(&(addr & !(CACHE_LINE - 1)))?;
        let at = (addr & (CACHE_LINE - 1)) as usize;
        Some(line[at..at + n].iter().fold(0, |v, &b| v << 8 | b as u64))
    }

    /// Store into the line, filling it from the flash first. `None`: not a
    /// flash address.
    fn store(&self, addr: u64, n: usize, val: u64) -> Option<u32> {
        if !BOOT_FLASH.contains(&addr) {
            return None;
        }
        let base = addr & !(CACHE_LINE - 1);
        let mut lines = self.lines.lock().unwrap();
        let line = match lines.entry(base) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut words = [0u64; (CACHE_LINE / 8) as usize];
                let r = self.bus.read_block(base, &mut words);
                if r != BUS_OK {
                    return Some(r);
                }
                let mut line = [0u8; CACHE_LINE as usize];
                for (chunk, w) in line.chunks_mut(8).zip(words) {
                    chunk.copy_from_slice(&w.to_be_bytes());
                }
                e.insert(line)
            }
        };
        let at = (addr & (CACHE_LINE - 1)) as usize;
        line[at..at + n].copy_from_slice(&val.to_be_bytes()[8 - n..]);
        Some(BUS_OK)
    }
}

macro_rules! cached_access {
    ($read:ident, $write:ident, $r:ident, $t:ty, $n:expr) => {
        fn $read(&self, addr: u64) -> $r {
            match self.load(addr, $n) {
                Some(v) => $r::ok(v as $t),
                None => self.bus.$read(addr),
            }
        }
        fn $write(&self, addr: u64, val: $t) -> u32 {
            self.store(addr, $n, val as u64).unwrap_or_else(|| self.bus.$write(addr, val))
        }
    };
}

impl BusDevice for CachedFlashLines {
    cached_access!(read8, write8, BusRead8, u8, 1);
    cached_access!(read16, write16, BusRead16, u16, 2);
    cached_access!(read32, write32, BusRead32, u32, 4);
    cached_access!(read64, write64, BusRead64, u64, 8);

    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, addr: u64) -> *const AtomicU64 {
        self.bus.gen_ptr(addr)
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
    /// Accesses that failed, first 64 distinct: `(addr, write, pc)`.
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
                let r = d.$read(addr);
                if r.status == BUS_ERR {
                    self.miss(addr, false);
                }
                return r;
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
                let r = d.$write(addr, val);
                if r == BUS_ERR {
                    self.miss(addr, true);
                }
                return r;
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

/// A one-wire bus of board records, as HEART's master sees it.
struct RecordBus(onewire::Bus);

impl OneWireResponder for RecordBus {
    fn reset(&mut self) -> bool {
        self.0.reset()
    }
    fn touch_bit(&mut self, bit: bool) -> bool {
        self.0.slot(bit)
    }
}

/// One board-record part on a bus of its own. The barcodes (EMU...) and ROM
/// serials are made up and marked so; the part numbers and names are an
/// Octane's.
fn record_bus(barcode: &str, part: &str, revision: &str, name: &str, serial: u64) -> onewire::Bus {
    let mut bus = onewire::Bus::new();
    bus.add(board_record_part(&BoardRecord { barcode, part, revision, name }, serial));
    bus
}

/// A line into one of the Bridge's directly wired interrupt inputs.
struct BridgeWire(std::sync::Weak<Bridge>);

impl crate::irq::InterruptController for BridgeWire {
    fn set_line(&self, line: u32, level: bool) {
        if let Some(b) = self.0.upgrade() {
            b.set_wire_irq(line as u8, level);
        }
    }
}

pub struct OctaneMachine {
    pub bus: Arc<Ip30Bus>,
    cpu: Arc<dyn CpuDevice>,
    flash_lines: Arc<CachedFlashLines>,
    timer_manager: Arc<TimerManager>,
    running: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    monitor: Arc<crate::monitor::Monitor>,
    monitor_port: u16,
    ci_serial: Option<Arc<crate::dev::z85c30::CiSerialBackend>>,
    pub bridge: Arc<Bridge>,
    pub ioc3: Arc<Ioc3>,
    pub scsi: [Arc<Isp1040>; 2],
    eth: Arc<Ioc3Eth>,
    rad: Arc<crate::dev::rad1::Rad1>,
    flash: Arc<crate::ip30::flash::CfiFlash>,
    /// The IMPACT board, unless `[octane] graphics = "none"`.
    pub gfx: Option<Arc<ImpactXio>>,
    nat: Option<(Arc<crate::net::NatControl>, (std::net::Ipv4Addr, std::net::Ipv4Addr, std::net::Ipv4Addr))>,
    scratch_path: Option<std::path::PathBuf>,
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
        let board = cfg.octane.graphics_board()?;
        let heart = Arc::new(Heart::new(HeartConfig::octane(banks), crate::ip30::heart::host_clock())?);
        let gfx_link = if board.is_some() { 1 << WIDGET_GFX } else { 0 };
        let xbow = Arc::new(Xbow::new(XBOW_REVISION, (1 << WIDGET_HEART) | (1 << WIDGET_BRIDGE) | gfx_link));
        xbow.connect_heart(heart.clone());
        let mut bus = Ip30Bus::new(heart.clone(), xbow.clone());

        // HEART's one-wire bus holds the processor module's record.
        heart.attach_onewire(Box::new(RecordBus(record_bus("EMUU00", "030-0888-004", "B", "PM10", 0x5145_4d55_5500))));

        // The onboard Bridge: memory and interrupts through HEART, the boot
        // flash on its first chip select, the system board's record on its
        // one-wire register.
        let bridge = Bridge::new(heart.clone() as Arc<dyn XioMemory>, heart.clone() as Arc<dyn XioInterruptSink>);
        // The flash holds the PROM's own records as well as its code; with
        // `[octane] flash` they last from run to run.
        let flash = match &cfg.octane.flash {
            Some(path) => crate::ip30::flash::CfiFlash::with_file(&prom, FLASH_SIZE as usize, path)
                .map_err(|e| format!("octane.flash {path}: {e}"))?,
            None => crate::ip30::flash::CfiFlash::new(&prom, FLASH_SIZE as usize),
        };
        let flash = Arc::new(flash);
        bridge.attach_flash(flash.clone(), FLASH_SIZE as u32);
        // The second chip select: a blank 2 MiB part that reads all ones and
        // keeps no writes, an approximation from one word read on a real
        // Octane2. The PROM looks for a part there too.
        bridge.attach_flash_select1(Arc::new(RawFlash::new(&[], 2 << 20)), 2 << 20);
        let system_record = Mutex::new(record_bus("EMUN00", "030-0887-003", "F", "IP30", 0x5145_4d55_4e00));
        bridge.set_onewire(Box::new(move |pulse, _sample| {
            // The same slot decode as HEART's: a long low drive is a reset, a
            // short one a write-one or read (chosen thresholds, not measured).
            let mut b = system_record.lock().unwrap();
            if pulse >= 400 { !b.reset() } else { b.slot(pulse < 40) }
        }));

        let mac = match cfg.network.mac.as_deref() {
            Some(m) => crate::config::parse_mac(m).map_err(|e| format!("network.mac: {e}"))?,
            None => DEFAULT_MAC,
        };
        let scsi = [Arc::new(Isp1040::new_at(0)), Arc::new(Isp1040::new_at(1))];
        bridge.attach(0, scsi[0].clone());
        bridge.attach(1, scsi[1].clone());
        let ioc3 = Arc::new(Ioc3::octane(mac, false));
        let eth = Ioc3Eth::new(mac);
        ioc3.attach_eth(eth.clone());
        bridge.attach(2, ioc3.clone());
        let rad = Arc::new(crate::dev::rad1::Rad1::new());
        bridge.attach(3, rad.clone());
        // SuperIO (serial, keyboard, clock) is Bridge input 4; the Ethernet
        // uses IOC3's PCI pin.
        ioc3.connect_superio_interrupt(crate::irq::IrqLine::new(Arc::new(BridgeWire(Arc::downgrade(&bridge))), 4));
        let nvram = if cfg.nvram == crate::config::DEFAULT_NVRAM { OCTANE_NVRAM_FILE } else { cfg.nvram.as_str() };
        if let Err(e) = ioc3.set_nvram_file(nvram) {
            eprintln!("iris: Octane NVRAM {nvram}: {e}");
        }
        bus.attach_widget(WIDGET_BRIDGE, bridge.clone(), false);

        // The status bar's activity bits and the CPU's Compare matches,
        // shared with the graphics board before the CPU exists.
        let heartbeat = Arc::new(AtomicU64::new(0));
        let fasttick = Arc::new(AtomicU64::new(0));
        let gfx = board.map(|_| {
            let g = ImpactXio::new(
                WIDGET_GFX,
                heart.clone() as Arc<dyn XioInterruptSink>,
                heart.clone() as Arc<dyn XioMemory>,
                heartbeat.clone(),
                fasttick.clone(),
            );
            bus.attach_widget(WIDGET_GFX, g.clone(), false);
            g
        });
        let bus = Arc::new(bus);
        // DMA reaches only widgets the Crossbow has a live link to.
        let links = xbow.clone();
        bridge.set_link_alive(Box::new(move |w| links.carries(w)));
        // A Crossbow link reset of widget 15 resets the Bridge at its far end.
        let weak = Arc::downgrade(&bridge);
        xbow.on_link_reset(Box::new(move |w| {
            if w == WIDGET_BRIDGE {
                if let Some(b) = weak.upgrade() {
                    b.link_reset();
                }
            }
        }));

        let scratch_path = Self::attach_scsi(cfg, &scsi)?;
        eprintln!(
            "iris: Octane with {} MB ({}), {}",
            banks.iter().sum::<u64>() >> 20,
            banks.iter().filter(|&&b| b != 0).map(|b| format!("{}", b >> 20)).collect::<Vec<_>>().join("+"),
            board.map_or("no graphics", |b| b.label())
        );

        let timer_manager = Arc::new(TimerManager::new());
        let count_hz = cfg.clock.fixed_mhz.map_or(OCTANE_COUNT_HZ, |mhz| (mhz * 1_000_000.0) as u64);
        let flash_lines = Arc::new(CachedFlashLines::new(bus.clone()));
        let cpu = Self::build_cpu(bus.clone(), flash_lines.clone(), &timer_manager, count_hz, fasttick);
        if let Some(g) = &gfx {
            g.mgras().set_cpu_cycles(cpu.cycles_ptr());
            g.mgras().register_locks();
            // Host GL frames composite straight into the IMPACT framebuffer.
            #[cfg(feature = "hostgl")]
            iris_hostcall::set_display(Arc::new(crate::dev::mgras::ImpactScreen(g.mgras().clone())));
        }

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
        for c in &scsi {
            monitor.register_device(c.clone());
            c.register_locks();
        }
        // `ps2 type` and friends: the keyboard and mouse from the monitor.
        monitor.register_device(ioc3.ps2());
        ioc3.register_locks();
        eth.register_locks();
        rad.register_locks();
        if let Some(g) = &gfx {
            monitor.register_device(g.mgras().clone());
        }
        let monitor_port = cfg.monitor_port.unwrap_or(crate::monitor::DEFAULT_PORT);
        let ci_serial = cfg.ci.then(|| Arc::new(crate::dev::z85c30::CiSerialBackend::new()));
        Self::connect_serial(cfg, &ioc3, ci_serial.as_ref());
        ioc3.spawn_service();

        // The Ethernet on the NAT gateway `[network]` describes.
        let mut nat = None;
        if cfg.network.mode != crate::config::NetMode::Off {
            let net = crate::net::GatewayConfig::from_network(&cfg.network());
            eprintln!("iris: Octane Ethernet on NAT: gateway {}, guest {} (DHCP)", net.gateway_ip, net.client_ip);
            let addrs = (net.client_ip, net.gateway_ip, net.netmask);
            nat = Some((eth.start_nat(net), addrs));
        }
        let (events, event_rx) = std::sync::mpsc::sync_channel(4);
        Ok(Self {
            bus,
            cpu,
            flash_lines,
            timer_manager,
            running: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            monitor: Arc::new(monitor),
            monitor_port,
            ci_serial,
            bridge,
            ioc3,
            scsi,
            eth: eth.clone(),
            rad,
            flash,
            gfx,
            nat,
            scratch_path,
            events,
            event_rx: Some(event_rx),
            controller_up: Arc::new(AtomicBool::new(false)),
            controller_slot: None,
            lifecycle: crate::machine_state::GuestState::Idle,
        })
    }

    /// The `[scsi.N]` devices: `controller` 0 is the internal bus, 1 the
    /// external one. Returns the `scratch = true` volume, if any.
    fn attach_scsi(cfg: &MachineConfig, ctls: &[Arc<Isp1040>; 2]) -> Result<Option<std::path::PathBuf>, String> {
        let mut scratch_path = None;
        let mut ids: Vec<u8> = cfg.scsi.keys().copied().collect();
        ids.sort();
        for key in ids {
            let dev = &cfg.scsi[&key];
            let (controller, id) = dev.placement(key);
            let at = crate::config::scsi_label(controller, id);
            if dev.is_daynaport() {
                return Err(format!("{at}: kind = \"daynaport\" is not available on the Octane"));
            }
            if let Some(path) = dev.prepare_scratch(key) {
                scratch_path.get_or_insert(path);
            }
            let ctl = ctls
                .get(controller as usize)
                .ok_or_else(|| format!("{at}: controller {controller} (an Octane has controllers 0 and 1)"))?;
            let (path, discs) = dev.media();
            let cdrom = dev.is_cdrom();
            let ci_overlay = (cfg.ci && dev.overlay && !cdrom).then(|| crate::config::ScsiDeviceConfig::ci_overlay_path(key));
            match ctl.attach_device(id, &path, cdrom, discs, dev.overlay, ci_overlay.as_deref()) {
                Ok(_) if path.is_empty() => eprintln!("ip30: {at}: CD-ROM, tray empty"),
                Ok(blocks) => eprintln!("ip30: {at}: {path} ({blocks} blocks)"),
                Err(e) => {
                    let msg = format!("could not attach {path} to {at}: {e}");
                    if std::env::var_os("IRIS_NO_EXIT_ON_POWEROFF").is_some() {
                        eprintln!("iris: warning: {msg}; continuing without {at}");
                    } else {
                        return Err(msg);
                    }
                }
            }
        }
        Ok(scratch_path)
    }

    /// IOC3's first port is the console: the CI socket's backend in CI
    /// mode, otherwise TCP on `serial_port_a`, either teed to `serial_log`.
    /// The second port serves TCP on `serial_port_b`.
    fn connect_serial(cfg: &MachineConfig, ioc3: &Ioc3, ci: Option<&Arc<crate::dev::z85c30::CiSerialBackend>>) {
        use crate::dev::z85c30::{null_serial_backend, tcp_serial_backend, TeeBackend};
        let [port_a, port_b] = cfg.serial_ports();
        let log = cfg.serial_log.as_deref().and_then(|path| {
            match crate::dev::serial_log::SerialLog::create(std::path::Path::new(path), cfg.serial_log_timestamps) {
                Ok(log) => Some(log),
                Err(e) => {
                    eprintln!("iris: Octane serial log {path}: {e}");
                    None
                }
            }
        });
        if let Some(ci) = ci {
            if let Some(log) = log {
                ci.set_log(log);
            }
            ioc3.set_serial_backend(0, ci.clone());
        } else {
            let mut backend = null_serial_backend();
            match tcp_serial_backend(port_a) {
                Some((b, p)) => {
                    eprintln!("iris: Octane serial port 1 on 127.0.0.1:{p}");
                    backend = b;
                }
                None => eprintln!("iris: Octane serial port 1: port {port_a} unavailable (serial_port_a)"),
            }
            if let Some(log) = log {
                backend = Arc::new(TeeBackend::new(backend, log));
            }
            ioc3.set_serial_backend(0, backend);
        }
        if let Some((b, p)) = tcp_serial_backend(port_b) {
            eprintln!("iris: Octane serial port 2 on 127.0.0.1:{p}");
            ioc3.set_serial_backend(1, b);
        }
    }

    fn build_cpu(
        sysad: Arc<dyn BusDevice>,
        cached: Arc<CachedFlashLines>,
        tm: &Arc<TimerManager>,
        count_hz: u64,
        fasttick: Arc<AtomicU64>,
    ) -> Arc<dyn CpuDevice> {
        let cfg = MipsCpuConfig::for_model::<R10000ShadowCache>();
        let tlb = MipsTlb::new(cfg.tlb_entries);
        let mut exec: MipsExecutor<MipsTlb, R10000ShadowCache> = MipsExecutor::new(sysad, tlb, &cfg);
        // Cached accesses see the PROM's dirty flash lines; see `CachedFlashLines`.
        exec.cache = R10000ShadowCache::from(cached as Arc<dyn BusDevice>);
        exec.core.set_timer_manager(tm.clone());
        exec.core.set_count_hz(count_hz);
        exec.core.fasttick_count = fasttick;
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
        // The guest switching the machine off through the clock.
        let (ioc3, cpu, running) = (self.ioc3.clone(), self.cpu.clone(), self.running.clone());
        let (events, controller_up) = (self.events.clone(), self.controller_up.clone());
        self.threads.push(
            std::thread::Builder::new()
                .name("Octane-Power".into())
                .spawn(move || {
                    while running.load(Ordering::Relaxed) {
                        if ioc3.powered_off() {
                            eprintln!("iris: the Octane switched itself off");
                            cpu.clone().as_device().stop();
                            if controller_up.load(Ordering::SeqCst) && events.try_send(MachineEvent::PowerOff).is_ok() {
                                return;
                            }
                            running.store(false, Ordering::SeqCst);
                            std::process::exit(0);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                })
                .unwrap(),
        );
        self.restart_peripherals();
        if self.ci_serial.is_none() {
            self.cpu.clone().as_device().start();
        }
    }

    /// Stop the CPU, then hold every device thread off guest memory and
    /// the interrupt lines (the SCSI ring threads let a command in flight
    /// finish first), as the O2 holds its service thread, and save the
    /// NVRAM. [`Self::restart_peripherals`] lets them go again.
    pub fn stop(&mut self) {
        self.cpu.clone().as_device().stop();
        if let Some(g) = &self.gfx {
            g.stop_display();
        }
        for c in &self.scsi {
            c.pause();
        }
        self.ioc3.pause();
        self.eth.pause();
        self.rad.pause();
        self.ioc3.flush_nvram();
        if self.lifecycle != crate::machine_state::GuestState::Idle {
            self.lifecycle = crate::machine_state::GuestState::Halted;
        }
    }

    pub fn reset(&mut self) {
        self.stop();
        crate::traits::Resettable::power_on(&*self.cpu);
        self.bus.power_on();
        self.flash_lines.clear();
        self.bridge.reset();
        self.bridge.reset_pci();
        self.ioc3.reset();
        self.eth.reset();
        for c in &self.scsi {
            c.reset();
        }
        self.rad.reset();
        self.flash.reset();
        if let Some(g) = &self.gfx {
            g.reset();
        }
        self.restart_peripherals();
    }

    /// Start what `stop` stopped, short of the CPU: the device threads, and
    /// the graphics engines and display. Without the latter the PROM's
    /// first graphics readback after a reset waits forever on a pipeline
    /// nothing drains.
    pub fn restart_peripherals(&mut self) {
        for c in &self.scsi {
            c.resume();
        }
        self.ioc3.resume();
        self.eth.resume();
        self.rad.resume();
        if let Some(g) = &self.gfx {
            g.start_display();
        }
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
    fn restart_peripherals(&mut self) {
        OctaneMachine::restart_peripherals(self)
    }
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
        self.gfx.as_ref().map(|g| g.display())
    }
    fn get_ps2(&self) -> Arc<crate::dev::ps2::Ps2Controller> {
        self.ioc3.ps2()
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
    fn scratch_path(&self) -> Option<&std::path::Path> {
        self.scratch_path.as_deref()
    }
    /// Disc changes on the internal bus.
    fn disc_changer(&self) -> Option<Arc<dyn crate::workstation::DiscChanger>> {
        Some(self.scsi[0].clone() as Arc<dyn crate::workstation::DiscChanger>)
    }
    fn nat_control(&self) -> Option<Arc<crate::net::NatControl>> {
        self.nat.as_ref().map(|(n, _)| n.clone())
    }
    fn nat_expected(&self) -> Option<(std::net::Ipv4Addr, std::net::Ipv4Addr, std::net::Ipv4Addr)> {
        self.nat.as_ref().map(|(_, a)| *a)
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
        let bridge = Bridge::new(b.heart.clone() as Arc<dyn XioMemory>, b.heart.clone() as Arc<dyn XioInterruptSink>);
        bridge.attach_flash(Arc::new(RawFlash::new(prom, FLASH_SIZE as usize)), FLASH_SIZE as u32);
        b.attach_widget(WIDGET_BRIDGE, bridge, false);
        b
    }

    /// The PROM stores to its own variables at their cached flash addresses.
    /// Through the CPU's cache the store completes and a later cached load
    /// sees it; the flash, read uncached, still holds the image, and a reset
    /// forgets the line.
    #[test]
    fn cached_stores_to_the_flash_stay_in_the_cache() {
        use crate::cpu::mips_cache_v2::MipsCache;
        let mut prom = vec![0u8; 0x2000];
        prom[0x1140..0x1150].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let flash: Arc<dyn BusDevice> = Arc::new(bus(&prom));
        let lines = Arc::new(CachedFlashLines::new(flash.clone()));
        let cache = R10000ShadowCache::from(lines.clone() as Arc<dyn BusDevice>);
        let (va, pa) = (0xffff_ffff_9fc0_1148, 0x1fc0_1148);

        assert_eq!(flash.write32(pa, 1), BUS_ERR, "the Bridge's flash port refuses a 4-byte write");
        assert_eq!(cache.write::<4>(va, pa, 1), BUS_OK, "a cached store completes");
        assert_eq!(cache.read::<4>(va, pa).data, 1, "a later cached load sees it");
        assert_eq!(cache.read::<4>(va - 8, pa - 8).data, 0xffff_ffff, "the rest of the line is the flash's");
        assert_eq!(cache.read::<8>(va, pa).data, 0x0000_0001_0000_0001, "wider loads see the line too");
        assert_eq!(flash.read32(pa).data, 0, "the flash, read uncached, is untouched");
        assert_eq!(cache.read::<4>(va + 0x80, pa + 0x80).data, 0, "the next line is not held");

        lines.clear();
        assert_eq!(cache.read::<4>(va, pa).data, 0, "reset forgets the line");
    }

    #[test]
    fn the_reset_vector_reads_the_prom() {
        let b = bus(&[0x0b, 0xf0, 0x00, 0xf0, 0, 0, 0, 0]);
        assert_eq!(b.read32(0x1fc0_0000).data, 0x0bf0_00f0);
        assert_eq!(b.read64(0x1fc0_0000).data, 0x0bf0_00f0_0000_0000);
        assert_eq!(b.read8(0x1fc0_0001).data, 0xf0);
        assert_eq!(b.read32(0x1fc0_0008).data, 0xffff_ffff, "erased past the image");
        assert_eq!(b.read32(0x1f00_0004).data >> 12 & 0xffff, 0xc002, "the Bridge's widget identity");
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

    #[test]
    fn the_graphics_port_reaches_impact_with_its_link_up() {
        use crate::ip30::xio::widget_small_base;
        let heart = Arc::new(Heart::new(HeartConfig::octane([128 << 20, 0, 0, 0]), crate::ip30::heart::host_clock()).unwrap());
        let gfx = ImpactXio::new(
            WIDGET_GFX,
            heart.clone() as Arc<dyn XioInterruptSink>,
            heart.clone() as Arc<dyn XioMemory>,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );
        let id = widget_small_base(WIDGET_GFX) as u64 + 4;
        for (links, alive) in [(0x8100, false), (0x8100 | 1 << WIDGET_GFX, true)] {
            let xbow = Arc::new(Xbow::new(XBOW_REVISION, links));
            xbow.connect_heart(heart.clone());
            let mut b = Ip30Bus::new(heart.clone(), xbow);
            b.attach_widget(WIDGET_GFX, gfx.clone(), false);
            let r = b.read32(id);
            if alive {
                assert_eq!(r.status, BUS_OK);
                assert_eq!(r.data >> 12 & 0xffff, 0xc003, "HQ4's widget part number");
            } else {
                assert_eq!(r.status, BUS_ERR, "no link, no route");
            }
        }
    }
}
