//! The SGI O2 (IP32): its physical bus. RAM, the PROM and its flash, and the
//! decode that hands everything else to CRIME (`dev::crime`), GBE
//! (`dev::o2gfx`), VICE and MACE (`dev::mace`), which subdivide their own
//! space as HPC3 does on the Indy.
//!
//! The O2 builds its bus from the same parts as the Indy: a [`Physical`]
//! over CRIME's memory banks ([`O2Memory`] places them as the bank controls
//! say), with CRIME, GBE, VICE, MACE (and the PCI bridge's windows, which
//! MACE decodes itself) and the flash mapped where the hardware decodes them.
//! `ip32_machine` drives it.
//!
//! Everything was derived from observing the PROM's own behaviour, the O2
//! ASIC specifications and NetBSD's published register names.
//!
//! See `docs/o2-ip32.md`; the bring-up log is `clanker/devices/ip32-o2-bringup-log.md`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::physical::Physical;

use crate::dev::mace::Mace;
use crate::traits::{BusDevice, BusRead8, BusRead16, BusRead32, BusRead64, BUS_OK, BUS_ERR};

pub use crate::ip32_map::*;
use crate::dev::crime_mem::O2Memory;
// The O2's devices live one to a file under `crate::dev`; their names stay
// reachable here for the bus and the machine.
pub use crate::dev::crime::*;
pub use crate::dev::ds1687::*;
pub use crate::dev::mace_isa::*;
pub use crate::dev::mace_pci::*;
pub use crate::dev::mace_ps2::*;
pub use crate::dev::mace_sdma::*;
pub use crate::dev::mace_ust::*;
pub use crate::dev::mace_uart::*;
pub use crate::dev::ns16550::*;
pub use crate::dev::mace_id::*;

/// What is plugged into an O2 beyond its memory: the SCSI controllers (the
/// machine attaches their disks from `[scsi.N]`, see `O2Machine::new`), and
/// where the PROM's flash (its environment) is kept.
#[derive(Default)]
pub struct Ip32Options {
    /// The buses of the two on-board AIC-7880s (internal, external), with
    /// their targets attached. Without the controllers the PROM finds nothing
    /// to boot from.
    pub scsi: Option<[crate::scsi::ScsiBus; 2]>,
    /// A file holding the flash, so `setenv` survives from run to run.
    pub flash_file: Option<std::path::PathBuf>,
    /// A PS/2 keyboard on port 0, and a mouse on port 1 if `Some(true)`.
    pub keyboard: Option<bool>,
    /// The Ethernet address in the 1-Wire ID chip, if not the default.
    pub mac: Option<[u8; 6]>,
}

pub use crate::dev::mace::Stub;

/// The flash at [`PROM_BASE`]: the PROM and, behind it, the environment the
/// O2 keeps there instead of an NVRAM chip.
pub struct O2Flash {
    /// Writable, because an O2 keeps its environment variables in it -- there
    /// is no separate NVRAM chip -- and the installer saves settings there.
    /// The user's PROM file is never touched: this is a copy.
    bytes: Mutex<Vec<u8>>,
    /// Writes into the flash, so a command protocol would be visible rather
    /// than silently corrupting the image.
    writes: Mutex<Vec<(u32, u8)>>,
}

impl O2Flash {
    fn read(&self, addr: u64, n: usize) -> u64 {
        let p = self.bytes.lock().unwrap();
        let i = (addr as u32).wrapping_sub(PROM_BASE) as usize;
        if i + n > p.len() {
            return 0;
        }
        (0..n).fold(0u64, |v, k| (v << 8) | p[i + k] as u64)
    }

    fn write(&self, addr: u64, n: usize, val: u64) -> u32 {
        let a = addr as u32;
        let mut p = self.bytes.lock().unwrap();
        let i = a.wrapping_sub(PROM_BASE) as usize;
        if i + n > p.len() {
            return BUS_ERR;
        }
        {
            let mut log = self.writes.lock().unwrap();
            for k in 0..n {
                if log.len() < 512 {
                    log.push((a + k as u32, (val >> (8 * (n - 1 - k))) as u8));
                }
            }
        }
        for k in 0..n {
            p[i + k] = (val >> (8 * (n - 1 - k))) as u8;
        }
        BUS_OK
    }
}

