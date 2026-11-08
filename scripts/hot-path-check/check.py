#!/usr/bin/env python3
"""Fail when the interpreter's hot functions format text or lose their inlining.

    scripts/hot-path-check/check.py target/release/iris [--rules FILE] [-v]

Disassembles a `lightning` release binary and applies the rules in
hot-paths.txt beside this script:

  * no direct call to a formatting function (`_eprint`, `fmt::format`,
    `fmt::write`, ...) from any copy of a hot function;
  * no out-of-line symbol for a function that must be inlined;
  * every `hot` and `anchor` rule matches at least one symbol, so a rename
    fails loudly instead of passing with nothing checked.

Why a disassembly check: aded5989 put `eprintln!` reports with a formatted
String into four bus-error branches of the load, store and fetch paths. None
of them ran on a good access, but the formatting code compiled into the hot
functions and the lightning interpreter lost 8% (suite 92.1 -> 84.9 MIPS).
Host instruction counts did not move; cycles and branch misses did. Moving
each report into its own `#[cold] #[inline(never)]` method takes the calls out
of the hot functions, which is what this check sees.

What it cannot see: a layout regression with no formatting in it, or a call to
a helper that is out of line but not cold and formats inside. It checks for a
cause seen twice; it does not measure speed.

Only `lightning` builds. Without `lightning`, `step_int` carries the
breakpoint and monitor paths, which format by design, so a default build
always fails.

Needs the symbol table. The release profile keeps it (`debug = 1`, no
`strip`). Disassembler: $OBJDUMP, else `objdump`, else `llvm-objdump`.
  * Linux ELF: GNU objdump 2.36 or later (Rust v0 demangling), or
    llvm-objdump; both give the same result.
  * macOS Mach-O: Xcode's `objdump` is llvm-objdump. The AArch64 `bl`/`b`
    forms are matched. Not yet run on a Mach-O binary.
  * Windows (MSVC): the symbols are in the PDB, not the executable, so this
    script cannot name functions there. It would need `llvm-pdbutil` or
    `dumpbin /disasm` with the PDB.
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

KINDS = ("hot", "inline", "anchor")


def load_rules(path):
    rules = {k: [] for k in KINDS + ("format",)}
    with open(path) as f:
        for n, line in enumerate(f, 1):
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            kind, *rest = line.split()
            try:
                if kind in KINDS and len(rest) == 2:
                    rules[kind].append((line, re.compile(rest[0]), rest[1]))
                elif kind == "format" and len(rest) == 1:
                    rules["format"].append(re.compile(rest[0]))
                else:
                    raise ValueError("expected `hot|inline|anchor OWNER METHOD` or `format REGEX`")
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


def owner_and_method(name):
    """`<A<..>>::read_data_impl::<false, 1>` -> (`<A<..>>`, `read_data_impl`).

    Drops a legacy `::h0123..` hash and trailing generic arguments. Closures
    and shims end in `{closure#0}` or similar and never match a method name.
    """
    segs = split_path(name)
    if re.fullmatch(r"h[0-9a-f]{16}", segs[-1]):
        segs.pop()
    while len(segs) > 1 and segs[-1].startswith("<"):
        segs.pop()
    if len(segs) < 2:
        return "", segs[-1]
    return segs[-2], segs[-1].split("<", 1)[0]   # legacy: `foo<T>` is one segment


def scan(lines, rules):
    """Walk a disassembly. Returns (hits, outlined, calls):
    hits      rule line -> [symbol] for `hot` and `anchor` rules;
    outlined  [(rule line, symbol)] for `inline` rules that found a symbol;
    calls     symbol -> callee -> count of formatting calls.
    """
    hits = defaultdict(list)
    outlined = []
    calls = defaultdict(lambda: defaultdict(int))
    cur = None                        # the function being read, if it is hot
    for line in lines:
        m = FUNC_RE.match(line)
        if m:
            name = m.group(2)
            owner, method = owner_and_method(name)
            cur = None
            for kind in KINDS:
                for rule, rx, meth in rules[kind]:
                    if method != meth or not rx.search(owner):
                        continue
                    if kind == "inline":
                        outlined.append((rule, name))
                    else:
                        hits[rule].append(name)
                    if kind != "anchor":
                        cur = name
            continue
        if cur is not None:
            m = CALL_RE.search(line)
            if m and any(rx.search(m.group(1)) for rx in rules["format"]):
                calls[cur][m.group(1)] += 1
    return hits, outlined, calls


def report(rules, hits, outlined, calls, rules_path, verbose=False):
    """Print the result; return True when the binary passes."""
    ok = True
    for rule, _, meth in rules["hot"] + rules["anchor"]:
        print(f"{len(hits[rule]):4d} x {meth}")
        if verbose:
            for s in hits[rule]:
                print(f"         {s}")
        if not hits[rule]:
            print(f"FAIL: no symbol matches `{rule}`. Renamed or inlined? "
                  f"Update {rules_path}.")
            ok = False
    for rule, sym in outlined:
        print(f"FAIL: must be inlined but has its own symbol ({rule}):\n      {sym}")
        ok = False
    if calls:
        ok = False
        print("FAIL: formatting calls in hot functions:")
    for sym, callees in sorted(calls.items()):
        what = ", ".join(f"{n} x {c}" for c, n in sorted(callees.items()))
        print(f"      {sym}: {what}")
    total = sum(sum(c.values()) for c in calls.values())
    nhot = sum(len(hits[r]) for r, _, _ in rules["hot"])
    print(f"{'ok' if ok else 'FAIL'}: {total} formatting calls in {nhot} hot symbols, "
          f"{len(outlined)} out-of-line must-inline symbols")
    return ok


def find_objdump():
    for cand in filter(None, [os.environ.get("OBJDUMP"), "objdump", "llvm-objdump"]):
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
                    help="list every symbol each rule matched")
    a = ap.parse_args()
    rules = load_rules(a.rules)
    found = scan(disassemble(find_objdump(), a.binary), rules)
    return 0 if report(rules, *found, a.rules, a.verbose) else 1


if __name__ == "__main__":
    sys.exit(main())
