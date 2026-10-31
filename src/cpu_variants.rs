//! The R10000-family processors each machine shipped with: part, clock and
//! secondary cache size, with the PRId the model reports.
//!
//! One table, [`VARIANTS`], is the whole answer. `MachineConfig::validate`
//! refuses a part, clock or cache size a machine has no row for, each machine
//! takes its Count rate and PRId from its row, and [`shadow_for`] maps a row
//! to the cache model (the L2 size is a const parameter of that model, so
//! a row without an arm there cannot be built).
//!
//! Every field is graded against origami's `r10000-family-identity` datasheet,
//! the owner of part, revision and machine pairings. `Grade::Observed` is an
//! inventory, capture or product catalogue; `Grade::Inferred` is derived from
//! one, or picked from a set of printed values (the PRId nibbles of the R10000
//! manual); `Grade::Borrowed` is a value this emulator already used that no
//! datasheet states for that machine (none now).
//!
//! # Adding a row
//!
//! Append a [`CpuVariant`], then add the (profile, part, cache) arm to
//! [`shadow_for`] if the cache model for that size and line does not exist.
//! The first row for a (machine, part) pair is the default when `cpu_mhz` and
//! `cpu_l2_mib` are left out, so list the common one first. The tests in this
//! file check each row against the cache model it builds.
//!
//! # The A parts
//!
//! The R12000A, R14000A and R16000A are not a separate guest-visible
//! identity. No PRId is reserved for them, and the IRIX and PROM name tables
//! have no entry for one: a module SGI sold as an R12000A at 400 MHz prints
//! `R12000 3.5`, and an R14000A at 600 MHz prints `R14000 2.4` (origami's
//! `r10000-family-identity`, "What the A parts are"). Those modules are
//! ordinary rows with the sheet's PRId, so [`Stepping`] has only `Base`.
//!
//! Two things the sheet reports about the A die are not modelled and need
//! their own contract reviews before they are: the nine-bit secondary-cache
//! syndrome in performance control 0, and tying a `vr12000a` variant to
//! revision major 3.

use crate::config::{CpuModel, MachineProfile};

/// How well a value is established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    /// Printed by a product catalogue, owner inventory or capture.
    Observed,
    /// Derived from an observation by a published encoding.
    Inferred,
    /// Used by this emulator for the machine before; no datasheet states it
    /// for this machine. Replace it when one does. No row uses it now.
    Borrowed,
}

/// Silicon variant of a part. Only `Base` exists: the A parts are not a
/// separate guest-visible identity, so a module sold as one is the plain part
/// (see the module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stepping {
    Base,
}

/// One shipped processor configuration of one machine.
#[derive(Debug, Clone, Copy)]
pub struct CpuVariant {
    pub profile: MachineProfile,
    pub model: CpuModel,
    pub stepping: Stepping,
    /// Pipeline clock. CP0 Count ticks at half of it.
    pub mhz: u32,
    /// External secondary cache, in MiB.
    pub l2_mib: u32,
    /// CP0 PRId a guest reads.
    pub prid: u32,
    /// Grade of `mhz` and `l2_mib` together.
    pub config: Grade,
    /// Grade of `prid`.
    pub prid_grade: Grade,
    /// Where the row comes from, as an origami datasheet or research note.
    pub source: &'static str,
}

impl CpuVariant {
    pub const fn count_hz(&self) -> u64 {
        self.mhz as u64 * 500_000
    }

    pub const fn l2_bytes(&self) -> usize {
        (self.l2_mib as usize) << 20
    }
}

use CpuModel::{R10000, R12000, R14000};
use Grade::{Inferred, Observed};
use MachineProfile::{FuelIp34, Indigo2Ip28, O2Ip32, OctaneIp30};

const fn row(
    profile: MachineProfile, model: CpuModel, mhz: u32, l2_mib: u32, config: Grade,
    prid: u32, prid_grade: Grade, source: &'static str,
) -> CpuVariant {
    CpuVariant { profile, model, stepping: Stepping::Base, mhz, l2_mib, prid, config, prid_grade, source }
}