impl BusDevice for O2Flash {
    fn read8(&self, a: u64) -> BusRead8 { BusRead8::ok(self.read(a, 1) as u8) }
    fn read16(&self, a: u64) -> BusRead16 { BusRead16::ok(self.read(a, 2) as u16) }
    fn read32(&self, a: u64) -> BusRead32 { BusRead32::ok(self.read(a, 4) as u32) }
    fn read64(&self, a: u64) -> BusRead64 { BusRead64::ok(self.read(a, 8)) }
    fn write8(&self, a: u64, v: u8) -> u32 { self.write(a, 1, v as u64) }
    fn write16(&self, a: u64, v: u16) -> u32 { self.write(a, 2, v as u64) }
    fn write32(&self, a: u64, v: u32) -> u32 { self.write(a, 4, v as u64) }
    fn write64(&self, a: u64, v: u64) -> u32 { self.write(a, 8, v) }
}

/// What answers where nothing on the O2's bus does: a read finds zero, a
/// write a bus error, and the first 64 distinct addresses are kept, as
/// `(addr, is_write, pc)`, for `o2 unmapped`. Once full, a miss costs one
/// relaxed load; a guest polling an absent device would otherwise lock and
/// scan the list on every poll. Not kept in a `lightning` build.
#[derive(Default)]
pub struct Misses {
    list: Mutex<Vec<(u32, bool, u32)>>,
    full: AtomicBool,
}

impl Misses {
    #[inline]
    fn note(&self, addr: u64, write: bool) {
        #[cfg(not(feature = "lightning"))]
        {
            if self.full.load(Ordering::Relaxed) {
                return;
            }
            let addr = addr as u32;
            let pc = crate::ip32_machine::LAST_PC.load(Ordering::Relaxed);
            let mut u = self.list.lock().unwrap();
            if !u.iter().any(|&(a, w, _)| a == addr && w == write) {
                u.push((addr, write, pc));
                if u.len() == 64 {
                    self.full.store(true, Ordering::Relaxed);
                }
            }
        }
        #[cfg(feature = "lightning")]
        let _ = (addr, write);
    }
}

impl BusDevice for Misses {
    fn read8(&self, a: u64) -> BusRead8 { self.note(a, false); BusRead8::ok(0) }
    fn read16(&self, a: u64) -> BusRead16 { self.note(a, false); BusRead16::ok(0) }
    fn read32(&self, a: u64) -> BusRead32 { self.note(a, false); BusRead32::ok(0) }
    fn read64(&self, a: u64) -> BusRead64 { self.note(a, false); BusRead64::ok(0) }
    fn write8(&self, a: u64, _: u8) -> u32 { self.note(a, true); BUS_ERR }
    fn write16(&self, a: u64, _: u16) -> u32 { self.note(a, true); BUS_ERR }
    fn write32(&self, a: u64, _: u32) -> u32 { self.note(a, true); BUS_ERR }
    fn write64(&self, a: u64, _: u64) -> u32 { self.note(a, true); BUS_ERR }
}

/// The O2's devices on its physical bus: RAM (CRIME's banks, placed by
/// [`O2Memory`]), the flash, and the chips that claim the rest, CRIME (with
/// its rendering engine), GBE, VICE and MACE, each of which decodes its own
/// space as HPC3 does on the Indigo2. MACE also takes the PCI bridge's
/// windows and decodes them itself.
pub struct Ip32Bus {
    /// The bus everything is mapped on: what the CPU and the DMA masters use.
    pub phys: Arc<Physical>,
    /// The runs of the PCI windows a BAR decodes whole, mapped over MACE
    /// (`MacePci::on_bar_runs`).
    pci_runs: Mutex<Vec<crate::pci::BarRun>>,
    pub(crate) ram: Arc<O2Memory>,
    pub(crate) flash: O2Flash,
    /// Where to keep the flash between runs, if anywhere. Never the image the
    /// caller supplied: settings written by a guest belong in the user's
    /// working directory, not in their copy of the PROM.
    flash_file: Option<std::path::PathBuf>,
    /// CRIME: its registers (interrupts, memory banks) and its rendering
    /// engine, which draws into RAM through its tile tables.
    pub crime: CrimeSpace,
    /// The display back end: scans the frame buffer out of RAM.
    pub gbe: crate::dev::o2gfx::Gbe,
    /// VICE, the video/image compression engine: a register file for now.
    /// IRIX probes it at boot (OpenBSD and NetBSD never look), and a bus
    /// error there panics the kernel.
    pub vice: Stub,
    /// MACE, the I/O ASIC, and every block behind it.
    pub mace: Mace,
    /// What answers where nothing does, and the addresses it was asked for.
    misses: Arc<Misses>,
    /// The SCSI controller, when one is attached, so the trace can report
    /// what the driver asked it for.
    pub scsi: Mutex<Vec<Arc<crate::dev::aic7880::Aic7880>>>,
}

