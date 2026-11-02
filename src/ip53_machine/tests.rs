use super::*;
use crate::traits::{BUS_ERR, BUS_OK};
use std::sync::atomic::AtomicU64;

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
    assert_eq!(io.pio_read(config_id(2), 4).status, BUS_ERR, "device 2 is empty");
}

/// The BCM5701's slot drives REQ64#, so the chip reports a 64-bit bus,
/// and still does after the board is reset.
#[test]
fn the_bcm5701_sees_req64_before_and_after_a_reset() {
    use crate::pci::PciFunction;
    const PCISTATE: usize = 0x70 / 4;
    const BUS_32BIT: u32 = 1 << 4;
    let board = TezroBoard::new(1).unwrap();
    assert_eq!(board.eth.config_dword(PCISTATE) & BUS_32BIT, 0);
    board.power_on();
    assert_eq!(board.eth.config_dword(PCISTATE) & BUS_32BIT, 0);
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
            assert_eq!(io.pio_read(widget(11, 0x2_0000 + (device << 12)), 4).status, BUS_ERR, "widget 11 device {device}");
        }
        assert_eq!(io.pio_read(config_id(1), 4).data, 0x100a_10a9);
    }
}

// Widget 0's crossbar as PROM 6.210 and the IP35 kernel walk it, from how the PROM finds
// widgets 0xb and 0xf.
// QEMU's sn/builder/tezro-two-four-processor-board in
// tests/qtest/sn-builder-test.c still expects link 11 absent; this board
// follows the PROM's walk instead.

/// Link `n`'s register block in widget 0, as the 32-bit word address the
/// PROM issues: the low word of each doubleword.
fn link_word(n: u64, register: u64) -> u64 {
    widget(0, 0x100 + (n - 8) * 0x40 + register + 4)
}
const LINK_CONTROL: u64 = 0x08;
const LINK_STATUS: u64 = 0x10;
const LINK_ARB_UPPER: u64 = 0x18;
const LINK_ARB_LOWER: u64 = 0x20;
const LINK_STATUS_CLEAR: u64 = 0x28;
const LINK_RESET: u64 = 0x30;
const LINK_AUX: u64 = 0x38;
const ALIVE: u64 = 1 << 31;
const PRESENT: u64 = 1 << 5;
const FAILURE_MODE: u64 = 1 << 6;

fn read_word(board: &TezroBoard, address: u64) -> u64 {
    let r = board.node.io.pio_read(address, 4);
    assert_eq!(r.status, BUS_OK, "{address:#x}");
    r.data
}

/// Widget 0's part and manufacturer, from its identity word.
fn widget_zero_part(board: &TezroBoard) -> (u64, u64) {
    let id = read_word(board, widget(0, 4));
    ((id >> 12) & 0xffff, (id >> 1) & 0x7ff)
}

fn alive_and_present(board: &TezroBoard, n: u64) -> (bool, bool) {
    (read_word(board, link_word(n, LINK_STATUS)) & ALIVE != 0, read_word(board, link_word(n, LINK_AUX)) & PRESENT != 0)
}

/// The PROM's per-link test: alive and present, except that link 15 is
/// taken regardless when widget 0 is part 0xd100.
fn prom_uses(board: &TezroBoard, n: u64) -> bool {
    let (alive, present) = alive_and_present(board, n);
    (alive && present) || (n == 15 && widget_zero_part(board).0 == 0xd100)
}

/// The kernel's: alive, and present unless widget 0 is part 0xd100 with
/// manufacturer 0 and the link is 15.
fn kernel_uses(board: &TezroBoard, n: u64) -> bool {
    let (alive, present) = alive_and_present(board, n);
    alive && (present || (n == 15 && widget_zero_part(board) == (0xd100, 0)))
}