/// Every shipped configuration this emulator models, from the "Machine and
/// part matrix" and "Identity registers per part" sections of origami's
/// `r10000-family-identity` datasheet (cited below as the sheet).
///
/// The sheet prints a set of revisions for some clock ranges (for example
/// R10000 2.4, 2.5 and 2.6 at 175 and 195 MHz) without saying which clock got
/// which. Those rows take one member of the printed set and are graded
/// `Inferred`; the clock and cache are still `Observed`. A module SGI sold as
/// an R12000A or R14000A is the plain part with the revision the sheet records
/// for it: the A name is not guest-visible.
///
/// Deliberately absent: the Fuel at 600 MHz with 4 MiB and the Fuel R16000
/// (sheet rows, but the Fuel PROM takes its Config boot-mode word from a
/// per-configuration flash record and only the 500 MHz, 2 MiB record is known,
/// see `ip35_machine::FUEL_CP0_CONFIG`), and the Tezro and Origin machines,
/// which this tree does not model.
pub const VARIANTS: &[CpuVariant] = &[
    // Indigo2 IMPACT R10000 (IP28). Sheet: R10000 2.4, 2.5, 2.6 at 175 and
    // 195 MHz with 1 MiB; 2.5 at 250 MHz with 2 MiB.
    row(Indigo2Ip28, R10000, 195, 1, Observed, 0x0925, Inferred,
        "sheet, matrix: IP28 R10000 2.4/2.5/2.6, 175 and 195 MHz, 1 MiB (revision picked from the set)"),
    row(Indigo2Ip28, R10000, 175, 1, Observed, 0x0925, Inferred,
        "sheet, matrix: as the 195 MHz row"),
    row(Indigo2Ip28, R10000, 250, 2, Observed, 0x0925, Observed,
        "sheet, matrix: IP28 R10000 2.5, 250 MHz, 2 MiB"),
    // O2 (IP32). Sheet: R10000 2.5/2.6/2.7 at 150, 174, 175, 195 MHz, 1 MiB;
    // R10000 3.4 at 225 and 250 MHz, 1 MiB; R12000 2.3 at 270 and 300 MHz,
    // 1 MiB; R12000 3.5 (the O2+, sold as R12000A) at 400 MHz, 2 MiB.
    row(O2Ip32, R10000, 195, 1, Observed, 0x0925, Inferred,
        "sheet, matrix: O2 R10000 2.5/2.6/2.7, 150-195 MHz, 1 MiB (revision picked from the set)"),
    row(O2Ip32, R10000, 175, 1, Observed, 0x0925, Inferred, "sheet, matrix: as the 195 MHz row"),
    row(O2Ip32, R10000, 174, 1, Observed, 0x0925, Inferred, "sheet, matrix: as the 195 MHz row"),
    row(O2Ip32, R10000, 150, 1, Observed, 0x0925, Inferred, "sheet, matrix: as the 195 MHz row"),
    row(O2Ip32, R10000, 250, 1, Observed, 0x0934, Observed, "sheet, matrix: O2 R10000 3.4, 225 and 250 MHz, 1 MiB"),
    row(O2Ip32, R10000, 225, 1, Observed, 0x0934, Observed, "sheet, matrix: as the 250 MHz row"),
    row(O2Ip32, R12000, 300, 1, Observed, 0x0e23, Observed,
        "sheet, matrix: O2 R12000 2.3, 270 and 300 MHz, 1 MiB"),
    row(O2Ip32, R12000, 270, 1, Observed, 0x0e23, Observed, "sheet, matrix: as the 300 MHz row"),
    row(O2Ip32, R12000, 400, 2, Observed, 0x0e35, Observed,
        "sheet, matrix: O2 R12000 3.5, 400 MHz, 2 MiB (the O2+ upgrade, sold as R12000A)"),
    // Octane and Octane2 (IP30). Sheet: R10000 2.6/2.7/3.1 at 175 and 195 MHz,
    // 1 MiB; 3.4 at 225 and 250 MHz, 1 MiB or (some 250 MHz) 2 MiB; R12000 2.3
    // 300 MHz, 3.5 at 360 and 400 MHz (the 400 MHz module is the R12000A),
    // 2 MiB; R14000 2.3 at 550 MHz, 2.4 at 500 and 600 MHz (the 550 and 600 MHz
    // modules are the R14000A), 2 MiB.
    row(OctaneIp30, R10000, 195, 1, Observed, 0x0927, Inferred,
        "sheet, matrix: Octane R10000 2.6/2.7/3.1, 175 and 195 MHz, 1 MiB (2.7 is the technical report's hinv)"),
    row(OctaneIp30, R10000, 175, 1, Observed, 0x0927, Inferred, "sheet, matrix: as the 195 MHz row"),
    row(OctaneIp30, R10000, 250, 1, Observed, 0x0934, Observed,
        "sheet, matrix: Octane R10000 3.4, 225 and 250 MHz, 1 MiB"),
    row(OctaneIp30, R10000, 225, 1, Observed, 0x0934, Observed, "sheet, matrix: as the 250 MHz row"),
    row(OctaneIp30, R10000, 250, 2, Observed, 0x0934, Observed,
        "sheet, matrix: Octane R10000 3.4, 2 MiB on some 250 MHz parts"),
    row(OctaneIp30, R12000, 360, 2, Observed, 0x0e35, Observed,
        "sheet, matrix: Octane and Octane2 R12000 3.5, 360 MHz, 2 MiB (the owner's Octane2)"),
    row(OctaneIp30, R12000, 300, 2, Observed, 0x0e23, Observed,
        "sheet, matrix: Octane and Octane2 R12000 2.3, 300 MHz, 2 MiB"),
    row(OctaneIp30, R12000, 400, 2, Observed, 0x0e35, Observed,
        "sheet, matrix: R12000 3.5, 400 MHz, 2 MiB; the R12000A module prints this"),
    row(OctaneIp30, R14000, 600, 2, Observed, 0x0f24, Observed,
        "sheet, matrix: Octane R14000 2.4, 600 MHz, 2 MiB; the R14000A module prints this"),
    row(OctaneIp30, R14000, 500, 2, Observed, 0x0f24, Observed,
        "sheet, matrix: Octane R14000 2.4, 500 MHz, 2 MiB (module name not stated)"),
    row(OctaneIp30, R14000, 550, 2, Observed, 0x0f23, Observed,
        "sheet, matrix: Octane R14000 2.3, 550 MHz, 2 MiB (R14000A module)"),
    // Fuel (IP34). Sheet: R14000 2.3, 500 MHz, 2 MiB (SGI called it R14000A).
    row(FuelIp34, R14000, 500, 2, Observed, 0x0f23, Observed,
        "sheet, matrix: Fuel R14000 2.3, 500 MHz, 2 MiB"),
];