impl Ip32Bus {
    /// `ram_bytes` of memory in the usual fitting for that size, or, for a
    /// size no O2 can have, one bank of it (power-of-two sizes only). `prom`
    /// must be the raw 512 KiB IP32 PROM image.
    pub fn new(ram_bytes: usize, prom: Vec<u8>) -> Arc<Self> {
        let mem = O2Memory::with_total((ram_bytes >> 20) as u32)
            .ok()
            .filter(|m| m.total() as usize == ram_bytes)
            .unwrap_or_else(|| O2Memory::single(ram_bytes));
        Self::with_options(mem, prom, Ip32Options::default())
    }

    /// A machine with `mem` fitted, configured by `opts`, its devices mapped
    /// on `mem`'s bus.
    pub fn with_options(mem: O2Memory, prom: Vec<u8>, opts: Ip32Options) -> Arc<Self> {
        let ram = Arc::new(mem);
        let phys = ram.phys().clone();
        let ust = Arc::new(MaceUst::new());
        let crime = Arc::new(Crime::new(ust.clone()));
        let mut bus = Self {
            phys: phys.clone(),
            pci_runs: Mutex::new(Vec::new()),
            ram: ram.clone(),
            flash: O2Flash { bytes: Mutex::new(prom), writes: Mutex::new(Vec::new()) },
            flash_file: opts.flash_file.clone(),
            crime: CrimeSpace::new(crime.clone(), ram.clone()),
            vice: Stub::new("vice"),
            gbe: {
                let ust = ust.clone();
                crate::dev::o2gfx::Gbe::new(Arc::new(move || ust.now()))
            },
            mace: Mace::new(ram.clone(), crime, ust),
            misses: Arc::new(Misses::default()),
            scsi: Mutex::new(Vec::new()),
        };
        if let Some(mac) = opts.mac {
            bus.mace.id = crate::dev::mace_id::MaceId::new(mac);
        }
        // The status bar's lights, from the register's heartbeat.
        bus.mace.mec.set_heartbeat(bus.mace.id.heartbeat.clone());
        if let Some(mouse) = opts.keyboard {
            // With a keyboard the PROM and IRIX take the graphics console;
            // without one, the serial console. The graphics hardware is
            // there either way.
            bus.mace.enable_ps2(mouse);
        }
        bus.gbe.connect_irq(bus.crime.regs.clone());
        if let Some([bus_a, bus_b]) = opts.scsi {
            // The controller fetches its work from host memory, which it
            // reaches through the PCI bridge it is attached to.
            let scsi = crate::dev::aic7880::Aic7880::new_at(0, bus_a);
            // The second controller. Real hardware has it whether or not
            // anything is plugged into it, and a kernel that expects two
            // waits on the one that is missing.
            let scsi_b = crate::dev::aic7880::Aic7880::new_at(1, bus_b);
            scsi.set_heartbeat(bus.mace.id.heartbeat.clone());
            scsi_b.set_heartbeat(bus.mace.id.heartbeat.clone());
            bus.mace.pci.attach(IP32_SCSI_SLOT_B, "aic7880-b", scsi_b.clone());
            bus.mace.pci.attach(IP32_SCSI_SLOT, "aic7880", scsi.clone());
            let mut list = bus.scsi.lock().unwrap();
            list.push(scsi);
            list.push(scsi_b);
        }
        // A saved flash carries the guest's environment -- boot paths, console
        // choice, everything the PROM's `setenv` writes -- from one run to
        // the next, which is what makes an install worth doing once.
        if let Some(path) = bus.flash_file.clone() {
            match std::fs::read(&path) {
                Ok(saved) if saved.len() == PROM_SIZE as usize => {
                    *bus.flash.bytes.lock().unwrap() = saved;
                    eprintln!("ip32: flash restored from {}", path.display());
                }
                Ok(_) => eprintln!("ip32: flash {} is the wrong size; ignoring", path.display()),
                Err(_) => {}
            }
        }
        let bus = Arc::new(bus);
        bus.map();
        // Window pages a PCI BAR decodes whole go straight to the function.
        let weak = Arc::downgrade(&bus);
        bus.mace.pci.on_bar_runs(Box::new(move |runs| {
            if let Some(b) = weak.upgrade() {
                b.set_pci_runs(runs);
            }
        }));
        bus
    }

