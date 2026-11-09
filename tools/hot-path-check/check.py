#!/usr/bin/env python3
"""Fail when interpreter code formats text or a must-inline fetch goes out of line.

    tools/hot-path-check/check.py target/release/iris [--build BUILD] [--rules FILE] [-v]

Disassembles a `lightning` release binary built with upstream's release
profile and applies the rules in hot-paths.txt beside this script:

  * no direct call or tail jump to a formatting function (`_eprint`,
    `fmt::format`, `fmt::write`, `<u64>::_fmt`, ...) from any function that
    belongs to a `MipsExecutor` method, closures included, except the short
    list of cold and debug methods in hot-paths.txt. Code the inliner moves
    from one method into another is checked wherever it lands, and a new
    instruction handler is covered without a rule edit;
  * no out-of-line symbol for a fetch that must be inlined (`CpuCache`,
    `ShadowCache` and `DecodeCache` `fetch`);
  * `forbid` rules: a named hot function must not call a named helper. The
    helper is correct; the call is the regression. `read_data_impl` and
    `write_data_impl` must not call `CpuCache::fill_l1d_line_raw`, which
    73a1df35 stopped inlining into the R4400 cache's copies (a call, a
    prologue and argument set-up on every cached load and store). Which
    copies hold depends on the build, so a rule can be written `forbid@BUILD`
    and checked with `--build BUILD`; hot-paths.txt has the measurements.
    The caller must exist, so a rename fails instead of passing by finding
    nothing;
  * `budget` rules: the prologue of a named function (`step_int`) may push
    at most N callee-saved registers and reserve at most M other bytes of
    stack, per object format. 73a1df35 also made `step_int` save one more
    register (`push r12` on every interpreted instruction). A new spill fails
    and prints the line to change if it is deliberate. See the note on
    budgets below;
  * each anchor exists. Anchors are `#[inline(never)]` functions in the types
    the inline rules name, so renaming a type fails instead of letting an
    inline rule pass by finding nothing;
  * a positive control shows at least one formatting call. The controls are
    executor methods that print by design: `report_bus_error`, which is
    `#[cold] #[inline(never)]`, and `handle_exception` for binaries older
    than it. A binary in which none shows a call is one this script cannot
    read, and the run fails instead of passing blind.

Why a disassembly check: aded5989 put `eprintln!` reports with a formatted
String into four bus-error branches of the load, store and fetch paths. None
of them ran on a good access, but the formatting code compiled into the hot
functions and the lightning interpreter lost 8% (suite 92.1 -> 84.9 MIPS).
Host instruction counts did not move; cycles and branch misses did. Moving
each report into its own `#[cold] #[inline(never)]` method takes the calls out
of the hot functions, which is what this check sees.

Budgets: x86-64 has six callee-saved registers and the release build's
`step_int` already pushes all six, so the register count can no longer rise
there; the next spill shows up as stack the prologue reserves (`push %rax`
today, a `sub $n,%rsp` after a spill). The release budget is therefore a
pair. In a `lightning,gpujit` build the register count moves (four at
73a1df35^, five after) and the frame size is noise (136, 128, 112, 16 bytes
over four revisions, no register change), so only the registers are
budgeted there. A budget is an upper bound over every copy of the function.
It is recorded per build (`budget@release`, `budget@lightning`), is not
portable to other feature sets, and a new nightly can move it without a
source change. Only the x86-64 `push` forms are read. For any other object
format the budget rules are skipped with a note.

What it does not check:
  * speed. A layout or inlining regression with no formatting call in it
    passes, and so does a helper outside `MipsExecutor` that formats;
  * allocation without formatting, such as `"x".to_string()`, which inlines
    to an allocator call and a copy;
  * builds without fat LTO. Calls into std then go through the GOT
    (`call *...(%rip)`), no formatting call is visible, and the positive
    control fails the run as unreadable;
  * non-`lightning` builds. There `step_int` carries the breakpoint and
    monitor paths, which format by design, so a default build always fails.

The result is deterministic for a given compiler. The toolchain is an
unpinned nightly, so an inlining change can still move code into or out of
a cold method without a source change.

Needs the symbol table. The release profile keeps it (`debug = 1`, no
`strip`). Disassembler: $OBJDUMP if set (it must exist), else `objdump`, else
`llvm-objdump`.
  * Linux ELF: GNU objdump 2.36 or later (Rust v0 demangling). Checked with
    GNU objdump only; llvm-objdump output has not been compared.
  * macOS Mach-O: Xcode's `objdump` is llvm-objdump. The AArch64 `bl`/`b`
    forms are matched. Not yet run on a Mach-O binary.
  * Windows (MSVC): the symbols are in the PDB, not the executable, so this
    script cannot name functions there.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))

# `0000000000a417a0 <name>:`, from GNU and LLVM objdump alike.
FUNC_RE = re.compile(r"^([0-9a-f]+) <(.*)>:$")
# A direct call or tail jump with a symbolic target, x86-64 or AArch64:
#   call   a12340 <std::io::stdio::_eprint>          GNU
#   callq  0xa12340 <std::io::stdio::_eprint>        LLVM
#   bl     0x100012340 <core::fmt::write+0x10>       AArch64
CALL_RE = re.compile(
    r"\t(?:call[lq]?|jmp[lq]?|bl|b)\s+(?:0x)?[0-9a-f]+\s+<(.+?)(?:\+0x[0-9a-f]+)?>\s*$")
# An indirect call, the form a call into std takes without fat LTO:
#   call   *0x953fc7(%rip)        # 14b12e0 <_DYNAMIC+0x8450>
INDIRECT_RE = re.compile(r"\tcall[lq]?\s+\*")

# rule kind -> number of fields after it
ARITY = {"hot": 1, "cold": 2, "inline": 2, "anchor": 2, "control": 2, "format": 1,
         "forbid": 4, "budget": 5}
# `file format elf64-x86-64`, in the header of GNU and LLVM objdump alike.
FORMAT_RE = re.compile(r"file format (\S+)")
# An instruction line: `  2000:\tpush   %rbp`.
INSN_RE = re.compile(r"^\s*[0-9a-f]+:\t(\S+)\s*(.*?)\s*(?:#.*)?$")
# Registers the x86-64 System V ABI makes the callee save.
CALLEE_SAVED = ("rbx", "rbp", "r12", "r13", "r14", "r15")


def load_rules(path, build="release"):
    """Read the rules that apply to `build`. A rule kind written `kind@NAME`
    applies to that build only; a plain `kind` applies to every build."""
    rules = {k: [] for k in ARITY}
    with open(path) as f:
        for n, line in enumerate(f, 1):
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            kind, *rest = line.split()
            kind, _, only = kind.partition("@")
            if only and only != build:
                continue
            try:
                if ARITY.get(kind) != len(rest):
                    raise ValueError("expected `hot OWNER`, `format CALLEE`, "
                                     "`cold|inline|anchor|control OWNER METHOD`, "
                                     "`forbid OWNER METHOD CALLEE-OWNER CALLEE-METHOD` or "
                                     "`budget OWNER METHOD FORMAT PUSHES FRAME-BYTES`")
                if kind in ("hot", "format"):
                    rules[kind].append(re.compile(rest[0]))
                elif kind == "cold":
                    rules[kind].append((re.compile(rest[0]), re.compile(rest[1])))
                elif kind == "forbid":
                    rules[kind].append((line, re.compile(rest[0]), rest[1],
                                        re.compile(rest[2]), rest[3]))
                elif kind == "budget":
                    rules[kind].append((line, re.compile(rest[0]), rest[1],
                                        re.compile(rest[2]), int(rest[3]),
                                        None if rest[4] == "-" else int(rest[4])))
                else:
                    rules[kind].append((line, re.compile(rest[0]), rest[1]))
            except (ValueError, re.error) as e:
                sys.exit(f"{path}:{n}: {e}")
    return rules


def split_path(name):
    """Split a demangled path at top-level `::`, keeping `<...>` groups whole."""
    segs, depth, start, i = [], 0, 0, 0
    while i < len(name):
        c = name[i]
        if c == "<":
            depth += 1
        elif c == ">" and depth > 0 and name[i - 1] != "-":   # `->` in fn types
            depth -= 1
        elif depth == 0 and name.startswith("::", i):
            segs.append(name[start:i])
            i += 2
            start = i
            continue
        i += 1
    segs.append(name[start:])
    return segs


def segments(name):
    segs = split_path(name)
    if re.fullmatch(r"h[0-9a-f]{16}", segs[-1]):     # legacy mangling hash
        segs.pop()
    return segs


def ident(seg):
    return seg.split("<", 1)[0]    # legacy: `foo<T>` is one segment


def owner_and_method(name):
    """`<A<..>>::read_data_impl::<false, 1>` -> (`<A<..>>`, `read_data_impl`).

    The last named segment and the one before it. Closures and shims end in
    `{closure#0}` or similar and never match a method name.
    """
    segs = segments(name)
    while len(segs) > 1 and segs[-1].startswith("<"):
        segs.pop()
    if len(segs) < 2:
        return "", segs[-1]
    return segs[-2], ident(segs[-1])


def hot_method(name, owner_res):
    """The (owner, method) of a `hot` type that function `name` belongs to.

    `<Exec<..>>::step_int::{closure#0}` belongs to `step_int`. Only the
    leading `<...>` of a qualified path or a plain path segment can be an
    owner, never generic arguments such as `drop_in_place::<Exec<..>>`.
    Returns None for anything else.
    """
    segs = segments(name)
    for i, seg in enumerate(segs[:-1]):
        if i and seg.startswith("<"):
            continue
        if any(rx.search(seg) for rx in owner_res):
            meth = ident(segs[i + 1])
            return (seg, meth) if meth else None
    return None


def scan(lines, rules):
    """Walk a disassembly. Returns a dict:
    named     rule line -> [symbol] for `anchor` and `control` rules;
    outlined  [(rule line, symbol)] for `inline` rules that found a symbol;
    hot       [symbol] of hot types, checked against the format rules;
    cold      [symbol] of hot types that a `cold` rule lets format;
    calls     symbol -> callee -> count of formatting calls;
    indirect  symbol -> count of indirect calls;
    callers   rule line -> [symbol] for `forbid` rules: the functions it watches;
    forbidden rule line -> symbol -> callee -> count of forbidden calls;
    saves     [{rule, sym, regs, scratch}] for `budget` rules: the callee-saved
              registers pushed at the top of the function, and the bytes of
              other stack the prologue reserves (`push %rax`, `sub $n,%rsp`);
    format    the object file format objdump reports, or None.
    """
    named = defaultdict(list)
    outlined, hot, cold, saves = [], [], [], []
    calls = defaultdict(lambda: defaultdict(int))
    indirect = defaultdict(int)
    callers = defaultdict(list)
    forbidden = defaultdict(lambda: defaultdict(lambda: defaultdict(int)))
    fmt = None
    cur = None                        # the function being read, if a rule wants it
    watch = []                        # `forbid` rules whose caller this function is
    pro = None                        # (rule line, symbol, regs) while in a prologue
    for line in lines:
        m = FUNC_RE.match(line)
        if m:
            name = m.group(2)
            cur = None
            watch = []
            pro = None
            owner, method = owner_and_method(name)
            for kind in ("inline", "anchor", "control"):
                for rule, rx, meth in rules[kind]:
                    if method != meth or not rx.search(owner):
                        continue
                    if kind == "inline":
                        outlined.append((rule, name))
                    else:
                        named[rule].append(name)
                    if kind != "anchor":
                        cur = name
            for rule, rx, meth, crx, cmeth in rules["forbid"]:
                if method == meth and rx.search(owner):
                    callers[rule].append(name)
                    watch.append((rule, crx, cmeth))
            for rule, rx, meth, _, _, _ in rules["budget"]:
                if method == meth and rx.search(owner):
                    pro = {"rule": rule, "sym": name, "regs": [], "scratch": 0}
                    saves.append(pro)
                    break
            hm = hot_method(name, rules["hot"])
            if hm:
                if any(o.search(hm[0]) and mx.fullmatch(hm[1]) for o, mx in rules["cold"]):
                    cold.append(name)
                else:
                    hot.append(name)
                cur = name
            continue
        if fmt is None:
            m = FORMAT_RE.search(line)
            if m:
                fmt = m.group(1)
        if pro is not None:
            m = INSN_RE.match(line)
            if m:
                op, arg = m.group(1), m.group(2)
                if op.startswith("push"):
                    reg = arg.lstrip("%")
                    if reg in CALLEE_SAVED:
                        pro["regs"].append(reg)
                    else:
                        pro["scratch"] += 8       # `push %rax` reserves a slot
                elif op.startswith("sub") and arg.endswith(",%rsp"):
                    imm = re.match(r"\$(0x[0-9a-f]+|\d+),", arg)
                    pro["scratch"] += int(imm.group(1), 0) if imm else 0
                    pro = None                    # the frame is allocated
                elif op != "endbr64":
                    pro = None    # the first other instruction ends the prologue
        if cur is not None or watch:
            m = CALL_RE.search(line)
            if m:
                if cur is not None and any(rx.search(m.group(1)) for rx in rules["format"]):
                    calls[cur][m.group(1)] += 1
                if watch:
                    cowner, cmethod = owner_and_method(m.group(1))
                    for rule, crx, cmeth in watch:
                        if cmethod == cmeth and crx.search(cowner):
                            forbidden[rule][name][m.group(1)] += 1
            elif cur is not None and INDIRECT_RE.search(line):
                indirect[cur] += 1
    return {"named": named, "outlined": outlined, "hot": hot, "cold": cold,
            "calls": calls, "indirect": indirect, "callers": callers,
            "forbidden": forbidden, "saves": saves, "format": fmt}


def readable(rules, found):
    """Check the positive controls. Print why and return False when the
    disassembly cannot show formatting calls.

    One control showing a formatting call is enough: each prints by design,
    and a binary that shows one call can show them all. Several are listed
    so that a binary older than one of them can still be read.
    """
    present = [(rule, meth, found["named"][rule]) for rule, _, meth in rules["control"]
               if found["named"][rule]]
    names = ", ".join(meth for _, _, meth in rules["control"])
    if not present:
        print(f"FAIL: no symbol matches any positive control ({names}). "
              f"Is the binary stripped, or were they renamed?")
        return False
    if any(found["calls"].get(s) for _, _, syms in present for s in syms):
        return True
    nsym = sum(len(syms) for _, _, syms in present)
    ind = sum(found["indirect"][s] for _, _, syms in present for s in syms)
    msg = (f"FAIL: unreadable build. {', '.join(m for _, m, _ in present)} print by "
           f"design, but none of their {nsym} copies shows a direct formatting call")
    if ind:
        msg += (f"; they make {ind} indirect calls (`call *...(%rip)`), which is "
                f"how a build without fat LTO reaches std through the GOT")
    print(msg + ".\n      Check a build with upstream's release profile "
          "(lto = \"fat\", codegen-units = 1), which plain `cargo build --release` uses.")
    return False


def report_forbidden(rules, found, verbose=False):
    """Print the `forbid` rules' result; return True when none is broken."""
    ok = True
    for rule, _, meth, _, cmeth in rules["forbid"]:
        syms = found["callers"][rule]
        calls = found["forbidden"][rule]
        print(f"{len(syms):4d} x {meth} (must not call {cmeth})")
        if not syms:
            print(f"FAIL: no symbol matches the caller in `{rule}`. It was renamed or "
                  f"inlined away, so the rule would pass by finding nothing: update "
                  f"hot-paths.txt to match.")
            ok = False
        for sym in sorted(calls):
            what = ", ".join(f"{n} x {c}" for c, n in sorted(calls[sym].items()))
            print(f"FAIL: {sym} calls {cmeth}, which must stay inlined ({rule}):\n"
                  f"      {what}")
            ok = False
    return ok


