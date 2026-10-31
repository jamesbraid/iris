#![allow(dead_code, unused_variables, unused_imports)]

#[cfg(all(feature = "lightning", feature = "developer"))]
compile_error!(
    "features `lightning` and `developer` are mutually exclusive: lightning strips \
     debuggability for speed (fixed jitv2 dispatch, no jitcheck/inline-compile \
     switching, etc.) while developer exists specifically to add it back — pick one"
);

#[cfg(all(feature = "jitv2_lockstep", feature = "lightning"))]
compile_error!(
    "features `jitv2_lockstep` and `lightning` are mutually exclusive: lockstep \
     verifies every JIT instruction against a real interpreter run and breaks \
     into the monitor on divergence, which needs the debuggability lightning \
     strips (it also implies `opcodefusion`, refused below for its own \
     reasons). lockstep already pulls in `developer`, which lightning \
     separately conflicts with — build lockstep without lightning"
);

#[cfg(all(feature = "jitv2_lockstep", feature = "opcodefusion"))]
compile_error!(
    "features `jitv2_lockstep` and `opcodefusion` are mutually exclusive: a \
     fused pair's second instruction is never independently fetched, decoded \
     or dispatched, so lockstep's per-instruction interpreter reference run \
     has nothing to bracket it against — the JIT would execute it while the \
     comparison silently skipped it. Build lockstep without opcodefusion \
     (note `lightning` implies opcodefusion)"
);

#[cfg(all(feature = "jitv2_lockstep", feature = "jitv2_opcodefusion"))]
compile_error!(
    "features `jitv2_lockstep` and `jitv2_opcodefusion` are mutually \
     exclusive, for the same reason as `opcodefusion`: codegen's fusion sites \
     (`try_emit_fused_nop_slot`, `try_emit_fused_lui`) already disable \
     themselves under lockstep because a fused instruction cannot be \
     individually step-bracketed — enabling the feature would be a silent \
     no-op that misrepresents what was built. Build lockstep without it"
);

#[cfg(all(feature = "instr_stats", feature = "jitv2"))]
compile_error!(
    "features `instr_stats` and `jitv2` together are meaningless: instr_stats' \
     execution counters are interpreter-path only (see its own Cargo.toml doc \
     comment) and a jitv2 build routes the vast majority of instructions \
     through compiled code, bypassing them entirely — the resulting counts \
     would silently undercount almost everything rather than erroring. Build \
     instr_stats without jitv2 to get real interpreter-only counts."
);

#[cfg(feature = "r5ksc_triton")]
compile_error!(
    "`r5ksc_triton` models the O2's on-die R5000 L2, not a machine IRIS \
     emulates (IRIS targets Indy/Indigo2; an Indy R5000 board has the \
     external R4600SC-style secondary cache instead — that's `r5ksc`, which \
     is ALSO currently broken and separately refused below). It's also \
     unfinished on its own terms: `cargo test --features r5ksc_triton` \
     fails mips_cache_v2::tests::cache_op_index_inv_l1i. Refusing to build \
     rather than silently shipping a broken cache model for a machine this \
     emulator doesn't target. See src/cpu/mips_cache_v2.rs and \
     clanker/cpu/r5k-l1i-cache-bugs.md."
);

#[cfg(feature = "r5ksc")]
compile_error!(
    "`r5ksc` (external R4600SC-style secondary cache — what a real Indy \
     R5000 board has) does not currently work: `cargo test --features \
     r5ksc` fails mips_cache_v2's L1I tests. There is currently no working \
     R5000 secondary-cache configuration. Refusing to build rather than silently \
     shipping a broken cache model. See src/cpu/mips_cache_v2.rs and \
     clanker/cpu/r5k-l1i-cache-bugs.md. The R5000 without a secondary \
     cache (`--cpu r5000`) is unaffected."
);

