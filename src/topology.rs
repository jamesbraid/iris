//! The socket map: every place a device can go on one machine, as rows keyed
//! by [`Location`]. A machine's `fill` adds the rows its profile has and
//! touches no hardware, so a probe (`validate`, `--list-slots`, the GUI) runs
//! `fill` alone. The constructor runs the same `fill`, builds the hardware and
//! binds each runtime object it made to its row. Placing a device is one
//! check, [`Topology::claim`], so config, the CLI, the GUI and tests give the
//! same error.
//!
//! Nothing here is on a CPU, memory or device access path: the map is locked
//! while attaching, saving and listing, never per access.

use std::any::Any;
use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::config::MachineConfig;
use crate::location::{Hop, Location};

/// The class of rows a device type goes into.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Scsi,
    Pci,
    Gio,
}

impl Family {
    /// What a device of this family is called ("cdrom is a SCSI device").
    pub fn device(self) -> &'static str {
        match self {
            Family::Scsi => "SCSI device",
            Family::Pci => "PCI card",
            Family::Gio => "GIO card",
        }
    }

    /// What a row that offers this family is called.
    pub fn place(self) -> &'static str {
        match self {
            Family::Scsi => "SCSI target",
            Family::Pci => "PCI card position",
            Family::Gio => "GIO slot",
        }
    }
}

/// One entry of the socket map.
#[derive(Clone)]
pub struct Row {
    /// The manual's name for the place ("PCI option slot"); empty below a controller.
    pub label: &'static str,
    /// What a user may place here; `None` is a fixed row.
    pub offers: Option<Family>,
    /// A network device (DaynaPort) goes only where this is set.
    pub network: bool,
    /// "Controller N of this profile", for migration and the legacy flags.
    pub ctl: Option<u8>,
    /// What the machine builds here ("aic7880", "graphics (head 2)"). A SCSI
    /// controller row is fixed: users place devices on its target rows.
    pub onboard: Option<&'static str>,
    /// The number or label is not established (inferred or unknown).
    pub approx: bool,
    taken: Option<String>,
    below: Option<Arc<dyn Any + Send + Sync>>,
}

impl Row {
    fn new(label: &'static str, offers: Option<Family>) -> Row {
        Row { label, offers, network: false, ctl: None, onboard: None, approx: false, taken: None, below: None }
    }
    pub fn fixed(label: &'static str) -> Row {
        Row::new(label, None)
    }
    pub fn offering(label: &'static str, family: Family) -> Row {
        Row::new(label, Some(family))
    }
    pub fn network(mut self) -> Row {
        self.network = true;
        self
    }
    pub fn ctl(mut self, n: u8) -> Row {
        self.ctl = Some(n);
        self
    }
    pub fn onboard(mut self, what: &'static str) -> Row {
        self.onboard = Some(what);
        self
    }
    pub fn approx(mut self) -> Row {
        self.approx = true;
        self
    }
    /// What is placed here ("hdd o2.img"), if anything.
    pub fn taken(&self) -> Option<&str> {
        self.taken.as_deref()
    }
}

#[derive(Default)]
pub struct Topology {
    rows: Mutex<BTreeMap<Location, Row>>,
}

impl Topology {
    pub fn new() -> Topology {
        Topology::default()
    }

    /// The rows the configured machine has, with nothing placed and no
    /// hardware built: what migration and the command line use to find
    /// "controller 0".
    pub fn fill_for(cfg: &MachineConfig) -> Topology {
        let t = Topology::new();
        cfg.machine.profile.fill(cfg, &t);
        t
    }

