//! The machines this build can emulate, as data.
//!
//! `iris --list-machines-json` prints [`catalogue`]: one record per
//! `[machine] profile`, with what a front end or a script needs to offer it
//! (name, label, processors, memory banks, SCSI, graphics boards) without
//! reading iris's source or guessing from the name. Everything comes from the
//! same tables `validate` checks a config against, so it cannot drift from what
//! iris accepts.
//!
//! The document is versioned by its own `schema` number. Fields are added in
//! place; a field's meaning does not change under the same number.

use crate::config::{GraphicsBoard, MachineProfile, MAX_BANKS};
use clap::ValueEnum;
use serde_json::{json, Value};

/// The layout of [`catalogue`]'s document.
pub const SCHEMA: u32 = 1;

/// The name `[graphics] board` takes for `board`.
fn board_name(board: GraphicsBoard) -> String {
    board.to_possible_value().expect("no board is skipped").get_name().to_string()
}

fn machine(profile: MachineProfile) -> Value {
    let cpus = profile.cpus();
    let boards: Vec<String> = GraphicsBoard::value_variants()
        .iter()
        .filter(|b| b.supports(profile))
        .map(|b| board_name(*b))
        .collect();
    json!({
        "name": profile.name(),
        "label": profile.label(),
        "cpus": cpus.iter().map(|p| json!({
            "name": p.name(),
            "label": p.label(),
            // The discriminants of `Family` are the model numbers.
            "family": format!("r{}", p.family as u32),
            "mhz": p.mhz,
        })).collect::<Vec<_>>(),
        "default_cpu": cpus[0].name(),
        "max_cpus": profile.max_cpus(),
        // `banks` entries the machine uses; the rest of the MAX_BANKS must be 0.
        "memory_banks": profile.memory_banks(),
        "max_memory_banks": MAX_BANKS,
        "bank_sizes_mb": profile.bank_sizes(),
        "scsi_controllers": profile.scsi_controllers(),
        "max_scsi_id": profile.max_scsi_id(),
        "graphics": {
            "default": board_name(GraphicsBoard::default_for(profile)),
            "boards": boards,
        },
    })
}

/// Every profile, in `MachineProfile::EVERY` order.
pub fn catalogue() -> Value {
    json!({
        "schema": SCHEMA,
        "iris": env!("CARGO_PKG_VERSION"),
        "machines": MachineProfile::EVERY.iter().map(|p| machine(*p)).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every profile is listed once, and what a record says is what the
    /// profile's own tables say.
    #[test]
    fn every_profile_is_listed_with_its_own_tables() {
        let doc = catalogue();
        let machines = doc["machines"].as_array().unwrap();
        assert_eq!(machines.len(), MachineProfile::EVERY.len());
        for (m, p) in machines.iter().zip(MachineProfile::EVERY) {
            assert_eq!(m["name"], p.name());
            assert_eq!(MachineProfile::from_name(m["name"].as_str().unwrap()), Ok(p));
            assert_eq!(m["default_cpu"], p.cpus()[0].name());
            assert_eq!(m["cpus"].as_array().unwrap().len(), p.cpus().len());
            assert_eq!(m["max_cpus"], p.max_cpus());
            assert_eq!(m["memory_banks"], p.memory_banks());
            assert_eq!(m["bank_sizes_mb"], json!(p.bank_sizes()));
            assert_eq!(m["scsi_controllers"], p.scsi_controllers());
            assert_eq!(m["max_scsi_id"], p.max_scsi_id());
            let want: Vec<_> = GraphicsBoard::value_variants().iter().filter(|b| b.supports(p)).map(|b| board_name(*b)).collect();
            assert_eq!(m["graphics"]["boards"], json!(want), "{}", p.name());
            assert_eq!(m["graphics"]["default"], board_name(GraphicsBoard::default_for(p)));
            let boards = m["graphics"]["boards"].as_array().unwrap();
            assert!(boards.contains(&m["graphics"]["default"]), "{}: default board is offered", p.name());
        }
    }

    /// The document survives a round trip through text, and says the Octane
    /// takes two processors and a VPro board that the Indy does not.
    #[test]
    fn the_document_round_trips_and_describes_the_octane() {
        let text = serde_json::to_string(&catalogue()).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["schema"], SCHEMA);
        let octane = doc["machines"].as_array().unwrap().iter().find(|m| m["name"] == "octane_ip30").unwrap();
        assert_eq!(octane["max_cpus"], 2);
        assert!(octane["graphics"]["boards"].as_array().unwrap().iter().any(|b| b == "v12"));
        let indy = doc["machines"].as_array().unwrap().iter().find(|m| m["name"] == "indy_ip24").unwrap();
        assert!(!indy["graphics"]["boards"].as_array().unwrap().iter().any(|b| b == "v12"));
    }
}
