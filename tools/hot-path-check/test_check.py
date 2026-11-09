#!/usr/bin/env python3
"""Tests for check.py on small hand-written disassemblies.

    cd tools/hot-path-check && python3 -m unittest

The symbol names are the shapes GNU objdump prints for iris today (Rust v0)
and for legacy mangling, so a demangler change that breaks the matching shows
up here rather than as a silent pass in CI. The call forms are copied from
real builds: fat LTO (direct calls), thin LTO and no LTO (calls into std
through the GOT).
"""

import contextlib
import io
import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import check  # noqa: E402

HOT_PATHS = os.path.join(check.HERE, "hot-paths.txt")
# The fixtures use the R4400 cache, which the lightning rules name.
RULES = check.load_rules(HOT_PATHS, "lightning")

EXEC = ("<iris::cpu::mips_exec::MipsExecutor<iris::cpu::mips_tlb::MipsTlb, "
        "iris::cpu::mips_cache_v2::CpuCache<16384, 16, true, 0>>>")
CACHE = ("<iris::cpu::mips_cache_v2::CpuCache<16384, 16, true, 0> as "
         "iris::cpu::mips_cache_v2::MipsCache>")
STDERR_FMT = "<&std::io::stdio::Stderr as core::io::write::Write>::write_fmt"
GOT_CALL = "call   *0x953fc7(%rip)        # 14b12e0 <_DYNAMIC+0x8450>"


def func(addr, name, *insns):
    return [f"{addr:016x} <{name}>:\n"] + [f"  {addr + 4 * i:x}:\t{s}\n" for i, s in enumerate(insns)]


def call(callee):
    return f"call   a000 <{callee}>"


# The cold reporter and the exception handler as fat LTO shows them.
CONTROLS = (
    func(0x1000, EXEC + "::report_bus_error",
         call(EXEC + "::fault_note"), call(STDERR_FMT))
    + func(0x1100, EXEC + "::handle_exception", call(STDERR_FMT))
)

ANCHORS = (
    func(0x1200, "<iris::cpu::mips_cache_v2::CpuCache<16384, 16, true, 0>>::fill_l1i_line", "ret")
    + func(0x1300, "<iris::cpu::mips_cache_shadow::ShadowCache<1, 2>>::index_store_data", "ret")
    + func(0x1400, "<iris::cpu::decode_cache::DecodeCache>::fetch_slow::<<iris::cpu::"
                   "mips_cache_shadow::ShadowCache<1, 2> as iris::cpu::mips_cache_v2::"
                   "MipsCache>::fetch::{closure#0}>", "ret")
)

# A clean fat-LTO build of the fix.
CLEAN = (
    CONTROLS + ANCHORS
    + func(0x2000, EXEC + "::step_int", "mov    %rdi,%rax", call(EXEC + "::report_bus_error"))
    + func(0x2100, EXEC + "::read_data_impl::<false, 4>",
           call(EXEC + "::report_bus_error"),
           call("core::panicking::panic_fmt"))       # panics are allowed
    + func(0x2200, EXEC + "::exec_lwu", call(EXEC + "::read_data_impl::<false, 4>"))
    + func(0x2300, EXEC + "::fault_note", call("alloc::fmt::format::format_inner"))
    + func(0x2400, EXEC + "::debug_read", call("alloc::fmt::format::format_inner"))
    + func(0x2500, EXEC + "::backtrace_from::{closure#0}", call("core::fmt::write"))
    + func(0x2600, EXEC + "::write_data_impl::<false, 8>", call(EXEC + "::report_bus_error"))
)


def scan(lines):
    return check.scan(lines, RULES)


def passes(lines):
    with contextlib.redirect_stdout(io.StringIO()) as out:
        ok = check.report(RULES, scan(lines))
    return ok, out.getvalue()


def bad_symbols(lines):
    found = scan(lines)
    checked = found["hot"] + [s for _, s in found["outlined"]]
    return {s for s in checked if found["calls"].get(s)}


