//! The machine's disks as the front ends see them: attach an image to a
//! target, flush every SCSI bus, count and fold pending CHD diffs, and name the
//! controller whose CD-ROM drives take disc changes. The buses are the ones
//! bound in the machine's socket map, so there is no list to keep in step.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::location::Location;
use crate::scsi::media::{CdromOptions, HddOptions};
use crate::scsi::{ScsiBus, ScsiPort};
use crate::scsi_monitor::ScsiTargets;
use crate::topology::Topology;
use crate::workstation::DiscChanger;

/// A SCSI controller, as far as the disks are concerned. (`ScsiTargets` has a
/// generic method and so is not an object-safe trait itself.)
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

/// A target that will not attach ends a command-line run, since booting on
/// without it only yields a confusing PROM "no such device" later. An
/// embedder (the GUI sets `IRIS_NO_EXIT_ON_POWEROFF`) must never be torn
/// down by the library, so it skips the one target and boots without it; the
/// GUI's Start preflight already warns about an unreadable disk.
pub fn attach_failed(msg: String, at: &str) -> Result<(), String> {
    attach_failed_in(std::env::var_os("IRIS_NO_EXIT_ON_POWEROFF").is_some(), msg, at)
}

fn attach_failed_in(embedded: bool, msg: String, at: &str) -> Result<(), String> {
    if embedded {
        eprintln!("iris: warning: {msg}; continuing without {at}");
        Ok(())
    } else {
        Err(msg)
    }
}

/// Where a `--ci` run keeps the copy-on-write overlay of the disk named
/// `name`: in the host's temporary directory (`$TMPDIR`, or `/tmp` when it is
/// unset), as an absolute path, so a later change of directory still finds it.
///
/// The name carries [`ci_run_tag`], so an interactive iris holding
/// `PATH.overlay`, any number of CI runs, and a later run that gets the same
/// process ID never open each other's overlay. A machine rebuilt in the same
/// process reopens its own. The files are kept after the run for inspection.
pub fn ci_overlay_path(name: &str) -> String {
    static RUN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let run = RUN.get_or_init(|| {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|before| before.duration());
        ci_run_tag(std::process::id(), started)
    });
    let path = std::env::temp_dir().join(format!("iris-ci-{run}-{name}.overlay"));
    std::path::absolute(&path).unwrap_or(path).to_string_lossy().into_owned()
}

/// One run's part of its overlay names: the process ID and the time the run
/// first named an overlay, in nanoseconds since the epoch, or with a `b` before
/// it when the host clock is set before the epoch, so such a clock still gives
/// each run its own name.
fn ci_run_tag(pid: u32, started: Result<std::time::Duration, std::time::Duration>) -> String {
    match started {
        Ok(after) => format!("{pid}-{:x}", after.as_nanos()),
        Err(before) => format!("{pid}-b{:x}", before.as_nanos()),
    }
}

/// For `scratch = true`: make sure the volume exists, creating it with a
/// minimal SGI volume header and `size_mb` MB when it does not, and return
/// its path for the CI socket's scratch commands. None when the target is
/// not a scratch volume, or cannot be one (an overlay is not host-writable
/// in place), which is reported.
fn prepare_scratch(o: &HddOptions, at: &str) -> Option<PathBuf> {
    if !o.scratch {
        return None;
    }
    if o.overlay {
        println!("Note: {at}: scratch=true is incompatible with overlay; ignoring scratch flag");
        return None;
    }
    let path = std::path::Path::new(&o.path);
    if !path.exists() {
        let size_mb = o.size_mb.unwrap_or(64) as u64;
        match crate::sgi_vh::create_scratch_image(path, size_mb * 1024 * 1024) {
            Ok(()) => println!("iris: created scratch volume {} ({} MB, with SGI VH)", o.path, size_mb),
            Err(e) => println!("Note: could not create scratch volume {}: {}", o.path, e),
        }
    }
    Some(path.to_path_buf())
}

/// One disk or CD-ROM, as a snapshot manifest records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaRecord {
    pub at: Location,
    pub cdrom: bool,
    pub path: String,
    /// The image's size on the host, 0 if it cannot be read.
    pub size_bytes: u64,
}

pub struct Disks {
    /// The machine's socket map, whose rows hold the SCSI controllers.
    topology: Arc<Topology>,
    /// A `--ci` run: overlays go to the temporary directory.
    ci: bool,
    /// The `scratch = true` volume the CI socket reads and writes: the lowest
    /// location, if several are configured.
    scratch: Mutex<Option<(Location, PathBuf)>>,
    records: Mutex<Vec<MediaRecord>>,
    /// Controllers with disks that have no row in the map yet: the NVMe
    /// card's namespace.
    unplaced: Mutex<Vec<Arc<dyn DiskController>>>,
}

