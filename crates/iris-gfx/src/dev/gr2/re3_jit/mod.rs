//! The RE3 span JIT (`gr2-jit`): GR2 span pipelines compiled to native code
//! with Cranelift, on the pattern of the IMPACT raster JIT (`mgras::rss_jit`).
//!
//! On GR2 the HQ2/GE7 side walks primitives and RE3 draws spans: SHADED
//! (every iterator steps per pixel, with depth, stencil and the emulator's
//! blend) and FLAT / FLAT4 (consecutive pixels, colour stepping every 5 or
//! 20). The interpreter in `re3.rs` decides, pixel by pixel, from the
//! registers, what each stage does. A shader decides it once: the state that
//! shapes the pipeline is reduced to a `SpanKey` (instruction, pixel format
//! and dither, raster op, pattern mode, WID test, depth and stencil tests,
//! blend factors), and the shader for that key is straight-line code for
//! exactly that pipeline. Everything the pipeline only reads (colours and
//! steps, pattern bits, masks, scissor, the current WID, stencil reference
//! and masks) it reads from the RE3 registers as it starts: the shader's one
//! argument is the `Re3` itself, whose context is laid out for this.
//!
//! The shaders are bit-exact with the interpreter (`re3_jit_tests.rs`
//! sweeps keys and compares whole boards).
//!
//! Shaders are compiled on one background thread and published in a
//! process-wide map; a span whose shader is not ready yet runs in the
//! interpreter. `IRIS_GR2_JIT=off|sync|async` picks the mode (`sync`
//! compiles on first use and waits, for tests and comparisons), as does the
//! monitor's `gr2 jit`. `IRIS_NO_JIT` forces it off, as it does the other
//! graphics JITs.

pub mod compiler;
pub mod key;

pub use key::{Blend, Fmt, Pat, Prim, SpanKey, Stencil, Wid};

use super::re3::{self, Re3};
use std::collections::HashMap;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, OnceLock, RwLock};

/// The shader ABI: the RE3 state, everything reached through it.
pub type ShaderFn = unsafe extern "C" fn(*mut Re3);

/// Interpreter only (a zeroed `Re3` is off).
pub const MODE_OFF: u32 = 0;
/// Compile in the background; the interpreter draws until a shader is ready.
pub const MODE_ASYNC: u32 = 1;
/// Compile on first use and wait (tests, comparisons).
pub const MODE_SYNC: u32 = 2;

/// Whether the host forbids JIT code (`IRIS_NO_JIT`): off then, whatever
/// `IRIS_GR2_JIT` or `gr2 jit` ask for.
pub fn forbidden() -> bool {
    std::env::var_os("IRIS_NO_JIT").is_some()
}

/// The mode a new board starts in: off if `forbidden`, else `IRIS_GR2_JIT`,
/// else async (off in unit tests, so they stay deterministic unless asked).
pub fn default_mode() -> u32 {
    if forbidden() {
        return MODE_OFF;
    }
    match std::env::var("IRIS_GR2_JIT").as_deref() {
        Ok("off") | Ok("0") => MODE_OFF,
        Ok("sync") => MODE_SYNC,
        Ok("async") | Ok("on") | Ok("1") => MODE_ASYNC,
        _ if cfg!(test) => MODE_OFF,
        _ => MODE_ASYNC,
    }
}

/// An instruction's last shader: valid while `epoch` matches and `f` is set.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Memo {
    epoch: u32,
    _pad: u32,
    f: usize,
    packed: u64,
}

/// The JIT's state on one board: plain data, part of `Re3`, valid zeroed.
///
/// Each instruction memoises its last shader. A change to state that shapes
/// the key (`shapes_key`, the private pixel-format, Z, stencil and blend
/// operations) bumps `epoch`, which drops them all.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct JitState {
    pub mode: u32,
    epoch: u32,
    memo: [Memo; 3],
    last: u64,
    /// Spans drawn by a shader / by the interpreter while their shader was
    /// compiling (or failed).
    pub hits: u64,
    pub misses: u64,
}

