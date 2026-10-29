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
    let physical = Arc::downgrade(board.md.physical());
    drop(cached);
    drop(board);
    assert!(md.upgrade().is_none(), "startup remapping must not retain the board");
    assert!(physical.upgrade().is_none());
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
fn cached_and_uncached_memory_keep_high_addresses() {
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    assert_eq!(board.md.md_bus().write64(8, 0x33), BUS_OK);
    let at = (1 << 30) + 0x1000;
    assert_eq!(board.md.memory_view().write64(at, 0x12345678), BUS_OK);
    assert_eq!(board.cached_bus().read64(at).data, 0x12345678);
    assert_eq!(board.bus.read64((3 << 57) | at).data, 0x12345678);
    assert_eq!(board.bus.read64(at).status, BUS_ERR);
    assert_eq!(board.md.dma_view().read64(at).data, 0x12345678);
}

#[test]
fn memory_errors_reach_both_interfaces_and_uncached_reads_fail() {
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
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

    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    board.md.md_bus().write64(8, 0x33);
    board.md.ecc_bus().write8(7, 3);
    for selector in [0, 3] {
        assert_eq!(board.bus.read64(selector << 57).status, BUS_ERR);
    }
    assert_eq!(board.cached_bus().read64(0).status, BUS_OK);
    assert_eq!(board.md.dma_view().read64(0).status, BUS_OK);
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
fn configured_identity_and_input_attachments_reach_the_devices() {
    let mac = [0x08, 0x00, 0x69, 0x21, 0x43, 0x65];
    for attached in [[false; 2], [true; 2]] {
        let board = crate::ip34_machine::FuelBoard::with_storage(
            &[], Arc::new(|| 0), 0, None, mac, attached,
        ).unwrap();
        assert_eq!(board.l1.station_mac(), mac);
        assert_eq!(board.eth.mac(), mac);
        assert_eq!(board.ioc3.ps2().type_char('a'), attached[0]);
    }
}

fn fuel_dma_board() -> crate::ip34_machine::FuelBoard {
    let board = crate::ip34_machine::FuelBoard::new(&[], Arc::new(|| 0)).unwrap();
    assert_eq!(board.md.md_bus().write64(8, 0x33), BUS_OK);
    board.xbridge.head(15).write32(0x24, 15);
    board.xbridge.set_wide_slot_mask(15, 0);
    board
}

fn fuel_dma_address(address: u64) -> u64 {
    (10 << 60) | (1 << 55) | address
}

#[test]
fn dma_preserves_byte_order_and_crosses_ragged_pages() {
    use crate::pci::PciDmaStatus;
    let board = fuel_dma_board();
    let dma = board.xbridge.dma(15, 0);
    let memory = board.md.memory_view();
    for (address, length) in [(0x1030, 8), (0x1103, 27), (0x1ffb, 19)] {
        let data: Vec<u8> = (0..length).map(|i| 0x31 + i as u8).collect();
        assert_eq!(dma.dma_write(fuel_dma_address(address), &data), PciDmaStatus::Ok);
        for (i, &byte) in data.iter().enumerate() {
            assert_eq!(memory.read8(address + i as u64).data, byte);
        }
        let mut back = vec![0xff; length];
        assert_eq!(dma.dma_read(fuel_dma_address(address), &mut back), PciDmaStatus::Ok);
        assert_eq!(back, data);
    }
    // Without byte swapping enabled, the bridge reverses each 32-bit word.
    assert_eq!(dma.dma_write((10 << 60) | 0x3000, &[0, 1, 2, 3, 4, 5, 6, 7]), PciDmaStatus::Ok);
    assert_eq!(memory.read64(0x3000).data, 0x0302010007060504);
}

#[test]
fn fuel_dma_refusal_zeros_failed_legs_and_keeps_successful_legs() {
    use crate::pci::PciDmaStatus;
    let board = fuel_dma_board();
    let dma = board.xbridge.dma(15, 0);
    let memory = board.md.memory_view();
    // Bank zero has 256 MiB fitted. The second qword is outside its backing.
    let last = 0x0fff_fff8;
    assert_eq!(memory.write64(last, 0x0102030405060708), BUS_OK);
    let mut bytes = [0xff; 16];
    assert_eq!(dma.dma_read(fuel_dma_address(last), &mut bytes), PciDmaStatus::MasterAbort);
    assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(dma.dma_write(fuel_dma_address(last), &[0x42; 16]), PciDmaStatus::MasterAbort);
    assert_eq!(memory.read64(last).data, 0x4242424242424242);
    board.io.disable_device(15, 0);
    bytes.fill(0xff);
    assert_eq!(dma.dma_read(fuel_dma_address(last), &mut bytes), PciDmaStatus::MasterAbort);
    assert_eq!(bytes, [0; 16]);
    assert_eq!(dma.dma_write(fuel_dma_address(last), &[0x99; 8]), PciDmaStatus::MasterAbort);
    assert_eq!(memory.read64(last).data, 0x4242424242424242);
}

#[test]
fn fuel_dma_checks_an_aligned_uncorrectable_word_once() {
    use crate::dev::bedrock_md::MemoryError;
    use crate::pci::PciDmaStatus;
    use std::sync::atomic::{AtomicU64, Ordering};
    let board = fuel_dma_board();
    assert_eq!(board.md.memory_view().write64(0, 0), BUS_OK);
    assert_eq!(board.md.ecc_bus().write8(7, 3), BUS_OK);
    let count = Arc::new(AtomicU64::new(0));
    let seen = count.clone();
    let md = Arc::downgrade(&board.md);
    board.md.on_error(Arc::new(move |kind| {
        assert_eq!(kind, MemoryError::Uncorrectable);
        assert_eq!(md.upgrade().unwrap().current_cell(), u64::MAX);
        seen.fetch_add(1, Ordering::Relaxed);
    }));
    // The former callback read the same persistent UCE once for each byte.
    for address in 0..8 {
        assert_eq!(board.io.inbound_read(15, Some(0), address, 1).status, BUS_OK);
    }
    assert_eq!(count.swap(0, Ordering::Relaxed), 8);
    assert_ne!(board.md.md_bus().read64(0x60018).data & (1 << 61), 0);
    let mut bytes = [0xff; 8];
    assert_eq!(board.xbridge.dma(15, 0).dma_read(fuel_dma_address(0), &mut bytes), PciDmaStatus::Ok);
    assert_eq!(bytes, [0; 8]);
    assert_eq!(count.load(Ordering::Relaxed), 1);
    let record = board.md.md_bus().read64(0x60010).data;
    assert_ne!(record & (1 << 60), 0);
    assert_eq!(record & (1 << 61), 0);
    assert_eq!(board.md.uncached_view().read64(0).status, BUS_ERR);
}

#[test]
fn dma_hub_access_preserves_width_and_source_permissions() {
    use crate::pci::PciDmaStatus;
    let board = fuel_dma_board();
    let dma = board.xbridge.dma(15, 0);
    let hub = (1 << 47) | 0x1800000;
    for (interface, offset) in [(0, 0), (1, 0x200000)] {
        let address = fuel_dma_address(hub + offset + 0x90);
        assert_eq!(dma.dma_write(address, &0x10cu64.to_be_bytes()), PciDmaStatus::Ok);
        assert_ne!(board.pi[interface].read64(0x98).data & (1 << 12), 0);
        for width in [1, 2, 4] {
            assert_eq!(dma.dma_write(address, &[0; 8][..width]), PciDmaStatus::MasterAbort);
            assert_ne!(board.pi[interface].read64(0x98).data & (1 << 12), 0);
        }
        assert_eq!(dma.dma_write(address, &12u64.to_be_bytes()), PciDmaStatus::Ok);
        assert_eq!(board.pi[interface].read64(0x98).data & (1 << 12), 0);
    }
    let address = fuel_dma_address(hub + 0x90);
    // I/O modify must retain PI region protection and requesting-slot identity.
    assert_eq!(board.pi[0].write64(0x10, 0), BUS_OK);
    assert_eq!(dma.dma_write(address, &0x10cu64.to_be_bytes()), PciDmaStatus::MasterAbort);
    assert_eq!(board.pi[0].read64(0x98).data & (1 << 12), 0);
    assert_eq!(board.pi[0].write64(0x10, 1), BUS_OK);
    board.io.disable_device(15, 0);
    assert_eq!(dma.dma_write(address, &0x10cu64.to_be_bytes()), PciDmaStatus::MasterAbort);
    assert_eq!(board.pi[0].read64(0x98).data & (1 << 12), 0);
    assert_eq!(board.xbridge.dma(15, 1).dma_write(address, &0x10cu64.to_be_bytes()), PciDmaStatus::Ok);
    assert_ne!(board.pi[0].read64(0x98).data & (1 << 12), 0);
    let mut bytes = [0xff; 8];
    assert_eq!(board.xbridge.dma(15, 1).dma_read(
        fuel_dma_address(hub + 0x40), &mut bytes,
    ), PciDmaStatus::MasterAbort);
    assert_eq!(bytes, [0; 8]);
    assert_eq!(board.bus.read64((1 << 57) | 0x1800040).status, BUS_OK);
}
