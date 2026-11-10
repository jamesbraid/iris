//! The GUI's view of the disks and network adapters on a machine's SCSI
//! buses: one [`Media`] per placed device, named by [`Location`]. Everything
//! outside this module reads and writes disks through here, so the config
//! form behind it can change without the panels noticing.

use iris::config::MachineConfig;
use iris::location::{Hop, Location};
use iris::scsi::media::{CdromOptions, DaynaportOptions, HddOptions};
use iris::topology::Topology;

/// What sits on one SCSI target: the options of the library's device type.
#[derive(Clone, Debug, PartialEq)]
pub enum Device {
    Hdd(HddOptions),
    Cdrom(CdromOptions),
    Daynaport(DaynaportOptions),
}

impl Device {
    /// A device of type `name` (`hdd`, `cdrom` or `daynaport`) with defaults
    /// and `path` as its image.
    pub fn new(name: &str, path: String) -> Device {
        match name {
            "cdrom" => Device::Cdrom(CdromOptions { path, ..Default::default() }),
            "daynaport" => Device::Daynaport(DaynaportOptions::default()),
            _ => Device::Hdd(HddOptions { path, ..Default::default() }),
        }
    }

    /// The device type's name, as a `[[<name>]]` array spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Device::Hdd(_) => "hdd",
            Device::Cdrom(_) => "cdrom",
            Device::Daynaport(_) => "daynaport",
        }
    }

    fn from_table(name: &str, opts: toml::Table) -> Option<Device> {
        let v = toml::Value::Table(opts);
        match name {
            "hdd" => v.try_into().ok().map(Device::Hdd),
            "cdrom" => v.try_into().ok().map(Device::Cdrom),
            "daynaport" => v.try_into().ok().map(Device::Daynaport),
            _ => None,
        }
    }

    fn to_table(&self) -> toml::Table {
        match self {
            Device::Hdd(o) => toml::Table::try_from(o),
            Device::Cdrom(o) => toml::Table::try_from(o),
            Device::Daynaport(o) => toml::Table::try_from(o),
        }
        .expect("device options serialize")
    }
}

/// One device on a SCSI target: a hard disk, a CD-ROM drive (its tray may be
/// empty) or a DaynaPort.
#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    pub at: Location,
    pub dev: Device,
}

impl Media {
    pub fn is_cdrom(&self) -> bool {
        matches!(self.dev, Device::Cdrom(_))
    }

    pub fn is_daynaport(&self) -> bool {
        matches!(self.dev, Device::Daynaport(_))
    }

    /// The image, empty for a DaynaPort and for a CD-ROM with its tray empty.
    pub fn path(&self) -> &str {
        match &self.dev {
            Device::Hdd(o) => &o.path,
            Device::Cdrom(o) => &o.path,
            Device::Daynaport(_) => "",
        }
    }

    pub fn path_mut(&mut self) -> Option<&mut String> {
        match &mut self.dev {
            Device::Hdd(o) => Some(&mut o.path),
            Device::Cdrom(o) => Some(&mut o.path),
            Device::Daynaport(_) => None,
        }
    }

    /// The CD-ROM's changer list.
    pub fn discs(&self) -> &[String] {
        match &self.dev {
            Device::Cdrom(o) => &o.discs,
            _ => &[],
        }
    }

    /// A hard disk that writes to a copy-on-write overlay.
    pub fn overlay(&self) -> bool {
        matches!(&self.dev, Device::Hdd(o) if o.overlay)
    }

    /// A hard disk the CI socket reads and writes.
    pub fn scratch(&self) -> bool {
        matches!(&self.dev, Device::Hdd(o) if o.scratch)
    }

    /// The target id, which is how the panels and the worker name a drive.
    pub fn id(&self) -> u8 {
        self.at.target().unwrap_or(0)
    }
}

/// A fresh configuration with the default disk (target 1) and CD-ROM
/// (target 4), as iris starts with no file.
pub fn default_config() -> MachineConfig {
    let mut cfg = MachineConfig::default();
    cfg.with_default_media(&Topology::fill_for(&cfg));
    cfg
}

/// Where target `id` of the first controller is on this machine.
pub fn controller_zero_target(cfg: &MachineConfig, id: u8) -> Location {
    let topo = Topology::fill_for(cfg);
    topo.controller(0).expect("every machine has a first SCSI controller").join(Hop::t(id))
}

/// Every disk, CD-ROM and DaynaPort of `cfg`, by location.
pub fn media(cfg: &MachineConfig) -> Vec<Media> {
    let mut out: Vec<Media> = cfg
        .placements()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| Some(Media { dev: Device::from_table(&p.device, p.opts)?, at: p.at }))
        .collect();
    out.sort_by(|a, b| a.at.cmp(&b.at));
    out
}

