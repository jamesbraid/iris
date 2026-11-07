//! The hard disk and CD-ROM device types: `[[hdd]]` and `[[cdrom]]` entries
//! at a target row. Both are passive: they attach to the controller's bus and
//! return no handles, and the controller saves their state.

use std::net::Ipv4Addr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::port::ScsiPort;
use crate::attach::{Ctx, DeviceType, Handles};
use crate::config::{parse_mac, parse_nat_subnet, DaynaportParams, MachineConfig, NatSubnet, NetMode};
use crate::disks::{attach_failed, MediaSpec};
use crate::location::Location;
use crate::register_device;
use crate::topology::Family;

fn yes() -> bool {
    true
}

fn is_yes(b: &bool) -> bool {
    *b
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HddOptions {
    pub path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overlay: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scratch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_mb: Option<u32>,
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub write_cache: bool,
}

impl Default for HddOptions {
    fn default() -> Self {
        HddOptions { path: String::new(), overlay: false, scratch: false, size_mb: None, write_cache: true }
    }
}

pub struct Hdd;

impl DeviceType for Hdd {
    const NAME: &'static str = "hdd";
    const FAMILY: Family = Family::Scsi;
    type Owner = ScsiPort;
    type Options = HddOptions;

    fn new(o: HddOptions, ctx: &mut Ctx, owner: &Arc<ScsiPort>, at: &Location) -> Result<Handles, String> {
        let spec = MediaSpec {
            path: o.path,
            overlay: o.overlay,
            scratch: o.scratch,
            size_mb: o.size_mb,
            write_cache: o.write_cache,
            ..MediaSpec::default()
        };
        ctx.disks.attach_media(owner, at, &spec)?;
        Ok(vec![])
    }
}
register_device!(Hdd);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CdromOptions {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discs: Vec<String>,
}

pub struct Cdrom;

impl DeviceType for Cdrom {
    const NAME: &'static str = "cdrom";
    const FAMILY: Family = Family::Scsi;
    type Owner = ScsiPort;
    type Options = CdromOptions;

    fn new(o: CdromOptions, ctx: &mut Ctx, owner: &Arc<ScsiPort>, at: &Location) -> Result<Handles, String> {
        let spec = MediaSpec { path: o.path, discs: o.discs, cdrom: true, ..MediaSpec::default() };
        ctx.disks.attach_media(owner, at, &spec)?;
        Ok(vec![])
    }
}
register_device!(Cdrom);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaynaportOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<String>,
}

impl DaynaportOptions {
    /// The settings with defaults filled in: a MAC ending in the target id,
    /// so two targets never collide, and the 192.168.10.0/24 gateway subnet.
    pub fn params(&self, id: u8) -> Result<DaynaportParams, String> {
        let mac = match &self.mac {
            Some(s) => parse_mac(s)?,
            None => [0x00, 0x80, 0x19, 0x44, 0x50, id],
        };
        let subnet = match &self.subnet {
            Some(cidr) => {
                let (gateway_ip, client_ip, netmask) = parse_nat_subnet(cidr)?;
                NatSubnet { gateway_ip, client_ip, netmask }
            }
            None => NatSubnet {
                gateway_ip: Ipv4Addr::new(192, 168, 10, 1),
                client_ip: Ipv4Addr::new(192, 168, 10, 2),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
            },
        };
        Ok(DaynaportParams { mac, subnet })
    }

    /// Values that need the rest of the config: a good MAC and subnet, a
    /// gateway that is not the machine's own NAT subnet, and no stream mode.
    pub fn check(&self, cfg: &MachineConfig) -> Result<(), String> {
        if cfg.network.mode == NetMode::Stream {
            return Err("a daynaport cannot join a stream segment: [network] stream names the onboard Ethernet's peer".into());
        }
        if let Some(mac) = &self.mac {
            parse_mac(mac).map_err(|e| format!("mac: {e}"))?;
        }
        if let Some(cidr) = &self.subnet {
            parse_nat_subnet(cidr).map_err(|e| format!("subnet \"{cidr}\": {e}"))?;
        }
        // Each DaynaPort runs its own NAT gateway. Sharing a subnet with ec0
        // gives the guest two interfaces on one network and nothing routes
        // predictably.
        let dp = self.params(0)?;
        let main = cfg
            .nat_subnet
            .as_deref()
            .map(|c| parse_nat_subnet(c).map(|(g, c2, n)| NatSubnet { gateway_ip: g, client_ip: c2, netmask: n }))
            .transpose()?
            .unwrap_or_default();
        if dp.subnet.gateway_ip == main.gateway_ip {
            return Err(format!(
                "DaynaPort subnet {} collides with the machine's NAT subnet (ec0). Give the DaynaPort its own, \
                 e.g. subnet = \"192.168.10.0/24\".",
                dp.subnet.gateway_ip
            ));
        }
        Ok(())
    }
}

