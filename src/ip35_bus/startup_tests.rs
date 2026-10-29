use super::*;
use crate::traits::*;

struct Ram(Mutex<Vec<u8>>);
macro_rules! ram_access {
    ($r:ident,$w:ident,$t:ty,$out:ident,$n:expr) => {
        fn $r(&self, a: u64) -> $out {
            let bytes = self.0.lock();
            let mut v = 0;
            for i in 0..$n {
                v = (v << 8) | bytes[(a as usize + i) % bytes.len()] as u64;
            }
            $out::ok(v as $t)
        }
        fn $w(&self, a: u64, v: $t) -> u32 {
            let mut b = self.0.lock();
            let len = b.len();
            for i in 0..$n {
                b[(a as usize + i) % len] = (v as u64 >> (8 * ($n - 1 - i))) as u8;
            }
            BUS_OK
        }
    };
}
impl BusDevice for Ram {
    ram_access!(read8, write8, u8, BusRead8, 1);
    ram_access!(read16, write16, u16, BusRead16, 2);
    ram_access!(read32, write32, u32, BusRead32, 4);
    ram_access!(read64, write64, u64, BusRead64, 8);
}
fn fixture() -> (Arc<StartupWindow>, CachedView, Arc<Ram>) {
    let window = Arc::new(StartupWindow::new(0x200000));
    let ram = Arc::new(Ram(Mutex::new(vec![0xa5; 0x20000])));
    let view = CachedView::new(ram.clone(), window.clone());
    (window, view, ram)
}
fn arm(w: &StartupWindow, v: &CachedView) {
    w.observe(2, Some(0x1fc00000), 0);
    let _scope = w.begin_access();
    assert_eq!(v.write8(WINDOW_BASE, 0x11), BUS_OK);
    drop(_scope);
}
#[test]
fn startup_all_widths_masks_blocks_and_uncached_dma_stay_distinct() {
    let (w, v, ram) = fixture();
    arm(&w, &v);
    let _scope = w.begin_access();
    assert_eq!(v.write8(WINDOW_BASE, 0x12), BUS_OK);
    assert_eq!(v.write16(WINDOW_BASE + 1, 0x3456), BUS_OK);
    assert_eq!(v.write32(WINDOW_BASE + 3, 0x789abcde), BUS_OK);
    assert_eq!(v.write64(WINDOW_BASE + 7, 0x0123456789abcdef), BUS_OK);
    assert_eq!(v.read8(WINDOW_BASE).data, 0x12);
    assert_eq!(v.read16(WINDOW_BASE + 1).data, 0x3456);
    assert_eq!(v.read32(WINDOW_BASE + 3).data, 0x789abcde);
    assert_eq!(v.read64(WINDOW_BASE + 7).data, 0x0123456789abcdef);
    assert_eq!(
        v.write_block(WINDOW_BASE + 32, &[0x1122334455667788, 0x8877665544332211]),
        BUS_OK
    );
    assert_eq!(
        v.write64_masked(WINDOW_BASE + 32, 0xffeeddccbbaa9988, 0xff000000000000ff),
        BUS_OK
    );
    let mut out = [0; 2];
    assert_eq!(v.read_block(WINDOW_BASE + 32, &mut out), BUS_OK);
    assert_eq!(out, [0xff22334455667788, 0x8877665544332211]);
    assert_eq!(ram.read64(WINDOW_BASE + 32).data, 0xa5a5a5a5a5a5a5a5);
    assert_eq!(v.dma_read64(WINDOW_BASE + 32).data, 0xa5a5a5a5a5a5a5a5);
    assert_eq!(v.dma_write64(WINDOW_BASE + 32, 0x42), BUS_OK);
    assert_eq!(v.read64(WINDOW_BASE + 32).data, out[0]);
    assert_eq!(v.read64((3 << 57) | WINDOW_BASE + 32).data, 0x42);
    assert!(v.mem_ptr(WINDOW_BASE).is_none());
}
#[test]
fn startup_cross_boundaries_preserve_private_and_backing_bytes() {
    let (w, v, ram) = fixture();
    arm(&w, &v);
    let _scope = w.begin_access();
    assert_eq!(v.write32(WINDOW_BASE - 2, 0x12345678), BUS_OK);
    assert_eq!(v.read32(WINDOW_BASE - 2).data, 0x12345678);
    assert_eq!(ram.read32(WINDOW_BASE - 2).data, 0x1234a5a5);
    assert_eq!(
        v.write32(WINDOW_BASE + WINDOW_SIZE as u64 - 2, 0xabcdef01),
        BUS_OK
    );
    assert_eq!(
        v.read32(WINDOW_BASE + WINDOW_SIZE as u64 - 2).data,
        0xabcdef01
    );
    assert_eq!(
        ram.read32(WINDOW_BASE + WINDOW_SIZE as u64 - 2).data,
        0xa5a5ef01
    );
}
#[test]
fn startup_both_exit_orders_release_without_access_and_cannot_rearm() {
    for bit_first in [false, true] {
        let (w, v, ram) = fixture();
        arm(&w, &v);
        if bit_first {
            w.observe(0, Some(0x01a01000), 0);
        } else {
            w.observe(2, Some(0x02000000), 0);
        }
        assert!(w.state.lock().private);
        w.observe(0, Some(0x02000000), 0);
        assert!(!w.state.lock().private);
        assert!(w.state.lock().left);
        w.observe(2, Some(0x1fc00000), 0);
        let _scope = w.begin_access();
        assert_eq!(v.read8(WINDOW_BASE).data, ram.read8(WINDOW_BASE).data);
        assert!(!w.state.lock().private);
    }
}
#[test]
fn startup_latch_requires_bit_home_and_cached_access() {
    let (w, v, _) = fixture();
    for (bit, pc) in [(0, Some(0x1fc00000)), (2, Some(0x2000000)), (2, None)] {
        w.observe(bit, pc, 0);
        let _scope = w.begin_access();
        v.read8(WINDOW_BASE);
        drop(_scope);
        assert!(!w.state.lock().private);
    }
    w.observe(2, Some(0x1fc00000), 0);
    v.read8(WINDOW_BASE); // debugger outside execution scope
    assert!(!w.state.lock().private);
    let _scope = w.begin_access();
    v.read8((3 << 57) | WINDOW_BASE);
    v.dma_read64(WINDOW_BASE);
    drop(_scope);
    assert!(!w.state.lock().private);
    arm(&w, &v);
    w.observe(2, Some(WINDOW_BASE), 0);
    let _scope = w.begin_access();
    v.read8(WINDOW_BASE);
    assert!(w.state.lock().left);
}
#[test]
fn startup_reset_zeroes_bytes_latches_and_counters() {
    let (w, v, _) = fixture();
    arm(&w, &v);
    w.reset();
    {
        let s = w.state.lock();
        assert!(!s.private);
        assert!(!s.left);
        assert_eq!((s.reads, s.writes), (0, 0));
    }
    w.observe(2, Some(0x1fc00000), 0);
    let _scope = w.begin_access();
    assert_eq!(v.read64(WINDOW_BASE).data, 0);
}
#[test]
fn startup_live_node_normalization_keeps_only_local_copy_home() {
    let (w, v, _) = fixture();
    arm(&w, &v);
    w.observe(0, Some((2 << 33) | 0x01a00000), 2);
    assert!(w.state.lock().private);
    w.observe(0, Some((1 << 33) | 0x01a00000), 2);
    assert!(w.state.lock().left);
}

