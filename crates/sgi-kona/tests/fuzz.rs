//! Random command words and bus writes, seeded: no input a guest can make may panic the engine,
//! hang it, or take unbounded memory. The page takes plain words and a word whose low half is a
//! selector opens a command, so the generator mixes selectors (GL and kernel) with edge-valued
//! operands, after the context activation that opens the GL grammar.

use sgi_engine::fuzz::{progress, LimitAlloc, Rng};
use sgi_engine::{BoardEngine, HostMem, MemFault, Source, Swap, Write};
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{KonaEngine, Options};
use sgi_kona::dev::kona::ident::KonaConfig;

#[global_allocator]
static MEMORY: LimitAlloc = LimitAlloc::new(3 << 30);

struct Noise(u64);

impl HostMem for Noise {
    fn ram_read(&self, addr: u64, out: &mut [u8], _: Swap) -> Result<(), MemFault> {
        if addr >> 40 != 0 {
            return Err(MemFault);
        }
        for (i, b) in out.iter_mut().enumerate() {
            let x = ((addr + i as u64) ^ self.0).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            *b = (x >> 29) as u8;
        }
        Ok(())
    }

    fn ram_write(&self, addr: u64, _: &[u8], _: Swap) -> Result<(), MemFault> {
        if addr >> 40 != 0 { Err(MemFault) } else { Ok(()) }
    }
}

fn word(r: &mut Rng) -> u32 {
    match r.below(10) {
        // A GL or kernel selector, with data in the top half as the short forms carry.
        0..=3 => r.below(0x200) as u32 | if r.below(4) == 0 { r.below(0x10000) as u32 } else { 0 } << 16,
        4 => 0x39,
        5 => 1,
        _ => r.edge(),
    }
}

fn run(seed: u64, threads: usize, ops: usize) {
    let mut r = Rng::new(seed);
    let config = KonaConfig::ir2(1).unwrap().with_pool(threads, 32);
    let mut e = KonaEngine::new(config, Noise(seed), Options::default(), None).unwrap();
    e.set_source(Source { id: 1, charged: true });
    // The context activation that opens the GL grammar: initialise, page, offset, length.
    for w in [CONTEXT_INITIALISE, 0x10, 0, 0x100] {
        let _ = e.write(COMMAND_PAGE, 32, w as u64);
    }
    let mut now = 0u64;
    for i in 0..ops {
        progress(120, || format!("seed {seed:#x} threads {threads} op {i} rng {:#x}", r.0));
        match r.below(8) {
            0..=4 => {
                for _ in 0..1 + r.below(16) {
                    let v = word(&mut r);
                    let _ = e.write(COMMAND_PAGE, 32, v as u64);
                }
            }
            5 => {
                let _ = e.write(COMMAND_PAGE, 64, r.next());
            }
            6 => {
                let off = (r.below(0x10_0000) as u32) & !3;
                let _ = e.write(off, [32, 64][r.below(2) as usize], r.next());
            }
            _ => {
                let off = (r.below(0x10_0000) as u32) & !3;
                let _ = e.read(off, [32, 64][r.below(2) as usize]);
            }
        }
        if i % 16 == 0 {
            now += 1_000_000;
            let _ = e.run(1 << 16);
            let _ = e.advance(now);
            e.frame(now);
            let _ = e.wait_idle(now, 1_000_000_000);
            let mut ev = Vec::new();
            e.drain_events(&mut ev, usize::MAX);
        }
    }
    let _ = e.wait_idle(now, 10_000_000_000);
    if let Some(f) = e.fault() {
        assert!(!f.contains("panicked"), "seed {seed:#x} threads {threads}: {f}");
    }
    assert_eq!(e.write(COMMAND_PAGE, 32, 0) == Write::Fault, e.fault().is_some());
}

#[test]
fn random_words_and_stores_never_panic_inline() {
    for seed in 1..=4u64 {
        run(seed * 0x9e37_79b9, 0, 500);
    }
}

#[test]
fn random_words_and_stores_never_panic_with_tile_workers() {
    for seed in 1..=2u64 {
        run(seed * 0x1234_5679, 2, 500);
    }
}

mod config {
    use std::ffi::{c_int, c_void, CString};
    use std::ptr::{null, null_mut};

    use sgi_engine::ffi::*;
    use sgi_engine::fuzz::{progress, Rng};
    use sgi_kona::ffi::*;

    unsafe extern "C" fn ram_read(_: *mut c_void, _: u64, buf: *mut u8, len: usize, _: u32) -> c_int {
        unsafe { std::ptr::write_bytes(buf, 0, len) };
        0
    }

    unsafe extern "C" fn ram_write(_: *mut c_void, _: u64, _: *const u8, _: usize, _: u32) -> c_int {
        0
    }

