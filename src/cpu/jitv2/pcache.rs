//! Persistent code cache: compiled pages kept on disk across runs, keyed by
//! what they were compiled from. Design, measurements and verification plan:
//! `docs/jitv2-persistent-cache.md`.
//!
//! Opt-in with `[jitv2] cache = true` in `iris.toml` (or the GUI's jitv2
//! section), which `Jitv2Config::apply_env` turns into the `IRIS_JIT_CACHE`
//! env var this module actually reads — that var (and `IRIS_JIT_CACHE_DIR`,
//! `[jitv2] cache_dir`'s counterpart, which moves the cache; default: the
//! platform's user cache directory, `iris/jitv2` inside it; see
//! `default_base`) still work directly too, same override rule as the rest
//! of `[debug]`/`[jitv2]`.
//!
//! Layout, under the base directory:
//!
//! ```text
//! <build-id>/<fingerprint>/<page-hash>-<fr>/<entries-hash>.jc
//! ```
//!
//! - `build-id` is a BLAKE3 hash of the running executable, so a rebuild never
//!   sees another build's code.
//! - `fingerprint` hashes every runtime switch that shapes emitted code
//!   (`Codegen::cache_fingerprint`); a run with `j2 alu off` has its own space.
//! - Each page directory holds that page's variants, one per entry set. A blob
//!   serves any request whose entries are a subset of its own, and a new
//!   variant that covers an older one replaces it.
//!
//! There is no index: a lookup lists one small directory, which is the only
//! filesystem work on a page that has never been cached. Several emulators
//! can share a cache, since every write is a temp file plus a rename.
//!
//! A hit is always verified against the full 4 KB of page words stored in the
//! blob, so no hash collision can serve code for other bytes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;

use crate::cpu::jitv2::{BITMAP_WORDS, ENTRIES_PER_PAGE};

pub type Fingerprint = [u8; 16];
pub type PageHash = [u8; 16];
pub type Entries = [u64; BITMAP_WORDS];

const MAGIC: [u8; 8] = *b"IRISJC\0\0";
/// Bump when the file layout changes. (A codegen change needs no bump: it
/// changes the executable, and with it the build id.)
const FORMAT: u32 = 1;
/// How many build directories survive startup pruning.
const KEEP_BUILDS: usize = 3;
/// A lookup reads at most this many variants of one page.
const MAX_VARIANTS: usize = 16;
const REPORT_EVERY: u64 = 500;

const HEADER_LEN: usize = 8 + 4 + 4 + 16 + 16 + BITMAP_WORDS * 8 * 2 + 4 + 4 + 4;
const WORDS_LEN: usize = ENTRIES_PER_PAGE * 4;
const SUM_LEN: usize = 16;

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = matches!(std::env::var("IRIS_JIT_CACHE").as_deref(), Ok("1") | Ok("on"));
        if on {
            match root() {
                Some(dir) => eprintln!("jitcache: on, {}", dir.display()),
                None => eprintln!("jitcache: requested but no cache directory could be set up; off"),
            }
        }
        on && root().is_some()
    })
}

/// The user cache directory's `iris/jitv2`: `~/Library/Caches` on macOS,
/// `%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` or `~/.cache` elsewhere.
fn default_base() -> Option<PathBuf> {
    let cache = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else {
        match std::env::var_os("XDG_CACHE_HOME") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
        }
    };
    Some(cache.join("iris").join("jitv2"))
}

/// `<base>/<build-id>`, created on first use; `None` if it can't be.
fn root() -> Option<&'static PathBuf> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let base = match std::env::var_os("IRIS_JIT_CACHE_DIR") {
            Some(d) => PathBuf::from(d),
            None => default_base()?,
        };
        let exe = std::fs::read(std::env::current_exe().ok()?).ok()?;
        let id = hex(&blake3::hash(&exe).as_bytes()[..16]);
        let dir = base.join(id);
        std::fs::create_dir_all(&dir).ok()?;
        // Mark this build as the most recently used one, for pruning.
        let _ = std::fs::File::create(dir.join(".used"));
        prune_builds(&base);
        Some(dir)
    }).as_ref()
}

