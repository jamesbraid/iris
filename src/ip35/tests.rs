use super::*;
use crate::traits::BUS_OK;

#[test]
fn startup_exit_remaps_eight_slots_and_reset_restores_the_private_view() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let clocks = Arc::new(AtomicU64::new(0));
    let count = clocks.clone();
    let board = crate::ip34_machine::FuelBoard::new(&[0; 16], Arc::new(move || {
        count.fetch_add(1, Ordering::Relaxed);
        0
    })).unwrap();
    let base = startup::WINDOW_BASE;
    let phys = board.md.physical();
    board.md.md_bus().write64(8, 0x33);
    assert_eq!(board.md.memory_view().write64(base, 0x2222222222222222), BUS_OK);
    let cached = board.cached_bus();
    let initial: Vec<_> = (0..8).map(|slot| phys.entry_at(base + slot * 4096)).collect();
    let outside = [phys.entry_at(base - 4096), phys.entry_at(base + 32768)];
    for _ in 0..2 {
        assert!(board.startup.observing());
        board.startup.observe(2, Some(0x1fc00000), 0);
        {
            let _scope = board.startup.begin_access();
            assert_eq!(cached.write8(base, 0x11), BUS_OK);
            assert_eq!(cached.read8(base).data, 0x11);
            assert_eq!(cached.dma_read64(base).data, 0x2222222222222222);
        }
        assert_eq!(cached.read8(base).data, 0x22, "debugger sees backing bytes");
        board.startup.observe(0, Some(0x200000), 0);
        assert!(!board.startup.observing());
        for slot in 0..8 {
            let entry = phys.entry_at(base + slot * 4096);
            assert!(!std::ptr::eq(entry, initial[slot as usize]));
            assert!(std::ptr::eq(entry, phys.entry_at(base)));
        }
        assert!(std::ptr::eq(outside[0], phys.entry_at(base - 4096)));
        assert!(std::ptr::eq(outside[1], phys.entry_at(base + 32768)));
        {
            let _scope = board.startup.begin_access();
            assert_eq!(cached.read8(base).data, 0x22);
        }
        let mut cpu = board.build_cpu().unwrap();
        cpu.core.pc = 0xffffffff80000000;
        let before = clocks.load(Ordering::Relaxed);
        assert_eq!(board.step_cpu(&mut cpu), 0);
        assert_eq!(clocks.load(Ordering::Relaxed), before, "completed startup never polls the clock");
        board.reset_startup();
        for slot in 0..8 {
            assert!(std::ptr::eq(phys.entry_at(base + slot * 4096), initial[slot as usize]));
        }
    }
    let md = Arc::downgrade(&board.md);
    drop(cached);
    drop(board);
    assert!(md.upgrade().is_none(), "startup remapping must not retain the board");
}

struct Word(u64);
impl BusDevice for Word {
    fn read64(&self, _: u64) -> BusRead64 {
        BusRead64::ok(self.0)
    }
}
#[test]
fn uc_spaces_keep_equal_offsets_distinct() {
    let mut bus = Ip35Bus::new(Physical::new(Vec::new()));
    for space in 0..4 {
        bus.map(
            space,
            0x0100_0000,
            0x1000,
            Arc::new(Word(0x100 + space as u64)),
        );
    }
    for space in 0..4 {
        let value = bus.read64(((space as u64) << 57) | 0x0100_0000);
        assert_eq!(value.status, BUS_OK);
        assert_eq!(value.data, 0x100 + space as u64);
    }
}
#[test]
fn an_access_cannot_cross_a_region_boundary() {
    let mut bus = Ip35Bus::new(Physical::new(Vec::new()));
    bus.map(2, 0x0100_0000, 4, Arc::new(Word(7)));
    assert_eq!(bus.read64((2 << 57) | 0x0100_0000).status, BUS_ERR);
    assert!(bus.first_fault().unwrap().contains("width=8"));
}
#[test]
fn unsupported_request_bits_are_not_silently_masked() {
    let bus = Ip35Bus::new(Physical::new(Vec::new()));
    assert_eq!(bus.read64(1 << 50).status, BUS_ERR);
}