#[test]
fn startup_debugger_other_thread_and_unwind_do_not_change_phase() {
    let (w, v, _) = fixture();
    let v = Arc::new(v);
    arm(&w, &v);
    let before = {
        let s = w.state.lock();
        (s.reads, s.writes)
    };
    let scope = w.begin_access();
    let other = v.clone();
    std::thread::spawn(move || {
        assert_eq!(other.read8(WINDOW_BASE).data, 0xa5);
        assert_eq!(other.write8(WINDOW_BASE, 0x77), BUS_OK);
    })
    .join()
    .unwrap();
    assert_eq!(v.read8(WINDOW_BASE).data, 0x11);
    drop(scope);
    let before_debug = {
        let s = w.state.lock();
        (s.reads, s.writes)
    };
    assert_eq!(before_debug, (before.0 + 1, before.1));
    assert_eq!(v.read8(WINDOW_BASE).data, 0x77);
    assert_eq!(
        {
            let s = w.state.lock();
            (s.reads, s.writes)
        },
        before_debug
    );
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _scope = w.begin_access();
        panic!("synthetic unwind");
    }));
    assert!(!w.cpu_access());
    assert_eq!(v.read8(WINDOW_BASE).data, 0x77);
}

#[test]
fn startup_selected_fuel_execution_home_has_exact_donor_payload_bounds() {
    // Pinned Fuel model flash.size, not its 8 MiB physical flash position.
    let w = StartupWindow::new(0x1686a8);
    assert!(!w.home(Some(0x1fc00000 - 1)));
    assert!(w.home(Some(0x1fc00000)));
    assert!(w.home(Some(0x1fc00000 + 1476264 - 1)));
    assert!(!w.home(Some(0x1fc00000 + 1476264)));
    assert!(!w.home(Some(0x18000000)));
    assert!(w.home(Some(0x01a00000)));
    assert!(w.home(Some(0x01bfffff)));
    assert!(!w.home(Some(0x01c00000)));
}

#[test]
#[cfg(not(feature = "opcodefusion"))]
fn startup_cached_instruction_fetch_observes_changed_forwarded_words() {
    let physical = crate::physical::Physical::new(Vec::new());
    let ram = Arc::new(Ram(Mutex::new(vec![0; 16])));
    let mut bus = crate::ip35_bus::Ip35Bus::new(physical);
    bus.map(0, 0x1fc00000, 16, ram.clone());
    let bus = Arc::new(bus);
    let window = Arc::new(StartupWindow::new(16));
    let view = Arc::new(CachedView::new(bus.clone(), window.clone()));
    let mut cpu = crate::ip34_machine::build_cpu(bus).unwrap();
    cpu.cache = crate::ip34_machine::FuelShadowCache::new(view);
    cpu.core.pc = 0xffffffff9fc00000;
    ram.write32(0, 0x24080001); // addiu t0,zero,1
    {
        let _scope = window.begin_access();
        assert_eq!(cpu.step_int(), 0);
    }
    assert_eq!(cpu.core.gpr[8], 1);
    ram.write32(0, 0x24080002); // same physical PC, changed instruction
    cpu.core.pc = 0xffffffff9fc00000;
    {
        let _scope = window.begin_access();
        assert_eq!(cpu.step_int(), 0);
    }
    assert_eq!(cpu.core.gpr[8], 2);
}

#[test]
fn startup_window_execution_test_keeps_the_raw_pc_carrier() {
    let (w, v, _) = fixture();
    arm(&w, &v);
    w.observe(2, Some((3 << 57) | WINDOW_BASE), 0);
    let _scope = w.begin_access();
    assert_eq!(v.read8(WINDOW_BASE).data, 0x11);
    assert!(!w.state.lock().left);
    w.observe(2, Some(WINDOW_BASE), 0);
    v.read8(WINDOW_BASE);
    assert!(w.state.lock().left);
}