/// Links 8 to 15 that pass both tests on their own bits, without either
/// guest's link-15 exception, as a widget mask.
fn live_links(board: &TezroBoard) -> u16 {
    (8..16).filter(|&n| alive_and_present(board, n) == (true, true)).fold(0, |m, n| m | 1 << n)
}

/// A board whose clock the test advances.
fn board_with_clock(cpus: usize) -> (TezroBoard, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(0));
    let clock = now.clone();
    let board = TezroBoard::with_config(TezroBoardConfig {
        payload: &[],
        clock: Arc::new(move || clock.load(Ordering::SeqCst)),
        flash_path: None,
        station_mac: L1Config::default().mac,
        timekeeper: Arc::new(Ds1744::new(YearBase::From(1968)).unwrap()),
        memory: &MemoryPopulation::default(),
        cpus,
        chip: &CHIPS[0],
        v12: false,
        graphics_pool: (2, 32),
        graphics_raster: (crate::config::RasterUnit::Cpu, "auto"),
    })
    .unwrap();
    (board, now)
}

/// On the two- and four-processor board widget 0's crossbar shows the
/// hub's link 10 and both PICs' links 11 and 15 alive and present, with no
/// failure mode and arbitration bits 0 and 2 clear, so the PROM and the
/// kernel both take 11 and 15 without leaning on the link-15 exception.
/// Every other link reads empty. A power-on keeps the same picture.
#[test]
fn widget_0_shows_both_pic_links_live_on_the_larger_board() {
    for cpus in [2, 4] {
        let board = TezroBoard::new(cpus).unwrap();
        for pass in ["built", "after power-on"] {
            assert_eq!(widget_zero_part(&board), (0xd100, 0), "{cpus} CPUs, {pass}");
            assert_eq!(read_word(&board, widget(0, 4)) & 1, 1);
            assert_eq!(live_links(&board), 1 << 10 | 1 << 11 | 1 << 15, "{cpus} CPUs, {pass}");
            for n in [11, 15] {
                assert!(prom_uses(&board, n) && kernel_uses(&board, n), "link {n}, {cpus} CPUs, {pass}");
                assert_eq!(read_word(&board, link_word(n, LINK_AUX)) & FAILURE_MODE, 0, "link {n}");
                assert_eq!(read_word(&board, link_word(n, LINK_ARB_UPPER)) & 0b101, 0, "link {n}: unclaimed");
            }
            for n in [8, 9, 12, 13, 14] {
                assert_eq!(alive_and_present(&board, n), (false, false), "link {n}, {cpus} CPUs, {pass}");
                assert!(!prom_uses(&board, n) && !kernel_uses(&board, n));
            }
            board.power_on();
        }
    }
}

/// The one-processor board has no second PIC: link 11 reads empty and
/// widget 11 does not answer, while link 15 stays usable.
#[test]
fn the_one_processor_board_shows_no_link_11() {
    let board = TezroBoard::new(1).unwrap();
    assert_eq!(live_links(&board), 1 << 10 | 1 << 15);
    assert_eq!(alive_and_present(&board, 11), (false, false));
    assert!(!prom_uses(&board, 11) && !kernel_uses(&board, 11));
    assert!(prom_uses(&board, 15) && kernel_uses(&board, 15));
    assert_eq!(board.node.io.pio_read(widget(11, 4), 4).status, BUS_ERR);
}

/// Widget 11 answers as a bridge-class PIC with both heads, the identity
/// the PROM's "Found PIC" path needs (manufacturer 0x24, bit 0 set, part
/// 0xd102 on head 0), and takes the PROM's control-word writes on both
/// heads. Widget 15 keeps its own.
#[test]
fn widget_11_answers_as_a_pic_with_both_heads() {
    use crate::dev::pic::HEAD_OFFSET;
    let board = TezroBoard::new(4).unwrap();
    let io = &board.node.io;
    for (head, part) in [(0, 0xd102), (HEAD_OFFSET, 0xd112)] {
        let id = read_word(&board, widget(11, head + 4));
        assert_eq!((id & 1, (id >> 1) & 0x7ff, (id >> 12) & 0xffff), (1, 0x24, part), "head at {head:#x}");
        // Widget control, as the PROM's table-driven field writes it.
        let control = widget(11, head + 0x24);
        let before = read_word(&board, control);
        assert_eq!(io.pio_write(control, 4, (before & !0xf000) | 0x4000), BUS_OK);
        assert_eq!(read_word(&board, control) & 0xf000, 0x4000, "head at {head:#x}");
    }
    assert_eq!(read_word(&board, widget(15, 4)) >> 12 & 0xffff, 0xd102);
}

