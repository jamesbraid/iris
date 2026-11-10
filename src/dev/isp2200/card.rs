//! The adapter as `[fibre_channel]` describes it: opened with its loop disks,
//! then fitted to a slot of an option board.
//!
//! The same shape as the NVMe card's ([`crate::dev::nvme::Card`]). A disk is
//! attached through [`crate::scsi::attach_table`], the call every `[scsi.N]`
//! disk goes through, so the loop disks take whatever a disk table does
//! (overlay, write cache, and what a disk's identity gains). The identity of
//! the adapter's disks as the guest reads them, vendor, product, revision and
//! serial, is the bus's: [`Isp2200::new_bus`] takes the machine-wide disk
//! identity until a table sets its own.

use std::sync::Arc;

use super::Isp2200;
use crate::config::{FibreChannelConfig, ScsiDeviceConfig};
use crate::dev::nvme::{BRIDGE_DEVICE, BRIDGE_VENDOR};
use crate::pci::{DevFn, PciBus, PciToPciBridge};
use crate::snapshot::DiskRef;

/// The adapter's number among the machine's SCSI adapters: the Octane's two
/// ISP1040s are 0 and 1, and the monitor's `scsi2` and `cow2` are this one.
pub const ADAPTER_INDEX: u8 = 2;

/// The snapshot manifest's id for the disk at loop ID 0; loop ID n is this
/// plus n. Clear of the SCSI slots (0 to 31) and the NVMe disk (0xf0).
pub const SNAPSHOT_BASE: u8 = 0x40;

/// The key a `--ci` run's overlay for the disk at `loop_id` is named by:
/// above every `[scsi.N]` key.
const CI_KEY_BASE: u8 = 0x80;

/// The card a `[fibre_channel]` section describes, before it is in a slot.
pub struct Card {
    pub fc: Arc<Isp2200>,
    bridge: bool,
}

impl Card {
    /// The adapter with its loop's disks attached. `ci` is a `--ci` run,
    /// which keeps each overlay in the temporary directory.
    pub fn open(cfg: &FibreChannelConfig, ci: bool) -> Result<Card, String> {
        let fc = Arc::new(Isp2200::new_at(ADAPTER_INDEX, Isp2200::new_bus()));
        for (&id, disk) in &cfg.disks {
            let at = format!("Fibre Channel loop ID {id}");
            let ci_overlay = (ci && disk.disk.overlay).then(|| ScsiDeviceConfig::ci_overlay_path(CI_KEY_BASE + id));
            crate::scsi::attach_table(fc.bus(), id, &at, &disk.disk, ci_overlay.as_deref(), Some("iris"))?;
            if let Some((node, port)) = disk.names(id)? {
                fc.set_names(id, node, port);
            }
        }
        Ok(Card { fc, bridge: cfg.bridge })
    }

    /// Put the card in `device` of `bus`: in the slot itself, or behind a
    /// PCI-to-PCI bridge there, as device 0 of the bridge's bus.
    pub fn fit(&self, bus: &PciBus, device: u8) {
        let at = |device| DevFn { device, function: 0 };
        if self.bridge {
            let ppb = PciToPciBridge::new(bus, at(device), BRIDGE_VENDOR, BRIDGE_DEVICE);
            bus.attach(at(device), ppb.clone());
            ppb.secondary().attach(at(0), self.fc.clone());
        } else {
            bus.attach(at(device), self.fc.clone());
        }
    }
}

/// The snapshot manifest's record of each loop disk, so a restore finds the
/// image it was saved with.
pub fn disk_refs(cfg: &FibreChannelConfig) -> Vec<DiskRef> {
    cfg.disks
        .iter()
        .map(|(&id, d)| DiskRef {
            id: SNAPSHOT_BASE + id,
            path: d.disk.path.clone(),
            size_bytes: std::fs::metadata(&d.disk.path).map(|m| m.len()).unwrap_or(0),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::{scsi_cmd, ScsiDataLength, ScsiRequest};

    fn inquiry(fc: &Isp2200, id: u8, evpd_page: Option<u8>) -> Vec<u8> {
        let req = ScsiRequest {
            cdb: vec![scsi_cmd::INQUIRY, evpd_page.is_some() as u8, evpd_page.unwrap_or(0), 0, 255, 0],
            data_len: ScsiDataLength::Unlimited,
            data_in: None,
        };
        fc.bus().request(id, &req).unwrap().unwrap().data
    }

    #[test]
    fn a_loop_disk_reports_the_identity_its_table_gives_it() {
        let path = std::env::temp_dir().join(format!("iris-fc-identity-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let p = path.to_string_lossy().replace('\\', "/");
        let cfg: FibreChannelConfig = toml::from_str(&format!(
            "[disk.5]\npath = \"{p}\"\nvendor = \"SEAGATE\"\nproduct = \"ST39102FC\"\nrevision = \"0005\"\nserial = \"3JA0K1X4\"\n\
             [disk.6]\npath = \"{p}\"\n"
        ))
        .unwrap();
        let card = Card::open(&cfg, false).unwrap();
        let named = inquiry(&card.fc, 5, None);
        assert_eq!(&named[8..16], b"SEAGATE ");
        assert_eq!(&named[16..32], b"ST39102FC       ");
        assert_eq!(&named[32..36], b"0005");
        assert_eq!(&named[36..44], b"3JA0K1X4");
        assert_eq!(inquiry(&card.fc, 5, Some(0x80)), [&[0u8, 0x80, 0, 8][..], b"3JA0K1X4"].concat());
        // A disk whose table sets nothing reports the bus's disk identity.
        let plain = inquiry(&card.fc, 6, None);
        let bus_default = card.fc.bus().identity(false);
        assert_eq!(&plain[8..16], &bus_default.vendor);
        assert_eq!(&plain[16..32], &bus_default.product);
        assert_eq!(plain.len(), 36);
        std::fs::remove_file(path).unwrap();
    }
}
