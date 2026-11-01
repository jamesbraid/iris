//! `iris --machines`: the machines this binary can build.
//!
//! Everything here is read from the same code that validates a config and
//! builds the machine ([`MachineProfile`], [`CpuModel::fits`] and friends),
//! so the list cannot show what `iris` would refuse.

use crate::config::{CpuModel, GraphicsBoard, MachineConfig, MachineProfile, MAX_BANKS};

/// A configuration of `profile` with `mb` in bank 0 and nothing else changed
/// from the defaults, to ask `validate()` whether the machine takes it.
fn config_with_bank(profile: MachineProfile, mb: u32) -> MachineConfig {
    let mut cfg = MachineConfig::default();
    cfg.machine.profile = profile;
    if let Some(cpu) = CpuModel::ALL.into_iter().find(|&c| profile.accepts_cpu(c).is_ok()) {
        cfg.machine.cpu = cpu;
    }
    cfg.banks = [0; MAX_BANKS];
    cfg.banks[0] = mb;
    cfg
}

/// The bank sizes in MB `validate()` accepts in bank 0 of `profile`. Some
/// machines take fewer than `bank_sizes` offers a menu: the O2 takes only the
/// kits SGI sold.
fn bank_mb(profile: MachineProfile) -> Vec<u32> {
    profile
        .bank_sizes()
        .iter()
        .copied()
        .filter(|&mb| mb != 0 && config_with_bank(profile, mb).validate().is_ok())
        .collect()
}

fn boards(profile: MachineProfile) -> Vec<GraphicsBoard> {
    GraphicsBoard::EVERY
        .into_iter()
        .filter(|b| profile.takes_graphics_board() && b.supports(profile))
        .collect()
}

