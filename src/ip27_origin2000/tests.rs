use super::*;
use crate::config::{MachineProfile, NetMode};
use crate::cpu::parts;
use crate::dev::hub::Space;
use crate::dev::{hub_md, hub_ni};
use crate::traits::{BusDevice, BUS_ERR, BUS_OK};
use std::path::Path;

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("iris-o2k-{name}-{}", std::process::id()));
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

/// A PROM stand-in: a recognisable first word and a configuration block in
/// the shape the flash initialiser patches.
fn prom(dir: &Path) -> String {
    let mut p = vec![0u8; 0x1000];
    p[..4].copy_from_slice(&0x3c08_a600u32.to_be_bytes());
    p[0x68..0x70].copy_from_slice(&0x6970_3237_636f_6e66u64.to_be_bytes());
    let path = dir.join("prom.bin");
    std::fs::write(&path, p).unwrap();
    path.to_string_lossy().into_owned()
}

fn config(dir: &Path, nodes: u8) -> MachineConfig {
    let mut cfg = MachineConfig::default();
    cfg.machine.profile = MachineProfile::Origin2000Ip27;
    cfg.set_cpu(&parts::R10000_180);
    cfg.origin2000.nodes = nodes;
    cfg.banks = [64, 0, 0, 0, 0, 0, 0, 0];
    cfg.prom = prom(dir);
    cfg.nvram = dir.join("nvram.bin").to_string_lossy().into_owned();
    cfg.headless = true;
    cfg.network.mode = NetMode::Off;
    let held: Vec<_> = (0..3).map(|_| std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap()).collect();
    cfg.monitor_port = Some(held[0].local_addr().unwrap().port());
    cfg.serial_port_a = Some(held[1].local_addr().unwrap().port());
    cfg.serial_port_b = Some(held[2].local_addr().unwrap().port());
    let disk = dir.join("disk.raw");
    std::fs::write(&disk, vec![0u8; 4 << 20]).unwrap();
    cfg.scsi.clear();
    cfg.scsi.insert(1, toml::from_str(&format!("path = {:?}\noverlay = true", disk.to_string_lossy())).unwrap());
    cfg
}

/// Building a machine puts more than a megabyte on the stack.
fn big_stack(body: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(64 << 20).spawn(body).unwrap().join().expect("thread panicked");
}

fn built(name: &str, nodes: u8) -> (Scratch, Origin2000Machine) {
    let dir = Scratch::new(name);
    let m = Origin2000Machine::new(&config(&dir.0, nodes)).unwrap();
    (dir, m)
}

#[test]
fn the_profile_is_chosen_by_its_snake_case_name() {
    assert_eq!(MachineProfile::from_name("origin2000_ip27"), Ok(MachineProfile::Origin2000Ip27));
    assert_eq!(MachineProfile::Origin2000Ip27.name(), "origin2000_ip27");
}

#[test]
fn the_deskside_has_one_or_two_node_boards() {
    let dir = Scratch::new("count");
    assert_eq!(config(&dir.0, 2).validate(), Ok(()));
    let err = Origin2000::specs(&config(&dir.0, 3)).err().unwrap();
    assert!(err.contains("one or two node boards"), "{err}");
    assert!(Origin2000::specs(&config(&dir.0, 0)).is_err());
}

#[test]
fn each_node_slot_drives_its_identity_into_the_hub() {
    big_stack(|| {
        let (_dir, m) = built("slots", 2);
        let slot = |n: usize| m.node_at(n).hub.read64(hub_md::reg::SLOTID_USTAT).data & 7;
        assert_eq!(slot(0), 7);
        assert_eq!(slot(1), 6);
    });
}

#[test]
fn the_flash_carries_no_machine_type() {
    big_stack(|| {
        let (_dir, m) = built("flash", 1);
        let word = |at: u64| m.node().hub.cpu_special().read32(0x1000_0000 + at).data;
        assert_eq!(word(0x90), 0);
        assert_eq!(word(0x8c), 15);
    });
}

#[test]
fn the_first_crossbow_carries_the_baseio_and_its_node() {
    big_stack(|| {
        let (_dir, m) = built("xbow", 1);
        let io = m.node().hub.claim(Space::Io);
        let id = |w: u64| io.read32((w << 24) + 4);
        // The Crossbow's identification: revision 2, bit 0 set.
        assert_eq!(id(0).data, 0x2000_0001);
        // The Bridge answers at widget 8, the Hub at its own port, 9.
        assert_eq!(id(8).status, BUS_OK);
        assert_ne!(id(8).data, id(9).data);
        // A port with nothing fitted fails.
        for w in [10, 11, 15] {
            assert_eq!(id(w).status, BUS_ERR, "widget {w}");
        }
        assert_eq!(io.write32((8 << 24) + crate::dev::bridge::reg::INT_ENABLE as u64 + 4, 0), BUS_OK);
    });
}

#[test]
fn the_second_node_is_the_host_of_a_crossbow_with_no_io() {
    big_stack(|| {
        let (_dir, m) = built("second", 2);
        let io = m.node_at(1).hub.claim(Space::Io);
        let id = |w: u64| io.read32((w << 24) + 4);
        assert_eq!(id(0).data, 0x2000_0001);
        assert_eq!(id(10).status, BUS_OK);
        assert_eq!(id(8).status, BUS_ERR, "the BaseIO is on the first node's Crossbow");
        assert_eq!(id(9).status, BUS_ERR);
    });
}

