use super::*;
use crate::traits::{BUS_ERR, BUS_OK};

/// A PIO offset in Bedrock IO's small window: widget in bits 27:24.
fn widget(widget: u64, offset: u64) -> u64 {
    (widget << 24) | offset
}

/// Configuration register 0 of `device` on the IO9 PIC's head 0.
fn config_id(device: u64) -> u64 {
    widget(15, 0x2_0000 + (device << 12))
}

/// The processor finds the PXBOW at widget 0 and the IO9's PIC at widget
/// 15, as the QEMU profile maps them; the one-processor board has no widget
/// 11 and no other widget answers.
#[test]
fn widget_0_is_the_pxbow_and_widget_15_the_io9_pic() {
    let board = TezroBoard::new(1).unwrap();
    let io = &board.node.io;
    let pxbow = io.pio_read(widget(0, 4), 4);
    assert_eq!(pxbow.status, BUS_OK);
    assert_eq!((pxbow.data >> 12) & 0xffff, u64::from(crate::dev::pxbow::PART));
    let pic = io.pio_read(widget(15, 0), 8);
    assert_eq!(pic.status, BUS_OK);
    assert_eq!(pic.data, u64::from(crate::dev::pic::identity(0)));
    for absent in [8, 9, 11, 12, 13, 14] {
        assert_eq!(io.pio_read(widget(absent, 0), 8).status, BUS_ERR, "widget {absent}");
    }
    assert_eq!(board.pics.len(), 1);
}

/// IO9's functions sit at PCI devices 1, 3 and 4 of head 0 with the
/// revisions the QEMU profile selects, and device 2 is empty.
#[test]
fn the_io9_bus_holds_the_ioc4_the_isp12160_and_the_bcm5701() {
    let board = TezroBoard::new(1).unwrap();
    let io = &board.node.io;
    for (device, id, revision) in [(1, 0x100a_10a9, 0x53), (3, 0x1216_1077, 0x06), (4, 0x1645_14e4, 0x00)] {
        assert_eq!(io.pio_read(config_id(device), 4).data, id, "device {device}");
        assert_eq!(io.pio_read(config_id(device) + 8, 4).data & 0xff, revision, "device {device}");
    }
    assert_eq!(io.pio_read(config_id(2), 4).data, 0xffff_ffff);
}

/// The BCM5701's slot drives REQ64#, so the chip reports a 64-bit bus,
/// and still does after the board is reset.
#[test]
fn the_bcm5701_sees_req64_before_and_after_a_reset() {
    use crate::pci::PciBusDevice;
    const PCISTATE: usize = 0x70 / 4;
    const BUS_32BIT: u32 = 1 << 4;
    let board = TezroBoard::new(1).unwrap();
    assert_eq!(board.eth.config().read(PCISTATE) & BUS_32BIT, 0);
    board.power_on();
    assert_eq!(board.eth.config().read(PCISTATE) & BUS_32BIT, 0);
}

/// The board identity the PROM reads: Bedrock board word 0x2001 and the
/// L1's Tezro station address.
#[test]
fn the_board_reports_the_tezro_board_word() {
    let board = TezroBoard::new(1).unwrap();
    let id = board.node.bus.read64(0x1000_0000 + 0x400_0000);
    assert_eq!((id.status, id.data), (BUS_OK, BOARD_ID));
    assert_eq!(board.node.l1.station_mac(), L1Config::default().mac);
}

/// PIC DMA reaches memory through Bedrock IO in naturally aligned legs, and
/// DMA aimed at any widget but Bedrock IO is refused.
#[test]
fn pic_dma_reaches_memory_through_bedrock_io_only() {
    let board = TezroBoard::new(1).unwrap();
    // Enable bank pair 0, as the Fuel's DMA tests do.
    assert_eq!(board.node.md.md_bus().write64(8, 0x33), BUS_OK);
    let peer = BedrockPeer(Arc::downgrade(&board.node.io));
    let data: Vec<u8> = (1..=19).collect();
    assert_eq!(peer.dma_write(15, HOST_WIDGET, 0x1003, &data), Ok(()));
    let mut back = vec![0; data.len()];
    assert_eq!(peer.dma_read(15, HOST_WIDGET, 0x1003, &mut back), Ok(()));
    assert_eq!(back, data);
    let memory = board.node.md.dma_view();
    assert_eq!(memory.read64(0x1000).data, 0x0000_0001_0203_0405);
    assert_eq!(peer.dma_write(15, 12, 0x1003, &data), Err(Refused));
    assert_eq!(peer.dma_read(15, 0, 0x1003, &mut back), Err(Refused));
}

