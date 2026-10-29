//! The SCSI bus: what sits on it, and the table every controller keeps.
//!
//! A controller (the WD33C93A, the AIC-7880, the ISP1040) is registers and
//! DMA; the targets behind it do not depend on which chip asks. [`ScsiTarget`]
//! is what varies on the bus, and [`ScsiBus`] is the shared mechanism: the
//! id-to-target table with the attach and query methods, which each
//! controller owns one of and reaches its targets through.

use std::io;

use parking_lot::{Mutex, MutexGuard};

use super::{ScsiDevice, ScsiIdentity, ScsiRequest, ScsiResponse};
use crate::dev::daynaport::DaynaPort;

/// A changer's discs: the active one, the queue (index 0 is the active
/// disc) and the physical and logical block sizes.
pub struct Discs {
    pub current: String,
    pub list: Vec<String>,
    pub block_sizes: (u64, u64),
}

/// A device on a SCSI bus, as the controller drives it: a CDB goes in, data
/// and a status come out.
///
/// A disk or CD-ROM is a [`ScsiDevice`], a DaynaPort Ethernet adapter a
/// [`DaynaPort`]. The contract is `request` and the lifecycle (`power_on`,
/// `start`, `stop`). Everything else is optional behaviour that a
/// kind of target has or does not (a copy-on-write layer, a disc changer),
/// defaulting to "not this target"; the controllers and the monitor ask for
/// the behaviour, never for the type.
pub trait ScsiTarget: Send {
    /// Run one command; `req.data_in` is what a write sends the target.
    fn request(&mut self, req: &ScsiRequest) -> io::Result<ScsiResponse>;

    /// Return to the state a power-up leaves, as the controller's own
    /// reset does.
    fn power_on(&mut self) {}

    /// Start any thread the target runs of its own.
    fn start(&mut self) {}

    /// Stop it, with no command part way through.
    fn stop(&mut self) {}

    /// Whether READ(6) and WRITE(6) move packets, not blocks, so a
    /// controller must not read their count as a number of sectors.
    fn moves_packets(&self) -> bool { false }

    /// The target's lines in the monitor's `scsi status`, for the target at
    /// `id`.
    fn status(&self, _id: usize) -> Vec<String> { Vec::new() }

    /// The longer report of `scsi dayna`, for a network adapter.
    fn adapter_status(&self) -> Vec<String> { Vec::new() }

    /// The logical block size, for a target that has one.
    fn block_size(&self) -> Option<u64> { None }

    /// A controller's bus reset: block size back to what the controller
    /// leaves, with a CD-ROM at 512 bytes if `cdrom_512`.
    fn reset_block_size(&mut self, _cdrom_512: bool) {}

    // ---- copy-on-write layer -----------------------------------------

    /// Uncommitted sectors, `None` when the target has no such layer.
    fn cow_dirty(&self) -> Option<usize> { None }

    /// Merge the layer into the base image; how much was committed.
    fn cow_commit(&mut self) -> io::Result<usize> { Ok(0) }

    /// Discard the layer's writes.
    fn cow_reset(&mut self) -> io::Result<()> { Ok(()) }

    /// Copy the layer to `dest` and return its dirty sectors; `None` when
    /// there is no layer.
    fn cow_export(&mut self, _dest: &std::path::Path) -> io::Result<Option<Vec<u64>>> { Ok(None) }

    /// Replace the layer with `source`, holding `dirty`.
    fn cow_import(&mut self, _source: &std::path::Path, _dirty: Vec<u64>) -> io::Result<()> { Ok(()) }

    /// (base, diff) of a CHD whose diff holds changes to fold back.
    fn pending_chd_sync(&self) -> Option<(std::path::PathBuf, std::path::PathBuf)> { None }

    /// [`ScsiTarget::pending_chd_sync`], releasing the files so the base can
    /// be rebuilt.
    fn take_pending_chd_sync(&mut self) -> Option<(std::path::PathBuf, std::path::PathBuf)> { None }

    // ---- disc changer ------------------------------------------------

    /// The discs, `None` unless this is a CD-ROM.
    fn discs(&self) -> Option<Discs> { None }

    /// Make `path` the active disc.
    fn load_disc(&mut self, _path: String) -> Result<String, String> { Err(NOT_A_CDROM.into()) }

    /// Eject, and load the next disc in the queue; `None` with none to load.
    fn eject_next(&mut self) -> Option<String> { None }

    /// Eject and leave the drive empty.
    fn unload_media(&mut self) {}