    /// MACE says which runs of its PCI windows a BAR decodes whole: map
    /// them over MACE, giving the runs it named before back to it.
    fn set_pci_runs(&self, new: Vec<crate::pci::BarRun>) {
        let mut runs = self.pci_runs.lock().unwrap();
        for r in runs.iter() {
            // SAFETY: MACE is a field of this bus, alive as long as it is.
            unsafe { self.phys.map_borrowed(r.start, r.end, &self.mace, "mace") };
        }
        for r in &new {
            self.phys.map(r.start, r.end, r.dev.clone(), "pci bar");
        }
        *runs = new;
    }

    /// Put everything where CRIME and MACE decode it. The devices are fields
    /// of this bus, which is in its `Arc` and keeps them as long as the
    /// physical bus is used.
    fn map(&self) {
        use crate::dev::mace_pci::{PCI_HI_MEM_BASE, PCI_IO_BASE, PCI_IO_SIZE, PCI_MEM_BASE, PCI_MEM_SIZE};
        use crate::ppmem::devmap::Entry;
        let phys = &*self.phys;
        // Whatever nothing claims reads as zero and logs. That covers every
        // slot, RAM's too, so the memory is laid out again below.
        phys.set_default(self.misses.clone(), "nothing (o2 unmapped)");
        let set = |base: u32, size: u32, dev: Entry, name: &str| {
            // CRIME's registers and its rendering engine answer a whole
            // 64 KB and check their own range inside it.
            let end = (base as u64 + size as u64).next_multiple_of(0x1_0000);
            unsafe { phys.map_borrowed(base as u64, end, dev, name) };
        };
        set(CRIME_BASE, CRIME_SIZE, &self.crime, "crime");
        set(CRIME_RE_BASE, CRIME_RE_SIZE, &self.crime, "crime");
        set(GBE_BASE, GBE_SIZE, &self.gbe, "gbe");
        set(VICE_BASE, VICE_SIZE, &self.vice, "vice");
        set(MACE_BASE, MACE_SIZE, &self.mace, "mace");
        set(PCI_IO_BASE, PCI_IO_SIZE, &self.mace, "mace (pci i/o)");
        set(PCI_MEM_BASE, PCI_MEM_SIZE, &self.mace, "mace (pci memory)");
        // PCI memory's one-to-one window: 0x2_8000_0000 as 32 bits.
        unsafe { phys.map_borrowed(PCI_HI_MEM_BASE as u64, 1 << 32, &self.mace, "mace (pci memory)") };
        set(PROM_BASE, PROM_SIZE, &self.flash, "flash");
        self.ram.rebuild();
    }

    /// Accesses that hit nothing, the first 64 distinct ones.
    pub fn unmapped(&self) -> Vec<(u32, bool, u32)> {
        self.misses.list.lock().unwrap().clone()
    }

    /// Memory fitted, in bytes.
    pub fn ram_len(&self) -> u32 {
        self.ram.total()
    }

    /// GBE's picture out of RAM into `out` (R, G, B, A bytes), or None while
    /// the display is off. See [`crate::dev::o2gfx::Gbe::render`].
    pub fn render_screen(&self, out: &mut [u8]) -> Option<(u32, u32)> {
        self.gbe.render_from(&self.ram, out)
    }

