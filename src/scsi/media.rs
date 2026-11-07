//! The hard disk and CD-ROM device types: `[[hdd]]` and `[[cdrom]]` entries
//! at a target row. Both are passive: they attach to the controller's bus and
//! return no handles, and the controller saves their state.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::port::ScsiPort;
use crate::attach::{Ctx, DeviceType, Handles};
use crate::disks::MediaSpec;
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

    fn new(o: HddOptions, ctx: &Ctx, owner: &Arc<ScsiPort>, at: &Location) -> Result<Handles, String> {
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

    fn new(o: CdromOptions, ctx: &Ctx, owner: &Arc<ScsiPort>, at: &Location) -> Result<Handles, String> {
        let spec = MediaSpec { path: o.path, discs: o.discs, cdrom: true, ..MediaSpec::default() };
        ctx.disks.attach_media(owner, at, &spec)?;
        Ok(vec![])
    }
}
register_device!(Cdrom);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::{attach_device, Ctx};
    use crate::config::{MachineConfig, Placement};
    use crate::devices::Devices;
    use crate::disks::Disks;
    use crate::scsi::{ScsiBus, ScsiIdentity};
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
        let bus = ScsiBus::new(8, crate::scsi::ScsiIdentities { disk: ID, cdrom: ID });
        t.bind(&at("hpc3.0"), Arc::new(ScsiPort::new(Arc::new(Ctl(bus))))).unwrap();
        t
    }

    fn attach(t: &Topology, device: &str, at: &str, opts: &str, probe: bool) -> Result<(), String> {
        let p = Placement { device: device.into(), at: at.parse().unwrap(), opts: toml::from_str(opts).unwrap() };
        let (d, k) = (Devices::new(), Disks::new());
        let ctx = Ctx { devices: &d, disks: &k };
        attach_device(&p, &MachineConfig::default(), (!probe).then_some(&ctx), t)
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
}
