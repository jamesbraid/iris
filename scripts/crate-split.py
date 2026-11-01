#!/usr/bin/env python3
"""Split the iris crate into a layered workspace, driven by layers.toml.

    scripts/crate-split.py [--repo DIR] [--dry-run]

Run it on a clean checkout whose module graph already respects the layers
(`cargo test --test layering` passes). It rewrites the tree in place:

  * creates crates/iris-{core,cpu,dev,gfx} with a Cargo.toml and lib.rs each;
  * moves every file under src/ to the crate its module belongs to, keeping the
    path below src/ so `crate::dev::ng1::rex3` stays that path;
  * generates the mod.rs of modules whose children live in several crates
    (dev/, cpu/) and re-exports lower layers at the root of each crate, so
    `crate::traits` and `crate::dlog!` resolve as before;
  * turns src/lib.rs into re-exports of the layers, so `iris::dev::ng1` and the
    other public paths stay where they were;
  * forwards every root feature to every crate and checks that no feature a
    crate reads with cfg(feature = ...) is undeclared there;
  * adds the `test-support` feature and the dev-dependencies the layers'
    tests need, and applies the source patches listed in layers.toml.

It does not use git; commit the result with `git add -A`. It stops, without
writing, if any anchor it relies on has drifted.
"""
import argparse
import os
import re
import shutil
import sys
import tomllib

ap = argparse.ArgumentParser()
ap.add_argument("--repo", default=".")
ap.add_argument("--dry-run", action="store_true")
args = ap.parse_args()
os.chdir(args.repo)

man = tomllib.load(open("layers.toml", "rb"))
MODS = man["modules"]
SPLIT = man["split"]
CRATES = SPLIT["crates"]            # layer -> package
CDIR = SPLIT["dir"]
LOWER = list(CRATES)                # core first
assert LOWER[0] == "core"
pkg_ident = lambda layer: CRATES[layer].replace("-", "_")


def fail(msg):
    sys.exit(f"crate-split: {msg}")


def layer_of(path):
    for n in range(len(path), 0, -1):
        k = "::".join(path[:n])
        if k in MODS:
            return MODS[k]
    return "top"


def rel_module(rel):
    parts = rel.split("/")
    parts[-1] = parts[-1].rsplit(".", 1)[0] if "." in parts[-1] else parts[-1]
    if parts[-1] in ("mod", "lib", "main"):
        parts.pop()
    return parts


# --- plan the moves -------------------------------------------------------

TOP_FILES = {"lib.rs", "main.rs", "coffdump.rs", "iris_ci_main.rs"}
moves = {}  # old rel (under src) -> (layer, new path)
for dp, dn, fn in sorted(os.walk("src")):
    for f in sorted(fn):
        rel = os.path.relpath(os.path.join(dp, f), "src")
        if rel in TOP_FILES or rel.startswith("bin/"):
            continue
        L = layer_of(rel_module(rel))
        if L != "top":
            moves[rel] = L
if os.path.exists(CDIR):
    fail(f"{CDIR}/ exists; run on a clean checkout")

# --- source text edits (in memory, applied before the move) ---------------

edits = {}  # path -> text


def text(path):
    if path not in edits:
        edits[path] = open(path).read()
    return edits[path]


for p in SPLIT.get("patch", []):
    path = os.path.normpath(os.path.join("src", p["file"]))
    t = text(path)
    want = p.get("count", 1)
    got = t.count(p["old"])
    if got != want:
        fail(f"{path}: expected {want} of {p['old']!r}, found {got} ({p['why']})")
    edits[path] = t.replace(p["old"], p["new"])

# Rustdoc examples name the crate: `use iris::dev::ng1::X` becomes iris_gfx.
for rel, L in moves.items():
    if rel.endswith(".rs"):
        path = f"src/{rel}"
        t = text(path)
        t2 = re.sub(r"^(\s*//[/!]\s*use )iris::", lambda m: m.group(1) + pkg_ident(L) + "::", t, flags=re.M)
        if t2 != t:
            edits[path] = t2

# --- parse src/lib.rs into blocks -----------------------------------------

