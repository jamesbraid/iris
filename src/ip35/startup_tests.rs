use super::*;
use crate::traits::*;

struct Ram(Mutex<Vec<u8>>);
impl BusDevice for Ram {
    fn read8(&self, address: u64) -> BusRead8 {
        let bytes = self.0.lock();
        BusRead8::ok(bytes[address as usize % bytes.len()])
    }
    fn write8(&self, address: u64, value: u8) -> u32 {
        let mut bytes = self.0.lock();
        let index = address as usize % bytes.len();
        bytes[index] = value;
        BUS_OK
    }
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
fn startup_release_runs_once_outside_state_and_reset_rearms() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for fetch_exit in [false, true] {
        let releases = Arc::new(AtomicUsize::new(0));
        let count = releases.clone();
        let window = Arc::new_cyclic(|weak: &std::sync::Weak<StartupWindow>| {
            let weak = weak.clone();
            StartupWindow::with_release(0x200000, Arc::new(move || {
                let window = weak.upgrade().unwrap();
                assert!(window.state.try_lock().is_some());
                assert!(!window.observing());
                count.fetch_add(1, Ordering::Relaxed);
            }))
        });
        let ram = Arc::new(Ram(Mutex::new(vec![0xa5; 0x20000])));
        let view = CachedView::new(ram, window.clone());
        for expected in [1, 2] {
            assert!(window.observing());
            window.observe(2, Some(0x1fc00000), 0);
            {
                let _scope = window.begin_access();
                assert_eq!(view.read8(WINDOW_BASE).data, 0, "reset clears private bytes");
            }
            arm(&window, &view);
            if fetch_exit {
                window.observe(2, Some((3 << 57) | WINDOW_BASE), 0);
                {
                    let _scope = window.begin_access();
                    assert_eq!(view.read8(WINDOW_BASE).data, 0x11);
                }
                assert!(window.observing(), "an uncached PC does not end the private phase");
                window.observe(2, Some(WINDOW_BASE), 0);
                let _scope = window.begin_access();
                view.read8(WINDOW_BASE);
            } else {
                window.observe(0, Some(0x200000), 0);
            }
            assert!(!window.observing());
            assert_eq!(releases.load(Ordering::Relaxed), expected);
            window.observe(0, Some(0x200000), 0);
            assert_eq!(releases.load(Ordering::Relaxed), expected);
            window.reset();
        }
    }
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
    drop(_scope);
    assert!(!w.state.lock().private);
    arm(&w, &v);
    w.observe(2, Some(WINDOW_BASE), 0);
    let _scope = w.begin_access();
    v.read8(WINDOW_BASE);
    assert!(w.state.lock().left);
}

#[test]
fn startup_debugger_other_thread_and_unwind_do_not_change_phase() {
    let (w, v, _) = fixture();
    let v = Arc::new(v);
    arm(&w, &v);
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
    assert_eq!(v.read8(WINDOW_BASE).data, 0x77);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _scope = w.begin_access();
        panic!("synthetic unwind");
    }));
    assert!(!w.cpu_access());
    assert_eq!(v.read8(WINDOW_BASE).data, 0x77);
}
