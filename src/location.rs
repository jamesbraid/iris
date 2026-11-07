//! Where a device goes: a path of hops from the machine root, such as
//! `hpc3.0.t1`, `pci0.1.t4`, `xio15.pci0.0.t1` or `gio1`. Config, errors,
//! snapshots, the monitor and the GUI all spell a place this way.
//!
//! A hop is a tag from one table plus a number. Numbers are local to the node
//! that owns them, so a location depends only on the machine and its config,
//! never on attach order. Parsing needs no machine: the single-bus alias
//! `xio15.pci1.t1` is rewritten by `Topology::resolve`, which knows the
//! rows. Output is always canonical.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// One row per kind of hop. The index is the `Tag`, so the order here is the
/// sort order of sibling hops. A tag that is not numbered is a whole token.
const TAGS: &[(&str, bool)] = &[
    ("xio", true),  // widget on the XIO fabric, by hardware number
    ("pci", true),  // PCI bus N of the enclosing node
    ("", true),     // a bare number: a position on the enclosing bus
    ("hpc3", false), // the Indy and Indigo2 I/O chip
    ("gfx", false), // the graphics slot
    ("gio", true),  // GIO expansion slot, by address order
    ("t", true),    // SCSI target id
];

const BARE: u8 = 2;

fn tag_named(name: &str) -> Option<u8> {
    // `exp` is the old spelling of `gio`, accepted on input only.
    let name = if name == "exp" { "gio" } else { name };
    TAGS.iter().position(|t| t.0 == name).map(|i| i as u8)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hop {
    tag: u8,
    n: u8,
}

impl Hop {
    fn of(name: &str, n: u8) -> Hop {
        Hop { tag: tag_named(name).expect("tag in TAGS"), n }
    }
    pub fn xio(n: u8) -> Hop { Hop::of("xio", n) }
    pub fn pci(n: u8) -> Hop { Hop::of("pci", n) }
    pub fn pos(n: u8) -> Hop { Hop { tag: BARE, n } }
    pub fn gio(n: u8) -> Hop { Hop::of("gio", n) }
    pub fn t(n: u8) -> Hop { Hop::of("t", n) }
    pub fn hpc3() -> Hop { Hop::of("hpc3", 0) }
    pub fn gfx() -> Hop { Hop::of("gfx", 0) }
}

impl fmt::Display for Hop {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (name, numbered) = TAGS[self.tag as usize];
        if numbered {
            write!(f, "{name}{}", self.n)
        } else {
            f.write_str(name)
        }
    }
}

impl FromStr for Hop {
    type Err = String;
    fn from_str(s: &str) -> Result<Hop, String> {
        let bad = || format!("`{s}` is not a location hop");
        // Whole-token tags first, so `hpc3`'s digit is not read as a number.
        if let Some(i) = TAGS.iter().position(|t| !t.1 && t.0 == s) {
            return Ok(Hop { tag: i as u8, n: 0 });
        }
        let split = s.find(|c: char| c.is_ascii_digit()).ok_or_else(bad)?;
        let (name, digits) = s.split_at(split);
        let tag = tag_named(name).ok_or_else(bad)?;
        if !TAGS[tag as usize].1 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let n = digits.parse::<u8>().map_err(|_| format!("`{s}`: number does not fit 0-255"))?;
        Ok(Hop { tag, n })
    }
}

/// A path of hops from the machine root. The empty path is the root.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Location(Vec<Hop>);

impl Location {
    pub fn root() -> Location { Location(Vec::new()) }
    pub fn join(&self, h: Hop) -> Location {
        let mut v = self.0.clone();
        v.push(h);
        Location(v)
    }
    pub fn join_opt(&self, h: Option<Hop>) -> Location {
        h.map_or_else(|| self.clone(), |h| self.join(h))
    }
    pub fn parent(&self) -> Option<Location> {
        self.0.split_last().map(|(_, rest)| Location(rest.to_vec()))
    }
    pub fn depth(&self) -> usize { self.0.len() }
    pub fn last(&self) -> Option<Hop> { self.0.last().copied() }
    pub fn hops(&self) -> &[Hop] { &self.0 }
    /// The SCSI target id when the last hop is `tN`.
    pub fn target(&self) -> Option<u8> {
        self.last().filter(|h| TAGS[h.tag as usize].0 == "t").map(|h| h.n)
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for (i, h) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{h}")?;
        }
        Ok(())
    }
}

impl From<Location> for String {
    fn from(l: Location) -> String {
        l.to_string()
    }
}

impl TryFrom<String> for Location {
    type Error = String;
    fn try_from(s: String) -> Result<Location, String> {
        s.parse()
    }
}

impl FromStr for Location {
    type Err = String;
    fn from_str(s: &str) -> Result<Location, String> {
        if s.is_empty() {
            return Err("a location is empty".into());
        }
        s.split('.').map(str::parse).collect::<Result<_, _>>().map(Location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(s: &str) -> Location { s.parse().unwrap() }

    #[test]
    fn every_spec_example_round_trips() {
        for s in [
            "xio15.pci0.0.t1", "xio15.pci0.1.t15", "xio11.pci0.5", "pci0.5.pci0.1.t2", "hpc3.0.t1",
            "hpc3.1.t4", "gio0", "gio1", "gfx",
        ] {
            assert_eq!(loc(s).to_string(), s);
        }
    }

    #[test]
    fn exp1_parses_to_gio1_and_prints_gio1() {
        assert_eq!(loc("exp1"), loc("gio1"));
        assert_eq!(loc("exp0").to_string(), "gio0");
    }

    #[test]
    fn ordering_is_by_hop_number_not_text() {
        assert!(loc("pci0.2") < loc("pci0.10"));
        assert!(loc("xio9") < loc("xio15"));
    }

    #[test]
    fn bare_and_tagged_hops_differ() {
        assert_eq!(loc("pci0.3").hops(), &[Hop::pci(0), Hop::pos(3)]);
        assert_ne!(loc("pci0.3"), loc("pci0.t3"));
    }

    #[test]
    fn bad_strings_are_errors() {
        for s in ["", "pci", "pci0.", "t", "zz1", "pci256", "hpc30", "gfx1", "pci0..1", "t-1", "pci0x"] {
            assert!(s.parse::<Location>().is_err(), "{s:?} parsed");
        }
    }

    #[test]
    fn serde_is_a_string() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W { at: Location }
        let w = W { at: loc("pci0.1.t4") };
        let text = toml::to_string(&w).unwrap();
        assert_eq!(text.trim(), "at = \"pci0.1.t4\"");
        assert_eq!(toml::from_str::<W>(&text).unwrap(), w);
        assert!(toml::from_str::<W>("at = \"nowhere\"").is_err());
    }

    #[test]
    fn target_is_the_last_t_hop() {
        assert_eq!(loc("hpc3.0.t4").target(), Some(4));
        assert_eq!(loc("pci0.3").target(), None);
    }

    #[test]
    fn join_parent_depth() {
        let l = Location::root().join(Hop::hpc3()).join(Hop::pos(0)).join_opt(Some(Hop::t(1))).join_opt(None);
        assert_eq!(l, loc("hpc3.0.t1"));
        assert_eq!(l.depth(), 3);
        assert_eq!(l.parent(), Some(loc("hpc3.0")));
        assert_eq!(Location::root().parent(), None);
        assert_eq!(l.last(), Some(Hop::t(1)));
    }
}
