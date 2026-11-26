// SPDX-License-Identifier: BSD-3-Clause
// Ported from rust/sgi-odyssey/src/dev/odyssey/tests/ported.rs in jamesbraid/qemu-sgi-origami, branch gfx-engines.
//! The drawing, scan-out and register-state cases of QEMU's old Odyssey
//! qtest, replayed against the engine's board.
//!
//! Each script in `src/dev/odyssey/testdata/qtest` is what one qtest case did to the old
//! device, recorded from a run of that qtest against it: the register and
//! command-FIFO stores, guest memory it prepared, the pixel it wrote and the
//! pixels, registers and memory it read. A line `a OP N` after a read is an
//! assertion the case made on that read (`as` for the display's colour in the
//! plane form the case compared). The engine must satisfy every one of them.
//! A script is the case's own numbers, not a paraphrase of them.

use super::{board, Recorder};
use crate::dev::odyssey::board::{Board, BoardSnapshot};
use crate::dev::odyssey::view::{Kind, Rows};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::Arc;

/// Guest RAM as the old machine had it: 512 MiB from address zero, a page
/// existing once something has been written to it, zero before.
struct Ram(parking_lot::Mutex<HashMap<u64, Box<[u8; 4096]>>>);

const RAM_SIZE: u64 = 512 << 20;

impl Ram {
    fn new() -> Arc<Ram> {
        Arc::new(Ram(Default::default()))
    }
    fn write(&self, addr: u64, bytes: &[u8]) -> bool {
        if addr + bytes.len() as u64 > RAM_SIZE {
            return false;
        }
        let mut m = self.0.lock();
        for (i, b) in bytes.iter().enumerate() {
            let a = addr + i as u64;
            m.entry(a >> 12).or_insert_with(|| Box::new([0; 4096]))[(a & 4095) as usize] = *b;
        }
        true
    }
    fn read(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        if addr + len as u64 > RAM_SIZE {
            return None;
        }
        let m = self.0.lock();
        Some(
            (0..len as u64)
                .map(|i| {
                    m.get(&((addr + i) >> 12))
                        .map_or(0, |p| p[((addr + i) & 4095) as usize])
                })
                .collect(),
        )
    }
}