#[test]
fn transfer_legs_keep_aligned_widths() {
    let legs: Vec<_> = BedrockPeer::legs(0x1003, 19).collect();
    assert_eq!(legs, [(0, 1), (1, 4), (5, 8), (13, 4), (17, 2)]);
    assert_eq!(BedrockPeer::legs(0x2000, 16).collect::<Vec<_>>(), [(0, 8), (8, 8)]);
    assert_eq!(BedrockPeer::legs(0x2000, 0).count(), 0);
}

/// The two- and four-processor board adds a second PIC at widget 11 with
/// nothing on its buses, keeps the IO9 on widget 15 behind widget 0, and
/// leaves the V12's widget 12 empty.
#[test]
fn two_or_four_processors_bring_the_second_pic_at_widget_11() {
    for cpus in [2, 4] {
        let board = TezroBoard::new(cpus).unwrap();
        let io = &board.node.io;
        assert_eq!(board.pics.iter().map(|p| p.widget_number()).collect::<Vec<_>>(), [11, 15]);
        assert_eq!(io.pio_read(widget(11, 0), 8).data, u64::from(crate::dev::pic::identity(0)));
        assert_eq!(io.pio_read(widget(15, 0), 8).data, u64::from(crate::dev::pic::identity(0)));
        assert_eq!(io.pio_read(widget(12, 0), 8).status, BUS_ERR, "no V12");
        assert_eq!((io.pio_read(widget(0, 4), 4).data >> 12) & 0xffff, u64::from(crate::dev::pxbow::PART));
        for device in 1..=4 {
            assert_eq!(io.pio_read(widget(11, 0x2_0000 + (device << 12)), 4).data, 0xffff_ffff, "widget 11 device {device}");
        }
        assert_eq!(io.pio_read(config_id(1), 4).data, 0x100a_10a9);
    }
}

/// Every fitted processor slot reads present, in the board's order (CPU 1
/// in PI1/A, CPUs 2 and 3 in the B slots), and only CPU 0 is enabled.
#[test]
fn fitted_processors_read_present_and_only_cpu_0_is_enabled() {
    use crate::dev::bedrock_pi::reg::{CPU_A_ENABLE, CPU_B_ENABLE, INTERFACE_SLOT_A, INTERFACE_SLOT_B};
    for (cpus, present) in [(1, [[1, 0], [0, 0]]), (2, [[1, 0], [1, 0]]), (4, [[1, 1], [1, 1]])] {
        let board = TezroBoard::new(cpus).unwrap();
        for (interface, pi) in board.node.pi.iter().enumerate() {
            let slots = [pi.read64(INTERFACE_SLOT_A).data, pi.read64(INTERFACE_SLOT_B).data];
            assert_eq!(slots, present[interface], "{cpus} CPUs, PI{interface}");
            let enabled = [pi.read64(CPU_A_ENABLE).data & 1, pi.read64(CPU_B_ENABLE).data & 1];
            assert_eq!(enabled, [u64::from(interface == 0), 0], "{cpus} CPUs, PI{interface}");
        }
    }
}

/// Only the counts a board fits are accepted.
#[test]
fn the_board_refuses_a_population_it_cannot_fit() {
    assert!(TezroBoard::new(3).is_err());
    assert!(TezroBoard::new(0).is_err());
    assert!(TezroBoard::new(5).is_err());
}

/// The poll returns lifted checked pages to the direct window once every
/// interval of board time while the board runs, not on every batch.
#[test]
fn the_poll_retires_lifted_pages_once_an_interval() {
    use std::sync::atomic::AtomicU64;
    let now = Arc::new(AtomicU64::new(0));
    let clock = now.clone();
    let board = TezroBoard::with_config(TezroBoardConfig {
        payload: &[],
        clock: Arc::new(move || clock.load(Ordering::SeqCst)),
        flash_path: None,
        station_mac: L1Config::default().mac,
        timekeeper: Arc::new(Ds1744::new(YearBase::From(1968)).unwrap()),
        memory: &MemoryPopulation::default(),
        cpus: 1,
        v12: false,
    })
    .unwrap();
    board.running.store(true, Ordering::SeqCst);
    now.store(RETIRE_INTERVAL_NS - 1, Ordering::SeqCst);
    board.poll();
    assert_eq!(board.last_retire.load(Ordering::SeqCst), 0);
    now.store(RETIRE_INTERVAL_NS, Ordering::SeqCst);
    board.poll();
    assert_eq!(board.last_retire.load(Ordering::SeqCst), RETIRE_INTERVAL_NS);
    board.running.store(false, Ordering::SeqCst);
}