lib = text("src/lib.rs").split("\n")
blocks = []  # dicts: attrs, name, vis, line index range
i = 0
while i < len(lib):
    m = re.match(r"^(pub(?:\([^)]*\))? )?mod ([A-Za-z0-9_]+);$", lib[i])
    if m:
        s = i
        while s > 0 and re.match(r"^\s*(#\[|///)", lib[s - 1]):
            s -= 1
        blocks.append(dict(start=s, end=i, name=m.group(2), pub=bool(m.group(1)), attrs=lib[s:i]))
    i += 1
byname = {b["name"]: b for b in blocks}
for b in blocks:
    b["layer"] = layer_of([b["name"]])

# `pub use <private core mod>::{...};` (the Pf* types) moves with its module.
reexp = []
for k, l in enumerate(lib):
    m = re.match(r"^pub use ([a-z_0-9]+)::\{([^}]*)\};$", l)
    if m and m.group(1) in byname and byname[m.group(1)]["layer"] != "top":
        reexp.append((k, m.group(1), [x.strip() for x in m.group(2).split(",")]))

# macros exported by moved files
macros = {L: [] for L in LOWER}
for rel, L in moves.items():
    if rel.endswith(".rs"):
        for m in re.finditer(r"#\[macro_export\]\s*(?:#\[[^\]]*\]\s*)*macro_rules!\s*(\w+)", text(f"src/{rel}")):
            macros[L].append(m.group(1))

# --- Cargo.toml pieces ----------------------------------------------------

root = open("Cargo.toml").read()
rt = tomllib.loads(root)
FEATURES = list(rt["features"])


def section_span(doc, header):
    m = re.search(r"^\[" + re.escape(header) + r"\]\n", doc, re.M)
    if not m:
        return None
    n = re.search(r"^\[", doc[m.end():], re.M)
    return m.end(), m.end() + (n.start() if n else len(doc) - m.end())


def section(doc, header):
    sp = section_span(doc, header)
    return doc[sp[0]:sp[1]] if sp else ""


feat_text = section(root, "features")
deps_text = section(root, "dependencies")
tdeps = re.findall(r"^\[(target\.[^\]]+\.dependencies)\]\n", root, re.M)
fix_path = lambda s: re.sub(r'path = "(iris-[\w-]+)"', r'path = "../../\1"', s)


def forward(feat_block, targets):
    """Append `target/feature` to each feature entry, line by line."""
    seen = set()

    def one(m):
        name, body, tail = m.group(1), m.group(2), m.group(3)
        seen.add(name)
        if name == "default" or not targets:
            return m.group(0)
        items = body.strip()
        extra = ", ".join(f'"{t}/{name}"' for t in targets)
        items = f"{items}, {extra}" if items else extra
        return f"{name} = [{items}]{tail}"

    out = re.sub(r"^([A-Za-z0-9_-]+)\s*=\s*\[([^\]]*)\]([^\n]*)$", one, feat_block, flags=re.M)
    missed = [f for f in FEATURES if f not in seen]
    if missed:
        fail(f"feature(s) not forwarded, the line format is not handled: {missed}")
    return out


def lower_toml(layer):
    below = [] if layer == "core" else ["core"]
    pk = CRATES[layer]
    out = [f'[package]\nname = "{pk}"\nversion = "{rt["package"]["version"]}"\nedition = "{rt["package"]["edition"]}"\npublish = false\n',
           "# One layer of the iris crate split (layers.toml; generated by scripts/crate-split.py).\n"
           "# Features mirror the iris package so a cfg inside a shared macro means the\n"
           "# same thing in every crate; iris forwards each one to all of them.\n"
           "[features]\n" + forward(feat_text, [CRATES[b] for b in below]).rstrip() +
           "\n" + "test-support = [" + ", ".join(f'"{CRATES[b]}/test-support"' for b in below) + "]\n",
           "[dependencies]\n" + fix_path(deps_text).rstrip() + "\n" +
           "".join(f'{CRATES[b]} = {{ path = "../{CRATES[b]}", default-features = false }}\n' for b in below)]
    for t in tdeps:
        out.append(f"[{t}]\n" + fix_path(section(root, t)).rstrip() + "\n")
    dd = '\n'.join(l for l in section(root, "dev-dependencies").split("\n") if l.strip() and not l.startswith("#")) + "\n"
    for b in below:
        dd += f'{CRATES[b]} = {{ path = "../{CRATES[b]}", default-features = false, features = ["test-support"] }}\n'
    for e in SPLIT.get("test_edge", []):
        if e["crate"] == layer:
            dd += f'{CRATES[e["needs"]]} = {{ path = "../{CRATES[e["needs"]]}", features = ["test-support"] }}\n'
    out.append("[dev-dependencies]\n" + dd)
    return "\n".join(out)


