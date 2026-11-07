//! Replay of a recorded command stream into a board.
//!
//! A recording is what a guest sent a pipe and what the pipe answered, made by
//! another model of the same board (the file is `KONAREC`, a NUL, a `u32`
//! version, then tagged little-endian records; QEMU's KONA manual page,
//! "Recording the stream", defines it). Replaying it into a board with
//! `NullBackend` checks the front end and the file format before anything
//! draws: the board is built from the `C` record, each `W` is a bus store and
//! each `R` a bus load, in order, retrying while the bus is busy; the board's
//! reads of guest memory are served from the `D` records and its writes are
//! compared with the `O` records.
//!
//! The recorder is another model, not a hardware measurement, so a match
//! shows the front end behaves as that model does and nothing more. What a
//! back end adds (pixel reads, query replies, image DMA, timing) a front end
//! on `NullBackend` cannot reproduce; the report lists those apart.
//!
//! The recordings are not in the repository. `KONA_RECORDINGS` names the
//! directory that holds them; without it the tests that need them skip.

use super::super::attach::{FifoLow, HostDma, InterruptPacket, KonaAttach, PacketSender};
use super::super::consts::{ACTIVITY, CAUSE_VIDEO, CREDIT, INT_CAUSE, PROBE};
use super::super::ident::KonaConfig;
use super::super::{Kona, KonaBuilder};
use super::census::{Census, Tap};
use crate::traits::{Resettable, BUS_BUSY, BUS_OK};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

/// One record of a recording, after the board's `C` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Rec {
    Store { size: u8, off: u32, val: u64 },
    Load { size: u8, off: u32, val: u64 },
    /// Guest memory the pipe read.
    DmaRead { src: u8, addr: u64, bytes: Vec<u8> },
    /// Guest memory the pipe wrote.
    DmaWrite { src: u8, addr: u64, bytes: Vec<u8> },
    Interrupt { kind: u8, vector: u8, delivered: u8 },
    Marker { kind: u32, arg: u32 },
}

/// The board a recording was made on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Board {
    pub part: u32,
    pub revision: u32,
    pub rm_count: u32,
    pub texture_mb: u32,
    pub product: String,
}

#[derive(Debug)]
pub(super) struct Recording {
    pub board: Board,
    pub records: Vec<Rec>,
    /// The record count the end record gave: None for a file cut off.
    pub end: Option<u64>,
}

/// One thing in a recording: the board, a record, or the end with its count.
pub(super) enum Item {
    Board(Board),
    Rec(Rec),
    End(u64),
}

/// A recording read record by record, so a file too big for memory can be
/// replayed.
pub(super) struct Stream<R: Read> {
    r: R,
    /// Bytes read so far, for messages.
    at: u64,
    seen_board: bool,
    ended: bool,
}

impl<R: Read> Stream<R> {
    pub fn new(mut r: R) -> Result<Stream<R>, String> {
        let mut head = [0u8; 12];
        r.read_exact(&mut head).map_err(|_| "not a KONAREC file".to_string())?;
        if &head[..8] != b"KONAREC\0" {
            return Err("not a KONAREC file".into());
        }
        let version = u32::from_le_bytes(head[8..12].try_into().unwrap());
        if version != 1 {
            return Err(format!("format version {version}, not 1"));
        }
        Ok(Stream { r, at: 12, seen_board: false, ended: false })
    }