/// The PROM's bring-up of link 11 (read the status clear, write credit 2,
/// clear both arbitration registers, then the 0xd102 credit 3) leaves the
/// link alive and present and the credit field reading back.
#[test]
fn the_proms_link_bring_up_keeps_link_11_alive() {
    let board = TezroBoard::new(4).unwrap();
    let io = &board.node.io;
    read_word(&board, link_word(11, LINK_STATUS_CLEAR));
    assert_eq!(io.pio_write(link_word(11, LINK_CONTROL), 4, 0x0008_0000), BUS_OK);
    assert_eq!(io.pio_write(link_word(11, LINK_ARB_UPPER), 4, 0), BUS_OK);
    assert_eq!(io.pio_write(link_word(11, LINK_ARB_LOWER), 4, 0), BUS_OK);
    assert_eq!(alive_and_present(&board, 11), (true, true));
    let control = read_word(&board, link_word(11, LINK_CONTROL));
    assert_eq!((control >> 18) & 0x1f, 2);
    assert_eq!(io.pio_write(link_word(11, LINK_CONTROL), 4, (control & !(0x1f << 18)) | 3 << 18), BUS_OK);
    assert_eq!((read_word(&board, link_word(11, LINK_CONTROL)) >> 18) & 0x1f, 3);
    assert_eq!(alive_and_present(&board, 11), (true, true));
    assert_eq!(io.pio_read(widget(11, 4), 4).status, BUS_OK);
}

