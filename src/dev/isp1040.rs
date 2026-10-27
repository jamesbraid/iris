//! QLogic ISP1040 SCSI host adapter, emulated above its RISC.
//!
//! The driver hands whole SCSI commands to a request ring in host memory and
//! collects completions from a response ring. Everything else is a mailbox
//! protocol of sixteen-bit registers. The microcode a driver downloads is
//! retained (so a checksum can be verified and read back) and never run: the
//! SCSI work is done here, against the shared target model, and the
//! completion is posted the way the firmware would.
//!
//! The register and ring contract follow the part's datasheet and the public
//! Linux and OpenBSD drivers. Where the model approximates, it says so:
//!
//! * Commands run one at a time, in ring order, on the adapter's own thread:
//!   the doorbell write returns at once and the completion and its
//!   interrupt follow when the target has answered, as the part's would.
//!   No abort/recovery mailbox commands exist (the ISP1040 does not carry
//!   them; they answer "invalid"). A soft reset or a queue initialisation
//!   waits for the command in flight and drops its completion.
//! * A completion with no room in the response ring is held and posted when
//!   the driver acknowledges a slot; the request ring is not consumed while
//!   any completion is held.
//! * The target model has no bus reset, so a bus reset or soft reset does not
//!   arm a unit attention on the disk as QEMU's SCSI devices do.
//! * Only logical unit 0 of a present target exists. A command for another
//!   unit is passed to the target with that unit number, which answers as
//!   SCSI says (INQUIRY: no device at this unit; anything else: check
//!   condition, logical unit not supported). An absent target times out.
//!
//! Register windows: BAR 0 is I/O space and BAR 1 memory space, both 256
//! bytes onto the same sixteen-bit registers. Accesses of any other width
//! are not decoded and are noted once.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use crate::irq::IrqLine;
use crate::pci::{Bar, PciBusDevice, PciConfig, PciDma, PCI_INTA};
use crate::scsi::{scsi_cmd, ScsiDataLength, ScsiDevice, ScsiIdentity, ScsiRequest};
use crate::snapshot::{get_field, hex_u16, hex_u32, load_u16_slice, toml_u16, toml_u32, toml_u8, u16_slice_to_toml, u8_slice_to_toml};

pub const PCI_VENDOR_QLOGIC: u16 = 0x1077;
pub const PCI_DEVICE_ISP1040: u16 = 0x1020;
pub const PCI_REVISION_ISP1040: u8 = 0x05;
/// Mass storage controller, SCSI.
pub const PCI_CLASS_SCSI: u32 = 0x0100_00;

/// What a disk or CD-ROM attached here answers INQUIRY with (the same
/// identity the O2's controller gives its targets).
pub const ISP_DISK: ScsiIdentity = ScsiIdentity::new(b"SGI     ", b"IRIS EMULATED   ", b"1.0 ");
pub const ISP_CDROM: ScsiIdentity = ISP_DISK;

pub const BLOCK_SIZE: usize = 512;

/// Register offsets.
pub mod reg {
    pub const ID_LOW: u32 = 0x00;
    pub const ID_HIGH: u32 = 0x02;
    pub const BUS_CFG0: u32 = 0x04;
    pub const BUS_CFG1: u32 = 0x06;
    pub const BUS_CTRL: u32 = 0x08;
    pub const BUS_STATUS: u32 = 0x0a;
    pub const SEMAPHORE: u32 = 0x0c;
    pub const NVRAM: u32 = 0x0e;
    pub const DMA_CMD_CFG: u32 = 0x20;
    pub const DMA_CMD_CTRL: u32 = 0x22;
    pub const DMA_DATA_CFG: u32 = 0x40;
    pub const DMA_DATA_CTRL: u32 = 0x42;
    pub const MAILBOX_BASE: u32 = 0x70;
    pub const REQUEST_QUEUE: u32 = MAILBOX_BASE + 0x08;
    pub const RESPONSE_QUEUE: u32 = MAILBOX_BASE + 0x0a;
    pub const BANK_BASE: u32 = 0x80;
    pub const HOST_CMD: u32 = 0xc0;
    pub const WINDOW_SIZE: u32 = 0x100;
}

const MAILBOX_COUNT: usize = 8;
const MAILBOX_END: u32 = reg::MAILBOX_BASE + MAILBOX_COUNT as u32 * 2;
const BANK_END: u32 = 0xc0;
const BANK_WORDS: usize = ((BANK_END - reg::BANK_BASE) / 2) as usize;
const MBOX_REQ: usize = 4;
const MBOX_RSP: usize = 5;

pub const CFG1_FIFO_MASK: u16 = 0x0070;
pub const CFG1_BANK_SELECT: u16 = 0x0008;
pub const CFG1_BURST_ENABLE: u16 = 0x0004;
const CFG1_WRITE_MASK: u16 = CFG1_FIFO_MASK | CFG1_BANK_SELECT | CFG1_BURST_ENABLE;
const DMA_CFG_WRITE_MASK: u16 = 0x0002;

pub const CTRL_SOFT_RESET: u16 = 0x0001;
pub const CTRL_INT_ENABLE: u16 = 0x0002;
pub const CTRL_RISC_INT_ENABLE: u16 = 0x0004;
const CTRL_WRITE_MASK: u16 = CTRL_INT_ENABLE | CTRL_RISC_INT_ENABLE;

pub const STATUS_INT_REQ: u16 = 0x0002;
pub const STATUS_RISC_INT_REQ: u16 = 0x0004;

pub const SEMAPHORE_LOCK: u16 = 0x0001;
const SEMAPHORE_MASK: u16 = 0x0003;

// Data-in is never asserted: every EEPROM read fails validation and the
// driver falls back to its defaults, as with an unprogrammed part.
const NVRAM_WRITE_MASK: u16 = 0x0007;

pub const SIG1: u16 = 0x4953;
pub const SIG2: u16 = 0x5020;
pub const SIG3: u16 = 0x2020;
pub const SIG4: u16 = 0x0001;

/// Mailbox command codes and completion codes.
pub mod mbox {
    pub const NOP: u16 = 0x0000;
    pub const LOAD_RAM: u16 = 0x0001;
    pub const EXEC_FIRMWARE: u16 = 0x0002;
    pub const DUMP_RAM: u16 = 0x0003;
    pub const WRITE_RAM_WORD: u16 = 0x0004;
    pub const READ_RAM_WORD: u16 = 0x0005;
    pub const REGISTER_TEST: u16 = 0x0006;
    pub const VERIFY_CHECKSUM: u16 = 0x0007;
    pub const ABOUT_FIRMWARE: u16 = 0x0008;
    pub const LOAD_RAM_A64: u16 = 0x0009;
    pub const INIT_REQ_QUEUE: u16 = 0x0010;
    pub const INIT_RSP_QUEUE: u16 = 0x0011;
    pub const ABORT_COMMAND: u16 = 0x0015;
    pub const BUS_RESET: u16 = 0x0018;
    pub const GET_FIRMWARE_STATUS: u16 = 0x001f;
    pub const GET_TARGET_PARAMETERS: u16 = 0x0028;
    pub const SET_INITIATOR_ID: u16 = 0x0030;
    pub const SET_SELECTION_TIMEOUT: u16 = 0x0031;
    pub const SET_RETRY_COUNT: u16 = 0x0032;
    pub const SET_TAG_AGE_LIMIT: u16 = 0x0033;
    pub const SET_CLOCK_RATE: u16 = 0x0034;
    pub const SET_ACTIVE_NEGATION: u16 = 0x0035;
    pub const SET_ASYNC_DATA_SETUP: u16 = 0x0036;
    pub const SET_PCI_CONTROL: u16 = 0x0037;
    pub const SET_TARGET_PARAMETERS: u16 = 0x0038;
    pub const SET_DEVICE_QUEUE: u16 = 0x0039;
    pub const SET_RESET_DELAY_PARAMS: u16 = 0x003a;
    pub const SET_SYSTEM_PARAMETER: u16 = 0x0045;
    pub const SET_FIRMWARE_FEATURES: u16 = 0x004a;
    pub const INIT_REQ_QUEUE_A64: u16 = 0x0052;
    pub const INIT_RSP_QUEUE_A64: u16 = 0x0053;
    pub const SET_DATA_OVERRUN_RECOVERY: u16 = 0x005a;

