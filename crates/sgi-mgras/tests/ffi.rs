//! The C ABI, driven the way a C shell would: a recording replayed through
//! `sgi_mgras_*`, a state saved and loaded through it, and the pixels
//! checked against the Rust interface on the same words.

mod support;

use std::ffi::{c_char, c_int, c_void};
use std::ptr::null_mut;

use sgi_mgras::dev::mgras::engine::{Mode, Region};
use sgi_mgras::dev::mgras::pixmem::Kind;
use sgi_mgras::ffi::*;
use sgi_engine::ffi::*;
use sgi_engine::{BoardEngine, Chunk, Event, Fault, Idle, Pump, Read, Refused, StallOn, StateSink, StateSource, VecSource, Write};
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
    h: *mut IrisEngine,
    mem: Box<Mem>,
}

fn mgras_config(board: u32, mode: u32) -> IrisMgrasConfig {
    IrisMgrasConfig {
        common: IrisEngineConfig {
            abi_version: IRIS_ENGINE_ABI_VERSION,
            struct_size: std::mem::size_of::<IrisMgrasConfig>() as u32,
            mode,
            queue_ops: 4096,
            gpu: 0,
            gpu_backend: std::ptr::null(),
            lent_ptr: null_mut(),
            lent_len: 0,
            reserved: [0; 4],
        },
        board,
        flags: 1,
        jit: 0,
        bdvers: [0; 2],
        cmap_rev: [0; 2],
        ge_revision: 0,
        pp1_revision: 0,
    }
}

fn host_of(mem: &Mem) -> IrisEngineHost {
    IrisEngineHost {
        abi_version: IRIS_ENGINE_ABI_VERSION,
        struct_size: std::mem::size_of::<IrisEngineHost>() as u32,
        user: mem as *const Mem as *mut c_void,
        ram_read: Some(ram_read),
        ram_write: Some(ram_write),
        bus_read: None,
        bus_write: None,
        notify: None,
    }
}

impl CEngine {
    fn new(board: u32, mode: u32) -> CEngine {
        let mem = Box::new(Mem::default());
        let h = unsafe { sgi_mgras_new(&mgras_config(board, mode), &host_of(&mem)) };
        assert!(!h.is_null());
        CEngine { h, mem }
    }
}

impl Drop for CEngine {
    fn drop(&mut self) {
        unsafe { iris_engine_free(self.h) };
    }
}

fn status(c: c_int) -> Result<(), Fault> {
    if c == IRIS_ENGINE_OK { Ok(()) } else { Err(Fault(format!("status {c}"))) }
}

fn idle_of(i: &IrisEngineIdle) -> Idle {
    match i.state {
        IRIS_ENGINE_IDLE => Idle::Yes,
        IRIS_ENGINE_WORKING => Idle::Working,
        IRIS_ENGINE_STALLED_FRAME => Idle::Stalled(StallOn::Frame),
        IRIS_ENGINE_STALLED_TIME => Idle::Stalled(StallOn::Time(i.stall_time)),
        _ => Idle::Faulted,
    }
}

fn refusal(c: c_int, h: *mut IrisEngine) -> Refused {
    let why = unsafe { iris_engine_last_error(h) };
    let why = if why.is_null() { String::new() } else { unsafe { std::ffi::CStr::from_ptr(why) }.to_string_lossy().into_owned() };
    match c {
        IRIS_ENGINE_NOT_QUIESCED => Refused::NotQuiesced,
        IRIS_ENGINE_MIDWAY => Refused::Midway(why),
        IRIS_ENGINE_MISMATCH => Refused::Mismatch(why),
        _ => Refused::Faulted(why),
    }
}