struct ResetRom;
impl BusDevice for ResetRom {
    fn read32(&self, addr: u64) -> BusRead32 {
        match addr {
            0 => BusRead32::ok(0x2408_002a), // addiu t0,zero,42
            4 => BusRead32::ok(0x2509_0001), // addiu t1,t0,1
            _ => BusRead32::err(),
        }
    }
}
#[test]
#[cfg(not(feature = "opcodefusion"))]
fn native_constructor_executes_reset_rom() {
    let mut bus = Ip35Bus::new(Physical::new(Vec::new()));
    bus.map(0, 0x1fc0_0000, 8, Arc::new(ResetRom));
    let mut cpu = crate::ip34_machine::build_cpu(Arc::new(bus)).unwrap();
    assert_eq!(cpu.step_int(), 0);
    assert_eq!(cpu.step_int(), 0);
    assert_eq!(cpu.core.gpr[8], 42);
    assert_eq!(cpu.core.gpr[9], 43);
}
#[test]
#[cfg(not(feature = "opcodefusion"))]
fn xkphys_uc_bits_survive_the_board_cpu_mask() {
    let bus = Arc::new(Ip35Bus::new(Physical::new(Vec::new())));
    let mut cpu = crate::ip34_machine::build_cpu(bus).unwrap();
    cpu.core.write_cp0(
        12,
        (cpu.core.cp0_status | crate::cpu::mips_core::STATUS_KX) as u64,
    );
    for raw_uc in 0..4u64 {
        let va = 0x9000_0000_0100_0020 | (raw_uc << 57);
        for _ in 0..2 {
            let translated = cpu.debug_translate(va);
            assert_eq!(translated.status, crate::cpu::mips_exec::TR_UNCACHED);
            assert_eq!(translated.phys, (raw_uc << 57) | 0x0100_0020);
        }
    }
}

#[test]
fn fuel_flash_command_windows_share_states_and_execution_bytes() {
    let board = crate::ip34_machine::FuelBoard::new(&[], crate::dev::heart::host_clock()).unwrap();
    let bus = &board.bus;
    const LOCAL: u64 = 0x1800_0000;
    const REMOTE: u64 = 0x3800_0000;
    const SECOND: u64 = 0x0200_0000;
    // The real PROM first uses this reset cycle at the second selection.
    assert_eq!(bus.write64(LOCAL + SECOND, 0xf0), BUS_OK);
    for part in [0, SECOND] {
        assert_eq!(bus.write64(LOCAL + part + 8 * 0x55, 0x98), BUS_OK);
        assert_eq!(bus.read16(REMOTE + part + 0x20).data, b'Q' as u16);
        assert_eq!(bus.read16(0x1fc0_0020).data, 0xffff);
        assert_eq!(bus.write64(REMOTE + part, 0xf0), BUS_OK);
        for (offset, value) in [
            (8 * 0x555, 0xaa),
            (8 * 0x2aa, 0x55),
            (8 * 0x555, 0xa0),
            (4 * 0x200, 0x1234),
        ] {
            assert_eq!(bus.write64(LOCAL + part + offset, value), BUS_OK);
        }
        assert_eq!(bus.read16(REMOTE + part + 0x200).data, 0x1234);
    }
    assert_eq!(bus.read16(0x1fc0_0200).data, 0x1234);
    assert_eq!(bus.write64(0x1fc0_0000 + 8 * 0x555, 0xaa), BUS_ERR);
    assert_eq!(board.flash.contents()[0x200..0x202], [0x12, 0x34]);
}

#[test]
#[cfg(not(feature = "opcodefusion"))]
fn fuel_programmed_instructions_are_fetched_without_restarting_machine() {
    let board = crate::ip34_machine::FuelBoard::new(
        &[0x24, 0x08, 0, 0x2a],
        crate::dev::heart::host_clock(),
    )
    .unwrap();
    let mut cpu = board.build_cpu().unwrap();
    assert_eq!(cpu.step_int(), 0);
    assert_eq!(cpu.core.gpr[8], 42);
    for (offset, value) in [
        (8 * 0x555, 0xaa),
        (8 * 0x2aa, 0x55),
        (8 * 0x555, 0xa0),
        (4 * 2, 0x0028),
    ] {
        assert_eq!(board.bus.write64(0x1800_0000 + offset, value), BUS_OK);
    }
    cpu.core.pc = 0xffff_ffff_bfc0_0000;
    assert_eq!(cpu.step_int(), 0);
    assert_eq!(cpu.core.gpr[8], 40);
}