    pub const COMPLETE: u16 = 0x4000;
    pub const INVALID: u16 = 0x4001;
    pub const HOST_INTERFACE_ERROR: u16 = 0x4002;
    pub const TEST_FAILED: u16 = 0x4003;
    pub const PARAM_ERROR: u16 = 0x4006;
}

const RAM_WORDS: usize = 0x10000;
/// The command budget the adapter announces in its firmware status.
pub const MAX_REQUESTS: u16 = 512;
const FW_VERSION: [u16; 3] = [1, 0, 0];

pub const HOST_CMD_SHIFT: u32 = 12;
pub const RISC_FLAG_HOST_INT: u16 = 0x0080;
pub const RISC_FLAG_RESET: u16 = 0x0040;
pub const RISC_FLAG_PAUSED: u16 = 0x0020;

pub const ENTRY_SIZE: usize = 64;
pub const ENTRY_TYPE_REQUEST: u8 = 0x01;
pub const ENTRY_TYPE_STATUS: u8 = 0x03;
pub const ENTRY_TYPE_MARKER: u8 = 0x04;
pub const ENTRY_TYPE_COMMAND_A64: u8 = 0x09;

const REQ_OFF_HANDLE: u64 = 4;
const REQ_OFF_DEVICE: u64 = 8;
const REQ_OFF_CDBLEN: u64 = 10;
const REQ_OFF_SEGCOUNT: u64 = 18;
const REQ_OFF_CDB: u64 = 20;
const CDB_FIELD_LEN: usize = 12;
const REQ_OFF_SEGS: u64 = 32;
const REQ_SEGS_INLINE: usize = 4;
const SEG_SIZE: u64 = 8;
const CONT_OFF_SEGS: u64 = 8;
const CONT_SEGS: usize = 7;
const A64_OFF_SEGS: u64 = 40;
const A64_SEGS_INLINE: usize = 2;
const A64_SEG_SIZE: u64 = 12;
const A64_CONT_OFF_SEGS: u64 = 4;
const A64_CONT_SEGS: usize = 5;

const SENSE_MAX: usize = 32;
pub const SCSI_STATUS_SENSE_VALID: u16 = 0x0200;

pub const IOCB_COMPLETE: u16 = 0x0000;
pub const IOCB_INCOMPLETE: u16 = 0x0001;
pub const IOCB_DATA_UNDERRUN: u16 = 0x0015;

pub const STATE_GOT_BUS: u16 = 0x0100;
pub const STATE_GOT_SENSE: u16 = 0x2000;
/// Every phase a completed command traverses; the ISP1040 reports this for
/// all of them, gated only on whether sense was collected.
pub const STATE_ALL_PHASES: u16 = 0x7f00;
pub const STATUS_FLAG_TIMEOUT: u16 = 0x0040;

const CHECK_CONDITION: u8 = 0x02;
const MAX_TARGET: u8 = 15;
const MAX_LUN: u8 = 7;

/// A completion held for response-ring room.
#[derive(Clone, Default, PartialEq, Debug)]
struct Pending {
    handle: u32,
    scsi_status: u16,
    completion: u16,
    state_flags: u16,
    status_flags: u16,
    resid: u32,
    sense: Vec<u8>,
}

/// What the request-ring thread and the register side tell each other.
#[derive(Default)]
struct Ctl {
    /// Bumped by every reset and queue initialisation: a command that
    /// finishes under another epoch has nowhere to post.
    epoch: u64,
    /// The ring may have entries the thread has not looked at.
    work: bool,
    /// The thread is walking the ring.
    walking: bool,
    /// A command is running with the state unlocked.
    busy: bool,
    /// Holders that need the thread not to start another command
    /// (saving and loading state).
    hold: u32,
    /// Resets and queue initialisations waiting for the command in flight;
    /// that command's completion waits for them, and is dropped.
    settling: u32,
    shutdown: bool,
}

/// One request entry, read and ready to run against its target.
struct Job {
    handle: u32,
    target: u8,
    cdb: Vec<u8>,
    xfer: usize,
    to_dev: bool,
    segs: Vec<(u64, u32)>,
    described: u64,
}

/// What reading a request entry came to.
enum Prepared {
    /// The fabric refused the entry; the walk stops on it.
    Refused,
    /// Answered without reaching a target.
    Answered,
    Run(Job),
}

struct State {
    ctl: Ctl,
    bus_cfg1: u16,
    bus_ctrl: u16,
    bus_status: u16,
    semaphore: u16,
    nvram: u16,
    risc_flags: u16,
    dma_cmd_cfg: u16,
    dma_data_cfg: u16,
    mailbox: [u16; MAILBOX_COUNT],
    risc_bank: [u16; BANK_WORDS],
    sxp_bank: [u16; BANK_WORDS],
    risc_ram: Vec<u16>,
    reset_delay: u16,
    rqst_base: u64,
    resp_base: u64,
    rqst_len: u16,
    resp_len: u16,
    rqst_cons: u16,
    resp_prod: u16,
    resp_cons: u16,
    pending: VecDeque<Pending>,
    total_cmds: u64,
}

impl State {
    fn new() -> Self {
        let mut s = State {
            ctl: Ctl::default(),
            bus_cfg1: 0,
            bus_ctrl: 0,
            bus_status: 0,
            semaphore: 0,
            nvram: 0,
            risc_flags: 0,
            dma_cmd_cfg: 0,
            dma_data_cfg: 0,
            mailbox: [0; MAILBOX_COUNT],
            risc_bank: [0; BANK_WORDS],
            sxp_bank: [0; BANK_WORDS],
            risc_ram: vec![0; RAM_WORDS],
            reset_delay: 0,
            rqst_base: 0,
            resp_base: 0,
            rqst_len: 0,
            resp_len: 0,
            rqst_cons: 0,
            resp_prod: 0,
            resp_cons: 0,
            pending: VecDeque::new(),
            total_cmds: 0,
        };
        s.reset();
        s
    }

    /// Power-on and guest soft reset land on one state. Mailbox 4's
    /// signature lives in the array so the register test's echo works before
    /// any queue exists.
    fn reset(&mut self) {
        self.ctl.epoch += 1;
        self.bus_cfg1 = 0;
        self.bus_ctrl = 0;
        self.bus_status = 0;
        self.semaphore = 0;
        self.nvram = 0;
        self.risc_flags = 0;
        self.dma_cmd_cfg = 0;
        self.dma_data_cfg = 0;
        self.mailbox = [0, SIG1, SIG2, SIG3, SIG4, 0, 0, 0];
        self.risc_bank = [0; BANK_WORDS];
        self.sxp_bank = [0; BANK_WORDS];
        // The driver re-downloads firmware after every reset.
        self.risc_ram.iter_mut().for_each(|w| *w = 0);
        self.reset_delay = 0;
        self.rqst_base = 0;
        self.resp_base = 0;
        self.rqst_len = 0;
        self.resp_len = 0;
        self.rqst_cons = 0;
        self.resp_prod = 0;
        self.resp_cons = 0;
        self.pending.clear();
    }

    /// Only the RISC request bit and its enable drive the line; they share a
    /// bit position.
    fn irq_level(&self) -> bool {
        self.bus_status & self.bus_ctrl & STATUS_RISC_INT_REQ != 0
    }

    fn resp_has_room(&self) -> bool {
        // One slot reserved so producer equal to consumer always means
        // empty; an uninitialised queue has no room.
        self.resp_len != 0 && (self.resp_prod + 1) % self.resp_len != self.resp_cons
    }

    fn active_bank(&mut self) -> &mut [u16; BANK_WORDS] {
        if self.bus_cfg1 & CFG1_BANK_SELECT != 0 { &mut self.sxp_bank } else { &mut self.risc_bank }
    }
}

