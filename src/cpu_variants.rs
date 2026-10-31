//! The R10000-family processors each machine shipped with: part, clock and
//! secondary cache size, with the PRId the model reports.
//!
//! One table, [`VARIANTS`], is the whole answer. `MachineConfig::validate`
//! refuses a part, clock or cache size a machine has no row for, each machine
//! takes its Count rate and PRId from its row, and [`shadow_for`] maps a row
//! to the cache model (the L2 size is a const parameter of that model, so
//! a row without an arm there cannot be built).
//!
//! Every field is graded against the origami datasheets at their latest
//! revision on trunk. `Grade::Observed` is an inventory, capture or product
//! catalogue; `Grade::Inferred` is derived from one by a published encoding
//! (the PRId nibbles of the R10000 manual); `Grade::Borrowed` is a value this
//! emulator already used that no datasheet states for that machine.
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
//! The R12000A, R14000A and R16000A are not here. What separates them from
//! the base parts is still being recorded in the origami CPU datasheets, so
//! [`Stepping`] has only `Base`. A part with an A variant gets a `Stepping::A`
//! arm, a row with it, and whatever the datasheet says it changes (PRId,
//! Config reset word, performance counters); nothing reads `stepping` yet
//! except the selector, so adding the arm needs no change in the machines
//! until a behaviour differs.

use crate::config::{CpuModel, MachineProfile};

/// How well a value is established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    /// Printed by a product catalogue, owner inventory or capture.
    Observed,
    /// Derived from an observation by a published encoding.
    Inferred,
    /// Used by this emulator for the machine before; no datasheet states it
    /// for this machine. Replace it when one does.
    Borrowed,
}

/// Silicon variant of a part. Only the base parts are modelled; see the
/// module documentation for where the A parts go.
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
use Grade::{Borrowed, Inferred, Observed};
use MachineProfile::{FuelIp34, Indigo2Ip28, O2Ip32, OctaneIp30};

const fn row(
    profile: MachineProfile, model: CpuModel, mhz: u32, l2_mib: u32, config: Grade,
    prid: u32, prid_grade: Grade, source: &'static str,
) -> CpuVariant {
    CpuVariant { profile, model, stepping: Stepping::Base, mhz, l2_mib, prid, config, prid_grade, source }
}

/// Every shipped base-part configuration this emulator models.
///
/// Waiting for the variant matrix the origami datasheets are collecting, so
/// deliberately absent: the Octane's 175 MHz R10000 (`ip30-classic-clock-public`
/// names the module, not its cache), a Fuel at 600 MHz (`ip34-fuel` names the
/// clock and a 4 MiB option, no inventory pairs them), the Fuel R16000 (one
/// repair capture, no cache size or PRId), the O2's 400 MHz R12000A
/// (`o2-plus-r12000-module`; an A part), every other IP28 clock, and the
/// Tezro, which has no machine in this tree.
pub const VARIANTS: &[CpuVariant] = &[
    row(Indigo2Ip28, R10000, 195, 1, Borrowed, 0x925, Borrowed,
        "existing IP28 model; no origami datasheet records an IP28 processor"),
    row(O2Ip32, R10000, 195, 1, Observed, 0x925, Borrowed,
        "o2-r10000-module (catalogue: 195 MHz, 1 MiB); revision not stated"),
    row(O2Ip32, R12000, 300, 1, Observed, 0x0e35, Borrowed,
        "o2-hardware-findings (SGI July 2000 release); PRId borrowed from ip30-r12000"),
    row(OctaneIp30, R10000, 195, 1, Observed, 0x0927, Inferred,
        "ip30-machine, ip30-r10000 (OCTANE Technical Report; hinv revision 2.7)"),
    row(OctaneIp30, R10000, 250, 1, Observed, 0x0934, Inferred,
        "ip30-machine (published inventory, revision 3.4, PM10250MHZ)"),
    row(OctaneIp30, R12000, 360, 2, Observed, 0x0e35, Inferred,
        "ip30-r12000, ip30-octane2-machine (owner Octane2, revision 3.5)"),
    row(OctaneIp30, R14000, 600, 2, Observed, 0x0f24, Observed,
        "r14000 (IP30 capture: two processors, revision 2.4, printed 00000f24)"),
    row(FuelIp34, R14000, 500, 2, Observed, 0x0f23, Inferred,
        "ip34-fuel (owner's guide 500 MHz, 2 MiB; hinv revision 2.3)"),
];

/// Silicon variants of `model` whose configurations are not modelled: the
/// part exists but no machine in this tree has a row for it.
pub fn unselectable_reason(model: CpuModel) -> Option<&'static str> {
    match model {
        CpuModel::R16000 => Some(
            "no machine in this tree has a recorded R16000 configuration: \
             the Tezro is not modelled, and the one Fuel capture reports no cache size or PRId",
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
            (P::O2Ip32, M::R10000, 1) => { type $c = sc::R10000O2ShadowCache; $body }
            (P::O2Ip32, M::R12000, 1) => { type $c = sc::R12000O2ShadowCache; $body }
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
            // An Octane applies the row's PRId over the model's (a 195 MHz
            // part reports revision 2.7, the model's default is the IP28's).
            if v.profile != OctaneIp30 {
                assert_eq!(prid, v.prid, "{v:?}: PRId");
            }
            assert_eq!(prid >> 8, v.prid >> 8, "{v:?}: implementation field");
            assert!(name == "shadow" || name == format!("{}", v.model.label().trim_start_matches("MIPS ")), "{name}");
        }
    }

    /// What a guest reads from CP0 on the processor a row builds: PRId (as the
    /// machine applies it) and Config's SS field, 512 KiB << SS.
    fn guest_view(v: &CpuVariant) -> (u32, u32) {
        // An executor is far larger than a test thread's stack.
        let v = *v;
        std::thread::Builder::new().stack_size(64 << 20).spawn(move || guest_view_here(&v)).unwrap().join().unwrap()
    }

    fn guest_view_here(v: &CpuVariant) -> (u32, u32) {
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
            (e.core.cp0_prid, (e.core.cp0_config >> 16) & 7)
        }, _ => panic!("{v:?} has no cache model"))
    }

    #[test]
    fn a_guest_reads_the_rows_prid_and_secondary_cache_size() {
        for v in VARIANTS {
            let (prid, ss) = guest_view(v);
            assert_eq!(prid, v.prid, "{v:?}: PRId");
            assert_eq!(512 << 10 << ss, v.l2_bytes(), "{v:?}: Config SS {ss} names the wrong size");
        }
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
        assert!(select(O2Ip32, R12000, Some(360), None).is_err());
        assert!(select(FuelIp34, R14000, Some(500), Some(2)).unwrap().is_some());
        assert!(select(FuelIp34, R14000, Some(600), None).is_err(), "waiting for the matrix");
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
