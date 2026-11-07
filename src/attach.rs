//! Placing devices: the rows decide where a device may go, and a device type
//! registers itself.
//!
//! A [`DeviceType`] names itself, says which family of rows it goes on, parses
//! its own options and builds itself with the runtime object its parent row
//! owns (a SCSI controller, the GIO bus). `register_device!` puts it in the
//! one link-time list, [`MAKERS`], so no central table names the types.
//! [`attach_device`] is the only claim-and-build path: the machines, `validate`
//! (a probe that builds nothing), the GUI and the tests all go through it and
//! so give the same errors.
//!
//! This runs at config, attach, reset and save time. Nothing here is on a CPU,
//! memory or device access path.

use std::any::Any;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config::{MachineConfig, Placement};
use crate::devices::Devices;
use crate::disks::Disks;
use crate::location::{Hop, Location};
use crate::topology::{Family, Topology};

/// What a device type's constructor may reach.
pub struct Ctx<'a> {
    pub devices: &'a Devices,
    pub disks: &'a Disks,
}

/// The runtime objects a device brings for the rows it made, by hop; `None`
/// is the device's own location.
pub type Handles = Vec<(Option<Hop>, Arc<dyn Any + Send + Sync>)>;
pub type BuildFn = fn(&Location, &Arc<dyn Any + Send + Sync>, &Ctx, &toml::Table) -> Result<Handles, String>;

pub trait DeviceType: Sized + 'static {
    const NAME: &'static str;
    const FAMILY: Family;
    const NEEDS_NETWORK: bool = false;
    /// What the owning row's handle holds: a SCSI port, the GIO bus.
    type Owner: Send + Sync + 'static;
    type Options: DeserializeOwned + Serialize + Default;

    /// Value checks that need more than the options themselves.
    fn check(_: &Self::Options, _: &MachineConfig) -> Result<(), String> {
        Ok(())
    }
    /// Create the device. A passive disk returns nothing; a device with
    /// threads or state of its own registers itself in `ctx.devices`.
    fn new(o: Self::Options, ctx: &Ctx, owner: &Arc<Self::Owner>, at: &Location) -> Result<Handles, String>;

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
    fn build(at: &Location, owner: &Arc<dyn Any + Send + Sync>, ctx: &Ctx, t: &toml::Table) -> Result<Handles, String> {
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
    pub build: BuildFn,
}

#[linkme::distributed_slice]
pub static MAKERS: [Maker];

/// Register a device type: one line next to the device. A name may register
/// once per family (`virtio_serial` goes on a GIO slot and on a PCI one).
#[macro_export]
macro_rules! register_device {
    ($ty:ty) => {
        const _: () = {
            #[linkme::distributed_slice($crate::attach::MAKERS)]
            static MAKER: $crate::attach::Maker = $crate::attach::Maker {
                name: <$ty as $crate::attach::DeviceType>::NAME,
                family: <$ty as $crate::attach::DeviceType>::FAMILY,
                needs_network: <$ty as $crate::attach::DeviceType>::NEEDS_NETWORK,
                parse: <$ty as $crate::attach::DeviceType>::parse_dyn,
                build: <$ty as $crate::attach::DeviceType>::build,
            };
        };
    };
}

/// The device type of this name that goes on rows of `family`.
pub fn maker_for(name: &str, family: Family) -> Option<&'static Maker> {
    MAKERS.iter().find(|m| m.name == name && m.family == family)
}

/// Every registered device name, sorted, once each.
pub fn device_names() -> Vec<&'static str> {
    let mut names: Vec<&str> = MAKERS.iter().map(|m| m.name).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Which of `expected` no device type registered. Every binary that links the
/// library asserts this is empty: a linker that dropped the entries would
/// leave a binary that parses no `[[hdd]]`.
pub fn missing_makers(expected: &[&str]) -> Vec<String> {
    expected.iter().filter(|n| !MAKERS.iter().any(|m| m.name == **n)).map(|n| n.to_string()).collect()
}

/// Check the key of a `[[<device>]]` array names a registered device type.
pub fn known_device(name: &str) -> Result<(), String> {
    if MAKERS.iter().any(|m| m.name == name) {
        return Ok(());
    }
    Err(format!("error: unknown device [[{name}]]; devices: {}", device_names().join(" ")))
}