/// The adapter. Its state lives in a [`Core`] shared with the thread that
/// runs the request ring.
pub struct Isp1040 {
    core: Arc<Core>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::ops::Deref for Isp1040 {
    type Target = Core;
    fn deref(&self) -> &Core { &self.core }
}

pub struct Core {
    st: Mutex<State>,
    /// Signals [`Ctl`] changes both ways.
    wake: Condvar,
    /// Serialises driving the interrupt line; see [`Core::sync_irq`].
    line_order: Mutex<()>,
    /// The targets on the bus, by SCSI id.
    targets: Mutex<[Option<ScsiDevice>; 16]>,
    dma: OnceLock<Arc<dyn PciDma>>,
    irq: OnceLock<IrqLine>,
    /// Which of the Octane's two controllers this is (0 internal, 1
    /// external): the monitor's `scsi`/`cow` address the first,
    /// `scsi1`/`cow1` the second.
    index: u8,
    /// The SCSI id a disk attached without one answers at.
    target: u8,
    pci: PciConfig,
    notes: Mutex<Vec<String>>,
    dma_misses: Mutex<std::collections::BTreeSet<u64>>,
    odd_accesses: Mutex<u32>,
}

impl Default for Isp1040 {
    fn default() -> Self { Self::new() }
}

fn le16(b: &[u8], off: usize) -> u16 { u16::from_le_bytes([b[off], b[off + 1]]) }

impl Isp1040 {
    pub fn new() -> Self {
        Self::new_at(0)
    }

    /// The controller at `index`: 0 internal, 1 external.
    pub fn new_at(index: u8) -> Self {
        let core = Arc::new(Core::new_at(index));
        let c = core.clone();
        let worker = std::thread::Builder::new()
            .name(format!("ISP1040-{index}"))
            .spawn(move || c.run_ring())
            .expect("spawn isp1040 thread");
        Self { core, worker: Mutex::new(Some(worker)) }
    }
}

impl Drop for Isp1040 {
    fn drop(&mut self) {
        self.st.lock().unwrap().ctl.shutdown = true;
        self.wake.notify_all();
        if let Some(h) = self.worker.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl Core {
    fn new_at(index: u8) -> Self {
        Self {
            st: Mutex::new(State::new()),
            wake: Condvar::new(),
            line_order: Mutex::new(()),
            targets: Mutex::new(std::array::from_fn(|_| None)),
            dma: OnceLock::new(),
            irq: OnceLock::new(),
            index,
            target: 1,
            // BAR 0 is the I/O window and BAR 1 its memory-space alias, one
            // register file.
            pci: PciConfig::new(
                PCI_VENDOR_QLOGIC, PCI_DEVICE_ISP1040, PCI_CLASS_SCSI, PCI_REVISION_ISP1040,
                [Bar::Io(reg::WINDOW_SIZE), Bar::Mem(reg::WINDOW_SIZE), Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused],
                PCI_INTA,
            ),
            notes: Mutex::new(Vec::new()),
            dma_misses: Mutex::new(std::collections::BTreeSet::new()),
            odd_accesses: Mutex::new(0),
        }
    }

    pub fn index(&self) -> u8 { self.index }

    /// Wire the adapter's interrupt pin to `line` (a level).
    pub fn connect_irq(&self, line: IrqLine) {
        let _ = self.irq.set(line);
        self.sync_irq();
    }

    /// True while the adapter asserts its interrupt line.
    pub fn interrupting(&self) -> bool {
        self.st.lock().unwrap().irq_level()
    }

    /// Drive the line to the level the state gives. The level is read
    /// after taking `line_order`, so whichever of the CPU and the ring
    /// thread drives last has seen the latest state. `st` is never held
    /// when this is called.
    fn sync_irq(&self) {
        let _order = self.line_order.lock().unwrap();
        let level = self.st.lock().unwrap().irq_level();
        if let Some(l) = self.irq.get() {
            l.set(level);
        }
    }

    /// Block until the ring thread has run everything it was given and is
    /// waiting for more (or is held on a full response ring).
    pub fn wait_idle(&self) {
        let mut st = self.st.lock().unwrap();
        while (st.ctl.work && st.ctl.hold == 0) || st.ctl.walking {
            st = self.wake.wait(st).unwrap();
        }
    }

    /// Wait out the command in flight before a reset or a new queue, keeping
    /// the lock. The command's completion then waits for the caller's change
    /// and sees the new epoch.
    fn settle<'a>(&self, mut st: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        st.ctl.settling += 1;
        while st.ctl.busy {
            st = self.wake.wait(st).unwrap();
        }
        st.ctl.settling -= 1;
        self.wake.notify_all();
        st
    }

    /// A reset or a save is waiting for the command in flight (tests).
    #[cfg(test)]
    fn waiting_on_command(&self) -> bool {
        let st = self.st.lock().unwrap();
        st.ctl.settling != 0 || st.ctl.hold != 0
    }

    /// Run `f` on the state with no command in flight and none allowed to
    /// start: the quiesce point for saving and loading.
    fn quiesced<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut st = self.st.lock().unwrap();
        st.ctl.hold += 1;
        // Not `settle`: a command in flight posts its completion first.
        while st.ctl.busy {
            st = self.wake.wait(st).unwrap();
        }
        let r = f(&mut st);
        st.ctl.hold -= 1;
        drop(st);
        self.wake.notify_all();
        r
    }

    /// Tell the ring thread the ring may have moved.
    fn kick(&self, st: &mut State) {
        st.ctl.work = true;
        self.wake.notify_all();
    }

    /// Commands run, for the whole session.
    pub fn total_commands(&self) -> u64 {
        self.st.lock().unwrap().total_cmds
    }

    /// A running commentary on the odd things a driver did (accesses of a
    /// width the part does not decode, DMA that is not memory, commands not
    /// modelled).
    pub fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap().clone()
    }

    fn note(&self, what: String) {
        let mut n = self.notes.lock().unwrap();
        if n.len() < 256 {
            n.push(what);
        }
    }

    // ---- attaching targets -------------------------------------------

    /// Attach a disk image at a SCSI id.
    pub fn attach_disk_at(&self, target: u8, path: &std::path::Path) -> std::io::Result<u64> {
        self.attach_at(target, path, false)
    }

    pub fn attach_at(&self, target: u8, path: &std::path::Path, cdrom: bool) -> std::io::Result<u64> {
        self.attach_with(target, path, cdrom, false)
    }

    /// [`Self::attach_at`], copy-on-write when `overlay` is set: the image is
    /// never written, writes go to `{path}.overlay`.
    pub fn attach_with(&self, target: u8, path: &std::path::Path, cdrom: bool, overlay: bool) -> std::io::Result<u64> {
        let path = path
            .to_str()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path is not UTF-8"))?;
        self.attach_device(target, path, cdrom, Vec::new(), overlay, None)
    }

