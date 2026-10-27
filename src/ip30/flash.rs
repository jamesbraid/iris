// Derived from MAME's intelfsh.cpp (the 16-bit Intel/Sharp command path),
// BSD-3-Clause, copyright Aaron Giles.

//! The Octane's boot flash: a 16-bit, big-endian part with the Intel/Sharp
//! command set, for [`Bridge::attach_flash`].
//!
//! The fitted chip is not identified. The model follows the Origami contract's
//! compatibility part: a 1 MiB array of sixteen 64 KiB erase blocks, manufacturer
//! `0x00b0` (Sharp) and device `0x66a8` as 16-bit identifier words, and the
//! command subset the stock PROM drives (identify, read status, clear status,
//! word program, block erase). The state machine is a port of the Intel/Sharp
//! path of MAME's `intelfsh`; `0x66a8` is an added part.
//!
//! Approximations, each deliberate:
//! - Program and erase finish instantly: the status register is ready the moment
//!   the data or confirming cycle lands.
//! - Programming overwrites the word, as MAME does; real NOR can only clear bits.
//! - The enhanced command set (page buffers, queued operations, extended and
//!   per-block status, block locking, suspend) is absent. Such a command is
//!   logged and ignored, so a guest finds it unsupported rather than
//!   silently successful.
//! - Status reads repeat the status byte on both data lanes.
//!
//! Persistence: [`CfiFlash::with_file`] backs the array with a file. An existing
//! file of the right size replaces the seed image; otherwise the file is created
//! from it. Completed program and erase cycles are written through to the file.
//!
//! [`Bridge::attach_flash`]: crate::ip30::bridge::Bridge::attach_flash

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

use crate::traits::{BusDevice, BusRead16, BUS_ERR, BUS_OK};

/// Erase block size.
pub const BLOCK_SIZE: usize = 64 * 1024;
/// Identifier words, read at part byte offsets 0 and 2 in identifier mode.
pub const MANUFACTURER_ID: u16 = 0x00b0;
pub const DEVICE_ID: u16 = 0x66a8;

// Command bytes, taken from DQ7:0 of a write cycle.
const CMD_PROGRAM: u8 = 0x40;
const CMD_PROGRAM_ALT: u8 = 0x10;
const CMD_ERASE: u8 = 0x20;
const CMD_CONFIRM: u8 = 0xd0;
const CMD_READ_STATUS: u8 = 0x70;
const CMD_CLEAR_STATUS: u8 = 0x50;
const CMD_READ_ID: u8 = 0x90;
const CMD_READ_ARRAY: u8 = 0xff;

// Status register bits.
const SR_READY: u8 = 0x80;
const SR_ERASE_ERR: u8 = 0x20;
const SR_PROGRAM_ERR: u8 = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Array,
    Id,
    Status,
    /// The next write cycle is the data word to program.
    ProgramData,
    /// An erase setup was seen; the next write must be the confirm.
    EraseConfirm,
}

struct State {
    data: Vec<u8>,
    mode: Mode,
    status: u8,
    file: Option<File>,
}

pub struct CfiFlash {
    st: Mutex<State>,
}

impl CfiFlash {
    /// A part of `size` bytes (a multiple of the block size, a power of two)
    /// holding `image` from offset 0 and erased (0xff) above it.
    pub fn new(image: &[u8], size: usize) -> Self {
        assert!(size.is_power_of_two() && size >= BLOCK_SIZE, "flash size must be a power of two of at least one block");
        assert!(image.len() <= size, "image must fit the part");
        let mut data = vec![0xff; size];
        data[..image.len()].copy_from_slice(image);
        Self { st: Mutex::new(State { data, mode: Mode::Array, status: SR_READY, file: None }) }
    }

    /// As [`CfiFlash::new`], backed by `path`. A file of exactly `size` bytes is
    /// the array's contents (a previous run's programming survives, and the
    /// seed image is ignored). Anything else is replaced by the seeded array.
    pub fn with_file(image: &[u8], size: usize, path: impl AsRef<Path>) -> io::Result<Self> {
        let flash = Self::new(image, size);
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        {
            let mut st = flash.st.lock().unwrap();
            if file.metadata()?.len() == size as u64 {
                file.read_exact_at(&mut st.data, 0)?;
            } else {
                file.set_len(0)?;
                file.write_all_at(&st.data, 0)?;
            }
            st.file = Some(file);
        }
        Ok(flash)
    }

    pub fn size(&self) -> usize {
        self.st.lock().unwrap().data.len()
    }