/// The processor reaches widget 11 through the crossbar, not around it: a
/// reset of link 11 or of the hub's link 10 takes widget 11 out of reach
/// for the pulse, and it answers again, registers kept, once it ends. A
/// reset of link 15 leaves widget 11 alone.
#[test]
fn widget_11_is_routed_through_widget_0s_crossbar() {
    let (board, now) = board_with_clock(4);
    let io = &board.node.io;
    let route = widget(11, 0x130);
    assert_eq!(io.pio_write(route, 8, 0x1122_3344_5566_7788), BUS_OK);
    for link in [11, 10] {
        now.fetch_add(100, Ordering::SeqCst);
        assert_eq!(io.pio_write(link_word(link, LINK_RESET), 4, 0), BUS_OK);
        assert_eq!(io.pio_read(route, 8).status, BUS_ERR, "down during link {link}'s pulse");
        now.fetch_add(20, Ordering::SeqCst);
        let r = io.pio_read(route, 8);
        assert_eq!((r.status, r.data), (BUS_OK, 0x1122_3344_5566_7788), "after link {link}'s pulse");
        assert_eq!(alive_and_present(&board, 11), (true, true));
    }
    now.fetch_add(100, Ordering::SeqCst);
    assert_eq!(io.pio_write(link_word(15, LINK_RESET), 4, 0), BUS_OK);
    assert_eq!(io.pio_read(route, 8).status, BUS_OK, "link 15's pulse");
    assert_eq!(io.pio_read(widget(15, 4), 4).status, BUS_ERR);
    assert_eq!(board.pics[1].pxbow().unwrap().endpoint_widgets(), 1 << 11 | 1 << 15);
    assert!(board.pics[0].pxbow().is_none(), "the second PIC has no crossbar of its own");
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

/// A board of one processor and one GiB of memory with `chip` fitted.
fn board_with(chip: &Part) -> TezroBoard {
    TezroBoard::with_config(TezroBoardConfig {
        payload: &[],
        clock: Arc::new(|| 0),
        flash_path: None,
        station_mac: L1Config::default().mac,
        timekeeper: Arc::new(Ds1744::new(YearBase::From(1968)).unwrap()),
        memory: &MemoryPopulation::one_gib(),
        cpus: 1,
        chip: self::chip(chip),
        v12: false,
        graphics_pool: (2, 32),
        graphics_raster: (crate::config::RasterUnit::Cpu, "auto"),
    })
    .unwrap()
}

/// Run `body` on a thread with the stack a debug build's processor needs.
fn on_big_stack(body: impl FnOnce() + Send + 'static) {
    if let Err(panic) = std::thread::Builder::new().stack_size(64 << 20).spawn(body).unwrap().join() {
        std::panic::resume_unwind(panic);
    }
}

/// Each chip resets with its own PRId, FIR and Config word, and keeps them
/// across a CPU reset, which reloads PRId and FIR from the reset values. The
/// board's flash record and Count rate follow the chip's clock. These are the
/// facts the chips grade as inferred or approximated, so each is pinned here.
#[test]
fn each_tezro_chip_resets_with_its_identity_and_clock() {
    fn check<C: CacheCpuModel + From<Arc<dyn BusDevice>>>(model: &Part, prid: u32, config0: u32, mhz: u32) {
        let board = board_with(model);
        let mut cpu = build_cpu::<C>(&board);
        let name = model.name();
        assert_eq!((cpu.core.cp0_prid, cpu.core.reset_prid), (prid, prid), "{name}");
        assert_eq!(model.prid(), prid, "{name}: the chip's PRId");
        assert_eq!((cpu.core.fpu_fir, cpu.core.reset_fir), (0x900, 0x900), "{name}");
        assert_eq!(cpu.core.cp0_config & CONFIG0_MODE_MASK, config0, "{name}");
        assert_eq!(cpu.core.cp0_config >> 26, 0b011_011, "{name}: 32 KB primary caches");
        let config = cpu.core.cp0_config;
        cpu.core.fpu_fir = 0;
        cpu.core.cp0_prid = 0;
        cpu.core.reset(false);
        assert_eq!((cpu.core.cp0_prid, cpu.core.fpu_fir, cpu.core.cp0_config), (prid, 0x900, config), "{name}: after reset");
        assert_eq!(model.mhz, mhz);
        assert_eq!(model.count_hz(), u64::from(mhz) * 500_000, "{name}: Count at half the clock");
        assert_eq!(chip(model).firmware.cpu_clock_hz, mhz * 1_000_000, "{name}: the PROM's clock record");
    }
    use crate::cpu::mips_cache_shadow::{R16000Scache16ShadowCache, R16000Scache8ShadowCache, R16000ShadowCache};
    on_big_stack(|| {
        check::<R16000ShadowCache>(&parts::R16000_700_EARLY, 0x0f41, 0x009b_ad85, 700);
        check::<R16000ShadowCache>(&parts::R16000_800, 0x0f42, 0x009b_af85, 800);
        check::<R16000Scache8ShadowCache>(&parts::R16000_900, 0x0f50, 0x009c_b185, 900);
        check::<R16000Scache16ShadowCache>(&parts::R16000_1000, 0x0f50, 0x00ed_b385, 1000);
    });
}

/// Every chip's Config word decodes, by the PROM editor's formulas, to the
/// fields its inventory states: SS gives the cache size, SC the printed cache
/// clock (2 * CPU / (SC + 1)) and EC the 200 MHz Bedrock (code 2 * CPU /
/// Bedrock - 1), with K0 5, PM 3, SB and BE set. The 1 GHz word is the only
/// approximated one, and this is what it is built from: SS 5, SC 5, EC 9.
#[test]
fn each_tezro_configs_fields_decode_to_the_stated_sizes_and_clocks() {
    for c in &CHIPS {
        let word = c.firmware.reset_mode;
        let name = c.chip.name();
        let field = |lo: u32, bits: u32| (word >> lo) & ((1 << bits) - 1);
        let mhz = c.chip.mhz;
        let bytes = crate::with_cpu!(c.chip, true, false, C => <C as MipsCache>::L2_SIZE as u32);
        assert_eq!((field(0, 3), field(7, 2), field(13, 1), field(15, 1)), (5, 3, 1, 1), "{name}: K0 PM SB BE");
        assert_eq!(field(9, 4), 2 * mhz / 200 - 1, "{name}: EC");
        assert_eq!(2 * mhz / (field(19, 3) + 1), c.scache_mhz, "{name}: SC gives the cache clock");
        assert_eq!(512u32 << 10 << field(16, 3), bytes, "{name}: SS gives the cache size");
        assert_eq!((word >> 23) & 1, 1, "{name}: bit 23 is set in every row of 500 MHz and up");
        assert_eq!(word >> 24, 0, "{name}: bit 24 clear");
    }
    let word = chip(&parts::R16000_1000).firmware.reset_mode;
    assert_eq!(((word >> 16) & 7, (word >> 19) & 7, (word >> 9) & 15, (word >> 22) & 1), (5, 5, 9, 1), "1 GHz SS SC EC and bit 22");
}

/// A part's revision, as hinv prints it, and the PRId register's raw one
/// differ for an R16000: the register's major is the displayed one plus 2 (the
/// rule Linux and OpenBSD apply). The raw bytes are written out here, not
/// derived, so this ties the Tezro's "2.1", "2.2", "3.0" and "3.0" to 0x0f41,
/// 0x0f42 and 0x0f50, the 1 GHz one also printed by the PROM banner as `f50`.
#[test]
fn every_parts_revision_is_the_one_its_prid_displays() {
    for (part, shown, raw) in
        CPUS.iter().zip(["2.1", "2.2", "3.0", "3.0"]).zip([0x41, 0x42, 0x50, 0x50]).map(|((p, s), r)| (p, s, r))
    {
        assert_eq!(format!("{}.{}", part.rev.0, part.rev.1), shown, "{}", part.label());
        assert_eq!(part.prid(), 0x0f00 | raw, "{}", part.label());
    }
}

/// A chip and its flash record are two statements of one chip: the clock,
/// and the Config word the PROM compares against the processor's, must agree.
#[test]
fn each_tezro_chip_agrees_with_its_flash_record() {
    for c in &CHIPS {
        let record = c.firmware;
        let name = c.chip.name();
        assert_eq!(u64::from(record.cpu_clock_hz), u64::from(c.chip.mhz) * 1_000_000, "{name}");
        assert_eq!(record.cpu_mode_word & CONFIG0_MODE_MASK, record.reset_mode, "{name}");
        assert_eq!(board_with(c.chip).chip.chip, c.chip);
        assert!(c.cpus.iter().all(|&n| n <= 4) && !c.cpus.is_empty(), "{name}");
    }
    assert_eq!(CHIPS.len(), CPUS.len());
    assert!(CHIPS.iter().zip(CPUS).all(|(c, &model)| c.chip == model), "CPUS lists the chips in table order");
}

/// The cache type `with_cpu!` builds follows the chip: the secondary-cache
/// size the chip is inventoried with, and nothing else about the type is the
/// chip's.
#[test]
fn the_processors_take_the_cache_type_of_the_chips_size() {
    for (model, mib) in [
        (&parts::R16000_700_EARLY, 4u32),
        (&parts::R16000_800, 4),
        (&parts::R16000_900, 8),
        (&parts::R16000_1000, 16),
    ] {
        let (name, bytes) = crate::with_cpu!(model, true, false, C => (<C as CacheCpuModel>::NAME, <C as MipsCache>::L2_SIZE as u32));
        assert_eq!((name, bytes >> 20), ("R16000", mib), "{}", model.name());
    }
}

/// A bare family name builds the 700 MHz chip, and a chip name its own:
/// PRId, flash record and Count rate follow, end to end from a configuration.
#[test]
fn the_family_name_builds_the_700_mhz_chip_and_a_chip_name_its_own() {
    on_big_stack(|| {
        for (cpu, prid, mhz, mib) in [("r16000", 0x0f41, 700, 4u32), ("r16000_800", 0x0f42, 800, 4), ("r16000_900", 0x0f50, 900, 8), ("r16000_1000", 0x0f50, 1000, 16)] {
            let cfg: MachineConfig = toml::from_str(&format!("[machine]\nprofile = \"tezro_ip53\"\ncpu = \"{cpu}\"\n")).unwrap();
            let part = cfg.cpu_part().unwrap();
            let board = board_with(part);
            let (core_prid, core_config, l2) = crate::with_cpu!(part, true, false, C => {
                let cpu_core = build_cpu::<C>(&board);
                (cpu_core.core.cp0_prid, cpu_core.core.cp0_config, <C as MipsCache>::L2_SIZE as u32)
            });
            assert_eq!(core_prid, prid, "{cpu}");
            assert_eq!(l2 >> 20, mib, "{cpu}: secondary cache");
            assert_eq!((core_config >> 16) & 7, 1 + mib.trailing_zeros(), "{cpu}: Config SS");
            assert_eq!(board.chip.firmware.cpu_clock_hz, mhz * 1_000_000, "{cpu}: the flash record");
            assert_eq!(board.chip.firmware.reset_config0(), core_config, "{cpu}: record and processor agree");
        }
    });
}

/// The poll returns lifted checked pages to the direct window once every
/// interval of board time while the board runs, not on every batch.
#[test]
fn the_poll_retires_lifted_pages_once_an_interval() {
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
        chip: &CHIPS[0],
        v12: false,
        graphics_pool: (2, 32),
        graphics_raster: (crate::config::RasterUnit::Cpu, "auto"),
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

// VPro host-interface registers the tests below touch, as byte offsets in
// the board's widget space. The board's own constants are private to it.
const ODSY_IDENTITY: u64 = 0x00;

fn board_with_v12(cpus: usize) -> TezroBoard {
    TezroBoard::with_config(TezroBoardConfig {
        payload: &[],
        clock: Arc::new(|| 0),
        flash_path: None,
        station_mac: L1Config::default().mac,
        timekeeper: Arc::new(Ds1744::new(YearBase::From(1968)).unwrap()),
        memory: &MemoryPopulation::default(),
        cpus,
        chip: &CHIPS[0],
        v12: true,
        graphics_pool: (2, 32),
        graphics_raster: (crate::config::RasterUnit::Cpu, "auto"),
    })
    .unwrap()
}

/// The V12 answers as widget 12 behind the PXBOW on either board, and
/// widget 12 stays empty without one. (On the one-processor board the
/// placement is inferred; see V12_WIDGET.)
#[test]
fn the_v12_answers_at_widget_12_on_both_boards() {
    for cpus in [1, 4] {
        let board = board_with_v12(cpus);
        let id = board.node.io.pio_read(widget(12, ODSY_IDENTITY), 4);
        let direct = board.v12.as_ref().unwrap().read32(ODSY_IDENTITY);
        assert_eq!((id.status, id.data), (BUS_OK, u64::from(direct.data)), "{cpus} CPUs");
        assert_eq!(board.pics[board.pics.len() - 1].pxbow().unwrap().endpoint_widgets() & (1 << 12), 1 << 12);
    }
    assert_eq!(TezroBoard::new(1).unwrap().node.io.pio_read(widget(12, ODSY_IDENTITY), 4).status, BUS_ERR);
}