    /// Attach what a `[scsi.N]` table describes: the image at `path` (empty
    /// for a CD-ROM with its tray empty), a CD-ROM's changer list `discs`,
    /// and copy-on-write when `overlay` is set, to `overlay_path` or
    /// `{path}.overlay`. Returns the size in blocks.
    pub fn attach_device(
        &self,
        target: u8,
        path: &str,
        cdrom: bool,
        discs: Vec<String>,
        overlay: bool,
        overlay_path: Option<&str>,
    ) -> std::io::Result<u64> {
        let d = ScsiDevice::open(path, cdrom, discs, overlay, overlay_path, if cdrom { ISP_CDROM } else { ISP_DISK })?;
        let blocks = d.size() / BLOCK_SIZE as u64;
        let mut targets = self.targets.lock().unwrap();
        let slot = targets.get_mut(target as usize).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("SCSI id {target} out of range"))
        })?;
        *slot = Some(d);
        Ok(blocks)
    }

    /// The id a disk attached without one goes on.
    pub fn default_target(&self) -> u8 { self.target }

    pub fn attach_disk(&self, path: &std::path::Path) -> std::io::Result<u64> {
        self.attach_disk_at(self.target, path)
    }

    /// The ids that have something on them.
    pub fn attached(&self) -> Vec<u8> {
        let t = self.targets.lock().unwrap();
        (0..t.len() as u8).filter(|&i| t[i as usize].is_some()).collect()
    }

    fn has_target(&self, id: u8) -> bool {
        self.targets.lock().unwrap().get(id as usize).is_some_and(|t| t.is_some())
    }

    // ---- DMA ---------------------------------------------------------

    fn dma_ok(addr: u64, len: usize) -> bool {
        addr.checked_add(len as u64).is_some()
    }

    fn dma_miss(&self, addr: u64, write: bool) {
        let first = {
            let mut seen = self.dma_misses.lock().unwrap();
            seen.len() < 64 && seen.insert(addr & !0xfff)
        };
        if first {
            let what = if write { "write to" } else { "read from" };
            eprintln!("isp1040: DMA {what} {addr:#x} is not memory");
            self.note(format!("DMA {what} {addr:#x} is not memory"));
        }
    }

    /// `buf.len()` bytes from bus address `addr`; false (and zeros) if the
    /// fabric refuses any of them.
    fn dma_read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let ok = Self::dma_ok(addr, buf.len()) && self.dma.get().is_some_and(|d| d.dma_read(addr, buf).is_ok());
        if !ok {
            buf.iter_mut().for_each(|b| *b = 0);
            self.dma_miss(addr, false);
        }
        ok
    }

    fn dma_write(&self, addr: u64, data: &[u8]) -> bool {
        let ok = Self::dma_ok(addr, data.len()) && self.dma.get().is_some_and(|d| d.dma_write(addr, data).is_ok());
        if !ok {
            self.dma_miss(addr, true);
        }
        ok
    }

    // A descriptor read the fabric refuses answers zero, deliberately: a
    // zeroed entry type takes the fail-visibly default and a zeroed segment
    // transfers nothing.
    fn ld8(&self, addr: u64) -> u8 {
        let mut b = [0u8; 1];
        self.dma_read(addr, &mut b);
        b[0]
    }
    fn ld16(&self, addr: u64) -> u16 {
        let mut b = [0u8; 2];
        self.dma_read(addr, &mut b);
        u16::from_le_bytes(b)
    }
    fn ld32(&self, addr: u64) -> u32 {
        let mut b = [0u8; 4];
        self.dma_read(addr, &mut b);
        u32::from_le_bytes(b)
    }

    // ---- RISC RAM ----------------------------------------------------

    fn mbox_host_addr(mb: &[u16; MAILBOX_COUNT]) -> u64 {
        ((mb[2] as u64) << 16) | mb[3] as u64
    }

    /// The whole transfer is read before a word of it is kept, so a refused
    /// DMA leaves the array as it was. Words arrive little-endian. This part
    /// has no edge to its array: a transfer past the end folds to the start.
    fn ram_load(&self, st: &mut State) -> u16 {
        let base = st.mailbox[1];
        let count = st.mailbox[4] as usize;
        let host = Self::mbox_host_addr(&st.mailbox);
        if count == 0 {
            return mbox::COMPLETE;
        }
        let mut bytes = vec![0u8; count * 2];
        if !self.dma_read(host, &mut bytes) {
            return mbox::HOST_INTERFACE_ERROR;
        }
        for i in 0..count {
            st.risc_ram[base.wrapping_add(i as u16) as usize] = le16(&bytes, i * 2);
        }
        mbox::COMPLETE
    }

    fn ram_dump(&self, st: &State) -> u16 {
        let base = st.mailbox[1];
        let count = st.mailbox[4] as usize;
        let host = Self::mbox_host_addr(&st.mailbox);
        if count == 0 {
            return mbox::COMPLETE;
        }
        let mut bytes = Vec::with_capacity(count * 2);
        for i in 0..count {
            bytes.extend_from_slice(&st.risc_ram[base.wrapping_add(i as u16) as usize].to_le_bytes());
        }
        if self.dma_write(host, &bytes) { mbox::COMPLETE } else { mbox::HOST_INTERFACE_ERROR }
    }

    // ---- mailbox -----------------------------------------------------

    /// Synchronous: the protocol has no way to leave a command pending.
    fn mailbox_command(&self, st: &mut State) {
        let cmd = st.mailbox[0];
        match cmd {
            mbox::NOP => st.mailbox[0] = mbox::COMPLETE,
            mbox::WRITE_RAM_WORD => {
                st.risc_ram[st.mailbox[1] as usize] = st.mailbox[2];
                st.mailbox[0] = mbox::COMPLETE;
            }
            // Mailbox 3 is an output too; left alone it hands back stale data.
            mbox::READ_RAM_WORD => {
                st.mailbox[2] = st.risc_ram[st.mailbox[1] as usize];
                st.mailbox[3] = 0;
                st.mailbox[0] = mbox::COMPLETE;
            }
            mbox::LOAD_RAM => st.mailbox[0] = self.ram_load(st),
            mbox::DUMP_RAM => st.mailbox[0] = self.ram_dump(st),
            // Mailboxes 1-7 are expected back unchanged; they already are.
            mbox::REGISTER_TEST => st.mailbox[0] = mbox::COMPLETE,
            // The word at base+3 is the image's length; a real image sums to
            // zero.
            mbox::VERIFY_CHECKSUM => {
                let start = st.mailbox[1];
                let len = st.risc_ram[start.wrapping_add(3) as usize];
                let mut sum = 0u16;
                for i in 0..len {
                    sum = sum.wrapping_add(st.risc_ram[start.wrapping_add(i) as usize]);
                }
                // The firmware reads the mailbox 2 copy.
                st.mailbox[1] = sum;
                st.mailbox[2] = sum;
                st.mailbox[0] = if sum == 0 { mbox::COMPLETE } else { mbox::TEST_FAILED };
            }
            // Accepted, never executed.
            mbox::EXEC_FIRMWARE => st.mailbox[0] = mbox::COMPLETE,
            mbox::ABOUT_FIRMWARE => {
                st.mailbox[1] = FW_VERSION[0];
                st.mailbox[2] = FW_VERSION[1];
                st.mailbox[3] = FW_VERSION[2];
                st.mailbox[6] = 0;
                st.mailbox[0] = mbox::COMPLETE;
            }
            // Sizes the driver's queues and command budget.
            mbox::GET_FIRMWARE_STATUS => {
                st.mailbox[1] = 0;
                st.mailbox[2] = MAX_REQUESTS;
                st.mailbox[3] = 0;
                st.mailbox[0] = mbox::COMPLETE;
            }
            // Asynchronous and narrow: the true state of an unnegotiated bus.
            mbox::GET_TARGET_PARAMETERS => {
                st.mailbox[2] = 0;
                st.mailbox[3] = 0;
                st.mailbox[0] = mbox::COMPLETE;
            }
            // Setters for state nothing reads back; answering invalid is a
            // fatal attach error, so they complete.
            mbox::SET_FIRMWARE_FEATURES
            | mbox::SET_CLOCK_RATE
            | mbox::SET_RETRY_COUNT
            | mbox::SET_ASYNC_DATA_SETUP
            | mbox::SET_ACTIVE_NEGATION
            | mbox::SET_TAG_AGE_LIMIT
            | mbox::SET_SELECTION_TIMEOUT
            | mbox::SET_INITIATOR_ID
            | mbox::SET_TARGET_PARAMETERS
            | mbox::SET_DEVICE_QUEUE
            | mbox::SET_PCI_CONTROL
            | mbox::SET_SYSTEM_PARAMETER
            | mbox::SET_DATA_OVERRUN_RECOVERY => st.mailbox[0] = mbox::COMPLETE,
            mbox::SET_RESET_DELAY_PARAMS => {
                st.reset_delay = st.mailbox[1];
                st.mailbox[0] = mbox::COMPLETE;
            }
            mbox::INIT_REQ_QUEUE | mbox::INIT_REQ_QUEUE_A64 | mbox::INIT_RSP_QUEUE | mbox::INIT_RSP_QUEUE_A64 => {
                let a64 = cmd == mbox::INIT_REQ_QUEUE_A64 || cmd == mbox::INIT_RSP_QUEUE_A64;
                let mut addr = Self::mbox_host_addr(&st.mailbox);
                if a64 {
                    addr |= ((st.mailbox[7] as u64) << 32) | ((st.mailbox[6] as u64) << 48);
                }
                if addr == 0 || st.mailbox[1] == 0 {
                    st.mailbox[0] = mbox::PARAM_ERROR;
                } else if cmd == mbox::INIT_REQ_QUEUE || cmd == mbox::INIT_REQ_QUEUE_A64 {
                    st.ctl.epoch += 1;
                    st.rqst_base = addr;
                    st.rqst_len = st.mailbox[1];
                    st.rqst_cons = 0;
                    st.mailbox[MBOX_REQ] = 0;
                    st.mailbox[0] = mbox::COMPLETE;
                } else {
                    st.ctl.epoch += 1;
                    st.resp_base = addr;
                    st.resp_len = st.mailbox[1];
                    st.resp_prod = 0;
                    st.resp_cons = 0;
                    // The backlog goes with the ring it was waiting for.
                    st.pending.clear();
                    st.mailbox[0] = mbox::COMPLETE;
                }
            }
            // Resets the bus, not the adapter: chip state survives. Mailbox 1
            // holds the configured reset delay, not a selector. A command in
            // flight is not retired; it completes as it would have.
            mbox::BUS_RESET => st.mailbox[0] = mbox::COMPLETE,
            _ => {
                self.note(format!("mailbox command {cmd:#06x} not modelled"));
                st.mailbox[0] = mbox::INVALID;
            }
        }
        st.semaphore = SEMAPHORE_LOCK;
        st.bus_status |= STATUS_INT_REQ | STATUS_RISC_INT_REQ;
    }

    fn host_command(&self, st: &mut State, val: u16) {
        match val >> HOST_CMD_SHIFT {
            0x0 => {}
            // A RISC reset replaces the whole flag set.
            0x1 => st.risc_flags = RISC_FLAG_RESET,
            0x2 => st.risc_flags |= RISC_FLAG_PAUSED,
            // Release clears reset too, or the flag would stand forever.
            0x3 => st.risc_flags &= !(RISC_FLAG_PAUSED | RISC_FLAG_RESET),
            // "A mailbox command is ready": the trigger to process one.
            0x5 => {
                st.risc_flags |= RISC_FLAG_HOST_INT;
                self.mailbox_command(st);
            }
            // Does nothing on real hardware; neither public driver issues it.
            0x6 => {}
            // The semaphore is released by the driver's separate write.
            0x7 => {
                st.risc_flags &= !RISC_FLAG_HOST_INT;
                st.bus_status = 0;
            }
            // Accepted with nothing behind them, so the attach proceeds.
            c @ (0x4 | 0x9 | 0xa | 0xe) => {
                self.note(format!("host command {c:#x} accepted, not modelled"));
            }
            c => self.note(format!("unknown host command {c:#x}")),
        }
    }

    // ---- completions -------------------------------------------------

    /// An I/O completion never takes the semaphore lock: the driver reads
    /// the semaphore first and falls through to bus status only when it is
    /// clear.
    fn post_one(&self, st: &mut State, p: &Pending) {
        let addr = st.resp_base + st.resp_prod as u64 * ENTRY_SIZE as u64;
        let mut entry = [0u8; ENTRY_SIZE];
        let sense_len = p.sense.len().min(SENSE_MAX);
        entry[0] = ENTRY_TYPE_STATUS;
        entry[1] = 1;
        entry[4..8].copy_from_slice(&p.handle.to_le_bytes());
        entry[8..10].copy_from_slice(&p.scsi_status.to_le_bytes());
        entry[10..12].copy_from_slice(&p.completion.to_le_bytes());
        entry[12..14].copy_from_slice(&p.state_flags.to_le_bytes());
        entry[14..16].copy_from_slice(&p.status_flags.to_le_bytes());
        entry[18..20].copy_from_slice(&(sense_len as u16).to_le_bytes());
        entry[20..24].copy_from_slice(&p.resid.to_le_bytes());
        entry[32..32 + sense_len].copy_from_slice(&p.sense[..sense_len]);
        // A completion the fabric refuses to store is dropped: advancing the
        // producer or raising the interrupt would point the driver at a slot
        // still holding a previous entry's bytes.
        if !self.dma_write(addr, &entry) {
            self.note(format!("status entry write to {addr:#x} refused; completion dropped"));
            return;
        }
        st.resp_prod = (st.resp_prod + 1) % st.resp_len;
        st.bus_status |= STATUS_INT_REQ | STATUS_RISC_INT_REQ;
    }

    /// A model with no bus latency can outrun the driver; the surplus is
    /// held.
    fn post_status(&self, st: &mut State, p: Pending) {
        if st.resp_len == 0 {
            self.note("status posted before the response queue was initialised".into());
            return;
        }
        if !st.resp_has_room() {
            self.note(format!("response queue full, holding handle {:#010x} until acknowledged", p.handle));
            st.pending.push_back(p);
            return;
        }
        self.post_one(st, &p);
    }

    /// Only the driver's acknowledgement can free room, so only it calls
    /// this.
    fn drain_pending(&self, st: &mut State) {
        while st.resp_has_room() {
            let Some(p) = st.pending.pop_front() else { break };
            self.post_one(st, &p);
        }
    }

    fn incomplete(handle: u32, state_flags: u16, status_flags: u16) -> Pending {
        Pending { handle, completion: IOCB_INCOMPLETE, state_flags, status_flags, ..Default::default() }
    }

    // ---- requests ----------------------------------------------------

    /// How many bytes a command is declared to move, and whether they go to
    /// the device. `block` is the target's logical block size.
    fn cdb_transfer(cdb: &[u8], block: usize) -> Option<(usize, bool)> {
        let at = |i: usize| cdb.get(i).copied().unwrap_or(0) as usize;
        let be16 = |i: usize| (at(i) << 8) | at(i + 1);
        let be24 = |i: usize| (at(i) << 16) | (at(i + 1) << 8) | at(i + 2);
        let six = || if at(4) == 0 { 256 } else { at(4) };
        match cdb[0] {
            scsi_cmd::READ_6 => Some((six() * block, false)),
            scsi_cmd::WRITE_6 => Some((six() * block, true)),
            scsi_cmd::READ_10 => Some((be16(7) * block, false)),
            scsi_cmd::WRITE_10 => Some((be16(7) * block, true)),
            scsi_cmd::INQUIRY => Some((be16(3), false)),
            scsi_cmd::REQUEST_SENSE | scsi_cmd::MODE_SENSE_6 => Some((at(4), false)),
            scsi_cmd::MODE_SELECT_6 => Some((at(4), true)),
            scsi_cmd::MODE_SENSE_10
            | scsi_cmd::READ_TOC_PMA_ATIP
            | scsi_cmd::GET_CONFIGURATION
            | scsi_cmd::READ_SUB_CHANNEL
            | scsi_cmd::READ_DISC_INFORMATION => Some((be16(7), false)),
            scsi_cmd::MODE_SELECT_10 => Some((be16(7), true)),
            scsi_cmd::READ_CAPACITY_10 => Some((8, false)),
            scsi_cmd::READ_BUFFER => Some((be24(6), false)),
            scsi_cmd::WRITE_BUFFER => Some((be24(6), true)),
            scsi_cmd::SEND_DIAGNOSTIC => Some((be16(3), true)),
            scsi_cmd::RECEIVE_DIAGNOSTIC_RESULTS => Some((be16(3), false)),
            _ => Some((0, false)),
        }
    }

    /// One descriptor into the transfer. The descriptors name the host's
    /// buffer and the command names how much of it the target has to fill,
    /// so only that much is offered and a longer buffer keeps its tail. The
    /// whole named length is still counted: the residual is measured against
    /// it.
    fn add_segment(segs: &mut Vec<(u64, u32)>, described: &mut u64, max: u64, base: u64, len: u32) {
        let room = max.saturating_sub(*described);
        let placed = (len as u64).min(room);
        *described += len as u64;
        if placed > 0 {
            segs.push((base, placed as u32));
        }
    }

    fn read_segments(&self, st: &State, entry_addr: u64, entry_count: u8, seg_count: u16, max: u64, a64: bool) -> (Vec<(u64, u32)>, u64) {
        let (inline_off, inline_n, seg_size, cont_off, cont_n) = if a64 {
            (A64_OFF_SEGS, A64_SEGS_INLINE, A64_SEG_SIZE, A64_CONT_OFF_SEGS, A64_CONT_SEGS)
        } else {
            (REQ_OFF_SEGS, REQ_SEGS_INLINE, SEG_SIZE, CONT_OFF_SEGS, CONT_SEGS)
        };
        let seg_at = |base: u64| -> (u64, u32) {
            if a64 {
                let lo = self.ld32(base) as u64;
                let hi = self.ld32(base + 4) as u64;
                (lo | (hi << 32), self.ld32(base + 8))
            } else {
                (self.ld32(base) as u64, self.ld32(base + 4))
            }
        };
        let mut segs = Vec::new();
        let mut described = 0u64;
        let mut collected = 0u16;
        for i in 0..inline_n {
            if collected >= seg_count { break; }
            let (a, l) = seg_at(entry_addr + inline_off + i as u64 * seg_size);
            Self::add_segment(&mut segs, &mut described, max, a, l);
            collected += 1;
        }
        let mut cont_index = st.rqst_cons;
        let mut conts_left = entry_count as i32 - 1;
        while collected < seg_count && conts_left > 0 {
            cont_index = (cont_index + 1) % st.rqst_len;
            let cont_addr = st.rqst_base + cont_index as u64 * ENTRY_SIZE as u64;
            for i in 0..cont_n {
                if collected >= seg_count { break; }
                let (a, l) = seg_at(cont_addr + cont_off + i as u64 * seg_size);
                Self::add_segment(&mut segs, &mut described, max, a, l);
                collected += 1;
            }
            conts_left -= 1;
        }
        (segs, described)
    }

    /// Run a CDB against the target; `data` is what a write sends it.
    fn scsi(&self, target: u8, cdb: &[u8], data: Option<Vec<u8>>) -> (Vec<u8>, u8) {
        let mut targets = self.targets.lock().unwrap();
        let Some(dev) = targets.get_mut(target as usize).and_then(|t| t.as_mut()) else {
            return (Vec::new(), CHECK_CONDITION);
        };
        let req = ScsiRequest { cdb: cdb.to_vec(), data_len: ScsiDataLength::Unlimited, data_in: data };
        match dev.request(&req) {
            Ok(r) => (r.data, r.status),
            Err(e) => {
                eprintln!("isp1040: target {target}: {e}");
                (Vec::new(), CHECK_CONDITION)
            }
        }
    }

    /// Read a request entry. Refused when the fabric refused the CDB read:
    /// the walk stops on the entry and nothing is posted.
    fn prepare_request(&self, st: &mut State, addr: u64, entry_count: u8, a64: bool) -> Prepared {
        let handle = self.ld32(addr + REQ_OFF_HANDLE);
        let device = self.ld16(addr + REQ_OFF_DEVICE);
        let cdb_len = self.ld16(addr + REQ_OFF_CDBLEN);
        let seg_count = self.ld16(addr + REQ_OFF_SEGCOUNT);
        let mut cdb = [0u8; CDB_FIELD_LEN];
        if !self.dma_read(addr + REQ_OFF_CDB, &mut cdb) {
            self.note(format!("CDB read for the request entry at {addr:#x} refused; request dropped"));
            return Prepared::Refused;
        }
        let channel = (device >> 15) & 1;
        let target = ((device >> 8) & 0x7f) as u8;
        let lun = (device & 0xff) as u8;

        // A bus this part does not have.
        if channel >= 1 {
            self.post_status(st, Self::incomplete(handle, 0, 0));
            return Prepared::Answered;
        }
        // GOT_BUS alone is Linux's bad-target; the timeout bit beside it is
        // what lets the empty slot be treated as benign.
        if target > MAX_TARGET || lun > MAX_LUN || !self.has_target(target) {
            self.post_status(st, Self::incomplete(handle, STATE_GOT_BUS, STATUS_FLAG_TIMEOUT));
            return Prepared::Answered;
        }

        // The target model reads the unit number from the CDB's second byte.
        cdb[1] = (cdb[1] & 0x1f) | (lun << 5);
        let cdb = &cdb[..(cdb_len as usize).clamp(1, CDB_FIELD_LEN)];
        let block = self
            .targets
            .lock()
            .unwrap()
            .get(target as usize)
            .and_then(|t| t.as_ref())
            .map(|d| d.block_sizes().1 as usize)
            .filter(|&b| b != 0)
            .unwrap_or(BLOCK_SIZE);
        // An opcode with no length rule is sized by what the descriptors
        // name, and treated as data coming from the target.
        let known = Self::cdb_transfer(cdb, block);
        let (mut xfer, to_dev) = known.unwrap_or((0, false));
        let max = if known.is_some() { xfer as u64 } else { u64::MAX };
        let (segs, described) = self.read_segments(st, addr, entry_count, seg_count, max, a64);
        if known.is_none() {
            xfer = described.min(u32::MAX as u64) as usize;
        }
        st.total_cmds += 1;
        Prepared::Run(Job { handle, target, cdb: cdb.to_vec(), xfer, to_dev, segs, described })
    }

    /// Run a command against its target and build its completion. The
    /// state is not locked: this is the part that waits on the disk.
    fn run_job(&self, job: Job) -> Pending {
        let Job { handle, target, cdb, xfer, to_dev, segs, described } = job;
        let (status, moved);
        if to_dev {
            let mut buf = Vec::new();
            for &(a, l) in &segs {
                let mut chunk = vec![0u8; l as usize];
                if !self.dma_read(a, &mut chunk) {
                    self.note(format!("payload DMA for handle {handle:#x} refused by the fabric"));
                }
                buf.extend_from_slice(&chunk);
            }
            moved = buf.len() as u64;
            status = self.scsi(target, &cdb, Some(buf)).1;
        } else {
            let (data, s) = self.scsi(target, &cdb, None);
            status = s;
            // Offer the target's bytes to the descriptors in order.
            let take = (data.len() as u64).min(described).min(xfer as u64) as usize;
            let mut off = 0usize;
            for &(a, l) in &segs {
                if off >= take { break; }
                let n = (l as usize).min(take - off);
                if !self.dma_write(a, &data[off..off + n]) {
                    self.note(format!("payload DMA for handle {handle:#x} refused by the fabric"));
                    break;
                }
                off += n;
            }
            moved = off as u64;
        }

        // Auto-sense runs only after a CHECK CONDITION.
        let mut sense = Vec::new();
        if status == CHECK_CONDITION {
            sense = self.scsi(target, &[scsi_cmd::REQUEST_SENSE, 0, 0, 0, SENSE_MAX as u8, 0], None).0;
            sense.truncate(SENSE_MAX);
        }
        let mut scsi_status = status as u16;
        let mut state_flags = STATE_ALL_PHASES & !STATE_GOT_SENSE;
        if !sense.is_empty() {
            scsi_status |= SCSI_STATUS_SENSE_VALID;
            state_flags |= STATE_GOT_SENSE;
        }
        let total = described.max(xfer as u64);
        let unused = (total - moved.min(total)) as u32;
        // Underrun is a short data transfer, not a refused command: a target
        // answering with a status of its own carries the failure there.
        let completion = if unused != 0 && status == 0 { IOCB_DATA_UNDERRUN } else { IOCB_COMPLETE };
        Pending { handle, scsi_status, completion, state_flags, status_flags: 0, resid: unused, sense }
    }

    /// The ring thread: wait to be kicked, then walk the request ring.
    fn run_ring(&self) {
        let mut st = self.st.lock().unwrap();
        loop {
            while !st.ctl.shutdown && !(st.ctl.work && st.ctl.hold == 0) {
                st = self.wake.wait(st).unwrap();
            }
            if st.ctl.shutdown {
                return;
            }
            st.ctl.work = false;
            st.ctl.walking = true;
            st = self.consume_requests(st);
            st.ctl.walking = false;
            self.wake.notify_all();
        }
    }

    /// Walk the request ring from the adapter's consumer position to the
    /// driver's producer, one entry at a time. Each command runs with the
    /// state unlocked, so the CPU's register accesses never wait on a disk.
    fn consume_requests<'a>(&'a self, mut st: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        loop {
            if st.ctl.shutdown || st.ctl.hold > 0 {
                // Resume once the holder lets go.
                st.ctl.work = true;
                return st;
            }
            if st.rqst_len == 0 {
                return st;
            }
            let prod = st.mailbox[MBOX_REQ] % st.rqst_len;
            // No exact-equality exit terminates for every driver-supplied
            // index and entry count, so each step is budgeted by the
            // published entries; every step consumes at least one.
            let budget = (prod + st.rqst_len - st.rqst_cons) % st.rqst_len;
            if budget == 0 {
                return st;
            }
            // A held completion stops the ring until the driver frees room.
            if !st.pending.is_empty() {
                return st;
            }
            let addr = st.rqst_base + st.rqst_cons as u64 * ENTRY_SIZE as u64;
            let entry_type = self.ld8(addr);
            let mut entry_count = self.ld8(addr + 1);
            if entry_count == 0 {
                entry_count = 1;
            }
            entry_count = entry_count.min(budget.min(255) as u8);
            match entry_type {
                ENTRY_TYPE_REQUEST | ENTRY_TYPE_COMMAND_A64 => {
                    let a64 = entry_type == ENTRY_TYPE_COMMAND_A64;
                    match self.prepare_request(&mut st, addr, entry_count, a64) {
                        Prepared::Refused => return st,
                        Prepared::Answered => {}
                        Prepared::Run(job) => {
                            let epoch = st.ctl.epoch;
                            st.ctl.busy = true;
                            drop(st);
                            let done = self.run_job(job);
                            st = self.st.lock().unwrap();
                            st.ctl.busy = false;
                            self.wake.notify_all();
                            while st.ctl.settling != 0 {
                                st = self.wake.wait(st).unwrap();
                            }
                            // A reset or a new queue took the ring away.
                            if st.ctl.epoch != epoch {
                                continue;
                            }
                            self.post_status(&mut st, done);
                        }
                    }
                }
                // Continuations are stepped past here; reading segments
                // visits them.
                ENTRY_TYPE_MARKER => {}
                // Answered incomplete rather than consumed silently.
                t => {
                    self.note(format!("unexpected request entry type {t:#04x} at index {}", st.rqst_cons));
                    let handle = self.ld32(addr + REQ_OFF_HANDLE);
                    self.post_status(&mut st, Self::incomplete(handle, 0, 0));
                }
            }
            st.rqst_cons = (st.rqst_cons + entry_count as u16) % st.rqst_len;
            drop(st);
            self.sync_irq();
            st = self.st.lock().unwrap();
        }
    }

    // ---- registers ---------------------------------------------------

    fn reg_read(&self, off: u32) -> u16 {
        let mut st = self.st.lock().unwrap();
        // The adapter's own producer count, never the driver's last write.
        if off == reg::RESPONSE_QUEUE {
            return if st.resp_len == 0 { st.mailbox[MBOX_RSP] } else { st.resp_prod };
        }
        // The adapter's consumer position: a driver computes its free ring
        // space from it, so echoing its own write breaks that.
        if off == reg::REQUEST_QUEUE {
            return if st.rqst_len == 0 { st.mailbox[MBOX_REQ] } else { st.rqst_cons };
        }
        if (reg::MAILBOX_BASE..MAILBOX_END).contains(&off) {
            return st.mailbox[((off - reg::MAILBOX_BASE) / 2) as usize];
        }
        if (reg::BANK_BASE..BANK_END).contains(&off) {
            return st.active_bank()[((off - reg::BANK_BASE) / 2) as usize];
        }
        match off {
            reg::ID_LOW => PCI_VENDOR_QLOGIC,
            reg::ID_HIGH => PCI_DEVICE_ISP1040,
            // The revision field runs one ahead of the PCI byte.
            reg::BUS_CFG0 => PCI_REVISION_ISP1040 as u16 + 1,
            reg::BUS_CFG1 => st.bus_cfg1,
            reg::BUS_CTRL => st.bus_ctrl,
            reg::BUS_STATUS => st.bus_status,
            reg::SEMAPHORE => st.semaphore,
            reg::NVRAM => st.nvram,
            reg::DMA_CMD_CFG => st.dma_cmd_cfg,
            reg::DMA_DATA_CFG => st.dma_data_cfg,
            reg::HOST_CMD => st.risc_flags,
            _ => {
                drop(st);
                self.note(format!("read of unimplemented register {off:#04x}"));
                0
            }
        }
    }

    fn reg_write(&self, off: u32, val: u16) {
        let mut st = self.st.lock().unwrap();
        if off == reg::RESPONSE_QUEUE {
            // The consumer acknowledgement: recorded, never over the
            // adapter's own producer count.
            if st.resp_len == 0 {
                st.mailbox[MBOX_RSP] = val;
            } else {
                st.resp_cons = val % st.resp_len;
                self.drain_pending(&mut st);
                self.kick(&mut st);
            }
            return;
        }
        if (reg::MAILBOX_BASE..MAILBOX_END).contains(&off) {
            st.mailbox[((off - reg::MAILBOX_BASE) / 2) as usize] = val;
            // Writing the request-in pointer is itself the trigger.
            if off == reg::REQUEST_QUEUE {
                self.kick(&mut st);
            }
            return;
        }
        if (reg::BANK_BASE..BANK_END).contains(&off) {
            st.active_bank()[((off - reg::BANK_BASE) / 2) as usize] = val;
            return;
        }
        match off {
            reg::BUS_CFG1 => {
                if val & !CFG1_WRITE_MASK != 0 {
                    self.note(format!("bus configuration 1 bits {:#06x} are not modelled", val & !CFG1_WRITE_MASK));
                }
                st.bus_cfg1 = val & CFG1_WRITE_MASK;
            }
            // Applied after the enables are stored, so the reset wins.
            reg::BUS_CTRL => {
                if val & CTRL_SOFT_RESET != 0 {
                    st = self.settle(st);
                }
                st.bus_ctrl = val & CTRL_WRITE_MASK;
                if val & CTRL_SOFT_RESET != 0 {
                    st.reset();
                }
            }
            reg::SEMAPHORE => st.semaphore = val & SEMAPHORE_MASK,
            reg::NVRAM => st.nvram = val & NVRAM_WRITE_MASK,
            reg::DMA_CMD_CFG => st.dma_cmd_cfg = val & DMA_CFG_WRITE_MASK,
            reg::DMA_DATA_CFG => st.dma_data_cfg = val & DMA_CFG_WRITE_MASK,
            // Nothing to quiesce: a command's transfers are its own.
            reg::DMA_CMD_CTRL | reg::DMA_DATA_CTRL => {}
            reg::HOST_CMD => {
                // A new queue must not receive the completion of a command
                // read from the old one.
                let init = matches!(
                    st.mailbox[0],
                    mbox::INIT_REQ_QUEUE | mbox::INIT_REQ_QUEUE_A64 | mbox::INIT_RSP_QUEUE | mbox::INIT_RSP_QUEUE_A64
                );
                if val >> HOST_CMD_SHIFT == 0x5 && init {
                    st = self.settle(st);
                }
                self.host_command(&mut st, val)
            }
            reg::ID_LOW | reg::ID_HIGH | reg::BUS_CFG0 | reg::BUS_STATUS => {
                drop(st);
                self.note(format!("write {val:#06x} to read-only register {off:#04x}"));
            }
            _ => {
                drop(st);
                self.note(format!("write {val:#06x} to unimplemented register {off:#04x}"));
            }
        }
    }

    /// A register access. Only sixteen-bit accesses are decoded.
    pub fn read(&self, off: u32, width: usize) -> u32 {
        if width != 2 || off & 1 != 0 {
            self.odd_access("read", off, width);
            return 0;
        }
        self.reg_read(off) as u32
    }

    pub fn write(&self, off: u32, val: u32, width: usize) {
        if width != 2 || off & 1 != 0 {
            self.odd_access("write", off, width);
            return;
        }
        self.reg_write(off, val as u16);
        self.sync_irq();
    }

    fn odd_access(&self, what: &str, off: u32, width: usize) {
        let first = {
            let mut n = self.odd_accesses.lock().unwrap();
            *n += 1;
            *n <= 16
        };
        if first {
            self.note(format!("{width}-byte {what} at {off:#04x} is not decoded"));
        }
    }
}