#[test]
#[ignore = "requires a caller-supplied raw IP35 PROM; retains raw execution evidence"]
#[cfg(not(feature = "opcodefusion"))]
fn stock_prom_first_fault() {
    use std::fmt::Write as _;
    let path =
        std::env::var("IRIS_FUEL_PROM").expect("IRIS_FUEL_PROM must name the raw IP35 payload");
    let output = std::path::PathBuf::from(
        std::env::var("IRIS_FUEL_EVIDENCE")
            .expect("IRIS_FUEL_EVIDENCE must name managed evidence storage"),
    );
    let image = std::fs::read(path).unwrap();
    let board =
        crate::ip34_machine::FuelBoard::new(&image, crate::dev::heart::host_clock()).unwrap();
    let console = board.serial[0].clone();
    let mut cpu = board.build_cpu().unwrap();
    let mut recent = std::collections::VecDeque::new();
    let mut serial = Vec::new();
    let mut report = String::new();
    let limit = std::env::var("IRIS_FUEL_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000u64);
    let seconds = std::env::var("IRIS_FUEL_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60u64);
    let start = std::time::Instant::now();
    writeln!(
        report,
        "initial pc={:#018x} prid={:#x} config={:#x} status={:#x} mask={:#x}",
        cpu.core.pc, cpu.core.cp0_prid, cpu.core.cp0_config, cpu.core.cp0_status, cpu.phys_mask
    )
    .unwrap();
    board.start();
    for step in 0..limit {
        if recent.len() == 64 {
            recent.pop_front();
        }
        recent.push_back((step, cpu.core.pc, cpu.core.gpr[29], cpu.core.fpr[0], cpu.core.cp0_taghi, cpu.core.cp0_taglo));
        let cause = cpu.core.cp0_cause;
        cpu.core
            .hot
            .interrupts
            .try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |value| Some((value & !0x7c00) | board.irq.bits()),
            )
            .unwrap();
        let status = board.step_cpu(&mut cpu);
        if step % 1024 == 0 {
            board.poll();
            serial.extend(console.drain_guest());
        }
        let menu = serial
            .windows(b"System Maintenance Menu".len())
            .any(|w| w == b"System Maintenance Menu");
        if status != 0
            || board.bus.first_fault().is_some()
            || (cpu.core.cp0_cause ^ cause) & 0x7c != 0
            || menu
            || step + 1 == limit
            || start.elapsed().as_secs() >= seconds
        {
            writeln!(report,"stop step={step} status={status:#x} pc={:#018x} cause={:#x} epc={:#018x} badvaddr={:#018x} fpr0={:#018x} fault={:?} menu={menu}",cpu.core.pc,cpu.core.cp0_cause,cpu.core.cp0_epc,cpu.core.cp0_badvaddr,cpu.core.fpr[0],board.bus.first_fault()).unwrap();
            break;
        }
    }
    board.stop();
    serial.extend(console.drain_guest());
    for (step, pc, sp, fpr0, taghi, taglo) in recent {
        writeln!(report, "PC {step} pc={pc:#018x} sp={sp:#018x} fpr0={fpr0:#018x} taghi={taghi:#x} taglo={taglo:#x}").unwrap();
    }
    for (i, value) in cpu.core.gpr.iter().enumerate() {
        writeln!(report, "GPR {i} {value:#018x}").unwrap();
    }
    for item in board.bus.recent_accesses() {
        writeln!(report, "BUS {item}").unwrap();
    }
    for (i, pi) in board.pi.iter().enumerate() {
        writeln!(report, "PI{i} unsupported={:?}", pi.unsupported()).unwrap();
    }
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("execution.txt"), &report).unwrap();
    std::fs::write(output.join("l1.raw"), &serial).unwrap();
    std::fs::write(output.join("ioc3-a.raw"), board.serial[1].drain_guest()).unwrap();
    std::fs::write(output.join("ioc3-b.raw"), board.serial[2].drain_guest()).unwrap();
    eprintln!("{report}");
    assert!(
        serial
            .windows(b"System Maintenance Menu".len())
            .any(|w| w == b"System Maintenance Menu"),
        "stock PROM did not reach the maintenance menu; see retained execution and L1 evidence"
    );
}