def report_budgets(rules, found, verbose=False):
    """Print the `budget` rules' result; return True when none is exceeded.

    Two numbers per function, each an upper bound: the callee-saved registers
    it pushes, and the other stack its prologue reserves. x86-64 has six
    callee-saved registers, so a function that already pushes all six can
    only grow in the second number. A function under budget passes and
    prints the number to tighten the budget to, so that a later spill cannot
    hide in the slack.
    """
    ok = True
    fmt = found["format"] or "unknown"
    for rule, _, meth, frx, max_regs, max_frame in rules["budget"]:
        if not frx.search(fmt):
            continue
        syms = [p for p in found["saves"] if p["rule"] == rule]
        if not syms:
            print(f"FAIL: no symbol matches `{rule}`. It was renamed or inlined away: "
                  f"update hot-paths.txt to match.")
            ok = False
            continue
        regs = max(len(p["regs"]) for p in syms)
        frame = max(p["scratch"] for p in syms)
        limit = f"{max_regs} and {max_frame}" if max_frame is not None else f"{max_regs}"
        print(f"{len(syms):4d} x {meth} (saves at most {regs} callee-saved registers and "
              f"reserves at most {frame} bytes, budget {limit})")
        over = False
        for p in syms:
            if verbose:
                print(f"         {len(p['regs'])}, {p['scratch']} bytes: "
                      f"{' '.join(p['regs'])}  {p['sym']}")
            if len(p["regs"]) > max_regs or (max_frame is not None and p["scratch"] > max_frame):
                print(f"FAIL: {p['sym']} saves {len(p['regs'])} callee-saved registers "
                      f"({' '.join(p['regs'])}) and reserves {p['scratch']} bytes of "
                      f"stack, budget {limit}.\n"
                      f"      Something kept one more value live across the function, and "
                      f"that costs a push and a pop (or a stack slot) on every call.\n"
                      f"      If it is deliberate, change the budget in hot-paths.txt "
                      f"(`{rule}`) to `{len(p['regs'])} {p['scratch'] if max_frame is not None else '-'}` "
                      f"and say why in the commit.")
                ok, over = False, True
        if not over and (regs < max_regs or (max_frame is not None and frame < max_frame)):
            print(f"note: under budget. Tighten it to `{regs} {frame if max_frame is not None else '-'}` in hot-paths.txt "
                  f"so the next spill fails.")
    if rules["budget"] and not any(r[3].search(fmt) for r in rules["budget"]):
        print(f"note: no register budget is recorded for object format {fmt}; "
              f"the budget rules were not applied.")
    return ok


