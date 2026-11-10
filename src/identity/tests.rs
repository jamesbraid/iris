//! The vectors are literals worked out by hand from the layouts in the module
//! header, so a change to the layout shows up here. Most use kind 1, which no
//! iris machine has, so that they differ from iris's own where the kind shows.

use super::*;

const MAC: [u8; 6] = [0x08, 0x00, 0x69, 0xe1, 0x02, 0xa0];

fn resolve(family: Family, emu_id: u32, serial: Option<&str>, hostid: Option<&str>, mac: Option<[u8; 6]>) -> Result<Identity, String> {
    Identity::resolve(family, 1, emu_id, serial, hostid, mac)
}

#[test]
fn iris_is_kind_two() {
    assert_eq!(KIND, 2);
}

#[test]
fn the_module_serial_counts_the_module_in_one_digit() {
    assert_eq!(module_serial(1, 42, 0).unwrap(), "KEMU100420");
    assert_eq!(hostid_from_text("KEMU100420"), 0xd510_0420);
    assert_eq!(module_serial(1, 42, 1).unwrap(), "KEMU100421");
    assert_eq!(module_serial(1, 42, 9).unwrap(), "KEMU100429");
    // The module number is one digit, so module 10 is refused, not 0.
    assert_eq!(module_serial(1, 42, 10), None);
    assert_eq!(module_serial(KIND, 7, 0).unwrap(), "KEMU200070");
}

/// IRIX folds only the first ten characters; the first two never show.
#[test]
fn the_fold_keeps_the_low_nibbles_of_the_first_ten_characters() {
    assert_eq!(hostid_from_text("LEMU1042"), 0xc5d5_1042);
    assert_eq!(hostid_from_text("EMU1-0042"), 0xd51d_0042);
    assert_eq!(hostid_from_text("EMU1L0000042"), 0x51c0_0000);
    assert_eq!(hostid_from_text("M2001347"), 0xd200_1347);
    assert_eq!(hostid_from_text("L3962017"), 0xc396_2017);
    assert_eq!(hostid_from_text(""), 0);
    assert_eq!(hostid_from_text("KEMU100420"), hostid_from_text("LEMU100420"));
}

#[test]
fn the_default_station_address_carries_the_kind_and_the_emu_id() {
    assert_eq!(default_mac(1, 42, 0).unwrap(), MAC);
    assert_eq!(hostid_from_mac(&MAC), 0x69e1_02a0);
    // Kind 2 changes only the nibble after E.
    assert_eq!(hostid_from_mac(&default_mac(2, 42, 0).unwrap()), 0x69e2_02a0);
    assert_eq!(hostid_from_mac(&default_mac(1, 0, 0).unwrap()), 0x69e1_0000);
    assert_eq!(default_mac(KIND, 0, 0).unwrap(), [0x08, 0x00, 0x69, 0xe2, 0x00, 0x00]);
}

/// The sub-index fills the low nibble of the last byte and never reaches the
/// kind or the emu-id, at both ends of the range.
#[test]
fn the_sub_index_never_reaches_the_emu_id() {
    for emu_id in [0, 1, 42, 255, 4094, 4095] {
        let base = default_mac(1, emu_id, 0).unwrap();
        assert_eq!(base[5] & 0xf, 0);
        for sub in 0..SUB_INDEX_COUNT {
            let mac = default_mac(1, emu_id, sub).unwrap();
            assert_eq!(mac[..5], base[..5]);
            assert_eq!(mac[5] >> 4, base[5] >> 4);
            assert_eq!(u32::from(mac[5] & 0xf), sub);
            assert_eq!(mac[3], 0xe1);
            assert_eq!(u32::from(mac[4]) << 4 | u32::from(mac[5] >> 4), emu_id);
        }
        assert_eq!(default_mac(1, emu_id, 16), None);
    }
}

#[test]
fn the_identity_chip_serial_is_the_marker_and_the_kind() {
    assert_eq!(chip_serial(1), 0x454d_5501_0000);
    assert_eq!(chip_serial(KIND), 0x454d_5502_0000);
}

