use super::*;
use crate::traits::BUS_OK;
struct Word(u64);
impl BusDevice for Word {
    fn read64(&self, _: u64) -> BusRead64 {
        BusRead64::ok(self.0)
    }
}
#[test]
fn uc_spaces_keep_equal_offsets_distinct() {
    let mut bus = Ip34Bus::new(Physical::new(Vec::new()));
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
    let mut bus = Ip34Bus::new(Physical::new(Vec::new()));
    bus.map(2, 0x0100_0000, 4, Arc::new(Word(7)));
    assert_eq!(bus.read64((2 << 57) | 0x0100_0000).status, BUS_ERR);
    assert!(bus.first_fault().unwrap().contains("width=8"));
}
#[test]
fn unsupported_request_bits_are_not_silently_masked() {
    let bus = Ip34Bus::new(Physical::new(Vec::new()));
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
    let mut bus = Ip34Bus::new(Physical::new(Vec::new()));
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
    let bus = Arc::new(Ip34Bus::new(Physical::new(Vec::new())));
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
    let mut cpu = crate::ip34_machine::build_cpu(board.bus.clone()).unwrap();
    let mut recent = std::collections::VecDeque::new();
    let mut serial = Vec::new();
    let mut report = String::new();
    let limit = std::env::var("IRIS_FUEL_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000u64);
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
        recent.push_back(format!(
            "{step} pc={:#018x} sp={:#018x} fpr0={:#018x} taghi={:#x} taglo={:#x} gpr2_15={:x?}",
            cpu.core.pc,
            cpu.core.gpr[29],
            cpu.core.fpr[0],
            cpu.core.cp0_taghi,
            cpu.core.cp0_taglo,
            &cpu.core.gpr[2..16]
        ));
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
        let status = cpu.step_int();
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
            || start.elapsed().as_secs() >= 60
        {
            writeln!(report,"stop step={step} status={status:#x} pc={:#018x} cause={:#x} epc={:#018x} badvaddr={:#018x} fpr0={:#018x} fault={:?} menu={menu}",cpu.core.pc,cpu.core.cp0_cause,cpu.core.cp0_epc,cpu.core.cp0_badvaddr,cpu.core.fpr[0],board.bus.first_fault()).unwrap();
            break;
        }
    }
    board.stop();
    serial.extend(console.drain_guest());
    for item in recent {
        writeln!(report, "PC {item}").unwrap();
    }
    for (i, value) in cpu.core.gpr.iter().enumerate() {
        writeln!(report, "GPR {i} {value:#018x}").unwrap();
    }
    for item in board.bus.recent_accesses() {
        writeln!(report, "BUS {item}").unwrap();
    }
    for item in board.registers.recent_accesses() {
        writeln!(report, "REG {item}").unwrap();
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
    let mut bus = Ip34Bus::new(Physical::new(Vec::new()));
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
    let mut uncached = Ip34Bus::new(physical.clone());
    uncached.map(
        0,
        0x1fc0_0000,
        16,
        Arc::new(Instructions(vec![
            0x8d0a0000, 0x8d2b0000, 0x8d0c0000, 0x8d2d0000,
        ])),
    );
    uncached.map(0, 0x1000, 4, Arc::new(Data32(0x1234)));
    let mut cached = Ip34Bus::new(physical);
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
        crate::ip34_machine::build_cpu(Arc::new(Ip34Bus::new(Physical::new(Vec::new())))).unwrap();
    assert_eq!(cpu.cache.get_config(CACH_PI), (32768, 64));
    assert_eq!(cpu.cache.get_config(CACH_PD), (32768, 32));
    assert_eq!(cpu.cache.get_config(CACH_SD), (2 * 1024 * 1024, 128));
    assert_eq!(cpu.core.cp0_prid, 0xf23);
    assert_eq!(cpu.core.reset_prid, 0xf23);
    assert_eq!(cpu.core.fpu_fir, 0x900);
}