#[test]
fn fuel_board_identity_and_one_cpu_population() {
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    for base in [0x0100_0000, 0x0180_0000] {
        assert_eq!(board.bus.read64((1 << 57) | base | 0x20).data, 0);
        assert_eq!(board.bus.read64((1 << 57) | base | 0x40).data, 1);
        assert_eq!(board.bus.read64((1 << 57) | base | 0x48).data, 0);
        assert_eq!(
            board.bus.read64((1 << 57) | (base + 0x20_0000) | 0x40).data,
            0
        );
    }
    assert_eq!(board.bus.read64(0x1400_0000).data, 0x4004);
    assert_eq!(board.bus.read64(0x3400_0000).data, 0x4004);
    assert_eq!(board.bus.read32(0x1400_0000).status, BUS_ERR);
    assert_eq!(board.bus.write64(0x1400_0000, 0), BUS_ERR);
}
#[test]
fn fuel_board_leds_share_local_and_remote_views() {
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    assert_eq!(board.bus.write64(0x1000_00c0, 0x1234), BUS_OK);
    assert_eq!(board.bus.read64(0x3000_00c0).data, 0x34);
    assert_eq!(board.bus.write32(0x1000_00c0, 0), BUS_ERR);
    assert_eq!(board.bus.read64(0x1000_00c0).data, 0x34);
}

#[test]
fn a_short_register_region_cannot_fall_through_to_pio() {
    let mut bus = Ip35Bus::new(Physical::new(Vec::new()));
    bus.set_fallback(1, 1 << 32, Arc::new(Word(0xfeed)));
    bus.map(1, 0x1000, 4, Arc::new(Word(1)));
    assert_eq!(bus.read64((1 << 57) | 0x1000).status, BUS_ERR);
    assert_eq!(bus.read64((1 << 57) | 0x2000).data, 0xfeed);
}

struct Instructions(Vec<u32>);
impl BusDevice for Instructions {
    fn read32(&self, a: u64) -> BusRead32 {
        self.0
            .get((a / 4) as usize)
            .copied()
            .map(BusRead32::ok)
            .unwrap_or_else(BusRead32::err)
    }
}
struct Data32(u32);
impl BusDevice for Data32 {
    fn read32(&self, _: u64) -> BusRead32 {
        BusRead32::ok(self.0)
    }
}
#[test]
#[cfg(not(feature = "opcodefusion"))]
fn cached_and_uncached_views_use_existing_executor_connections() {
    let physical = Physical::new(Vec::new());
    let mut uncached = Ip35Bus::new(physical.clone());
    uncached.map(
        0,
        0x1fc0_0000,
        16,
        Arc::new(Instructions(vec![
            0x8d0a0000, 0x8d2b0000, 0x8d0c0000, 0x8d2d0000,
        ])),
    );
    uncached.map(0, 0x1000, 4, Arc::new(Data32(0x1234)));
    let mut cached = Ip35Bus::new(physical);
    cached.map(0, 0x1000, 4, Arc::new(Data32(0x5678)));
    let mut cpu = crate::ip34_machine::build_cpu(Arc::new(uncached)).unwrap();
    cpu.cache = crate::ip34_machine::FuelShadowCache::new(Arc::new(cached));
    cpu.core
        .write_cp0(16, (cpu.core.cp0_config & !7 | 3) as u64);
    cpu.resync_privilege_state();
    cpu.core.gpr[8] = 0xffff_ffff_8000_1000;
    cpu.core.gpr[9] = 0xffff_ffff_a000_1000;
    for _ in 0..4 {
        assert_eq!(cpu.step_int(), 0);
    }
    assert_eq!(cpu.core.gpr[10], 0x5678);
    assert_eq!(cpu.core.gpr[11], 0x1234);
    assert_eq!(cpu.core.gpr[12], 0x5678);
    assert_eq!(cpu.core.gpr[13], 0x1234);
}

#[test]
#[cfg(not(feature = "opcodefusion"))]
fn reset_config_matches_fuel_flash_mode() {
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    let mut cpu = crate::ip34_machine::build_cpu(board.bus.clone()).unwrap();
    let reference = board.bus.read32(0x1800_0064);
    assert_eq!(reference.status, BUS_OK);
    assert_eq!(
        cpu.core.cp0_config & 0x01ff_ffff,
        reference.data & 0x01ff_ffff
    );
    cpu.core.gpr[8] = reference.data as u64;
    // mtc0 t0,Config: the early firmware comparison preserves boot inputs.
    assert_eq!(cpu.exec(0x4088_8000), 0);
    assert_eq!(
        cpu.core.cp0_config & 0x01ff_ffff,
        reference.data & 0x01ff_ffff
    );
}