#[test]
fn an_id_spells_as_a_serial_and_folds_back() {
    for (id, text) in [
        (0xd210_0887, "M2100887"),
        (0xd21a_0f87, "M21J0O87"),
        (0x0000_0001, "P0000001"),
        (0xffff_ffff, "OOOOOOOO"),
        (0xc000_0000, "L0000000"),
        (0x69e1_02a0, "F9N102J0"),
        (0xd510_0420, "M5100420"),
        (0, "P0000000"),
    ] {
        assert_eq!(text_from_hostid(id), text);
        assert_eq!(hostid_from_text(text), id);
    }
    // The round trip holds for every high nibble and a spread of ids.
    for id in (0..=u64::from(u32::MAX)).step_by(0x0123_4567) {
        let text = text_from_hostid(id as u32);
        assert_eq!(u64::from(hostid_from_text(&text)), id);
        assert!(text.as_bytes()[0].is_ascii_alphabetic());
    }
}

#[test]
fn a_hostid_is_exactly_eight_hex_digits() {
    assert_eq!(hostid_parse("d2100887"), Some(0xd210_0887));
    assert_eq!(hostid_parse("00000001"), Some(1));
    for bad in ["d210088", "d21008877", "d210088g", ""] {
        assert_eq!(hostid_parse(bad), None, "{bad}");
    }
}

#[test]
fn a_serial_is_one_to_ten_printable_characters_without_separators() {
    assert!(serial_valid("A"));
    assert!(serial_valid("AB CD"));
    for bad in ["", "M2100887000", "AB:CD", "AB;CD", "AB\tCD"] {
        assert!(!serial_valid(bad), "{bad:?}");
    }
}

/// The module families: the guest reads a module serial only when it starts
/// with K, the text is ten characters, and a short serial is padded on the
/// left with K.
#[test]
fn a_module_family_needs_a_serial_that_starts_with_k() {
    let id = resolve(Family::Module, 42, None, None, None).unwrap();
    assert_eq!(id.id, 0xd510_0420);
    assert_eq!(id.module_text(1).unwrap(), "KEMU100421");
    assert_eq!(id.module_text(10), None);

    assert_eq!(
        resolve(Family::Module, 0, Some("M2100887"), None, None).unwrap_err(),
        "serial 'M2100887' must start with K on this machine: the guest ignores a module serial that does not"
    );
    resolve(Family::Module, 0, Some("LEMU100420"), None, None).unwrap_err();

    // A short serial is padded on the left with K and folds as padded.
    let id = resolve(Family::Module, 0, Some("K0017299"), None, None).unwrap();
    assert_eq!(id.module_text(0).unwrap(), "KKK0017299");
    assert_eq!((hostid_from_text("KKK0017299"), id.id), (0xb001_7299, 0xb001_7299));
    // A ten-character one is used as is.
    let id = resolve(Family::Module, 0, Some("K210088799"), None, None).unwrap();
    assert_eq!((id.module_text(0).unwrap().as_str(), id.id), ("K210088799", 0x1008_8799));
    assert_eq!(resolve(Family::Module, 0, Some("KEMU100887"), None, None).unwrap().id, 0xd510_0887);
    // The id is the fold of the padded text even for a very short one.
    let id = resolve(Family::Module, 0, Some("K12"), None, None).unwrap();
    assert_eq!(id.module_text(0).unwrap(), "KKKKKKKK12");
    assert_eq!(id.id, hostid_from_text("KKKKKKKK12"));

    // hostid: "KK" and the spelling, so any id is reachable.
    let id = resolve(Family::Module, 0, None, Some("d2100887"), None).unwrap();
    assert_eq!(id.module_text(0).unwrap(), "KKM2100887");
    assert_eq!(hostid_from_text(&id.module_text(0).unwrap()), 0xd210_0887);
    assert_eq!(id.module_text(1).unwrap(), "KKM2100888");
    assert_eq!(hostid_from_text(&id.module_text(9).unwrap()), 0xd210_0890);
    assert_eq!(id.module_text(10), None);
    assert_eq!(resolve(Family::Module, 0, None, Some("ffffffff"), None).unwrap().module_text(0).unwrap(), "KKOOOOOOOO");
    // The wrap past ffffffff stays a valid spelling.
    let id = resolve(Family::Module, 0, None, Some("fffffffe"), None).unwrap();
    assert_eq!(hostid_from_text(&id.module_text(3).unwrap()), 1);

    // serial and hostid must agree, as the padded text folds.
    resolve(Family::Module, 0, Some("K0017299"), Some("b0017299"), None).unwrap();
    let e = resolve(Family::Module, 0, Some("K0017299"), Some("b0017298"), None).unwrap_err();
    assert!(e.contains("serial K0017299 folds to b0017299"), "{e}");
    // hostid -> text -> id.
    for value in (0..=u64::from(u32::MAX)).step_by(0x0123_4567) {
        let id = resolve(Family::Module, 0, None, Some(&format!("{value:08x}")), None).unwrap();
        let text = id.module_text(0).unwrap();
        assert!(text.starts_with('K'));
        assert_eq!(u64::from(hostid_from_text(&text)), value);
    }
}