/// Keep the `KEEP_BUILDS` most recently used build directories.
fn prune_builds(base: &Path) {
    let Ok(rd) = std::fs::read_dir(base) else { return };
    let mut builds: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| {
            let used = std::fs::metadata(e.path().join(".used")).and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            (used, e.path())
        })
        .collect();
    builds.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in builds.into_iter().skip(KEEP_BUILDS) {
        let _ = std::fs::remove_dir_all(old);
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn page_hash(words: &[u32; ENTRIES_PER_PAGE]) -> PageHash {
    let mut h = blake3::Hasher::new();
    for w in words {
        h.update(&w.to_le_bytes());
    }
    first16(h.finalize())
}

fn first16(h: blake3::Hash) -> [u8; 16] {
    h.as_bytes()[..16].try_into().unwrap()
}

fn entries_hash(e: &Entries) -> [u8; 8] {
    let mut h = blake3::Hasher::new();
    for w in e {
        h.update(&w.to_le_bytes());
    }
    h.finalize().as_bytes()[..8].try_into().unwrap()
}

fn page_dir(fp: &Fingerprint, ph: &PageHash, fr1: bool) -> Option<PathBuf> {
    Some(root()?.join(hex(fp)).join(format!("{}-{}", hex(ph), fr1 as u8)))
}

fn covers(have: &Entries, want: &Entries) -> bool {
    have.iter().zip(want).all(|(h, w)| w & !h == 0)
}

fn count(e: &Entries) -> u32 {
    e.iter().map(|w| w.count_ones()).sum()
}

// ---- statistics ---------------------------------------------------------

static LOOKUPS: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
/// Same page hash, but the stored words differed: a hash collision, or the
/// fault-injected skip turned off. Should stay 0.
static COMPARE_FAILS: AtomicU64 = AtomicU64::new(0);
static BAD_FILES: AtomicU64 = AtomicU64::new(0);
static STORES: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static UNION_ADDED: AtomicU64 = AtomicU64::new(0);
static LOAD_NS: AtomicU64 = AtomicU64::new(0);

/// A compile whose output can't be stored (relocations, or a configuration
/// that bakes host addresses).
pub fn note_refused() {
    REFUSED.fetch_add(1, Relaxed);
}

/// Entries union-on-miss added to a compile.
pub fn note_union_added(n: u32) {
    UNION_ADDED.fetch_add(n as u64, Relaxed);
}

pub fn note_load_time(d: std::time::Duration) {
    LOAD_NS.fetch_add(d.as_nanos() as u64, Relaxed);
}

pub fn summary() -> String {
    let (l, h) = (LOOKUPS.load(Relaxed), HITS.load(Relaxed));
    format!(
        "jitcache: lookups={l} hits={h} ({:.1}%) stored={} refused={} union_added={} compare_fail={} bad_files={} load_us_avg={:.0}",
        if l == 0 { 0.0 } else { 100.0 * h as f64 / l as f64 },
        STORES.load(Relaxed), REFUSED.load(Relaxed), UNION_ADDED.load(Relaxed),
        COMPARE_FAILS.load(Relaxed), BAD_FILES.load(Relaxed),
        if h == 0 { 0.0 } else { LOAD_NS.load(Relaxed) as f64 / h as f64 / 1000.0 },
    )
}

// ---- blobs ---------------------------------------------------------------

/// One compiled page, as stored.
pub struct Blob {
    /// Entry points the code has a dispatch case for.
    pub entries: Entries,
    /// Words the compile decoded (the churn-avoidance snapshot's mask).
    pub used: Entries,
    pub instr_count: u32,
    pub align: u32,
    pub words: Box<[u32; ENTRIES_PER_PAGE]>,
    pub code: Vec<u8>,
}

struct Header {
    fr1: bool,
    fp: Fingerprint,
    ph: PageHash,
    entries: Entries,
    used: Entries,
    instr_count: u32,
    align: u32,
    code_len: usize,
}

fn put_entries(out: &mut Vec<u8>, e: &Entries) {
    for w in e {
        out.extend_from_slice(&w.to_le_bytes());
    }
}

fn encode(fp: &Fingerprint, ph: &PageHash, fr1: bool, b: &Blob) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + WORDS_LEN + b.code.len() + SUM_LEN);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&(fr1 as u32).to_le_bytes());
    out.extend_from_slice(fp);
    out.extend_from_slice(ph);
    put_entries(&mut out, &b.entries);
    put_entries(&mut out, &b.used);
    out.extend_from_slice(&b.instr_count.to_le_bytes());
    out.extend_from_slice(&b.align.to_le_bytes());
    out.extend_from_slice(&(b.code.len() as u32).to_le_bytes());
    debug_assert_eq!(out.len(), HEADER_LEN);
    for w in b.words.iter() {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.extend_from_slice(&b.code);
    let sum = first16(blake3::hash(&out));
    out.extend_from_slice(&sum);
    out
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn take(&mut self, n: usize) -> &[u8] {
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        a
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap())
    }
    fn arr16(&mut self) -> [u8; 16] {
        self.take(16).try_into().unwrap()
    }
    fn entries(&mut self) -> Entries {
        let mut e = [0u64; BITMAP_WORDS];
        for w in &mut e {
            *w = u64::from_le_bytes(self.take(8).try_into().unwrap());
        }
        e
    }
}