impl crate::traits::BusDevice for Ram {
    fn read8(&self, addr: u64) -> crate::traits::BusRead8 {
        match self.read(addr, 1) {
            Some(b) => crate::traits::BusRead8::ok(b[0]),
            None => crate::traits::BusRead8::err(),
        }
    }
    fn read16(&self, addr: u64) -> crate::traits::BusRead16 {
        match self.read(addr, 2) {
            Some(b) => crate::traits::BusRead16 {
                status: crate::traits::BUS_OK,
                data: u16::from_be_bytes(b.try_into().unwrap()),
            },
            None => crate::traits::BusRead16::err(),
        }
    }
    fn read32(&self, addr: u64) -> crate::traits::BusRead32 {
        match self.read(addr, 4) {
            Some(b) => crate::traits::BusRead32 {
                status: crate::traits::BUS_OK,
                data: u32::from_be_bytes(b.try_into().unwrap()),
            },
            None => crate::traits::BusRead32::err(),
        }
    }
    fn write8(&self, addr: u64, val: u8) -> u32 {
        if self.write(addr, &[val]) {
            crate::traits::BUS_OK
        } else {
            crate::traits::BUS_ERR
        }
    }
    fn write16(&self, addr: u64, val: u16) -> u32 {
        if self.write(addr, &val.to_be_bytes()) {
            crate::traits::BUS_OK
        } else {
            crate::traits::BUS_ERR
        }
    }
    fn write32(&self, addr: u64, val: u32) -> u32 {
        if self.write(addr, &val.to_be_bytes()) {
            crate::traits::BUS_OK
        } else {
            crate::traits::BUS_ERR
        }
    }
    fn read64(&self, addr: u64) -> crate::traits::BusRead64 {
        match self.read(addr, 8) {
            Some(b) => crate::traits::BusRead64 {
                status: crate::traits::BUS_OK,
                data: u64::from_be_bytes(b.try_into().unwrap()),
            },
            None => crate::traits::BusRead64::err(),
        }
    }
    fn write64(&self, addr: u64, val: u64) -> u32 {
        if self.write(addr, &val.to_be_bytes()) {
            crate::traits::BUS_OK
        } else {
            crate::traits::BUS_ERR
        }
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn num(s: &str) -> u64 {
    u64::from_str_radix(s, 16).unwrap()
}

fn dec(s: &str) -> i32 {
    s.parse().unwrap()
}

fn script(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/dev/odyssey/testdata/qtest")
        .join(format!("{name}.gz"));
    let mut text = String::new();
    flate2::read::GzDecoder::new(
        std::fs::File::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .read_to_string(&mut text)
    .unwrap();
    text
}

/// What the last read returned, for the assertion lines that follow it.
enum Last {
    None,
    Value(u64),
    /// Pixels read three ways (`Rig::rows`, `Rig::shown_rows`): an assertion holds if any does.
    Pixels(Vec<[u64; 3]>),
    Mem(u64, usize),
}

fn compare(op: &str, got: u64, want: u64) -> bool {
    match op {
        "==" => got == want,
        "!=" => got != want,
        "<" => got < want,
        ">" => got > want,
        "<=" => got <= want,
        ">=" => got >= want,
        _ => panic!("operator {op}"),
    }
}

struct Rig {
    s: Box<Board>,
    ram: Arc<Ram>,
    _rec: Arc<Recorder>,
    /// The pixels the script stored and their words, for `select` and `place_for_read`.
    stored: BTreeMap<(u32, i32, i32), u32>,
    /// The last colour pixel mode, so `select` never carries a colour pixel into a byte plane.
    colour_mode: [u32; 4],
    /// The script started on an 8-byte-pixel screen (see `default_select`).
    eight_byte: bool,
    /// `place_for_read` set window 0's display mode.
    window_from_header: bool,
    /// Where `place_for_read` put each stored pixel, and the bytes it left there.
    placed: BTreeMap<(u32, i32, i32), (Rows, u64)>,
}

impl Rig {
    fn new(product: &str) -> Rig {
        let (mut s, rec) = board(product);
        let ram = Ram::new();
        s.guest = Some(ram.clone());
        let colour_mode = s.pixel_mode;
        let mut rig = Rig {
            s,
            ram,
            _rec: rec,
            stored: Default::default(),
            colour_mode,
            eight_byte: false,
            window_from_header: false,
            placed: Default::default(),
        };
        rig.driver_defaults();
        rig
    }

    /// The old device never applied the index write mask and drew with copy, so scripts send
    /// neither word; the engine resets to a closed mask and clear.
    fn driver_defaults(&mut self) {
        self.s.gl_write_mask |= crate::dev::odyssey::consts::write::INDEX_MASK;
        self.s.logic_op_word = 3;
    }

    /// The board's state and memory, as a migration stream carries them.
    fn save(&mut self) -> (BoardSnapshot, Vec<u64>) {
        self.s.gpu_sync();
        let snap = BoardSnapshot::capture(&self.s).expect("capture");
        let size = self.s.vram.size();
        (snap, self.s.vram_ptr().words_at(0, size).to_vec())
    }

    /// Replace this board's live state and memory with a saved one.
    fn load(&mut self, saved: (BoardSnapshot, Vec<u64>)) {
        self.s.gpu_sync();
        self.s.restore_snapshot(saved.0).expect("restore");
        self.s.vram_ptr().write_words_untracked(0, &saved.1);
        self.colour_mode = self.s.pixel_mode;
    }

    /// Colour buffer `b` under pixel mode `mode`, two ways. The old device
    /// kept one RGBA8 plane in every mode, so a case that names a pixel does
    /// not say whether it means the board's layout for the mode selected now
    /// or the RGBA8 layout it wrote. The board's layout is the first, the
    /// RGBA8 field of the test screen the second.
    fn rows(&self, mode: &[u32; 4], b: u32) -> [Rows; 2] {
        use crate::dev::odyssey::frame::pbj;
        use crate::dev::odyssey::view::DestViews;
        self.s.gpu_sync();
        let views = DestViews::of(mode, self.s.pbj(pbj::OVERLAY_BASE), self.s.pbj(pbj::STRIDE));
        [
            Rows::new(self.s.vram_ptr(), views.colour[(b != 0) as usize]),
            super::screen_field(&self.s, b),
        ]
    }

    /// The main window id of pixel (x, y), from its window-id byte.
    fn window(&self, x: i32, y: i32) -> usize {
        (super::aux_at(&self.s, 0, (y.max(0) * 1280 + x.max(0)) as u32)
            >> crate::dev::odyssey::consts::ODSY_DID_MAIN_SHIFT) as usize
    }

    /// Colour buffer `b` where a window with display mode word `dm` is scanned out.
    fn shown_rows(&self, dm: u32, b: u32) -> Rows {
        use crate::dev::odyssey::frame::{mode_fields, pbj};
        use crate::dev::odyssey::view::{View, MAX_DIM};
        use crate::dev::odyssey::vram::Surface;
        let surface = Surface::new(
            self.s.pbj(pbj::PIXEL_BASE),
            self.s.pbj(pbj::STRIDE),
            (self.s.pbj(pbj::DEPTH) & 7) as u8 + 1,
        );
        Rows::new(
            self.s.vram_ptr(),
            View::colour(&surface, mode_fields(dm)[(b != 0) as usize], MAX_DIM),
        )
    }

    /// A pixel three ways: the board's layout, the test screen's field, the field the display reads.
    fn views(&self, b: u32, x: i32, y: i32) -> [Rows; 3] {
        let [mode, plain] = self.rows(&self.s.pixel_mode, b);
        [
            mode,
            plain,
            self.shown_rows(self.s.regs().dispmode(self.window(x, y)), b),
        ]
    }

    fn pixel(&self, b: u32, x: i32, y: i32) -> [u64; 3] {
        self.views(b, x, y)
            .map(|r| r.load_word(x, y).unwrap_or(0) as u64)
    }

    /// A pixel the case stores goes into every view: whichever one a later
    /// read means, it finds the pixel.
    fn store(&mut self, b: u32, x: i32, y: i32, v: u32) {
        for rows in self.views(b, x, y) {
            rows.store_word(x, y, v);
        }
        self.stored.insert((b, x, y), v);
    }

    /// A pixel the case stored is still that pixel after the case selects
    /// another colour mode: carry its word from the layout it was in to the new one.
    fn select(&mut self) {
        let was = self.colour_mode;
        let now = self.s.pixel_mode;
        // The window-id buffer is a byte plane, not another layout of a colour pixel.
        if self.rows(&now, 0)[0].v.kind == Kind::Byte || now == was {
            return;
        }
        for &(b, x, y) in self.stored.keys() {
            let word = self.rows(&was, b)[0].load_word(x, y).unwrap_or(0);
            self.rows(&now, b)[0].store_word(x, y, word);
        }
        self.colour_mode = now;
    }

    /// Begin a script section on the board as `test_screen` left it.
    fn start(&mut self, section: &[&str]) {
        self.stored.clear();
        self.driver_defaults();
        let word = default_select(section);
        self.eight_byte = word == Some(0x2000_4950);
        if let Some(word) = word {
            self.s.pixel_mode[2] = word;
        }
        self.colour_mode = self.s.pixel_mode;
    }

    /// Store `value` at `addr` as the script says, then keep the replay's own state in step.
    fn write(&mut self, size: u32, addr: u64, value: u64) {
        let was = self.s.pixel_mode;
        let dm_was = self.s.regs().dispmode(0);
        let origin = self.s.win.origin;
        self.s.mmio_write(addr, value, size);
        // The read origin is its own register; the old device had one origin.
        if self.s.win.origin != origin {
            self.s.win.read_origin = self.s.win.origin;
        }
        let dm0 = self.s.regs().dispmode(0);
        if dm0 != dm_was {
            self.pair_pixel_mode(dm0);
        }
        if self.s.pixel_mode != was {
            self.select();
        }
    }

    /// A driver pairs the pixel mode (where drawing lands) with the display mode (where scan-out
    /// reads): take the pixel mode whose colour fields are the ones window 0's mode `dm` reads.
    fn pair_pixel_mode(&mut self, dm: u32) {
        if let Some(status) = paired_status(dm) {
            self.s.pixel_mode[2] = status;
        }
    }

    /// A board-to-host header names the window format and field offsets it reads
    /// (word 4): put the stored pixels there before the read runs.
    fn place_for_read(&mut self, hdr: &[u8]) {
        let w: Vec<u32> = (0..8)
            .map(|i| u32::from_be_bytes(hdr[4 * i..4 * i + 4].try_into().unwrap()))
            .collect();
        if w[7] & crate::dev::odyssey::consts::dma::TO_HOST == 0 {
            return;
        }
        let (class, code) = (w[4] & 7, (w[4] >> 3) & 0x1f);
        // The window read has the display mode word 4's class names.
        let window = match (class, code) {
            (0, 0 | 1) => Some(0x004b_5f10),
            // Class 1, 4:4:4:4 codes, then the 5:5:5:1 and 5:6:5 ones.
            (1, 6..=8 | 13) => Some(0x004b_59a6),
            (1, 1 | 3..=5 | 9 | 10 | 12) => Some(0x004b_57a6),
            _ => None,
        };
        if let Some(mode) =
            window.filter(|_| self.s.regs().dispmode(0) == 0x0096_55a6 || self.window_from_header)
        {
            self.s.regs().set_dispmode(0, mode);
            self.window_from_header = true;
            self.pair_pixel_mode(mode);
            self.select();
        }
        let (fields, _) = self.s.dma_fields(&w, true, false);
        // A pixel the board has since written is no longer the stored one.
        let gone: Vec<_> = self
            .placed
            .iter()
            .filter(|(&(_, x, y), (f, raw))| f.load_raw(x, y) != *raw)
            .map(|(k, _)| *k)
            .collect();
        for k in gone {
            self.stored.remove(&k);
            self.placed.remove(&k);
        }
        for (&(b, x, y), &v) in &self.stored {
            let f = fields[(b & 1) as usize];
            f.store_word(x, y, v);
            self.placed.insert((b, x, y), (f, f.load_raw(x, y)));
        }
    }

    fn shown(&mut self, x: usize, y: usize) -> u64 {
        self.s.scanout_update();
        let r = &self.s.reader;
        let w = r.frame_width as usize;
        // The old accessor gave 0x00RRGGBB; the reader composes that or, for a head, 0xAABBGGRR.
        let p = r.frame.get(y * w + x).copied().unwrap_or(0);
        if r.frame_abgr {
            ((p & 0xff) << 16 | (p & 0xff00) | (p >> 16) & 0xff) as u64
        } else {
            (p & 0xff_ffff) as u64
        }
    }
}

/// Corrections to a script, rows `case<TAB>line<TAB>op<TAB>value<TAB>evidence` in `src/dev/odyssey/testdata/qtest/overrides*.tsv`:
/// `skip` drops an assertion, `set` replaces the value a store sends, other ops replace an assertion.
fn overrides(name: &str) -> HashMap<usize, (String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/dev/odyssey/testdata/qtest");
    let mut out = HashMap::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let f = entry.file_name().to_string_lossy().into_owned();
        if !(f.starts_with("overrides") && f.ends_with(".tsv")) {
            continue;
        }
        for row in std::fs::read_to_string(entry.path()).unwrap().lines() {
            let c: Vec<&str> = row.split('\t').collect();
            if c.len() >= 5 && c[0] == name {
                out.insert(c[1].parse().unwrap(), (c[2].to_string(), c[3].to_string()));
            }
        }
    }
    out
}

/// The pixel mode status whose two colour fields are the ones display mode word `dm` reads.
fn paired_status(dm: u32) -> Option<u32> {
    use crate::dev::odyssey::field::Layout;
    use crate::dev::odyssey::frame::mode_fields;
    use crate::dev::odyssey::vram::Format;
    let want = mode_fields(dm);
    let index = !matches!((dm >> 9) & 0xf, 0x6 | 0x9 | 0xa | 0xb | 0xc);
    (0u32..8 * 8 * 8)
        .map(|n| {
            0x2000_0000
                | (index as u32) << 19
                | 3 << 13
                | (n >> 6) << 10
                | ((n >> 3) & 7) << 7
                | (n & 7) << 4
        })
        .find(|&status| {
            let l = Layout::of(Format::decode(status));
            l.front == want[0] && l.back == want[1] && Format::decode(status).win_log2 > 1
        })
}

/// Words the old qtest sent that real software does not and the old device ignored, as
/// same-length edits so assertion lines hold: id-plane copies, colour selects, display modes.
fn adapt(text: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let word = |l: &[String], i: usize| l[i].rsplit(' ').next().unwrap_or("").to_string();
    for l in lines
        .iter_mut()
        .filter(|l| *l == "w 8 400000 3a46000100000120")
    {
        *l = "w 8 400000 3a460001004b5f20".to_string();
    }
    for i in 0..lines.len().saturating_sub(9) {
        let set = |l: &mut Vec<String>, k: usize, w: &str| l[i + 1 + k] = format!("w 4 110000 {w}");
        if lines[i] == "w 4 110000 2031"
            && word(&lines, i + 1) == "142000"
            && word(&lines, i + 6) == "142000"
        {
            set(&mut lines, 2, "500050");
            set(&mut lines, 4, "80001000");
            set(&mut lines, 7, "3a000041");
        }
        if lines[i] == "w 4 110000 11017"
            && word(&lines, i + 1) == "142000"
            && !["20080080", "80080", "0"].contains(&word(&lines, i + 3).as_str())
        {
            set(&mut lines, 0, "2000");
            let rgb = lines[i + 5..]
                .iter()
                .map(|l| l.rsplit(' ').next().unwrap())
                .find(|w| matches!(*w, "c580c406" | "c580cc08"))
                == Some("c580cc08");
            if rgb && word(&lines, i + 3) == "20086a00" {
                set(&mut lines, 2, "406d50");
            }
        }
    }
    // The X root's display mode reads colour at bytes 4 and 0, the test screen draws at 12 and 4:
    // use the double-buffered window's word unless the case selects an X root pixel mode.
    if !lines
        .iter()
        .any(|l| l.ends_with(" 20006950") || l.ends_with(" 20006850"))
    {
        for i in 1..lines.len() {
            let hdr = lines[i - 1].rsplit(' ').next().unwrap();
            let id = u32::from_str_radix(hdr, 16).map_or(0, |h| (h >> 14) & 0x3fff);
            if lines[i - 1].starts_with("w 4 400000")
                && (0x2900..0x2920).contains(&id)
                && lines[i].ends_with(" 5215a6")
            {
                lines[i] = lines[i].replace("5215a6", "9655a6");
            }
        }
    }
    lines.join("\n") + "\n"
}

/// The colour select a section implies when it draws before selecting one: an 8-byte screen
/// for console copy words, the colour-index window for an index fill that selects it later.
fn default_select(section: &[&str]) -> Option<u32> {
    let first = section
        .iter()
        .position(|l| *l == "w 4 110000 11017")
        .unwrap_or(section.len());
    let has = |lines: &[&str], w: &str| lines.contains(&w);
    // Copy words before the case draws anything: the screen has not been chosen.
    let drawn = section
        .iter()
        .position(|l| l.starts_with("w 4 110000 c580"))
        .unwrap_or(section.len());
    if section[..first.min(drawn)].contains(&"w 4 110000 82223042") {
        Some(0x2000_4950)
    } else if has(&section[..first], "w 4 110000 c580c406") && has(section, "w 4 110000 20086a00") {
        Some(0x2008_6a00)
    } else {
        None
    }
}

/// The script lines from `n` (a `board` or `reset` line) to the next one.
fn section<'a>(all: &'a [&'a str], n: usize) -> &'a [&'a str] {
    let end = all[n + 1..]
        .iter()
        .position(|l| l.starts_with("board ") || *l == "reset")
        .map_or(all.len(), |e| n + 1 + e);
    &all[n + 1..end]
}

/// Where the destination loads the source's state, for cases whose destination
/// holds state of its own. The script does not record it; read off the old cases.
fn migrate_point(name: &str, lines: &[&str], incoming: usize) -> Option<usize> {
    match name {
        "query-record-migration"
        | "query-record-migration-defaults"
        | "back-material-migration-defaults" => lines.iter().rposition(|l| *l == "w 4 4004 0"),
        "command-fifo-gl-window-id-compare-migration"
        | "command-fifo-gl-window-tail-migration-absent" => {
            (incoming + 1..lines.len()).find(|&i| lines[i].starts_with("w 4 100000 "))
        }
        _ => None,
    }
}

/// Cases where window-id writes leave the overlay mode selected before an RGB draw, which
/// the GL library would precede with the colour window's select.
const COLOUR_WINDOW_CASES: [&str; 5] = [
    "command-fifo-gl-colour-index",
    "command-fifo-gl-window-id-compare",
    "command-fifo-gl-window-id-compare-migration",
    "command-fifo-gl-window-id-compare-restore",
    "command-fifo-gl-window-id-compare-triangle",
];

/// Replay a script and say how every assertion it carries came out.
pub(super) fn replay(name: &str) -> Vec<String> {
    let text = adapt(&script(name));
    let all: Vec<&str> = text.lines().collect();
    let mut saved: Option<(BoardSnapshot, Vec<u64>)> = None;
    let mut load_at = None;
    let ovr = overrides(name);
    let mut fails = Vec::new();
    let mut rig: Option<Rig> = None;
    let mut last = Last::None;
    for (n, line) in text.lines().enumerate() {
        let line = match ovr.get(&(n + 1)) {
            Some((op, _)) if op == "skip" => continue,
            // `set` replaces the value a store writes: `w SIZE ADDR VALUE`.
            Some((op, val)) if op == "set" && line.starts_with("w ") => {
                let t: Vec<&str> = line.split(' ').collect();
                format!("w {} {} {val}", t[1], t[2])
            }
            Some((op, val)) if line.starts_with("am ") => {
                let _ = op;
                format!("am {val}")
            }
            Some((op, val)) => {
                let t: Vec<&str> = line.split(' ').collect();
                format!(
                    "{} {op} {val}{}",
                    t[0],
                    t.get(3).map_or(String::new(), |j| format!(" {j}"))
                )
            }
            None => line.to_string(),
        };
        let t: Vec<&str> = line.split(' ').collect();
        if line == "w 4 100000 14400" && COLOUR_WINDOW_CASES.contains(&name) {
            if let Some(r) = rig
                .as_mut()
                .filter(|r| r.s.pixel_mode[2] & (1 << 19) != 0 && !r.s.gl_index_mode())
            {
                for w in [0x11017, 0x2000, 0x50, 0x0040_6d50, 0] {
                    r.write(4, 0x100000, w);
                }
            }
        }
        if load_at == Some(n) {
            if let (Some(r), Some(s)) = (rig.as_mut(), saved.take()) {
                r.load(s);
            }
        }
        let check = |op: &str, want: u64, got: u64, what: &str, fails: &mut Vec<String>| {
            if !compare(op, got, want) {
                fails.push(format!(
                    "line {}: {what}: engine {got:#x}, case needs {op} {want:#x}",
                    n + 1
                ));
            }
        };
        match t[0] {
            "board" => {
                if t.get(3) == Some(&"incoming") {
                    // A migration target: a fresh board that loads the source's state at
                    // the case's load point, else the replay carries on in the same board.
                    if let (Some(at), Some(r)) = (migrate_point(name, &all, n), rig.as_mut()) {
                        saved = Some(r.save());
                        let stored = std::mem::take(&mut r.stored);
                        let mut to = Rig::new(if t[1] == "-" { "v12" } else { t[1] });
                        to.stored = stored;
                        rig = Some(to);
                        load_at = Some(at);
                    }
                    continue;
                }
                let mut b = Rig::new(if t[1] == "-" { "v12" } else { t[1] });
                b.start(section(&all, n));
                rig = Some(b);
            }
            "quit" => {}
            _ => {
                let Some(r) = rig.as_mut() else { continue };
                match t[0] {
                    "w" => {
                        let mut value = num(t[3]);
                        // A copy that names 8-byte pixels on the 16-byte frame takes the frame's size code.
                        if !r.eight_byte && matches!(value, 0x8222_3042 | 0x3222_204b) {
                            value = if value == 0x8222_3042 {
                                0x8223_3042
                            } else {
                                0x3223_204b
                            };
                        }
                        r.write(dec(t[1]) as u32, num(t[2]), value);
                    }
                    "r" => last = Last::Value(r.s.mmio_read(num(t[2]), dec(t[1]) as u32)),
                    "xw" => {
                        let v = num(t[3]).to_be_bytes();
                        let sz = dec(t[1]) as usize;
                        r.ram.write(num(t[2]), &v[8 - sz..]);
                    }
                    "xr" => {
                        let sz = dec(t[1]) as usize;
                        last = Last::Value(
                            r.ram
                                .read(num(t[2]), sz)
                                .map_or(0, |b| b.iter().fold(0u64, |a, x| a << 8 | *x as u64)),
                        );
                    }
                    "mw" => {
                        let bytes = hex(t.get(2).copied().unwrap_or(""));
                        r.ram.write(num(t[1]), &bytes);
                        if num(t[1]) == 0xb80000 && bytes.len() >= 32 {
                            r.place_for_read(&bytes);
                        }
                    }
                    "ms" => {
                        r.ram.write(
                            num(t[1]),
                            &vec![u8::from_str_radix(t[3], 16).unwrap(); num(t[2]) as usize],
                        );
                    }
                    "mr" => last = Last::Mem(num(t[1]), dec(t[2]) as usize),
                    "am" => {
                        if let Last::Mem(a, l) = last {
                            let got = r.ram.read(a, l).unwrap_or_default();
                            if got != hex(t[1]) {
                                fails.push(format!(
                                    "line {}: guest memory at {a:#x}: engine {}, case needs {}",
                                    n + 1,
                                    got.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                                    t[1]
                                ));
                            }
                        }
                    }
                    "px" => {
                        last = Last::Pixels(vec![r.pixel(dec(t[1]) as u32, dec(t[2]), dec(t[3]))])
                    }
                    "pw" => {
                        r.store(dec(t[1]) as u32, dec(t[2]), dec(t[3]), num(t[4]) as u32);
                    }
                    "pr" => {
                        let (b, x, y, c) = (dec(t[1]) as u32, dec(t[2]), dec(t[3]), dec(t[4]));
                        last = Last::Pixels((0..c).map(|i| r.pixel(b, x + i, y)).collect());
                    }
                    "did" => {
                        last = Last::Value(super::aux_at(
                            &r.s,
                            0,
                            (dec(t[2]) * 1280 + dec(t[1])) as u32,
                        ) as u64)
                    }
                    "ov" => {
                        last = Last::Value(super::aux_at(
                            &r.s,
                            1,
                            (dec(t[2]) * 1280 + dec(t[1])) as u32,
                        ) as u64)
                    }
                    "didw" => super::set_aux(
                        &mut r.s,
                        0,
                        (dec(t[2]) * 1280 + dec(t[1])) as u32,
                        num(t[3]) as u8,
                    ),
                    "ovw" => super::set_aux(
                        &mut r.s,
                        1,
                        (dec(t[2]) * 1280 + dec(t[1])) as u32,
                        num(t[3]) as u8,
                    ),
                    "dp" => last = Last::Value(r.shown(dec(t[1]) as usize, dec(t[2]) as usize)),
                    "reset" => {
                        r.s.reset();
                        r.s.test_screen();
                        r.start(section(&all, n));
                    }
                    "retrace" => r.s.retrace(),
                    "step" => {
                        for _ in 0..(t[1].parse::<i64>().unwrap_or(0) / 16_666_667) {
                            r.s.retrace();
                        }
                    }
                    "a" => match &last {
                        Last::Value(v) => check(t[1], num(t[2]), *v, "read", &mut fails),
                        Last::Pixels(row) => {
                            let j: usize = t
                                .get(3)
                                .map_or(0, |j| j.trim_start_matches('@').parse().unwrap_or(0));
                            let [mode, plain, shown] = row.get(j).copied().unwrap_or([u64::MAX; 3]);
                            if !compare(t[1], plain, num(t[2])) && !compare(t[1], shown, num(t[2]))
                            {
                                check(t[1], num(t[2]), mode, "pixel", &mut fails);
                            }
                        }
                        _ => {}
                    },
                    "as" => {
                        if let Last::Value(v) = &last {
                            check(
                                t[1],
                                num(t[2]),
                                (*v << 8 | 0xff) & 0xffff_ffff,
                                "displayed colour",
                                &mut fails,
                            );
                        }
                    }
                    other => panic!("script line {}: {other}", n + 1),
                }
            }
        }
    }
    fails
}

macro_rules! ported {
    ($($(#[$m:meta])* $name:ident => $file:literal,)*) => {
        $(
            #[test]
            #[allow(non_snake_case, reason = "the old case's path, double underscore for a slash")]
            $(#[$m])*
            fn $name() {
                let fails = replay($file);
                assert!(fails.is_empty(), "{} assertion(s) of the old case {} fail:\n{}", fails.len(), $file, fails.iter().take(12).cloned().collect::<Vec<_>>().join("\n"));
            }
        )*
    };
}

// An ignored case's reason gives the verdict, the first assertion the engine fails and why.
// Run them with `--ignored`, one process each.
ported! {
    back_material_migration => "back-material-migration",
    back_material_migration_defaults => "back-material-migration-defaults",
    backend_colormap_word_stream => "backend-colormap-word-stream",
    backend_mode_reset => "backend-mode-reset",
    backend_tables_and_readback => "backend-tables-and-readback",
    backend_version_is_pinned => "backend-version-is-pinned",
    board_memory_bound_is_a_guest_error => "board-memory-bound-is-a-guest-error",
    buffers_captured_copy_preserves_both_sources => "buffers-captured-copy-preserves-both-sources",
    buffers_display_selection_per_window => "buffers-display-selection-per-window",
    buffers_draw_selection_keeps_clip_and_masks => "buffers-draw-selection-keeps-clip-and-masks",
    buffers_gl_triangle_follows_mask => "buffers-gl-triangle-follows-mask",
    buffers_hold_opposite_colours => "buffers-hold-opposite-colours",
    buffers_migration => "buffers-migration",
    buffers_read_and_draw_words_are_independent => "buffers-read-and-draw-words-are-independent",
    buffers_swap_shows_only_complete_frames => "buffers-swap-shows-only-complete-frames",
    #[ignore = "unknown: line 46: pixel: engine 0x11223300, case needs == 0x112233ff; a source alpha of 0 in an ABGR byte burst (format 0x4c): engine stores 0, old device forced 0xff, board unknown"]
    buffers_transfers_follow_mask => "buffers-transfers-follow-mask",
    buffers_unsupported_mask_reported_once => "buffers-unsupported-mask-reported-once",
    byte_colour__conversion => "byte-colour__conversion",
    byte_colour__drawable_id => "byte-colour__drawable-id",
    byte_colour__index_guard => "byte-colour__index-guard",
    byte_colour__override => "byte-colour__override",
    command_fifo_accepts_64bit_stores => "command-fifo-accepts-64bit-stores",
    command_fifo_compare_incomplete => "command-fifo-compare-incomplete",
    command_fifo_compare_neutral_kernel => "command-fifo-compare-neutral-kernel",
    command_fifo_compare_neutral_user => "command-fifo-compare-neutral-user",
    command_fifo_compare_nonzero_first => "command-fifo-compare-nonzero-first",
    command_fifo_compare_nonzero_first_user => "command-fifo-compare-nonzero-first-user",
    command_fifo_compare_nonzero_second => "command-fifo-compare-nonzero-second",
    command_fifo_compare_nonzero_second_user => "command-fifo-compare-nonzero-second-user",
    command_fifo_context_save_colour => "command-fifo-context-save-colour",
    command_fifo_context_save_idle_state => "command-fifo-context-save-idle-state",
    command_fifo_copy_id_plane => "command-fifo-copy-id-plane",
    command_fifo_copy_id_plane_clipping => "command-fifo-copy-id-plane-clipping",
    command_fifo_copy_id_plane_dispatch => "command-fifo-copy-id-plane-dispatch",
    command_fifo_copy_id_plane_mask_selection => "command-fifo-copy-id-plane-mask-selection",
    command_fifo_copy_id_plane_overlap => "command-fifo-copy-id-plane-overlap",
    command_fifo_copy_id_plane_plane_mask => "command-fifo-copy-id-plane-plane-mask",
    command_fifo_copy_id_plane_refreshes_scanout => "command-fifo-copy-id-plane-refreshes-scanout",
    command_fifo_copy_id_plane_select_base => "command-fifo-copy-id-plane-select-base",
    command_fifo_copyrect => "command-fifo-copyrect",
    command_fifo_copyrect_ignores_window_clip => "command-fifo-copyrect-ignores-window-clip",
    command_fifo_copyrect_large_offscreen => "command-fifo-copyrect-large-offscreen",
    command_fifo_copyrect_overlapping_shift => "command-fifo-copyrect-overlapping-shift",
    command_fifo_dest_select_flags => "command-fifo-dest-select-flags",
    command_fifo_dma_chunks => "command-fifo-dma-chunks",
    command_fifo_drawable_words => "command-fifo-drawable-words",
    command_fifo_fill_runs_match_pixel_rules => "command-fifo-fill-runs-match-pixel-rules",
    command_fifo_fillrect => "command-fifo-fillrect",
    command_fifo_gl_alpha_test => "command-fifo-gl-alpha-test",
    command_fifo_gl_bitmap_rect_depth_only => "command-fifo-gl-bitmap-rect-depth-only",
    command_fifo_gl_bitmap_rect_disabled => "command-fifo-gl-bitmap-rect-disabled",
    command_fifo_gl_bitmap_rect_index => "command-fifo-gl-bitmap-rect-index",
    command_fifo_gl_bitmap_rect_large_anchor => "command-fifo-gl-bitmap-rect-large-anchor",
    command_fifo_gl_bitmap_rect_origin_clip => "command-fifo-gl-bitmap-rect-origin-clip",
    command_fifo_gl_bitmap_rect_reverse_y => "command-fifo-gl-bitmap-rect-reverse-y",
    command_fifo_gl_bitmap_rect_unsupported => "command-fifo-gl-bitmap-rect-unsupported",
    command_fifo_gl_bitmap_rect_wrap_zero => "command-fifo-gl-bitmap-rect-wrap-zero",
    command_fifo_gl_bitmap_rect_zero_depth => "command-fifo-gl-bitmap-rect-zero-depth",
    command_fifo_gl_blend => "command-fifo-gl-blend",
    command_fifo_gl_clear_rect => "command-fifo-gl-clear-rect",
    command_fifo_gl_clear_then_triangle => "command-fifo-gl-clear-then-triangle",
    command_fifo_gl_clip_box => "command-fifo-gl-clip-box",
    command_fifo_gl_clip_plane => "command-fifo-gl-clip-plane",
    command_fifo_gl_clip_plane_eye_space => "command-fifo-gl-clip-plane-eye-space",
    command_fifo_gl_clip_plane_migration => "command-fifo-gl-clip-plane-migration",
    command_fifo_gl_clip_rectangles => "command-fifo-gl-clip-rectangles",
    command_fifo_gl_colour_index => "command-fifo-gl-colour-index",
    command_fifo_gl_colour_index_write_mask => "command-fifo-gl-colour-index-write-mask",
    command_fifo_gl_colour_mask => "command-fifo-gl-colour-mask",
    command_fifo_gl_colour_material => "command-fifo-gl-colour-material",
    command_fifo_gl_combined_carries_modelview => "command-fifo-gl-combined-carries-modelview",
    command_fifo_gl_crossing_eye => "command-fifo-gl-crossing-eye",
    command_fifo_gl_cull_face => "command-fifo-gl-cull-face",
    #[ignore = "unknown: line 76: pixel: engine 0xffff, case needs == 0xff0000ff; clear depth 2^34 is above the range: engine clamps, old device saturated, board conversion unknown"]
    command_fifo_gl_depth_clear_saturates => "command-fifo-gl-depth-clear-saturates",
    command_fifo_gl_depth_eye_linear => "command-fifo-gl-depth-eye-linear",
    command_fifo_gl_depth_float => "command-fifo-gl-depth-float",
    command_fifo_gl_depth_model_view_perspective => "command-fifo-gl-depth-model-view-perspective",
    command_fifo_gl_depth_only_clear => "command-fifo-gl-depth-only-clear",
    command_fifo_gl_depth_scale => "command-fifo-gl-depth-scale",
    command_fifo_gl_depth_scale_fallback => "command-fifo-gl-depth-scale-fallback",
    command_fifo_gl_depth_sign => "command-fifo-gl-depth-sign",
    command_fifo_gl_depth_test => "command-fifo-gl-depth-test",
    command_fifo_gl_dither => "command-fifo-gl-dither",
    command_fifo_gl_empty_primitive => "command-fifo-gl-empty-primitive",
    command_fifo_gl_flat_shading => "command-fifo-gl-flat-shading",
    command_fifo_gl_float_colour_integer_vertices => "command-fifo-gl-float-colour-integer-vertices",
    command_fifo_gl_fog_curve => "command-fifo-gl-fog-curve",
    command_fifo_gl_fog_migration => "command-fifo-gl-fog-migration",
    command_fifo_gl_index_does_not_blend => "command-fifo-gl-index-does-not-blend",
    command_fifo_gl_integer_assemblies => "command-fifo-gl-integer-assemblies",
    command_fifo_gl_integer_colour => "command-fifo-gl-integer-colour",
    command_fifo_gl_integer_lines_transform => "command-fifo-gl-integer-lines-transform",
    command_fifo_gl_integer_normals => "command-fifo-gl-integer-normals",
    command_fifo_gl_integer_points_transform => "command-fifo-gl-integer-points-transform",
    command_fifo_gl_integer_polygon_transform => "command-fifo-gl-integer-polygon-transform",
    command_fifo_gl_integer_quads_transform => "command-fifo-gl-integer-quads-transform",
    command_fifo_gl_integer_routing_modes => "command-fifo-gl-integer-routing-modes",
    command_fifo_gl_integer_triangle => "command-fifo-gl-integer-triangle",
    command_fifo_gl_line_stipple => "command-fifo-gl-line-stipple",
    command_fifo_gl_line_stipple_migration => "command-fifo-gl-line-stipple-migration",
    command_fifo_gl_never_set_index => "command-fifo-gl-never-set-index",
    command_fifo_gl_opaque_stipple_index => "command-fifo-gl-opaque-stipple-index",
    command_fifo_gl_ortho_combined_matrix => "command-fifo-gl-ortho-combined-matrix",
    command_fifo_gl_point_smoothing => "command-fifo-gl-point-smoothing",
    command_fifo_gl_polygon_mode => "command-fifo-gl-polygon-mode",
    command_fifo_gl_polygon_offset => "command-fifo-gl-polygon-offset",
    command_fifo_gl_polygon_offset_word_steps => "command-fifo-gl-polygon-offset-word-steps",
    command_fifo_gl_program_list_lit => "command-fifo-gl-program-list-lit",
    command_fifo_gl_program_list_lit_local_viewer => "command-fifo-gl-program-list-lit-local-viewer",
    command_fifo_gl_program_list_lit_positional => "command-fifo-gl-program-list-lit-positional",
    command_fifo_gl_program_list_unlit => "command-fifo-gl-program-list-unlit",
    command_fifo_gl_program_older_numbering => "command-fifo-gl-program-older-numbering",
    command_fifo_gl_program_record_clip_head => "command-fifo-gl-program-record-clip-head",
    command_fifo_gl_program_record_lit => "command-fifo-gl-program-record-lit",
    command_fifo_gl_program_record_unlit => "command-fifo-gl-program-record-unlit",
    command_fifo_gl_program_records_lit => "command-fifo-gl-program-records-lit",
    command_fifo_gl_program_texgen => "command-fifo-gl-program-texgen",
    command_fifo_gl_program_texture_finish => "command-fifo-gl-program-texture-finish",
    command_fifo_gl_program_two_sided => "command-fifo-gl-program-two-sided",
    command_fifo_gl_program_unknown => "command-fifo-gl-program-unknown",
    command_fifo_gl_program_window_local_viewer => "command-fifo-gl-program-window-local-viewer",
    command_fifo_gl_program_window_one_address => "command-fifo-gl-program-window-one-address",
    command_fifo_gl_program_window_record => "command-fifo-gl-program-window-record",
    command_fifo_gl_program_window_unknown => "command-fifo-gl-program-window-unknown",
    command_fifo_gl_raster_rules => "command-fifo-gl-raster-rules",
    command_fifo_gl_stencil_masks_draws => "command-fifo-gl-stencil-masks-draws",
    command_fifo_gl_texture_slot_leaves_geometry => "command-fifo-gl-texture-slot-leaves-geometry",
    #[ignore = "unknown: line 146: pixel: engine 0xff, case needs == 0xffffffff; texture with a header and no texels: engine samples empty memory, old device drew the current colour, hardware undefined"]
    command_fifo_gl_texture_state => "command-fifo-gl-texture-state",
    command_fifo_gl_triangle => "command-fifo-gl-triangle",
    command_fifo_gl_vertex_2i_lit => "command-fifo-gl-vertex-2i-lit",
    command_fifo_gl_vertex_3i => "command-fifo-gl-vertex-3i",
    command_fifo_gl_vertex_3i_lit => "command-fifo-gl-vertex-3i-lit",
    command_fifo_gl_vertex_4f => "command-fifo-gl-vertex-4f",
    command_fifo_gl_window_id_compare => "command-fifo-gl-window-id-compare",
    command_fifo_gl_window_id_compare_migration => "command-fifo-gl-window-id-compare-migration",
    command_fifo_gl_window_id_compare_restore => "command-fifo-gl-window-id-compare-restore",
    command_fifo_gl_window_id_compare_triangle => "command-fifo-gl-window-id-compare-triangle",
    command_fifo_gl_window_offscreen_tail => "command-fifo-gl-window-offscreen-tail",
    command_fifo_gl_window_origin_and_clip => "command-fifo-gl-window-origin-and-clip",
    command_fifo_gl_window_restore => "command-fifo-gl-window-restore",
    command_fifo_gl_window_tail_migration => "command-fifo-gl-window-tail-migration",
    command_fifo_gl_window_tail_migration_absent => "command-fifo-gl-window-tail-migration-absent",
    command_fifo_id_plane_write => "command-fifo-id-plane-write",
    #[ignore = "unknown: line 12: pixel: engine 0x116cc7ff, case needs == 0x116cc722; alpha of a DMA image put: engine forces 0xff, its burst put keeps source alpha, old device kept it, board unknown"]
    command_fifo_image_alpha_dma => "command-fifo-image-alpha-dma",
    command_fifo_image_alpha_screen => "command-fifo-image-alpha-screen",
    #[ignore = "unknown: line 27: pixel: engine 0x44556600, case needs == 0x445566ff; alpha byte of an A=0 code 0x4c source: engine stores 0, old device opaque, board unknown"]
    command_fifo_image_raster_clips_descending => "command-fifo-image-raster-clips-descending",
    command_fifo_image_raster_colour_index => "command-fifo-image-raster-colour-index",
    command_fifo_image_raster_colour_index_groups => "command-fifo-image-raster-colour-index-groups",
    command_fifo_image_raster_colour_index16 => "command-fifo-image-raster-colour-index16",
    #[ignore = "unknown: line 33: pixel: engine 0x11223300, case needs == 0x112233ff; alpha byte of an A=0 code 0x4c source: engine stores 0, old device opaque, board unknown"]
    command_fifo_image_raster_descending => "command-fifo-image-raster-descending",
    #[ignore = "unknown: line 84: pixel: engine 0x77889900, case needs == 0x778899ff; alpha byte of an A=0 code 0x4c source: engine stores 0, old device opaque, board unknown"]
    command_fifo_image_raster_empty_extent => "command-fifo-image-raster-empty-extent",
    command_fifo_image_raster_formats => "command-fifo-image-raster-formats",
    #[ignore = "unknown: line 29: pixel: engine 0x11223300, case needs == 0x112233ff; alpha byte of an A=0 code 0x4c source: engine stores 0, old device opaque, board unknown"]
    command_fifo_image_raster_mixed_directions => "command-fifo-image-raster-mixed-directions",
    #[ignore = "unknown: line 29: pixel: engine 0x11223300, case needs == 0x112233ff; alpha byte of an A=0 code 0x4c source: engine stores 0, old device opaque, board unknown"]
    command_fifo_image_raster_rgb => "command-fifo-image-raster-rgb",
    command_fifo_image_raster_short => "command-fifo-image-raster-short",
    command_fifo_image_raster_short_incomplete => "command-fifo-image-raster-short-incomplete",
    #[ignore = "old-wrong: line 84: pixel: engine 0x11223300, case needs == 0x112233ff; lines 55-59 (5:6:5 widening) are overridden; this one is the unknown alpha byte of an A=0 code 0x4c source"]
    command_fifo_image_raster_unknown_code => "command-fifo-image-raster-unknown-code",
    command_fifo_integer_logic_op => "command-fifo-integer-logic-op",
    command_fifo_integer_primitives_stream => "command-fifo-integer-primitives-stream",
    command_fifo_kernel_clear_box => "command-fifo-kernel-clear-box",
    command_fifo_lines => "command-fifo-lines",
    command_fifo_lines_cross_screen_edge => "command-fifo-lines-cross-screen-edge",
    command_fifo_opaque_span_ignores_shape => "command-fifo-opaque-span-ignores-shape",
    command_fifo_opaque_stippled_span => "command-fifo-opaque-stippled-span",
    command_fifo_pattern_quad => "command-fifo-pattern-quad",
    command_fifo_pattern_quad_opaque => "command-fifo-pattern-quad-opaque",
    command_fifo_pattern_quad_straddles_screen => "command-fifo-pattern-quad-straddles-screen",
    command_fifo_setup_registers => "command-fifo-setup-registers",
    command_fifo_unknown_word_is_unimplemented => "command-fifo-unknown-word-is-unimplemented",
    command_fifo_unknown_word_skips_its_operands => "command-fifo-unknown-word-skips-its-operands",
    command_fifo_x11_integer_after_gl => "command-fifo-x11-integer-after-gl",
    command_fifo_x11_line_cap_default => "command-fifo-x11-line-cap-default",
    command_fifo_x11_line_cap_not_last => "command-fifo-x11-line-cap-not-last",
    command_fifo_x11_line_cap_reverse_default => "command-fifo-x11-line-cap-reverse-default",
    command_fifo_x11_line_cap_reverse_not_last => "command-fifo-x11-line-cap-reverse-not-last",
    command_fifo_x11_line_cap_screen_bounds => "command-fifo-x11-line-cap-screen-bounds",
    command_fifo_x11_line_cap_vertical => "command-fifo-x11-line-cap-vertical",
    command_fifo_x11_line_cap_zero_default => "command-fifo-x11-line-cap-zero-default",
    command_fifo_x11_line_cap_zero_not_last => "command-fifo-x11-line-cap-zero-not-last",
    command_work_read_waits => "command-work-read-waits",
    context_restart_edited_page => "context-restart-edited-page",
    context_restart_one_vertex_strip => "context-restart-one-vertex-strip",
    context_restart_quad_strip_after_wrap => "context-restart-quad-strip-after-wrap",
    context_restart_triangle => "context-restart-triangle",
    context_restart_unsupported_words => "context-restart-unsupported-words",
    context_save_back_material => "context-save-back-material",
    context_save_back_material_reset => "context-save-back-material-reset",
    context_save_idle_descriptors => "context-save-idle-descriptors",
    context_save_normal_ring => "context-save-normal-ring",
    context_save_primitive_records => "context-save-primitive-records",
    context_save_texture_coordinate => "context-save-texture-coordinate",
    context_save_texture_coordinate_3d => "context-save-texture-coordinate-3d",
    context_save_texture_coordinate_inactive_negative_zero_r => "context-save-texture-coordinate-inactive-negative-zero-r",
    context_save_texture_coordinate_inactive_q => "context-save-texture-coordinate-inactive-q",
    context_save_texture_coordinate_inactive_r => "context-save-texture-coordinate-inactive-r",
    context_save_unrecorded_attributes => "context-save-unrecorded-attributes",
    context_save_zero_vertex_primitive => "context-save-zero-vertex-primitive",
    context_switch_completion => "context-switch-completion",
    context_switch_integer_quad => "context-switch-integer-quad",
    context_switch_polygon_assembly => "context-switch-polygon-assembly",
    context_switch_polygon_raster_restore => "context-switch-polygon-raster-restore",
    cursor__crosshair => "cursor__crosshair",
    cursor__deferred_records => "cursor__deferred-records",
    cursor__glyph_and_plane => "cursor__glyph-and-plane",
    cursor__migration => "cursor__migration",
    cursor__moves_and_clips => "cursor__moves-and-clips",
    cursor__over_overlay => "cursor__over-overlay",
    cursor__reset_leaves_it_off => "cursor__reset-leaves-it-off",
    cursor__unknowns_reported_once => "cursor__unknowns-reported-once",
    dma__draw_sentinel_abgr => "dma__draw-sentinel-abgr",
    dma__draw_sentinel_gl_byte_types => "dma__draw-sentinel-gl-byte-types",
    dma__draw_sentinel_gl_index_types => "dma__draw-sentinel-gl-index-types",
    dma__draw_sentinel_gl_int_types => "dma__draw-sentinel-gl-int-types",
    dma__draw_sentinel_gl_luminance => "dma__draw-sentinel-gl-luminance",
    dma__draw_sentinel_gl_packed_types => "dma__draw-sentinel-gl-packed-types",
    dma__draw_sentinel_gl_short_types => "dma__draw-sentinel-gl-short-types",
    dma__draw_sentinel_gl_single_channel => "dma__draw-sentinel-gl-single-channel",
    dma__draw_sentinel_gl_unmodeled => "dma__draw-sentinel-gl-unmodeled",
    dma__draw_sentinel_movie => "dma__draw-sentinel-movie",
    dma__draw_sentinel_short => "dma__draw-sentinel-short",
    dma__draw_sentinel_short_rejections => "dma__draw-sentinel-short-rejections",
    dma__draw_sentinel_short_row_gap => "dma__draw-sentinel-short-row-gap",
    dma__draw_sentinel_short_zero_origin_clip => "dma__draw-sentinel-short-zero-origin-clip",
    flow_control_credit_target => "flow-control-credit-target",
    framebuffer_pixel_accessor_bounds => "framebuffer-pixel-accessor-bounds",
    framebuffer_pixel_reaches_scanout => "framebuffer-pixel-reaches-scanout",
    gl_aux_refusals_reported_once => "gl-aux-refusals-reported-once",
    gl_large_clear_draws_in_bands => "gl-large-clear-draws-in-bands",
    mesa_per_draw_fallback => "mesa-per-draw-fallback",
    mesa_selector_and_model_did => "mesa-selector-and-model-did",
    query_record_migration => "query-record-migration",
    query_record_migration_defaults => "query-record-migration-defaults",
    readback__back_buffer => "readback__back-buffer",
    readback__direction => "readback__direction",
    readback__failures => "readback__failures",
    readback__front_follows_displayed_buffer => "readback__front-follows-displayed-buffer",
    #[ignore = "unknown: line 23: guest memory at 0xb84000, byte 16: engine 0x77, case needs 0x7b; synthetic read header (offsets 0,0) disagrees with the index mode the case programs; board answer unknown"]
    readback__gl_colour_index => "readback__gl-colour-index",
    readback__gl_layouts => "readback__gl-layouts",
    readback__gl_read_table => "readback__gl-read-table",
    readback__gl_rgba16_object => "readback__gl-rgba16-object",
    readback__gl_stencil_round_trip => "readback__gl-stencil-round-trip",
    readback__gl_unlisted_rows => "readback__gl-unlisted-rows",
    readback__greeter_caret_cell => "readback__greeter-caret-cell",
    readback__intermediate_block => "readback__intermediate-block",
    readback__page_crossing => "readback__page-crossing",
    readback__put_abgr_x_image => "readback__put-abgr-x-image",
    readback__row_gap_bottom_up => "readback__row-gap-bottom-up",
    readback__sentinel_drawable_relative => "readback__sentinel-drawable-relative",
    readback__two_by_two_row_order => "readback__two-by-two-row-order",
    reset_and_memory_control_writes => "reset-and-memory-control-writes",
    reset_clears_the_framebuffer => "reset-clears-the-framebuffer",
    rgba__clear_masks => "rgba__clear-masks",
    rgba__disabled_colour_depth => "rgba__disabled-colour-depth",
    rgba__four_float_alpha => "rgba__four-float-alpha",
    rgba__index_transitions => "rgba__index-transitions",
    rgba__material_masks => "rgba__material-masks",
    rgba__pattern_float_rectangle => "rgba__pattern-float-rectangle",
    rgba__pattern_float_rectangle_migration => "rgba__pattern-float-rectangle-migration",
    rgba__pattern_origin_ownership => "rgba__pattern-origin-ownership",
    rgba__read_bounds => "rgba__read-bounds",
    scanout_display_mode_recolours => "scanout-display-mode-recolours",
    scanout_main_id_0_uses_its_own_mode => "scanout-main-id-0-uses-its-own-mode",
    scanout_overlay_menu => "scanout-overlay-menu",
    scanout_overlay_planes => "scanout-overlay-planes",
    scanout_overlay_unselected_mask => "scanout-overlay-unselected-mask",
    scanout_paint_order => "scanout-paint-order",
    #[ignore = "unknown: line 4801: displayed colour: engine 0xff, case needs == 0x4880a0ff; display-mode format bits 0x9, 0xe, 0xf over pixels stored as 8:8:8:8: board output for the mismatch unknown"]
    scanout_rgb_narrow_visuals => "scanout-rgb-narrow-visuals",
    scanout_rgb_through_gamma => "scanout-rgb-through-gamma",
    scanout_wipe_repaint_reload => "scanout-wipe-repaint-reload",
    statistics_region_registers => "statistics-region-registers",
    stored_volume_partial_slice_keeps_fallback => "stored-volume-partial-slice-keeps-fallback",
    swap_arm_accumulates => "swap-arm-accumulates",
    swap_arm_toggles_displayed_buffer => "swap-arm-toggles-displayed-buffer",
    swap_set_and_clear_at_retrace => "swap-set-and-clear-at-retrace",
    texture_adapter__3d_state_restore => "texture-adapter__3d-state-restore",
    #[ignore = "unknown: line 229: pixel: engine 0x7f7f7fa0, case needs == 0x808080a0; linear tap at weight 1/2: engine truncates 12 to 8 bits (0x7f), old device rounded (0x80); board unknown"]
    texture_adapter__binding_filters_matrix => "texture-adapter__binding-filters-matrix",
    texture_adapter__blend_environment => "texture-adapter__blend-environment",
    texture_adapter__blend_state_restore => "texture-adapter__blend-state-restore",
    texture_adapter__bordered_mipmap => "texture-adapter__bordered-mipmap",
    texture_adapter__clamp_border_state_restore => "texture-adapter__clamp-border-state-restore",
    texture_adapter__colour_table => "texture-adapter__colour-table",
    texture_adapter__colour_table_incomplete => "texture-adapter__colour-table-incomplete",
    texture_adapter__colour_table_l8_rgb_alpha => "texture-adapter__colour-table-l8-rgb-alpha",
    texture_adapter__colour_table_l8_rgba_alpha => "texture-adapter__colour-table-l8-rgba-alpha",
    texture_adapter__colour_table_migration => "texture-adapter__colour-table-migration",
    texture_adapter__colour_table_reset => "texture-adapter__colour-table-reset",
    texture_adapter__colour_table_rgb_basis => "texture-adapter__colour-table-rgb-basis",
    texture_adapter__colour_table_rgba8_rgb_basis => "texture-adapter__colour-table-rgba8-rgb-basis",
    texture_adapter__colour_table_rgba8_rgba_basis => "texture-adapter__colour-table-rgba8-rgba-basis",
    texture_adapter__components_environment => "texture-adapter__components-environment",
    texture_adapter__detail_function_endpoints => "texture-adapter__detail-function-endpoints",
    texture_adapter__detail_rectangular_migration => "texture-adapter__detail-rectangular-migration",
    texture_adapter__detail_zero_alpha_reference => "texture-adapter__detail-zero-alpha-reference",
    texture_adapter__detail_zero_identity => "texture-adapter__detail-zero-identity",
    texture_adapter__edge_clamp_size_flag => "texture-adapter__edge-clamp-size-flag",
    texture_adapter__height_one => "texture-adapter__height-one",
    texture_adapter__integer_quads => "texture-adapter__integer-quads",
    texture_adapter__linear_q => "texture-adapter__linear-q",
    texture_adapter__linear_r => "texture-adapter__linear-r",
    texture_adapter__linear_srq_bulk => "texture-adapter__linear-srq-bulk",
    texture_adapter__linear_st => "texture-adapter__linear-st",
    texture_adapter__linear_strq_bulk => "texture-adapter__linear-strq-bulk",
    texture_adapter__lod_bias => "texture-adapter__lod-bias",
    texture_adapter__max_clamp => "texture-adapter__max-clamp",
    texture_adapter__nearest => "texture-adapter__nearest",
    #[ignore = "unknown: line 439: pixel: engine 0x7f7f0060, case needs == 0x80800060; linear taps at weight 1/2: engine 0x7f, old device 0x80; board filter rounding unknown"]
    texture_adapter__nearest_clamp => "texture-adapter__nearest-clamp",
    texture_adapter__nearest_clamp_border => "texture-adapter__nearest-clamp-border",
    texture_adapter__offset_and_unknown_table => "texture-adapter__offset-and-unknown-table",
    texture_adapter__one_dimensional_clamp_linear => "texture-adapter__one-dimensional-clamp-linear",
    texture_adapter__one_dimensional_integer_coordinate => "texture-adapter__one-dimensional-integer-coordinate",
    texture_adapter__one_dimensional_mode => "texture-adapter__one-dimensional-mode",
    texture_adapter__one_dimensional_stored_nearest => "texture-adapter__one-dimensional-stored-nearest",
    texture_adapter__one_dimensional_stored_upload => "texture-adapter__one-dimensional-stored-upload",
    texture_adapter__projective_3d => "texture-adapter__projective-3d",
    texture_adapter__quad_lod => "texture-adapter__quad-lod",
    texture_adapter__sampler_bit17 => "texture-adapter__sampler-bit17",
    texture_adapter__scale_bias => "texture-adapter__scale-bias",
    texture_adapter__state_restore => "texture-adapter__state-restore",
    texture_adapter__storage_dimensions => "texture-adapter__storage-dimensions",
    texture_adapter__unsupported_missing => "texture-adapter__unsupported-missing",
    texture_adapter__volume_slice_spacing => "texture-adapter__volume-slice-spacing",
    texture_level_without_rows_is_missing => "texture-level-without-rows-is-missing",
    texture_partial_level_is_present => "texture-partial-level-is-present",
    unimplemented_reports_independent_budgets => "unimplemented-reports-independent-budgets",
    unmodeled_offsets_are_visible => "unmodeled-offsets-are-visible",
}