/// The version this binary was built as: `RELEASE_VERSION` from the
/// distribution build (`nightly.YYYY.MM.DD` or `X.Y.Z`, see
/// scripts/dist/version.sh), or the crate version for a plain `cargo build`.
/// Cargo tracks `option_env!`, so changing it rebuilds this crate.
pub const VERSION: &str = match option_env!("RELEASE_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

/// Compile-time feature flags exposed for tooling (e.g. iris-gui) so it can
/// surface "rebuild with ..." hints without duplicating the cargo feature set.
pub mod build_features {
    pub const PCAP:      bool = cfg!(feature = "pcap");
    pub const JITV2:     bool = cfg!(feature = "jitv2");
    pub const REX_JIT:   bool = cfg!(feature = "rex-jit");
    pub const GR4_JIT:   bool = cfg!(feature = "gr4-jit");
    pub const GR2_JIT:   bool = cfg!(feature = "gr2-jit");
    /// Lightning build strips breakpoint checks and the traceback buffer
    /// from the MIPS executor hot path. Interactive debugging (GDB stub,
    /// monitor breakpoints) is non-functional in this build.
    pub const LIGHTNING: bool = cfg!(feature = "lightning");
    pub const IDLE_PAUSE: bool = cfg!(feature = "idle-pause");
    /// Host services for IRIX programs (private syscalls 3000-3009,
    /// `iris-hostcall`). No per-machine config — a guest either gets the trap
    /// or doesn't, decided at build time.
    pub const HOSTCALL: bool = cfg!(feature = "hostcall");
    /// Host OpenGL for IRIX programs over host call 3000 (`iris-hostgl`).
    /// Implies `hostcall`. macOS (CGL) and Linux (EGL) have backends —
    /// `hostgl` can be built elsewhere, but `register()` then has nothing to
    /// register, so this is `false` there even in a `--features hostgl` build.
    pub const HOSTGL: bool = cfg!(feature = "hostgl") && cfg!(any(target_os = "macos", target_os = "linux"));
    // There is deliberately no `CPU` constant here any more. The emulated CPU
    // stopped being a build-time property: all three CPU/cache models are
    // monomorphised into every binary and `Machine::new` picks between them
    // from `cfg.machine.cpu`. A constant derived from cargo features could only
    // report how the binary was compiled, which is no longer the same question
    // as which CPU is running — and it was being displayed to users as though
    // it were. Ask the config (`MachineConfig::machine.cpu`), or the guest,
    // which reads PRId.

    /// Every compile-time flag this binary was built with, in a fixed order.
    ///
    /// Execution-engine flags come first: these change what the guest sees,
    /// not just how fast it sees it, and a benchmark result is meaningless
    /// without them. The CPU model is not here — it is a runtime setting.
    ///
    /// In the library rather than in `main.rs` because a saved benchmark result
    /// records this list, and the in-process runner has no startup banner to
    /// read it back out of.
    pub fn enabled() -> Vec<&'static str> {
        const FEATURES: &[(&str, bool)] = &[
            ("r5ksc", cfg!(feature = "r5ksc")),
            ("r5ksc_triton", cfg!(feature = "r5ksc_triton")),
            ("jitv2", cfg!(feature = "jitv2")),
            ("jitv2_opcodefusion", cfg!(feature = "jitv2_opcodefusion")),
            ("opcodefusion", cfg!(feature = "opcodefusion")),
            ("idle-pause", cfg!(feature = "idle-pause")),
            ("rex-jit", cfg!(feature = "rex-jit")),
            ("gr4-jit", cfg!(feature = "gr4-jit")),
            ("gr2-jit", cfg!(feature = "gr2-jit")),
            ("lightning", cfg!(feature = "lightning")),
            ("hostcall", cfg!(feature = "hostcall")),
            ("hostgl", cfg!(feature = "hostgl")),
            ("tcache", cfg!(feature = "tcache")),
            ("tlbvmap", cfg!(feature = "tlbvmap")),
            ("tlbstats", cfg!(feature = "tlbstats")),
            ("tlbcheck", cfg!(feature = "tlbcheck")),
            ("instr_stats", cfg!(feature = "instr_stats")),
            ("pcap", cfg!(feature = "pcap")),
            ("ci_clock", cfg!(feature = "ci_clock")),
            ("developer", cfg!(feature = "developer")),
            ("developer_ip7", cfg!(feature = "developer_ip7")),
            ("debug_cache", cfg!(feature = "debug_cache")),
            ("jitv2_lockstep", cfg!(feature = "jitv2_lockstep")),
            ("jitv2_smc_check", cfg!(feature = "jitv2_smc_check")),
            ("fetchverify", cfg!(feature = "fetchverify")),
            ("j2wp", cfg!(feature = "j2wp")),
            ("llstats", cfg!(feature = "llstats")),
            ("perfetto", cfg!(feature = "perfetto")),
        ];
        FEATURES.iter().filter(|(_, e)| *e).map(|(n, _)| *n).collect()
    }

    /// `enabled()` as the emulator prints it at startup.
    pub fn banner() -> String {
        let on = enabled();
        if on.is_empty() { "(none)".to_string() } else { on.join(" ") }
    }
}