fn on_a_big_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(64 << 20).spawn(f).unwrap().join().unwrap();
}

/// A board with a payload of recognisable words, and its first bank
/// enabled over the reset window, as the PROM leaves it once memory is up.
fn board_with_ram_over_the_prom() -> (TezroBoard, Vec<u8>) {
    let payload: Vec<u8> = (0..0x2000u32).flat_map(|i| (0x5000_0000 | i).to_be_bytes()).collect();
    let board = TezroBoard::with_config(TezroBoardConfig {
        payload: &payload,
        clock: Arc::new(|| 0),
        flash_path: None,
        station_mac: L1Config::default().mac,
        timekeeper: Arc::new(Ds1744::new(YearBase::From(1968)).unwrap()),
        memory: &MemoryPopulation::default(),
        cpus: 1,
        v12: false,
    })
    .unwrap();
    // Banks 0 and 1 enabled at 1 GiB each: bank 0 covers 0x1fc0_0000.
    assert_eq!(board.node.md.md_bus().write64(8, 0x53), BUS_OK);
    (board, payload)
}

/// With RAM over the reset window, the PROM's noncoherent cached references
/// (cache algorithm 3, as KSEG0 and xkphys 0x98 give) still read the PROM,
/// coherent ones (xkphys 0xa8) read RAM, and the uncached views keep the
/// PROM they map there. The plane bit keeps the two kinds of cached line
/// apart in the cache.
#[test]
fn cached_prom_references_stay_on_the_prom_when_ram_covers_it() {
    // An executor is built on the stack; give it room.
    on_a_big_stack(|| {
        let (board, payload) = board_with_ram_over_the_prom();
        let word = |i: usize| u64::from(u32::from_be_bytes(payload[i * 4..i * 4 + 4].try_into().unwrap()));
        // RAM under the window holds something else.
        let ram = board.node.md.dma_view();
        assert_eq!(ram.write64(0x1fc0_0010, 0x1111_2222_3333_4444), BUS_OK);
        let mut cpu = build_cpu(&board);
        cpu.core.cp0_status |= crate::cpu::mips_core::STATUS_KX;
        cpu.core.cp0_config = (cpu.core.cp0_config & !7) | 3;
        cpu.resync_privilege_state();
        let read = |cpu: &mut TezroCpu, va: u64| cpu.debug_read(va, 4).unwrap();
        assert_eq!(read(&mut cpu, 0x9800_0000_1fc0_0010), word(4), "cache algorithm 3 reads the PROM");
        assert_eq!(read(&mut cpu, 0xffff_ffff_9fc0_0010), word(4), "KSEG0 with K0 = 3 reads the PROM");
        assert_eq!(read(&mut cpu, 0xa800_0000_1fc0_0010), 0x1111_2222, "a coherent reference reads RAM");
        assert_eq!(read(&mut cpu, 0xffff_ffff_bfc0_0010), word(4), "KSEG1 reads the PROM");
        assert_eq!(read(&mut cpu, 0x9600_0000_1fc0_0010), 0x1111_2222, "uncached memory space reads RAM");
        // Outside the payload the window does not apply.
        let end = 0x1fc0_0000 + payload.len() as u64;
        assert_eq!(ram.write64(end, 0x5555_6666_7777_8888), BUS_OK);
        assert_eq!(read(&mut cpu, 0x9800_0000_0000_0000 | end), 0x5555_6666);
    });
}

/// The CPU tags a cache algorithm 3 reference in the window with the plane
/// bit and leaves every other reference alone; a board that sets no window
/// sees no plane at all.
#[test]
fn only_algorithm_3_inside_the_window_takes_the_plane() {
    // An executor is built on the stack; give it room.
    on_a_big_stack(|| {
        let (board, payload) = board_with_ram_over_the_prom();
        let mut cpu = build_cpu(&board);
        cpu.core.cp0_status |= crate::cpu::mips_core::STATUS_KX;
        cpu.resync_privilege_state();
        let phys = |cpu: &mut TezroCpu, va: u64| cpu.debug_translate(va).phys;
        assert_eq!(phys(&mut cpu, 0x9800_0000_1fc0_0000), PROM_PLANE | 0x1fc0_0000);
        assert_eq!(phys(&mut cpu, 0xa800_0000_1fc0_0000), 0x1fc0_0000);
        assert_eq!(phys(&mut cpu, 0x9000_0000_1fc0_0000), 0x1fc0_0000);
        assert_eq!(phys(&mut cpu, 0x9800_0000_1fbf_fff8), 0x1fbf_fff8);
        let end = 0x1fc0_0000 + payload.len() as u64;
        assert_eq!(phys(&mut cpu, 0x9800_0000_0000_0000 | end), end);
        cpu.cached_plane = None;
        assert_eq!(phys(&mut cpu, 0x9800_0000_1fc0_0000), 0x1fc0_0000);
    });
}