def report(rules, found, verbose=False):
    """Print the result; return True when the binary passes."""
    if not readable(rules, found):
        return False
    ok = True
    for rule, _, meth in rules["control"]:
        syms = found["named"][rule]
        print(f"{len(syms):4d} x {meth} (control, {sum(sum(found['calls'][s].values()) for s in syms)} calls)")
    for rule, _, meth in rules["anchor"]:
        syms = found["named"][rule]
        print(f"{len(syms):4d} x {meth}")
        if verbose:
            for s in syms:
                print(f"         {s}")
        if not syms:
            print(f"FAIL: no symbol matches `{rule}`. It is `#[inline(never)]`, so it "
                  f"was renamed or removed: update hot-paths.txt to match.")
            ok = False
    if not found["hot"]:
        print("FAIL: no function of a `hot` type found. Was the type renamed?")
        ok = False
    for rule, sym in found["outlined"]:
        print(f"FAIL: must be inlined but has its own symbol ({rule}):\n      {sym}")
        ok = False
    checked = found["hot"] + [s for _, s in found["outlined"]]
    bad = {s: found["calls"][s] for s in checked if found["calls"].get(s)}
    if bad:
        ok = False
        print("FAIL: formatting calls in interpreter code outside the cold list:")
    for sym, callees in sorted(bad.items()):
        what = ", ".join(f"{n} x {c}" for c, n in sorted(callees.items()))
        print(f"      {sym}: {what}")
    ok = report_forbidden(rules, found, verbose) and ok
    ok = report_budgets(rules, found, verbose) and ok
    if verbose:
        print("cold symbols, allowed to format:")
        for s in sorted(found["cold"]):
            print(f"      {s}: {sum(found['calls'][s].values())}")
    total = sum(sum(c.values()) for c in bad.values())
    print(f"{'ok' if ok else 'FAIL'}: {total} formatting calls in {len(found['hot'])} "
          f"interpreter symbols ({len(found['cold'])} cold ones allowed), "
          f"{len(found['outlined'])} out-of-line must-inline symbols")
    return ok