class Formatting(unittest.TestCase):
    def test_clean_build_passes(self):
        ok, out = passes(CLEAN)
        self.assertTrue(ok, out)

    def test_eprint_in_an_instruction_handler_fails(self):
        # The review's first mutant: an eprintln! in exec_lwu's error branch,
        # one function out from the load path.
        lines = CLEAN + func(0x3000, EXEC + "::exec_lwu",
                             "cmp    $0x7fff0001,%eax", call(EXEC + "::fault_note"),
                             call("std::io::stdio::_eprint"))
        self.assertEqual(bad_symbols(lines), {EXEC + "::exec_lwu"})
        self.assertFalse(passes(lines)[0])

    def test_integer_to_string_in_a_load_fails(self):
        # The review's second mutant: `phys_addr.to_string()` in read_data_impl.
        # std specialises ToString for integers, so only the integer formatter
        # and the allocator are called.
        lines = CLEAN + func(0x3000, EXEC + "::read_data_impl::<false, 8>",
                             "call   b000 <alloc::raw_vec::RawVecInner::try_allocate_in>",
                             call("<u64>::_fmt"), "call   *0x1234(%rip)        # 4000 <free@GLIBC_2.2.5>")
        self.assertEqual(bad_symbols(lines), {EXEC + "::read_data_impl::<false, 8>"})
        self.assertFalse(passes(lines)[0])

    def test_formatting_callees(self):
        for callee in ("std::io::stdio::_eprint", "std::io::stdio::_print", STDERR_FMT,
                       "std::io::Write::write_fmt", "alloc::fmt::format::format_inner",
                       "core::fmt::write", "<core::fmt::Formatter>::pad_integral",
                       "<i32>::_fmt", "<usize>::_fmt", "core::fmt::num::imp::fmt_u64",
                       "<u64 as core::fmt::LowerHex>::fmt",
                       "<iris::Foo<1, 2> as core::fmt::Debug>::fmt",
                       "<str as alloc::string::ToString>::to_string",
                       "log::__private_api::log_impl"):
            with self.subTest(callee=callee):
                lines = CLEAN + func(0x3000, EXEC + "::exec_addu", call(callee))
                self.assertEqual(bad_symbols(lines), {EXEC + "::exec_addu"})

    def test_panics_are_not_formatting(self):
        for callee in ("core::panicking::panic_fmt", "core::panicking::panic_bounds_check",
                       "core::option::expect_failed"):
            with self.subTest(callee=callee):
                lines = CLEAN + func(0x3000, EXEC + "::exec_addu", call(callee))
                self.assertEqual(bad_symbols(lines), set())

    def test_tail_jump_and_llvm_and_aarch64_forms_count(self):
        lines = (CLEAN
                 + func(0x3000, EXEC + "::read_data_impl::<false, 1>",
                        "jmp    b000 <alloc::fmt::format::format_inner>")
                 + func(0x3100, EXEC + "::read_data_impl::<false, 2>",
                        "callq\t0xc000 <core::fmt::write>")
                 + func(0x3200, EXEC + "::read_data_impl::<false, 8>",
                        "bl\t0x10000d000 <core::fmt::write+0x10>"))
        self.assertEqual(len(bad_symbols(lines)), 3)


class Coverage(unittest.TestCase):
    def test_closure_inside_a_hot_method_is_checked(self):
        lines = CLEAN + func(0x3000, EXEC + "::step_int::{closure#0}", call("core::fmt::write"))
        self.assertEqual(bad_symbols(lines), {EXEC + "::step_int::{closure#0}"})

    def test_cold_and_debug_methods_may_format(self):
        found = scan(CLEAN)
        self.assertIn(EXEC + "::fault_note", found["cold"])
        self.assertIn(EXEC + "::debug_read", found["cold"])
        self.assertIn(EXEC + "::backtrace_from::{closure#0}", found["cold"])

    def test_trait_methods_of_the_executor_are_checked(self):
        name = "<" + EXEC[1:-1] + " as iris::cpu::Executor>::step"
        lines = CLEAN + func(0x3000, name, call("std::io::stdio::_eprint"))
        self.assertEqual(bad_symbols(lines), {name})

    def test_executor_as_a_generic_argument_is_not_executor_code(self):
        name = "core::ptr::drop_in_place::<" + EXEC[1:-1] + ">"
        self.assertIsNone(check.hot_method(name, RULES["hot"]))
        lines = CLEAN + func(0x3000, name, call("core::fmt::write"))
        self.assertTrue(passes(lines)[0])

    def test_hot_methods_may_be_inlined_away(self):
        # A build where the inliner folded step_int into its caller: nothing
        # requires it to exist, and its code is checked in the handler it
        # landed in. (read_data_impl is different: the forbid rules name it, so
        # a build without it fails instead of passing by finding nothing.)
        keep = [l for l in CLEAN if "step_int" not in l]
        self.assertTrue(passes(keep)[0])
        lines = keep + func(0x3000, EXEC + "::exec_lw", call("std::io::stdio::_eprint"))
        self.assertEqual(bad_symbols(lines), {EXEC + "::exec_lw"})

    def test_legacy_mangling(self):
        name = "iris::cpu::mips_exec::MipsExecutor<T,C>::step_int::h0123456789abcdef"
        self.assertEqual(check.owner_and_method(name), ("MipsExecutor<T,C>", "step_int"))
        self.assertEqual(check.hot_method(name, RULES["hot"]), ("MipsExecutor<T,C>", "step_int"))
        lines = CLEAN + func(0x3000, name, call("std::io::stdio::_eprint"))
        self.assertEqual(bad_symbols(lines), {name})