/// The monitor's `scsi`/`cow` subcommands and the run-time disc changes come
/// from `scsi_monitor` through this.
/// The monitor's `scsi`/`cow` commands (`scsi1`/`cow1` for the external
/// bus), as the other SCSI controllers have them, plus `scsi notes`.
impl crate::traits::Device for Isp1040 {
    fn step(&self, _cycles: u64) {}
    fn stop(&self) {}
    fn start(&self) {}
    fn is_running(&self) -> bool { true }
    fn get_clock(&self) -> u64 { 0 }

    fn register_commands(&self) -> Vec<(String, String)> {
        let usage = crate::scsi_monitor::SCSI_USAGE;
        if self.index == 0 {
            vec![
                ("scsi".to_string(), format!("SCSI (QLogic ISP1040, controller 0): {usage} | scsi notes")),
                ("scsi0".to_string(), "Alias for `scsi` (controller 0)".to_string()),
                ("cow".to_string(), "COW overlays on controller 0: cow status | cow commit [id] | cow reset [id]".to_string()),
            ]
        } else {
            vec![
                (format!("scsi{}", self.index), format!("SCSI controller {} (external): same subcommands as `scsi`", self.index)),
                (format!("cow{}", self.index), format!("COW overlays on controller {}: same subcommands as `cow`", self.index)),
            ]
        }
    }

