//! Placing devices: the rows decide where a device may go, and a device type
//! says what it is.
//!
//! A [`DeviceType`] names itself, says which family of rows it goes on, parses
//! its own options, declares the rows it brings below itself and builds itself
//! with the runtime object its parent row owns (a SCSI controller, a PCI bus).
//! The types are listed once, in [`registry::MAKERS`]. [`attach_device`] is the
//! only claim-and-build path: the machines, `validate` (a probe that builds
//! nothing), the GUI and the tests all go through it and so give the same
//! errors.
//!
//! This runs at config, attach, reset and save time. Nothing here is on a CPU,
//! memory or device access path.

pub mod registry;

use std::any::Any;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config::MachineConfig;
use crate::devices::Devices;
use crate::disks::Disks;
use crate::location::{Hop, Location};
use crate::topology::{Family, Topology};

pub use registry::MAKERS;

/// One device to place: a type name, where, and its options.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub device: String,
    pub at: Location,
    pub opts: toml::Table,
}

/// What a device type's constructor may reach.
pub struct Ctx<'a> {
    pub devices: &'a mut Devices,
    pub disks: &'a Disks,
}

/// The runtime objects a device brings for the rows it made, by hop; `None`
/// is the device's own location.
pub type Handles = Vec<(Option<Hop>, Arc<dyn Any + Send + Sync>)>;
pub type BuildFn = fn(&Location, &Arc<dyn Any + Send + Sync>, &mut Ctx, &toml::Table) -> Result<Handles, String>;

pub trait DeviceType: Sized + 'static {
    const NAME: &'static str;
    const FAMILY: Family;
    const NEEDS_NETWORK: bool = false;
    /// What the owning row's handle holds: a SCSI port, a PCI bus.
    type Owner: Send + Sync + 'static;
    type Options: DeserializeOwned + Serialize + Default;

    /// Value checks that need more than the options themselves.
    fn check(_: &Self::Options, _: &MachineConfig) -> Result<(), String> {
        Ok(())
    }
    /// Add the rows this device brings below `at`: a card's bus and slots, a
    /// controller's targets. Runs in the probe and in the build, right after
    /// the claim and before anything deeper is placed, so `validate`,
    /// `--list-slots` and the GUI see the same places the machine gets.
    fn rows(_: &Self::Options, _: &Location, _: &Topology) -> Result<(), String> {
        Ok(())
    }
    /// Create the device. A passive disk returns nothing; a device with
    /// threads or state of its own registers itself in `ctx.devices`.
    fn new(o: Self::Options, ctx: &mut Ctx, owner: &Arc<Self::Owner>, at: &Location) -> Result<Handles, String>;

    fn parse_options(t: &toml::Table) -> Result<Self::Options, String> {
        // serde's unknown-field error already lists the accepted keys.
        toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| {
            format!("{} for {}", e.to_string().lines().collect::<Vec<_>>().join(" "), Self::NAME)
        })
    }
    /// The probe: parse and check the options, build nothing.
    fn parse_dyn(t: &toml::Table, cfg: &MachineConfig) -> Result<(), String> {
        Self::check(&Self::parse_options(t)?, cfg)
    }
    fn rows_dyn(t: &toml::Table, at: &Location, topo: &Topology) -> Result<(), String> {
        Self::rows(&Self::parse_options(t)?, at, topo)
    }
    fn build(at: &Location, owner: &Arc<dyn Any + Send + Sync>, ctx: &mut Ctx, t: &toml::Table) -> Result<Handles, String> {
        let owner = owner
            .clone()
            .downcast::<Self::Owner>()
            .map_err(|_| format!("{at}: the owner of this row is not what {} goes on", Self::NAME))?;
        Self::new(Self::parse_options(t)?, ctx, &owner, at)
    }
}

/// A device type as the list holds it.
pub struct Maker {
    pub name: &'static str,
    pub family: Family,
    pub needs_network: bool,
    pub parse: fn(&toml::Table, &MachineConfig) -> Result<(), String>,
    pub rows: fn(&toml::Table, &Location, &Topology) -> Result<(), String>,
    pub build: BuildFn,
}