#[test]
fn an_address_family_takes_its_id_from_the_station_address() {
    let other = [0x08, 0x00, 0x69, 0xaa, 0xbb, 0xcc];
    let foreign = [0x52, 0x54, 0x00, 0x11, 0x22, 0x33];
    let suggested = [0x08, 0x00, 0x69, 0x10, 0x08, 0x87];
    let a = |emu, serial, hostid, mac| resolve(Family::Address, emu, serial, hostid, mac);

    let id = a(0, None, None, None).unwrap();
    assert_eq!((id.id, id.user), (0x69e1_0000, false));
    assert_eq!(a(42, None, None, None).unwrap().id, 0x69e1_02a0);

    // hostid starts 69 and sets the address.
    let id = a(42, None, Some("69aabbcc"), None).unwrap();
    assert_eq!((id.id, id.user, id.address_from_id), (0x69aa_bbcc, true, true));
    assert_eq!(
        a(0, None, Some("d2100887"), None).unwrap_err(),
        "hostid=d2100887: this machine's host id is 69 plus 6 hex digits; use hostid=100887 or mac=08:00:69:10:08:87"
    );
    // The suggestions are accepted, and name the same id.
    assert_eq!(a(0, None, Some("100887"), None).unwrap().id, 0x6910_0887);
    assert_eq!(a(0, None, None, Some(suggested)).unwrap().id, 0x6910_0887);
    // The six-digit shorthand equals the full form.
    let id = a(0, None, Some("0bea40"), None).unwrap();
    assert_eq!((id.id, id.address_from_id), (0x690b_ea40, true));
    assert_eq!(a(0, None, Some("690bea40"), None).unwrap().id, 0x690b_ea40);
    assert_eq!(a(0, None, Some("0BEA40"), None).unwrap().id, 0x690b_ea40);
    assert_eq!(a(0, None, Some("0bea4"), None).unwrap_err(), "hostid must be 6 hex digits (the 69 is implied) or 8 starting 69, not '0bea4'");
    for bad in ["690bea400", "0bea4g", "690bea4g"] {
        a(0, None, Some(bad), None).unwrap_err();
    }

    // serial is refused unless it folds to 69xxxxxx, with this message.
    assert_eq!(
        a(0, Some("M2100887"), None, None).unwrap_err(),
        "serial folds to d2100887; this machine's host id comes from its station address, which always starts 69; use hostid=100887 or mac=08:00:69:10:08:87"
    );
    // "69" is characters whose low nibbles are 6 and 9: 'F' and 'I'.
    assert_eq!(a(0, Some("F9N102J0"), None, None).unwrap().id, 0x69e1_02a0);

    // mac sets the id; the three spellings must agree.
    assert_eq!(a(0, None, None, Some(other)).unwrap().id, 0x69aa_bbcc);
    a(0, None, Some("69e102a0"), Some(MAC)).unwrap();
    assert_eq!(
        a(0, None, Some("69aabbcc"), Some(MAC)).unwrap_err(),
        "mac=08:00:69:e1:02:a0 gives host id 69e102a0 but hostid=69aabbcc gives 69aabbcc; this machine's host id comes from its station address, so it would take 69e102a0 from mac; make them agree"
    );
    assert!(a(0, None, Some("aabbcc"), Some(MAC)).unwrap_err().contains("hostid=aabbcc"));
    a(0, None, Some("e102a0"), Some(MAC)).unwrap();
    assert!(a(0, Some("F9N102J0"), None, Some(other)).unwrap_err().contains("mac"));
    let e = a(0, Some("F9N102J0"), Some("69aabbcc"), None).unwrap_err();
    assert!(e.contains("F9N102J0") && e.contains("69aabbcc"), "{e}");
    // A mac outside 08:00:69 is refused, and the suggestion is accepted.
    assert_eq!(
        a(0, None, None, Some(foreign)).unwrap_err(),
        "mac=52:54:00:11:22:33: this machine's station address must start 08:00:69; use mac=08:00:69:11:22:33 or hostid=112233"
    );
    assert_eq!(a(0, None, Some("112233"), None).unwrap().id, 0x6911_2233);
    a(4096, None, None, None).unwrap_err();
}

