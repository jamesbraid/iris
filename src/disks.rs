//! The machine's disks as the front ends see them: flush every SCSI bus, count
//! and fold pending CHD diffs, and name the controller whose CD-ROM drives take
//! disc changes. A machine registers each SCSI controller once, in bus order,
//! and the shared `Workstation` methods answer from here.
//!
//! A bus is owned by value by its controller, so a list of buses is a list of
//! controllers. `ScsiTargets` has a generic method and so cannot itself be a
//! trait object; [`DiskController`] is the small object-safe view of it.

use std::io;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::scsi::ScsiBus;
use crate::scsi_monitor::ScsiTargets;
use crate::workstation::DiscChanger;

/// A SCSI controller, as far as the disks are concerned.
pub trait DiskController: Send + Sync {
    fn bus(&self) -> &ScsiBus;
    fn changer(self: Arc<Self>) -> Arc<dyn DiscChanger>;
}

impl<T: ScsiTargets + Send + Sync + 'static> DiskController for T {
    fn bus(&self) -> &ScsiBus {
        self.scsi_bus()
    }
    fn changer(self: Arc<Self>) -> Arc<dyn DiscChanger> {
        self
    }
}

#[derive(Default)]
pub struct Disks {
    controllers: Mutex<Vec<Arc<dyn DiskController>>>,
}

impl Disks {
    pub fn new() -> Disks {
        Disks::default()
    }

    /// Register a controller. Order is bus order: the first is the machine's
    /// internal bus, which takes disc changes.
    pub fn track(&self, controller: Arc<dyn DiskController>) {
        self.controllers.lock().push(controller);
    }

    fn copy(&self) -> Vec<Arc<dyn DiskController>> {
        self.controllers.lock().clone()
    }

    /// Make every disk's accepted writes survive the process, including a
    /// copy-on-write overlay's record of the sectors it holds.
    pub fn flush(&self) -> io::Result<()> {
        self.copy().iter().try_for_each(|c| c.bus().flush())
    }

    /// Attached CHD disks whose `.diff.chd` holds changes waiting to be folded
    /// into their bases on a clean shutdown.
    pub fn pending_chd_sync_count(&self) -> usize {
        self.copy().iter().map(|c| c.bus().pending_chd_sync_count()).sum()
    }

    /// Fold pending CHD diffs back into their bases, preserving compression.
    /// `only` limits it to one SCSI id (on every bus). Call only after the
    /// guest has stopped, so disk I/O is quiesced. `progress(done, total,
    /// fraction)` drives a UI; `cancel()` aborts cleanly, leaving un-synced
    /// bases and diffs intact. Returns the count synced.
    pub fn sync_chd(
        &self,
        only: Option<usize>,
        progress: &mut dyn FnMut(usize, usize, f32),
        cancel: &dyn Fn() -> bool,
    ) -> io::Result<usize> {
        let controllers = self.copy();
        let buses: Vec<&ScsiBus> = controllers.iter().map(|c| c.bus()).collect();
        ScsiBus::sync_chd_disks(&buses, only, progress, cancel)
    }

    /// The first controller's CD-ROM drives take disc changes.
    pub fn disc_changer(&self) -> Option<Arc<dyn DiscChanger>> {
        self.copy().into_iter().next().map(|c| c.changer())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.controllers.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::{ScsiIdentities, ScsiRequest, ScsiResponse, ScsiTarget};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ID: crate::scsi::ScsiIdentity = crate::scsi::ScsiIdentity::new(b"TEST    ", b"DISKS           ", b"1.0 ");

    /// A target that counts flushes and may hold a CHD diff.
    struct Probe {
        flushed: Arc<AtomicUsize>,
        pending: bool,
    }

    impl ScsiTarget for Probe {
        fn request(&mut self, _req: &ScsiRequest) -> io::Result<ScsiResponse> {
            unreachable!("the disks never issue commands")
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn pending_chd_sync(&self) -> Option<(PathBuf, PathBuf)> {
            self.pending.then(|| (PathBuf::from("base.chd"), PathBuf::from("base.diff.chd")))
        }
        fn take_pending_chd_sync(&mut self) -> Option<(PathBuf, PathBuf)> {
            self.pending_chd_sync()
        }
    }

    struct Ctl(ScsiBus);

    impl ScsiTargets for Ctl {
        fn scsi_bus(&self) -> &ScsiBus {
            &self.0
        }
    }

    fn ctl(targets: &[(u8, Arc<AtomicUsize>, bool)]) -> Arc<Ctl> {
        let bus = ScsiBus::new(8, ScsiIdentities { disk: ID, cdrom: ID });
        for (id, flushed, pending) in targets {
            bus.attach(*id, Box::new(Probe { flushed: flushed.clone(), pending: *pending })).unwrap();
        }
        Arc::new(Ctl(bus))
    }

    fn counter() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    #[test]
    fn flush_reaches_every_tracked_bus() {
        let (a, b, c) = (counter(), counter(), counter());
        let disks = Disks::new();
        disks.track(ctl(&[(1, a.clone(), false), (2, b.clone(), false)]));
        disks.track(ctl(&[(1, c.clone(), false)]));
        assert_eq!(disks.len(), 2);
        disks.flush().unwrap();
        assert_eq!([&a, &b, &c].map(|x| x.load(Ordering::SeqCst)), [1, 1, 1]);
    }

    #[test]
    fn pending_chd_sync_count_sums_the_buses() {
        let disks = Disks::new();
        disks.track(ctl(&[(1, counter(), true), (2, counter(), false)]));
        disks.track(ctl(&[(1, counter(), true), (3, counter(), true)]));
        assert_eq!(disks.pending_chd_sync_count(), 3);
    }

    #[test]
    fn sync_chd_with_nothing_pending_returns_zero() {
        let disks = Disks::new();
        disks.track(ctl(&[(1, counter(), false)]));
        let n = disks.sync_chd(None, &mut |_, _, _| {}, &|| false).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn disc_changer_is_the_first_tracked_controller() {
        let disks = Disks::new();
        let first = ctl(&[]);
        disks.track(first.clone());
        disks.track(ctl(&[]));
        // The changer is the same allocation as the first controller.
        let changer = disks.disc_changer().expect("a changer");
        assert_eq!(Arc::as_ptr(&changer) as *const (), Arc::as_ptr(&first) as *const ());
    }

    #[test]
    fn no_controllers_means_no_changer_and_a_clean_flush() {
        let disks = Disks::new();
        assert!(disks.disc_changer().is_none());
        assert!(disks.flush().is_ok());
        assert_eq!(disks.pending_chd_sync_count(), 0);
    }
}