/// Silicon variants of `model` whose configurations are not modelled: the
/// part exists but no machine in this tree has a row for it.
pub fn unselectable_reason(model: CpuModel) -> Option<&'static str> {
    match model {
        CpuModel::R16000 => Some(
            "no machine in this tree can take one: the Tezro is not modelled, \
             and the Fuel PROM's Config record is known only for its 500 MHz R14000",
        ),
        _ => None,
    }
}

/// Rows of `profile`.
pub fn for_profile(profile: MachineProfile) -> impl Iterator<Item = &'static CpuVariant> {
    VARIANTS.iter().filter(move |v| v.profile == profile)
}

/// The row a configuration names. `model` is the `[machine] cpu` value; a
/// Fuel accepts `r10000` as the name of its one family. `Ok(None)` means the
/// machine has no rows at all (the Indy and Indigo2 keep their older
/// unconstrained choice of part).
pub fn select(
    profile: MachineProfile, model: CpuModel, mhz: Option<u32>, l2_mib: Option<u32>,
) -> Result<Option<&'static CpuVariant>, String> {
    let family = matches!(model, R10000 | R12000 | R14000 | CpuModel::R16000);
    if !family {
        if mhz.is_some() || l2_mib.is_some() {
            return Err(format!("cpu_mhz and cpu_l2_mib apply only to the R10000 family, not {}", model.label()));
        }
        return Ok(None);
    }
    if let Some(why) = unselectable_reason(model) {
        return Err(format!("{} cannot be selected: {why}", model.label()));
    }
    if for_profile(profile).next().is_none() {
        // The Indy and Indigo2 keep their older choice of R10000 (the IP28
        // aside, none shipped one); the later parts never fitted them.
        if model != R10000 {
            return Err(format!("{} is not a processor the {} shipped with", model.label(), profile.label()));
        }
        if mhz.is_some() || l2_mib.is_some() {
            return Err(format!("cpu_mhz and cpu_l2_mib are not available on the {}", profile.label()));
        }
        return Ok(None);
    }
    let model = if profile == FuelIp34 && model == R10000 { R14000 } else { model };
    let of_part: Vec<_> = for_profile(profile).filter(|v| v.model == model).collect();
    if of_part.is_empty() {
        return Err(format!("{} is not a processor the {} shipped with", model.label(), profile.label()));
    }
    let hit = of_part.iter().copied().find(|v| mhz.map_or(true, |m| v.mhz == m) && l2_mib.map_or(true, |l| v.l2_mib == l));
    hit.map(Some).ok_or_else(|| {
        let shipped: Vec<_> = of_part.iter().map(|v| format!("{} MHz with {} MiB", v.mhz, v.l2_mib)).collect();
        format!(
            "{} at {} MHz with {} MiB of secondary cache is not a configuration the {} shipped with (modelled: {})",
            model.label(),
            mhz.map_or("any".to_string(), |m| m.to_string()),
            l2_mib.map_or("any".to_string(), |l| l.to_string()),
            profile.label(),
            shipped.join(", "),
        )
    })
}