def root_toml():
    r = root
    r = re.sub(r'members = \[([^\]]*)\]', lambda m: "members = [" + m.group(1) + "".join(f', "{CDIR}/{CRATES[l]}"' for l in LOWER) + "]", r, count=1)
    sp = section_span(r, "features")
    r = r[:sp[0]] + forward(feat_text, [CRATES[l] for l in LOWER]) + r[sp[1]:]
    r = r.replace("[dependencies]\n", "[dependencies]\n" + "".join(
        f'{CRATES[l]} = {{ path = "{CDIR}/{CRATES[l]}", default-features = false }}\n' for l in LOWER), 1)
    sp = section_span(r, "dev-dependencies")
    body = r[sp[0]:sp[1]]
    add = "# Helpers other layers' tests use are behind each layer's `test-support` feature.\n"
    add += "".join(f'{CRATES[l]} = {{ path = "{CDIR}/{CRATES[l]}", default-features = false, features = ["test-support"] }}\n'
                   for l in LOWER if l != "cpu" or any(e["needs"] == "cpu" for e in SPLIT.get("test_edge", [])))
    nl = "" if body.endswith("\n\n") else ""
    r = r[:sp[0]] + body.rstrip("\n") + "\n" + add + "\n" + r[sp[1]:]
    return r


# --- lib.rs of each crate -------------------------------------------------

inner = [l for l in lib if l.startswith("#![")]
HDR = "\n".join(inner) + "\n"


def reexport_line(b, owner_ident):
    attrs = [a for a in b["attrs"] if a.startswith("#[cfg")]
    return "\n".join(attrs + [f"pub use {owner_ident}::{b['name']};"])


def own_block(b):
    attrs = []
    for a in b["attrs"]:
        if a.strip() == "#[cfg(test)]" and b["layer"] == "core":
            a = '#[cfg(any(test, feature = "test-support"))]'
        attrs.append(a)
    return "\n".join(attrs + [f"pub mod {b['name']};" if b["pub"] else f"mod {b['name']};"])


# modules that are split between crates (a mod.rs whose children live elsewhere)


def mod_children(path):
    t = text(path) if os.path.exists(path) else ""
    out = []
    ls = t.split("\n")
    for k, l in enumerate(ls):
        m = re.match(r"^(pub(?:\([^)]*\))? )?mod ([A-Za-z0-9_]+);$", l)
        if m:
            s = k
            while s > 0 and re.match(r"^\s*(#\[|///|//)", ls[s - 1]):
                s -= 1
            out.append(dict(name=m.group(2), line=k, start=s, vis=m.group(1) or "", attrs=ls[s:k]))
    return ls, out


plan = {}  # (layer, rel path of mod.rs under src) -> new text
parents = {}  # parent module path (tuple) -> dict(layer-of-mod.rs, children by layer)
for dp, dn, fn in os.walk("src"):
    if "mod.rs" in fn:
        rel = os.path.relpath(os.path.join(dp, "mod.rs"), "src")
        P = rel_module(rel)
        Lp = layer_of(P)
        ls, ch = mod_children("src/" + rel)
        bylayer = {}
        for c in ch:
            Lc = layer_of(P + [c["name"]])
            bylayer.setdefault(Lc, []).append(c)
        if set(bylayer) - {Lp}:
            parents[tuple(P)] = dict(layer=Lp, rel=rel, lines=ls, children=bylayer)

