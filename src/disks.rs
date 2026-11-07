//! The machine's disks as the front ends see them: attach an image to a
//! target, flush every SCSI bus, count and fold pending CHD diffs, and name the
//! controller whose CD-ROM drives take disc changes. The buses are the ones
//! bound in the machine's socket map, so there is no list to keep in step.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::location::{Hop, Location};
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
        let ci_overlay = (self.ci && spec.overlay && !spec.cdrom).then(|| ci_overlay_path(&name));
        let (path, discs) = spec.media();
        match port.bus().attach_device(id, &path, spec.cdrom, discs, spec.overlay, ci_overlay.as_deref(), spec.write_cache) {
            Ok(_) if path.is_empty() => {
                eprintln!("iris: {name}: CD-ROM, tray empty");
                Ok(())
            }
            Ok(blocks) => {
                eprintln!("iris: {name}: {path} ({blocks} blocks)");
                Ok(())
            }
            Err(e) => {
                let msg = match &ci_overlay {
                    Some(ov) => format!("could not attach {path} to {name} with its --ci overlay {ov}: {e}"),
                    None => format!("could not attach {path} to {name}: {e}"),
                };
                attach_failed(msg, &name)
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
    pub fn flush(&self, t: &Topology) -> io::Result<()> {
        t.buses::<ScsiPort>().iter().try_for_each(|(_, p)| p.bus().flush())
    }

    /// Attached CHD disks whose `.diff.chd` holds changes waiting to be folded
    /// into their bases on a clean shutdown.
    pub fn pending_chd_sync_count(&self, t: &Topology) -> usize {
        t.buses::<ScsiPort>().iter().map(|(_, p)| p.bus().pending_chd_sync_count()).sum()
    }

    /// Fold pending CHD diffs back into their bases, preserving compression.
    /// `only` limits it to one target; `None` is every disk. Call only after
    /// the guest has stopped, so disk I/O is quiesced. `progress(done, total,
    /// fraction)` drives a UI; `cancel()` aborts cleanly, leaving un-synced
    /// bases and diffs intact. Returns the count synced.
    pub fn sync_chd(
        &self,
        t: &Topology,
        only: Option<&Location>,
        progress: &mut dyn FnMut(usize, usize, f32),
        cancel: &dyn Fn() -> bool,
    ) -> io::Result<usize> {
        let ports = t.buses::<ScsiPort>();
        let parent = only.and_then(Location::parent);
        let buses: Vec<&ScsiBus> = ports.iter().filter(|(l, _)| parent.as_ref().is_none_or(|p| p == l)).map(|(_, p)| p.bus()).collect();
        ScsiBus::sync_chd_disks(&buses, only.and_then(Location::target).map(usize::from), progress, cancel)
    }

    /// Every CD-ROM drive: (location, current disc, the configured disc list,
    /// list position, list length), controllers in location order.
    pub fn disc_status(&self, t: &Topology) -> Vec<(Location, String, Vec<String>, u64, u64)> {
        let mut out = Vec::new();
        for (at, port) in t.buses::<ScsiPort>() {
            for (id, disc, list, pos, len) in port.changer().disc_status() {
                out.push((at.join(Hop::t(id as u8)), disc, list, pos, len));
            }
        }
        out
    }

    /// The changer of the controller the drive at `at` is on, and its id.
    fn drive(t: &Topology, at: &Location) -> Result<(Arc<dyn DiscChanger>, usize), String> {
        let id = at.target().ok_or_else(|| format!("{at} is not a SCSI target"))?;
        let port = at
            .parent()
            .and_then(|p| t.handle::<ScsiPort>(&p))
            .ok_or_else(|| format!("{at}: no SCSI controller there"))?;
        Ok((port.changer(), id as usize))
    }

    /// Put the disc at `path` in the drive at `at`; returns the path loaded.
    pub fn load_disc(&self, t: &Topology, at: &Location, path: String) -> Result<String, String> {
        let (changer, id) = Self::drive(t, at)?;
        changer.load_disc(id, path)
    }

    /// Eject the drive's disc and load the next in its list; returns its path.
    pub fn eject_disc(&self, t: &Topology, at: &Location) -> Result<String, String> {
        let (changer, id) = Self::drive(t, at)?;
        changer.eject_disc(id)
    }

    /// Eject the drive's disc and leave it empty.
    pub fn eject_to_empty(&self, t: &Topology, at: &Location) -> Result<(), String> {
        let (changer, id) = Self::drive(t, at)?;
        changer.eject_to_empty(id)
    }

    /// The first CD-ROM in location order, with the changer of the controller
    /// it sits on: its drives take disc changes.
    pub fn disc_changer(&self, t: &Topology) -> Option<(Location, Arc<dyn DiscChanger>)> {
        let first = self.records().into_iter().find(|r| r.cdrom)?;
        let port = t.handle::<ScsiPort>(&first.at.parent()?)?;
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

    /// A map with one controller row per entry of `ctls`, each bound to a port.
    fn topo(ctls: Vec<Arc<Ctl>>) -> Topology {
        let t = Topology::new();
        for (n, ctl) in ctls.into_iter().enumerate() {
            let at: Location = format!("hpc3.{n}").parse().unwrap();
            t.add(at.clone(), crate::topology::Row::fixed("").ctl(n as u8)).unwrap();
            t.bind(&at, Arc::new(ScsiPort::new(ctl))).unwrap();
        }
        t
    }

    #[test]
    fn flush_reaches_every_bound_port() {
        let (a, b, c) = (counter(), counter(), counter());
        let t = topo(vec![ctl(&[(1, a.clone(), false), (2, b.clone(), false)]), ctl(&[(1, c.clone(), false)])]);
        Disks::new().flush(&t).unwrap();
        assert_eq!([&a, &b, &c].map(|x| x.load(Ordering::SeqCst)), [1, 1, 1]);
    }

    #[test]
    fn pending_chd_sync_count_sums() {
        let t = topo(vec![ctl(&[(1, counter(), true), (2, counter(), false)]), ctl(&[(1, counter(), true), (3, counter(), true)])]);
        assert_eq!(Disks::new().pending_chd_sync_count(&t), 3);
    }

    #[test]
    fn sync_chd_with_nothing_pending_returns_zero() {
        let t = topo(vec![ctl(&[(1, counter(), false)])]);
        assert_eq!(Disks::new().sync_chd(&t, None, &mut |_, _, _| {}, &|| false).unwrap(), 0);
    }

    /// One location limits the sync to the controller and target it names.
    #[test]
    fn sync_chd_only_one_location() {
        let t = topo(vec![ctl(&[(1, counter(), true)]), ctl(&[(1, counter(), true), (3, counter(), true)])]);
        let at = |s: &str| s.parse::<Location>().unwrap();
        let none = Disks::new().sync_chd(&t, Some(&at("hpc3.1.t2")), &mut |_, _, _| {}, &|| false).unwrap();
        assert_eq!(none, 0, "nothing at that target");
        assert_eq!(Disks::new().pending_chd_sync_count(&t), 3);
    }

    /// A CD-ROM on the second controller, with none on the first, is found.
    #[test]
    fn disc_changer_is_the_first_cdrom_in_location_order() {
        let iso = image("changer-iso", 2048);
        let (first, second) = (ctl(&[]), ctl(&[]));
        let t = topo(vec![first.clone(), second.clone()]);
        let disks = Disks::new();
        assert!(disks.disc_changer(&t).is_none(), "no CD-ROM yet");
        let port = t.handle::<ScsiPort>(&"hpc3.1".parse().unwrap()).unwrap();
        disks.attach_media(&port, &"hpc3.1.t4".parse().unwrap(), &MediaSpec { cdrom: true, ..disk(&iso) }).unwrap();
        let (at, changer) = disks.disc_changer(&t).expect("a changer");
        assert_eq!(at.to_string(), "hpc3.1.t4");
        assert_eq!(Arc::as_ptr(&changer) as *const (), Arc::as_ptr(&second) as *const ());
        std::fs::remove_file(iso).unwrap();
    }

    #[test]
    fn an_empty_topology_has_no_changer_and_a_clean_flush() {
        let (disks, t) = (Disks::new(), Topology::new());
        assert!(disks.disc_changer(&t).is_none());
        assert!(disks.flush(&t).is_ok());
        assert_eq!(disks.pending_chd_sync_count(&t), 0);
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

    /// The status lists each CD-ROM by location, and a disc loads, ejects and
    /// empties by the same name.
    #[test]
    fn disc_status_lists_cdroms_with_their_locations() {
        let (a, b) = (image("status-a", 2048), image("status-b", 2048));
        let t = topo(vec![ctl(&[]), ctl(&[])]);
        let disks = Disks::new();
        let at = |s: &str| s.parse::<Location>().unwrap();
        let second = t.handle::<ScsiPort>(&at("hpc3.1")).unwrap();
        let spec = MediaSpec { cdrom: true, discs: vec![b.clone()], ..disk(&a) };
        disks.attach_media(&second, &at("hpc3.1.t4"), &spec).unwrap();
        let status = disks.disc_status(&t);
        assert_eq!(status.len(), 1);
        assert_eq!((status[0].0.to_string(), status[0].1.as_str()), ("hpc3.1.t4".to_string(), a.as_str()));
        assert_eq!(disks.eject_disc(&t, &at("hpc3.1.t4")).unwrap(), b);
        disks.eject_to_empty(&t, &at("hpc3.1.t4")).unwrap();
        assert_eq!(disks.disc_status(&t)[0].1, "");
        assert_eq!(disks.load_disc(&t, &at("hpc3.1.t4"), a.clone()).unwrap(), a);
        assert!(disks.load_disc(&t, &at("hpc3.0.t4"), a.clone()).is_err(), "no drive there");
        assert!(disks.load_disc(&t, &at("hpc3.1"), a.clone()).is_err(), "not a target");
        for f in [a, b] {
            std::fs::remove_file(f).unwrap();
        }
    }
}
