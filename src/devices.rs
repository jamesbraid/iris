//! The parts a machine starts, stops, resets, saves and gives monitor
//! commands, held once in registration order.
//!
//! A part that owns others (the HPC3, the IOC, the O2's bus) starts, stops
//! and resets them itself and is registered as one entry. `Devices` reaches a
//! child only to save and load its state, through [`Saveable::children`], so
//! the snapshot keeps the key each child always had. There are no per-entry
//! role flags: every top-level part gets every call and one with nothing to do
//! inherits the trait's no-op. The one registration-level fact is `optional`
//! (legacy snapshots may lack the key).
//!
//! A machine fills its `Devices` while it is built and never changes it after.
//! Everything here runs on build, start, stop, save, load and monitor threads.
//! No CPU, memory or device access path goes through it.

use std::sync::Arc;

use crate::monitor::Monitor;
use crate::traits::{Device, Part, Saveable};

/// When a part starts. Start runs Chipset, Io, Display, each in registration
/// order. Stop runs Io then Chipset, each in reverse, and Display last: a
/// display stop can hang joining its thread, so the disks must be stopped
/// before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    Chipset,
    Io,
    Display,
}

#[derive(Clone)]
struct Entry {
    key: String,
    phase: Phase,
    optional: bool,
    dev: Arc<dyn Part>,
}

/// One piece of saved state: a registered part or a child of one.
struct Node {
    key: String,
    state: Arc<dyn Saveable>,
    optional: bool,
}

#[derive(Default)]
pub struct Devices {
    entries: Vec<Entry>,
}

fn is_empty(v: &toml::Value) -> bool {
    v.as_table().is_some_and(|t| t.is_empty())
}

impl Devices {
    pub fn new() -> Devices {
        Devices::default()
    }

    /// Register a part. `key` is its snapshot key: a fixed name for a chip
    /// ("crime", "heart"), the location text for an attached device.
    pub fn add(&mut self, key: &str, phase: Phase, dev: Arc<dyn Part>) {
        self.push(key, phase, false, dev);
    }

    /// As `add`, for state a legacy snapshot may lack: `load` accepts the key
    /// absent. Said once, where the part is registered.
    pub fn add_optional(&mut self, key: &str, phase: Phase, dev: Arc<dyn Part>) {
        self.push(key, phase, true, dev);
    }

    fn push(&mut self, key: &str, phase: Phase, optional: bool, dev: Arc<dyn Part>) {
        assert!(self.entries.iter().all(|e| e.key != key), "devices: {key} registered twice");
        self.entries.push(Entry { key: key.to_string(), phase, optional, dev });
    }

    fn start_order(&self) -> Vec<&Entry> {
        let mut order: Vec<&Entry> = self.entries.iter().collect();
        order.sort_by_key(|e| e.phase); // stable: registration order within a phase
        order
    }

    fn stop_order(&self) -> Vec<&Entry> {
        let (display, mut rest): (Vec<_>, Vec<_>) = self.start_order().into_iter().partition(|e| e.phase == Phase::Display);
        rest.reverse();
        rest.extend(display);
        rest
    }

    pub fn start(&self) {
        for e in self.start_order() {
            e.dev.start();
        }
    }

    pub fn stop(&self) {
        for e in self.stop_order() {
            e.dev.stop();
        }
    }

    /// Put every part back in its power-on state, in registration order.
    pub fn power_on(&self) {
        for e in &self.entries {
            e.dev.power_on();
        }
    }

    /// Add each part's monitor commands, in registration order. A composite's
    /// `register_commands` includes its children's.
    pub fn register_monitor(&self, monitor: &Monitor) {
        for e in &self.entries {
            monitor.register_device(e.dev.clone() as Arc<dyn Device>);
        }
    }

    /// The order `start` (or `stop`) calls the parts in, by key.
    #[cfg(test)]
    pub fn lifecycle_order(&self, stopping: bool) -> Vec<String> {
        let order = if stopping { self.stop_order() } else { self.start_order() };
        order.into_iter().map(|e| e.key.clone()).collect()
    }

    /// The same parts with `last` moved to the end of the registration order.
    #[cfg(test)]
    pub(crate) fn reordered(&self, last: &str) -> Devices {
        let mut entries = self.entries.clone();
        let at = entries.iter().position(|e| e.key == last).expect("a registered key");
        let moved = entries.remove(at);
        entries.push(moved);
        Devices { entries }
    }