fn decode_header(buf: &[u8]) -> Option<Header> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let mut r = Reader(&buf[..HEADER_LEN]);
    if r.take(8) != MAGIC || r.u32() != FORMAT {
        return None;
    }
    let fr = r.u32();
    if fr > 1 {
        return None;
    }
    Some(Header {
        fr1: fr == 1,
        fp: r.arr16(),
        ph: r.arr16(),
        entries: r.entries(),
        used: r.entries(),
        instr_count: r.u32(),
        align: r.u32(),
        code_len: r.u32() as usize,
    })
}

/// Parse and fully check one file against the key it was found under.
fn decode(buf: &[u8], fp: &Fingerprint, ph: &PageHash, fr1: bool) -> Option<Blob> {
    let h = decode_header(buf)?;
    if h.fr1 != fr1 || &h.fp != fp || &h.ph != ph || !h.align.is_power_of_two() {
        return None;
    }
    if buf.len() != HEADER_LEN + WORDS_LEN + h.code_len + SUM_LEN {
        return None;
    }
    let body = &buf[..buf.len() - SUM_LEN];
    if first16(blake3::hash(body)) != buf[body.len()..] {
        return None;
    }
    let mut words = Box::new([0u32; ENTRIES_PER_PAGE]);
    for (w, c) in words.iter_mut().zip(buf[HEADER_LEN..HEADER_LEN + WORDS_LEN].chunks_exact(4)) {
        *w = u32::from_le_bytes(c.try_into().unwrap());
    }
    Some(Blob {
        entries: h.entries,
        used: h.used,
        instr_count: h.instr_count,
        align: h.align,
        words,
        code: buf[HEADER_LEN + WORDS_LEN..body.len()].to_vec(),
    })
}

fn variant_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jc"))
        .take(MAX_VARIANTS)
        .collect()
}

fn discard_bad(path: &Path) {
    BAD_FILES.fetch_add(1, Relaxed);
    let _ = std::fs::remove_file(path);
}

/// Find a stored compile of exactly `words` whose entries cover `want`. When
/// several do, the one with the most entries wins.
pub fn lookup(
    fp: &Fingerprint,
    ph: &PageHash,
    fr1: bool,
    words: &[u32; ENTRIES_PER_PAGE],
    want: &Entries,
) -> Option<Blob> {
    let n = LOOKUPS.fetch_add(1, Relaxed) + 1;
    if n % REPORT_EVERY == 0 {
        eprintln!("{}", summary());
    }
    let dir = page_dir(fp, ph, fr1)?;
    let mut best: Option<Blob> = None;
    for path in variant_files(&dir) {
        let Ok(buf) = std::fs::read(&path) else { continue };
        // Only a covering variant is worth the full check.
        match decode_header(&buf) {
            Some(h) if covers(&h.entries, want) => {}
            Some(_) => continue,
            None => { discard_bad(&path); continue; }
        }
        let Some(blob) = decode(&buf, fp, ph, fr1) else { discard_bad(&path); continue };
        if *blob.words != *words {
            COMPARE_FAILS.fetch_add(1, Relaxed);
            continue;
        }
        if best.as_ref().is_none_or(|b| count(&blob.entries) > count(&b.entries)) {
            best = Some(blob);
        }
    }
    if best.is_some() {
        HITS.fetch_add(1, Relaxed);
    }
    best
}