impl JitState {
    #[inline]
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// The packed key of the last shader run.
    pub fn last_key(&self) -> u64 {
        self.last
    }
}

/// RE3 registers that shape the key. Every other register a span uses is
/// read by the shader as it starts.
#[inline]
pub fn shapes_key(r: usize) -> bool {
    matches!(
        r,
        re3::REG_FUNC | re3::REG_ALIGNPAT | re3::REG_ENABPAT | re3::REG_ENABDITH | re3::REG_ENABWID | re3::REG_DEPTHFN | re3::REG_FBOPTION
    )
}

/// The key of instruction `prim` in the current state.
pub fn key_of(re3: &Re3, prim: Prim) -> SpanKey {
    let c = &re3.ctx;
    let r = &c.reg;
    let wid = if r[re3::REG_ENABWID] == 0 {
        Wid::Off
    } else if r[re3::REG_FBOPTION] & 1 != 0 {
        if r[re3::REG_DEPTHFN] & 8 != 0 { Wid::Three } else { Wid::Four }
    } else {
        Wid::Two
    };
    let pat = if r[re3::REG_ENABPAT] == 0 {
        Pat::Off
    } else if r[re3::REG_ALIGNPAT] != 0 {
        Pat::Aligned
    } else {
        Pat::Span
    };
    let st = c.stencil;
    SpanKey {
        prim,
        fmt: Fmt::of(c.pixfmt),
        dither: r[re3::REG_ENABDITH] != 0,
        rop: (r[re3::REG_FUNC] & 0xf) as u8,
        pat,
        wid,
        z: (c.zctl & 1 != 0).then_some(((c.zctl >> 1) & 7) as u8),
        stencil: (st & 1 != 0).then_some(Stencil {
            func: ((st >> 1) & 7) as u8,
            fail: ((st >> 28) & 0xf) as u8,
            zfail: ((st >> 32) & 0xf) as u8,
            zpass: ((st >> 36) & 0xf) as u8,
        }),
        blend: (c.blend & 1 != 0).then_some(Blend { src: ((c.blend >> 1) & 7) as u8, dst: ((c.blend >> 4) & 7) as u8 }),
    }
    .normalized()
}

// ── dispatch ─────────────────────────────────────────────────────────────────

/// Draw the span `ir` starts (iterators loaded) with its shader. False when
/// there is none (yet): the caller draws with the interpreter.
pub fn run(re3: &mut Re3, ir: u32) -> bool {
    if re3.jit.mode == MODE_OFF {
        return false;
    }
    let Some(prim) = Prim::of_ir(ir) else {
        return false;
    };
    let st = &mut re3.jit;
    let m = st.memo[prim as usize];
    let f = if m.f != 0 && m.epoch == st.epoch {
        st.last = m.packed;
        // SAFETY: memoised from a ShaderFn; shaders are never freed.
        unsafe { std::mem::transmute::<usize, ShaderFn>(m.f) }
    } else {
        let packed = key_of(re3, prim).pack();
        let st = &mut re3.jit;
        match store().get(packed, st.mode == MODE_SYNC) {
            Some(f) => {
                st.memo[prim as usize] = Memo { epoch: st.epoch, _pad: 0, f: f as usize, packed };
                st.last = packed;
                f
            }
            None => {
                st.misses += 1;
                return false;
            }
        }
    };
    // SAFETY: the shader reads and writes only this RE3's context, VRAM and
    // Z buffer, held mutably here, within the bounds the clip test keeps.
    unsafe { f(re3) };
    re3.jit.hits += 1;
    true
}

// ── shader store and compiler thread ─────────────────────────────────────────

enum Entry {
    Queued,
    Ready(ShaderFn, u32),
    Failed,
}