mod_new = {}  # (layer, rel) -> content, for generated mod.rs
removed = {}  # rel -> new text of the kept mod.rs
for P, info in parents.items():
    Lp, rel, ls, by = info["layer"], info["rel"], info["lines"], info["children"]
    pname = "::".join(P)
    gone = set()
    for Lc, cs in by.items():
        if Lc == Lp:
            continue
        for c in cs:
            gone.update(range(c["start"], c["line"] + 1))
    # the kept mod.rs: drop lowered children, re-export the core ones
    kept = [l for k, l in enumerate(ls) if k not in gone]
    while kept and not kept[-1].strip():
        kept.pop()
    core_kids = by.get("core", []) if Lp != "core" else []
    if core_kids:
        kept += ["", f"pub use {pkg_ident('core')}::{pname}::{{" + ", ".join(c["name"] for c in core_kids) + "};"]
    for e in SPLIT.get("test_edge", []):
        if e["crate"] == Lp and e["module"] == pname:
            kept += [f"// Tests here drive a real {', '.join(e['items'])} from {CRATES[e['needs']]}.",
                     "#[cfg(test)]", f"pub use {pkg_ident(e['needs'])}::{pname}::{{" + ", ".join(e["items"]) + "};"]
    while kept and not kept[-1].strip():
        kept.pop()
    for k in range(len(kept) - 1, 0, -1):
        if kept[k] == "" and kept[k - 1] == "":
            del kept[k]
    removed[rel] = "\n".join(kept) + "\n"
    # generated mod.rs in each other crate that holds children
    for Lc, cs in by.items():
        if Lc == Lp:
            continue
        body = ["//! The part of `" + pname + "` that lives in " + CRATES[Lc] + ".", ""]
        for c in cs:
            body += [a for a in c["attrs"] if a.startswith("#[cfg") or a.startswith("///")]
            body.append(f"pub mod {c['name']};")
        if Lc != "core":
            lowc = by.get("core", [])
            if lowc:
                body += ["", f"pub use {pkg_ident('core')}::{pname}::{{" + ", ".join(c["name"] for c in lowc) + "};"]
        for e in SPLIT.get("test_edge", []):
            if e["crate"] == Lc and e["module"] == pname:
                body += [f"// Tests here drive a real {', '.join(e['items'])} from {CRATES[e['needs']]}.",
                         "#[cfg(test)]", f"pub use {pkg_ident(e['needs'])}::{pname}::{{" + ", ".join(e["items"]) + "};"]
        mod_new[(Lc, rel)] = "\n".join(body) + "\n"


def crate_has_module(L, name):
    if any(m == name or m.startswith(name + "/") for m, l in moves.items() if l == L):
        return True
    return any(l == L and r.split("/")[0] == name for (l, r) in mod_new)


def lib_for(L):
    out = [HDR.rstrip("\n"), ""]
    if L != "core":
        out += ["// The layers below this one, re-exported at the crate root so `crate::config`,",
                "// `crate::traits` and the rest resolve in code moved out of the iris crate.",
                "#[macro_use]", f"extern crate {pkg_ident('core')};"]
        names = [b for b in blocks if b["layer"] == "core" and b["pub"]]
        for b in names:
            out.append(reexport_line(b, pkg_ident("core")))
        pf = [n for (_, _, ns) in reexp for n in ns]
        if pf:
            out.append(f"pub use {pkg_ident('core')}::{{" + ", ".join(pf) + "};")
        if macros["core"]:
            out.append(f"pub use {pkg_ident('core')}::{{" + ", ".join(macros["core"]) + "};")
        for P in parents:
            if len(P) == 1 and parents[P]["children"].get("core") and not crate_has_module(L, P[0]):
                out.append(f"pub use {pkg_ident('core')}::{P[0]};")
        out.append("")
    for b in blocks:
        if b["layer"] == L:
            out.append(own_block(b))
    for P in parents:
        if len(P) == 1 and crate_has_module(L, P[0]) and layer_of([P[0]]) != L:
            out.append(f"pub mod {P[0]};")
    if L == "core":
        for (k, mod, ns) in reexp:
            out.append(f"pub use {mod}::{{" + ", ".join(ns) + "};")
    return "\n".join(out) + "\n"


