use super::*;

fn send(l1: &L1, bytes: &[u8]) {
    for &b in bytes {
        assert_eq!(l1.write64(0, b as u64), BUS_OK);
    }
}
fn drain(l1: &L1) -> Vec<u8> {
    let mut out = Vec::new();
    while l1.read64(0x28).data & 1 != 0 {
        out.push(l1.read64(0).data as u8);
    }
    out
}
fn request(l1: &L1, command: u16, args: &[u8], count: u8) -> Vec<u8> {
    let mut p = vec![
        0x10,
        0x0f,
        0xff,
        0xf0,
        6,
        (command >> 8) as u8,
        command as u8,
        count,
    ];
    p.extend_from_slice(args);
    send(l1, &encode(&p));
    drain(l1)
}
#[test]
fn l1_static_position_vector_and_native_uart() {
    let l1 = L1::new(L1Config::default()).unwrap();
    assert_eq!(l1.read8(0).status, BUS_ERR);
    assert_eq!(l1.read32(0).status, BUS_ERR);
    assert_eq!(l1.read64(1).status, BUS_ERR);
    assert_eq!(l1.read64(0x28).data, 0x60);
    send(
        &l1,
        &[
            0x7e, 0x10, 0x0f, 0xff, 0xf0, 6, 0, 0x0e, 0, 0xf9, 0x72, 0x7e,
        ],
    );
    assert_eq!(
        drain(&l1),
        [0x7e, 0x30, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0x30, 0x98, 0x7e]
    );
}
#[test]
fn l1_crc_errors_escaping_reset_and_revision() {
    let l1 = L1::new(L1Config::default()).unwrap();
    send(&l1, &[0x7e, 1, 2, 3, 0x7e]);
    assert!(drain(&l1).is_empty());
    assert_eq!(
        request(&l1, 5, &[], 0),
        encode(&[0x30, 0xff, 0xff, 0xff, 0x98, 0])
    );
    let mut text = vec![1];
    text.extend_from_slice(b"hi~}\r\0");
    assert_eq!(
        request(&l1, 0x1004, &text, 1),
        encode(&[0x30, 0, 0, 0, 0, 0])
    );
    assert_eq!(l1.display_line(), "hi~}?");
    assert!(encode(&[0x7e, 0x7d, 0x0d])
        .windows(2)
        .any(|x| x == [0x7d, 0x2d]));
    l1.power_on();
    assert_eq!(l1.read64(0x28).data, 0x60);
    assert_eq!(
        request(&l1, 5, &[], 0),
        encode(&[0x30, 0xff, 0xff, 0xff, 0x98, 0])
    );
}
#[test]
fn l1_irq_priority_and_divisor() {
    let l1 = L1::new(L1Config::default()).unwrap();
    let (probe, line) = crate::irq::LevelProbe::line();
    l1.set_irq(line);
    l1.write64(8, 3);
    assert!(probe.level());
    assert_eq!(l1.read64(0x10).data, 2);
    assert!(!probe.level());
    l1.write64(0x18, 0x80);
    l1.write64(0, 42);
    l1.write64(8, 7);
    assert_eq!(l1.read64(0).data, 42);
    assert_eq!(l1.read64(8).data, 7);
    l1.write64(0x18, 3);
    send(&l1, &encode(&[0x10, 0x0f, 0xff, 0xf0, 6, 0, 0x0e, 0]));
    assert_eq!(l1.read64(0x10).data, 4);
    drain(&l1);
    assert_eq!(l1.read64(0x10).data, 2);
    assert!(!probe.level());
}
#[test]
fn l1_fuel_identity_records_and_mac() {
    let l1 = L1::new(L1Config::default()).unwrap();
    assert_eq!(
        request(&l1, 3, &[], 0),
        encode(&[0x30, 0, 0, 0, 0, 2, 0, 0, 0, 0, 65, 0, 0, 0, 0, 65])
    );
    let chassis = l1.record(0, 1).unwrap();
    assert_eq!(
        chassis,
        [0, 2, 0, 0xc2, 0x4e, 0x41, 0xc2, 0x4e, 0x41, 0xc1, 0, 0, 0, 0, 0, 0x9b]
    );
    for c in 0..2 {
        let b = l1.record(c, 2).unwrap();
        assert_eq!(b.iter().fold(0u8, |s, x| s.wrapping_add(*x)), 0);
        assert!(b.windows(4).any(|x| x == b"IP34"));
        assert!(b.windows(6).any(|x| x == b"EMUN00"));
    }
    assert!(l1
        .record(1, 2)
        .unwrap()
        .windows(12)
        .any(|x| x == b"080069123456"));
    assert_eq!(
        request(&l1, 8, &[], 0),
        encode(&[
            0x30, 0, 0, 0, 0, 1, 1, b'0', b'8', b'0', b'0', b'6', b'9', b'1', b'2', b'3', b'4',
            b'5', b'6', 0
        ])
    );
}