struct Request {
    key: u64,
    /// Signalled when the key is settled (sync requests).
    done: Option<SyncSender<()>>,
}

/// The process-wide shader map. Shaders depend on nothing but their key, so
/// every board shares them; the compiler thread and its code live for the
/// rest of the process.
pub struct Store {
    map: RwLock<HashMap<u64, Entry>>,
    tx: Mutex<Option<SyncSender<Request>>>,
}

pub fn store() -> &'static Store {
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(|| Store { map: RwLock::new(HashMap::new()), tx: Mutex::new(None) })
}

impl Store {
    fn sender(&self) -> SyncSender<Request> {
        let mut tx = self.tx.lock().unwrap();
        tx.get_or_insert_with(|| {
            let (tx, rx) = mpsc::sync_channel::<Request>(256);
            std::thread::Builder::new()
                .name("gr2-jit".into())
                .spawn(move || {
                    let mut comp = compiler::Compiler::new();
                    for req in rx {
                        let s = store();
                        let settled = matches!(s.map.read().unwrap().get(&req.key), Some(Entry::Ready(..) | Entry::Failed));
                        if !settled {
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| comp.compile(req.key)));
                            let entry = match r {
                                Ok(Some((f, bytes))) => Entry::Ready(f, bytes),
                                Ok(None) => Entry::Failed,
                                Err(_) => {
                                    eprintln!("GR2 JIT: compiler panicked on {}", SpanKey::unpack(req.key));
                                    comp = compiler::Compiler::new();
                                    Entry::Failed
                                }
                            };
                            s.map.write().unwrap().insert(req.key, entry);
                        }
                        if let Some(done) = req.done {
                            let _ = done.send(());
                        }
                    }
                })
                .expect("spawn gr2-jit thread");
            tx
        })
        .clone()
    }

    /// The shader for a packed key. Async: None until it is compiled (the
    /// first miss queues it). Sync: compile now if need be and wait.
    pub fn get(&self, key: u64, sync: bool) -> Option<ShaderFn> {
        match self.map.read().unwrap().get(&key) {
            Some(Entry::Ready(f, _)) => return Some(*f),
            Some(Entry::Failed) => return None,
            Some(Entry::Queued) if !sync => return None,
            _ => {}
        }
        if sync {
            let (dtx, drx) = mpsc::sync_channel(1);
            self.sender().send(Request { key, done: Some(dtx) }).ok()?;
            drx.recv().ok()?;
            return match self.map.read().unwrap().get(&key) {
                Some(Entry::Ready(f, _)) => Some(*f),
                _ => None,
            };
        }
        {
            let mut map = self.map.write().unwrap();
            if map.contains_key(&key) {
                return None;
            }
            map.insert(key, Entry::Queued);
        }
        // Never block the RE3 thread: a full queue drops the request, and a
        // later span asks again.
        if self.sender().try_send(Request { key, done: None }).is_err() {
            self.map.write().unwrap().remove(&key);
        }
        None
    }

    /// (compiled, queued, failed, code bytes) for `gr2 jit`.
    pub fn summary(&self) -> (usize, usize, usize, u64) {
        let map = self.map.read().unwrap();
        let mut s = (0, 0, 0, 0u64);
        for e in map.values() {
            match e {
                Entry::Ready(_, b) => {
                    s.0 += 1;
                    s.3 += *b as u64;
                }
                Entry::Queued => s.1 += 1,
                Entry::Failed => s.2 += 1,
            }
        }
        s
    }

    /// Compiled keys, for listings.
    pub fn compiled(&self) -> Vec<(u64, u32)> {
        let map = self.map.read().unwrap();
        let mut v: Vec<(u64, u32)> = map
            .iter()
            .filter_map(|(k, e)| match e {
                Entry::Ready(_, b) => Some((*k, *b)),
                _ => None,
            })
            .collect();
        v.sort_unstable();
        v
    }
}
