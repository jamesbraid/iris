//! The machine's disks as the front ends see them: attach an image to a
//! target, flush every SCSI bus, count and fold pending CHD diffs, and name the
//! controller whose CD-ROM drives take disc changes. A machine registers each
//! SCSI controller once, in bus order, and the shared `Workstation` methods
//! answer from here.
//!
//! A bus is owned by value by its controller, so a list of buses is a list of
//! controllers. `ScsiTargets` has a generic method and so cannot itself be a
//! trait object; [`DiskController`] is the small object-safe view of it.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::location::Location;
use crate::scsi::{ScsiBus, ScsiPort};
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

/// What a hard disk or CD-ROM target is given to open.
#[derive(Default, Clone)]
pub struct MediaSpec {
    pub path: String,
    pub discs: Vec<String>,
    pub cdrom: bool,
    pub overlay: bool,
    pub scratch: bool,
    pub size_mb: Option<u32>,
    pub write_cache: bool,
}

impl MediaSpec {
    /// The media a target starts with: the active image and, for a CD-ROM, the
    /// changer list built from `path` and `discs` with empty entries skipped
    /// (an empty `path` is a drive with its tray empty, to be loaded at run
    /// time). A disk is just its path.
    pub fn media(&self) -> (String, Vec<String>) {
        if !self.cdrom {
            return (self.path.clone(), vec![]);
        }
        let mut list: Vec<String> = Vec::new();
        if !self.path.is_empty() {
            list.push(self.path.clone());
        }
        for d in &self.discs {
            if !d.is_empty() && !list.contains(d) {
                list.push(d.clone());
            }
        }
        let active = list.first().cloned().unwrap_or_default();
        (active, list)
    }

