//! The system id, serial numbers and station addresses of an emulated SGI
//! machine.
//!
//! A machine has one observable, the 32-bit system id that IRIX reports
//! through `sysinfo -s` and FLEXlm's `lmhostid`. Every other value is derived
//! from it or is a spelling of it. The inputs, from highest precedence down:
//!
//! - `serial`, the system serial as a real unit has it written, or `hostid`,
//!   the id itself, or on a machine whose id comes from its station address,
//!   `[network] mac`;
//! - `emu_id`, 0 to 4095, which mints an id in the emulator layout;
//! - `emu_id` 0, the default.
//!
//! The emulator layout is a pure function of the kind, the `emu_id` and a
//! small sub-index (a node, module or port), with no hashing. The kind is a
//! digit that marks an id as minted by an emulator, and iris's is 2:
//!
//! - The system serial every L1 of a Tezro answers with is `LEMU`, the kind
//!   digit, the `emu_id` as four digits and `0`.
//! - A module serial of the Origin 2000 and Onyx2 is `KEMU`, the kind digit,
//!   the `emu_id` as four digits and the module number 0 to 9.
//! - The default station address is `08:00:69:E<kind>:<emu_id >> 4>:
//!   <emu_id & 15, sub-index>`. The sub-index takes the low nibble of the
//!   last byte, so an address derived from the default by adding a node or
//!   port number up to 15 never reaches the id.
//! - The O2's identity chip carries a 48-bit ROM serial: `45 4d 55` (`EMU`),
//!   the kind and two zero bytes.
//!
//! IRIX builds the id from a text serial by taking the low nibble of each of
//! its first ten characters and keeping the low 32 bits, or from the last
//! four bytes of the station address. A serial text is spelled from an id as
//! eight characters, one per hex digit: a digit 0 to 9 stays itself, a later
//! digit a to f becomes a letter J to O, and the first character is always a
//! letter, P for 0, A to I for 1 to 9 and J to O for a to f.
//!
//! These are synthetic values, not any machine's factory identity.
//!
//! Where an id is a model choice rather than something measured, it says so
//! where it is made: the padding of a short module serial, the letter
//! spelling of an id, and the O2 chip's filler are approximations.
//!
//! Ported from include/hw/mips/sgi/sgi-board-identity.h and
//! hw/mips/sgi/board-mac.c in jamesbraid/qemu-sgi-origami.

/// The kind of every machine iris builds.
pub const KIND: u32 = 2;

/// The `emu_id` is 0 (the default) or 1 to this.
pub const EMU_ID_MAX: u32 = 4095;

/// Nodes or ports a default station address can number, 0 to 15.
pub const SUB_INDEX_COUNT: u32 = 16;

/// Modules a module serial can number, 0 to 9.
pub const MODULE_COUNT: u32 = 10;

/// The longest serial text the model takes.
pub const SERIAL_MAX_LENGTH: usize = 10;

/// Whether `text` can be a serial: 1 to 10 printable ASCII characters, none
/// of `:` or `;`, which the module record and the L1's replies use as
/// separators.
pub fn serial_valid(text: &str) -> bool {
    let b = text.as_bytes();
    !b.is_empty() && b.len() <= SERIAL_MAX_LENGTH && b.iter().all(|&c| (0x20..=0x7e).contains(&c) && c != b':' && c != b';')
}

/// `KEMU`, the kind digit, four `emu_id` digits and the module number. `None`
/// when `module` is 10 or more, which the one digit cannot name.
pub fn module_serial(kind: u32, emu_id: u32, module: u32) -> Option<String> {
    assert!((1..=9).contains(&kind) && emu_id <= EMU_ID_MAX);
    (module < MODULE_COUNT).then(|| format!("KEMU{kind}{emu_id:04}{module}"))
}

/// `LEMU`, the kind digit, four `emu_id` digits and `0`: the system serial an
/// L1 answers with. It names the system, not a brick, so every L1 of a
/// machine gives the same text.
pub fn system_serial(kind: u32, emu_id: u32) -> String {
    assert!((1..=9).contains(&kind) && emu_id <= EMU_ID_MAX);
    format!("LEMU{kind}{emu_id:04}0")
}

/// The default station address of node or port `sub`: `08:00:69:E<kind>`, the
/// `emu_id`'s high 8 bits, then its low 4 bits over `sub`. `None` when `sub`
/// is 16 or more.
pub fn default_mac(kind: u32, emu_id: u32, sub: u32) -> Option<[u8; 6]> {
    assert!((1..=15).contains(&kind) && emu_id <= EMU_ID_MAX);
    (sub < SUB_INDEX_COUNT).then(|| [0x08, 0x00, 0x69, 0xe0 | kind as u8, (emu_id >> 4) as u8, ((emu_id & 0xf) << 4 | sub) as u8])
}