/// IOC4 reads its 32-bit registers in the processor's byte order through
/// the IO9 PIC's self-ordered slot: what the processor writes is the
/// register's value, as on the QEMU Tezro, where PROM 6.210 writes the
/// serial DMA control 0x100201ff and the TX producer 0x3f8.
#[test]
fn the_processor_reads_and_writes_ioc4_registers_in_its_own_byte_order() {
    let board = TezroBoard::new(1).unwrap();
    let io = &board.node.io;
    // IOC4's BAR at PCI memory 0x200000, decoding memory, reached through
    // head 0's first short window.
    assert_eq!(io.pio_write(config_id(1) + 0x10, 4, 0x20_0000), BUS_OK);
    assert_eq!(io.pio_write(config_id(1) + 4, 4, 2), BUS_OK);
    assert_eq!(io.pio_write(widget(15, 0x200), 8, (1 << 12) | 2), BUS_OK);
    let reg = |offset: u64| widget(15, 0x20_0000 + offset);
    assert_eq!(io.pio_read(reg(0x24), 4).data, 1, "MISC: the 66 MHz clock");
    assert_eq!(io.pio_write(reg(0x314), 4, 0x3f8), BUS_OK);
    assert_eq!(io.pio_read(reg(0x314), 4).data, 0x3f8, "port 0 TX producer");
}

fn board_with_v12(cpus: usize) -> TezroBoard {
    TezroBoard::with_config(TezroBoardConfig {
        payload: &[],
        clock: Arc::new(|| 0),
        flash_path: None,
        station_mac: L1Config::default().mac,
        timekeeper: Arc::new(Ds1744::new(YearBase::From(1968)).unwrap()),
        memory: &MemoryPopulation::default(),
        cpus,
        v12: true,
    })
    .unwrap()
}

/// The V12 answers as widget 12 behind the PXBOW on either board, with its
/// Odyssey identity, and widget 12 stays empty without one. (On the
/// one-processor board the placement is inferred; see V12_WIDGET.)
#[test]
fn the_v12_answers_at_widget_12_on_both_boards() {
    for cpus in [1, 4] {
        let board = board_with_v12(cpus);
        let id = board.node.io.pio_read(widget(12, 4), 4);
        assert_eq!((id.status, id.data), (BUS_OK, 0xac01_3047), "{cpus} CPUs");
        assert_eq!(board.pics[board.pics.len() - 1].pxbow().unwrap().endpoint_widgets() & (1 << 12), 1 << 12);
    }
    assert_eq!(TezroBoard::new(1).unwrap().node.io.pio_read(widget(12, 4), 4).status, BUS_ERR);
}

/// A CPU 0 store into its enabled graphics page is counted against PI0's
/// slot A credits and, once the V12 has executed it, the credit comes back
/// along the node's graphics map. A store outside the page costs nothing.
/// Follows sn1/pi-gfx/ip53-odyssey-credit-return and
/// uncovered-stores-not-counted in tests/qtest/sn1-pi-gfx-test.c.
#[test]
fn a_counted_store_to_the_v12_returns_its_credit() {
    use crate::dev::bedrock_io::reg::GFX_MAP_BASE;
    let board = board_with_v12(4);
    let (pi, io, bus) = (&board.node.pi[0], &board.node.io, &board.node.bus);
    // The page of widget 12's small window, enabled, with 16 credits.
    assert_eq!(pi.write64(0x300, crate::dev::pi_gfx::PAGE_ENABLE | (12 << 24)), BUS_OK);
    assert_eq!(pi.write64(0x308, 16), BUS_OK);
    // Widget 12's credits go to node 0, PI0, slot A.
    assert_eq!(io.write64(GFX_MAP_BASE, 12), BUS_OK);
    let io_view = 1u64 << 57;
    board.v12.as_ref().unwrap().start();
    assert_eq!(bus.write64(io_view | widget(12, 0x110000), (0xc580_c406u64 << 32) | 5), BUS_OK);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while pi.read64(0x308).data != 16 {
        assert!(std::time::Instant::now() < deadline, "credit {} not returned", pi.read64(0x308).data);
        std::thread::yield_now();
    }
    // Outside the page: neither counted nor returned.
    let _ = bus.write64(io_view | widget(11, 0x100), 0);
    assert_eq!(pi.read64(0x308).data, 16);
    board.v12.as_ref().unwrap().stop();
}