impl Maker {
    /// The list entry for `T`.
    pub const fn of<T: DeviceType>() -> Maker {
        Maker {
            name: T::NAME,
            family: T::FAMILY,
            needs_network: T::NEEDS_NETWORK,
            parse: T::parse_dyn,
            rows: T::rows_dyn,
            build: T::build,
        }
    }
}

/// The device type of this name that goes on rows of `family`.
pub fn maker_for(name: &str, family: Family) -> Option<&'static Maker> {
    MAKERS.iter().find(|m| m.name == name && m.family == family)
}

/// Every device type name, sorted, once each.
pub fn device_names() -> Vec<&'static str> {
    names(MAKERS)
}

fn names(makers: &[Maker]) -> Vec<&'static str> {
    let mut names: Vec<&str> = makers.iter().map(|m| m.name).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Place one device: the row must take it, its options must parse, its rows
/// are added, and with a `ctx` it is built and its handles are bound. Without
/// a `ctx` this is the probe and nothing is built.
pub fn attach_device(p: &Placement, cfg: &MachineConfig, ctx: Option<&mut Ctx>, topo: &Topology) -> Result<(), String> {
    attach_with(MAKERS, p, cfg, ctx, topo)
}

fn attach_with(makers: &[Maker], p: &Placement, cfg: &MachineConfig, ctx: Option<&mut Ctx>, topo: &Topology) -> Result<(), String> {
    if !makers.iter().any(|m| m.name == p.device) {
        return Err(format!("error: unknown device [[{}]]; devices: {}", p.device, names(makers).join(" ")));
    }
    let at = &p.at;
    let family = topo.offers(at)?;
    let Some(m) = makers.iter().find(|m| m.name == p.device && m.family == family) else {
        // The claim words the family mismatch; the row's own list of what it takes follows.
        let other = makers.iter().find(|m| m.name == p.device).expect("known above");
        let e = topo.claim(at, &p.device, other.family, false, "").expect_err("the row takes another family");
        let mut takes: Vec<&str> = makers.iter().filter(|m| m.family == family).map(|m| m.name).collect();
        takes.sort_unstable();
        return Err(format!("{e} (takes: {})", takes.join(" ")));
    };
    // Parse before claiming, so a bad entry leaves its row free.
    (m.parse)(&p.opts, cfg).map_err(|e| format!("error: {at}: {e}"))?;
    let note = p.opts.get("path").and_then(toml::Value::as_str).unwrap_or("");
    topo.claim(at, m.name, m.family, m.needs_network, note)?;
    (m.rows)(&p.opts, at, topo).map_err(|e| format!("error: {at}: {e}"))?;
    let Some(ctx) = ctx else { return Ok(()) };
    let owner = topo.owner(at)?;
    for (hop, handle) in (m.build)(at, &owner, ctx, &p.opts).map_err(|e| format!("error: {at}: {e}"))? {
        topo.bind(&at.join_opt(hop), handle)?;
    }
    Ok(())
}

/// Place every entry of `placements`. A card comes before what sits on it
/// (depth); within one depth the list's own order holds.
pub fn attach_devices(placements: &[Placement], cfg: &MachineConfig, ctx: Option<&mut Ctx>, topo: &Topology) -> Result<(), String> {
    attach_all(MAKERS, placements, cfg, ctx, topo)
}