    /// Queue `path` as the next disc.
    fn add_disc(&mut self, _path: String) -> Result<(), String> { Err(NOT_A_CDROM.into()) }

    /// Remove the queued disc at `ordinal`.
    fn remove_disc(&mut self, _ordinal: usize) -> Result<String, String> { Err(NOT_A_CDROM.into()) }

    /// Make the queued disc at `ordinal` the next.
    fn move_disc_next(&mut self, _ordinal: usize) -> Result<(), String> { Err(NOT_A_CDROM.into()) }
}

const NOT_A_CDROM: &str = "Not a CD-ROM device";

/// A DaynaPort answers its own vendor command set, which reuses the READ(6)
/// and WRITE(6) opcodes for packet receive and transmit; it answers no
/// storage command at all.
impl ScsiTarget for DaynaPort {
    fn request(&mut self, req: &ScsiRequest) -> io::Result<ScsiResponse> {
        let mut response = DaynaPort::request(self, req)?;
        if let super::ScsiDataLength::Fixed(max_len) = req.data_len {
            response.data.truncate(max_len.min(response.data.len()));
        }
        Ok(response)
    }
    fn power_on(&mut self) { DaynaPort::power_on(self) }
    fn start(&mut self) { DaynaPort::start(self) }
    fn stop(&mut self) { DaynaPort::stop(self) }
    fn moves_packets(&self) -> bool { true }
    fn status(&self, _id: usize) -> Vec<String> { self.status_lines().into_iter().take(1).collect() }
    fn adapter_status(&self) -> Vec<String> { self.status_lines() }
}

/// What a controller's disks and CD-ROMs report in INQUIRY. The command set
/// is the same on every machine; who a machine says its drives are is its
/// own, so the controller gives them to its bus.
#[derive(Clone, Copy, Debug)]
pub struct ScsiIdentities {
    pub disk: ScsiIdentity,
    pub cdrom: ScsiIdentity,
}

/// The targets on a bus by SCSI id, `None` where nothing answers.
pub type Targets = Vec<Option<Box<dyn ScsiTarget>>>;

/// One SCSI bus: a controller's table of targets and the methods that fill
/// and query it. Locking is the bus's own; the controller holds a bus beside
/// its register state, so a monitor command or a snapshot reaches the
/// targets without the chip's lock.
pub struct ScsiBus {
    identities: ScsiIdentities,
    targets: Mutex<Targets>,
}

impl ScsiBus {
    /// A bus of `ids` SCSI ids (8 narrow, 16 wide) with nothing on it.
    pub fn new(ids: usize, identities: ScsiIdentities) -> Self {
        Self {
            identities,
            targets: Mutex::new(std::iter::repeat_with(|| None).take(ids).collect()),
        }
    }