    /// Random values for every field of the constructor's configuration, pointers drawn from
    /// strings that are valid, invalid or absent: it must refuse or build, and the engine it builds
    /// must run a few stores, without spinning or taking guest-sized memory.
    #[test]
    fn a_random_configuration_is_refused_or_built_without_spinning() {
        let words = ["ir2", "ir2e", "ir3", "ir9", "", "ge14-2", "ge16-4", "ge14-4", "rm9", "rm10", "dg5-2", "dg5-8", "dg5-2-gvo", "small", "medium", "030-1067-001", "030-9999-001", "\u{e9}"];
        let strings: Vec<CString> = words.iter().map(|w| CString::new(*w).unwrap()).collect();
        let host = IrisEngineHost { abi_version: IRIS_ENGINE_ABI_VERSION, struct_size: std::mem::size_of::<IrisEngineHost>() as u32, user: null_mut(), ram_read: Some(ram_read), ram_write: Some(ram_write), bus_read: None, bus_write: None, notify: None };
        let mut r = Rng::new(0xc0ffee);
        let mut built = 0;
        for case in 0..600 {
            progress(120, || format!("configuration {case} rng {:#x}", r.0));
            let pick = |r: &mut Rng| -> *const std::ffi::c_char { if r.below(4) == 0 { null() } else { strings[r.below(strings.len() as u64) as usize].as_ptr() } };
            let small = |r: &mut Rng| if r.below(2) == 0 { r.below(5) as u32 } else { r.edge() };
            // A valid pipe with a few fields replaced, so most cases get past the first refusal.
            let mut field = |r: &mut Rng| r.below(5) == 0;
            // Part lists: a few entries, repeats among them; now and then a count past the array.
            let list: Vec<*const std::ffi::c_char> = (0..r.below(4)).map(|_| pick(&mut r)).collect();
            let mut c = IrisKonaConfig {
                common: IrisEngineConfig { abi_version: IRIS_ENGINE_ABI_VERSION, struct_size: std::mem::size_of::<IrisKonaConfig>() as u32, mode: 0, queue_ops: 0, gpu: 0, gpu_backend: null(), lent_ptr: null_mut(), lent_len: 0, reserved: [0; 4] },
                product: strings[0].as_ptr(),
                rm_count: 1,
                identity_index: 0,
                host_widget: 0,
                widget: 0,
                probe: 0,
                managed_width: 0,
                managed_height: 0,
                pool_threads: 0,
                pool_tile: 32,
                geometry: null(),
                rm_type: null(),
                display: null(),
                pixel_depth: null(),
                texture_mb: 0,
                channels: 0,
                ktown2: 0,
                parts: null(),
                num_parts: 0,
                shared_identity: 0,
                geometry_serial: 0,
                user: null_mut(),
                flash_image: null(),
                flash_len: 0,
                flash_write: None,
                dma_widget: None,
            };
            if field(&mut r) { c.product = pick(&mut r) }
            if field(&mut r) { c.rm_count = small(&mut r) }
            if field(&mut r) { c.identity_index = small(&mut r) }
            if field(&mut r) { c.host_widget = small(&mut r) }
            if field(&mut r) { c.widget = small(&mut r) }
            if field(&mut r) { c.probe = r.edge() }
            if field(&mut r) { c.managed_width = r.edge() }
            if field(&mut r) { c.managed_height = r.edge() }
            if field(&mut r) { c.pool_threads = r.edge() }
            if field(&mut r) { c.pool_tile = r.edge() }
            if field(&mut r) { c.geometry = pick(&mut r) }
            if field(&mut r) { c.rm_type = pick(&mut r) }
            if field(&mut r) { c.display = pick(&mut r) }
            if field(&mut r) { c.pixel_depth = pick(&mut r) }
            if field(&mut r) { c.texture_mb = small(&mut r) }
            if field(&mut r) { c.channels = small(&mut r) }
            if field(&mut r) { c.ktown2 = small(&mut r) }
            if field(&mut r) { c.shared_identity = small(&mut r) }
            if field(&mut r) { c.geometry_serial = r.next() }
            if field(&mut r) { c.flash_len = r.edge() }
            if field(&mut r) {
                c.parts = if list.is_empty() { null() } else { list.as_ptr() };
                c.num_parts = if r.below(4) == 0 { r.edge() } else { list.len() as u32 };
            }
            // A count past the array reads off its end: the contract is a count the array has.
            if !c.parts.is_null() && c.num_parts as usize > list.len() {
                c.num_parts = list.len() as u32;
            }
            let h = unsafe { sgi_kona_new(&c, &host) };
            if h.is_null() {
                continue;
            }
            built += 1;
            for w in [1u32, 3, 1, 0xff << 2, 0, 0, 1, 2, 1, 1, 6, 100, 50] {
                unsafe { iris_engine_write(h, 0xc10000, 32, w as u64) };
            }
            unsafe { iris_engine_quiesce(h, 1_000_000_000) };
            unsafe { iris_engine_free(h) };
        }
        assert!(built > 0, "some configurations are valid");
    }
}