/// The O2 identity chip's 48-bit ROM serial, most significant byte first: `45
/// 4d 55` (`EMU`), the kind and two zero bytes. It belongs to the die and not
/// to the system, so it carries no `emu_id`.
pub fn chip_serial(kind: u32) -> u64 {
    0x454d55u64 << 24 | u64::from(kind & 0xff) << 16
}

/// The id IRIX computes from a text serial: the low nibble of each of the
/// first ten characters shifted in, the low 32 bits kept. A shorter text folds
/// in fewer nibbles; an empty one gives 0.
pub fn hostid_from_text(text: &str) -> u32 {
    let id = text.bytes().take(10).fold(0u64, |id, c| id << 4 | u64::from(c & 0xf));
    id as u32
}

/// Spell `id` as an eight-character serial text that folds back to `id`. A
/// digit 0 to 9 stays itself; a digit a to f becomes J to O; the first
/// character is always a letter, P for 0 and A to I for 1 to 9. The letters
/// are an approximation: the fold is the only thing measured.
pub fn text_from_hostid(id: u32) -> String {
    (0..8)
        .map(|i| {
            let nibble = (id >> (28 - 4 * i) & 0xf) as u8;
            match (nibble, i) {
                (10.., _) => (b'J' + nibble - 10) as char,
                (0, 0) => 'P',
                (_, 0) => (b'A' + nibble - 1) as char,
                _ => (b'0' + nibble) as char,
            }
        })
        .collect()
}

/// The id IRIX takes from a station address: its last four bytes.
pub fn hostid_from_mac(mac: &[u8; 6]) -> u32 {
    u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]])
}

/// The station address whose last four bytes are `id`.
pub fn mac_from_hostid(id: u32) -> [u8; 6] {
    let b = id.to_be_bytes();
    [0x08, 0x00, b[0], b[1], b[2], b[3]]
}

/// Parses exactly eight hex digits.
pub fn hostid_parse(text: &str) -> Option<u32> {
    (text.len() == 8 && text.bytes().all(|c| c.is_ascii_hexdigit())).then(|| u32::from_str_radix(text, 16).unwrap())
}

/// Parses the `hostid` of a machine that takes its id from a station address
/// starting `08:00:69`: eight hex digits starting 69, or six, the low 24 bits
/// with the 69 implied.
fn address_hostid_parse(text: &str) -> Result<u32, String> {
    if text.len() == 6 && text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Ok(0x6900_0000 | u32::from_str_radix(text, 16).unwrap());
    }
    let Some(value) = hostid_parse(text) else {
        return Err(format!("hostid must be 6 hex digits (the 69 is implied) or 8 starting 69, not '{text}'"));
    };
    if value >> 24 != 0x69 {
        let [_, a, b, c] = value.to_be_bytes();
        return Err(format!(
            "hostid={text}: this machine's host id is 69 plus 6 hex digits; use hostid={:06x} or mac=08:00:69:{a:02x}:{b:02x}:{c:02x}",
            value & 0xff_ffff
        ));
    }
    Ok(value)
}

/// Where a machine's guest takes its system id from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// The id comes from a module serial, which the guest only reads when it
    /// starts with K: the Origin 2000 and Onyx2.
    Module,
    /// The id comes from the station address, which must start `08:00:69`:
    /// the Octane, Origin 200 and O2, whose firmware insists on it. IRIX on
    /// the Indy and Indigo2 rejects any other first three bytes ("ec0: machine
    /// has bad ethernet address") and brings ec0 down, so they are held to the
    /// same prefix.
    Address,
    /// The id comes from the system serial text the L1 answers with: the
    /// Tezro. IRIX accepts any text the serial rule allows, so a serial is
    /// kept as given.
    Text,
}

/// What the inputs of one machine come to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub family: Family,
    pub kind: u32,
    pub emu_id: u32,
    /// The id of node, module or port 0.
    pub id: u32,
    /// The id came from `serial`, `hostid` or `mac`, so it has no emulator
    /// marker.
    pub user: bool,
    /// Module 0's serial text of a module family, or the system serial of a
    /// text family.
    pub text: String,
    /// The station address derives from an id the user gave, not `emu_id`.
    pub address_from_id: bool,
    /// The `mac` the user gave, the base address of node or port 0.
    pub mac: Option<[u8; 6]>,
}

fn unicast(mac: &[u8; 6]) -> bool {
    mac.iter().any(|&b| b != 0) && mac[0] & 1 == 0
}