    /// A copy of the array.
    pub fn contents(&self) -> Vec<u8> {
        self.st.lock().unwrap().data.clone()
    }
}

impl State {
    fn sync(&self, off: usize, len: usize) {
        if let Some(f) = &self.file {
            if let Err(e) = f.write_all_at(&self.data[off..off + len], off as u64) {
                // The guest sees a completed cycle; the file is only the backing store.
                log::warn!("flash: backing file write at {off:#x} failed: {e}");
            }
        }
    }

    fn word(&self, off: usize) -> u16 {
        u16::from_be_bytes([self.data[off], self.data[off + 1]])
    }

    fn read(&self, off: usize) -> u16 {
        match self.mode {
            Mode::Array => self.word(off),
            Mode::Id => match off {
                0 => MANUFACTURER_ID,
                2 => DEVICE_ID,
                _ => 0,
            },
            // Any read in status mode returns the status register, on both lanes.
            Mode::Status | Mode::ProgramData | Mode::EraseConfirm => u16::from_be_bytes([self.status; 2]),
        }
    }

    fn write(&mut self, off: usize, val: u16) {
        let cmd = val as u8;
        match self.mode {
            Mode::ProgramData => {
                self.data[off..off + 2].copy_from_slice(&val.to_be_bytes());
                self.sync(off, 2);
                self.mode = Mode::Status;
            }
            Mode::EraseConfirm => {
                if cmd == CMD_CONFIRM {
                    let base = off & !(BLOCK_SIZE - 1);
                    self.data[base..base + BLOCK_SIZE].fill(0xff);
                    self.sync(base, BLOCK_SIZE);
                } else {
                    // Erase setup not followed by the confirm: both error bits.
                    self.status |= SR_ERASE_ERR | SR_PROGRAM_ERR;
                }
                self.mode = Mode::Status;
            }
            _ => match cmd {
                CMD_READ_ARRAY => self.mode = Mode::Array,
                CMD_READ_ID => self.mode = Mode::Id,
                CMD_READ_STATUS => self.mode = Mode::Status,
                CMD_CLEAR_STATUS => {
                    self.status &= !(SR_ERASE_ERR | SR_PROGRAM_ERR | 0x08);
                    self.mode = Mode::Status;
                }
                CMD_PROGRAM | CMD_PROGRAM_ALT => self.mode = Mode::ProgramData,
                CMD_ERASE => self.mode = Mode::EraseConfirm,
                other => {
                    log::warn!("flash: unsupported command {other:#04x} at {off:#x} ignored");
                }
            },
        }
    }
}

impl BusDevice for CfiFlash {
    fn read16(&self, addr: u64) -> BusRead16 {
        let st = self.st.lock().unwrap();
        let off = addr as usize & !1;
        if off >= st.data.len() {
            return BusRead16::err();
        }
        BusRead16::ok(st.read(off))
    }