/// Run `$body` with the type alias `$c` naming the shadow cache model of
/// variant `$v`; `$other` runs for a row with no model, which `select`
/// cannot return.
///
/// The cache models differ per machine in their secondary line size (the O2's
/// PROM needs 64 bytes, the others 128) as well as in size, so the machine is
/// part of the key.
#[macro_export]
macro_rules! shadow_for {
    ($v:expr, $c:ident => $body:expr, _ => $other:expr) => {{
        use $crate::cpu::mips_cache_shadow as sc;
        use $crate::config::{CpuModel as M, MachineProfile as P};
        let v: &$crate::cpu_variants::CpuVariant = $v;
        match (v.profile, v.model, v.l2_mib) {
            (P::Indigo2Ip28 | P::OctaneIp30, M::R10000, 1) => { type $c = sc::R10000ShadowCache; $body }
            (P::Indigo2Ip28 | P::OctaneIp30, M::R10000, 2) => { type $c = sc::R10000L2x2ShadowCache; $body }
            (P::O2Ip32, M::R10000, 1) => { type $c = sc::R10000O2ShadowCache; $body }
            (P::O2Ip32, M::R12000, 1) => { type $c = sc::R12000O2ShadowCache; $body }
            (P::O2Ip32, M::R12000, 2) => { type $c = sc::R12000O2L2x2ShadowCache; $body }
            (P::OctaneIp30, M::R12000, 2) => { type $c = sc::R12000ShadowCache; $body }
            (P::OctaneIp30, M::R14000, 2) => { type $c = sc::R14000Ip30ShadowCache; $body }
            (P::FuelIp34, M::R14000, 2) => { type $c = sc::R14000ShadowCache; $body }
            _ => $other,
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::mips_cache_v2::{CpuModel as _, MipsCache};

    /// Identity facts of the cache model a row builds.
    fn built(v: &CpuVariant) -> Option<(u32, usize, usize, &'static str)> {
        shadow_for!(v, C => Some((<C as crate::cpu::mips_cache_v2::CpuModel>::PRID, <C as MipsCache>::L2_SIZE,
                                  <C as MipsCache>::L2_LINE, <C as crate::cpu::mips_cache_v2::CpuModel>::NAME)), _ => None)
    }

    #[test]
    fn every_row_builds_the_cache_it_names() {
        for v in VARIANTS {
            let (prid, l2, _line, name) = built(v).unwrap_or_else(|| panic!("{v:?} has no cache model"));
            assert_eq!(l2, v.l2_bytes(), "{v:?}: secondary cache size");
            // The machine applies the row's PRId over the model's default
            // (a 195 MHz Octane part reports revision 2.7, the model's is 2.5).
            assert_eq!(prid >> 8, v.prid >> 8, "{v:?}: implementation field");
            assert!(name == "shadow" || name == format!("{}", v.model.label().trim_start_matches("MIPS ")), "{name}");
        }
    }

    /// What a guest reads on the processor a row builds: CP0 PRId (as the
    /// machine applies it), Config's SS field (512 KiB << SS) and CP1 FIR.
    fn guest_view(v: &CpuVariant) -> (u32, u32, u32) {
        // An executor is far larger than a test thread's stack.
        let v = *v;
        std::thread::Builder::new().stack_size(64 << 20).spawn(move || guest_view_here(&v)).unwrap().join().unwrap()
    }

    fn guest_view_here(v: &CpuVariant) -> (u32, u32, u32) {
        use crate::cpu::mips_exec::{MipsCpuConfig, MipsExecutor};
        use crate::cpu::mips_tlb::MipsTlb;
        use crate::traits::BusDevice;
        use std::sync::Arc;
        shadow_for!(v, C => {
            let mem: Arc<dyn BusDevice> = Arc::new(crate::dev::mem::Memory::new(1));
            let cfg = MipsCpuConfig::for_model::<C>();
            let mut e: MipsExecutor<MipsTlb, C> = MipsExecutor::new(mem, MipsTlb::new(cfg.tlb_entries), &cfg);
            // The Octane sets the row's PRId on top of the model's, as `OctaneMachine` does.
            e.core.reset_prid = v.prid;
            e.core.reset(false);
            (e.core.cp0_prid, (e.core.cp0_config >> 16) & 7, e.core.read_fpu_control(0))
        }, _ => panic!("{v:?} has no cache model"))
    }

    #[test]
    fn a_guest_reads_the_rows_prid_and_secondary_cache_size() {
        for v in VARIANTS {
            let (prid, ss, fir) = guest_view(v);
            assert_eq!(prid, v.prid, "{v:?}: PRId");
            assert_eq!(512 << 10 << ss, v.l2_bytes(), "{v:?}: Config SS {ss} names the wrong size");
            // The sheet: every family part reads FIR 0x0900 (R10000 implementation
            // 0x09), which is why hinv's FPU line is "0.0" on an Octane.
            assert_eq!(fir, 0x0900, "{v:?}: FIR");
        }
    }

    /// The sheet's `cpu-identity` list (name, revision, PRId), and the hinv
    /// naming rule over it: the key `implementation << 8 | major << 4 | minor`
    /// against descending lower bounds, and for implementation 0x0f a major of
    /// 3 or more printing as major minus two.
    const SHEET: &[(&str, &str, u32)] = &[
        ("R10000", "2.4", 0x0924), ("R10000", "2.5", 0x0925), ("R10000", "2.6", 0x0926),
        ("R10000", "2.7", 0x0927), ("R10000", "3.1", 0x0931), ("R10000", "3.4", 0x0934),
        ("R12000", "2.3", 0x0e23), ("R12000", "2.5", 0x0e25), ("R12000", "3.3", 0x0e33),
        ("R12000", "3.5", 0x0e35), ("R14000", "1.4", 0x0f14), ("R14000", "2.3", 0x0f23),
        ("R14000", "2.4", 0x0f24), ("R16000", "2.1", 0x0f41), ("R16000", "2.2", 0x0f42),
        ("R16000", "3.0", 0x0f50),
    ];

    fn hinv_text(prid: u32) -> (&'static str, String) {
        let name = match prid & 0xffff {
            k if k >= 0x0f30 => "R16000",
            k if k >= 0x0f00 => "R14000",
            k if k >= 0x0e00 => "R12000",
            k if k >= 0x0900 => "R10000",
            _ => "older",
        };
        let (major, minor) = ((prid >> 4) & 0xf, prid & 0xf);
        let major = if (prid >> 8) & 0xff == 0x0f && major >= 3 { major - 2 } else { major };
        (name, format!("{major}.{minor}"))
    }

    #[test]
    fn every_rows_prid_is_a_sheet_revision_that_prints_the_rows_part() {
        for (name, rev, prid) in SHEET {
            assert_eq!(hinv_text(*prid), (*name, rev.to_string()), "sheet rule");
        }
        for v in VARIANTS {
            let (name, rev) = hinv_text(v.prid);
            assert_eq!(name, v.model.label().trim_start_matches("MIPS "), "{v:?}");
            assert!(SHEET.iter().any(|(n, r, p)| *n == name && *r == rev && *p == v.prid), "{v:?} is not a sheet revision");
        }
    }

    /// The A modules are plain parts to a guest: SGI's R12000A at 400 MHz prints
    /// R12000 3.5 and its R14000A at 600 MHz prints R14000 2.4 (the sheet,
    /// "What the A parts are").
    #[test]
    fn the_a_modules_print_the_plain_part() {
        let r12 = select(OctaneIp30, R12000, Some(400), None).unwrap().unwrap();
        assert_eq!(hinv_text(r12.prid), ("R12000", "3.5".to_string()));
        let r14 = select(OctaneIp30, R14000, Some(600), None).unwrap().unwrap();
        assert_eq!(hinv_text(r14.prid), ("R14000", "2.4".to_string()));
    }

    /// Index CACHE ops wrap at the cache's own set count: two index addresses
    /// alias exactly L2/2 apart (two ways), and not closer.
    #[test]
    fn secondary_index_ops_wrap_at_the_rows_cache_size() {
        use crate::cpu::mips_cache_v2::{C_ILT, C_IST, CACH_SD};
        use crate::traits::BusDevice;
        use std::sync::Arc;
        for v in VARIANTS {
            let wrap = shadow_for!(v, C => {
                let mem: Arc<dyn BusDevice> = Arc::new(crate::dev::mem::Memory::new(1));
                let c = C::from(mem);
                let span = (<C as MipsCache>::L2_SIZE / 2) as u64;
                c.cache_op(C_IST | CACH_SD, 0x8000_0000, 0x11);
                c.cache_op(C_IST | CACH_SD, 0x8000_0000 + span / 2, 0x22);
                let before_wrap = c.cache_op(C_ILT | CACH_SD, 0x8000_0000, 0);
                c.cache_op(C_IST | CACH_SD, 0x8000_0000 + span, 0x33);
                (before_wrap, c.cache_op(C_ILT | CACH_SD, 0x8000_0000, 0))
            }, _ => panic!("{v:?}"));
            assert_eq!(wrap, (0x11, 0x33), "{v:?}: index aliasing is not at L2 / 2");
        }
    }

    #[test]
    fn the_implementation_field_names_the_part() {
        for v in VARIANTS {
            let imp = match v.model { R10000 => 0x09, R12000 => 0x0e, R14000 => 0x0f, _ => unreachable!() };
            assert_eq!(v.prid >> 8, imp, "{v:?}");
        }
    }

    #[test]
    fn rows_are_unique_and_the_first_is_the_default() {
        for (i, a) in VARIANTS.iter().enumerate() {
            for b in &VARIANTS[i + 1..] {
                assert!((a.profile, a.model, a.mhz, a.l2_mib) != (b.profile, b.model, b.mhz, b.l2_mib), "{a:?}");
            }
        }
        let octane = select(OctaneIp30, R10000, None, None).unwrap().unwrap();
        assert_eq!((octane.mhz, octane.prid), (195, 0x927));
        let o250 = select(OctaneIp30, R10000, Some(250), None).unwrap().unwrap();
        assert_eq!((o250.mhz, o250.l2_mib, o250.prid), (250, 1, 0x934));
        let o250l2 = select(OctaneIp30, R10000, Some(250), Some(2)).unwrap().unwrap();
        assert_eq!((o250l2.l2_mib, o250l2.prid), (2, 0x934));
        // The Octane2's R12000 is the 360 MHz part; the others are named.
        assert_eq!(select(OctaneIp30, R12000, None, None).unwrap().unwrap().mhz, 360);
        assert_eq!(select(OctaneIp30, R14000, None, None).unwrap().unwrap().mhz, 600);
        // The O2's R12000 is the 2.3 revision, the O2+ upgrade the 3.5.
        assert_eq!(select(O2Ip32, R12000, None, None).unwrap().unwrap().prid, 0x0e23);
        assert_eq!(select(O2Ip32, R12000, Some(400), None).unwrap().unwrap().l2_mib, 2);
    }

    #[test]
    fn a_machine_takes_only_what_it_shipped_with() {
        // Part per machine.
        for (profile, parts) in [
            (Indigo2Ip28, &[R10000][..]),
            (O2Ip32, &[R10000, R12000]),
            (OctaneIp30, &[R10000, R12000, R14000]),
            (FuelIp34, &[R14000]),
        ] {
            for m in [R10000, R12000, R14000] {
                let ok = select(profile, m, None, None).is_ok();
                // The Fuel also takes `r10000`, the family's name.
                let expect = parts.contains(&m) || (profile == FuelIp34 && m == R10000);
                assert_eq!(ok, expect, "{m:?} on {profile:?}");
            }
        }
        // Clock and cache must be a pair the machine has.
        assert!(select(OctaneIp30, R12000, Some(360), Some(2)).unwrap().is_some());
        assert!(select(OctaneIp30, R12000, Some(360), Some(1)).is_err(), "no 1 MiB Octane2");
        assert!(select(OctaneIp30, R10000, Some(360), None).is_err(), "360 MHz is the R12000");
        assert!(select(O2Ip32, R12000, Some(300), Some(1)).unwrap().is_some());
        assert!(select(O2Ip32, R12000, Some(300), Some(2)).is_err(), "the 2 MiB O2 part is the 400 MHz one");
        assert!(select(O2Ip32, R12000, Some(360), None).is_err());
        assert!(select(Indigo2Ip28, R10000, Some(250), Some(1)).is_err(), "the IP28's 250 MHz part has 2 MiB");
        assert!(select(Indigo2Ip28, R10000, Some(250), Some(2)).unwrap().is_some());
        assert!(select(OctaneIp30, R14000, Some(550), Some(2)).unwrap().is_some());
        assert!(select(OctaneIp30, R14000, Some(600), Some(4)).is_err());
        assert!(select(FuelIp34, R14000, Some(500), Some(2)).unwrap().is_some());
        assert!(select(FuelIp34, R14000, Some(600), None).is_err(), "the 600 MHz Fuel's Config record is not known");
        let e = select(FuelIp34, R12000, None, None).unwrap_err();
        assert!(e.contains("R12000") && e.contains("shipped"), "{e}");
    }

    #[test]
    fn the_older_machines_and_older_parts_are_unconstrained() {
        use MachineProfile::{Indigo2Ip22, IndyIp24};
        assert_eq!(select(IndyIp24, R10000, None, None).unwrap().map(|v| v.mhz), None);
        assert!(select(IndyIp24, R10000, Some(195), None).is_err(), "keys need a machine with rows");
        assert!(select(Indigo2Ip22, CpuModel::R5000, None, None).unwrap().is_none());
        for m in [R12000, R14000] {
            assert!(select(IndyIp24, m, None, None).unwrap_err().contains("shipped with"));
        }
        assert!(select(O2Ip32, CpuModel::R5000, Some(200), None).is_err(), "R5000 has no variant keys here");
    }

    #[test]
    fn the_r16000_is_not_selectable_on_any_machine() {
        for p in [MachineProfile::IndyIp24, Indigo2Ip28, O2Ip32, OctaneIp30, FuelIp34] {
            let e = select(p, CpuModel::R16000, None, None).unwrap_err();
            assert!(e.contains("R16000") && e.contains("Tezro"), "{p:?}: {e}");
        }
    }
}