/// Every entry any stored variant of this page has, for union-on-miss. Only
/// headers are read; the walk that follows decides what is really covered.
pub fn known_entries(fp: &Fingerprint, ph: &PageHash, fr1: bool) -> Entries {
    let mut all = [0u64; BITMAP_WORDS];
    let Some(dir) = page_dir(fp, ph, fr1) else { return all };
    for path in variant_files(&dir) {
        let Ok(mut f) = std::fs::File::open(&path) else { continue };
        let mut buf = vec![0u8; HEADER_LEN];
        if std::io::Read::read_exact(&mut f, &mut buf).is_err() {
            continue;
        }
        if let Some(h) = decode_header(&buf) {
            if &h.fp == fp && &h.ph == ph && h.fr1 == fr1 {
                for (a, e) in all.iter_mut().zip(&h.entries) {
                    *a |= e;
                }
            }
        }
    }
    all
}

// ---- writer --------------------------------------------------------------

struct Job {
    fp: Fingerprint,
    ph: PageHash,
    fr1: bool,
    blob: Blob,
}

/// Queue a successful compile for writing. Returns at once; a background
/// thread does the filesystem work.
pub fn store(fp: Fingerprint, ph: PageHash, fr1: bool, blob: Blob) {
    static TX: OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("jitcache-writer".into())
            .spawn(move || {
                for job in rx {
                    write_job(job);
                }
            })
            .expect("spawn jitcache writer");
        std::sync::Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send(Job { fp, ph, fr1, blob });
}