impl BoardEngine for CEngine {
    type Region = Region;

    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
        match unsafe { iris_engine_write(self.h, off, bits, val) } {
            IRIS_ENGINE_OK => Write::Done,
            IRIS_ENGINE_BUSY => Write::Busy,
            _ => Write::Fault,
        }
    }
    fn push_words(&mut self, port: u32, words: &[u32]) -> usize {
        unsafe { iris_engine_push_words(self.h, port, words.as_ptr(), words.len()) }
    }
    fn read(&mut self, off: u32, bits: u32) -> Read {
        let mut v = 0;
        match unsafe { iris_engine_read(self.h, off, bits, &mut v) } {
            IRIS_ENGINE_OK => Read::Value(v),
            IRIS_ENGINE_BUSY => Read::Busy,
            _ => Read::Fault,
        }
    }
    fn run(&mut self, max_ops: usize) -> usize {
        unsafe { iris_engine_run(self.h, max_ops) }
    }
    fn advance(&mut self, now_ns: u64) -> Pump {
        let mut p = IrisEnginePump::default();
        unsafe { iris_engine_advance(self.h, now_ns, &mut p) };
        Pump { idle: idle_of(&p.idle), events: p.events != 0, next_deadline: (p.has_deadline != 0).then_some(p.next_deadline), irq_levels: p.irq_levels }
    }
    fn wait_idle(&mut self, now_ns: u64, budget_ns: u64) -> Idle {
        let mut i = IrisEngineIdle::default();
        unsafe { iris_engine_wait_idle(self.h, now_ns, budget_ns, &mut i) };
        idle_of(&i)
    }
    fn irq_levels(&self) -> u64 {
        unsafe { iris_engine_irq_levels(self.h) }
    }
    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize) {
        let mut buf: Vec<IrisEngineEvent> = Vec::with_capacity(8);
        let mut left = max;
        while left > 0 {
            let want = left.min(8);
            let n = unsafe { iris_engine_drain_events(self.h, buf.as_mut_ptr(), want) };
            unsafe { buf.set_len(n) };
            left -= n;
            for e in buf.drain(..) {
                out.push(match e.kind {
                    IRIS_ENGINE_EVENT_IRQ => Event::Irq,
                    IRIS_ENGINE_EVENT_DAMAGE => {
                        let d = unsafe { e.data.damage };
                        Event::Damage(sgi_engine::Mark { store: d.store, planes: d.planes, rect: sgi_engine::Rect::new(d.x0, d.y0, d.x1, d.y1) })
                    }
                    IRIS_ENGINE_EVENT_NOTE => Event::Note(unsafe { std::ffi::CStr::from_ptr(e.data.text.as_ptr()) }.to_string_lossy().into()),
                    IRIS_ENGINE_EVENT_BUS_FAULT => {
                        let b = unsafe { e.data.bus_fault };
                        Event::BusFault { addr: b.addr, write: b.write != 0 }
                    }
                    IRIS_ENGINE_EVENT_DEVICE_FAULT => Event::DeviceFault(unsafe { std::ffi::CStr::from_ptr(e.data.text.as_ptr()) }.to_string_lossy().into()),
                    k => panic!("event kind {k}"),
                });
            }
            if n < want {
                return;
            }
        }
    }
    fn observe(&mut self, r: &Region) -> Result<(), Fault> {
        status(unsafe { sgi_mgras_observe(self.h, &c_region(r)) })
    }
    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool {
        unsafe { iris_engine_access_waits(self.h, off, bits, write as u32) == 1 }
    }
    fn observe_all(&mut self) -> Result<(), Refused> {
        match unsafe { iris_engine_observe_all(self.h) } {
            IRIS_ENGINE_OK => Ok(()),
            c => Err(refusal(c, self.h)),
        }
    }
    fn quiesce(&mut self, budget_ns: u64) -> Result<(), Refused> {
        match unsafe { iris_engine_quiesce(self.h, budget_ns) } {
            IRIS_ENGINE_OK => Ok(()),
            c => Err(refusal(c, self.h)),
        }
    }
    fn resume(&mut self) {
        unsafe { iris_engine_resume(self.h) };
    }
    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused> {
        // The sink is reached through a pointer to a fat pointer.
        unsafe extern "C" fn put(user: *mut c_void, name: *const c_char, index: u64, bytes: *const u8, len: usize) -> c_int {
            let sink = unsafe { &mut *(user as *mut &mut dyn StateSink) };
            let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned();
            let bytes = unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec();
            sink.put(Chunk { name, index, bytes }).is_err() as c_int
        }
        let mut sink = sink;
        match unsafe { iris_engine_save(self.h, Some(put), &mut sink as *mut &mut dyn StateSink as *mut c_void) } {
            IRIS_ENGINE_OK => Ok(()),
            c => Err(refusal(c, self.h)),
        }
    }
    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused> {
        struct Cur<'a>(&'a mut dyn StateSource, Vec<u8>);
        unsafe extern "C" fn next(user: *mut c_void, c: *mut IrisEngineChunk) -> c_int {
            let cur = unsafe { &mut *(user as *mut Cur) };
            let Some(ch) = cur.0.next().unwrap() else { return 0 };
            cur.1 = ch.bytes;
            let c = unsafe { &mut *c };
            for (d, b) in c.name.iter_mut().zip(ch.name.bytes().chain([0])) {
                *d = b as c_char;
            }
            (c.index, c.bytes, c.len) = (ch.index, cur.1.as_ptr(), cur.1.len());
            1
        }
        let mut cur = Cur(src, Vec::new());
        match unsafe { iris_engine_load(self.h, Some(next), &mut cur as *mut Cur as *mut c_void) } {
            IRIS_ENGINE_OK => Ok(()),
            c => Err(refusal(c, self.h)),
        }
    }
    fn state_version(&self) -> u32 {
        unsafe { iris_engine_state_version(self.h) }
    }
    fn reset(&mut self) -> Result<(), Refused> {
        match unsafe { iris_engine_reset(self.h) } {
            IRIS_ENGINE_OK => Ok(()),
            c => Err(refusal(c, self.h)),
        }
    }
    fn fault(&self) -> Option<String> {
        let p = unsafe { iris_engine_fault(self.h) };
        (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into())
    }
    fn fail(&self, _: &str) {
        unreachable!("the C ABI has no entry that stops the engine from outside");
    }
}