#[test]
fn the_cable_joins_the_nodes_once_they_are_numbered() {
    big_stack(|| {
        let (_dir, m) = built("cable", 2);
        let (a, b) = (&m.node_at(0).hub, &m.node_at(1).hub);
        // The PROM numbers the second node 1; the first keeps 0.
        assert_eq!(b.write64(hub_ni::reg::STATUS, 1 << 8), BUS_OK);
        let at = (1u64 << 32) + 0x1000;
        // A store through the first node's claim lands in the second's memory.
        assert_eq!(a.claim(Space::Memory).write64(at, 0x1122_3344_5566_7788), BUS_OK);
        assert_eq!(b.cpu_memory().read64(0x1000).data, 0x1122_3344_5566_7788);
        assert_eq!(a.cpu_memory().read64(0x1000).data, 0);
        // A cached reference from the first node to node 1 takes the same way.
        assert_eq!(m.node_at(0).memory.physical().read64(at).data, 0x1122_3344_5566_7788);
    });
}

#[test]
fn a_dropped_machine_frees_its_hubs_memory_and_timers() {
    big_stack(|| {
        let (_dir, m) = built("drop", 2);
        let hubs: Vec<_> = (0..2).map(|i| Arc::downgrade(&m.node_at(i).hub)).collect();
        let memory: Vec<_> = (0..2).map(|i| Arc::downgrade(m.node_at(i).memory.physical())).collect();
        let timers: Vec<_> = (0..2).map(|i| Arc::downgrade(&m.node_at(i).timers)).collect();
        let cpus: Vec<_> = (0..2).map(|i| Arc::downgrade(m.cpu_at(i))).collect();
        let bridge = Arc::downgrade(m.bridge());
        drop(m);
        for (i, w) in hubs.iter().enumerate() {
            assert_eq!(w.strong_count(), 0, "hub {i}");
        }
        for (i, w) in memory.iter().enumerate() {
            assert_eq!(w.strong_count(), 0, "memory map {i}");
        }
        for (i, w) in timers.iter().enumerate() {
            assert_eq!(w.strong_count(), 0, "timers {i}");
        }
        for (i, w) in cpus.iter().enumerate() {
            assert_eq!(w.strong_count(), 0, "cpu {i}");
        }
        assert_eq!(bridge.strong_count(), 0, "the Bridge");
    });
}

/// IRIX folds the first ten characters of the module serial the module's
/// identity part holds into the host id, and reads the serial only if it
/// starts with K; the Ethernet address is the port's alone and takes no part.
#[test]
fn the_module_part_carries_the_configured_serial() {
    use crate::dev::o2000_module::identity_pages;
    for (inputs, serial, hostid) in [
        ("", "KEMU200000", 0xd520_0000u32),
        ("emu_id = 42", "KEMU200420", 0xd520_0420),
        ("serial = \"K2100887\"", "KKK2100887", 0xb210_0887),
        ("serial = \"KEMU100887\"", "KEMU100887", 0xd510_0887),
        ("hostid = \"d2100887\"", "KKM2100887", 0xd210_0887),
    ] {
        let cfg: MachineConfig = toml::from_str(&format!("[machine]\nprofile = \"origin2000_ip27\"\n{inputs}\n")).unwrap();
        cfg.validate().expect(inputs);
        assert_eq!(Origin2000::module_serial(&cfg).unwrap(), serial, "{inputs}");
        let pages = identity_pages(serial).expect(inputs);
        assert_eq!((pages[0][0], &pages[0][1..11]), (0x01, serial.as_bytes()), "{inputs}");
        assert_eq!(crate::identity::hostid_from_text(std::str::from_utf8(&pages[0][1..11]).unwrap()), hostid, "{inputs}");
        assert_eq!(cfg.identity().unwrap().hostids(1).unwrap(), [hostid], "{inputs}");
    }
}

#[test]
fn a_serial_the_guest_would_ignore_is_refused() {
    let cfg: MachineConfig = toml::from_str("[machine]\nprofile = \"origin2000_ip27\"\nserial = \"M2100887\"\n").unwrap();
    let e = cfg.validate().unwrap_err();
    assert!(e.contains("serial 'M2100887' must start with K on this machine: the guest ignores a module serial that does not"), "{e}");
}

/// The port keeps any address, and takes the layout's by default.
#[test]
fn the_ethernet_port_has_its_own_address() {
    use crate::dev::ioc3::default_wire;
    use crate::dev::w1::sgi::nic_address;
    for (inputs, mac) in [("emu_id = 42", [0x08, 0x00, 0x69, 0xe2, 0x02, 0xa0]), ("serial = \"K2100887\"", [0x08, 0x00, 0x69, 0xe2, 0x00, 0x00])] {
        let cfg: MachineConfig = toml::from_str(&format!("[machine]\nprofile = \"origin2000_ip27\"\n{inputs}\n")).unwrap();
        let mut wire = default_wire(&Origin2000::ioc3_config(cfg.network().mac));
        assert_eq!(nic_address(&mut wire), Some(mac), "{inputs}");
    }
    let mut cfg: MachineConfig = toml::from_str("[machine]\nprofile = \"origin2000_ip27\"\n").unwrap();
    cfg.network.mac = Some("52:54:00:12:34:56".into());
    cfg.validate().expect("a module machine's port keeps any address");
    assert_eq!(cfg.network().mac, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
}