    fn fill(&mut self, buf: &mut [u8]) -> Result<(), String> {
        self.r.read_exact(buf).map_err(|_| format!("record cut off at byte {}", self.at))?;
        self.at += buf.len() as u64;
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, String> {
        let mut b = [0u8; 1];
        self.fill(&mut b)?;
        Ok(b[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let mut b = [0u8; 4];
        self.fill(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let mut b = [0u8; 8];
        self.fill(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn bytes(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let mut v = vec![0u8; n];
        self.fill(&mut v)?;
        Ok(v)
    }

    /// The next item; None at the end of the file between records.
    pub fn next(&mut self) -> Result<Option<Item>, String> {
        let tag_at = self.at;
        let mut t = [0u8; 1];
        let n = self.r.read(&mut t).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        self.at += 1;
        if self.ended {
            return Err(format!("data after the end record, at byte {tag_at}"));
        }
        let tag = t[0];
        if tag != b'C' && !self.seen_board {
            return Err("the board record is not first".into());
        }
        Ok(Some(match tag {
            b'C' => {
                let (part, revision, rm_count, texture_mb) = (self.u32()?, self.u32()?, self.u32()?, self.u32()?);
                let n = self.u8()? as usize;
                let product = String::from_utf8_lossy(&self.bytes(n)?).into_owned();
                if std::mem::replace(&mut self.seen_board, true) {
                    return Err(format!("second board record at byte {tag_at}"));
                }
                Item::Board(Board { part, revision, rm_count, texture_mb, product })
            }
            b'W' | b'R' => {
                let (size, off, val) = (self.u8()?, self.u32()?, self.u64()?);
                Item::Rec(if tag == b'W' { Rec::Store { size, off, val } } else { Rec::Load { size, off, val } })
            }
            b'D' | b'O' => {
                let (src, addr, n) = (self.u8()?, self.u64()?, self.u32()? as usize);
                let bytes = self.bytes(n)?;
                Item::Rec(if tag == b'D' { Rec::DmaRead { src, addr, bytes } } else { Rec::DmaWrite { src, addr, bytes } })
            }
            b'I' => Item::Rec(Rec::Interrupt { kind: self.u8()?, vector: self.u8()?, delivered: self.u8()? }),
            b'M' => Item::Rec(Rec::Marker { kind: self.u32()?, arg: self.u32()? }),
            b'E' => {
                self.ended = true;
                Item::End(self.u64()?)
            }
            _ => return Err(format!("unknown record tag {tag:#04x} at byte {tag_at}")),
        }))
    }
}

impl Recording {
    /// Parse a file. A file with no end record parses, with `end` None and
    /// whole records only: the recorder flushes at record boundaries, but the
    /// last record of a killed run may be partial and is an error.
    pub fn parse(bytes: &[u8]) -> Result<Recording, String> {
        let mut stream = Stream::new(bytes)?;
        let mut board = None;
        let mut records = Vec::new();
        let mut end = None;
        while let Some(item) = stream.next()? {
            match item {
                Item::Board(b) => board = Some(b),
                Item::Rec(r) => records.push(r),
                Item::End(n) => end = Some(n),
            }
        }
        let board = board.ok_or("no board record")?;
        if let Some(n) = end {
            // The count is of the records before the end, the board's included.
            if n != records.len() as u64 + 1 {
                return Err(format!("end record counts {n} records, the file has {}", records.len() + 1));
            }
        }
        Ok(Recording { board, records, end })
    }

    /// The `KonaConfig` of the recorded board. The product must be the
    /// InfiniteReality2 this front end models.
    pub fn config(&self) -> Result<KonaConfig, String> {
        if self.board.product != "ir2" {
            return Err(format!("product {:?}: only ir2 is modelled", self.board.product));
        }
        KonaConfig::ir2(self.board.rm_count)
    }
}

/// The directory the recordings are in, if the environment names one that
/// exists. Prints why a test is skipping when not.
pub(super) fn recordings_dir() -> Option<std::path::PathBuf> {
    match std::env::var_os("KONA_RECORDINGS") {
        Some(d) if std::path::Path::new(&d).is_dir() => Some(d.into()),
        Some(d) => {
            eprintln!("skipping: KONA_RECORDINGS={d:?} is not a directory");
            None
        }
        None => {
            eprintln!("skipping: set KONA_RECORDINGS to the directory of the KONAREC recordings");
            None
        }
    }
}

/// The machine side of a replay: the interrupt packets, the guest memory the
/// board reads (served from the recording's `D` records) and the memory it
/// writes.
pub(super) struct ReplayHost {
    packets: Mutex<Vec<InterruptPacket>>,
    reads: Mutex<Reads>,
    /// The board's writes with the record index the replay was at.
    writes: Mutex<Vec<(usize, u64, Vec<u8>)>>,
    at: Arc<AtomicUsize>,
}

struct ReadRec {
    src: u8,
    addr: u64,
    bytes: Vec<u8>,
    /// Bytes at the front already served.
    served: usize,
}

#[derive(Default)]
struct Reads {
    recs: Vec<ReadRec>,
    /// The first record not yet wholly served.
    cursor: usize,
    /// Reads answered from a record.
    matched: u64,
    /// Reads answered from a record later than the first unserved one.
    skipped_over: u64,
    /// Bytes of reads no record covered, answered with zeros, with the first
    /// few addresses.
    unmatched: u64,
    unmatched_at: Vec<(u64, usize)>,
}

impl ReplayHost {
    fn new(rec: &Recording, at: Arc<AtomicUsize>) -> Arc<ReplayHost> {
        let reads = rec.records.iter().filter_map(|r| match r {
            Rec::DmaRead { src, addr, bytes } => Some((*src, *addr, bytes.clone())),
            _ => None,
        });
        ReplayHost::with_reads(reads.collect(), at)
    }

    /// A host that serves the board's reads from `reads` (source, address,
    /// bytes), in order.
    fn with_reads(reads: Vec<(u8, u64, Vec<u8>)>, at: Arc<AtomicUsize>) -> Arc<ReplayHost> {
        let recs = reads.into_iter().map(|(src, addr, bytes)| ReadRec { src, addr, bytes, served: 0 }).collect();
        Arc::new(ReplayHost { packets: Mutex::new(Vec::new()), reads: Mutex::new(Reads { recs, ..Default::default() }), writes: Mutex::new(Vec::new()), at })
    }

    fn attach(self: &Arc<Self>) -> KonaAttach {
        KonaAttach { packets: self.clone(), dma: self.clone(), fifo_low: self.clone() }
    }
}

impl PacketSender for ReplayHost {
    fn send(&self, p: InterruptPacket) {
        self.packets.lock().push(p);
    }
}

impl FifoLow for ReplayHost {
    fn drained(&self) {}
}

impl HostDma for ReplayHost {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let mut r = self.reads.lock();
        let mut done = 0;
        while done < buf.len() {
            let want = addr + done as u64;
            // The first record from the cursor with unserved bytes at `want`.
            let found = (r.cursor..r.recs.len()).find(|&i| {
                let rec = &r.recs[i];
                want >= rec.addr + rec.served as u64 && want < rec.addr + rec.bytes.len() as u64
            });
            let Some(i) = found else {
                r.unmatched += (buf.len() - done) as u64;
                if r.unmatched_at.len() < 8 {
                    r.unmatched_at.push((want, buf.len() - done));
                }
                buf[done..].fill(0);
                return true;
            };
            if i > r.cursor {
                r.skipped_over += 1;
            }
            let rec = &mut r.recs[i];
            let from = (want - rec.addr) as usize;
            let n = (rec.bytes.len() - from).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&rec.bytes[from..from + n]);
            rec.served = from + n;
            done += n;
            while r.cursor < r.recs.len() && r.recs[r.cursor].served == r.recs[r.cursor].bytes.len() {
                r.cursor += 1;
            }
        }
        r.matched += 1;
        true
    }

    fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.writes.lock().push((self.at.load(Relaxed), addr, data.to_vec()));
        true
    }
}

/// A load whose answer depends on timing the replay does not reproduce.
/// Returns the mask of the bits that are compared: zero skips the load.
fn load_mask(off: u32) -> u64 {
    match off {
        // The FIFO's fill level and the watchdog latch follow the thread's
        // pace, which a replay without the thread does not have.
        CREDIT | ACTIVITY => 0,
        // The video bit rises at a retrace, which the recorder times against
        // its own clock.
        INT_CAUSE => !(CAUSE_VIDEO as u64),
        _ => !0,
    }
}

/// What a replay found.
#[derive(Default, Debug)]
pub(super) struct Report {
    pub stores: u64,
    pub loads: u64,
    pub loads_compared: u64,
    /// Loads that differed: (offset, expected, got) to (count, record index
    /// of the first).
    pub load_mismatches: BTreeMap<(u32, u64, u64), (u64, usize)>,
    pub resets: u64,
    /// The board's failure, if it stopped taking commands.
    pub failure: Option<String>,
    /// Recorded interrupts of the geometry-engine kind, and those the board sent.
    pub ge_expected: Vec<u8>,
    pub ge_sent: Vec<u8>,
    pub general_expected: u64,
    pub general_sent: u64,
    /// Retrace completions the recording marked, and the tags the board's
    /// back end was asked for.
    pub retrace_expected: Vec<u32>,
    pub retrace_asked: Vec<u32>,
    /// Swap markers: only a back end that draws can produce them.
    pub swap_markers: u64,
    pub reads_matched: u64,
    pub reads_skipped_over: u64,
    pub reads_unmatched: u64,
    pub reads_unmatched_at: Vec<(u64, usize)>,
    /// Recorded reads no read of the board used, by source.
    pub reads_unused: BTreeMap<u8, (u64, u64)>,
    /// Bytes the board wrote that the recording has the same (run, bytes) for,
    /// bytes it wrote that the recording does not have, and recorded bytes it
    /// never wrote, each by source of the recorded write where known.
    pub writes_same: u64,
    pub writes_extra: Vec<(u64, usize)>,
    pub writes_extra_bytes: u64,
    pub writes_missing: Vec<(u8, u64, usize)>,
    pub writes_missing_bytes: u64,
    pub writes_missing_by_src: BTreeMap<u8, (u64, u64)>,
    /// Writes in order: board runs found in the recorded runs in order.
    pub runs_in_order: u64,
    pub runs_board: u64,
    /// The first record index a load differed at.
    pub first_mismatch: Option<usize>,
    /// The record index of the 64th dropped word in a row (the run of data the
    /// front end could not size began a little before), where a replay into `NullBackend` stops being exact (the back
    /// end answers `Reply::Data` for those; `NullBackend` does not).
    pub exact_until: usize,
    /// Records in the recording, after the board's.
    pub records: usize,
    /// Before that index: runs of bytes the board wrote that the recording's
    /// writes also hold, in order, out of all the board wrote; how many runs
    /// the recording has (it has more: what only a back end writes); and the
    /// first board runs it lacks.
    pub exact_runs: (u64, u64),
    pub exact_runs_recorded: u64,
    pub exact_runs_lost: Vec<(u64, usize)>,
}

/// Merge consecutive writes that touch end to start into runs.
fn runs(writes: impl IntoIterator<Item = (u64, Vec<u8>)>) -> Vec<(u64, Vec<u8>)> {
    let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
    for (a, d) in writes {
        match out.last_mut() {
            Some((la, ld)) if *la + ld.len() as u64 == a => ld.extend_from_slice(&d),
            _ => out.push((a, d)),
        }
    }
    out
}

/// Replay `rec` into a board with `NullBackend`, through a `Tap`.
pub(super) fn replay(rec: &Recording) -> Result<(Report, Census), String> {
    let cfg = rec.config()?;
    let at = Arc::new(AtomicUsize::new(0));
    let host = ReplayHost::new(rec, at.clone());
    let (tap, census) = Tap::new(at.clone());
    // The probe register's value is the board's configuration, which the
    // recording leaves out; its first answer is the value the recorder's
    // board had.
    let probe = rec.records.iter().find_map(|r| match r {
        Rec::Load { off, val, .. } if *off == PROBE => Some(*val as u32),
        _ => None,
    });
    let kona: Arc<Kona> = KonaBuilder::new(cfg, host.attach()).backend(Box::new(tap)).probe_value(probe.unwrap_or(0)).build()?;
    let mut rep = Report::default();
    let busy_limit = 50_000_000u32;
    for (index, r) in rec.records.iter().enumerate() {
        at.store(index, Relaxed);
        match *r {
            Rec::Store { size, off, val } => {
                rep.stores += 1;
                let mut n = 0;
                while kona.bus_write(off as u64, size as u32, val) == BUS_BUSY {
                    n += 1;
                    if n > busy_limit {
                        return Err(format!("store at {off:#x} stayed busy"));
                    }
                    std::thread::yield_now();
                }
            }
            Rec::Load { size, off, val } => {
                rep.loads += 1;
                let mut n = 0;
                let got = loop {
                    let (status, data) = kona.bus_read(off as u64, size as u32);
                    if status == BUS_OK {
                        break data;
                    }
                    n += 1;
                    if status != BUS_BUSY || n > busy_limit {
                        return Err(format!("load at {off:#x} answered {status:#x}"));
                    }
                    std::thread::yield_now();
                };
                let mask = load_mask(off);
                if mask != 0 {
                    rep.loads_compared += 1;
                    if got & mask != val & mask {
                        rep.load_mismatches.entry((off, val & mask, got & mask)).or_insert((0, index)).0 += 1;
                        rep.first_mismatch.get_or_insert(index);
                    }
                }
            }
            Rec::Marker { kind: 1, .. } => {
                rep.resets += 1;
                kona.power_on();
            }
            Rec::Marker { kind: 2, .. } => rep.swap_markers += 1,
            Rec::Marker { kind: 3, arg } => rep.retrace_expected.push(arg),
            Rec::Marker { .. } | Rec::DmaRead { .. } | Rec::DmaWrite { .. } => {}
            Rec::Interrupt { kind: 1, vector, .. } => rep.ge_expected.push(vector),
            Rec::Interrupt { .. } => rep.general_expected += 1,
        }
    }
    rep.failure = kona.failure();

    // Interrupts. The recording names a kind and a vector; a packet carries
    // only the vector, so the geometry-engine vector is told from the
    // general one by the vectors the recording used for each.
    let general: Vec<u8> = rec
        .records
        .iter()
        .filter_map(|r| if let Rec::Interrupt { kind: 0, vector, .. } = r { Some(*vector) } else { None })
        .collect();
    for p in host.packets.lock().iter() {
        if rep.ge_expected.contains(&p.vector) && !general.contains(&p.vector) {
            rep.ge_sent.push(p.vector);
        } else {
            rep.general_sent += 1;
        }
    }
    rep.retrace_asked = census.lock().retraces.clone();

    // Reads.
    {
        let r = host.reads.lock();
        rep.reads_matched = r.matched;
        rep.reads_skipped_over = r.skipped_over;
        rep.reads_unmatched = r.unmatched;
        rep.reads_unmatched_at = r.unmatched_at.clone();
        for rec in r.recs.iter().filter(|x| x.served == 0) {
            let e = rep.reads_unused.entry(rec.src).or_default();
            e.0 += 1;
            e.1 += rec.bytes.len() as u64;
        }
    }

    // Writes: as byte maps, so chunking does not matter, and as runs, so
    // order does.
    let recorded: Vec<(u8, u64, Vec<u8>)> = rec
        .records
        .iter()
        .filter_map(|r| if let Rec::DmaWrite { src, addr, bytes } = r { Some((*src, *addr, bytes.clone())) } else { None })
        .collect();
    let board_all = host.writes.lock().clone();
    let board: Vec<(u64, Vec<u8>)> = board_all.iter().map(|(_, a, d)| (*a, d.clone())).collect();
    let mut rec_final: BTreeMap<u64, (u8, u8)> = BTreeMap::new();
    for (src, a, d) in &recorded {
        for (i, b) in d.iter().enumerate() {
            rec_final.insert(a + i as u64, (*b, *src));
        }
    }
    let mut board_final: BTreeMap<u64, u8> = BTreeMap::new();
    for (a, d) in &board {
        for (i, b) in d.iter().enumerate() {
            board_final.insert(a + i as u64, *b);
        }
    }
    let mut extra: Vec<u64> = Vec::new();
    for (a, b) in &board_final {
        match rec_final.get(a) {
            Some((rb, _)) if rb == b => rep.writes_same += 1,
            _ => extra.push(*a),
        }
    }
    rep.writes_extra_bytes = extra.len() as u64;
    for run in extra.chunk_by(|a, b| *b == *a + 1) {
        if rep.writes_extra.len() < 8 {
            rep.writes_extra.push((run[0], run.len()));
        }
    }
    let mut missing: Vec<(u64, u8)> = Vec::new();
    for (a, (b, src)) in &rec_final {
        if board_final.get(a) != Some(b) {
            missing.push((*a, *src));
        }
    }
    rep.writes_missing_bytes = missing.len() as u64;
    for run in missing.chunk_by(|a, b| b.0 == a.0 + 1 && b.1 == a.1) {
        let e = rep.writes_missing_by_src.entry(run[0].1).or_default();
        e.0 += 1;
        e.1 += run.len() as u64;
        if rep.writes_missing.len() < 8 {
            rep.writes_missing.push((run[0].1, run[0].0, run.len()));
        }
    }
    let rec_runs = runs(recorded.iter().map(|(_, a, d)| (*a, d.clone())));
    let board_runs = runs(board);
    let mut at = 0;
    for run in &board_runs {
        rep.runs_board += 1;
        if let Some(i) = rec_runs[at..].iter().position(|x| x == run) {
            at += i + 1;
            rep.runs_in_order += 1;
        }
    }

    let census = std::mem::take(&mut *census.lock());
    rep.records = rec.records.len();
    rep.exact_until = census.first_long_drop.unwrap_or(rep.records);
    let before = |at: usize| at < rep.exact_until;
    let exact_board = runs(board_all.iter().filter(|(at, ..)| before(*at)).map(|(_, a, d)| (*a, d.clone())));
    let exact_rec = runs(rec.records.iter().enumerate().filter(|(at, _)| before(*at)).filter_map(|(_, r)| match r {
        Rec::DmaWrite { addr, bytes, .. } => Some((*addr, bytes.clone())),
        _ => None,
    }));
    let mut at = 0;
    for run in &exact_board {
        rep.exact_runs.1 += 1;
        match exact_rec[at..].iter().position(|x| x == run) {
            Some(i) => {
                at += i + 1;
                rep.exact_runs.0 += 1;
            }
            None if rep.exact_runs_lost.len() < 8 => rep.exact_runs_lost.push((run.0, run.1.len())),
            None => {}
        }
    }
    rep.exact_runs_recorded = exact_rec.len() as u64;
    Ok((rep, census))
}

impl Report {
    pub fn render(&self) -> String {
        let mut o = String::new();
        let _ = writeln!(o, "stores {}  loads {} ({} compared)  resets {}", self.stores, self.loads, self.loads_compared, self.resets);
        let _ = writeln!(o, "board failure: {:?}", self.failure);
        if self.exact_until >= self.records {
            let _ = writeln!(o, "exact over the whole stream; first load mismatch at {:?}", self.first_mismatch);
        } else {
            let _ = writeln!(
                o,
                "exact until record {}, where the {}th dropped word in a row came (a data run the front end could not size, begun a little before); first load mismatch at {:?}",
                self.exact_until,
                super::census::LONG_DROP,
                self.first_mismatch
            );
        }
        let _ = writeln!(
            o,
            "writes before that: {} of the board's {} runs are in the recording's {} in order; not found: {:x?}",
            self.exact_runs.0, self.exact_runs.1, self.exact_runs_recorded, self.exact_runs_lost
        );
        let _ = writeln!(o, "load mismatches: {}", self.load_mismatches.len());
        for ((off, want, got), (n, first)) in &self.load_mismatches {
            let _ = writeln!(o, "  {off:#08x} wanted {want:#x}, got {got:#x}: {n} times, first at record {first}");
        }
        let _ = writeln!(
            o,
            "geometry-engine interrupts: recorded {:?}, sent {:?}; general: recorded {}, sent {}",
            self.ge_expected, self.ge_sent, self.general_expected, self.general_sent
        );
        let _ = writeln!(o, "retraces: recorded {:?}, asked of the back end {:?}; swap markers {}", self.retrace_expected, self.retrace_asked, self.swap_markers);
        let _ = writeln!(
            o,
            "board reads: {} served from records ({} skipped over earlier ones), {} bytes unmatched {:?}",
            self.reads_matched, self.reads_skipped_over, self.reads_unmatched, self.reads_unmatched_at
        );
        let _ = writeln!(o, "recorded reads never used, by source: {:?}", self.reads_unused);
        let _ = writeln!(
            o,
            "board writes: {} bytes equal to the recording, {} bytes not in it {:?}, {} recorded bytes the board did not write {:?}",
            self.writes_same, self.writes_extra_bytes, self.writes_extra, self.writes_missing_bytes, self.writes_missing
        );
        let _ = writeln!(o, "recorded bytes not written, by source (runs, bytes): {:?}", self.writes_missing_by_src);
        let _ = writeln!(o, "board write runs found in the recorded runs, in order: {} of {}", self.runs_in_order, self.runs_board);
        o
    }
}

/// What a stretch of a stream between two user marks sent.
pub(super) struct Span {
    /// The mark the span starts at (0 for the start of the file).
    pub mark: u32,
    /// Records in it, swap markers and retrace-completion markers it holds.
    pub records: u64,
    pub swaps: u64,
    pub retraces: u64,
    pub census: Census,
}

/// Replay a recording too big to hold in memory into a board with a census
/// tap, cutting a census at every user mark. Loads are not compared (the
/// stream is past the point where a replay into `NullBackend` stays exact);
/// the point is which tokens the stream sends. Reads the file twice: the
/// board's DMA reads first, since they are recorded after the store that
/// causes them.
pub(super) fn census_spans(path: &std::path::Path) -> Result<Vec<Span>, String> {
    let open = || -> Result<Stream<std::io::BufReader<std::fs::File>>, String> {
        let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Stream::new(std::io::BufReader::with_capacity(1 << 20, f))
    };
    let mut board = None;
    let mut reads = Vec::new();
    let mut probe = None;
    let mut stream = open()?;
    while let Some(item) = stream.next()? {
        match item {
            Item::Board(b) => board = Some(b),
            Item::Rec(Rec::DmaRead { src, addr, bytes }) => reads.push((src, addr, bytes)),
            Item::Rec(Rec::Load { off, val, .. }) if off == PROBE && probe.is_none() => probe = Some(val as u32),
            _ => {}
        }
    }
    let board = board.ok_or("no board record")?;
    let cfg = Recording { board, records: Vec::new(), end: None }.config()?;
    let at = Arc::new(AtomicUsize::new(0));
    let host = ReplayHost::with_reads(reads, at.clone());
    let (tap, census) = Tap::new(at.clone());
    let kona = KonaBuilder::new(cfg, host.attach()).backend(Box::new(tap)).probe_value(probe.unwrap_or(0)).build()?;
    let mut spans = Vec::new();
    let (mut mark, mut records, mut swaps, mut retraces) = (0u32, 0u64, 0u64, 0u64);
    let mut stream = open()?;
    let mut index = 0usize;
    while let Some(item) = stream.next()? {
        let Item::Rec(r) = item else { continue };
        at.store(index, Relaxed);
        index += 1;
        records += 1;
        match r {
            Rec::Store { size, off, val } => {
                let mut n = 0u32;
                while kona.bus_write(off as u64, size as u32, val) == BUS_BUSY {
                    n += 1;
                    if n > 50_000_000 {
                        return Err(format!("store at {off:#x} stayed busy"));
                    }
                    std::thread::yield_now();
                }
            }
            Rec::Load { size, off, .. } => {
                let mut n = 0u32;
                while kona.bus_read(off as u64, size as u32).0 == BUS_BUSY {
                    n += 1;
                    if n > 50_000_000 {
                        return Err(format!("load at {off:#x} stayed busy"));
                    }
                    std::thread::yield_now();
                }
            }
            Rec::Marker { kind: 1, .. } => kona.power_on(),
            Rec::Marker { kind: 2, .. } => swaps += 1,
            Rec::Marker { kind: 3, .. } => retraces += 1,
            Rec::Marker { kind: 0x100, arg } => {
                let done = std::mem::take(&mut *census.lock());
                spans.push(Span { mark, records, swaps, retraces, census: done });
                (mark, records, swaps, retraces) = (arg, 0, 0, 0);
            }
            _ => {}
        }
    }
    spans.push(Span { mark, records, swaps, retraces, census: std::mem::take(&mut *census.lock()) });
    Ok(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording's bytes, built record by record.
    #[derive(Default)]
    struct Builder(Vec<u8>);

    impl Builder {
        fn new(rm: u32, product: &str) -> Builder {
            let mut b = Builder(b"KONAREC\0".to_vec());
            b.0.extend(1u32.to_le_bytes());
            b.0.push(b'C');
            for v in [0xc102u32, 0, rm, 64] {
                b.0.extend(v.to_le_bytes());
            }
            b.0.push(product.len() as u8);
            b.0.extend(product.as_bytes());
            b
        }

        fn rw(mut self, tag: u8, size: u8, off: u32, val: u64) -> Self {
            self.0.push(tag);
            self.0.push(size);
            self.0.extend(off.to_le_bytes());
            self.0.extend(val.to_le_bytes());
            self
        }

        fn mem(mut self, tag: u8, src: u8, addr: u64, bytes: &[u8]) -> Self {
            self.0.push(tag);
            self.0.push(src);
            self.0.extend(addr.to_le_bytes());
            self.0.extend((bytes.len() as u32).to_le_bytes());
            self.0.extend(bytes);
            self
        }

        fn marker(mut self, kind: u32, arg: u32) -> Self {
            self.0.push(b'M');
            self.0.extend(kind.to_le_bytes());
            self.0.extend(arg.to_le_bytes());
            self
        }

        fn end(mut self, count: u64) -> Vec<u8> {
            self.0.push(b'E');
            self.0.extend(count.to_le_bytes());
            self.0
        }
    }

    const PORT: u32 = 0xc10000;

    #[test]
    fn the_parser_reads_every_record_kind() {
        let mut b = Builder::new(2, "ir2")
            .rw(b'W', 4, PORT, 7)
            .rw(b'R', 8, 0x800240, 0x1122_3344_5566_7788)
            .mem(b'D', 3, 0x1000, &[1, 2, 3])
            .mem(b'O', 1, 0x2000, &[9]);
        b.0.extend([b'I', 1, 0x12, 1]);
        let b = b.marker(1, 0).end(7);
        let r = Recording::parse(&b).unwrap();
        assert_eq!(r.board, Board { part: 0xc102, revision: 0, rm_count: 2, texture_mb: 64, product: "ir2".into() });
        assert_eq!(r.end, Some(7));
        assert_eq!(
            r.records,
            [
                Rec::Store { size: 4, off: PORT, val: 7 },
                Rec::Load { size: 8, off: 0x800240, val: 0x1122_3344_5566_7788 },
                Rec::DmaRead { src: 3, addr: 0x1000, bytes: vec![1, 2, 3] },
                Rec::DmaWrite { src: 1, addr: 0x2000, bytes: vec![9] },
                Rec::Interrupt { kind: 1, vector: 0x12, delivered: 1 },
                Rec::Marker { kind: 1, arg: 0 },
            ]
        );
        assert!(r.config().is_ok());
    }

    #[test]
    fn the_parser_refuses_what_it_cannot_trust() {
        assert!(Recording::parse(b"ODSYREC\0\x01\0\0\0").is_err());
        let ok = Builder::new(2, "ir2").rw(b'W', 4, PORT, 7).end(2);
        assert!(Recording::parse(&ok).is_ok());
        // A wrong count, a cut-off record, an unknown tag, a second version.
        assert!(Recording::parse(&Builder::new(2, "ir2").rw(b'W', 4, PORT, 7).end(3)).unwrap_err().contains("counts 3"));
        // The end record cut in half is a cut-off record, not a short file.
        assert!(Recording::parse(&ok[..ok.len() - 4]).unwrap_err().contains("cut off"));
        let cut = Builder::new(2, "ir2").rw(b'W', 4, PORT, 7).0;
        assert!(Recording::parse(&cut[..cut.len() - 3]).unwrap_err().contains("cut off"));
        let mut bad = Builder::new(2, "ir2").0;
        bad.push(b'Z');
        assert!(Recording::parse(&bad).unwrap_err().contains("unknown record tag"));
        let mut v2 = Builder::new(2, "ir2").0;
        v2[8] = 2;
        assert!(Recording::parse(&v2).unwrap_err().contains("version 2"));
        // A file with no end record parses, and says so.
        assert_eq!(Recording::parse(&cut).unwrap().end, None);
        // Data after the end record, a record before the board, a second board.
        let mut after = ok.clone();
        after.push(b'W');
        assert!(Recording::parse(&after).unwrap_err().contains("after the end record"));
        let mut early = b"KONAREC\0\x01\0\0\0".to_vec();
        early.extend([b'W', 4]);
        assert!(Recording::parse(&early).unwrap_err().contains("not first"));
        let board = Builder::new(2, "ir2").0;
        let mut twice = board.clone();
        twice.extend(&board[12..]);
        assert!(Recording::parse(&twice).unwrap_err().contains("second board"));
        assert!(Recording::parse(&board[..12]).unwrap_err().contains("no board record"));
        // Only the InfiniteReality2 is modelled.
        assert!(Recording::parse(&Builder::new(2, "ir4").0).unwrap().config().is_err());
    }

    /// The textport probe and a colour select, a mailbox read the reply
    /// answers, a retrace request and a reset marker, through the board.
    #[test]
    fn a_small_stream_replays_and_is_compared() {
        let b = Builder::new(2, "ir2")
            .rw(b'W', 4, PORT, 1)
            .rw(b'W', 4, PORT, 9)
            .rw(b'W', 4, PORT, 0)
            .rw(b'R', 4, 0x800240, 0x1234abcd)
            .rw(b'W', 4, PORT, 0x1e7)
            .rw(b'W', 4, PORT, 0x42)
            .rw(b'R', 4, 0x800248, 0x42)
            .marker(3, 0x42)
            .marker(1, 0)
            .rw(b'R', 4, 0x800248, 0)
            .end(11);
        let rec = Recording::parse(&b).unwrap();
        let (rep, census) = replay(&rec).unwrap();
        assert_eq!(rep.loads_compared, 3, "{}", rep.render());
        assert!(rep.load_mismatches.is_empty(), "{}", rep.render());
        assert_eq!(rep.resets, 1);
        assert_eq!(rep.retrace_asked, [0x42]);
        assert_eq!(rep.retrace_asked, rep.retrace_expected);
        assert_eq!(census.retraces, [0x42]);
        assert!(rep.failure.is_none());
    }

    /// A load that differs from the recorded answer is reported, one that
    /// depends on timing is not compared.
    #[test]
    fn a_wrong_answer_is_reported_and_a_timing_load_is_not() {
        let b = Builder::new(2, "ir2")
            .rw(b'R', 4, 0x04, 0xdead_beef)
            .rw(b'R', 4, CREDIT, 0x7fe)
            .rw(b'R', 4, ACTIVITY, 4)
            .end(4);
        let (rep, ..) = replay(&Recording::parse(&b).unwrap()).unwrap();
        assert_eq!(rep.loads, 3);
        assert_eq!(rep.loads_compared, 1);
        assert_eq!(rep.load_mismatches.keys().map(|k| k.0).collect::<Vec<_>>(), [0x04]);
    }

    /// The board's reads are served from the recorded ones in order, a read
    /// no record covers is counted, and what the board writes is compared
    /// with the recorded writes.
    #[test]
    fn dma_is_served_and_compared() {
        let host = ReplayHost::new(&Recording::parse(&Builder::new(2, "ir2").mem(b'D', 3, 0x1000, &[1, 2, 3, 4]).mem(b'D', 3, 0x2000, &[5, 6]).end(3)).unwrap(), Arc::default());
        let mut a = [0u8; 2];
        assert!(host.read(0x1000, &mut a));
        assert_eq!(a, [1, 2]);
        let mut a = [0u8; 2];
        assert!(host.read(0x1002, &mut a));
        assert_eq!(a, [3, 4]);
        let mut a = [0xffu8; 2];
        assert!(host.read(0x9000, &mut a));
        assert_eq!(a, [0, 0]);
        let r = host.reads.lock();
        assert_eq!((r.matched, r.unmatched, r.cursor), (2, 2, 1));
    }

    /// The recordings, when KONA_RECORDINGS names them. The PROM stream uses
    /// the register aperture only; the IRIX stream adds DMA, contexts and
    /// the GL grammar.
    fn replay_file(name: &str) -> Option<(Recording, Report, Census)> {
        let dir = recordings_dir()?;
        let path = dir.join(name);
        if !path.exists() {
            eprintln!("skipping: {} is not there", path.display());
            return None;
        }
        let rec = Recording::parse(&std::fs::read(&path).unwrap()).unwrap();
        let (rep, census) = replay(&rec).unwrap();
        eprintln!("{name}\n{}", rep.render());
        eprintln!("{}", census.render());
        if let Some(before) = &census.before_long_drop {
            eprintln!("\n=== the same census up to record {}, where the front end could no longer size the stream ===\n{}", rep.exact_until, before.render());
        }
        Some((rec, rep, census))
    }

    #[test]
    fn the_prom_recording_replays() {
        let Some((rec, rep, ..)) = replay_file("onyx2-deskside-prom.kona.rec") else { return };
        assert_eq!(rec.end, Some(rec.records.len() as u64 + 1));
        assert!(rep.failure.is_none(), "{}", rep.render());
        assert!(rep.load_mismatches.is_empty(), "{}", rep.render());
        assert_eq!(rep.ge_expected, rep.ge_sent);
        assert_eq!(rep.exact_runs, (0, 0));
    }

    #[test]
    fn the_irix_recording_replays() {
        let Some((rec, rep, census)) = replay_file("onyx2-deskside-irix-4dwm.kona.rec") else { return };
        assert_eq!(rec.end, Some(rec.records.len() as u64 + 1));
        assert!(rep.failure.is_none(), "{}", rep.render());
        // Exact as far as the front end can frame the stream alone: every
        // deterministic load, and every byte the board wrote to guest memory,
        // matches the recording before the first run of data only a back end
        // can size (measured as the 64th dropped word in a row). Past it the stream misframes, as it must on NullBackend.
        // The window is pinned: a front-end change that makes the stream
        // misframe sooner shrinks what is checked, and must fail here.
        assert_eq!(rep.exact_until, 1_415_538, "{}", rep.render());
        assert_eq!(rep.exact_runs, (4, 4), "{}", rep.render());
        assert_eq!(rep.ge_sent, rep.ge_expected, "{}", rep.render());
        assert!(rep.first_mismatch.is_none_or(|at| at >= rep.exact_until), "{}", rep.render());
        assert!(rep.exact_runs.1 > 0 && rep.exact_runs.0 == rep.exact_runs.1, "{}", rep.render());
        // The recording has more: the data a pixel read's reply carries, which
        // only a back end posts.
        assert!(rep.exact_runs.1 < rep.exact_runs_recorded, "{}", rep.render());
        // Video interrupts depend on timing, so only that some came is
        // checked: the back end completes no retrace here, so none is sent.
        assert!(rep.general_expected > 0);
        assert!(census.jobs > 0);
    }

    /// The GL demo recording, when KONA_GL_RECORDING names it (a gigabyte: a
    /// minute or two in a debug build). Prints each demo's span; asserts that
    /// the marks are there and that GL commands reached the back end.
    #[test]
    fn the_gl_demo_recording_is_censused() {
        let Some(path) = std::env::var_os("KONA_GL_RECORDING").map(std::path::PathBuf::from) else {
            eprintln!("skipping: set KONA_GL_RECORDING to the GL demo recording");
            return;
        };
        let spans = census_spans(&path).unwrap();
        let marks: Vec<u32> = spans.iter().map(|s| s.mark).collect();
        for m in [210, 220, 230] {
            assert!(marks.contains(&m), "mark {m} not in {marks:?}");
        }
        for sp in spans.iter().filter(|s| [210, 220, 230].contains(&s.mark)) {
            eprintln!("=== span from mark {}: {} records, {} swap markers, {} retrace markers ===", sp.mark, sp.records, sp.swaps, sp.retraces);
            eprintln!("{}", sp.census.render());
            eprintln!("{}", sp.census.render_families());
            assert!(sp.census.jobs > 0);
        }
    }
}