    fn execute_command(&self, cmd: &str, args: &[&str], mut w: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        use std::io::Write as _;
        if cmd.starts_with("scsi") {
            if let Some(done) = crate::scsi_monitor::scsi_subcommand(self, args, &mut *w) {
                return done;
            }
            if args.first().copied() == Some("notes") {
                let _ = writeln!(w, "QLogic ISP1040, controller {}: {} commands", self.index, self.total_commands());
                for n in self.notes() {
                    let _ = writeln!(w, "  note: {n}");
                }
                return Ok(());
            }
            return Err(format!("Usage: {} | scsi notes", crate::scsi_monitor::SCSI_USAGE));
        }
        if cmd.starts_with("cow") {
            return crate::scsi_monitor::cow_command(self, args, &mut *w);
        }
        Err("Command not found".to_string())
    }
}

impl crate::scsi_monitor::ScsiTargets for Isp1040 {
    fn with_targets<R>(&self, f: impl FnOnce(&mut [Option<ScsiDevice>]) -> R) -> R {
        let mut targets = self.targets.lock().unwrap();
        f(&mut targets[..])
    }
}

/// Both BARs reach the same registers.
impl PciBusDevice for Isp1040 {
    fn config(&self) -> &PciConfig {
        &self.pci
    }

    fn bar_read(&self, _bar: usize, off: u32, width: usize) -> u32 {
        self.read(off, width)
    }