    fn write16(&self, addr: u64, val: u16) -> u32 {
        let mut st = self.st.lock().unwrap();
        let off = addr as usize & !1;
        if off >= st.data.len() {
            return BUS_ERR;
        }
        st.write(off, val);
        BUS_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: usize = 1 << 20;

    fn part() -> CfiFlash {
        CfiFlash::new(&[0x12, 0x34, 0x56, 0x78], SIZE)
    }

    fn rd(f: &CfiFlash, a: u64) -> u16 {
        let r = f.read16(a);
        assert!(r.is_ok());
        r.data
    }

    fn wr(f: &CfiFlash, a: u64, v: u16) {
        assert_eq!(f.write16(a, v), BUS_OK);
    }

    fn program(f: &CfiFlash, a: u64, v: u16) -> u16 {
        wr(f, a, 0x4040);
        wr(f, a, v);
        wr(f, a, 0x7070);
        rd(f, a)
    }

    fn erase(f: &CfiFlash, a: u64) -> u16 {
        wr(f, a, 0x2020);
        wr(f, a, 0xd0d0);
        wr(f, a, 0x7070);
        rd(f, a)
    }

    #[test]
    fn array_reads_and_erased_fill() {
        let f = part();
        assert_eq!(rd(&f, 0), 0x1234);
        assert_eq!(rd(&f, 2), 0x5678);
        assert_eq!(rd(&f, 4), 0xffff);
        assert_eq!(f.size(), SIZE);
        assert!(!f.read16(SIZE as u64).is_ok());
    }

    #[test]
    fn identifier_words_and_return_to_array() {
        let f = part();
        wr(&f, 0, 0x9090);
        assert_eq!(rd(&f, 0), 0x00b0);
        assert_eq!(rd(&f, 2), 0x66a8);
        wr(&f, 0, 0xffff);
        assert_eq!(rd(&f, 0), 0x1234);
    }

    #[test]
    fn program_sets_ready_and_stores() {
        let f = part();
        let sr = program(&f, 0x1000, 0xbeef);
        assert_eq!(sr & 0xff, 0x80);
        wr(&f, 0, 0xffff);
        assert_eq!(rd(&f, 0x1000), 0xbeef);
        // The data word may equal a command opcode, including the array command.
        assert_eq!(program(&f, 0x1002, 0xffff) & 0xff, 0x80);
        assert_eq!(program(&f, 0x1004, 0x2020) & 0xff, 0x80);
        wr(&f, 0, 0xffff);
        assert_eq!([rd(&f, 0x1002), rd(&f, 0x1004)], [0xffff, 0x2020]);
    }

    #[test]
    fn clear_status_drops_errors() {
        let f = part();
        wr(&f, 0x20000, 0x2020);
        wr(&f, 0x20000, 0x0000); // not a confirm
        wr(&f, 0x20000, 0x7070);
        assert_eq!(rd(&f, 0) & 0xff, 0xb0);
        wr(&f, 0, 0x5050);
        assert_eq!(rd(&f, 0) & 0xff, 0x80);
    }

    #[test]
    fn erase_one_block_only() {
        let f = part();
        program(&f, 0x10000, 0x0000);
        program(&f, 0x20000, 0x0000);
        program(&f, 0x2fffe, 0x0000);
        let sr = erase(&f, 0x20004);
        assert_eq!(sr & 0xff, 0x80);
        wr(&f, 0, 0xffff);
        assert_eq!(rd(&f, 0x10000), 0);
        assert_eq!(rd(&f, 0x20000), 0xffff);
        assert_eq!(rd(&f, 0x2fffe), 0xffff);
        // Reprogram after the erase works.
        assert_eq!(program(&f, 0x20000, 0xa5a5) & 0xff, 0x80);
    }

    #[test]
    fn erase_without_confirm_is_a_command_error() {
        let f = part();
        wr(&f, 0x20000, 0x2020);
        wr(&f, 0x20000, 0x1234);
        wr(&f, 0x20000, 0x7070);
        assert_eq!(rd(&f, 0) & 0xff, 0xb0); // ready, erase and program error
        wr(&f, 0, 0x5050);
        wr(&f, 0, 0x7070);
        assert_eq!(rd(&f, 0) & 0xff, 0x80);
    }

    #[test]
    fn status_is_returned_for_any_address() {
        let f = part();
        wr(&f, 0, 0x7070);
        assert_eq!(rd(&f, 0), 0x8080);
        assert_eq!(rd(&f, 0x8000), 0x8080);
    }

    #[test]
    fn unsupported_command_is_ignored() {
        let f = part();
        wr(&f, 0, 0x7070);
        wr(&f, 0, 0x7171);
        assert_eq!(rd(&f, 0), 0x8080); // still in status mode
    }

    #[test]
    fn file_backing_seeds_then_reloads() {
        let path = std::env::temp_dir().join(format!("iris-flash-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let f = CfiFlash::with_file(&[0xaa, 0xbb], SIZE, &path).unwrap();
            assert_eq!(rd(&f, 0), 0xaabb);
            assert_eq!(program(&f, 0x40, 0x1357) & 0xff, 0x80);
            assert_eq!(erase(&f, 0xf0000) & 0xff, 0x80);
            program(&f, 0xf0002, 0x2468);
        }
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk.len(), SIZE);
        assert_eq!(&on_disk[..2], &[0xaa, 0xbb]);
        assert_eq!(&on_disk[0x40..0x42], &[0x13, 0x57]);
        assert_eq!(&on_disk[0xf0002..0xf0004], &[0x24, 0x68]);
        // A reload takes the file, not the seed image.
        let g = CfiFlash::with_file(&[0x00, 0x00], SIZE, &path).unwrap();
        assert_eq!(rd(&g, 0), 0xaabb);
        assert_eq!(rd(&g, 0x40), 0x1357);
        assert_eq!(rd(&g, 0xf0002), 0x2468);
        // A file of the wrong size is replaced by the seed.
        drop(g);
        std::fs::write(&path, [1, 2, 3]).unwrap();
        let h = CfiFlash::with_file(&[0xcc, 0xdd], SIZE, &path).unwrap();
        assert_eq!(rd(&h, 0), 0xccdd);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), SIZE as u64);
        std::fs::remove_file(&path).unwrap();
    }
}
