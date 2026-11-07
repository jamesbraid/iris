use super::*;
use crate::config::{MachineProfile, NetMode};
use crate::dev::bedrock_ni::{reg as ni, LINK_UP, UNCABLED};

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("iris-o3200-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(dir: &Path, profile: MachineProfile) -> MachineConfig {
    let mut cfg = MachineConfig::default();
    cfg.machine.profile = profile;
    cfg.banks = [256, 256, 0, 0, 0, 0, 0, 0];
    let prom = dir.join("prom.bin");
    std::fs::write(&prom, vec![0u8; 0x1000]).unwrap();
    cfg.prom = prom.to_string_lossy().into_owned();
    cfg.nvram = dir.join("nvram.bin").to_string_lossy().into_owned();
    cfg.headless = true;
    cfg.network.mode = NetMode::Off;
    let held: Vec<_> = (0..4).map(|_| std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap()).collect();
    cfg.monitor_port = Some(held[0].local_addr().unwrap().port());
    cfg.serial_port_a = Some(held[1].local_addr().unwrap().port());
    cfg.serial_port_b = Some(held[2].local_addr().unwrap().port());
    cfg.fuel.l1_port = Some(held[3].local_addr().unwrap().port());
    cfg.scsi.clear();
    cfg
}

/// Building a machine puts more than a megabyte on the stack.
fn big_stack(body: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(64 << 20).spawn(body).unwrap().join().expect("thread panicked");
}

/// The brick supplies the board identification word, the processor row and
/// the part as literal values, not as another read of the same constants.
#[test]
fn the_c_brick_supplies_its_identity_and_processor_row() {
    assert_eq!(CBRICK_BOARD_ID, 2);
    assert_eq!(CBRICK_FIRMWARE.cpu_mode_word, 0x5414_a785);
    assert_eq!(CBRICK_FIRMWARE.cpu_clock_hz, 400_000_000);
    assert_eq!(CBRICK_FIRMWARE.cpu_type, 0);
    assert_eq!(CBRICK_FIRMWARE.reset_config0(), 0x6c14_a785);
    let part = CPUS[0];
    assert_eq!((part.mhz, part.l2_kb, part.prid()), (400, 8192, 0x0e35));
}

/// Both Origin 3200 profiles build their composition: the one-C-brick machine
/// has a single node and no cable, the two-C-brick machine two nodes and a
/// direct cable, and only the first node carries the I-brick.
#[test]
fn the_profiles_build_their_compositions() {
    big_stack(|| {
        let dir = Scratch::new("compose");
        let one = Ip35Machine::new(&config(&dir.0, MachineProfile::Origin3200OneCIp35)).unwrap();
        assert_eq!(one.bricks.len(), 1);
        assert_eq!(one.bricks[0].node.ni.read64(ni::PORT_STATUS).data, UNCABLED);
        let two = Ip35Machine::new(&config(&dir.0, MachineProfile::Origin3200TwoCIp35)).unwrap();
        assert_eq!(two.bricks.len(), 2);
        for brick in &two.bricks {
            assert_eq!(brick.node.ni.read64(ni::PORT_STATUS).data, LINK_UP);
        }
        let word = |brick: &CBrick| brick.node.bus.read64(0x1000_0000 + 0x400_0000);
        assert_eq!(word(&two.bricks[0]).data, 2);
    });
}

/// A dropped machine gives back its nodes, processors and decode tables: the
/// process holds only a few dozen tables.
#[test]
fn a_dropped_machine_frees_its_nodes() {
    big_stack(|| {
        let dir = Scratch::new("drop");
        let m = Ip35Machine::new(&config(&dir.0, MachineProfile::Origin3200TwoCIp35)).unwrap();
        let memory: Vec<_> = m.bricks.iter().map(|b| Arc::downgrade(b.node.md.physical())).collect();
        let cpus: Vec<_> = m.bricks.iter().map(|b| Arc::downgrade(&b.cpu)).collect();
        drop(m);
        for (i, w) in memory.iter().enumerate() {
            assert_eq!(w.strong_count(), 0, "memory {i}");
        }
        for (i, w) in cpus.iter().enumerate() {
            assert_eq!(w.strong_count(), 0, "cpu {i}");
        }
    });
}