/// `a, b or c`; `none` for an empty list.
fn list(items: &[String]) -> String {
    match items {
        [] => "none".into(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// The value of `[machine] profile` or `cpu` for `v`, as a config spells it.
fn name<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

fn describe(profile: MachineProfile) -> String {
    let mut out = format!("{}: {}\n", name(&profile), profile.label());
    let mut row = |key: &str, value: String| out.push_str(&format!("  {key}: {value}\n"));
    let cpus: Vec<String> = CpuModel::ALL.into_iter().filter(|&c| profile.accepts_cpu(c).is_ok()).map(|c| name(&c)).collect();
    row("cpu", list(&cpus));
    row("cpus", list(&(1..=profile.max_cpus()).map(|n| n.to_string()).collect::<Vec<_>>()));
    let sizes: Vec<String> = bank_mb(profile).iter().map(u32::to_string).collect();
    row("banks", format!("{}, each {} MB or empty", profile.memory_banks(), list(&sizes)));
    let default = GraphicsBoard::default_for(profile);
    let boards = boards(profile);
    row("graphics.board", if boards.is_empty() {
        "built in".to_string()
    } else {
        list(&boards.iter().map(|&b| if b == default { format!("{b} (default)") } else { b.to_string() }).collect::<Vec<_>>())
    });
    let controllers = profile.scsi_controllers();
    row("scsi", format!("{controllers} controller{}, target IDs 1 to {}", if controllers == 1 { "" } else { "s" }, profile.max_scsi_id()));
    row("section", profile.section().map_or("none".to_string(), |s| format!("[{s}]")));
    out
}

/// The text `--machines` prints with no name: a line per machine.
pub fn render_all() -> String {
    MachineProfile::EVERY.into_iter().map(|p| format!("{}  {}\n", name(&p), p.label())).collect()
}

/// The machine `wanted` names: a profile name as `[machine] profile` spells it,
/// or a prefix of exactly one (`octane`).
fn find(wanted: &str) -> Result<MachineProfile, String> {
    let wanted = wanted.trim().to_ascii_lowercase();
    let names = || MachineProfile::EVERY.iter().map(name).collect::<Vec<_>>().join(", ");
    if let Some(p) = MachineProfile::EVERY.into_iter().find(|p| name(p) == wanted) {
        return Ok(p);
    }
    let mut matches = MachineProfile::EVERY.into_iter().filter(|p| !wanted.is_empty() && name(p).starts_with(&wanted));
    match (matches.next(), matches.next()) {
        (Some(p), None) => Ok(p),
        (Some(_), Some(_)) => Err(format!("machine \"{wanted}\" is ambiguous (valid: {})", names())),
        _ => Err(format!("unknown machine \"{wanted}\" (valid: {})", names())),
    }
}

/// The text `--machines [NAME]` prints: a line per machine, or the block of
/// the one `name` names, each line of it named by the config key it describes.
pub fn render(name: Option<&str>) -> Result<String, String> {
    match name {
        None => Ok(render_all()),
        Some(wanted) => find(wanted).map(describe),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no name, a line per machine; with one, that machine's block with
    /// lines named by config key. A prefix naming one machine will do, and an
    /// unknown or ambiguous name lists the valid ones.
    #[test]
    fn machines_lists_one_line_each_and_describes_one_by_name() {
        let all = render(None).unwrap();
        assert_eq!(all.lines().count(), MachineProfile::EVERY.len());
        assert!(all.starts_with("indy_ip24  SGI Indy (IP24)\n"));
        assert!(all.contains("octane_ip30  SGI Octane (IP30)\n"));
        assert_eq!(render(Some("octane")).unwrap(), render(Some("octane_ip30")).unwrap());
        let text = render(Some("o2")).unwrap();
        assert!(text.contains("  graphics.board: built in\n"), "the O2");
        assert!(text.contains("  banks: 4, each 32, 64, 128 or 256 MB or empty\n"), "the O2 takes the kits SGI sold");
        let e = render(Some("vax")).unwrap_err();
        assert!(e.starts_with("unknown machine \"vax\" (valid: indy_ip24, "), "{e}");
        assert!(render(Some("indigo2")).unwrap_err().contains("ambiguous"));
        assert!(render(Some("")).is_err());
        let octane = render(Some("octane")).unwrap();
        assert_eq!(octane.trim_end(), "\
octane_ip30: SGI Octane (IP30)
  cpu: r10000
  cpus: 1 or 2
  banks: 4, each 64, 128, 256, 512, 1024 or 2048 MB or empty
  graphics.board: solidimpact (default), highimpact, maximpact or none
  scsi: 2 controllers, target IDs 1 to 15
  section: [octane]");
        assert!(!octane.contains('{'));
    }

    /// Everything listed is something a config file accepts: the profile,
    /// each processor and board, and the section.
    #[test]
    fn everything_listed_is_accepted_by_the_config() {
        for profile in MachineProfile::EVERY {
            let p = name(&profile);
            let cfg: MachineConfig = toml::from_str(&format!("[machine]\nprofile = \"{p}\"\n")).unwrap();
            assert_eq!(cfg.machine.profile, profile);
            for cpu in CpuModel::ALL.into_iter().filter(|&c| profile.accepts_cpu(c).is_ok()) {
                let cfg: MachineConfig = toml::from_str(&format!("[machine]\nprofile = \"{p}\"\ncpu = \"{}\"\n", name(&cpu))).unwrap();
                cfg.machine.profile.accepts_cpu(cfg.machine.cpu).unwrap();
            }
            for board in boards(profile) {
                toml::from_str::<MachineConfig>(&format!("[machine]\nprofile = \"{p}\"\n[graphics]\nboard = \"{board}\"\n")).unwrap();
            }
            if let Some(section) = profile.section() {
                toml::from_str::<MachineConfig>(&format!("[machine]\nprofile = \"{p}\"\n[{section}]\n")).unwrap();
            }
        }
    }

    /// Every board a profile lists passes the validation a config gets;
    /// every one it leaves out is refused.
    #[test]
    fn listed_boards_are_the_ones_validate_accepts() {
        for profile in MachineProfile::EVERY {
            if !profile.takes_graphics_board() {
                assert!(boards(profile).is_empty());
                continue;
            }
            for board in GraphicsBoard::EVERY {
                let mut cfg = config_with_bank(profile, bank_mb(profile)[bank_mb(profile).len() - 1]);
                cfg.graphics.board = Some(board);
                let refused = cfg.validate().err().is_some_and(|e| e.contains("graphics.board"));
                assert_eq!(boards(profile).contains(&board), !refused, "{profile:?} / {board}");
            }
        }
    }

    /// The bank sizes listed are the ones `validate()` accepts, and every
    /// other size the menu offers is refused: the O2 takes 32 to 256 MB kits,
    /// not the 8, 16 or 512 MB its menu list holds.
    #[test]
    fn listed_bank_sizes_are_the_ones_validate_accepts() {
        for profile in MachineProfile::EVERY {
            for &mb in profile.bank_sizes().iter().filter(|&&mb| mb != 0) {
                let accepted = config_with_bank(profile, mb).validate().is_ok();
                assert_eq!(bank_mb(profile).contains(&mb), accepted, "{profile:?} / {mb} MB");
            }
        }
        assert_eq!(bank_mb(MachineProfile::O2Ip32), [32, 64, 128, 256]);
    }

    /// Each list names every variant (a new one fails to compile here).
    #[test]
    fn the_lists_of_profiles_and_boards_are_complete() {
        for p in MachineProfile::EVERY {
            match p {
                MachineProfile::IndyIp24 | MachineProfile::Indigo2Ip22 | MachineProfile::Indigo2Ip28
                | MachineProfile::O2Ip32 | MachineProfile::OctaneIp30 => {}
            }
        }
        for b in GraphicsBoard::EVERY {
            match b {
                GraphicsBoard::Newport | GraphicsBoard::Xz | GraphicsBoard::Extreme | GraphicsBoard::SolidImpact
                | GraphicsBoard::HighImpact | GraphicsBoard::MaxImpact | GraphicsBoard::None => {}
            }
        }
        let mut names: Vec<String> = MachineProfile::EVERY.iter().map(name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), MachineProfile::EVERY.len());
    }
}
