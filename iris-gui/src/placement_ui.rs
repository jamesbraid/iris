//! The GUI's view of the disks and network adapters on a machine's SCSI
//! buses: one [`Media`] per placed device, named by [`Location`]. Everything
//! outside this module reads and writes disks through here, so the config
//! form behind it can change without the panels noticing.

use iris::config::{MachineConfig, ScsiKind};
use iris::location::{Hop, Location};
use iris::topology::Topology;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Disk,
    Cdrom,
    Daynaport,
}

/// One device on a SCSI target: a hard disk, a CD-ROM drive (its tray may be
/// empty) or a DaynaPort.
#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    pub at: Location,
    pub kind: MediaKind,
    pub path: String,
    pub discs: Vec<String>,
    pub overlay: bool,
    pub scratch: bool,
    pub size_mb: Option<u32>,
    pub write_cache: bool,
    pub mac: Option<String>,
    pub subnet: Option<String>,
}

impl Media {
    pub fn new(at: Location, kind: MediaKind) -> Media {
        Media {
            at,
            kind,
            path: String::new(),
            discs: vec![],
            overlay: false,
            scratch: false,
            size_mb: None,
            write_cache: true,
            mac: None,
            subnet: None,
        }
    }

    pub fn is_cdrom(&self) -> bool {
        self.kind == MediaKind::Cdrom
    }

    pub fn is_daynaport(&self) -> bool {
        self.kind == MediaKind::Daynaport
    }

    /// The target id, which is how the panels and the worker name a drive.
    pub fn id(&self) -> u8 {
        self.at.target().unwrap_or(0)
    }
}

/// Where target `id` of the first controller is on this machine.
pub fn controller_zero_target(cfg: &MachineConfig, id: u8) -> Location {
    let topo = Topology::fill_for(cfg);
    topo.controller(0).expect("every machine has a first SCSI controller").join(Hop::t(id))
}

/// Every disk, CD-ROM and DaynaPort of `cfg`, by location.
pub fn media(cfg: &MachineConfig) -> Vec<Media> {
    let topo = Topology::fill_for(cfg);
    let mut out: Vec<Media> = cfg
        .scsi
        .iter()
        .filter_map(|(&key, dev)| {
            let (controller, id) = dev.placement(key);
            let at = topo.controller(controller)?.join(Hop::t(id));
            let kind = match dev.kind() {
                ScsiKind::Disk => MediaKind::Disk,
                ScsiKind::Cdrom => MediaKind::Cdrom,
                ScsiKind::Daynaport => MediaKind::Daynaport,
            };
            Some(Media {
                at,
                kind,
                path: dev.path.clone(),
                discs: dev.discs.clone(),
                overlay: dev.overlay,
                scratch: dev.scratch,
                size_mb: dev.size_mb,
                write_cache: dev.write_cache,
                mac: dev.mac.clone(),
                subnet: dev.subnet.clone(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.at.cmp(&b.at));
    out
}

/// The device at `at`, if any.
pub fn media_at(cfg: &MachineConfig, at: &Location) -> Option<Media> {
    media(cfg).into_iter().find(|m| &m.at == at)
}

/// The key and controller of `at` in the `[scsi.N]` map.
fn slot(cfg: &MachineConfig, at: &Location) -> Option<(u8, u8)> {
    let topo = Topology::fill_for(cfg);
    let parent = at.parent()?;
    let controller = (0..=1).find(|&c| topo.controller(c).as_ref() == Some(&parent))?;
    Some((controller, at.target()?))
}

/// Put `m` on its target, replacing what was there.
pub fn set_media(cfg: &mut MachineConfig, m: Media) {
    let Some((controller, id)) = slot(cfg, &m.at) else { return };
    let key = iris::config::scsi_slot(controller, id);
    cfg.scsi.insert(
        key,
        iris::config::ScsiDeviceConfig {
            path: m.path,
            discs: m.discs,
            cdrom: false,
            kind_field: match m.kind {
                MediaKind::Disk => ScsiKind::Disk,
                MediaKind::Cdrom => ScsiKind::Cdrom,
                MediaKind::Daynaport => ScsiKind::Daynaport,
            },
            mac: m.mac,
            subnet: m.subnet,
            write_cache: m.write_cache,
            overlay: m.overlay,
            scratch: m.scratch,
            size_mb: m.size_mb,
            controller: 0,
        },
    );
}

/// Take the device at `at` off its target.
pub fn remove_media(cfg: &mut MachineConfig, at: &Location) {
    if let Some((controller, id)) = slot(cfg, at) {
        cfg.scsi.remove(&iris::config::scsi_slot(controller, id));
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
        let cfg = MachineConfig::default();
        let m = media(&cfg);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].at.to_string(), m[0].kind), ("hpc3.0.t1".to_string(), MediaKind::Disk));
        assert_eq!((m[1].at.to_string(), m[1].kind), ("hpc3.0.t4".to_string(), MediaKind::Cdrom));
        assert_eq!(m[0].id(), 1);
        assert_eq!(controller_zero_target(&cfg, 3), at("hpc3.0.t3"));
    }

    #[test]
    fn set_and_remove_round_trip() {
        let mut cfg = MachineConfig::default();
        let mut disk = Media::new(at("hpc3.0.t2"), MediaKind::Disk);
        disk.path = "two.img".into();
        disk.overlay = true;
        set_media(&mut cfg, disk.clone());
        assert_eq!(media_at(&cfg, &at("hpc3.0.t2")), Some(disk));
        remove_media(&mut cfg, &at("hpc3.0.t2"));
        assert_eq!(media_at(&cfg, &at("hpc3.0.t2")), None);
        assert_eq!(media(&cfg).len(), 2, "the defaults stay");
    }

    #[test]
    fn a_cdrom_with_an_empty_tray_is_listed() {
        let mut cfg = MachineConfig::default();
        set_media(&mut cfg, Media::new(at("hpc3.0.t5"), MediaKind::Cdrom));
        let m = media_at(&cfg, &at("hpc3.0.t5")).expect("listed");
        assert!(m.is_cdrom() && m.path.is_empty() && m.discs.is_empty());
    }

    /// On the Indigo2 the second controller's targets are their own.
    #[test]
    fn the_second_controller_is_its_own_row() {
        let mut cfg = MachineConfig::default();
        cfg.machine.profile = iris::config::MachineProfile::Indigo2Ip22;
        set_media(&mut cfg, Media::new(at("hpc3.1.t3"), MediaKind::Disk));
        assert!(media_at(&cfg, &at("hpc3.1.t3")).is_some() && media_at(&cfg, &at("hpc3.0.t3")).is_none());
    }
}
