use super::*;

#[test]
fn link_negotiation_restart_and_reset() {
    let now = Instant::now();
    let mut p = Dp83843::new(1);
    assert_eq!(p.read(1, now) & 0x24, 0);
    assert!(!p.wire_enabled());
    assert_eq!(p.read(1, now + Duration::from_secs(2)) & 0x24, 0x24);
    p.write(0, 0x3300, now + Duration::from_secs(3));
    for (millis, restart, link) in [(4499, true, false), (4500, false, false), (6500, false, true)] {
        let at = now + Duration::from_millis(millis);
        assert_eq!(p.read(0, at) & 0x200 != 0, restart);
        assert_eq!(p.read(1, at) & 4 != 0, link);
        assert_eq!(p.wire_enabled(), link);
    }
    let at = now + Duration::from_secs(7);
    p.set_backend_link(false, at);
    p.write(0x19, 7, at);
    p.write(0, 0x8000, at);
    assert_eq!(p.address(), 1);
    assert_eq!(p.read(1, at) & 4, 0);
    assert_eq!(p.read(1, at + Duration::from_secs(2)) & 4, 0);
    p.set_backend_link(true, at + Duration::from_secs(2));
    assert!(p.wire_enabled());
}