    fn bar_write(&self, _bar: usize, off: u32, val: u32, width: usize) {
        self.write(off, val, width)
    }

    fn connect_interrupt(&self, line: IrqLine) {
        self.connect_irq(line);
    }

    fn connect_dma(&self, dma: Arc<dyn PciDma>) {
        let _ = self.dma.set(dma);
    }
}

impl crate::traits::Saveable for Isp1040 {
    fn save_state(&self) -> toml::Value {
        self.quiesced(|st| Self::save_locked(st))
    }

    fn load_state(&self, v: &toml::Value) -> Result<(), String> {
        self.quiesced(|st| Self::load_locked(st, v));
        self.sync_irq();
        Ok(())
    }
}

impl Isp1040 {
    fn save_locked(st: &State) -> toml::Value {
        let mut t = toml::map::Map::new();
        for (k, v) in [
            ("bus_cfg1", st.bus_cfg1), ("bus_ctrl", st.bus_ctrl), ("bus_status", st.bus_status),
            ("semaphore", st.semaphore), ("nvram", st.nvram), ("risc_flags", st.risc_flags),
            ("dma_cmd_cfg", st.dma_cmd_cfg), ("dma_data_cfg", st.dma_data_cfg),
            ("rqst_len", st.rqst_len), ("resp_len", st.resp_len), ("rqst_cons", st.rqst_cons),
            ("resp_prod", st.resp_prod), ("resp_cons", st.resp_cons), ("reset_delay", st.reset_delay),
        ] {
            t.insert(k.into(), hex_u16(v));
        }
        t.insert("rqst_base".into(), hex_u32(st.rqst_base as u32));
        t.insert("rqst_base_hi".into(), hex_u32((st.rqst_base >> 32) as u32));
        t.insert("resp_base".into(), hex_u32(st.resp_base as u32));
        t.insert("resp_base_hi".into(), hex_u32((st.resp_base >> 32) as u32));
        t.insert("mailbox".into(), u16_slice_to_toml(&st.mailbox));
        t.insert("risc_bank".into(), u16_slice_to_toml(&st.risc_bank));
        t.insert("sxp_bank".into(), u16_slice_to_toml(&st.sxp_bank));
        // The downloaded microcode is large and usually all zero between a
        // reset and the driver's download.
        if st.risc_ram.iter().any(|&w| w != 0) {
            t.insert("risc_ram".into(), u16_slice_to_toml(&st.risc_ram));
        }
        let pending = st.pending.iter().map(|p| {
            let mut m = toml::map::Map::new();
            m.insert("handle".into(), hex_u32(p.handle));
            m.insert("scsi_status".into(), hex_u16(p.scsi_status));
            m.insert("completion".into(), hex_u16(p.completion));
            m.insert("state_flags".into(), hex_u16(p.state_flags));
            m.insert("status_flags".into(), hex_u16(p.status_flags));
            m.insert("resid".into(), hex_u32(p.resid));
            m.insert("sense".into(), u8_slice_to_toml(&p.sense));
            toml::Value::Table(m)
        });
        t.insert("pending".into(), toml::Value::Array(pending.collect()));
        toml::Value::Table(t)
    }

