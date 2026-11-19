//! The C ABI, driven the way a C shell would: a recording replayed through
//! `iris_mgras_*`, a state saved and loaded through it, and the pixels
//! checked against the Rust interface on the same words.

mod support;

use std::ffi::{c_int, c_void};
use std::ptr::null_mut;

use iris_mgras::dev::mgras::engine::{Mode, Region};
use iris_mgras::dev::mgras::pixmem::Kind;
use iris_mgras::ffi::*;
use iris_mgras::interface::{VecSource, decode_chunks, encode_chunks, BoardEngine, Event, Fault, Pump, Read, Refused, StateSink, StateSource, Write};
use support::*;

unsafe extern "C" fn ram_read(user: *mut c_void, addr: u64, buf: *mut u8, len: usize, _: u32) -> c_int {
    let mem = unsafe { &*(user as *const Mem) };
    unsafe { std::ptr::copy_nonoverlapping(mem.get(addr, len).as_ptr(), buf, len) };
    0
}

unsafe extern "C" fn ram_write(user: *mut c_void, addr: u64, buf: *const u8, len: usize, _: u32) -> c_int {
    let mem = unsafe { &*(user as *const Mem) };
    mem.put(addr, unsafe { std::slice::from_raw_parts(buf, len) });
    0
}

/// The C ABI as a `BoardEngine`.
struct CEngine {
    h: *mut IrisMgras,
    mem: Box<Mem>,
}

impl CEngine {
    fn new(board: u32, mode: u32) -> CEngine {
        let mem = Box::new(Mem::default());
        let cfg = IrisMgrasConfig { abi_version: IRIS_MGRAS_ABI_VERSION, board, flags: 1, mode, queue_ops: 4096, jit: 0 };
        let ops = IrisMgrasMemOps { ram_read: Some(ram_read), ram_write: Some(ram_write), bus_read: None, bus_write: None };
        let h = unsafe { iris_mgras_new(&cfg, &ops, &*mem as *const Mem as *mut c_void, None, null_mut()) };
        assert!(!h.is_null());
        CEngine { h, mem }
    }
}

impl Drop for CEngine {
    fn drop(&mut self) {
        unsafe { iris_mgras_free(self.h) };
    }
}

fn status(c: c_int) -> Result<(), Fault> {
    if c == IRIS_MGRAS_OK { Ok(()) } else { Err(Fault(format!("status {c}"))) }
}

impl BoardEngine for CEngine {
    type Region = Region;

    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
        match unsafe { iris_mgras_write(self.h, off, bits, val) } {
            IRIS_MGRAS_OK => Write::Done,
            IRIS_MGRAS_BUSY => Write::Busy,
            _ => Write::Fault,
        }
    }
    fn push_words(&mut self, port: u32, words: &[u32]) -> usize {
        unsafe { iris_mgras_push_words(self.h, port, words.as_ptr(), words.len()) }
    }
    fn read(&mut self, off: u32, bits: u32) -> Read {
        let mut v = 0;
        match unsafe { iris_mgras_read(self.h, off, bits, &mut v) } {
            IRIS_MGRAS_OK => Read::Value(v),
            IRIS_MGRAS_BUSY => Read::Busy,
            _ => Read::Fault,
        }
    }
    fn run(&mut self, max_ops: usize) -> usize {
        unsafe { iris_mgras_run(self.h, max_ops) }
    }
    fn advance(&mut self, now_ns: u64) -> Pump {
        let mut p = IrisMgrasPump::default();
        unsafe { iris_mgras_advance(self.h, now_ns, &mut p) };
        Pump { idle: p.idle != 0, events: p.events != 0, next_deadline: (p.has_deadline != 0).then_some(p.next_deadline) }
    }
    fn wait_idle(&mut self, now_ns: u64, budget_ns: u64) -> bool {
        unsafe { iris_mgras_wait_idle(self.h, now_ns, budget_ns) == 1 }
    }
    fn drain_events(&mut self, out: &mut Vec<Event>) {
        let mut buf: Vec<IrisMgrasEvent> = Vec::with_capacity(8);
        loop {
            let n = unsafe { iris_mgras_drain_events(self.h, buf.as_mut_ptr(), 8) };
            unsafe { buf.set_len(n) };
            for e in buf.drain(..) {
                out.push(match e.kind {
                    IRIS_MGRAS_EVENT_FLAGS => Event::Flags,
                    IRIS_MGRAS_EVENT_DAMAGE => Event::Damage(iris_mgras::display::Mark {
                        store: e.store,
                        planes: e.planes,
                        rect: iris_mgras::display::Rect::new(e.x0, e.y0, e.x1, e.y1),
                    }),
                    IRIS_MGRAS_EVENT_NOTE => Event::Note(unsafe { std::ffi::CStr::from_ptr(e.text.as_ptr()) }.to_string_lossy().into()),
                    IRIS_MGRAS_EVENT_BUS_FAULT => Event::BusFault { addr: e.store, write: e.write != 0 },
                    _ => Event::DeviceFault(String::new()),
                });
            }
            if n < 8 {
                return;
            }
        }
    }
    fn observe(&mut self, r: &Region) -> Result<(), Fault> {
        status(unsafe { iris_mgras_observe(self.h, &c_region(r)) })
    }
    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool {
        unsafe { iris_mgras_access_waits(self.h, off, bits, write as u32) == 1 }
    }
    fn observe_all(&mut self) -> Result<(), Refused> {
        status(unsafe { iris_mgras_observe_all(self.h) }).map_err(|e| Refused::Faulted(e.0))
    }
    fn quiesce(&mut self) -> Result<(), Fault> {
        status(unsafe { iris_mgras_quiesce(self.h) })
    }
    fn resume(&mut self) {
        unsafe { iris_mgras_resume(self.h) };
    }
    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused> {
        status(unsafe { iris_mgras_save(self.h) }).map_err(|e| Refused::Faulted(e.0))?;
        let mut buf = vec![0u8; unsafe { iris_mgras_state_len(self.h) }];
        assert_eq!(unsafe { iris_mgras_state_copy(self.h, buf.as_mut_ptr(), buf.len()) }, buf.len());
        for c in decode_chunks(&buf).expect("the C ABI's state is chunks") {
            sink.put(c).unwrap();
        }
        Ok(())
    }
    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused> {
        let mut chunks = Vec::new();
        while let Some(c) = src.next().unwrap() {
            chunks.push(c);
        }
        let bytes = encode_chunks(&chunks).unwrap();
        status(unsafe { iris_mgras_load(self.h, bytes.as_ptr(), bytes.len()) }).map_err(|e| Refused::Mismatch(e.0))
    }
    fn reset(&mut self) -> Result<(), Refused> {
        status(unsafe { iris_mgras_reset(self.h) }).map_err(|e| Refused::Faulted(e.0))
    }
    fn fault(&self) -> Option<String> {
        let p = unsafe { iris_mgras_fault(self.h) };
        (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into())
    }
}

