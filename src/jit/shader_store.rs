//! Process-wide cache of Cranelift raster shaders, shared by the JIT raster
//! engines (GR2 span pipelines, IMPACT primitive pipelines).
//!
//! A shader depends only on its packed key, so every board shares one map.
//! Each board names its compiler through [`Compile`] and keeps one static
//! `Store` for the process. A background thread, started on first use,
//! compiles the keys it is asked for; the map keeps every result for the rest
//! of the process, since shaders are never freed.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, RwLock};

/// A board's shader compiler, run on the store's thread.
pub trait Compile: 'static {
    /// The compiled shader: a plain `unsafe extern "C"` function pointer.
    type Shader: Copy + Send + Sync + 'static;
    /// Names the compiler thread.
    const THREAD: &'static str;
    /// Prefixes the message printed when the compiler panics.
    const LABEL: &'static str;

    fn fresh() -> Self;
    /// The shader and its code size in bytes, or None if the key cannot be compiled.
    fn compile_key(&mut self, key: u64) -> Option<(Self::Shader, u32)>;
    /// The key as the board prints it.
    fn key_text(key: u64) -> String;
}

enum Entry<F> {
    Queued,
    Ready(F, u32),
    Failed,
}

struct Request {
    key: u64,
    /// Signalled when the key is settled (sync requests).
    done: Option<SyncSender<()>>,
}

pub struct Store<C: Compile> {
    map: RwLock<HashMap<u64, Entry<C::Shader>>>,
    tx: Mutex<Option<SyncSender<Request>>>,
    _compiler: PhantomData<fn() -> C>,
}

impl<C: Compile> Default for Store<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Compile> Store<C> {
    pub fn new() -> Self {
        Store { map: RwLock::new(HashMap::new()), tx: Mutex::new(None), _compiler: PhantomData }
    }

    fn sender(&'static self) -> SyncSender<Request> {
        let mut tx = self.tx.lock().unwrap();
        tx.get_or_insert_with(|| {
            let (tx, rx) = mpsc::sync_channel::<Request>(256);
            std::thread::Builder::new()
                .name(C::THREAD.into())
                .spawn(move || {
                    let mut comp = C::fresh();
                    for req in rx {
                        let settled = matches!(self.map.read().unwrap().get(&req.key), Some(Entry::Ready(..) | Entry::Failed));
                        if !settled {
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| comp.compile_key(req.key)));
                            let entry = match r {
                                Ok(Some((f, bytes))) => Entry::Ready(f, bytes),
                                Ok(None) => Entry::Failed,
                                Err(_) => {
                                    eprintln!("{}: compiler panicked on {}", C::LABEL, C::key_text(req.key));
                                    comp = C::fresh();
                                    Entry::Failed
                                }
                            };
                            self.map.write().unwrap().insert(req.key, entry);
                        }
                        if let Some(done) = req.done {
                            let _ = done.send(());
                        }
                    }
                })
                .unwrap_or_else(|e| panic!("spawn {} thread: {e}", C::THREAD));
            tx
        })
        .clone()
    }

    /// The shader for a packed key. Async: None until it is compiled (the
    /// first miss queues it). Sync: compile now if need be and wait.
    pub fn get(&'static self, key: u64, sync: bool) -> Option<C::Shader> {
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
        // Never block the caller's render thread: a full queue drops the
        // request, and a later span or primitive asks again.
        if self.sender().try_send(Request { key, done: None }).is_err() {
            self.map.write().unwrap().remove(&key);
        }
        None
    }

    /// (compiled, queued, failed, code bytes) for the board's `jit` command.
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