def root_lib():
    new = list(lib)
    drop = set()
    repl = {}
    for b in blocks:
        L = b["layer"]
        if L == "top":
            continue
        for k in range(b["start"], b["end"] + 1):
            drop.add(k)
        pname = b["name"]
        provs = [x for x in LOWER if crate_has_module(x, pname) or x == L]
        # a crate that re-exports core's part of the module stands for it
        if len(provs) > 1 and "core" in provs:
            provs.remove("core")
        if len(provs) > 1 and pname in {P[0] for P in parents}:
            prov = provs
            mods = "\n".join(f"    pub use {pkg_ident(x)}::{pname}::*;" for x in prov)
            repl[b["start"]] = f"pub mod {pname} {{\n{mods}\n}}"
        elif b["pub"]:
            repl[b["start"]] = reexport_line(b, pkg_ident(L))
    for (k, mod, ns) in reexp:
        drop.add(k)
    # macro plumbing, once, where the Pf* line was
    plumb = ["#[macro_use]", f"extern crate {pkg_ident('core')};"]
    pf = [n for (_, _, ns) in reexp for n in ns]
    if pf:
        plumb.append(f"pub use {pkg_ident('core')}::{{" + ", ".join(pf) + "};")
    if macros["core"]:
        plumb.append(f"pub use {pkg_ident('core')}::{{" + ", ".join(macros["core"]) + "};")
    if reexp:
        repl[reexp[0][0]] = "\n".join(plumb)
        drop.discard(reexp[0][0])
    out = []
    for k, l in enumerate(new):
        if k in repl:
            out.append(repl[k])
        elif k not in drop:
            out.append(l)
    return "\n".join(out)


# --- feature check --------------------------------------------------------

def check_features(crate_toml_text, files, name):
    declared = set(tomllib.loads(crate_toml_text)["features"])
    bad = {}
    for f in files:
        if not f.endswith(".rs"):
            continue
        for m in re.finditer(r'feature\s*=\s*"([A-Za-z0-9_-]+)"', text_of(f)):
            if m.group(1) not in declared:
                bad.setdefault(m.group(1), f)
    return bad


def text_of(path):
    return edits[path] if path in edits else open(path).read()


# --- write ----------------------------------------------------------------

outputs = {}  # new path -> text
for L in LOWER:
    outputs[f"{CDIR}/{CRATES[L]}/Cargo.toml"] = lower_toml(L)
    outputs[f"{CDIR}/{CRATES[L]}/src/lib.rs"] = lib_for(L)
for (L, rel), t in mod_new.items():
    outputs[f"{CDIR}/{CRATES[L]}/src/{rel}"] = t
outputs["Cargo.toml"] = root_toml()
outputs["src/lib.rs"] = root_lib()

problems = []
# every feature a crate reads must be declared by that crate
for L in LOWER:
    files = [f"src/{r}" for r, l in moves.items() if l == L]
    for feat, f in check_features(outputs[f"{CDIR}/{CRATES[L]}/Cargo.toml"], files, CRATES[L]).items():
        if feat in FEATURES:
            problems.append(f"{CRATES[L]} reads feature {feat!r} ({f}) but does not declare it")
        else:
            print(f"crate-split: note: {f} reads feature {feat!r}, which the iris package does not declare either")
if problems:
    fail("\n  ".join(["feature check failed:"] + problems))

if args.dry_run:
    print(f"would move {len(moves)} files into {len(LOWER)} crates; {len(parents)} split module(s): "
          + ", ".join("::".join(p) for p in parents))
    sys.exit(0)

for rel, L in moves.items():
    src = f"src/{rel}"
    dst = f"{CDIR}/{CRATES[L]}/src/{rel}"
    if rel in removed:
        edits[src] = removed[rel]
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    if src in edits:
        open(dst, "w").write(edits[src])
        shutil.copymode(src, dst)
        os.remove(src)
    else:
        os.rename(src, dst)
for path, t in edits.items():
    if os.path.exists(path):
        open(path, "w").write(t)
for path, t in outputs.items():
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    open(path, "w").write(t)
# prune emptied directories under src/
for dp, dn, fn in sorted(os.walk("src", topdown=False)):
    if not dn and not fn:
        os.rmdir(dp)
print(f"moved {len(moves)} files into {len(LOWER)} crates; split modules: "
      + ", ".join("::".join(p) for p in parents))