/// The Indy and Indigo2 take their id from the last four bytes of whatever
/// address they hold: an Indy at 02:12:34:56:78:9a printed `3456789a` from
/// `sysinfo -s`.
#[test]
fn the_indy_takes_its_id_from_any_station_address() {
    let id = Identity::resolve(Family::AnyAddress, KIND, 0, None, None, None).unwrap();
    assert_eq!((id.id, id.station(0).unwrap()), (0x69e2_0000, [0x08, 0x00, 0x69, 0xe2, 0x00, 0x00]));
    let measured = [0x02, 0x12, 0x34, 0x56, 0x78, 0x9a];
    let id = Identity::resolve(Family::AnyAddress, KIND, 0, None, None, Some(measured)).unwrap();
    assert_eq!((id.id, id.station(0).unwrap()), (0x3456_789a, measured));
    let id = Identity::resolve(Family::AnyAddress, KIND, 0, None, Some("3456789a"), None).unwrap();
    assert_eq!(id.station(0).unwrap(), [0x08, 0x00, 0x34, 0x56, 0x78, 0x9a]);
}

#[test]
fn a_nodes_address_is_the_base_plus_its_number() {
    let id = Identity::resolve(Family::Address, KIND, 42, None, None, None).unwrap();
    assert_eq!(id.station(0).unwrap(), [0x08, 0x00, 0x69, 0xe2, 0x02, 0xa0]);
    assert_eq!(id.station(15).unwrap(), [0x08, 0x00, 0x69, 0xe2, 0x02, 0xaf]);
    // The emulator layout has room for sixteen only.
    assert!(id.station(16).unwrap_err().contains("at most 16"));

    // An id or address the user gave is not limited to sixteen and carries.
    let id = resolve(Family::Address, 0, None, Some("69000ffe"), None).unwrap();
    assert_eq!(id.station(3).unwrap(), [0x08, 0x00, 0x69, 0x00, 0x10, 0x01]);
    assert_eq!(id.hostids(2).unwrap(), [0x6900_0ffe, 0x6900_0fff]);
    // ...but not out of the 08:00:69 block.
    let id = resolve(Family::Address, 0, None, None, Some([0x08, 0x00, 0x69, 0xff, 0xff, 0xff])).unwrap();
    assert_eq!(id.station(0).unwrap(), [0x08, 0x00, 0x69, 0xff, 0xff, 0xff]);
    assert!(id.station(1).unwrap_err().contains("would leave the 08:00:69 block"));
}

#[test]
fn a_machines_ids_are_what_its_guest_prints() {
    let id = Identity::resolve(Family::Address, KIND, 42, None, None, None).unwrap();
    assert_eq!(id.hostids(1).unwrap(), [0x69e2_02a0]);
    let id = Identity::resolve(Family::Module, KIND, 42, None, None, None).unwrap();
    assert_eq!(id.hostids(2).unwrap(), [0xd520_0420, 0xd520_0421]);
    assert_eq!(id.station(0).unwrap(), [0x08, 0x00, 0x69, 0xe2, 0x02, 0xa0]);
    assert!(id.hostids(11).is_err());
}

#[test]
fn a_machine_says_which_id_its_guest_will_print() {
    let id = Identity::resolve(Family::Address, KIND, 42, None, None, None).unwrap();
    assert_eq!(id.to_string(), "host id 69e202a0 (station address 08:00:69:e2:02:a0)");
    let id = Identity::resolve(Family::Module, KIND, 42, None, None, None).unwrap();
    assert_eq!(id.to_string(), "host id d5200420 (module serial KEMU200420)");
}
