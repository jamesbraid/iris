#!/usr/bin/env python3
"""Tests for check.py on small hand-written disassemblies.

    python3 scripts/hot-path-check/test_check.py

The symbol names are the shapes the demanglers print for iris today (Rust v0,
GNU and LLVM objdump) and for legacy mangling, so a demangler change that
breaks the matching shows up here rather than as a silent pass in CI.
"""

import contextlib
import io
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import check  # noqa: E402

RULES = check.load_rules(os.path.join(check.HERE, "hot-paths.txt"))

EXEC = ("<iris::cpu::mips_exec::MipsExecutor<iris::cpu::mips_tlb::MipsTlb, "
        "iris::cpu::mips_cache_v2::CpuCache<16384, 16, true, 0>>>")
CACHE = ("<iris::cpu::mips_cache_v2::CpuCache<16384, 16, true, 0> as "
         "iris::cpu::mips_cache_v2::MipsCache>")


def func(addr, name, *insns):
    return [f"{addr:016x} <{name}>:\n"] + [f"  {addr + 4 * i:x}:\t{s}\n" for i, s in enumerate(insns)]


# Every hot and anchor rule matched once, nothing wrong.
CLEAN = (
    func(0x1000, EXEC + "::step_int", "mov    %rdi,%rax", "call   2000 <" + EXEC + "::exec_cold>")
    + func(0x1100, EXEC + "::fetch_instr_impl::<true>", "ret")
    + func(0x1200, EXEC + "::read_data_impl::<false, 4>",
           "call   9000 <core::panicking::panic_fmt>")       # panics are allowed
    + func(0x1300, EXEC + "::write_data_impl::<false, 4>", "ret")
    + func(0x1400, EXEC + "::write_data64_masked_impl::<false>", "ret")
    + func(0x1500, "<iris::cpu::mips_cache_v2::CpuCache<16384, 16, true, 0>>::fill_l1i_line", "ret")
    + func(0x1600, "<iris::cpu::mips_cache_shadow::ShadowCache<1, 2> as "
                   "iris::cpu::mips_cache_v2::MipsCache>::cache_op", "ret")
    + func(0x1700, "<iris::cpu::decode_cache::DecodeCache>::fetch_slow::<<iris::cpu::"
                   "mips_cache_shadow::ShadowCache<1, 2> as iris::cpu::mips_cache_v2::"
                   "MipsCache>::fetch::{closure#0}>", "ret")
    # Formatting outside the hot list is fine: this is the cold report.
    + func(0x2000, EXEC + "::exec_cold", "call   a000 <std::io::stdio::_eprint>")
)


def run(lines):
    return check.scan(lines, RULES)


class Check(unittest.TestCase):
    def passes(self, lines):
        with contextlib.redirect_stdout(io.StringIO()):
            return check.report(RULES, *run(lines), "hot-paths.txt")

    def test_clean_binary_passes(self):
        self.assertTrue(self.passes(CLEAN))

    def test_closure_named_after_fetch_is_not_an_outlined_fetch(self):
        _, outlined, _ = run(CLEAN)
        self.assertEqual(outlined, [])

    def test_eprint_in_a_hot_function_fails(self):
        lines = CLEAN + func(0x3000, EXEC + "::write_data_impl::<false, 8>",
                             "test   %al,%al", "call   a000 <std::io::stdio::_eprint>")
        _, _, calls = run(lines)
        self.assertEqual(dict(calls[EXEC + "::write_data_impl::<false, 8>"]),
                         {"std::io::stdio::_eprint": 1})
        self.assertFalse(self.passes(lines))

    def test_tail_jump_and_llvm_and_aarch64_forms_count(self):
        lines = (CLEAN
                 + func(0x3000, EXEC + "::read_data_impl::<false, 1>",
                        "jmp    b000 <alloc::fmt::format::format_inner>")
                 + func(0x3100, EXEC + "::read_data_impl::<false, 2>",
                        "callq\t0xc000 <core::fmt::write>")
                 + func(0x3200, EXEC + "::read_data_impl::<false, 8>",
                        "bl\t0x10000d000 <core::fmt::write+0x10>"))
        _, _, calls = run(lines)
        self.assertEqual(len(calls), 3)

    def test_out_of_line_cache_fetch_fails(self):
        lines = CLEAN + func(0x3000, CACHE + "::fetch", "ret")
        _, outlined, _ = run(lines)
        self.assertEqual(len(outlined), 1)
        self.assertFalse(self.passes(lines))

    def test_formatting_in_an_outlined_fetch_is_reported_too(self):
        lines = CLEAN + func(0x3000, CACHE + "::fetch", "call   a000 <std::io::stdio::_eprint>")
        _, _, calls = run(lines)
        self.assertIn(CACHE + "::fetch", calls)

    def test_renamed_hot_function_fails(self):
        lines = [l.replace("::step_int", "::step_interp") for l in CLEAN]
        hits, _, _ = run(lines)
        self.assertFalse(any("step_int" in r for r, v in hits.items() if v))
        self.assertFalse(self.passes(lines))

    def test_renamed_cache_type_fails_through_its_anchor(self):
        lines = [l.replace("CpuCache", "L1Cache") for l in CLEAN]
        self.assertFalse(self.passes(lines))

    def test_stripped_binary_fails(self):
        self.assertFalse(self.passes([]))

    def test_legacy_mangling(self):
        self.assertEqual(
            check.owner_and_method("iris::cpu::mips_exec::MipsExecutor<T,C>::step_int::h0123456789abcdef"),
            ("MipsExecutor<T,C>", "step_int"))


if __name__ == "__main__":
    unittest.main()