#[test]
fn inbound_interrupts_respect_each_pi_io_protection() {
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    for interface in 0..2 {
        let pi = &board.pi[interface];
        let address = (1 << 47) | 0x0180_0090 | ((interface as u64) << 21);
        assert_eq!(pi.write64(0x10, 0), BUS_OK);
        assert_eq!(
            board.io.inbound_write(15, Some(4), address, 8, 0x10c),
            BUS_ERR
        );
        assert_eq!(pi.read64(0x98).data & (1 << 12), 0);
        assert_eq!(pi.write64(0x10, 1), BUS_OK);
        assert_eq!(
            board.io.inbound_write(15, Some(4), address, 8, 0x10c),
            BUS_OK
        );
        assert_ne!(pi.read64(0x98).data & (1 << 12), 0);
        assert_eq!(pi.write64(0x10, 0), BUS_OK);
        assert_eq!(pi.write64(0x90, 12), BUS_OK);
        assert_eq!(pi.read64(0x98).data & (1 << 12), 0);
    }
}

#[test]
#[cfg(not(feature = "opcodefusion"))]
fn fuel_cache_geometry_matches_selected_population() {
    use crate::cpu::mips_cache_v2::{MipsCache, CACH_PD, CACH_PI, CACH_SD};
    let cpu =
        crate::ip34_machine::build_cpu(Arc::new(Ip35Bus::new(Physical::new(Vec::new())))).unwrap();
    assert_eq!(cpu.cache.get_config(CACH_PI), (32768, 64));
    assert_eq!(cpu.cache.get_config(CACH_PD), (32768, 32));
    assert_eq!(cpu.cache.get_config(CACH_SD), (2 * 1024 * 1024, 128));
    assert_eq!(cpu.core.cp0_prid, 0xf23);
    assert_eq!(cpu.core.reset_prid, 0xf23);
    assert_eq!(cpu.core.fpu_fir, 0x900);
}

#[test]
#[cfg(not(feature = "opcodefusion"))]
fn startup_private_bytes_precede_dram_ready() {
    let board =
        crate::ip34_machine::FuelBoard::with_startup_home(&[], Arc::new(|| 0), 0x1686a8).unwrap();
    let mut cpu = board.build_cpu().unwrap();
    cpu.core.fpr[0] = 2;
    board.observe_cpu(&mut cpu);
    let _scope = board.startup.begin_access();
    let cached = board.cached_bus();
    assert_eq!(cached.write64(0x103f78, 0x123456789abcdef0), BUS_OK);
    assert_eq!(cached.read64(0x103f78).data, 0x123456789abcdef0);
    assert_eq!(board.bus.read64(0x103f78).status, BUS_ERR);
    assert_eq!(board.md.dma_view().read64(0x103f78).status, BUS_ERR);
}

#[test]
#[cfg(not(feature = "opcodefusion"))]
fn startup_observer_uses_translated_copy_home_and_debug_reads_do_not_latch() {
    use crate::cpu::mips_tlb::{Tlb, TlbEntry};
    let board =
        crate::ip34_machine::FuelBoard::with_startup_home(&[], Arc::new(|| 0), 0x1686a8).unwrap();
    let mut cpu = board.build_cpu().unwrap();
    cpu.core.fpr[0] = 2;
    board.observe_cpu(&mut cpu);
    // A cached debug read must not create a private phase while DRAM is absent.
    assert!(cpu.debug_read(0xffffffff80100000, 8).is_err());
    let mut entry = TlbEntry::new();
    entry.entry_hi = 0xffffffffc1000000;
    entry.entry_lo = [(0x01a00000 >> 6) | 0x1f, (0x01a01000 >> 6) | 0x1f];
    cpu.tlb.write(0, entry);
    cpu.core.cp0_status &= !crate::cpu::mips_core::STATUS_ERL;
    cpu.resync_privilege_state();
    cpu.core.pc = 0xffffffffc1000000;
    assert_eq!(cpu.debug_translate(cpu.core.pc).phys, 0x01a00000);
    board.observe_cpu(&mut cpu);
    {
        let _scope = board.startup.begin_access();
        assert_eq!(board.cached_bus().write64(0x100000, 0x1234), BUS_OK);
    }
    cpu.core.fpr[0] = 0;
    board.observe_cpu(&mut cpu);
    {
        let _scope = board.startup.begin_access();
        assert_eq!(board.cached_bus().read64(0x100000).data, 0x1234);
    }
    cpu.core.pc = 0xffffffff82000000;
    board.observe_cpu(&mut cpu);
    let _scope = board.startup.begin_access();
    assert_eq!(board.cached_bus().read64(0x100000).status, BUS_ERR);
}