fn write_job(job: Job) {
    let Some(dir) = page_dir(&job.fp, &job.ph, job.fr1) else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let bytes = encode(&job.fp, &job.ph, job.fr1, &job.blob);
    let name = format!("{}.jc", hex(&entries_hash(&job.blob.entries)));
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_err() || std::fs::rename(&tmp, dir.join(&name)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    STORES.fetch_add(1, Relaxed);
    // A variant the new one covers can never be chosen over it again.
    for path in variant_files(&dir) {
        if path.file_name().is_some_and(|n| n == name.as_str()) {
            continue;
        }
        let Ok(mut f) = std::fs::File::open(&path) else { continue };
        let mut buf = vec![0u8; HEADER_LEN];
        if std::io::Read::read_exact(&mut f, &mut buf).is_ok() {
            if let Some(h) = decode_header(&buf) {
                if covers(&job.blob.entries, &h.entries) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

// ---- in-session tier ----------------------------------------------------

/// Compiled pages still in one code arena, under the same key and variant
/// rules as the disk cache, so a page whose bytes return to an earlier
/// version gets its earlier code back without a compile or a copy. Always
/// on; it needs only a fingerprint, not `[jitv2] cache`.
///
/// The case it was built for: IRIX rewrites the first word of the refill and
/// XTLB refill vectors on context switches (`ld k1` of the handler address,
/// or a branch past it), so physical page 0 alternates between two contents.
/// Without this, every switch recompiled the page, about 35 times a second
/// under a GL demo on the Octane.
///
/// A record holds a function pointer into the arena, so the table belongs to
/// the arena (`PagedArenaState::resident`): every arena reset (flush, page
/// pool exhaustion, a new compile pool) starts with an empty one. Memory is
/// bounded by [`RESIDENT_CAP`] records of about 4.3 KB each (the page copy a
/// hit is verified against), least recently used evicted first, and
/// [`MAX_VARIANTS`] per key as on disk. Evicting a record frees only that
/// copy; its code stays in the arena until the arena goes.
pub const RESIDENT_CAP: usize = 4096;

/// What a hit hands back: a published function and what it was compiled
/// for.
#[derive(Clone, Copy, Debug)]
pub struct Resident {
    /// The published, sealed function.
    pub func: *const (),
    pub entries: Entries,
    pub used: Entries,
    pub instr_count: u32,
    pub code_size: u32,
}

// Safety: `func` is sealed code in the arena that owns the table, callable
// from any thread for as long as that arena lives.
unsafe impl Send for Resident {}

struct ResidentRec {
    words: Box<[u32; ENTRIES_PER_PAGE]>,
    r: Resident,
    last_use: u64,
}

#[derive(Default)]
struct ResidentInner {
    pages: std::collections::HashMap<(Fingerprint, PageHash, bool), Vec<ResidentRec>>,
    len: usize,
    tick: u64,
    hits: u64,
}

/// The in-session tier for one arena. See [`RESIDENT_CAP`].
#[derive(Default)]
pub struct ResidentTable(std::sync::Mutex<ResidentInner>);

impl ResidentTable {
    /// Find a compile of exactly `words` whose entries cover `want`; the one
    /// with the most entries wins, as for [`lookup`].
    pub fn lookup(
        &self,
        fp: &Fingerprint,
        ph: &PageHash,
        fr1: bool,
        words: &[u32; ENTRIES_PER_PAGE],
        want: &Entries,
    ) -> Option<Resident> {
        let mut t = self.0.lock().unwrap();
        t.tick += 1;
        let tick = t.tick;
        let best = t.pages.get_mut(&(*fp, *ph, fr1))?
            .iter_mut()
            .filter(|r| covers(&r.r.entries, want) && *r.words == *words)
            .max_by_key(|r| count(&r.r.entries))?;
        best.last_use = tick;
        let r = best.r;
        t.hits += 1;
        Some(r)
    }

    /// Every entry a variant of this page has, for union-on-miss.
    pub fn known_entries(&self, fp: &Fingerprint, ph: &PageHash, fr1: bool) -> Entries {
        let mut all = [0u64; BITMAP_WORDS];
        if let Some(v) = self.0.lock().unwrap().pages.get(&(*fp, *ph, fr1)) {
            for r in v {
                for (a, e) in all.iter_mut().zip(&r.r.entries) {
                    *a |= e;
                }
            }
        }
        all
    }

    /// Record a function just published for `words`. A variant the new one
    /// covers is dropped, as on disk; a function already recorded (a hit
    /// being republished) only counts as a use.
    pub fn insert(&self, fp: Fingerprint, ph: PageHash, fr1: bool, words: &[u32; ENTRIES_PER_PAGE], r: Resident) {
        let mut t = self.0.lock().unwrap();
        t.tick += 1;
        let tick = t.tick;
        let v = t.pages.entry((fp, ph, fr1)).or_default();
        if let Some(same) = v.iter_mut().find(|x| x.r.func == r.func) {
            same.last_use = tick;
            return;
        }
        let before = v.len();
        v.retain(|x| !covers(&r.entries, &x.r.entries));
        let mut dropped = before - v.len();
        if v.len() >= MAX_VARIANTS {
            // This page's least recently used variant makes room.
            let i = v.iter().enumerate().min_by_key(|(_, x)| x.last_use).map(|(i, _)| i).unwrap();
            v.swap_remove(i);
            dropped += 1;
        }
        v.push(ResidentRec { words: Box::new(*words), r, last_use: tick });
        t.len = t.len + 1 - dropped;
        while t.len > RESIDENT_CAP {
            Self::evict_lru(&mut t);
        }
    }

    fn evict_lru(t: &mut ResidentInner) {
        let Some((key, i)) = t.pages.iter()
            .flat_map(|(k, v)| v.iter().enumerate().map(move |(i, r)| (r.last_use, k, i)))
            .min_by_key(|x| x.0)
            .map(|(_, k, i)| (*k, i))
        else { return };
        let v = t.pages.get_mut(&key).unwrap();
        v.swap_remove(i);
        if v.is_empty() {
            t.pages.remove(&key);
        }
        t.len -= 1;
    }

    /// `(records, hits)`, for `j2 status`.
    pub fn stats(&self) -> (usize, u64) {
        let t = self.0.lock().unwrap();
        (t.len, t.hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(entries: Entries, seed: u32) -> Blob {
        let mut words = Box::new([0u32; ENTRIES_PER_PAGE]);
        for (i, w) in words.iter_mut().enumerate() {
            *w = seed.wrapping_mul(2654435761).wrapping_add(i as u32);
        }
        Blob { entries, used: [!0; BITMAP_WORDS], instr_count: 7, align: 16, words, code: vec![0xd5, 0x03, 0x20, 0x1f, 1, 2, 3] }
    }

    #[test]
    fn roundtrip_and_tamper() {
        let fp = [1u8; 16];
        let mut e = [0u64; BITMAP_WORDS];
        e[0] = 0b1011;
        let b = blob(e, 5);
        let ph = page_hash(&b.words);
        let bytes = encode(&fp, &ph, true, &b);
        let back = decode(&bytes, &fp, &ph, true).expect("decodes");
        assert_eq!(back.entries, b.entries);
        assert_eq!(back.code, b.code);
        assert_eq!(*back.words, *b.words);
        assert_eq!(back.align, 16);
        // Wrong key, wrong FR, one flipped bit, truncation: all rejected.
        assert!(decode(&bytes, &[2u8; 16], &ph, true).is_none());
        assert!(decode(&bytes, &fp, &ph, false).is_none());
        for i in [HEADER_LEN + 3, bytes.len() - SUM_LEN - 1] {
            let mut bad = bytes.clone();
            bad[i] ^= 0x40;
            assert!(decode(&bad, &fp, &ph, true).is_none(), "flip at {i}");
        }
        assert!(decode(&bytes[..bytes.len() - 1], &fp, &ph, true).is_none());
    }

    fn resident(entries: u64, func: usize) -> Resident {
        let mut e = [0u64; BITMAP_WORDS];
        e[0] = entries;
        Resident { func: func as *const (), entries: e, used: e, instr_count: 1, code_size: 1 }
    }

    fn want(entries: u64) -> Entries {
        let mut e = [0u64; BITMAP_WORDS];
        e[0] = entries;
        e
    }

    #[test]
    fn resident_least_recently_used_record_is_evicted_at_the_cap() {
        let t = ResidentTable::default();
        let fp = [1u8; 16];
        let words = [7u32; ENTRIES_PER_PAGE];
        let key = |i: usize| -> PageHash { let mut h = [0u8; 16]; h[..8].copy_from_slice(&(i as u64).to_le_bytes()); h };
        for i in 0..RESIDENT_CAP {
            t.insert(fp, key(i), false, &words, resident(1, 0x1000 + i));
        }
        // Touch the oldest, so the second oldest is now the eviction target.
        assert!(t.lookup(&fp, &key(0), false, &words, &want(1)).is_some());
        t.insert(fp, key(RESIDENT_CAP), false, &words, resident(1, 0x9000_0000));
        assert_eq!(t.stats().0, RESIDENT_CAP, "the table stays at its cap");
        assert!(t.lookup(&fp, &key(0), false, &words, &want(1)).is_some(), "a recently used record survives");
        assert!(t.lookup(&fp, &key(1), false, &words, &want(1)).is_none(), "the least recently used one goes");
        assert!(t.lookup(&fp, &key(RESIDENT_CAP), false, &words, &want(1)).is_some());
    }

    #[test]
    fn resident_variants_follow_the_disk_rules() {
        let t = ResidentTable::default();
        let (fp, ph) = ([1u8; 16], [2u8; 16]);
        let words = [7u32; ENTRIES_PER_PAGE];
        t.insert(fp, ph, false, &words, resident(0b001, 0x1000));
        t.insert(fp, ph, false, &words, resident(0b010, 0x2000));
        assert_eq!(t.stats().0, 2);
        // A variant covering both replaces them, as `write_job` does on disk.
        t.insert(fp, ph, false, &words, resident(0b111, 0x3000));
        assert_eq!(t.stats().0, 1);
        assert_eq!(t.lookup(&fp, &ph, false, &words, &want(0b010)).unwrap().func as usize, 0x3000);
        // Other bytes under the same hash, the other FR mode, or an entry
        // the variant lacks: no hit.
        let mut other = words;
        other[5] = 8;
        assert!(t.lookup(&fp, &ph, false, &other, &want(1)).is_none());
        assert!(t.lookup(&fp, &ph, true, &words, &want(1)).is_none());
        assert!(t.lookup(&fp, &ph, false, &words, &want(0b1000)).is_none());
        // At most MAX_VARIANTS per page; the least recently used goes.
        for i in 0..MAX_VARIANTS as u32 {
            t.insert(fp, ph, false, &words, resident(1 << (8 + i), 0x10_0000 + i as usize));
        }
        assert_eq!(t.stats().0, MAX_VARIANTS);
        assert!(t.lookup(&fp, &ph, false, &words, &want(0b111)).is_none(), "the oldest variant made room");
    }

    #[test]
    fn subset_rule() {
        let mut have = [0u64; BITMAP_WORDS];
        have[1] = 0b110;
        let mut want = [0u64; BITMAP_WORDS];
        want[1] = 0b100;
        assert!(covers(&have, &want));
        want[2] = 1;
        assert!(!covers(&have, &want));
    }
}