    pub fn add(&self, at: Location, row: Row) -> Result<(), String> {
        match self.rows.lock().entry(at) {
            std::collections::btree_map::Entry::Occupied(e) => Err(format!("error: {}: row added twice", e.key())),
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(row);
                Ok(())
            }
        }
    }

    /// Give a row the runtime object that owns the rows under it (a bus, a
    /// controller). A bind to a row `fill` never made is an error, so a
    /// constructor cannot drift from `fill`.
    pub fn bind(&self, at: &Location, handle: Arc<dyn Any + Send + Sync>) -> Result<(), String> {
        let mut rows = self.rows.lock();
        let row = rows.get_mut(at).ok_or_else(|| format!("error: {at}: no such location to bind a handle to"))?;
        if row.below.is_some() {
            return Err(format!("error: {at}: a handle is already bound"));
        }
        row.below = Some(handle);
        Ok(())
    }

    /// The handle bound at `at`, when it is a `T`.
    pub fn handle<T: Any + Send + Sync>(&self, at: &Location) -> Option<Arc<T>> {
        self.rows.lock().get(at)?.below.clone()?.downcast().ok()
    }

    /// What the row above `at` owns: an `Arc` clone, so nothing stays locked.
    pub fn owner(&self, at: &Location) -> Result<Arc<dyn Any + Send + Sync>, String> {
        let parent = at.parent().ok_or_else(|| format!("error: {at}: the machine root has no owner"))?;
        self.rows
            .lock()
            .get(&parent)
            .and_then(|r| r.below.clone())
            .ok_or_else(|| format!("error: {at}: nothing is bound at {parent}"))
    }

    /// Every handle of type `T`, in location order.
    pub fn buses<T: Any + Send + Sync>(&self) -> Vec<(Location, Arc<T>)> {
        let rows = self.rows.lock();
        rows.iter()
            .filter_map(|(l, r)| Some((l.clone(), r.below.clone()?.downcast::<T>().ok()?)))
            .collect()
    }

    /// The row that is controller `n` of this profile.
    pub fn controller(&self, n: u8) -> Option<Location> {
        self.rows.lock().iter().find(|(_, r)| r.ctl == Some(n)).map(|(l, _)| l.clone())
    }

    /// `fill_for`, then every placement of `cfg` claimed and its options
    /// parsed. Nothing is built: this is what `validate`, `--list-slots` and
    /// the GUI run, so config errors appear without a machine.
    pub fn probe(cfg: &MachineConfig) -> Result<Topology, String> {
        let t = Topology::fill_for(cfg);
        crate::attach::attach_devices(&cfg.placements()?, cfg, None, &t)?;
        Ok(t)
    }

    /// The controller number of the row above `at`, for the snapshot's
    /// controller * 16 + id disk numbers.
    pub fn ctl_of(&self, at: &Location) -> Option<u8> {
        self.rows.lock().get(&at.parent()?)?.ctl
    }

    /// Add target rows `t<id>` below `at`, each offering SCSI.
    pub fn scsi(&self, at: &Location, ids: RangeInclusive<u8>, network: bool) {
        for id in ids {
            let mut row = Row::offering("", Family::Scsi);
            row.network = network;
            self.add(at.join(Hop::t(id)), row).expect("target rows are added once");
        }
    }

    /// The family `at` takes: the row exists and is not fixed.
    pub fn offers(&self, at: &Location) -> Result<Family, String> {
        let rows = self.rows.lock();
        let row = rows.get(at).ok_or_else(|| Self::missing(&rows, at))?;
        row.offers.ok_or_else(|| Self::fixed_error(&rows, at, row))
    }

    /// Place a device: the row must exist, offer `family`, suit a network
    /// device when `needs_network` and be free. `note` says what is placed.
    pub fn claim(&self, at: &Location, name: &str, family: Family, needs_network: bool, note: &str) -> Result<(), String> {
        let mut rows = self.rows.lock();
        let Some(row) = rows.get(at) else {
            return Err(Self::missing(&rows, at));
        };
        match row.offers {
            None => return Err(Self::fixed_error(&rows, at, row)),
            Some(f) if f != family => {
                return Err(format!(
                    "error: {at}: {name} is a {}; {at} is a {}",
                    family.device(),
                    f.place()
                ));
            }
            Some(_) => {}
        }
        if needs_network && !row.network {
            return Err(format!("error: {at}: {name} is not supported on this SCSI bus"));
        }
        if let Some(by) = &row.taken {
            return Err(format!("error: {at}: taken by {by}"));
        }
        let row = rows.get_mut(at).expect("checked above");
        row.taken = Some(if note.is_empty() { name.to_owned() } else { format!("{name} {note}") });
        Ok(())
    }

    fn missing(rows: &BTreeMap<Location, Row>, at: &Location) -> String {
        let Some(p) = at.parent() else { return format!("error: {at}: no such location") };
        if !rows.contains_key(&p) {
            return format!("error: {at}: no such location ({p} does not exist)");
        }
        if rows.keys().any(|l| l.parent().as_ref() == Some(&p)) {
            format!("error: {at}: no such location ({p} has no {})", at.last().expect("has a parent"))
        } else {
            format!("error: {at}: no such location (nothing is placed at {p}; place a card there first)")
        }
    }

    fn fixed_error(rows: &BTreeMap<Location, Row>, at: &Location, row: &Row) -> String {
        let mut msg = format!("error: {at}: not a place for add-in devices");
        if let Some(what) = row.onboard {
            msg += &format!(" (onboard {what})");
        }
        if let Some(parent) = at.parent() {
            let takes: Vec<String> = rows
                .iter()
                .filter(|(l, r)| r.offers.is_some() && l.parent().as_ref() == Some(&parent))
                .map(|(l, r)| match (l.last(), r.label) {
                    (Some(h), "") => h.to_string(),
                    (Some(h), label) => format!("{h} (\"{label}\")"),
                    (None, _) => String::new(),
                })
                .collect();
            if !takes.is_empty() {
                msg += &format!("; {parent} takes cards at: {}", takes.join(", "));
            }
        }
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(s: &str) -> Location {
        s.parse().unwrap()
    }

    /// An O2-shaped map: a bus, an onboard controller, an external one and a
    /// free option slot.
    fn o2() -> Topology {
        let t = Topology::new();
        t.add(Location::root(), Row::fixed("")).unwrap();
        t.add(loc("pci0"), Row::fixed("PCI bus")).unwrap();
        t.add(loc("pci0.1"), Row::fixed("SCSI internal").ctl(0).onboard("aic7880")).unwrap();
        t.add(loc("pci0.4"), Row::offering("PCI option slot", Family::Pci)).unwrap();
        t.scsi(&loc("pci0.1"), 1..=7, false);
        t
    }

    #[test]
    fn claim_marks_the_row_taken_and_refuses_a_second() {
        let t = o2();
        t.claim(&loc("pci0.1.t4"), "cdrom", Family::Scsi, false, "a.iso").unwrap();
        let e = t.claim(&loc("pci0.1.t4"), "hdd", Family::Scsi, false, "").unwrap_err();
        assert_eq!(e, "error: pci0.1.t4: taken by cdrom a.iso");
    }

    #[test]
    fn a_fixed_row_refuses_a_claim_naming_what_is_onboard() {
        let t = o2();
        t.add(loc("pci0.2"), Row::fixed("").onboard("isp1040")).unwrap();
        let e = t.claim(&loc("pci0.2"), "aic7880", Family::Pci, false, "").unwrap_err();
        assert_eq!(
            e,
            "error: pci0.2: not a place for add-in devices (onboard isp1040); pci0 takes cards at: 4 (\"PCI option slot\")"
        );
    }

    #[test]
    fn the_wrong_family_names_both() {
        let t = o2();
        let e = t.claim(&loc("pci0.4"), "cdrom", Family::Scsi, false, "").unwrap_err();
        assert_eq!(e, "error: pci0.4: cdrom is a SCSI device; pci0.4 is a PCI card position");
    }

    #[test]
    fn a_missing_row_names_its_nearest_parent() {
        let t = o2();
        let e = t.claim(&loc("pci0.4.t1"), "hdd", Family::Scsi, false, "").unwrap_err();
        assert!(e.contains("nothing is placed at pci0.4"), "{e}");
        let e = t.claim(&loc("pci0.1.t9"), "hdd", Family::Scsi, false, "").unwrap_err();
        assert_eq!(e, "error: pci0.1.t9: no such location (pci0.1 has no t9)");
    }

    #[test]
    fn network_rows_only_take_network_devices() {
        let t = Topology::new();
        t.add(loc("hpc3"), Row::fixed("")).unwrap();
        t.add(loc("hpc3.0"), Row::fixed("")).unwrap();
        t.scsi(&loc("hpc3.0"), 1..=2, true);
        t.add(loc("pci0"), Row::fixed("")).unwrap();
        t.scsi(&loc("pci0"), 1..=2, false);
        t.claim(&loc("hpc3.0.t1"), "daynaport", Family::Scsi, true, "").unwrap();
        let e = t.claim(&loc("pci0.t1"), "daynaport", Family::Scsi, true, "").unwrap_err();
        assert!(e.contains("not supported on this SCSI bus"), "{e}");
        t.claim(&loc("pci0.t1"), "hdd", Family::Scsi, false, "").unwrap();
    }

    #[test]
    fn bind_to_a_row_that_was_never_added_is_an_error() {
        let t = o2();
        assert!(t.bind(&loc("pci0.9"), Arc::new(1u8)).is_err());
        t.bind(&loc("pci0.1"), Arc::new(1u8)).unwrap();
        assert!(t.bind(&loc("pci0.1"), Arc::new(2u8)).unwrap_err().contains("already bound"));
    }

    #[test]
    fn owner_is_the_parent_rows_handle_and_survives_the_lock() {
        let t = Arc::new(o2());
        t.bind(&loc("pci0.1"), Arc::new(7u32)).unwrap();
        let owner = t.owner(&loc("pci0.1.t1")).unwrap();
        let t2 = t.clone();
        std::thread::spawn(move || t2.add(loc("pci0.9"), Row::fixed("")).unwrap()).join().unwrap();
        assert_eq!(*owner.downcast::<u32>().unwrap(), 7);
        assert!(t.owner(&loc("pci0.4.t1")).is_err());
        assert_eq!(t.handle::<u32>(&loc("pci0.1")).map(|a| *a), Some(7));
        assert!(t.handle::<u8>(&loc("pci0.1")).is_none());
    }

    #[test]
    fn buses_come_back_in_location_order_with_pci0_2_before_pci0_10() {
        let t = Topology::new();
        for s in ["pci0.10", "pci0.2", "pci0.1"] {
            t.add(loc(s), Row::fixed("")).unwrap();
            t.bind(&loc(s), Arc::new(s.to_string())).unwrap();
        }
        let got: Vec<String> = t.buses::<String>().into_iter().map(|(l, _)| l.to_string()).collect();
        assert_eq!(got, ["pci0.1", "pci0.2", "pci0.10"]);
    }

    #[test]
    fn scsi_helper_adds_exactly_the_requested_targets() {
        let t = Topology::new();
        t.add(loc("hpc3.0"), Row::fixed("").ctl(0)).unwrap();
        t.scsi(&loc("hpc3.0"), 1..=7, true);
        assert!(t.claim(&loc("hpc3.0.t7"), "hdd", Family::Scsi, false, "").is_ok());
        assert!(t.claim(&loc("hpc3.0.t8"), "hdd", Family::Scsi, false, "").is_err());
        assert!(t.claim(&loc("hpc3.0.t0"), "hdd", Family::Scsi, false, "").is_err());
        assert_eq!(t.controller(0), Some(loc("hpc3.0")));
        assert_eq!(t.controller(1), None);
    }
}
