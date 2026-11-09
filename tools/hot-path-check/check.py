#!/usr/bin/env python3
"""Fail when interpreter code formats text or a must-inline fetch goes out of line.

    tools/hot-path-check/check.py target/release/iris [--rules FILE] [-v]

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
ARITY = {"hot": 1, "cold": 2, "inline": 2, "anchor": 2, "control": 2, "format": 1}


def load_rules(path):
    rules = {k: [] for k in ARITY}
    with open(path) as f:
        for n, line in enumerate(f, 1):
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            kind, *rest = line.split()
            try:
                if ARITY.get(kind) != len(rest):
                    raise ValueError("expected `hot OWNER`, `format CALLEE` or "
                                     "`cold|inline|anchor|control OWNER METHOD`")
                if kind in ("hot", "format"):
                    rules[kind].append(re.compile(rest[0]))
                elif kind == "cold":
                    rules[kind].append((re.compile(rest[0]), re.compile(rest[1])))
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
    indirect  symbol -> count of indirect calls.
    """
    named = defaultdict(list)
    outlined, hot, cold = [], [], []
    calls = defaultdict(lambda: defaultdict(int))
    indirect = defaultdict(int)
    cur = None                        # the function being read, if a rule wants it
    for line in lines:
        m = FUNC_RE.match(line)
        if m:
            name = m.group(2)
            cur = None
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
            hm = hot_method(name, rules["hot"])
            if hm:
                if any(o.search(hm[0]) and mx.fullmatch(hm[1]) for o, mx in rules["cold"]):
                    cold.append(name)
                else:
                    hot.append(name)
                cur = name
            continue
        if cur is not None:
            m = CALL_RE.search(line)
            if m:
                if any(rx.search(m.group(1)) for rx in rules["format"]):
                    calls[cur][m.group(1)] += 1
            elif INDIRECT_RE.search(line):
                indirect[cur] += 1
    return {"named": named, "outlined": outlined, "hot": hot, "cold": cold,
            "calls": calls, "indirect": indirect}


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
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="list the anchors, and the cold symbols with their call counts")
    a = ap.parse_args()
    rules = load_rules(a.rules)
    found = scan(disassemble(find_objdump(), a.binary), rules)
    return 0 if report(rules, found, a.verbose) else 1


if __name__ == "__main__":
    sys.exit(main())