/// Place one device: the row must take it, its options must parse, and with a
/// `ctx` it is built and its handles are bound. Without a `ctx` this is the
/// probe and nothing is built.
pub fn attach_device(p: &Placement, cfg: &MachineConfig, ctx: Option<&Ctx>, topo: &Topology) -> Result<(), String> {
    known_device(&p.device)?;
    let at = &p.at;
    let family = topo.offers(at)?;
    let Some(m) = maker_for(&p.device, family) else {
        let takes: Vec<&str> = MAKERS
            .iter()
            .filter(|m| m.family == family)
            .map(|m| m.name)
            .collect();
        let other = MAKERS.iter().find(|m| m.name == p.device).expect("known above");
        return Err(format!(
            "error: {at}: {} is a {}; {at} is a {} (takes: {})",
            p.device,
            other.family.device(),
            family.place(),
            takes.join(" ")
        ));
    };
    let note = p.opts.get("path").and_then(toml::Value::as_str).unwrap_or("");
    topo.claim(at, m.name, m.family, m.needs_network, note)?;
    (m.parse)(&p.opts, cfg).map_err(|e| format!("error: {at}: {e}"))?;
    let Some(ctx) = ctx else { return Ok(()) };
    let owner = topo.owner(at)?;
    for (hop, handle) in (m.build)(at, &owner, ctx, &p.opts).map_err(|e| format!("error: {at}: {e}"))? {
        topo.bind(&at.join_opt(hop), handle)?;
    }
    Ok(())
}