fn c_region(r: &Region) -> IrisMgrasRegion {
    let (x0, y0, x1, y1) = (r.rect.x0, r.rect.y0, r.rect.x1, r.rect.y1);
    IrisMgrasRegion { page: r.buffer.ptr, overlay: (r.buffer.kind == Kind::Overlay) as u32, drbsize: 0, x0, y0, x1, y1 }
}

/// The pixels of the window the sphere draws in, through the C ABI.
fn window(h: *mut IrisMgras, drbsize: u32, page: u32) -> Vec<u64> {
    let r = IrisMgrasRegion { page, overlay: 0, drbsize, x0: 90, y0: 820, x1: 230, y1: 930 };
    assert_eq!(unsafe { iris_mgras_observe(h, &r) }, IRIS_MGRAS_OK);
    (r.y0..=r.y1)
        .flat_map(|y| (r.x0..=r.x1).map(move |x| (x, y)))
        .map(|(x, y)| {
            let mut v = 0;
            assert_eq!(unsafe { iris_mgras_pixel(h, &r, x as u32, y as u32, &mut v) }, IRIS_MGRAS_OK);
            v
        })
        .collect()
}

#[test]
fn a_c_shell_replays_a_recording_and_saves_it() {
    for mode in [0, 1] {
        let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-swap-tmesh-sphere").unwrap().1);
        let mut front = Front::new();
        let mut c = CEngine::new(0, mode);
        for cmd in &rep.cmds {
            match cmd {
                Cmd::Write { bits, off, val } => front.write(&mut c, *bits, *off, *val),
                Cmd::Dma { addr, bytes } => c.mem.put(*addr, bytes),
                Cmd::Read { .. } => {}
            }
        }
        drive(&mut c);
        let drb = get(&mut c, iris_mgras::dev::mgras::hq3::host::RASTER + 4 * 0x16E, 32) as u32;
        let page = 0x240;
        let pixels = window(c.h, drb, page);
        assert!(pixels.iter().any(|&p| p != 0), "the sphere window is empty");

        // The same words through the Rust interface.
        let mut r = Player::new(config(&rep.head, Mode::Inline, None), Drive::Each);
        rep.cmds.iter().for_each(|cmd| r.apply(cmd));
        let b = iris_mgras::dev::mgras::pixmem::Buffer::new(page, Kind::Wide, drb);
        let want: Vec<u64> = (820..=930).flat_map(|y| (90..=230).map(move |x| (x, y))).map(|(x, y)| r.e.pixel(&b, x, y)).collect();
        assert_eq!(pixels, want, "mode {mode}: the C ABI stored other words");

        // Save through the C ABI, load into a second engine, same window.
        c.quiesce().unwrap();
        let mut chunks = Vec::new();
        c.save(&mut chunks).unwrap();
        let mut d = CEngine::new(0, mode);
        d.quiesce().unwrap();
        d.load(&mut VecSource(chunks.clone().into_iter())).unwrap();
        assert_eq!(window(d.h, drb, page), pixels);
        chunks.retain(|k| k.name != "hq3");
        assert!(d.load(&mut VecSource(chunks.into_iter())).is_err());
        assert_eq!(window(d.h, drb, page), pixels, "a refused load changed the engine");
    }
}