fn attach_all(makers: &[Maker], placements: &[Placement], cfg: &MachineConfig, mut ctx: Option<&mut Ctx>, topo: &Topology) -> Result<(), String> {
    let mut all = placements.to_vec();
    all.sort_by_key(|p| p.at.depth());
    for p in &all {
        attach_with(makers, p, cfg, ctx.as_deref_mut(), topo)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::Row;

    fn loc(s: &str) -> Location {
        s.parse().unwrap()
    }

    #[derive(Default, serde::Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct WidgetOptions {
        #[serde(default)]
        size: u32,
        label: String,
    }

    /// Labels and owners, in the order the devices were built.
    static ORDER: parking_lot::Mutex<Vec<(String, u32)>> = parking_lot::Mutex::new(Vec::new());

    /// A test-only SCSI device that records its builds.
    struct Widget;
    impl DeviceType for Widget {
        const NAME: &'static str = "test_widget";
        const FAMILY: Family = Family::Scsi;
        type Owner = u32;
        type Options = WidgetOptions;
        fn new(o: WidgetOptions, _: &mut Ctx, owner: &Arc<u32>, _: &Location) -> Result<Handles, String> {
            ORDER.lock().push((o.label, **owner));
            Ok(vec![])
        }
    }

    /// The same name on a PCI row; `new` panics so a probe that builds shows.
    struct OtherWidget;
    impl DeviceType for OtherWidget {
        const NAME: &'static str = "test_widget";
        const FAMILY: Family = Family::Pci;
        type Owner = u32;
        type Options = WidgetOptions;
        fn new(_: WidgetOptions, _: &mut Ctx, _: &Arc<u32>, _: &Location) -> Result<Handles, String> {
            panic!("a probe must not build")
        }
    }

    /// A card that brings a controller row and its targets below itself, and
    /// binds a handle there.
    struct Brings;
    impl DeviceType for Brings {
        const NAME: &'static str = "test_brings";
        const FAMILY: Family = Family::Pci;
        type Owner = u32;
        type Options = WidgetOptions;
        fn rows(o: &WidgetOptions, at: &Location, t: &Topology) -> Result<(), String> {
            t.add(at.join(Hop::pos(0)), Row::fixed("").onboard("test bus"))?;
            t.scsi(&at.join(Hop::pos(0)), 1..=o.size.max(1) as u8, false);
            Ok(())
        }
        fn new(o: WidgetOptions, _: &mut Ctx, _: &Arc<u32>, _: &Location) -> Result<Handles, String> {
            ORDER.lock().push((o.label.clone(), 0));
            let hop = if o.size == 9 { Hop::t(7) } else { Hop::pos(0) };
            Ok(vec![(Some(hop), Arc::new(5u32))])
        }
    }

    const TEST_MAKERS: &[Maker] = &[Maker::of::<Widget>(), Maker::of::<OtherWidget>(), Maker::of::<Brings>()];

    fn machine() -> Topology {
        let t = Topology::new();
        t.add(Location::root(), Row::fixed("")).unwrap();
        t.add(loc("hpc3"), Row::fixed("")).unwrap();
        t.add(loc("hpc3.0"), Row::fixed("")).unwrap();
        t.bind(&loc("hpc3.0"), Arc::new(3u32)).unwrap();
        t.scsi(&loc("hpc3.0"), 1..=2, false);
        t.add(loc("pci0"), Row::fixed("")).unwrap();
        t.bind(&loc("pci0"), Arc::new(4u32)).unwrap();
        t.add(loc("pci0.3"), Row::offering("option", Family::Pci)).unwrap();
        t
    }

    fn place(device: &str, at: &str, toml_opts: &str) -> Placement {
        Placement { device: device.into(), at: loc(at), opts: toml::from_str(toml_opts).unwrap() }
    }

    fn probe(t: &Topology, p: &Placement) -> Result<(), String> {
        attach_with(TEST_MAKERS, p, &MachineConfig::default(), None, t)
    }

    fn build(t: &Topology, p: &Placement) -> Result<(), String> {
        let (mut d, k) = (Devices::new(), Disks::new(Arc::new(Topology::new())));
        attach_with(TEST_MAKERS, p, &MachineConfig::default(), Some(&mut Ctx { devices: &mut d, disks: &k }), t)
    }

    #[test]
    fn the_registry_lists_hdd_cdrom_and_daynaport_on_scsi_rows() {
        for name in ["hdd", "cdrom", "daynaport"] {
            assert_eq!(maker_for(name, Family::Scsi).map(|m| m.name), Some(name));
        }
        assert!(device_names().windows(2).all(|w| w[0] < w[1]), "sorted and unique");
    }

    #[test]
    fn wrong_family_error_names_both_and_lists_takes() {
        let t = machine();
        let e = probe(&t, &place("test_brings", "hpc3.0.t1", "label = \"x\"")).unwrap_err();
        assert_eq!(e, "error: hpc3.0.t1: test_brings is a PCI card; hpc3.0.t1 is a SCSI target (takes: test_widget)");
    }

    #[test]
    fn unknown_device_lists_the_known_ones() {
        let t = machine();
        let e = probe(&t, &place("test_widgt", "hpc3.0.t1", "")).unwrap_err();
        assert_eq!(e, "error: unknown device [[test_widgt]]; devices: test_brings test_widget");
    }

    #[test]
    fn probe_claims_and_parses_but_builds_nothing() {
        let t = machine();
        probe(&t, &place("test_widget", "pci0.3", "label = \"x\"")).unwrap();
        let e = probe(&t, &place("test_widget", "pci0.3", "label = \"x\"")).unwrap_err();
        assert_eq!(e, "error: pci0.3: taken by test_widget");
        let e = probe(&t, &place("test_widget", "hpc3.0.t1", "speed = 1\nlabel = \"x\"")).unwrap_err();
        assert!(e.starts_with("error: hpc3.0.t1: unknown field `speed`, expected `size` or `label` for test_widget"), "{e}");
        assert!(t.claim(&loc("hpc3.0.t1"), "x", Family::Scsi, false, "").is_ok(), "a bad entry leaves its row free");
    }

    /// The rows a card brings exist after the probe, so a disk below the card
    /// validates without a machine.
    #[test]
    fn a_cards_rows_come_from_the_probe_so_what_sits_on_it_validates() {
        let t = machine();
        let e = probe(&t, &place("test_widget", "pci0.3.0.t2", "label = \"x\"")).unwrap_err();
        assert!(e.contains("nothing is placed at pci0.3"), "{e}");
        let all = [place("test_widget", "pci0.3.0.t2", "label = \"d\""), place("test_brings", "pci0.3", "label = \"c\"\nsize = 3")];
        attach_all(TEST_MAKERS, &all, &MachineConfig::default(), None, &t).unwrap();
        assert_eq!(t.lines().iter().filter(|l| l.starts_with("pci0.3.0.t")).count(), 3, "{:?}", t.lines());
        assert!(probe(&t, &place("test_widget", "pci0.3.0.t4", "label = \"x\"")).unwrap_err().contains("pci0.3.0 has no t4"));
    }

    #[test]
    fn build_binds_returned_handles_at_rows_the_card_made() {
        let t = machine();
        build(&t, &place("test_brings", "pci0.3", "label = \"x\"")).unwrap();
        assert_eq!(t.handle::<u32>(&loc("pci0.3.0")).map(|a| *a), Some(5));
        // What sits on the card is built with the card's handle.
        build(&t, &place("test_widget", "pci0.3.0.t1", "label = \"on-card\"")).unwrap();
        assert!(ORDER.lock().contains(&("on-card".to_string(), 5)));
    }

    #[test]
    fn a_handle_for_a_hop_rows_did_not_make_is_a_bind_error() {
        let t = machine();
        let e = build(&t, &place("test_brings", "pci0.3", "label = \"x\"\nsize = 9")).unwrap_err();
        assert!(e.contains("pci0.3.t7") && e.contains("no such location"), "{e}");
    }

    #[test]
    fn attach_devices_orders_by_depth_then_list_order() {
        let t = machine();
        let all = [
            place("test_widget", "pci0.3.0.t1", "label = \"ord-c\""),
            place("test_widget", "hpc3.0.t2", "label = \"ord-a\""),
            place("test_brings", "pci0.3", "label = \"ord-0\""),
            place("test_widget", "hpc3.0.t1", "label = \"ord-b\""),
        ];
        let (mut d, k) = (Devices::new(), Disks::new(Arc::new(Topology::new())));
        attach_all(TEST_MAKERS, &all, &MachineConfig::default(), Some(&mut Ctx { devices: &mut d, disks: &k }), &t).unwrap();
        let got: Vec<String> = ORDER.lock().iter().map(|l| l.0.clone()).filter(|l| l.starts_with("ord-")).collect();
        assert_eq!(got, ["ord-0", "ord-a", "ord-b", "ord-c"]);
    }
}