fn c_region(r: &Region) -> IrisMgrasRegion {
    let (x0, y0, x1, y1) = (r.rect.x0, r.rect.y0, r.rect.x1, r.rect.y1);
    IrisMgrasRegion { page: r.buffer.ptr, overlay: (r.buffer.kind == Kind::Overlay) as u32, drbsize: 0, x0, y0, x1, y1 }
}

/// The pixels of the window the sphere draws in, through the C ABI.
fn window(h: *mut IrisEngine, drbsize: u32, page: u32) -> Vec<u64> {
    let r = IrisMgrasRegion { page, overlay: 0, drbsize, x0: 90, y0: 820, x1: 230, y1: 930 };
    assert_eq!(unsafe { sgi_mgras_observe(h, &r) }, IRIS_ENGINE_OK);
    (r.y0..=r.y1)
        .flat_map(|y| (r.x0..=r.x1).map(move |x| (x, y)))
        .map(|(x, y)| {
            let mut v = 0;
            assert_eq!(unsafe { sgi_mgras_pixel(h, &r, x as u32, y as u32, &mut v) }, IRIS_ENGINE_OK);
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
        let drb = get(&mut c, sgi_mgras::dev::mgras::hq3::host::RASTER + 4 * 0x16E, 32) as u32;
        let page = 0x240;
        let pixels = window(c.h, drb, page);
        assert!(pixels.iter().any(|&p| p != 0), "the sphere window is empty");

        // The same words through the Rust interface.
        let mut r = Player::new(config(&rep.head, Mode::Inline, None), Drive::Each);
        rep.cmds.iter().for_each(|cmd| r.apply(cmd));
        let b = sgi_mgras::dev::mgras::pixmem::Buffer::new(page, Kind::Wide, drb);
        let want: Vec<u64> = (820..=930).flat_map(|y| (90..=230).map(move |x| (x, y))).map(|(x, y)| r.e.pixel(&b, x, y)).collect();
        assert_eq!(pixels, want, "mode {mode}: the C ABI stored other words");

        // A row in one call is the same words, the low 32 bits of each pixel; the overlay's are 9 bits.
        let region = IrisMgrasRegion { page, overlay: 0, drbsize: drb, x0: 0, y0: 0, x1: 0, y1: 0 };
        let mut row = vec![0u32; 141];
        assert_eq!(unsafe { sgi_mgras_read_row(c.h, &region, 875, 90, 231, row.as_mut_ptr()) }, IRIS_ENGINE_OK);
        let at = (875 - 820) * 141;
        assert!(row.iter().zip(&pixels[at..at + 141]).all(|(r, p)| *r == *p as u32), "mode {mode}: a row differs from its pixels");
        assert!(row.iter().any(|&p| p != 0));
        let overlay = IrisMgrasRegion { overlay: 1, page: 0x1C0, ..region };
        assert_eq!(unsafe { sgi_mgras_read_row(c.h, &overlay, 875, 90, 231, row.as_mut_ptr()) }, IRIS_ENGINE_OK);
        for (i, r) in row.iter().enumerate() {
            let mut v = 0;
            assert_eq!(unsafe { sgi_mgras_pixel(c.h, &overlay, 90 + i as u32, 875, &mut v) }, IRIS_ENGINE_OK);
            assert_eq!(*r as u64, v, "overlay pixel {i}");
        }
        // Ranges outside the planes, and no buffer, are refused and read nothing.
        let mut one = [0xAAu32; 1];
        for (y, x0, x1) in [(2048, 0, 1), (0, 0, 2049), (0, 5, 4)] {
            assert_eq!(unsafe { sgi_mgras_read_row(c.h, &region, y, x0, x1, one.as_mut_ptr()) }, IRIS_ENGINE_ERROR, "({y}, {x0}, {x1})");
        }
        assert_eq!(unsafe { sgi_mgras_read_row(c.h, &region, 0, 3, 3, one.as_mut_ptr()) }, IRIS_ENGINE_OK, "an empty row");
        assert_eq!(unsafe { sgi_mgras_read_row(c.h, std::ptr::null(), 0, 0, 1, one.as_mut_ptr()) }, IRIS_ENGINE_ERROR);
        assert_eq!(one, [0xAA]);

        // Save through the C ABI, load into a second engine, same window.
        c.quiesce(u64::MAX).unwrap();
        let mut chunks = Vec::new();
        c.save(&mut chunks).unwrap();
        let mut d = CEngine::new(0, mode);
        d.quiesce(u64::MAX).unwrap();
        d.load(&mut VecSource::new(chunks.clone())).unwrap();
        assert_eq!(window(d.h, drb, page), pixels);
        chunks.retain(|k| k.name != "hq3");
        assert!(matches!(d.load(&mut VecSource::new(chunks)), Err(Refused::Mismatch(_))));
        assert_eq!(window(d.h, drb, page), pixels, "a refused load changed the engine");
    }
}

unsafe extern "C" fn bus_read(user: *mut c_void, dev: u32, crs: u32, _width: u32, data: *mut u32) -> c_int {
    // Slot 20 block 0 is an option board's probe byte; nothing else is its.
    if dev == 20 && crs == 0 {
        unsafe { *data = *(user as *const u32) };
        return 1;
    }
    0
}

unsafe extern "C" fn bus_write(user: *mut c_void, dev: u32, _crs: u32, _width: u32, data: u32) -> c_int {
    if dev == 20 {
        unsafe { *(user as *mut u32) = data };
        return 1;
    }
    0
}

/// The C ABI's display-bus call-out: an option board's slot is answered by the shell's callbacks, and
/// detaching returns it to the engine's scratch register.
#[test]
fn a_c_shell_attaches_a_device_to_the_display_bus() {
    let mut probe = 0xa5u32;
    let c = CEngine::new(0, 0);
    let slot = |dev: u32, crs: u32| 0x60000 + dev * 0x400 + (crs << 7);
    let mut v = 0u64;
    assert_eq!(unsafe { iris_engine_read(c.h, slot(20, 0), 32, &mut v) }, IRIS_ENGINE_OK);
    assert_eq!(v, 0, "no device: the engine's scratch register");
    let user = (&mut probe) as *mut u32 as *mut c_void;
    assert_eq!(unsafe { sgi_mgras_set_display_bus(c.h, user, Some(bus_read), Some(bus_write)) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_read(c.h, slot(20, 0), 32, &mut v) }, IRIS_ENGINE_OK);
    assert_eq!(v, 0xa5, "the shell's probe byte");
    assert_eq!(unsafe { iris_engine_write(c.h, slot(20, 1), 32, 0x77) }, IRIS_ENGINE_OK);
    assert_eq!(probe, 0x77, "the store went to the shell");
    assert_eq!(unsafe { sgi_mgras_set_display_bus(c.h, null_mut(), None, None) }, IRIS_ENGINE_OK);
    assert_eq!(unsafe { iris_engine_read(c.h, slot(20, 0), 32, &mut v) }, IRIS_ENGINE_OK);
    assert_eq!(v, 0, "detached");
}