/// The device at `at`, if any.
pub fn media_at(cfg: &MachineConfig, at: &Location) -> Option<Media> {
    media(cfg).into_iter().find(|m| &m.at == at)
}

/// Put `m` on its target, replacing what was there, whatever its type.
pub fn set_media(cfg: &mut MachineConfig, m: Media) {
    cfg.unplace(&m.at);
    cfg.place(m.dev.name(), m.at, m.dev.to_table()).expect("a SCSI device type");
}

/// The Ultra64 board's place on this machine, when it has one (the Indy and
/// Indigo2's first GIO slot).
pub fn ultra64_slot(cfg: &MachineConfig) -> Option<Location> {
    let gio0 = Location::root().join(Hop::gio(0));
    Topology::fill_for(cfg).offers(&gio0).ok().map(|_| gio0)
}

/// Whether the Ultra64 board is placed.
pub fn has_ultra64(cfg: &MachineConfig) -> bool {
    cfg.placements().unwrap_or_default().iter().any(|p| p.device == "ultra64")
}

/// Place the Ultra64 board in its slot, or take it out.
pub fn set_ultra64(cfg: &mut MachineConfig, on: bool) {
    let Some(at) = ultra64_slot(cfg) else { return };
    if on {
        cfg.place("ultra64", at, toml::Table::new()).expect("ultra64 is a device type");
    } else if has_ultra64(cfg) {
        cfg.unplace(&at);
    }
}

/// Take the device at `at` off its target.
pub fn remove_media(cfg: &mut MachineConfig, at: &Location) {
    cfg.unplace(at);
}

/// Test helpers shared by the modules whose tests set up a machine with the
/// default disk and CD-ROM.
#[cfg(test)]
pub mod fixtures {
    use super::*;

    /// The default config with the default disk (target 1) and CD-ROM (target 4).
    pub fn with_disks() -> MachineConfig {
        default_config()
    }

    /// Change what is on target `id` of the first controller.
    pub fn edit(cfg: &mut MachineConfig, id: u8, change: impl FnOnce(&mut Media)) {
        let at = controller_zero_target(cfg, id);
        let mut m = media_at(cfg, &at).expect("a device there");
        change(&mut m);
        set_media(cfg, m);
    }

    /// The device on target `id` of the first controller.
    pub fn at_id(cfg: &MachineConfig, id: u8) -> Media {
        media_at(cfg, &controller_zero_target(cfg, id)).expect("a device there")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Location {
        s.parse().unwrap()
    }

    #[test]
    fn media_lists_the_default_disk_and_cdrom_by_location() {
        let cfg = default_config();
        let m = media(&cfg);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].at.to_string(), m[0].dev.name()), ("hpc3.0.t1".to_string(), "hdd"));
        assert_eq!((m[1].at.to_string(), m[1].dev.name()), ("hpc3.0.t4".to_string(), "cdrom"));
        assert_eq!(m[0].id(), 1);
        assert_eq!(controller_zero_target(&cfg, 3), at("hpc3.0.t3"));
    }

    #[test]
    fn set_and_remove_round_trip() {
        let mut cfg = MachineConfig::default();
        let disk = Media {
            at: at("hpc3.0.t2"),
            dev: Device::Hdd(HddOptions { path: "two.img".into(), overlay: true, ..Default::default() }),
        };
        set_media(&mut cfg, disk.clone());
        assert_eq!(media_at(&cfg, &at("hpc3.0.t2")), Some(disk));
        remove_media(&mut cfg, &at("hpc3.0.t2"));
        assert_eq!(media_at(&cfg, &at("hpc3.0.t2")), None);
        assert!(media(&cfg).is_empty());
    }

    #[test]
    fn a_cdrom_with_an_empty_tray_is_listed() {
        let mut cfg = MachineConfig::default();
        set_media(&mut cfg, Media { at: at("hpc3.0.t5"), dev: Device::new("cdrom", String::new()) });
        let m = media_at(&cfg, &at("hpc3.0.t5")).expect("listed");
        assert!(m.is_cdrom() && m.path().is_empty() && m.discs().is_empty());
    }

    /// On the Indigo2 the second controller's targets are their own.
    #[test]
    fn the_second_controller_is_its_own_row() {
        let mut cfg = MachineConfig::default();
        cfg.machine.profile = iris::config::MachineProfile::Indigo2Ip22;
        set_media(&mut cfg, Media { at: at("hpc3.1.t3"), dev: Device::new("hdd", "x.img".into()) });
        assert!(media_at(&cfg, &at("hpc3.1.t3")).is_some() && media_at(&cfg, &at("hpc3.0.t3")).is_none());
    }
}
