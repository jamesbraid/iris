use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
fn setup() -> (Arc<BedrockPi>, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(0));
    let clock = now.clone();
    let pi = BedrockPi::new(
        BedrockPiConfig::default(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    );
    pi.start();
    (pi, now)
}
fn r(pi: &BedrockPi, off: u64) -> u64 {
    pi.read64(off).data
}
#[test]
fn identity_protection_and_reserved_bits() {
    let (pi, _) = setup();
    assert_eq!(r(&pi, 0x20), 0);
    assert_eq!(r(&pi, 0x40), 1);
    assert_eq!(r(&pi, 0x48), 0);
    pi.write64(0, 0);
    pi.write64(8, 0x53474972756c6573);
    assert_eq!(r(&pi, 0), 1);
    pi.write64(0x30, !0);
    assert_eq!(r(&pi, 0x30), 255);
    pi.write64(0x38, !0);
    assert_eq!(r(&pi, 0x38), 0xffffff);
    let absent = BedrockPi::new(
        BedrockPiConfig {
            interface: 1,
            request_region: 0,
        },
        Arc::new(|| 0),
    );
    assert_eq!(r(&absent, 0x20), 2);
    assert_eq!(r(&absent, 0x40), 0);
    assert_eq!(pi.read32(0x20).status, BUS_ERR);
    assert_eq!(pi.write32(COUNTER, 0), BUS_ERR);
}
#[test]
fn pending_reflection_coherency_and_io_protection() {
    let (pi, _) = setup();
    pi.write64(0x90, 0x10c);
    assert_eq!(r(&pi, 0x98), 1 << 12);
    pi.write64(0x180, 12);
    assert_eq!(r(&pi, 0x98), 0);
    pi.set_lb_error(true);
    pi.write64(0x90, 64 + 55);
    assert_eq!(r(&pi, 0xa0), 1 << 55);
    pi.write64(0x90, 0x100 | 64 | 63);
    assert_eq!(r(&pi, 0xa0), 1 << 55);
    pi.write64(0xc8, 0);
    pi.write64(0xe8, 1);
    assert_eq!(r(&pi, 0x98), 1 << 5);
    pi.write64(0xd8, 1);
    assert_eq!(r(&pi, 0x98), 0);
    pi.write64(0x10, 0);
    assert!(!pi.io_modify(0x108, 0));
    pi.write64(0x10, 1);
    assert!(pi.io_modify(0x108, 0));
    assert_eq!(r(&pi, 0x98), 1 << 8);
}
#[test]
fn compare_progress_disabled_irq_phase_wrap_and_clear() {
    let (pi, now) = setup();
    pi.write64(COUNTER, 0x1_0000_0000);
    pi.write64(0x108, 0x1_0000_0002);
    now.store(1599, Ordering::SeqCst);
    assert_eq!(r(&pi, 0x120), 0);
    now.store(1600, Ordering::SeqCst);
    assert_eq!(r(&pi, 0x120), 1);
    assert_eq!(pi.read32(COUNTER).data, 1);
    assert_eq!(pi.read32(COUNTER + 4).data, 2);
    pi.write64(0x120, !0);
    assert_eq!(r(&pi, 0x120), 0);
    pi.write64(COUNTER, RT_MASK);
    pi.write64(0x118, 0);
    now.store(2400, Ordering::SeqCst);
    assert_eq!(r(&pi, COUNTER), 0);
    assert_eq!(r(&pi, 0x130), 1);
    assert_eq!(r(&pi, 0x138), 1);
    pi.write64(0x130, 0);
    assert_eq!(r(&pi, 0x130), 0);
    assert_eq!(r(&pi, 0x138), 1);
}
#[test]
fn reset_preserves_error_and_controls_but_clears_timer_state() {
    let (pi, _) = setup();
    pi.write64(0x28, 9);
    pi.write64(0x50, 1);
    pi.write64(0x418, !0);
    pi.set_memory_uncorrectable();
    pi.write64(0x3f8, 3);
    pi.write64(0x108, 0x24);
    pi.power_on();
    assert_eq!(r(&pi, 0x400), MEMORY_ERROR | 3);
    assert_eq!(r(&pi, 0x28), 9);
    assert_eq!(r(&pi, 0x50), 1);
    assert_eq!(r(&pi, 0x418), 0x1fffffff8);
    assert_eq!(r(&pi, 0x108), 0);
    assert_eq!(r(&pi, COUNTER), 0);
    pi.write64(0x3f8, 0);
    assert_eq!(r(&pi, 0x400), MEMORY_ERROR);
    pi.write64(0x400, MEMORY_ERROR);
    assert_eq!(r(&pi, 0x400), 0);
}
#[test]
fn level_reissue_and_pause_preserve_timer_phase() {
    let (pi, now) = setup();
    pi.set_level_source(0, 12, true);
    pi.write64(0x90, 0x10c);
    pi.write64(0x90, 12);
    now.store(999999, Ordering::SeqCst);
    assert_eq!(r(&pi, 0x98), 0);
    now.store(1000000, Ordering::SeqCst);
    assert_eq!(r(&pi, 0x98), 1 << 12);
    pi.write64(COUNTER, 0);
    pi.write64(0x108, 2);
    now.store(1000400, Ordering::SeqCst);
    pi.stop();
    now.store(2000000, Ordering::SeqCst);
    assert_eq!(r(&pi, COUNTER), 0);
    pi.start();
    now.store(2001200, Ordering::SeqCst);
    assert_eq!(r(&pi, 0x120), 1);
}
#[test]
fn irq_masks_route_all_five_lines_and_callbacks_can_read_registers() {
    struct Probe {
        pi: Weak<BedrockPi>,
        bits: AtomicU64,
    }
    impl InterruptController for Probe {
        fn set_line(&self, line: u32, level: bool) {
            if let Some(pi) = self.pi.upgrade() {
                let _ = pi.read64(0x98);
            }
            if level {
                self.bits.fetch_or(1 << line, Ordering::SeqCst);
            } else {
                self.bits.fetch_and(!(1 << line), Ordering::SeqCst);
            }
        }
    }
    let (pi, now) = setup();
    let probe = Arc::new(Probe {
        pi: Arc::downgrade(&pi),
        bits: AtomicU64::new(0),
    });
    pi.set_interrupt_controller(probe.clone());
    pi.write64(0xa8, 1 << 12);
    pi.write64(0xb0, 1 << 55);
    pi.write64(0x90, 0x10c);
    pi.set_lb_error(true);
    pi.write64(0x108, 2);
    pi.write64(0x118, 2);
    pi.write64(0x140, 1);
    pi.write64(0x150, 1);
    pi.write64(0x408, MEMORY_ERROR);
    pi.set_memory_uncorrectable();
    now.store(1600, Ordering::SeqCst);
    pi.poll();
    assert_eq!(probe.bits.load(Ordering::SeqCst), 31);
    pi.write64(0x120, 0);
    assert_eq!(probe.bits.load(Ordering::SeqCst), 27);
    pi.write64(0x90, 12);
    pi.set_lb_error(false);
    pi.write64(0x130, 0);
    pi.write64(0x400, MEMORY_ERROR);
    assert_eq!(probe.bits.load(Ordering::SeqCst), 0);
}
#[test]
fn node_hooks_observe_progress_without_timer_enable() {
    struct Hooks(Mutex<Vec<(u8, usize)>>);
    impl NodeHooks for Hooks {
        fn rt_compare_written(&self, interface: u8, slot: usize) {
            self.0.lock().push((interface, slot));
        }
    }
    let (pi, _) = setup();
    let hooks = Arc::new(Hooks(Mutex::new(Vec::new())));
    pi.set_node_hooks(hooks.clone());
    pi.write64(0x108, 0x24);
    pi.write64(0x110, 0x123);
    assert_eq!(r(&pi, 0x108), 0x24);
    assert_eq!(r(&pi, 0x110), 0x123);
    assert_eq!(*hooks.0.lock(), vec![(0, 0), (0, 1)]);
}
#[test]
fn timer_manager_delivers_without_mmio_poll_and_stop_removes_timer() {
    struct Probe(std::sync::mpsc::Sender<()>);
    impl InterruptController for Probe {
        fn set_line(&self, line: u32, level: bool) {
            if line == 2 && level {
                self.0.send(()).unwrap();
            }
        }
    }
    let pi = BedrockPi::new(BedrockPiConfig::default(), crate::dev::heart::host_clock());
    let tm = Arc::new(TimerManager::new());
    pi.set_timer_manager(tm.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    pi.set_interrupt_controller(Arc::new(Probe(tx)));
    pi.write64(0x108, 12500);
    pi.write64(0x140, 1);
    pi.start();
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    pi.stop();
    assert!(pi.timer.lock().is_none());
    assert!(!pi.is_running());
}