    /// Every piece of state in load order: a part's children (each after its
    /// own), then the part.
    fn nodes(&self) -> Vec<Node> {
        fn child(c: crate::traits::Child, out: &mut Vec<Node>) {
            for g in c.state.children() {
                child(g, out);
            }
            out.push(Node { key: c.key.to_string(), state: c.state, optional: c.optional });
        }
        let mut out = Vec::new();
        for e in &self.entries {
            for c in e.dev.children() {
                child(c, &mut out);
            }
            out.push(Node { key: e.key.clone(), state: e.dev.clone() as Arc<dyn Saveable>, optional: e.optional });
        }
        out
    }

    /// The saved state, a table keyed by `key`: each part's children first,
    /// under their own keys. A part with an empty state is left out.
    pub fn save(&self) -> toml::Value {
        let mut table = toml::map::Map::new();
        for n in self.nodes() {
            let v = n.state.save_state();
            if !is_empty(&v) {
                table.insert(n.key, v);
            }
        }
        toml::Value::Table(table)
    }

    /// The keys `save` would write now, in load order, each with whether the
    /// snapshot may lack it (a part registered with `add_optional`, or a child
    /// flagged optional).
    pub fn expected(&self) -> Vec<(String, bool)> {
        Self::expected_of(&self.nodes())
    }

    fn expected_of(nodes: &[Node]) -> Vec<(String, bool)> {
        nodes.iter().filter(|n| !is_empty(&n.state.save_state())).map(|n| (n.key.clone(), n.optional)).collect()
    }

    pub fn keys(&self) -> Vec<String> {
        self.expected().into_iter().map(|(k, _)| k).collect()
    }

    /// Check the table's key set against this machine's, then load children
    /// before their parent in registration order. A key missing that is not
    /// optional, or one the machine does not have, is refused before any part
    /// is touched, printing both sets.
    pub fn load(&self, v: &toml::Value) -> Result<(), String> {
        let table = v.as_table().ok_or("snapshot state is not a table")?;
        let nodes = self.nodes();
        let expected = Self::expected_of(&nodes);
        let missing = expected.iter().any(|(k, optional)| !optional && !table.contains_key(k));
        let unknown = table.keys().any(|k| !expected.iter().any(|(e, _)| e == k));
        if missing || unknown {
            let mut have: Vec<&str> = table.keys().map(String::as_str).collect();
            have.sort_unstable();
            let mut want: Vec<&str> = expected.iter().map(|(k, _)| k.as_str()).collect();
            want.sort_unstable();
            return Err(format!("the snapshot's state does not match this machine: it has {have:?}, the machine has {want:?}"));
        }
        Self::load_nodes(&nodes, table)
    }

    /// Load the parts the table holds and leave the rest as they are, without
    /// the key-set check: the developer live checkpoint, which leaves the
    /// SCSI controllers out.
    pub fn load_present(&self, v: &toml::Value) -> Result<(), String> {
        let table = v.as_table().ok_or("snapshot state is not a table")?;
        Self::load_nodes(&self.nodes(), table)
    }

    fn load_nodes(nodes: &[Node], table: &toml::map::Map<String, toml::Value>) -> Result<(), String> {
        for n in nodes {
            if let Some(state) = table.get(&n.key) {
                n.state.load_state(state).map_err(|e| format!("{}: {e}", n.key))?;
            }
        }
        Ok(())
    }
}