    /// Take the table, for a controller that has to hold it across a check
    /// and a command.
    pub fn lock(&self) -> MutexGuard<'_, Targets> {
        self.targets.lock()
    }

    /// Whether the table is held; for the lock registry.
    pub fn is_locked(&self) -> bool {
        self.targets.is_locked()
    }

    /// Run `f` over the table under the lock.
    pub fn with_targets<R>(&self, f: impl FnOnce(&mut [Option<Box<dyn ScsiTarget>>]) -> R) -> R {
        f(&mut self.targets.lock())
    }

    /// Put `target` at `id`, replacing what was there.
    pub fn attach(&self, id: u8, target: Box<dyn ScsiTarget>) -> io::Result<()> {
        let mut targets = self.targets.lock();
        let slot = targets.get_mut(id as usize).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("SCSI id {id} out of range"))
        })?;
        *slot = Some(target);
        Ok(())
    }

    /// Attach what a `[scsi.N]` table describes: the image at `path` (empty
    /// for a CD-ROM with its tray empty), a CD-ROM's changer list `discs`,
    /// and copy-on-write when `overlay` is set, to `overlay_path` or the
    /// default place ([`ScsiDevice::open`]). The bus gives it its INQUIRY
    /// identity. Returns the size in 512-byte blocks.
    pub fn attach_device(
        &self,
        id: u8,
        path: &str,
        cdrom: bool,
        discs: Vec<String>,
        overlay: bool,
        overlay_path: Option<&str>,
    ) -> io::Result<u64> {
        let identity = if cdrom { self.identities.cdrom } else { self.identities.disk };
        let dev = ScsiDevice::open(path, cdrom, discs, overlay, overlay_path, identity)?;
        let blocks = dev.size() / 512;
        self.attach(id, Box::new(dev))?;
        Ok(blocks)
    }

    /// A raw image as a disk or CD-ROM, for the controllers' tests.
    #[cfg(test)]
    pub fn attach_image(&self, id: u8, path: &std::path::Path, cdrom: bool) -> io::Result<u64> {
        self.attach_device(id, path.to_str().unwrap(), cdrom, Vec::new(), false, None)
    }

    /// Whether something answers at `id`.
    pub fn has_target(&self, id: u8) -> bool {
        self.targets.lock().get(id as usize).is_some_and(|t| t.is_some())
    }

    /// Run a command on the target at `id`; `None` when nothing answers.
    pub fn request(&self, id: u8, req: &ScsiRequest) -> Option<io::Result<ScsiResponse>> {
        self.targets.lock().get_mut(id as usize)?.as_mut().map(|t| t.request(req))
    }

    /// Run `f` on every target, with its id.
    pub fn for_each(&self, mut f: impl FnMut(usize, &mut dyn ScsiTarget)) {
        for (id, t) in self.targets.lock().iter_mut().enumerate() {
            if let Some(t) = t {
                f(id, &mut **t);
            }
        }
    }

    /// Copy each target's copy-on-write overlay into `dir` as
    /// `scsi<id>.overlay`, and return the dirty sectors of each.
    pub fn export_overlays(&self, dir: &std::path::Path) -> io::Result<Vec<(usize, Vec<u64>)>> {
        let mut out = Vec::new();
        let mut err = None;
        self.for_each(|id, t| {
            if err.is_none() {
                match t.cow_export(&dir.join(format!("scsi{id}.overlay"))) {
                    Ok(Some(dirty)) => out.push((id, dirty)),
                    Ok(None) => {}
                    Err(e) => err = Some(e),
                }
            }
        });
        err.map_or(Ok(out), Err)
    }

    /// Put back the overlays [`Self::export_overlays`] wrote. A target with
    /// no entry in `dirty_sets` keeps its overlay.
    pub fn import_overlays(&self, dir: &std::path::Path, dirty_sets: &[(usize, Vec<u64>)]) -> io::Result<()> {
        let mut targets = self.targets.lock();
        for (id, dirty) in dirty_sets {
            if let Some(t) = targets.get_mut(*id).and_then(|t| t.as_mut()) {
                t.cow_import(&dir.join(format!("scsi{id}.overlay")), dirty.clone())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::scsi_cmd;

    const ID: ScsiIdentity = ScsiIdentity::new(b"TEST    ", b"BUS             ", b"1.0 ");

    fn bus(ids: usize) -> ScsiBus {
        ScsiBus::new(ids, ScsiIdentities { disk: ID, cdrom: ID })
    }

    fn attach(b: &ScsiBus, id: u8, p: &std::path::Path, overlay: bool) -> io::Result<u64> {
        b.attach_device(id, p.to_str().unwrap(), false, Vec::new(), overlay, None)
    }

    fn image(tag: &str, blocks: usize) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("iris-scsibus-{tag}-{}", std::process::id()));
        std::fs::write(&p, vec![0u8; blocks * 512]).unwrap();
        p
    }

    fn inquiry() -> ScsiRequest {
        ScsiRequest {
            cdb: vec![scsi_cmd::INQUIRY, 0, 0, 0, 36, 0],
            data_len: crate::scsi::ScsiDataLength::Unlimited,
            data_in: None,
        }
    }

    #[test]
    fn attach_query_and_answer_with_the_buses_identity() {
        let b = bus(8);
        let p = image("attach", 8);
        assert!(!b.has_target(3));
        assert_eq!(attach(&b, 3, &p, false).unwrap(), 8);
        assert!(b.has_target(3));
        let r = b.request(3, &inquiry()).expect("target answers").unwrap();
        assert_eq!(&r.data[8..16], b"TEST    ");
        assert!(b.request(2, &inquiry()).is_none(), "an empty id does not answer");
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn an_id_the_bus_does_not_have_is_refused() {
        let b = bus(8);
        let p = image("range", 1);
        let e = attach(&b, 8, &p, false).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(attach(&bus(16), 8, &p, false).is_ok(), "a wide bus has id 8");
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn overlays_export_and_import_by_id() {
        let b = bus(8);
        let p = image("cow", 4);
        attach(&b, 2, &p, true).unwrap();
        let dir = std::env::temp_dir().join(format!("iris-scsibus-ov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sets = b.export_overlays(&dir).unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].0, 2);
        b.import_overlays(&dir, &sets).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_file(format!("{}.overlay", p.display()));
        std::fs::remove_file(p).unwrap();
    }
}