pub use iris_core::config;
pub use iris_core::traits;
pub use iris_core::devlog;
pub use iris_dev::prombin;
pub use iris_dev::prombini2;
pub use iris_dev::prombinip28;
pub use iris_core::ppmem;
pub mod machine;
pub mod workstation;
/// The SGI O2 (IP32): its bus and devices -- see `docs/o2-ip32.md` (and, for how it was built, `clanker/devices/ip32-o2-bringup-log.md`).
/// Not yet wired into `Physical`; `ip32_machine` drives the CPU against it.
pub mod ip32;
pub use iris_dev::ip32_map;
pub mod ip32_screen;
pub mod ip32_machine;
/// The SGI Octane (IP30): its bus and machine -- see `docs/Octane/ip30-address-map.md`.
pub mod ip30;
pub use iris_core::ip30_map;
pub mod ip30_machine;
pub mod machine_state;
pub use iris_core::platform;
pub use iris_core::physical;
pub mod ip22_bus;
pub use iris_core::ip22_map;
pub mod monitor;
pub use iris_core::locks;
pub use iris_dev::net;
pub mod bench_report;
pub mod benchsuite;
pub mod bench_runner;
pub use iris_dev::cow_disk;
pub use iris_dev::chd_disk;
pub use iris_dev::block_node;
pub use iris_dev::scsi;
pub use iris_dev::scsi_monitor;
pub use iris_core::irq;
pub use iris_dev::host_audio;
pub use iris_dev::pci;
pub mod ui;
pub use iris_cpu::cpu;
pub mod dev {
    pub use iris_dev::dev::*;
    pub use iris_gfx::dev::*;
}
pub use iris_core::gfifo;
pub use iris_core::trace_writer;
pub use iris_gfx::gfx_display;
pub mod hostinput;
pub mod keysym;
pub mod vnc;
pub use iris_gfx::compositor;
pub mod gl_compositor;
pub mod headless_gl;
pub use iris_gfx::debug_overlay;
pub use iris_gfx::disp;
pub use iris_core::heartbeat;
pub use iris_core::screenshot;
pub use iris_core::exp;
pub use iris_cpu::gdb_stub;
pub use iris_core::snapshot;
pub use iris_core::sgi_vh;
pub use iris_core::elf;
pub use iris_core::chunk_store;
pub mod validate;
pub mod registry;
pub use iris_core::thread_affinity;
pub mod perf_monitor;
pub mod ci;
pub use iris_core::hptimer;
pub use iris_core::hptimer_tests;
pub use iris_gfx::vga_font;
pub use iris_dev::video_source;
pub use iris_dev::ultra_proto;
pub mod crash_diag;

pub mod hwwatch;
#[macro_use]
extern crate iris_core;
pub use iris_core::{PfBits, PfLevel, PfValue};
pub use iris_core::{dlog, dlog_unconditional, dlog_dev, pf_instant, pf_begin, pf_end, pf_counter, pf_scope, pf_level, pf_bits, pf_value};
#[cfg(feature = "perfetto")]
pub use iris_core::perfetto;

#[cfg(test)]
mod platform_profile_tests;
#[cfg(test)]
pub use iris_core::test_board;