#[test]
#[cfg(not(feature = "opcodefusion"))]
fn startup_step_wrapper_routes_synthetic_cpu_store_and_load() {
    // sd t1,0(t0); ld t2,0(t0), ordinary public MIPS instructions.
    let payload: Vec<u8> = [0xfd090000u32, 0xdd0a0000]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect();
    let board = crate::ip34_machine::FuelBoard::new(&payload, Arc::new(|| 0)).unwrap();
    let mut cpu = board.build_cpu().unwrap();
    cpu.core.fpr[0] = 2;
    cpu.core.gpr[8] = 0xffffffff80103f78;
    cpu.core.gpr[9] = 0x123456789abcdef0;
    assert_eq!(board.step_cpu(&mut cpu), 0);
    assert_eq!(board.step_cpu(&mut cpu), 0);
    assert_eq!(cpu.core.gpr[10], 0x123456789abcdef0);
    assert!(cpu.debug_read(0xffffffff80103f78, 8).is_err());
    assert_eq!(board.bus.read64(0x103f78).status, BUS_ERR);
}

#[test]
fn physical_map_slots_own_dispatch() {
    let physical = Physical::new(Vec::new());
    physical.set_default(Arc::new(Word(0)), "fixture default");
    let mut bus = Ip35Bus::new(physical.clone());
    bus.map(0, 0x1000, 0x1000, Arc::new(Word(0x55)));
    assert_eq!(bus.views[0].read64(0x1000).data, 0x55);
    assert_eq!(physical.read64(0x1000).status, BUS_ERR);
    assert_eq!(bus.read64(0x1000).data, 0x55);
}

#[test]
#[cfg(not(feature = "opcodefusion"))]
fn physical_startup_slots_share_only_the_cached_bank_owner() {
    use crate::ppmem::devmap::{entry_eq, SLOT_SHIFT};
    let board =
        crate::ip34_machine::FuelBoard::with_startup_home(&[], Arc::new(|| 0), 1476264).unwrap();
    assert!(Arc::ptr_eq(&board.bus.phys, board.md.physical()));
    assert_eq!(board.bus.phys.banks().len(), 2);
    for view in &board.bus.views {
        assert!(view.banks().is_empty());
    }
    let entry = board.bus.phys.entry_at(0x100000);
    for page in 0..8 {
        assert!(entry_eq(
            entry,
            board.bus.phys.entry_at(0x100000 + (page << SLOT_SHIFT))
        ));
    }
    assert!(!entry_eq(entry, board.bus.phys.entry_at(0x0ff000)));
    assert!(!entry_eq(entry, board.bus.phys.entry_at(0x108000)));
    assert_eq!(board.md.md_bus().write64(8, 0x33), BUS_OK);
    let plain = board.md.memory_view();
    let dma = board.md.dma_view();
    let mut cpu = board.build_cpu().unwrap();
    cpu.core.fpr[0] = 2;
    board.observe_cpu(&mut cpu);
    let _scope = board.startup.begin_access();
    let cached = board.cached_bus();
    for page in 0..8 {
        let at = 0x100000 + (page << SLOT_SHIFT);
        assert_eq!(plain.write64(at, 0x100 + page), BUS_OK);
        assert_eq!(cached.write64(at, 0x200 + page), BUS_OK);
        assert_eq!(cached.read64(at).data, 0x200 + page);
        assert_eq!(board.bus.read64(at).data, 0x100 + page);
        assert_eq!(board.bus.read64((3 << 57) | at).data, 0x100 + page);
        assert_eq!(dma.read64(at).data, 0x100 + page);
        assert_eq!(cached.dma_read64(at).data, 0x100 + page);
        assert_eq!(board.bus.write64(at, 0x300 + page), BUS_OK);
        assert_eq!(board.bus.write64((3 << 57) | at, 0x400 + page), BUS_OK);
        assert_eq!(dma.write64(at, 0x500 + page), BUS_OK);
        assert_eq!(plain.read64(at).data, 0x500 + page);
        assert_eq!(cached.read64(at).data, 0x200 + page);
    }
    assert!(cached.mem_ptr(0x100000).is_none());
}
#[test]
fn physical_cached_plane_retains_full_md_decode_without_extra_banks() {
    use crate::ppmem::devmap::entry_eq;
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    assert_eq!(board.md.md_bus().write64(8, 0x33), BUS_OK);
    let at = (1 << 30) + 0x1000;
    assert_eq!(board.md.memory_view().write64(at, 0x12345678), BUS_OK);
    assert_eq!(board.cached_bus().read64(at).data, 0x12345678);
    assert_eq!(board.bus.read64((3 << 57) | at).data, 0x12345678);
    assert_eq!(board.bus.read64(at).status, BUS_ERR);
    assert!(entry_eq(
        board.bus.phys.entry_at(at),
        board.bus.phys.entry_at((7 << 30) + 0x1000)
    ));
    assert_eq!(board.md.dma_view().read64(at).data, 0x12345678);
}
#[test]
fn physical_mixed_hub_page_does_not_alias_neighboring_registers() {
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    for base in [0x10000000, 0x30000000] {
        assert_eq!(board.bus.read64(base + 0x78).status, BUS_OK);
        assert_eq!(board.bus.read64(base + 0x7c).status, BUS_ERR);
        assert_eq!(board.bus.read64(base + 0xbc).status, BUS_ERR);
        assert_eq!(board.bus.write64(base + 0xd8, 0x55), BUS_OK);
        assert_eq!(board.bus.read64(base + 0xd8).data, 0x55);
        assert_eq!(board.bus.read64(base + 0xdc).status, BUS_ERR);
        assert_eq!(board.bus.read8(base + 0xe0).status, BUS_ERR);
        assert_eq!(board.bus.read64(base + 0x1000).status, BUS_ERR);
        assert_eq!(board.bus.read64(base + 0x4000000 + 8).status, BUS_ERR);
    }
    assert_eq!(
        board.registers.read64(crate::dev::bedrock_pi::SIZE).status,
        BUS_ERR
    );
}