    /// Where CRIME's output goes: the CPU's IP2, for a machine whose CPU
    /// runs on its own thread. Without it (the harness) nothing is pushed.
    pub fn connect_cpu_irq(&self, line: crate::irq::IrqLine) {
        self.crime.regs.connect_output(line);
    }

    /// What time and the outside world have done since the last call:
    /// frames from the network, console input, audio and serial DMA moving,
    /// and the beam's timing interrupts. As on the Indy, there is no
    /// refresh of interrupt lines: each device drives its own when its state
    /// changes, from the register access, the event or this tick that
    /// changed it. Returns whether CRIME is interrupting the CPU (IP2).
    pub fn service(&self) -> bool {
        self.mace.service();
        self.gbe.service();
        self.crime.regs.interrupting()
    }

    /// Write the flash out, if a file was named for it.
    pub fn save_flash(&self) {
        let Some(path) = &self.flash_file else { return };
        let p = self.flash.bytes.lock().unwrap();
        match std::fs::write(path, &p[..]) {
            Ok(()) => eprintln!("ip32: flash saved to {}", path.display()),
            Err(e) => eprintln!("ip32: flash {}: {e}", path.display()),
        }
    }

    /// The whole flash, PROM and environment, for a snapshot.
    pub fn flash_bytes(&self) -> Vec<u8> {
        self.flash.bytes.lock().unwrap().clone()
    }

    /// Put back a flash [`Self::flash_bytes`] took from a machine with the
    /// same PROM size.
    pub fn set_flash(&self, bytes: &[u8]) -> Result<(), String> {
        let mut p = self.flash.bytes.lock().unwrap();
        if bytes.len() != p.len() {
            return Err(format!("saved flash is {} bytes, this machine's {}", bytes.len(), p.len()));
        }
        p.copy_from_slice(bytes);
        Ok(())
    }

    /// How many bytes have been written into the flash so far.
    pub fn flash_writes(&self) -> usize {
        self.flash.writes.lock().unwrap().len()
    }

    /// Everything written into the flash, in order.
    pub fn prom_writes(&self) -> Vec<(u32, u8)> {
        self.flash.writes.lock().unwrap().clone()
    }
}

/// The O2's devices answer the monitor through the bus that holds them,
/// as the IOC and HPC3 pass their children's commands through on the
/// other machines.
impl Ip32Bus {
    fn devices(&self) -> Vec<&dyn crate::traits::Device> {
        let mut d: Vec<&dyn crate::traits::Device> = vec![&*self.crime.regs, &self.crime.re, &self.gbe];
        d.extend(self.mace.devices());
        d
    }
}

/// CRIME (which maps RAM again from its bank controls), GBE, VICE, MACE
/// and the SCSI controllers. RAM and the flash are too big for this and go
/// beside it as files ([`Ip32Bus::save_raw`]), as do the disks' overlays
/// ([`crate::dev::aic7880::Aic7880::export_overlays`]).
impl crate::traits::Saveable for Ip32Bus {
    fn save_state(&self) -> toml::Value {
        let mut t = toml::map::Map::new();
        t.insert("crime".into(), self.crime.save_state());
        t.insert("gbe".into(), self.gbe.save_state());
        t.insert("vice".into(), self.vice.save_state());
        t.insert("mace".into(), self.mace.save_state());
        let scsi = self.scsi.lock().unwrap().iter().map(|c| c.save_state()).collect();
        t.insert("scsi".into(), toml::Value::Array(scsi));
        if let Some(words) = self.ram.stack_page_words() {
            t.insert("stack_page".into(), toml::Value::Array(words.into_iter().map(crate::snapshot::hex_u64).collect()));
        }
        toml::Value::Table(t)
    }

    fn load_state(&self, v: &toml::Value) -> Result<(), String> {
        use crate::snapshot::get_field;
        if let Some(m) = get_field(v, "mace") {
            self.mace.load_state(m)?;
        }
        if let Some(c) = get_field(v, "crime") {
            self.crime.load_state(c)?;
        }
        if let Some(g) = get_field(v, "gbe") {
            self.gbe.load_state(g)?;
        }
        if let Some(x) = get_field(v, "vice") {
            self.vice.load_state(x)?;
        }
        if let Some(toml::Value::Array(a)) = get_field(v, "scsi") {
            let scsi = self.scsi.lock().unwrap();
            if a.len() != scsi.len() {
                return Err(format!("saved state has {} SCSI controllers, this machine {}", a.len(), scsi.len()));
            }
            for (c, s) in scsi.iter().zip(a) {
                c.load_state(s)?;
            }
        }
        if let Some(toml::Value::Array(a)) = get_field(v, "stack_page") {
            let words: Option<Vec<u64>> = a.iter().map(crate::snapshot::toml_u64).collect();
            self.ram.restore_stack_page(&words.ok_or("stack_page: not a list of dwords")?)?;
        }
        Ok(())
    }
}

