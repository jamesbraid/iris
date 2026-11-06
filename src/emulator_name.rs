//! The emulator's name for banners and window titles.

use crate::config::MachineProfile;

/// The profile of the machine this process is running, set by
/// [`crate::workstation::build`] so [`emulator_name`] can pick a model word
/// that matches it. Stored as the `MachineProfile` discriminant; `u8::MAX`
/// means no machine has been built yet.
static EMULATOR_PROFILE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);

/// Record the profile of the machine being built (see [`emulator_name`]).
pub fn set_emulator_profile(profile: MachineProfile) {
    EMULATOR_PROFILE.store(profile as u8, std::sync::atomic::Ordering::Relaxed);
}

fn emulator_profile() -> Option<MachineProfile> {
    let raw = EMULATOR_PROFILE.load(std::sync::atomic::Ordering::Relaxed);
    [MachineProfile::IndyIp24, MachineProfile::Indigo2Ip22, MachineProfile::Indigo2Ip28, MachineProfile::O2Ip32, MachineProfile::OctaneIp30]
        .into_iter().find(|p| *p as u8 == raw)
}

/// The words an `I` in the expansion may stand for, per machine. Each list
/// names the machine as the user configured it, so the title never calls an
/// Indigo2 an Indy or an O2 anything but IP32. Without a profile only the
/// model-neutral words remain.
fn emulator_name_thirds(profile: Option<MachineProfile>) -> &'static [&'static str] {
    match profile {
        Some(MachineProfile::IndyIp24) => &["IRIX", "Iris", "IPsomething", "Indy", "IP24"],
        Some(MachineProfile::Indigo2Ip22) => &["IRIX", "Iris", "IPsomething", "Indigo2", "IP22"],
        Some(MachineProfile::Indigo2Ip28) => &["IRIX", "Iris", "IPsomething", "Indigo2", "IP28"],
        Some(MachineProfile::O2Ip32) => &["IRIX", "Iris", "IPsomething", "IP32"],
        Some(MachineProfile::OctaneIp30) => &["IRIX", "Iris", "IPsomething", "IP30"],
        None => &["IRIX", "Iris", "IPsomething"],
    }
}

/// The emulator's name for banners and window titles. Three runs in four it
/// is the canonical one; otherwise an expansion of IRIS rolled once per
/// process, whose third word is chosen to match the machine profile that
/// `set_emulator_profile` recorded (so a GUI that starts a different machine
/// later gets the same roll with the right model word).
pub fn emulator_name() -> String {
    /// `None` for the canonical name, else the roll (first, third, fourth).
    static ROLL: std::sync::OnceLock<Option<(usize, u64, usize)>> = std::sync::OnceLock::new();
    const FIRSTS: [&str; 9] = ["Irresponsible", "Incredible", "Insufferable", "Infuriating", "Inaccurate", "Incomplete", "Interactive", "Indomitable", "Irresistible"];
    const FOURTHS: [&str; 4] = ["Simulator", "System", "Substitute", "Sandbox"];

    let roll = ROLL.get_or_init(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        if now % 4 != 0 {
            return None;
        }
        Some((((now / 4) % FIRSTS.len() as u64) as usize, now / 64, ((now / 256) % FOURTHS.len() as u64) as usize))
    });

    match *roll {
        None => "Irresponsible Rust IRIX Simulator".to_string(),
        Some((first, third_roll, fourth)) => {
            let thirds = emulator_name_thirds(emulator_profile());
            let third = thirds[(third_roll % thirds.len() as u64) as usize];
            format!("{} Rust {} {}", FIRSTS[first], third, FOURTHS[fourth])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No profile's expansion may name another machine: an Indigo2 is never
    /// an "Indy", an Indy never "IP22", and the O2 is only ever "IP32".
    #[test]
    fn thirds_name_only_the_configured_machine() {
        let foreign = |p: Option<MachineProfile>| -> &'static [&'static str] {
            match p {
                Some(MachineProfile::IndyIp24) => &["Indigo2", "IP22", "IP28", "IP32", "O2"],
                Some(MachineProfile::Indigo2Ip22) => &["Indy", "IP24", "IP28", "IP32", "O2"],
                Some(MachineProfile::Indigo2Ip28) => &["Indy", "IP24", "IP22", "IP32", "O2"],
                Some(MachineProfile::O2Ip32) => &["Indy", "Indigo2", "IP24", "IP22", "IP28", "IP30", "O2"],
                Some(MachineProfile::OctaneIp30) => &["Indy", "Indigo2", "IP24", "IP22", "IP28", "IP32", "O2"],
                None => &["Indy", "Indigo2", "IP24", "IP22", "IP28", "IP30", "IP32", "O2"],
            }
        };
        for p in [None, Some(MachineProfile::IndyIp24), Some(MachineProfile::Indigo2Ip22),
                  Some(MachineProfile::Indigo2Ip28), Some(MachineProfile::O2Ip32), Some(MachineProfile::OctaneIp30)] {
            let thirds = emulator_name_thirds(p);
            assert!(!thirds.is_empty());
            for w in thirds {
                assert!(w.starts_with('I'), "{p:?}: {w} breaks the acronym");
                assert!(!foreign(p).contains(w), "{p:?}: {w} names another machine");
            }
        }
    }

    #[test]
    fn profile_round_trips_through_the_global() {
        for p in [MachineProfile::IndyIp24, MachineProfile::Indigo2Ip22, MachineProfile::Indigo2Ip28, MachineProfile::O2Ip32, MachineProfile::OctaneIp30] {
            set_emulator_profile(p);
            assert_eq!(emulator_profile(), Some(p));
        }
        let name = emulator_name();
        assert!(name.starts_with('I') && name.contains(" Rust "), "{name}");
    }
}