/// A disk on the loop is accepted, and a Daynaport is refused: the I-brick's
/// Ethernet is built in.
#[test]
fn a_disk_is_accepted_and_a_daynaport_refused() {
    let dir = Scratch::new("disk");
    let mut cfg = config(&dir.0, MachineProfile::Origin3200OneCIp35);
    cfg.scsi.insert(1, toml::from_str("path = \"/dev/null\"\nkind = \"disk\"").unwrap());
    assert_eq!(cfg.validate(), Ok(()));
    cfg.scsi.insert(2, toml::from_str("kind = \"daynaport\"").unwrap());
    assert!(cfg.validate().unwrap_err().contains("daynaport"));
}

/// Processors on several nodes share memory, so a store-conditional must be a
/// host compare-exchange: with the flag clear it always succeeds, and IRIX
/// deadlocks in a lock the second node's processor also takes.
#[test]
fn processors_of_a_multi_node_board_run_smp() {
    use crate::cpu::mips_cache_shadow::R12000Scache8ShadowCache;
    big_stack(|| {
        let dir = Scratch::new("smp");
        for (profile, smp) in [(MachineProfile::Origin3200OneCIp35, false), (MachineProfile::Origin3200TwoCIp35, true)] {
            let cfg = config(&dir.0, profile);
            let m = Ip35Machine::new(&cfg).unwrap();
            let cpu = build_cpu::<R12000Scache8ShadowCache>(&m.bricks[0].node, cfg.cpu(), bricks(&cfg) > 1);
            assert_eq!(cpu.core.smp, smp, "{profile:?}");
        }
    });
}

/// A second C-brick's processor reaches node field zero through its node's
/// processor connection, which sends what is not a reader-relative form to
/// the node that carries 0; the bus the cable offers the peer would keep it
/// on the node.
#[test]
fn a_cabled_processor_takes_its_nodes_processor_connection() {
    use crate::cpu::mips_cache_shadow::R12000Scache8ShadowCache;
    big_stack(|| {
        let dir = Scratch::new("processor");
        let cfg = config(&dir.0, MachineProfile::Origin3200TwoCIp35);
        let m = Ip35Machine::new(&cfg).unwrap();
        for brick in &m.bricks {
            let cpu = build_cpu::<R12000Scache8ShadowCache>(&brick.node, cfg.cpu(), true);
            assert!(std::ptr::addr_eq(Arc::as_ptr(&cpu.sysad), Arc::as_ptr(&brick.node.processor)));
            assert!(!std::ptr::addr_eq(Arc::as_ptr(&cpu.sysad), Arc::as_ptr(&brick.node.bus)));
        }
    });
}

/// The I-brick fits the Fibre Channel adapter at the bus slot where the Fuel
/// has its SCSI controller, and a link reset of the port resets it as well.
#[test]
fn the_i_brick_fits_a_fibre_channel_adapter_in_the_storage_slot() {
    use crate::dev::isp2200::{mbox, reg};
    use crate::pci::PciBusDevice;
    big_stack(|| {
        let dir = Scratch::new("fc");
        let m = Ip35Machine::new(&config(&dir.0, MachineProfile::Origin3200OneCIp35)).unwrap();
        let fc = &m.base.storage;
        assert_eq!(fc.config().read(0), 0x2200_1077);
        fc.config().write(1, 3);
        fc.config().write(5, 0x02000000);
        fc.bar_write(1, reg::mbox(0), u32::from(mbox::NOP), 2);
        fc.bar_write(1, reg::HCCR, 0x5000, 2);
        assert_eq!(fc.bar_read(1, reg::mbox(0), 2), u32::from(mbox::COMPLETE));
        assert_eq!(m.base.xbridge.widget(0).unwrap().write32(0x2f4, 1), BUS_OK);
        assert_eq!(fc.config().command(), 0);
        assert_eq!(fc.bar_read(1, reg::mbox(1), 2), 0x4953);
    });
}