#[test]
fn physical_cached_plane_excludes_uncached_hub_aliases_and_rejects_metadata() {
    use crate::ppmem::devmap::entry_eq;
    let board = crate::ip34_machine::FuelBoard::new(&[0; 8], Arc::new(|| 0)).unwrap();
    assert_eq!(board.md.md_bus().write64(8, 0x33), BUS_OK);
    let cached = board.cached_bus();
    for at in [
        0x10000000, 0x14000000, 0x18000000, 0x1a000000, 0x30000000, 0x38000000,
    ] {
        assert_eq!(board.bus.read64(at).status, BUS_OK);
        assert_eq!(cached.read64(at).status, BUS_ERR);
    }
    let normal = board.bus.phys.entry_at(0x40001000);
    assert!(entry_eq(normal, board.bus.phys.entry_at(0x1c0001000)));
    assert!(!entry_eq(normal, board.bus.views[0].entry_at(0x1c0001000)));
    assert_eq!(cached.read64(0x1c0001000).status, BUS_ERR);
    assert_eq!(board.bus.read64(0x1c0001000).status, BUS_OK);
    assert_eq!(cached.read32(0x1fc00000).status, BUS_OK);
    for raw in 1..4 {
        assert_eq!(cached.read64((raw << 57) | 0x100000).status, BUS_ERR);
        assert_eq!(cached.read32((raw << 57) | 0x1fc00000).status, BUS_ERR);
    }
}
#[test]
fn physical_board_drop_releases_md_and_its_bank_owner() {
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    let md = Arc::downgrade(&board.md);
    let physical = Arc::downgrade(&board.bus.phys);
    drop(board);
    assert!(md.upgrade().is_none());
    assert!(physical.upgrade().is_none());
}
#[test]
#[cfg(not(feature = "opcodefusion"))]
fn physical_cached_cpu_connection_rejects_unaligned_window_crossings() {
    let board =
        crate::ip34_machine::FuelBoard::with_startup_home(&[], Arc::new(|| 0), 1476264).unwrap();
    assert_eq!(board.md.md_bus().write64(8, 0x33), BUS_OK);
    let mut cpu = board.build_cpu().unwrap();
    cpu.core.fpr[0] = 2;
    board.observe_cpu(&mut cpu);
    let _scope = board.startup.begin_access();
    let cached = board.cached_bus();
    assert_eq!(cached.write64(0x100000, 0x12345678), BUS_OK);
    assert_eq!(cached.write32(0x0ffffe, 0xabcdef01), BUS_ERR);
    assert_eq!(cached.write32(0x107ffe, 0xabcdef01), BUS_ERR);
    assert_eq!(cached.read32(0x0ffffe).status, BUS_ERR);
    assert_eq!(cached.read32(0x107ffe).status, BUS_ERR);
    assert_eq!(cached.read64(0x100000).data, 0x12345678);
    assert_eq!(board.md.memory_view().read64(0x100000).data, 0);
}