impl Ip32Bus {
    /// A cold reset (CRIME's CONTROL register): the devices with a power-on
    /// state go back to it, and memory and the flash keep their contents, as
    /// on the hardware. The PROM sets up the rest again as it starts.
    pub fn power_on(&self) {
        use crate::traits::Resettable;
        self.ram.set_stack_page(false);
        self.crime.regs.power_on();
        let m = &self.mace;
        let devices: [&dyn Resettable; 8] =
            [&*m.ust, &m.rtc, &m.ps2, &m.id, &m.com0, &m.com1, &m.sdma, &*m.isa];
        for d in devices {
            d.power_on();
        }
    }

    /// RAM and the flash, as `ram.bin` and `flash.bin` in `dir`.
    pub fn save_raw(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.ram.save_raw(&dir.join("ram.bin"))?;
        std::fs::write(dir.join("flash.bin"), &*self.flash.bytes.lock().unwrap())
    }

    /// What [`Self::save_raw`] wrote, back.
    pub fn load_raw(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.ram.load_raw(&dir.join("ram.bin"))?;
        let flash = std::fs::read(dir.join("flash.bin"))?;
        let mut prom = self.flash.bytes.lock().unwrap();
        if flash.len() != prom.len() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "saved flash is the wrong size"));
        }
        *prom = flash;
        Ok(())
    }
}

impl crate::traits::Device for Ip32Bus {
    fn step(&self, _cycles: u64) {}
    fn stop(&self) {}
    fn start(&self) {}
    fn is_running(&self) -> bool { true }
    fn get_clock(&self) -> u64 { 0 }

    fn register_commands(&self) -> Vec<(String, String)> {
        self.devices().iter().flat_map(|d| d.register_commands()).collect()
    }

    fn execute_command(&self, cmd: &str, args: &[&str], writer: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        for d in self.devices() {
            if d.register_commands().iter().any(|(c, _)| c == cmd) {
                return d.execute_command(cmd, args, writer);
            }
        }
        Err("Command not found".to_string())
    }
}

/// The bus, for code that holds the machine's bus structure: every access
/// goes to [`Ip32Bus::phys`].
impl BusDevice for Ip32Bus {
    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, addr: u64) -> *const std::sync::atomic::AtomicU64 { self.phys.gen_ptr(addr) }
    fn read8(&self, addr: u64) -> BusRead8 { self.phys.read8(addr) }
    fn read16(&self, addr: u64) -> BusRead16 { self.phys.read16(addr) }
    fn read32(&self, addr: u64) -> BusRead32 { self.phys.read32(addr) }
    fn read64(&self, addr: u64) -> BusRead64 { self.phys.read64(addr) }
    fn write8(&self, addr: u64, val: u8) -> u32 { self.phys.write8(addr, val) }
    fn write16(&self, addr: u64, val: u16) -> u32 { self.phys.write16(addr, val) }
    fn write32(&self, addr: u64, val: u32) -> u32 { self.phys.write32(addr, val) }
    fn write64(&self, addr: u64, val: u64) -> u32 { self.phys.write64(addr, val) }
    fn describe_fault(&self, addr: u64, size: usize) -> Option<String> { self.phys.describe_fault(addr, size) }
    fn mem_ptr(&self, addr: u64) -> Option<*const u64> { self.phys.mem_ptr(addr) }
    fn read_block(&self, addr: u64, buf: &mut [u64]) -> u32 { self.phys.read_block(addr, buf) }
    fn write_block(&self, addr: u64, buf: &[u64]) -> u32 { self.phys.write_block(addr, buf) }
}

#[cfg(test)]
mod tests;