class Inlining(unittest.TestCase):
    def test_closure_named_after_fetch_is_not_an_outlined_fetch(self):
        self.assertEqual(scan(CLEAN)["outlined"], [])

    def test_out_of_line_cache_fetch_fails(self):
        lines = CLEAN + func(0x3000, CACHE + "::fetch", "ret")
        self.assertEqual(len(scan(lines)["outlined"]), 1)
        self.assertFalse(passes(lines)[0])

    def test_formatting_in_an_outlined_fetch_is_reported_too(self):
        lines = CLEAN + func(0x3000, CACHE + "::fetch", call("std::io::stdio::_eprint"))
        self.assertIn(CACHE + "::fetch", bad_symbols(lines))

    def test_renamed_cache_type_fails_through_its_anchor(self):
        lines = [l.replace("CpuCache", "L1Cache") for l in CLEAN]
        ok, out = passes(lines)
        self.assertFalse(ok)
        self.assertIn("fill_l1i_line", out)


FILL = CACHE.replace(" as iris::cpu::mips_cache_v2::MipsCache", "") + "::fill_l1d_line_raw"
X86 = ["a.out:     file format elf64-x86-64\n"]
PROLOGUE = ("push   %rbp", "push   %r15", "push   %r14", "push   %r12", "push   %rbx",
            "sub    $0x28,%rsp")


def with_step_int(*insns, fmt=X86):
    """CLEAN with its step_int replaced by one that starts with `insns`."""
    keep = [l for l in CLEAN if EXEC + "::step_int" not in l]
    return fmt + keep + func(0x4000, EXEC + "::step_int", *insns)


class Forbidden(unittest.TestCase):
    def test_clean_build_passes(self):
        found = scan(CLEAN)
        self.assertEqual(len(found["callers"]), 2)
        self.assertEqual(len(found["forbidden"]), 0)

    def test_outlined_cache_fill_in_a_load_or_store_fails(self):
        # 73a1df35 stopped inlining fill_l1d_line_raw into these two.
        for meth in ("read_data_impl::<false, 4>", "write_data_impl::<false, 8>"):
            with self.subTest(meth=meth):
                lines = CLEAN + func(0x3000, EXEC + "::" + meth, call(FILL))
                ok, out = passes(lines)
                self.assertFalse(ok)
                self.assertIn("fill_l1d_line_raw", out)
                self.assertIn("must stay inlined", out)
                self.assertIn(EXEC + "::" + meth, out)

    def test_tail_jump_to_the_fill_counts(self):
        lines = CLEAN + func(0x3000, EXEC + "::read_data_impl::<false, 2>",
                             f"jmp    b000 <{FILL}>")
        self.assertFalse(passes(lines)[0])

    def test_only_the_named_callers_are_watched(self):
        # exec_lw may call the fill (it does not today); the rule is about the
        # two functions whose inlining was lost.
        lines = CLEAN + func(0x3000, EXEC + "::exec_lw", call(FILL))
        self.assertTrue(passes(lines)[0])

    def test_other_callees_of_a_watched_caller_pass(self):
        lines = CLEAN + func(0x3000, EXEC + "::read_data_impl::<false, 1>",
                             call(CACHE.replace(" as iris::cpu::mips_cache_v2::MipsCache", "")
                                  + "::fill_l2_line_for"))
        self.assertTrue(passes(lines)[0])

    def test_missing_caller_fails_instead_of_passing_blind(self):
        lines = [l.replace("read_data_impl", "load_data_impl") for l in CLEAN]
        ok, out = passes(lines)
        self.assertFalse(ok)
        self.assertIn("read_data_impl", out)
        self.assertIn("inlined away", out)


def budget_rules(regs, frame):
    return {**RULES, "budget": [(l, rx, m, f, regs, frame) for l, rx, m, f, _, _ in RULES["budget"]]}