def find_objdump():
    env = os.environ.get("OBJDUMP")
    if env:
        path = shutil.which(env)
        if not path:
            sys.exit(f"check.py: OBJDUMP={env} is not an executable file or on PATH")
        return path
    for cand in ("objdump", "llvm-objdump"):
        path = shutil.which(cand)
        if path:
            return path
    sys.exit("check.py: no objdump or llvm-objdump on PATH")


def disassemble(objdump, binary):
    args = [objdump, "-d", "-C", "--no-show-raw-insn", binary]
    p = subprocess.Popen(args, stdout=subprocess.PIPE, text=True, errors="replace")
    yield from p.stdout
    if p.wait() != 0:
        sys.exit(f"check.py: `{' '.join(args)}` failed with status {p.returncode}")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("binary")
    ap.add_argument("--rules", default=os.path.join(HERE, "hot-paths.txt"))
    ap.add_argument("--build", default="release",
                    help="which build the binary is, for rules written `kind@NAME`: "
                         "`release` (CI's feature set, the default) or `lightning` "
                         "(--features lightning,gpujit)")
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="list the anchors, and the cold symbols with their call counts")
    a = ap.parse_args()
    rules = load_rules(a.rules, a.build)
    found = scan(disassemble(find_objdump(), a.binary), rules)
    return 0 if report(rules, found, a.verbose) else 1


if __name__ == "__main__":
    sys.exit(main())
