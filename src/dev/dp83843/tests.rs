use super::*;
#[test]
fn identity_masks_counters_and_address_reset() {
    let now = Instant::now();
    let mut p = Dp83843::new(1);
    for (r, v) in [
        (0, 0x3100),
        (1, 0x7849),
        (2, 0x2000),
        (3, 0x5c10),
        (4, 0x1e1),
        (6, 4),
        (7, 0x2001),
        (0x10, 0x200),
        (0x16, 0x20),
        (0x19, 1),
    ] {
        assert_eq!(p.read(r, now), v, "register {r:x}");
    }
    p.write(4, 0xffff, now);
    assert_eq!(p.read(4, now), 0xa5ff);
    p.write(7, 0xffff, now);
    assert_eq!(p.read(7, now), 0xb7ff);
    for r in 0x13..=0x15 {
        p.write(r, 0xabcd, now);
        assert_eq!(p.read(r, now), 0xabcd);
        assert_eq!(p.read(r, now), 0);
    }
    p.write(0x19, 7, now);
    assert_eq!(p.address(), 7);
    p.write(0, 0x8000, now);
    assert_eq!(p.address(), 1);
}
#[test]
fn initial_settle_restart_timing_and_held_mode() {
    let now = Instant::now();
    let mut p = Dp83843::new(1);
    assert!(p.deadline().is_none());
    assert_eq!(p.read(1, now) & 0x24, 0);
    p.sync(now + Duration::from_secs(2));
    assert_eq!(p.read(1, now + Duration::from_secs(2)) & 0x24, 0x24);
    p.write(4, 0x21, now + Duration::from_secs(2));
    assert_eq!(p.read(0x10, now + Duration::from_secs(2)) & 6, 4);
    let now = now + Duration::from_secs(3);
    p.write(0, 0x3300, now);
    assert_eq!(p.read(1, now) & 0x24, 0);
    p.sync(now + Duration::from_millis(1499));
    assert_ne!(p.read(0, now + Duration::from_millis(1499)) & 0x200, 0);
    p.sync(now + Duration::from_millis(1500));
    assert_eq!(p.read(0, now + Duration::from_millis(1500)) & 0x200, 0);
    p.sync(now + Duration::from_millis(3500));
    assert_eq!(p.read(0x10, now + Duration::from_millis(3500)) & 0x27, 0x23);
}
#[test]
fn unsupported_media_and_interrupt_clear_order() {
    let now = Instant::now();
    let mut p = Dp83843::new(1);
    p.write(0x11, 2, now);
    p.write(0, 0x2100, now);
    assert_ne!(p.read(0x10, now) & 0x100, 0);
    assert_eq!(p.read(0x12, now), 0);
    p.write(0x11, 1, now);
    assert_eq!(p.read(0x12, now), 0x8000);
    assert_eq!(p.read(0x12, now), 0x8000);
    p.write(0x11, 0, now);
    assert_eq!(p.read(0x12, now), 0x8000);
    assert_eq!(p.read(0x12, now), 0);
    p.write(0x19, 0x21, now);
    assert_eq!(p.read(1, now) & 0x24, 0);
    assert!(!p.wire_enabled());
    p.write(0x19, 1, now);
    assert!(!p.wire_enabled());
}