def budgets(lines, rules):
    """(passed, output) of the budget report alone."""
    found = check.scan(lines, rules)
    with contextlib.redirect_stdout(io.StringIO()) as out:
        ok = check.report_budgets(rules, found)
    return ok, out.getvalue()


class Budget(unittest.TestCase):
    def test_within_budget_passes(self):
        ok, out = budgets(with_step_int(*PROLOGUE), budget_rules(5, 40))
        self.assertTrue(ok, out)
        self.assertNotIn("FAIL", out)

    def test_registers_are_counted_in_the_prologue_only(self):
        found = scan(with_step_int(*PROLOGUE[:5], "mov    %rdi,%rbx", "push   %r13"))
        self.assertEqual(found["saves"][0]["regs"], ["rbp", "r15", "r14", "r12", "rbx"])

    def test_a_new_spill_fails_with_the_register_and_the_fix(self):
        lines = with_step_int(*PROLOGUE[:3], "push   %r13", *PROLOGUE[3:])
        ok, out = budgets(lines, budget_rules(5, 40))
        self.assertFalse(ok)
        self.assertIn("saves 6 callee-saved registers (rbp r15 r14 r13 r12 rbx)", out)
        self.assertIn("budget 5 and 40", out)
        self.assertIn("change the budget in hot-paths.txt", out)
        self.assertIn("`6 40`", out)

    def test_a_bigger_frame_fails_when_every_register_is_already_saved(self):
        # All six callee-saved registers are taken on x86-64, so the next
        # spill can only show up as stack the prologue reserves.
        six = ("push   %rbp", "push   %r15", "push   %r14", "push   %r13", "push   %r12",
               "push   %rbx")
        rules = budget_rules(6, 8)
        self.assertTrue(budgets(with_step_int(*six, "push   %rax", "mov    %rdi,%rbx"), rules)[0])
        ok, out = budgets(with_step_int(*six, "sub    $0x28,%rsp", "mov    %rdi,%rbx"), rules)
        self.assertFalse(ok)
        self.assertIn("reserves 40 bytes", out)

    def test_every_copy_is_checked(self):
        other = EXEC.replace("16384", "32768")
        lines = (with_step_int(*PROLOGUE)
                 + func(0x5000, other + "::step_int", *PROLOGUE[:3], "push   %r13", *PROLOGUE[3:]))
        ok, out = budgets(lines, budget_rules(5, 40))
        self.assertFalse(ok)
        self.assertIn(other, out)

    def test_leading_endbr64_and_scratch_pushes_are_skipped(self):
        found = scan(with_step_int("endbr64", "push   %rax", "push   %rbx", "push   %r15",
                                   "mov    %rdi,%rbx"))
        self.assertEqual(found["saves"][0]["regs"], ["rbx", "r15"])
        self.assertEqual(found["saves"][0]["scratch"], 8)

    def test_slack_prints_the_numbers_to_tighten_to(self):
        ok, out = budgets(with_step_int(*PROLOGUE), budget_rules(7, 64))
        self.assertTrue(ok)
        self.assertIn("Tighten it to `5 40`", out)

    def test_missing_function_fails(self):
        ok, out = budgets(X86 + CLEAN[:len(CONTROLS)], budget_rules(5, 40))
        self.assertFalse(ok)
        self.assertIn("step_int", out)

    def test_other_object_formats_are_not_judged(self):
        lines = with_step_int(*PROLOGUE, fmt=["a.out:     file format mach-o-arm64\n"])
        ok, out = budgets(lines, budget_rules(1, 0))
        self.assertTrue(ok)
        self.assertIn("no register budget is recorded for object format mach-o-arm64", out)

    def test_the_recorded_budget_covers_x86_64_linux(self):
        self.assertTrue(any(r[3].search("elf64-x86-64") and r[4] > 0 for r in RULES["budget"]))

    def test_the_full_report_runs_the_budget(self):
        lines = with_step_int(*PROLOGUE[:3], "push   %r13", *PROLOGUE[3:])
        rules = budget_rules(5, 40)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertFalse(check.report(rules, check.scan(lines, rules)))