/// Place every entry of `placements` (the config's `[[<device>]]` arrays). A
/// card comes before what sits on it (depth); the list arrives key-sorted,
/// because toml tables are, so the order across device types is by name, then
/// by position in the array.
pub fn attach_devices(placements: &[Placement], cfg: &MachineConfig, ctx: Option<&Ctx>, topo: &Topology) -> Result<(), String> {
    let mut all = placements.to_vec();
    all.sort_by_key(|p| p.at.depth());
    for p in &all {
        attach_device(p, cfg, ctx, topo)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::Row;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    static BUILT: AtomicUsize = AtomicUsize::new(0);
    /// Labels in the order the devices were built.
    static ORDER: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());

    /// A test-only GIO device that records its builds.
    struct Widget;
    impl DeviceType for Widget {
        const NAME: &'static str = "test_widget";
        const FAMILY: Family = Family::Gio;
        type Owner = u32;
        type Options = WidgetOptions;
        fn new(o: WidgetOptions, _: &Ctx, owner: &Arc<u32>, _: &Location) -> Result<Handles, String> {
            BUILT.fetch_add(**owner as usize, Ordering::SeqCst);
            ORDER.lock().push(o.label);
            Ok(vec![])
        }
    }
    register_device!(Widget);

    /// The same name on a PCI row; `new` panics so a probe that builds shows.
    struct PciWidget;
    impl DeviceType for PciWidget {
        const NAME: &'static str = "test_widget";
        const FAMILY: Family = Family::Pci;
        type Owner = u32;
        type Options = WidgetOptions;
        fn new(_: WidgetOptions, _: &Ctx, _: &Arc<u32>, _: &Location) -> Result<Handles, String> {
            panic!("a probe must not build")
        }
    }
    register_device!(PciWidget);

    /// A card that brings a row below itself.
    struct Brings;
    impl DeviceType for Brings {
        const NAME: &'static str = "test_brings";
        const FAMILY: Family = Family::Pci;
        type Owner = u32;
        type Options = WidgetOptions;
        fn new(o: WidgetOptions, _: &Ctx, _: &Arc<u32>, _: &Location) -> Result<Handles, String> {
            ORDER.lock().push(o.label.clone());
            let hop = if o.size == 1 { Hop::t(7) } else { Hop::t(0) };
            Ok(vec![(Some(hop), Arc::new(5u8))])
        }
    }
    register_device!(Brings);

    fn machine() -> Topology {
        let t = Topology::new();
        t.add(Location::root(), Row::fixed("")).unwrap();
        t.bind(&Location::root(), Arc::new(3u32)).unwrap();
        t.add(loc("gio0"), Row::offering("slot", Family::Gio)).unwrap();
        t.add(loc("pci0"), Row::fixed("")).unwrap();
        t.bind(&loc("pci0"), Arc::new(4u32)).unwrap();
        t.add(loc("pci0.3"), Row::offering("option", Family::Pci)).unwrap();
        t.add(loc("pci0.3.t0"), Row::fixed("")).unwrap();
        t
    }

    fn place(device: &str, at: &str, toml_opts: &str) -> Placement {
        Placement { device: device.into(), at: loc(at), opts: toml::from_str(toml_opts).unwrap() }
    }

    fn ctx_run<R>(f: impl FnOnce(&Ctx) -> R) -> R {
        let (d, k) = (Devices::new(), Disks::new(vec![]));
        f(&Ctx { devices: &d, disks: &k })
    }

    #[test]
    fn a_registered_type_is_in_makers() {
        assert!(MAKERS.iter().any(|m| m.name == "test_widget"));
        assert!(device_names().contains(&"test_brings"));
    }

    #[test]
    fn maker_lookup_is_by_name_and_family() {
        assert_eq!(maker_for("test_widget", Family::Gio).unwrap().family, Family::Gio);
        assert_eq!(maker_for("test_widget", Family::Pci).unwrap().family, Family::Pci);
        assert!(maker_for("test_widget", Family::Scsi).is_none());
        assert_eq!(device_names().iter().filter(|n| **n == "test_widget").count(), 1);
    }

    #[test]
    fn wrong_family_error_names_both_and_lists_takes() {
        let t = machine();
        let cfg = MachineConfig::default();
        let e = attach_device(&place("test_brings", "gio0", "label = \"x\""), &cfg, None, &t).unwrap_err();
        assert_eq!(e, "error: gio0: test_brings is a PCI card; gio0 is a GIO slot (takes: test_widget)");
    }

    #[test]
    fn unknown_device_lists_the_known_ones() {
        let t = machine();
        let e = attach_device(&place("test_widgt", "gio0", ""), &MachineConfig::default(), None, &t).unwrap_err();
        assert!(e.starts_with("error: unknown device [[test_widgt]]; devices: ") && e.contains("test_widget"), "{e}");
    }

    #[test]
    fn probe_claims_and_parses_but_builds_nothing() {
        let t = machine();
        let cfg = MachineConfig::default();
        attach_device(&place("test_widget", "pci0.3", "label = \"x\""), &cfg, None, &t).unwrap();
        let e = attach_device(&place("test_widget", "pci0.3", "label = \"x\""), &cfg, None, &t).unwrap_err();
        assert_eq!(e, "error: pci0.3: taken by test_widget");
        let e = attach_device(&place("test_widget", "gio0", "speed = 1\nlabel = \"x\""), &cfg, None, &t).unwrap_err();
        assert!(e.starts_with("error: gio0: unknown field `speed`, expected `size` or `label` for test_widget"), "{e}");
    }

    #[test]
    fn build_binds_returned_handles_at_the_card_and_at_hops_rows_made() {
        let t = machine();
        let cfg = MachineConfig::default();
        ctx_run(|ctx| {
            attach_device(&place("test_brings", "pci0.3", "label = \"x\""), &cfg, Some(ctx), &t).unwrap();
            attach_device(&place("test_widget", "gio0", "label = \"x\""), &cfg, Some(ctx), &t).unwrap();
        });
        assert_eq!(t.handle::<u8>(&loc("pci0.3.t0")).map(|a| *a), Some(5));
        assert_eq!(BUILT.load(Ordering::SeqCst) % 3, 0);
        assert!(BUILT.load(Ordering::SeqCst) >= 3);
    }

    #[test]
    fn a_handle_for_a_hop_rows_did_not_make_is_a_bind_error() {
        let t = machine();
        let e = ctx_run(|ctx| {
            attach_device(&place("test_brings", "pci0.3", "label = \"x\"\nsize = 1"), &MachineConfig::default(), Some(ctx), &t)
        })
        .unwrap_err();
        assert!(e.contains("pci0.3.t7") && e.contains("no such location"), "{e}");
    }

    #[test]
    fn attach_devices_orders_by_depth_then_name_then_index() {
        let t = machine();
        t.add(loc("gio1"), Row::offering("slot", Family::Gio)).unwrap();
        let mut cfg = MachineConfig::default();
        let opts = |l: &str| -> toml::Table { toml::from_str(&format!("label = \"{l}\"")).unwrap() };
        // "test_brings" sorts before "test_widget" but sits deeper, so it goes last;
        // within the array the entries keep their file order.
        cfg.place("test_widget", loc("gio1"), opts("ord-b"));
        cfg.place("test_widget", loc("gio0"), opts("ord-c"));
        cfg.place("test_brings", loc("pci0.3"), opts("ord-a"));
        ctx_run(|ctx| attach_devices(&cfg.placements().unwrap(), &cfg, Some(ctx), &t)).unwrap();
        let got: Vec<String> = ORDER.lock().iter().filter(|l| l.starts_with("ord-")).cloned().collect();
        assert_eq!(got, ["ord-b", "ord-c", "ord-a"]);
    }
}