impl Disks {
    pub fn new(topology: Arc<Topology>) -> Disks {
        Disks { topology, ci: false, scratch: Mutex::new(None), records: Mutex::new(Vec::new()), unplaced: Mutex::new(Vec::new()) }
    }

    pub fn with_ci(mut self, ci: bool) -> Disks {
        self.ci = ci;
        self
    }

    /// Count `ctl`'s disks in the flush and the CHD sync, though no row of the
    /// map holds it.
    pub fn add_unplaced(&self, ctl: Arc<dyn DiskController>) {
        self.unplaced.lock().push(ctl);
    }

    /// Every bus with disks on it: the bound controllers in location order,
    /// then the unplaced ones.
    fn buses(&self) -> Vec<Arc<dyn DiskController>> {
        let mut all: Vec<Arc<dyn DiskController>> =
            self.topology.buses::<ScsiPort>().into_iter().map(|(_, p)| p.controller().clone()).collect();
        all.extend(self.unplaced.lock().iter().cloned());
        all
    }

    /// How many buses with disks the machine has.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.buses().len()
    }

    /// The socket map the disks are placed in.
    pub fn topology(&self) -> &Arc<Topology> {
        &self.topology
    }

    /// Put a hard disk on the target `at` names, on `port`'s bus: make a
    /// scratch volume if asked, send a CI run's overlay to the temporary
    /// directory, attach, and apply the `attach_failed` policy. The target is
    /// recorded for a snapshot whether or not it attached.
    pub fn attach_hdd(&self, port: &ScsiPort, at: &Location, o: &HddOptions) -> Result<(), String> {
        self.record(at, false, &o.path);
        // The file gets an SGI volume header, without which IRIX returns I/O
        // errors on every read of the volume.
        if let Some(path) = prepare_scratch(o, &at.to_string()) {
            let mut scratch = self.scratch.lock();
            if scratch.is_some() {
                println!("Note: multiple scratch SCSI devices configured; CI socket will use the lowest location");
            }
            if scratch.as_ref().is_none_or(|(first, _)| at < first) {
                *scratch = Some((at.clone(), path));
            }
        }
        // CI mode isolates each copy-on-write overlay in the temporary
        // directory, so an interactive iris holding one and any number of CI
        // runs can share the image.
        let ci_overlay = (self.ci && o.overlay).then(|| ci_overlay_path(&at.to_string()));
        let id = at.target().ok_or_else(|| format!("{at} is not a SCSI target"))?;
        let r = port.bus().attach_device(id, &o.path, false, vec![], o.overlay, ci_overlay.as_deref(), o.write_cache);
        Self::report(r, at, &o.path, ci_overlay.as_deref())
    }

    /// Put a CD-ROM drive on the target `at` names; see [`Self::attach_hdd`].
    pub fn attach_cdrom(&self, port: &ScsiPort, at: &Location, o: &CdromOptions) -> Result<(), String> {
        self.record(at, true, &o.path);
        let id = at.target().ok_or_else(|| format!("{at} is not a SCSI target"))?;
        let (path, discs) = o.media();
        let r = port.bus().attach_device(id, &path, true, discs, false, None, false);
        Self::report(r, at, &path, None)
    }

    fn record(&self, at: &Location, cdrom: bool, path: &str) {
        let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        self.records.lock().push(MediaRecord { at: at.clone(), cdrom, path: path.to_owned(), size_bytes });
    }

    /// Say how the attach went, and apply the `attach_failed` policy.
    fn report(r: io::Result<u64>, at: &Location, path: &str, ci_overlay: Option<&str>) -> Result<(), String> {
        match r {
            Ok(_) if path.is_empty() => {
                eprintln!("iris: {at}: CD-ROM, tray empty");
                Ok(())
            }
            Ok(blocks) => {
                eprintln!("iris: {at}: {path} ({blocks} blocks)");
                Ok(())
            }
            Err(e) => {
                let msg = match ci_overlay {
                    Some(ov) => format!("could not attach {path} to {at} with its --ci overlay {ov}: {e}"),
                    None => format!("could not attach {path} to {at}: {e}"),
                };
                attach_failed(msg, &at.to_string())
            }
        }
    }

    /// Every disk and CD-ROM placed, by location.
    pub fn records(&self) -> Vec<MediaRecord> {
        let mut r = self.records.lock().clone();
        r.sort_by(|a, b| a.at.cmp(&b.at));
        r
    }

    /// The scratch volume the CI socket uses.
    pub fn scratch(&self) -> Option<PathBuf> {
        self.scratch.lock().as_ref().map(|s| s.1.clone())
    }

    /// Make every disk's accepted writes survive the process, including a
    /// copy-on-write overlay's record of the sectors it holds.
    pub fn flush(&self) -> io::Result<()> {
        self.buses().iter().try_for_each(|c| c.bus().flush())
    }

    /// Attached CHD disks whose `.diff.chd` holds changes waiting to be folded
    /// into their bases on a clean shutdown.
    pub fn pending_chd_sync_count(&self) -> usize {
        self.buses().iter().map(|c| c.bus().pending_chd_sync_count()).sum()
    }

    /// Fold pending CHD diffs back into their bases, preserving compression.
    /// `only` limits it to one target (a location that is a bare `tN` names
    /// that target on every controller); `None` is every disk. Call only
    /// after the guest has stopped, so disk I/O is quiesced. `progress(done,
    /// total, fraction)` drives a UI; `cancel()` aborts cleanly, leaving
    /// un-synced bases and diffs intact. Returns the count synced.
    pub fn sync_chd(
        &self,
        only: Option<&Location>,
        progress: &mut dyn FnMut(usize, usize, f32),
        cancel: &dyn Fn() -> bool,
    ) -> io::Result<usize> {
        let ctl = only.and_then(Location::parent).filter(|p| p.depth() > 0);
        let ports: Vec<Arc<dyn DiskController>> = match &ctl {
            Some(c) => self.topology.handle::<ScsiPort>(c).map(|p| p.controller().clone()).into_iter().collect(),
            None => self.buses(),
        };
        let buses: Vec<&ScsiBus> = ports.iter().map(|c| c.bus()).collect();
        ScsiBus::sync_chd_disks(&buses, only.and_then(Location::target).map(usize::from), progress, cancel)
    }

    /// The changer of the controller the drive at `at` is on, and its id.
    pub fn drive(&self, at: &Location) -> Result<(Arc<dyn DiscChanger>, usize), String> {
        let id = at.target().ok_or_else(|| format!("{at} is not a SCSI target"))?;
        let port = at
            .parent()
            .and_then(|p| self.topology.handle::<ScsiPort>(&p))
            .ok_or_else(|| format!("{at}: no SCSI controller there"))?;
        Ok((port.changer(), id as usize))
    }

    /// The first CD-ROM in location order, with the changer of the controller
    /// it sits on: its drives take disc changes.
    pub fn disc_changer(&self) -> Option<(Location, Arc<dyn DiscChanger>)> {
        let first = self.records().into_iter().find(|r| r.cdrom)?;
        let port = self.topology.handle::<ScsiPort>(&first.at.parent()?)?;
        Some((first.at, port.changer()))
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

    /// A map with one controller row per entry of `ctls`, each bound to a port,
    /// and the disks that look into it.
    fn topo(ctls: Vec<Arc<Ctl>>) -> (Arc<Topology>, Disks) {
        let t = Arc::new(Topology::new());
        for (n, ctl) in ctls.into_iter().enumerate() {
            let at: Location = format!("hpc3.{n}").parse().unwrap();
            t.add(at.clone(), crate::topology::Row::fixed("").ctl(n as u8)).unwrap();
            t.bind(&at, Arc::new(ScsiPort::new(ctl))).unwrap();
        }
        let disks = Disks::new(t.clone());
        (t, disks)
    }

    #[test]
    fn flush_reaches_every_bound_port() {
        let (a, b, c) = (counter(), counter(), counter());
        let (_, disks) = topo(vec![ctl(&[(1, a.clone(), false), (2, b.clone(), false)]), ctl(&[(1, c.clone(), false)])]);
        disks.flush().unwrap();
        assert_eq!([&a, &b, &c].map(|x| x.load(Ordering::SeqCst)), [1, 1, 1]);
    }

    #[test]
    fn pending_chd_sync_count_sums() {
        let (_, disks) = topo(vec![ctl(&[(1, counter(), true), (2, counter(), false)]), ctl(&[(1, counter(), true), (3, counter(), true)])]);
        assert_eq!(disks.pending_chd_sync_count(), 3);
    }

    #[test]
    fn sync_chd_with_nothing_pending_returns_zero() {
        let (_, disks) = topo(vec![ctl(&[(1, counter(), false)])]);
        assert_eq!(disks.sync_chd(None, &mut |_, _, _| {}, &|| false).unwrap(), 0);
    }

    /// One location limits the sync to the controller and target it names; a
    /// bare target id names that target on every controller.
    #[test]
    fn sync_chd_only_one_location() {
        let (_, disks) = topo(vec![ctl(&[(1, counter(), true)]), ctl(&[(1, counter(), true), (3, counter(), true)])]);
        let sync = |l: &str| disks.sync_chd(Some(&at(l)), &mut |_, _, _| {}, &|| false).unwrap();
        assert_eq!(sync("hpc3.1.t2"), 0, "nothing at that target");
        assert_eq!(disks.pending_chd_sync_count(), 3);
        assert_eq!(sync("t9"), 0, "no such target on any controller");
    }

    /// A CD-ROM on the second controller, with none on the first, is found.
    #[test]
    fn disc_changer_is_the_first_cdrom_in_location_order() {
        let iso = image("changer-iso", 2048);
        let (first, second) = (ctl(&[]), ctl(&[]));
        let (t, disks) = topo(vec![first.clone(), second.clone()]);
        assert!(disks.disc_changer().is_none(), "no CD-ROM yet");
        let port = t.handle::<ScsiPort>(&"hpc3.1".parse().unwrap()).unwrap();
        disks.attach_cdrom(&port, &"hpc3.1.t4".parse().unwrap(), &CdromOptions { path: iso.clone(), discs: vec![] }).unwrap();
        let (at, changer) = disks.disc_changer().expect("a changer");
        assert_eq!(at.to_string(), "hpc3.1.t4");
        assert_eq!(Arc::as_ptr(&changer) as *const (), Arc::as_ptr(&second) as *const ());
        std::fs::remove_file(iso).unwrap();
    }

    #[test]
    fn an_empty_topology_has_no_changer_and_a_clean_flush() {
        let disks = Disks::new(Arc::new(Topology::new()));
        assert!(disks.disc_changer().is_none());
        assert!(disks.flush().is_ok());
        assert_eq!(disks.pending_chd_sync_count(), 0);
    }

    fn port() -> ScsiPort {
        let bus = ScsiBus::new(16, ScsiIdentities { disk: ID, cdrom: ID });
        ScsiPort::new(Arc::new(Ctl(bus)))
    }

    fn at(s: &str) -> Location {
        s.parse().unwrap()
    }

    fn image(tag: &str, len: usize) -> String {
        let p = std::env::temp_dir().join(format!("iris-media-{tag}-{}", std::process::id()));
        std::fs::write(&p, vec![0u8; len]).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn disk(path: &str) -> HddOptions {
        HddOptions { path: path.into(), ..HddOptions::default() }
    }

    fn lone() -> Disks {
        Disks::new(Arc::new(Topology::new()))
    }

    #[test]
    fn a_target_lands_on_the_id_in_its_location() {
        let (a, b) = (image("land-a", 4096), image("land-b", 4096));
        let (disks, p) = (lone(), port());
        disks.attach_hdd(&p, &at("hpc3.0.t1"), &disk(&a)).unwrap();
        disks.attach_hdd(&p, &at("hpc3.0.t5"), &disk(&b)).unwrap();
        assert_eq!((0..16).filter(|&i| p.bus().has_target(i)).collect::<Vec<u8>>(), vec![1, 5]);
        assert!(disks.attach_hdd(&p, &at("hpc3.0"), &disk(&a)).is_err(), "not a target");
        for f in [a, b] {
            std::fs::remove_file(f).unwrap();
        }
    }

    #[test]
    fn scratch_is_the_lowest_location() {
        let dir = std::env::temp_dir().join(format!("iris-scratch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (hi, lo) = (dir.join("hi.img"), dir.join("lo.img"));
        let scratch = |p: &std::path::Path| HddOptions { scratch: true, size_mb: Some(1), ..disk(p.to_str().unwrap()) };
        let (disks, p) = (lone(), port());
        disks.attach_hdd(&p, &at("hpc3.0.t3"), &scratch(&hi)).unwrap();
        disks.attach_hdd(&p, &at("hpc3.0.t1"), &scratch(&lo)).unwrap();
        assert_eq!(disks.scratch(), Some(lo));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_attach_ends_the_run_unless_embedded() {
        let e = attach_failed_in(false, "could not attach x".into(), "hpc3.0.t1").unwrap_err();
        assert_eq!(e, "could not attach x");
        assert!(attach_failed_in(true, "could not attach x".into(), "hpc3.0.t1").is_ok());
        let (disks, p) = (lone(), port());
        let e = disks.attach_hdd(&p, &at("hpc3.0.t1"), &disk("/nonexistent/disk.img"));
        if std::env::var_os("IRIS_NO_EXIT_ON_POWEROFF").is_none() {
            assert!(e.unwrap_err().contains("could not attach /nonexistent/disk.img to hpc3.0.t1"));
        }
    }

    #[test]
    fn records_list_disks_and_cdroms_with_size() {
        let (a, iso) = (image("rec-a", 4096), image("rec-iso", 2048));
        let (disks, p) = (lone(), port());
        disks.attach_cdrom(&p, &at("hpc3.0.t4"), &CdromOptions { path: iso.clone(), discs: vec![] }).unwrap();
        disks.attach_hdd(&p, &at("hpc3.0.t1"), &disk(&a)).unwrap();
        let got: Vec<(String, bool, u64)> =
            disks.records().iter().map(|r| (r.at.to_string(), r.cdrom, r.size_bytes)).collect();
        assert_eq!(got, vec![("hpc3.0.t1".into(), false, 4096), ("hpc3.0.t4".into(), true, 2048)]);
        for f in [a, iso] {
            std::fs::remove_file(f).unwrap();
        }
    }

    #[test]
    fn ci_overlay_names_include_the_location() {
        let a = ci_overlay_path("pci0.1.t1");
        let path = std::path::Path::new(&a);
        assert!(path.is_absolute(), "{a}");
        assert_eq!(path.parent(), std::path::absolute(std::env::temp_dir()).ok().as_deref());
        assert_eq!(a, ci_overlay_path("pci0.1.t1"));
        assert_ne!(a, ci_overlay_path("pci0.1.t2"));
        assert!(a.contains(&format!("iris-ci-{}-", std::process::id())) && a.ends_with("-pci0.1.t1.overlay"), "{a}");
    }

    /// Process IDs come round again, and a container gives iris the same one
    /// every time, so a later run must not take an earlier run's name and
    /// adopt its overlay.
    #[test]
    fn a_later_run_with_the_same_process_id_gets_its_own_overlay() {
        let first = ci_run_tag(2, Ok(std::time::Duration::from_nanos(1_000)));
        let later = ci_run_tag(2, Ok(std::time::Duration::from_nanos(2_000)));
        assert_ne!(first, later);
    }

    /// A host clock set before the epoch still names each run apart, and
    /// never as a run the same distance after it.
    #[test]
    fn a_clock_before_the_epoch_still_names_runs_apart() {
        use std::time::Duration;
        let tag = |t| ci_run_tag(2, t);
        let (a, b) = (tag(Err(Duration::from_nanos(5_000))), tag(Err(Duration::from_nanos(4_000))));
        assert_ne!(a, b);
        assert_ne!(a, tag(Ok(Duration::from_nanos(5_000))));
        assert_ne!(tag(Err(Duration::ZERO)), tag(Ok(Duration::ZERO)));
    }

    /// A disc loads, ejects and empties by the drive's location, which finds
    /// the changer of the controller it is on.
    #[test]
    fn drives_are_found_by_location() {
        let (a, b) = (image("status-a", 2048), image("status-b", 2048));
        let (t, disks) = topo(vec![ctl(&[]), ctl(&[])]);
        let second = t.handle::<ScsiPort>(&at("hpc3.1")).unwrap();
        let spec = CdromOptions { path: a.clone(), discs: vec![b.clone()] };
        disks.attach_cdrom(&second, &at("hpc3.1.t4"), &spec).unwrap();
        let (changer, id) = disks.drive(&at("hpc3.1.t4")).unwrap();
        assert_eq!(changer.eject_disc(id).unwrap(), b);
        changer.eject_to_empty(id).unwrap();
        assert_eq!(changer.disc_status()[0].1, "");
        assert_eq!(changer.load_disc(id, a.clone()).unwrap(), a);
        let (changer, id) = disks.drive(&at("hpc3.0.t4")).unwrap();
        assert!(changer.load_disc(id, a.clone()).is_err(), "no drive there");
        assert!(disks.drive(&at("hpc3.1")).is_err(), "not a target");
        for f in [a, b] {
            std::fs::remove_file(f).unwrap();
        }
    }
}