class Builds(unittest.TestCase):
    def rules(self, text, build):
        with tempfile.NamedTemporaryFile("w", suffix=".txt") as f:
            f.write(text)
            f.flush()
            return check.load_rules(f.name, build)

    def test_a_rule_applies_to_its_own_build_and_plain_rules_to_all(self):
        text = ("hot MipsExecutor<\nforbid@release A a B b\nforbid@lightning C c D d\n"
                "budget@release X x elf 1 2\n")
        for build, forbid, budget in (("release", 1, 1), ("lightning", 1, 0), ("other", 0, 0)):
            with self.subTest(build=build):
                rules = self.rules(text, build)
                self.assertEqual(len(rules["hot"]), 1)
                self.assertEqual(len(rules["forbid"]), forbid)
                self.assertEqual(len(rules["budget"]), budget)

    def test_shipped_release_rules_leave_the_r4400_loads_alone(self):
        # Every build so far calls the fill from the R4400 loads under the
        # release feature set, so only the other caches are watched.
        rules = check.load_rules(HOT_PATHS, "release")
        r4400 = EXEC + "::read_data_impl::<false, 4>"
        other = EXEC.replace("16384", "32768") + "::read_data_impl::<false, 4>"
        lines = (CLEAN + func(0x3000, r4400, call(FILL)) + func(0x3100, other, "ret"))
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertTrue(check.report_forbidden(rules, check.scan(lines, rules)))
        lines = CLEAN + func(0x3100, other, call(FILL))
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertFalse(check.report_forbidden(rules, check.scan(lines, rules)))

    def test_shipped_lightning_rules_watch_the_r4400_loads(self):
        r4400 = EXEC + "::read_data_impl::<false, 4>"
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertFalse(passes(CLEAN + func(0x3000, r4400, call(FILL)))[0])

    def test_unbudgeted_frame_is_not_judged(self):
        rules = {**RULES, "budget": [(l, rx, m, f, 5, None) for l, rx, m, f, _, _ in RULES["budget"]]}
        ok, out = budgets(with_step_int(*PROLOGUE[:5], "sub    $0x400,%rsp"), rules)
        self.assertTrue(ok, out)
        ok, out = budgets(with_step_int(*PROLOGUE[:3], "push   %r13", *PROLOGUE[3:]), rules)
        self.assertFalse(ok)
        self.assertIn("budget 5.", out)
        self.assertIn("`6 -`", out)


class Readable(unittest.TestCase):
    def test_thin_lto_build_is_unreadable(self):
        # Without fat LTO every call into std goes through the GOT and no
        # direct formatting call is visible anywhere, so a check that only
        # looked for bad calls would pass this build.
        lines = (func(0x1000, EXEC + "::report_bus_error", call(EXEC + "::fault_note"), GOT_CALL)
                 + func(0x1100, EXEC + "::handle_exception", GOT_CALL, GOT_CALL)
                 + [l for l in CLEAN[len(CONTROLS):]]
                 + func(0x3000, EXEC + "::read_data_impl::<false, 4>", GOT_CALL))
        ok, out = passes(lines)
        self.assertFalse(ok)
        self.assertIn("unreadable build", out)
        self.assertIn("3 indirect calls", out)
        self.assertIn('lto = "fat"', out)

    def test_no_lto_build_is_unreadable(self):
        # The loop profile inlines the *_impl methods away and calls std
        # through the GOT. The failure names the profile, not the rules.
        lines = (func(0x1000, EXEC + "::report_bus_error", GOT_CALL)
                 + ANCHORS + func(0x2000, EXEC + "::step_int", "ret"))
        ok, out = passes(lines)
        self.assertFalse(ok)
        self.assertIn("unreadable build", out)
        self.assertNotIn("update hot-paths.txt", out)

    def test_one_control_is_enough(self):
        # Binaries older than report_bus_error are read through handle_exception.
        lines = [l for l in CLEAN if "report_bus_error>:" not in l]
        self.assertTrue(passes(lines)[0])

    def test_missing_controls_fail(self):
        lines = [l.replace("report_bus_error", "report_fault")
                  .replace("handle_exception", "take_exception") for l in CLEAN]
        ok, out = passes(lines)
        self.assertFalse(ok)
        self.assertIn("positive control", out)

    def test_stripped_binary_fails(self):
        self.assertFalse(passes([])[0])


class Objdump(unittest.TestCase):
    def test_unusable_objdump_env_fails(self):
        with mock.patch.dict(os.environ, {"OBJDUMP": "/nonexistent/objdump"}):
            with self.assertRaises(SystemExit) as e:
                check.find_objdump()
        self.assertIn("OBJDUMP=/nonexistent/objdump", str(e.exception.code))


if __name__ == "__main__":
    unittest.main()