#[test]
fn l1_eeprom_chunks_errors_and_explicit_revision() {
    let l1 = L1::new(L1Config {
        revision: Some([1, 48, 1]),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        request(&l1, 5, &[], 0),
        encode(&[0x30, 0, 0, 0, 0, 3, 0, 0, 0, 0, 1, 0, 0, 0, 0, 48, 0, 0, 0, 0, 1])
    );
    let args = |c, area, off, len| model::ints(&[c, area, off, len]);
    let board = l1.record(1, 2).unwrap();
    let mut expected = vec![0x30, 0, 0, 0, 0, 2];
    expected.extend(model::ints(&[0]));
    expected.push(0x80 | board.len() as u8);
    expected.extend(board);
    assert_eq!(request(&l1, 6, &args(1, 2, 0, 116), 4), encode(&expected));
    assert_eq!(
        request(&l1, 6, &args(2, 5, 0, 1), 4),
        encode(&[0x30, 255, 255, 255, 152, 0])
    );
    assert_eq!(
        request(&l1, 6, &args(1, 5, 0, 1), 4),
        encode(&[0x30, 255, 255, 255, 151, 0])
    );
    assert_eq!(
        request(&l1, 6, &args(1, 2, 0, 1), 3),
        encode(&[0x30, 255, 255, 255, 156, 0])
    );
    let mut raw = vec![0; 160];
    raw[100] = 1;
    l1.set_record(4, 4, raw.clone()).unwrap();
    assert_ne!(
        request(&l1, 6, &args(4, 4, 0, 1), 4),
        encode(&[0x30, 255, 255, 255, 149, 0])
    );
    raw[1] = 1;
    l1.set_record(4, 4, raw).unwrap();
    assert_eq!(
        request(&l1, 6, &args(4, 4, 0, 1), 4),
        encode(&[0x30, 255, 255, 255, 149, 0])
    );
    l1.power_on();
    assert_eq!(l1.state.lock().config.revision, Some([1, 48, 1]));
}
#[test]
fn l1_malformed_frames_recover_and_unknown_routes_remain_observable() {
    let l1 = L1::new(Default::default()).unwrap();
    send(&l1, &[1, 2, 3, 0x7e]);
    send(&l1, &vec![0x41; 259]);
    send(&l1, &[0x7e]);
    assert!(drain(&l1).is_empty());
    assert_eq!(
        request(&l1, 0x2000, &[], 0),
        encode(&[0x30, 255, 255, 255, 155, 0])
    );
    send(&l1, &encode(&[0x10, 0x4f, 0xff, 0xf0, 6, 0, 3, 0]));
    assert_eq!(drain(&l1), encode(&[0x30, 255, 255, 255, 255, 0, 0, 0, 8]));
    assert_eq!(
        request(&l1, 0x1002, &[1, b'x', 0], 1),
        encode(&[0x30, 255, 255, 255, 151, 0])
    );
    assert_eq!(
        request(&l1, 0x1002, &[0, 1], 1),
        encode(&[0x30, 255, 255, 255, 156, 0])
    );
    assert_eq!(
        request(&l1, 0x1002, &model::ints(&[0x40]), 1),
        encode(&[0x30, 0, 0, 0, 0, 0])
    );
    assert_eq!(
        request(&l1, 0xe, &[], 0),
        encode(&[0x30, 0, 0, 0, 0, 1, 0, 0, 0, 0, 2])
    );
    l1.power_on();
    assert_eq!(
        request(&l1, 0xe, &[], 0),
        encode(&[0x30, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0])
    );
}
#[test]
fn l1_console_and_power_callbacks_run_outside_state_lock() {
    let l1 = Arc::new(L1::new(Default::default()).unwrap());
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = output.clone();
    let weak = Arc::downgrade(&l1);
    l1.set_event_handler(Arc::new(move |e| {
        let device = weak.upgrade().unwrap();
        let _ = device.node_powered();
        captured.lock().push(e);
    }));
    l1.receive_console(b"x");
    assert_eq!(drain(&l1), encode(&[0, b'x']));
    l1.receive_console(&[0x14]);
    l1.receive_console(b"nmi\r");
    assert_eq!(*output.lock(), [L1Event::Nmi]);
    request(&l1, 0x2005, &[], 0);
    request(&l1, 0x2005, &[], 0);
    assert_eq!(
        *output.lock(),
        [L1Event::Nmi, L1Event::Power(false), L1Event::Shutdown]
    );
    request(&l1, 0x2006, &[], 0);
    assert_eq!(
        *output.lock(),
        [
            L1Event::Nmi,
            L1Event::Power(false),
            L1Event::Shutdown,
            L1Event::Power(true),
            L1Event::Reset
        ]
    );
}
#[test]
fn l1_event_registration_and_fifo_clear() {
    let l1 = L1::new(Default::default()).unwrap();
    l1.send_event(0x8000, 7, "test");
    assert!(drain(&l1).is_empty());
    request(&l1, 0x1007, &model::ints(&[23]), 1);
    l1.send_event(0x8000, 7, "test");
    let mut expected = vec![0x57, 0, 4, 0x10, 4, 0, 0, 0x80, 0, 2, 0, 0, 0, 0, 7, 1];
    expected.extend_from_slice(b"\r\n001c01 ATTN: test\r\n\0");
    assert_eq!(drain(&l1), encode(&expected));
    l1.send_event(0x8000, 7, "test");
    l1.write64(0x10, 2);
    assert!(drain(&l1).is_empty());
    request(&l1, 0x100a, &[], 0);
    l1.send_event(0x8000, 7, "test");
    assert!(drain(&l1).is_empty());
}
#[test]
fn l1_ci_backend_and_joined_worker() {
    let l1 = L1::new(Default::default()).unwrap();
    let backend = Arc::new(crate::dev::z85c30::CiSerialBackend::new());
    l1.set_serial_backend(backend.clone());
    backend.push_host(b"z");
    l1.poll_serial();
    assert_eq!(drain(&l1), encode(&[0, b'z']));
    send(&l1, &encode(&[0x40, b'h', b'i']));
    assert_eq!(backend.drain_guest(), b"hi");
    l1.start();
    backend.push_host(b"w");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while l1.read64(0x28).data & 1 == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    l1.stop();
    assert!(!l1.is_running());
    assert_eq!(drain(&l1), encode(&[0, b'w']));
    l1.start();
    l1.stop();
}