/// A DaynaPort SCSI/Link: an Ethernet adapter on a SCSI target. The
/// controller builds it, since it needs the machine's network.
pub struct Daynaport;

impl DeviceType for Daynaport {
    const NAME: &'static str = "daynaport";
    const FAMILY: Family = Family::Scsi;
    const NEEDS_NETWORK: bool = true;
    type Owner = ScsiPort;
    type Options = DaynaportOptions;

    fn check(o: &DaynaportOptions, cfg: &MachineConfig) -> Result<(), String> {
        o.check(cfg)
    }

    fn new(o: DaynaportOptions, _: &mut Ctx, owner: &Arc<ScsiPort>, at: &Location) -> Result<Handles, String> {
        let id = at.target().ok_or_else(|| format!("{at} is not a SCSI target"))?;
        let params = o.params(id)?;
        match owner.add_daynaport(id, params) {
            Ok(()) => println!(
                "iris: DaynaPort SCSI/Link at {at} \u{2014} MAC {}, gateway {} (guest {}/{})",
                crate::net::mac_str(&params.mac),
                params.subnet.gateway_ip,
                params.subnet.client_ip,
                params.subnet.netmask
            ),
            Err(e) => attach_failed(format!("could not attach DaynaPort to {at}: {e}"), &at.to_string())?,
        }
        Ok(vec![])
    }
}
register_device!(Daynaport);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::{attach_device, Ctx};
    use crate::config::{MachineConfig, Placement};
    use crate::devices::Devices;
    use crate::disks::Disks;
    use crate::scsi::{ScsiBus, ScsiIdentities, ScsiIdentity};
    use crate::scsi_monitor::ScsiTargets;
    use crate::topology::{Row, Topology};

    const ID: ScsiIdentity = ScsiIdentity::new(b"TEST    ", b"MEDIA           ", b"1.0 ");

    struct Ctl(ScsiBus);
    impl ScsiTargets for Ctl {
        fn scsi_bus(&self) -> &ScsiBus {
            &self.0
        }
    }

    fn machine() -> Topology {
        let t = Topology::new();
        let at = |s: &str| s.parse::<Location>().unwrap();
        t.add(at("hpc3"), Row::fixed("")).unwrap();
        t.add(at("hpc3.0"), Row::fixed("").ctl(0)).unwrap();
        t.scsi(&at("hpc3.0"), 1..=7, true);
        let bus = ScsiBus::new(8, ScsiIdentities { disk: ID, cdrom: ID });
        t.bind(&at("hpc3.0"), Arc::new(ScsiPort::new(Arc::new(Ctl(bus))))).unwrap();
        t
    }

    fn attach(t: &Topology, device: &str, at: &str, opts: &str, probe: bool) -> Result<(), String> {
        let p = Placement { device: device.into(), at: at.parse().unwrap(), opts: toml::from_str(opts).unwrap() };
        let (mut d, k) = (Devices::new(), Disks::new(vec![]));
        let mut ctx = Ctx { devices: &mut d, disks: &k };
        attach_device(&p, &MachineConfig::default(), (!probe).then_some(&mut ctx), t)
    }

    #[test]
    fn hdd_without_path_is_an_error_naming_path() {
        let e = attach(&machine(), "hdd", "hpc3.0.t1", "", true).unwrap_err();
        assert!(e.contains("missing field `path` for hdd"), "{e}");
    }

    #[test]
    fn cdrom_with_empty_tray_is_valid() {
        let t = machine();
        attach(&t, "cdrom", "hpc3.0.t4", "", false).unwrap();
        let port = t.handle::<ScsiPort>(&"hpc3.0".parse().unwrap()).unwrap();
        assert!(port.bus().has_target(4));
    }

    #[test]
    fn hdd_options_default_to_a_write_cache_and_refuse_unknown_keys() {
        assert!(HddOptions::default().write_cache);
        let o: HddOptions = toml::from_str("path = \"a\"").unwrap();
        assert!(o.write_cache && !o.overlay);
        let e = toml::from_str::<HddOptions>("path = \"a\"\ndiscs = []").unwrap_err().to_string();
        assert!(e.contains("unknown field `discs`") && e.contains("`write_cache`"), "{e}");
    }

    #[test]
    fn register_makers_lists_hdd_and_cdrom() {
        for name in ["hdd", "cdrom"] {
            assert!(crate::attach::maker_for(name, Family::Scsi).is_some(), "{name}");
        }
    }

    fn dayna_machine(log: Arc<parking_lot::Mutex<Vec<(u8, DaynaportParams)>>>) -> Topology {
        let t = Topology::new();
        let at = |s: &str| s.parse::<Location>().unwrap();
        t.add(at("hpc3"), Row::fixed("")).unwrap();
        t.add(at("hpc3.0"), Row::fixed("").ctl(0)).unwrap();
        t.scsi(&at("hpc3.0"), 1..=7, true);
        let bus = ScsiBus::new(8, ScsiIdentities { disk: ID, cdrom: ID });
        let port = ScsiPort::new(Arc::new(Ctl(bus))).with_daynaport(move |id, p| {
            log.lock().push((id, p));
            Ok(())
        });
        t.bind(&at("hpc3.0"), Arc::new(port)).unwrap();
        t
    }

    fn attach_cfg(t: &Topology, cfg: &MachineConfig, opts: &str, probe: bool) -> Result<(), String> {
        let p = Placement { device: "daynaport".into(), at: "hpc3.0.t3".parse().unwrap(), opts: toml::from_str(opts).unwrap() };
        let (mut d, k) = (Devices::new(), Disks::new(vec![]));
        attach_device(&p, cfg, (!probe).then_some(&mut Ctx { devices: &mut d, disks: &k }), t)
    }

    #[test]
    fn daynaport_check_rejects_a_subnet_that_collides_with_the_nat_subnet() {
        let mut cfg = MachineConfig::default();
        cfg.nat_subnet = Some("192.168.10.0/24".into());
        let e = attach_cfg(&machine(), &cfg, "", true).unwrap_err();
        assert!(e.starts_with("error: hpc3.0.t3: DaynaPort subnet") && e.contains("collides"), "{e}");
        cfg.nat_subnet = Some("192.168.20.0/24".into());
        attach_cfg(&machine(), &cfg, "", true).unwrap();
    }

    #[test]
    fn daynaport_check_rejects_stream_mode() {
        let mut cfg = MachineConfig::default();
        cfg.network.mode = NetMode::Stream;
        let e = attach_cfg(&machine(), &cfg, "", true).unwrap_err();
        assert!(e.contains("cannot join a stream segment"), "{e}");
    }

    #[test]
    fn bad_mac_and_subnet_are_named() {
        let cfg = MachineConfig::default();
        let e = attach_cfg(&machine(), &cfg, "mac = \"zz\"", true).unwrap_err();
        assert!(e.contains("mac:") && e.contains("zz"), "{e}");
        let e = attach_cfg(&machine(), &cfg, "subnet = \"nope\"", true).unwrap_err();
        assert!(e.contains("subnet \"nope\""), "{e}");
    }

    #[test]
    fn default_mac_ends_in_the_target_id() {
        let p = DaynaportOptions::default().params(5).unwrap();
        assert_eq!(p.mac, [0x00, 0x80, 0x19, 0x44, 0x50, 5]);
        assert_eq!(p.subnet.gateway_ip, std::net::Ipv4Addr::new(192, 168, 10, 1));
    }

    #[test]
    fn new_reaches_the_port_closure_with_the_params() {
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let t = dayna_machine(log.clone());
        attach_cfg(&t, &MachineConfig::default(), "mac = \"00:80:19:12:34:56\"", false).unwrap();
        let got = log.lock().clone();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].0, got[0].1.mac), (3, [0, 0x80, 0x19, 0x12, 0x34, 0x56]));
    }
}