fn mac_text(mac: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

impl Identity {
    /// Work out the identity of a machine of `family` from its inputs.
    /// `serial` and `hostid` are `None` or empty when not given, `mac` is
    /// `None` when not given and is only a source of the id on an address
    /// family. Two inputs that name different ids are refused, with a message
    /// naming both and the family's rule.
    pub fn resolve(family: Family, kind: u32, emu_id: u32, serial: Option<&str>, hostid: Option<&str>, mac: Option<[u8; 6]>) -> Result<Identity, String> {
        let serial = serial.filter(|s| !s.is_empty());
        let hostid = hostid.filter(|s| !s.is_empty());
        let address = family == Family::Address;
        let module = family == Family::Module;
        let mut out = Identity { family, kind, emu_id, id: 0, user: false, text: String::new(), address_from_id: false, mac };
        if emu_id > EMU_ID_MAX {
            return Err(format!("emu_id must be 0 to {EMU_ID_MAX}, not {emu_id}"));
        }
        let mut serial_id = 0;
        if let Some(s) = serial {
            if !serial_valid(s) {
                return Err(format!("serial '{s}' must be 1 to {SERIAL_MAX_LENGTH} printable characters, none of ':' or ';'"));
            }
            if module && !s.starts_with('K') {
                return Err(format!("serial '{s}' must start with K on this machine: the guest ignores a module serial that does not"));
            }
            // The record holds ten characters. A shorter serial is padded on
            // the left with K, so it still starts with K; the padding is a
            // model choice (an approximation). The id is the fold of the
            // padded text. The other families keep the serial as given.
            out.text = if module { format!("{}{s}", "K".repeat(SERIAL_MAX_LENGTH - s.len())) } else { s.to_string() };
            serial_id = hostid_from_text(&out.text);
        }
        let mut hostid_id = 0;
        if let Some(h) = hostid {
            hostid_id = match family {
                Family::Address => address_hostid_parse(h)?,
                _ => hostid_parse(h).ok_or_else(|| format!("hostid must be 8 hex digits, not '{h}'"))?,
            };
        }
        if address {
            if family == Family::Address && serial.is_some() && serial_id >> 24 != 0x69 {
                return Err(format!(
                    "serial folds to {serial_id:08x}; this machine's host id comes from its station address, which always starts 69; use hostid={:06x} or mac=08:00:69:{:02x}:{:02x}:{:02x}",
                    serial_id & 0xff_ffff,
                    serial_id >> 16 & 0xff,
                    serial_id >> 8 & 0xff,
                    serial_id & 0xff
                ));
            }
            let mac_id = mac.as_ref().map(hostid_from_mac);
            let mtext = mac.as_ref().map(mac_text).unwrap_or_default();
            if let Some(m) = mac.filter(|m| family == Family::Address && m[..3] != [0x08, 0x00, 0x69]) {
                return Err(format!(
                    "mac={mtext}: this machine's station address must start 08:00:69; use mac=08:00:69:{:02x}:{:02x}:{:02x} or hostid={:02x}{:02x}{:02x}",
                    m[3], m[4], m[5], m[3], m[4], m[5]
                ));
            }
            if let (Some(s), Some(h)) = (serial, hostid) {
                if serial_id != hostid_id {
                    return Err(format!(
                        "serial {s} folds to {serial_id:08x} but hostid={h} gives {hostid_id:08x}; this machine's host id comes from its station address, so they must name the same id"
                    ));
                }
            }
            if let (Some(id), Some(h)) = (mac_id, hostid) {
                if id != hostid_id {
                    return Err(format!(
                        "mac={mtext} gives host id {id:08x} but hostid={h} gives {hostid_id:08x}; this machine's host id comes from its station address, so it would take {id:08x} from mac; make them agree"
                    ));
                }
            }
            if let (Some(id), Some(s)) = (mac_id, serial) {
                if id != serial_id {
                    return Err(format!(
                        "mac={mtext} gives host id {id:08x} but serial {s} folds to {serial_id:08x}; this machine's host id comes from its station address, so it would take {id:08x} from mac; make them agree"
                    ));
                }
            }
            out.text.clear();
            if let Some(id) = mac_id {
                out.user = true;
                out.id = id;
            } else if serial.is_some() || hostid.is_some() {
                out.user = true;
                out.address_from_id = true;
                out.id = if hostid.is_some() { hostid_id } else { serial_id };
            } else {
                out.id = hostid_from_mac(&default_mac(kind, emu_id, 0).unwrap());
            }
            return Ok(out);
        }
        if let (Some(s), Some(h)) = (serial, hostid) {
            if serial_id != hostid_id {
                return Err(format!("serial {s} folds to {serial_id:08x} but hostid={h}; this machine's host id comes from its serial text, so they must name the same id"));
            }
        }
        if serial.is_some() || hostid.is_some() {
            out.user = true;
            if serial.is_some() {
                out.id = serial_id;
            } else {
                out.id = hostid_id;
                // "KK" and the spelling: any id is reachable and K comes first.
                out.text = if module { format!("KK{}", text_from_hostid(hostid_id)) } else { text_from_hostid(hostid_id) };
            }
        } else {
            out.text = if module { module_serial(kind, emu_id, 0).unwrap() } else { system_serial(kind, emu_id) };
            out.id = hostid_from_text(&out.text);
        }
        Ok(out)
    }

    /// The serial text of module `module`, always ten characters and starting
    /// with K. An id the user gave puts the serial as given, padded on the
    /// left with K, on module 0, and `KK` and the spelling of the id plus
    /// `module` on module N; otherwise it is the emulator layout. `None` when
    /// `module` is 10 or more.
    pub fn module_text(&self, module: u32) -> Option<String> {
        if module >= MODULE_COUNT {
            return None;
        }
        if self.family != Family::Module || !self.user {
            return module_serial(self.kind, self.emu_id, module);
        }
        Some(if module == 0 { self.text.clone() } else { format!("KK{}", text_from_hostid(self.id.wrapping_add(module))) })
    }

    /// The station address of node or port `offset`: the `mac` the user
    /// gave, or the address an id the user gave spells, or the emulator
    /// layout, plus `offset`. An error when the sum is not a unicast address,
    /// when the emulator layout has no room for `offset` (16 or more), or
    /// when an address family's node would leave the `08:00:69` block.
    pub fn station(&self, offset: u32) -> Result<[u8; 6], String> {
        let base = match self.mac {
            Some(m) => m,
            None if self.address_from_id => mac_from_hostid(self.id),
            None => {
                // The default leaves the low nibble for the node or port
                // number, so adding it below cannot carry into the emu_id.
                default_mac(self.kind, self.emu_id, 0).filter(|_| offset < SUB_INDEX_COUNT).ok_or_else(|| {
                    format!("the default station address numbers at most {SUB_INDEX_COUNT} nodes or ports, and node {offset} does not fit; set [network] mac")
                })?
            }
        };
        if !unicast(&base) {
            return Err("mac must be a nonzero unicast address".into());
        }
        let value = base.iter().fold(0u64, |v, &b| v << 8 | u64::from(b));
        // An address family's id is the address's last four bytes and starts
        // 69, so a node's address must stay in the 08:00:69 block: the sum
        // may not carry out of the last three bytes.
        if self.family == Family::Address && (value & 0xff_ffff) + u64::from(offset) > 0xff_ffff {
            return Err(format!(
                "node {offset} of mac={} would leave the 08:00:69 block, which this machine's station address must stay in; use mac=08:00:69:xx:xx:xx with room for the nodes, or a lower hostid",
                mac_text(&base)
            ));
        }
        let sum = (value + u64::from(offset)) & 0xffff_ffff_ffff;
        let mac: [u8; 6] = std::array::from_fn(|i| (sum >> (40 - 8 * i)) as u8);
        if !unicast(&mac) {
            return Err(format!("mac leaves node {offset} without a unicast address"));
        }
        Ok(mac)
    }

    /// Every id the guest can print, one per node, port or module in order:
    /// `count` of them. The first is the machine's `id`.
    pub fn hostids(&self, count: u32) -> Result<Vec<u32>, String> {
        (0..count)
            .map(|n| match self.family {
                Family::Address => self.station(n).map(|m| hostid_from_mac(&m)),
                Family::Module => self.module_text(n).map(|t| hostid_from_text(&t)).ok_or_else(|| format!("module serials number at most {MODULE_COUNT} modules, and module {n} does not fit")),
                // Every L1 answers with the one system serial.
                Family::Text => Ok(hostid_from_text(&self.text)),
            })
            .collect()
    }
}

impl std::fmt::Display for Identity {
    /// The line a machine prints at start-up: the id the guest will print,
    /// and where it comes from.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.family, self.hostids(1)) {
            (Family::Module, Ok(ids)) => write!(f, "host id {:08x} (module serial {})", ids[0], self.module_text(0).unwrap_or_default()),
            (Family::Text, Ok(ids)) => write!(f, "host id {:08x} (system serial {})", ids[0], self.text),
            (_, Ok(ids)) => write!(f, "host id {:08x} (station address {})", ids[0], self.station(0).map(|m| mac_text(&m)).unwrap_or_default()),
            (_, Err(e)) => write!(f, "{e}"),
        }
    }
}

#[cfg(test)]
mod tests;