/// Helpers for tests that dirty a machine's saved state and check it comes back.
#[cfg(test)]
pub(crate) mod testing {
    /// `v` with its first hex scalar changed, or `None` where there is none: a
    /// state a part will load, and that differs from the one it saved. Fields
    /// named in `keep` are left alone (a register that survives a reset).
    pub(crate) fn dirtied(v: &toml::Value, keep: &[&str]) -> Option<toml::Value> {
        match v {
            toml::Value::String(s) if s.starts_with("0x") => {
                let n = u64::from_str_radix(&s[2..], 16).ok()?;
                Some(toml::Value::String(format!("0x{:0w$x}", n ^ 1, w = s.len() - 2)))
            }
            toml::Value::Table(t) => {
                for (k, x) in t {
                    if keep.contains(&k.as_str()) {
                        continue;
                    }
                    if let Some(d) = dirtied(x, keep) {
                        let mut t = t.clone();
                        t.insert(k.clone(), d);
                        return Some(toml::Value::Table(t));
                    }
                }
                None
            }
            toml::Value::Array(a) => {
                for (i, x) in a.iter().enumerate() {
                    if let Some(d) = dirtied(x, keep) {
                        let mut a = a.clone();
                        a[i] = d;
                        return Some(toml::Value::Array(a));
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// The table of a `Devices::save` with every part's state `dirtied`.
    pub(crate) fn dirtied_everywhere(state: &toml::Value, keep: &[&str]) -> toml::Value {
        let mut out = state.as_table().unwrap().clone();
        for (key, v) in state.as_table().unwrap() {
            if let Some(d) = dirtied(v, keep) {
                out.insert(key.clone(), d);
            }
        }
        toml::Value::Table(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{Child, Resettable};

    type Log = Arc<parking_lot::Mutex<Vec<String>>>;

    /// A state with a value, loaded and saved through a log.
    struct State {
        name: &'static str,
        value: parking_lot::Mutex<toml::Value>,
        log: Log,
        kids: Vec<Child>,
    }

    impl State {
        fn new(name: &'static str, log: &Log, value: Option<i64>) -> State {
            let value = match value {
                Some(n) => toml::Value::Integer(n),
                None => toml::Value::Table(toml::map::Map::new()),
            };
            State { name, value: parking_lot::Mutex::new(value), log: log.clone(), kids: Vec::new() }
        }
    }

    impl Saveable for State {
        fn save_state(&self) -> toml::Value {
            self.value.lock().clone()
        }
        fn load_state(&self, v: &toml::Value) -> Result<(), String> {
            self.log.lock().push(format!("{}:load", self.name));
            *self.value.lock() = v.clone();
            Ok(())
        }
        fn children(&self) -> Vec<Child> {
            self.kids.iter().map(|c| Child { key: c.key, state: c.state.clone(), optional: c.optional }).collect()
        }
    }

    impl Device for State {
        fn start(&self) {
            self.log.lock().push(format!("{}:start", self.name));
        }
        fn stop(&self) {
            self.log.lock().push(format!("{}:stop", self.name));
        }
    }

    impl Resettable for State {
        fn power_on(&self) {
            self.log.lock().push(format!("{}:power_on", self.name));
        }
    }

    fn log() -> Log {
        Arc::new(parking_lot::Mutex::new(Vec::new()))
    }

    fn taken(log: &Log) -> Vec<String> {
        std::mem::take(&mut *log.lock())
    }

    fn rec(name: &'static str, l: &Log) -> Arc<State> {
        Arc::new(State::new(name, l, Some(1)))
    }

    #[test]
    fn start_runs_chipset_then_io_then_display_in_registration_order() {
        let l = log();
        let mut d = Devices::new();
        d.add("disp", Phase::Display, rec("disp", &l));
        d.add("io1", Phase::Io, rec("io1", &l));
        d.add("chip1", Phase::Chipset, rec("chip1", &l));
        d.add("io2", Phase::Io, rec("io2", &l));
        d.add("chip2", Phase::Chipset, rec("chip2", &l));
        d.start();
        assert_eq!(taken(&l), ["chip1:start", "chip2:start", "io1:start", "io2:start", "disp:start"]);
        assert_eq!(d.lifecycle_order(false), ["chip1", "chip2", "io1", "io2", "disp"]);
    }

    #[test]
    fn stop_runs_io_then_chipset_in_reverse_then_display() {
        let l = log();
        let mut d = Devices::new();
        d.add("chip1", Phase::Chipset, rec("chip1", &l));
        d.add("disp", Phase::Display, rec("disp", &l));
        d.add("io1", Phase::Io, rec("io1", &l));
        d.add("chip2", Phase::Chipset, rec("chip2", &l));
        d.add("io2", Phase::Io, rec("io2", &l));
        d.add("disp2", Phase::Display, rec("disp2", &l));
        d.stop();
        assert_eq!(taken(&l), ["io2:stop", "io1:stop", "chip2:stop", "chip1:stop", "disp:stop", "disp2:stop"]);
        assert_eq!(d.lifecycle_order(true), ["io2", "io1", "chip2", "chip1", "disp", "disp2"]);
    }

    #[test]
    fn power_on_runs_in_registration_order() {
        let l = log();
        let mut d = Devices::new();
        d.add("a", Phase::Display, rec("a", &l));
        d.add("b", Phase::Chipset, rec("b", &l));
        d.power_on();
        assert_eq!(taken(&l), ["a:power_on", "b:power_on"]);
    }

    #[test]
    fn save_is_a_table_keyed_by_key_children_first_and_leaves_out_empty_state() {
        let l = log();
        let mut d = Devices::new();
        let kid = rec("kid", &l);
        let mut parent = State::new("parent", &l, Some(10));
        parent.kids = vec![Child { key: "kid", state: kid, optional: false }];
        d.add("parent", Phase::Chipset, Arc::new(parent));
        d.add("empty", Phase::Io, Arc::new(State::new("empty", &l, None)));
        d.add("solo", Phase::Io, rec("solo", &l));
        let saved = d.save();
        let t = saved.as_table().unwrap();
        assert_eq!(t.keys().cloned().collect::<std::collections::BTreeSet<_>>(), ["kid", "parent", "solo"].map(String::from).into());
        assert_eq!(t["parent"], toml::Value::Integer(10));
        assert_eq!(d.keys(), ["kid", "parent", "solo"], "children first, empty state left out");
    }

    #[test]
    fn load_checks_keys_before_touching_any_part_and_prints_both_sets() {
        let l = log();
        let mut d = Devices::new();
        d.add("a", Phase::Chipset, rec("a", &l));
        d.add("b", Phase::Io, rec("b", &l));
        let table = |keys: &[&str]| {
            toml::Value::Table(keys.iter().map(|k| (k.to_string(), toml::Value::Integer(2))).collect())
        };
        let e = d.load(&table(&["a"])).unwrap_err();
        assert!(e.contains("\"a\"") && e.contains("\"b\""), "both sets named: {e}");
        let e = d.load(&table(&["a", "b", "c"])).unwrap_err();
        assert!(e.contains("\"c\""), "an unknown key is refused: {e}");
        assert!(taken(&l).is_empty(), "no part was touched");
        d.load(&table(&["a", "b"])).unwrap();
        assert_eq!(taken(&l), ["a:load", "b:load"]);
    }

    #[test]
    fn load_lets_an_optional_key_or_optional_child_be_absent() {
        let l = log();
        let mut d = Devices::new();
        let mut parent = State::new("parent", &l, Some(1));
        parent.kids = vec![
            Child { key: "kid", state: rec("kid", &l), optional: false },
            Child { key: "kid2", state: rec("kid2", &l), optional: true },
        ];
        d.add("parent", Phase::Chipset, Arc::new(parent));
        d.add_optional("old", Phase::Io, rec("old", &l));
        let optional: Vec<_> = d.expected().into_iter().filter(|(_, o)| *o).map(|(k, _)| k).collect();
        assert_eq!(optional, ["kid2", "old"]);
        let table = toml::Value::Table(["kid", "parent"].iter().map(|k| (k.to_string(), toml::Value::Integer(3))).collect());
        d.load(&table).unwrap();
        assert_eq!(taken(&l), ["kid:load", "parent:load"]);
        let table = toml::Value::Table(["kid", "parent", "old"].iter().map(|k| (k.to_string(), toml::Value::Integer(3))).collect());
        d.load(&table).unwrap();
    }

    #[test]
    fn load_runs_children_before_their_parent() {
        let l = log();
        let mut d = Devices::new();
        let grandkid = rec("grandkid", &l);
        let mut kid = State::new("kid", &l, Some(1));
        kid.kids = vec![Child { key: "grandkid", state: grandkid, optional: false }];
        let mut parent = State::new("parent", &l, Some(1));
        parent.kids = vec![Child { key: "kid", state: Arc::new(kid), optional: false }];
        d.add("parent", Phase::Chipset, Arc::new(parent));
        d.add("next", Phase::Io, rec("next", &l));
        let saved = d.save();
        d.load(&saved).unwrap();
        assert_eq!(taken(&l), ["grandkid:load", "kid:load", "parent:load", "next:load"]);
        d.reordered("parent").load(&saved).unwrap();
        assert_eq!(taken(&l), ["next:load", "grandkid:load", "kid:load", "parent:load"]);
    }

    #[test]
    fn load_present_loads_only_what_the_table_holds() {
        let l = log();
        let mut d = Devices::new();
        d.add("a", Phase::Chipset, rec("a", &l));
        d.add("b", Phase::Io, rec("b", &l));
        let table = toml::Value::Table([("b".to_string(), toml::Value::Integer(9))].into_iter().collect());
        d.load_present(&table).unwrap();
        assert_eq!(taken(&l), ["b:load"]);
        assert_eq!(d.save().as_table().unwrap()["b"], toml::Value::Integer(9));
    }

    #[test]
    fn register_monitor_adds_top_level_parts_in_order() {
        struct Cmds(&'static str);
        impl Device for Cmds {
            fn register_commands(&self) -> Vec<(String, String)> {
                vec![(self.0.to_string(), String::new())]
            }
        }
        impl Saveable for Cmds {}
        impl Resettable for Cmds {}
        let mut d = Devices::new();
        d.add("a", Phase::Io, Arc::new(Cmds("alpha")));
        d.add("b", Phase::Chipset, Arc::new(Cmds("beta")));
        let m = Monitor::new(0);
        d.register_monitor(&m);
        assert_eq!(m.command_names(), ["alpha", "beta"]);
    }
}