#[test]
fn physical_cached_reset_rom_has_exact_payload_bounds_in_its_last_slot() {
    let payload = vec![0x5a; 1476264];
    // The fixture PC home is deliberately independent of its ROM extent.
    let board = crate::ip34_machine::FuelBoard::with_startup_home(&payload, Arc::new(|| 0), 0x1000)
        .unwrap();
    let cached = board.cached_bus();
    let end = 0x1fc00000 + payload.len() as u64;
    assert_eq!(cached.read8(end - 1).data, 0x5a);
    assert_eq!(cached.read64(end - 8).data, 0x5a5a5a5a5a5a5a5a);
    assert_eq!(cached.read8(end).status, BUS_ERR);
    assert_eq!(cached.read8(end + 16).status, BUS_ERR);
    assert_eq!(board.bus.read8(end).status, BUS_OK);
    drop(board);
    let empty = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    assert_eq!(empty.cached_bus().read32(0x1fc00000).status, BUS_ERR);
}

mod memory_error_tests {
    use crate::ip34_machine::FuelBoard;
    use crate::traits::{BusDevice, BUS_ERR, BUS_OK};
    use std::sync::Arc;

    #[test]
    fn fuel_memory_regression_correctable_errors_notify_both_interfaces() {
        let board = FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
        let correctable = 1 << 61;
        board.pi[0].write64(0xb0, correctable);
        board.md.md_bus().write64(8, 0x33);
        for directory in [false, true] {
            if directory {
                board.md.md_bus().write64(0x40000, 2);
                board.md.directory_bus().write64(0, 1 << 16);
                board.md.md_bus().write64(0x40000, 0);
            } else {
                board.md.ecc_bus().write8(7, 1);
            }
            assert_eq!(board.bus.read64(0).status, BUS_OK);
            for pi in &board.pi {
                assert_eq!(pi.read64(0xa0).data & correctable, correctable);
            }
            assert_eq!(board.irq.bits() & (1 << 11), 1 << 11);
            for pi in &board.pi {
                pi.write64(0x90, 64 + 61);
                assert_eq!(pi.read64(0xa0).data & correctable, 0);
            }
            assert_eq!(board.irq.bits() & (1 << 11), 0);
        }
    }

    #[test]
    fn fuel_memory_regression_uncached_uce_fails_in_both_memory_spaces() {
        let board = FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
        board.md.md_bus().write64(8, 0x33);
        board.md.ecc_bus().write8(7, 3);
        for selector in [0, 3] {
            assert_eq!(board.bus.read64(selector << 57).status, BUS_ERR);
        }
        assert_eq!(board.cached_bus().read64(0).status, BUS_OK);
        assert_eq!(board.md.dma_view().read64(0).status, BUS_OK);
    }
}


#[test]
fn fuel_link_reset_resets_the_attached_scsi_engine() {
    use crate::dev::isp1040::{mbox, reg, CTRL_INT_ENABLE, CTRL_RISC_INT_ENABLE, HOST_CMD_SHIFT};
    use crate::pci::PciBusDevice;
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    let isp = &board.scsi;
    isp.config().write(1, 6);
    isp.config().write(5, 0x02000000);
    isp.bar_write(1, reg::BUS_CTRL, u32::from(CTRL_INT_ENABLE | CTRL_RISC_INT_ENABLE), 2);
    isp.bar_write(1, reg::MAILBOX_BASE, u32::from(mbox::NOP), 2);
    isp.bar_write(1, reg::HOST_CMD, 0x5 << HOST_CMD_SHIFT, 2);
    assert_eq!(isp.bar_read(1, reg::MAILBOX_BASE, 2), u32::from(mbox::COMPLETE));
    assert_ne!(isp.bar_read(1, reg::BUS_CTRL, 2), 0);
    assert_eq!(board.xbridge.widget(0).unwrap().write32(0x2f4, 1), BUS_OK);
    assert_eq!(isp.config().command(), 0);
    assert_eq!(isp.config().bar_base(1), 0);
    assert_eq!(isp.bar_read(1, reg::BUS_CTRL, 2), 0);
    assert_eq!(isp.bar_read(1, reg::MAILBOX_BASE, 2), 0);
    assert_eq!(isp.bar_read(1, reg::MAILBOX_BASE + 2, 2), 0x4953);
}


#[test]
fn configured_station_address_reaches_the_l1_identity() {
    let mac = [0x08, 0x00, 0x69, 0x21, 0x43, 0x65];
    let board = crate::ip34_machine::FuelBoard::with_storage(&[], Arc::new(|| 0), 0, None, mac).unwrap();
    assert_eq!(board.l1.station_mac(), mac);
}