    /// Unprocessed request entries survive: the ring thread is kicked and
    /// runs them after the load.
    fn load_locked(st: &mut State, v: &toml::Value) {
        {
            let ctl = std::mem::take(&mut st.ctl);
            *st = State::new();
            st.ctl = ctl;
            st.ctl.epoch += 1;
            st.ctl.work = true;
            let u16f = |k: &str| get_field(v, k).and_then(toml_u16).unwrap_or(0);
            let u32f = |k: &str| get_field(v, k).and_then(toml_u32).unwrap_or(0);
            st.bus_cfg1 = u16f("bus_cfg1");
            st.bus_ctrl = u16f("bus_ctrl");
            st.bus_status = u16f("bus_status");
            st.semaphore = u16f("semaphore");
            st.nvram = u16f("nvram");
            st.risc_flags = u16f("risc_flags");
            st.dma_cmd_cfg = u16f("dma_cmd_cfg");
            st.dma_data_cfg = u16f("dma_data_cfg");
            st.rqst_len = u16f("rqst_len");
            st.resp_len = u16f("resp_len");
            st.rqst_cons = u16f("rqst_cons");
            st.resp_prod = u16f("resp_prod");
            st.resp_cons = u16f("resp_cons");
            st.reset_delay = u16f("reset_delay");
            st.rqst_base = u32f("rqst_base") as u64 | ((u32f("rqst_base_hi") as u64) << 32);
            st.resp_base = u32f("resp_base") as u64 | ((u32f("resp_base_hi") as u64) << 32);
            if let Some(m) = get_field(v, "mailbox") { load_u16_slice(m, &mut st.mailbox); }
            if let Some(m) = get_field(v, "risc_bank") { load_u16_slice(m, &mut st.risc_bank); }
            if let Some(m) = get_field(v, "sxp_bank") { load_u16_slice(m, &mut st.sxp_bank); }
            if let Some(m) = get_field(v, "risc_ram") { load_u16_slice(m, &mut st.risc_ram); }
            if let Some(toml::Value::Array(a)) = get_field(v, "pending") {
                for p in a {
                    let sense = match get_field(p, "sense") {
                        Some(toml::Value::Array(s)) => s.iter().take(SENSE_MAX).map(|x| toml_u8(x).unwrap_or(0)).collect(),
                        _ => Vec::new(),
                    };
                    st.pending.push_back(Pending {
                        handle: get_field(p, "handle").and_then(toml_u32).unwrap_or(0),
                        scsi_status: get_field(p, "scsi_status").and_then(toml_u16).unwrap_or(0),
                        completion: get_field(p, "completion").and_then(toml_u16).unwrap_or(0),
                        state_flags: get_field(p, "state_flags").and_then(toml_u16).unwrap_or(0),
                        status_flags: get_field(p, "status_flags").and_then(toml_u16).unwrap_or(0),
                        resid: get_field(p, "resid").and_then(toml_u32).unwrap_or(0),
                        sense,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