    /// For `scratch = true`: make sure the volume exists, creating it with a
    /// minimal SGI volume header and `size_mb` MB when it does not, and return
    /// its path for the CI socket's scratch commands. None when the target is
    /// not a scratch volume, or cannot be one (a CD-ROM or an overlay is not
    /// host-writable in place), which is reported.
    pub fn prepare_scratch(&self, at: &str) -> Option<PathBuf> {
        if !self.scratch {
            return None;
        }
        if self.cdrom || self.overlay {
            println!("Note: {at}: scratch=true is incompatible with cdrom/overlay; ignoring scratch flag");
            return None;
        }
        let path = std::path::Path::new(&self.path);
        if !path.exists() {
            let size_mb = self.size_mb.unwrap_or(64) as u64;
            match crate::sgi_vh::create_scratch_image(path, size_mb * 1024 * 1024) {
                Ok(()) => println!("iris: created scratch volume {} ({} MB, with SGI VH)", self.path, size_mb),
                Err(e) => println!("Note: could not create scratch volume {}: {}", self.path, e),
            }
        }
        Some(path.to_path_buf())
    }
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

/// Open the image and put it on target `id` of `bus`, logging it as
/// `{log}: {at}: ...` when there is a prefix. `ci_overlay` sends a `--ci`
/// run's copy-on-write overlay to the temporary directory, so an interactive
/// iris holding one and any number of CI runs can share the image.
pub(crate) fn attach_target(
    bus: &ScsiBus,
    id: u8,
    at: &str,
    ci_overlay: Option<&str>,
    spec: &MediaSpec,
    log: Option<&str>,
) -> Result<(), String> {
    let (path, discs) = spec.media();
    match bus.attach_device(id, &path, spec.cdrom, discs, spec.overlay, ci_overlay, spec.write_cache) {
        Ok(blocks) => {
            if let Some(prefix) = log {
                if path.is_empty() {
                    eprintln!("{prefix}: {at}: CD-ROM, tray empty");
                } else {
                    eprintln!("{prefix}: {at}: {path} ({blocks} blocks)");
                }
            }
            Ok(())
        }
        Err(e) => {
            let msg = match ci_overlay {
                Some(ov) => format!("could not attach {path} to {at} with its --ci overlay {ov}: {e}"),
                None => format!("could not attach {path} to {at}: {e}"),
            };
            attach_failed(msg, at)
        }
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

#[derive(Default)]
pub struct Disks {
    controllers: Mutex<Vec<Arc<dyn DiskController>>>,
    /// A `--ci` run: overlays go to the temporary directory.
    ci: bool,
    /// The `scratch = true` volume the CI socket reads and writes: the lowest
    /// location, if several are configured.
    scratch: Mutex<Option<(Location, PathBuf)>>,
    records: Mutex<Vec<MediaRecord>>,
}

impl Disks {
    pub fn new() -> Disks {
        Disks::default()
    }

    pub fn with_ci(mut self, ci: bool) -> Disks {
        self.ci = ci;
        self
    }

    /// Put the image of `spec` on the target `at` names, on `port`'s bus: make
    /// a scratch volume if asked, send a CI run's overlay to the temporary
    /// directory, attach, and apply the `attach_failed` policy. The target is
    /// recorded for a snapshot whether or not it attached.
    pub fn attach_media(&self, port: &ScsiPort, at: &Location, spec: &MediaSpec) -> Result<(), String> {
        let id = at.target().ok_or_else(|| format!("{at} is not a SCSI target"))?;
        let name = at.to_string();
        self.records.lock().push(MediaRecord {
            at: at.clone(),
            cdrom: spec.cdrom,
            path: spec.path.clone(),
            size_bytes: std::fs::metadata(&spec.path).map(|m| m.len()).unwrap_or(0),
        });
        // The file gets an SGI volume header, without which IRIX returns I/O
        // errors on every read of the volume.
        if let Some(path) = spec.prepare_scratch(&name) {
            let mut scratch = self.scratch.lock();
            match &*scratch {
                Some((first, _)) if first < at => {
                    println!("Note: multiple scratch SCSI devices configured; CI socket will use the lowest location");
                }
                other => {
                    if other.is_some() {
                        println!("Note: multiple scratch SCSI devices configured; CI socket will use the lowest location");
                    }
                    *scratch = Some((at.clone(), path));
                }
            }
        }
        let ci_overlay = (self.ci && spec.overlay && !spec.cdrom).then(|| ci_overlay_path(&name));
        attach_target(port.bus(), id, &name, ci_overlay.as_deref(), spec, Some("iris"))
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

    fn disk(path: &str) -> MediaSpec {
        MediaSpec { path: path.into(), write_cache: true, ..MediaSpec::default() }
    }

    #[test]
    fn a_target_lands_on_the_id_in_its_location() {
        let (a, b) = (image("land-a", 4096), image("land-b", 4096));
        let (disks, p) = (Disks::new(), port());
        disks.attach_media(&p, &at("hpc3.0.t1"), &disk(&a)).unwrap();
        disks.attach_media(&p, &at("hpc3.0.t5"), &disk(&b)).unwrap();
        assert_eq!((0..16).filter(|&i| p.bus().has_target(i)).collect::<Vec<u8>>(), vec![1, 5]);
        assert!(disks.attach_media(&p, &at("hpc3.0"), &disk(&a)).is_err(), "not a target");
        for f in [a, b] {
            std::fs::remove_file(f).unwrap();
        }
    }

    #[test]
    fn scratch_is_the_lowest_location() {
        let dir = std::env::temp_dir().join(format!("iris-scratch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (hi, lo) = (dir.join("hi.img"), dir.join("lo.img"));
        let scratch = |p: &std::path::Path| MediaSpec { scratch: true, size_mb: Some(1), ..disk(p.to_str().unwrap()) };
        let (disks, p) = (Disks::new(), port());
        disks.attach_media(&p, &at("hpc3.0.t3"), &scratch(&hi)).unwrap();
        disks.attach_media(&p, &at("hpc3.0.t1"), &scratch(&lo)).unwrap();
        assert_eq!(disks.scratch(), Some(lo));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_attach_ends_the_run_unless_embedded() {
        let e = attach_failed_in(false, "could not attach x".into(), "hpc3.0.t1").unwrap_err();
        assert_eq!(e, "could not attach x");
        assert!(attach_failed_in(true, "could not attach x".into(), "hpc3.0.t1").is_ok());
        let (disks, p) = (Disks::new(), port());
        let e = disks.attach_media(&p, &at("hpc3.0.t1"), &disk("/nonexistent/disk.img"));
        if std::env::var_os("IRIS_NO_EXIT_ON_POWEROFF").is_none() {
            assert!(e.unwrap_err().contains("could not attach /nonexistent/disk.img to hpc3.0.t1"));
        }
    }

    #[test]
    fn records_list_disks_and_cdroms_with_size() {
        let (a, iso) = (image("rec-a", 4096), image("rec-iso", 2048));
        let (disks, p) = (Disks::new(), port());
        disks.attach_media(&p, &at("hpc3.0.t4"), &MediaSpec { cdrom: true, ..disk(&iso) }).unwrap();
        disks.attach_media(&p, &at("hpc3.0.t1"), &disk(&a)).unwrap();
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

    #[test]
    fn a_cd_rom_is_its_list_with_empty_entries_skipped() {
        let spec = MediaSpec { cdrom: true, path: "a.iso".into(), discs: vec!["".into(), "b.iso".into(), "a.iso".into()], ..MediaSpec::default() };
        assert_eq!(spec.media(), ("a.iso".to_string(), vec!["a.iso".to_string(), "b.iso".to_string()]));
        let empty = MediaSpec { cdrom: true, ..MediaSpec::default() };
        assert_eq!(empty.media(), (String::new(), vec![]));
        assert_eq!(disk("x.img").media(), ("x.img".to_string(), vec![]));
    }
}
