//! Module layering: a lower layer must not name a higher one.
//!
//! `layers.toml` assigns every module under `src/` to a layer (core, cpu, dev,
//! gfx, top) and says which layers each may depend on. This test reads every
//! `crate::` and `super::` path in the sources and fails, with file:line, for
//! each one that points the wrong way. It needs nothing but `toml` and `serde`,
//! which iris already depends on, and works on the single crate as it is: the
//! point is to keep the module graph splittable into crates (see
//! `scripts/crate-split.py`) without having to split it first.
//!
//! What it looks at, and what it does not:
//!
//! * `crate::a::b::{c, d::e}` and `super::x` in `use` declarations and in
//!   expressions, resolved against the module the file defines (inline
//!   `mod x { .. }` blocks are followed for `super`).
//! * Code under `#[cfg(test)]` is skipped, including `#[cfg(test)] mod x;` and
//!   the file it names. Tests may name a sibling layer; the split gives the
//!   crate a dev-dependency for that. `IRIS_LAYERING_TESTS=1` counts them too.
//! * Not seen: macros (bare `foo!` invocations), `use` paths relative to the
//!   current module (`use child::x`), `#[path]` attributes and glob imports of
//!   a re-exporting parent. The compiler finds those when the crates are
//!   actually split.
//!
//! To tolerate a known violation while it is being removed, add an `[[allow]]`
//! entry to `layers.toml`. Entries that stop matching fail the test, so the
//! list only shrinks.
//!
//! `IRIS_LAYERING_LIST=1 cargo test --test layering -- --nocapture` prints
//! every violation and ready-to-paste `[[allow]]` entries, and does not fail.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Deserialize)]
struct Manifest {
    layers: BTreeMap<String, Vec<String>>,
    modules: BTreeMap<String, String>,
    #[serde(default)]
    allow: Vec<Allow>,
}

#[derive(Deserialize)]
struct Allow {
    from: String,
    to: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    /// Any single punctuation character; `::` is two of these.
    P(char),
}

struct Lexed {
    toks: Vec<(Tok, u32)>,
}

/// Tokenise Rust source well enough to follow paths: comments, strings, chars
/// and lifetimes are dropped, identifiers and punctuation kept with the line
/// they start on.
fn lex(src: &str) -> Lexed {
    let b = src.as_bytes();
    let mut toks = Vec::new();
    let (mut i, mut line) = (0usize, 1u32);
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        let c = b[i];
        match c {
            b'\n' => {
                line += 1;
                i += 1;
            }
            _ if c.is_ascii_whitespace() => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        if b[i] == b'\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                }
            }
            // Raw strings: r"..", r#".."#, br"..".
            b'r' | b'b'
                if {
                    let mut j = i + usize::from(c == b'b');
                    let raw = b.get(j) == Some(&b'r');
                    if raw {
                        j += 1;
                        while b.get(j) == Some(&b'#') {
                            j += 1;
                        }
                    }
                    raw && b.get(j) == Some(&b'"') && (i == 0 || !is_id(b[i - 1]))
                } =>
            {
                let mut j = i + usize::from(c == b'b') + 1;
                let mut hashes = 0;
                while b[j] == b'#' {
                    hashes += 1;
                    j += 1;
                }
                j += 1; // opening quote
                'raw: while j < b.len() {
                    if b[j] == b'\n' {
                        line += 1;
                    }
                    if b[j] == b'"' && (0..hashes).all(|k| b.get(j + 1 + k) == Some(&b'#')) {
                        j += 1 + hashes;
                        break 'raw;
                    }
                    j += 1;
                }
                i = j;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    if b.get(i) == Some(&b'\n') {
                        line += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'\'' => {
                // A char literal ('a', '\n', '\u{..}') or a lifetime ('a).
                if b.get(i + 1) == Some(&b'\\') {
                    i += 3; // quote, backslash, the escaped character
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                } else if b.get(i + 2) == Some(&b'\'') {
                    i += 3;
                } else if i + 1 < src.len() && src.is_char_boundary(i + 1) {
                    let ch = src[i + 1..].chars().next().unwrap();
                    if b.get(i + 1 + ch.len_utf8()) == Some(&b'\'') {
                        i += 2 + ch.len_utf8();
                    } else {
                        i += 1; // lifetime tick; the name lexes as an identifier
                    }
                } else {
                    i += 1;
                }
            }
            _ if is_id(c) && !c.is_ascii_digit() => {
                let s = i;
                while i < b.len() && is_id(b[i]) {
                    i += 1;
                }
                toks.push((Tok::Ident(src[s..i].to_string()), line));
            }
            _ if c.is_ascii_digit() => {
                while i < b.len() && (is_id(b[i]) || b[i] == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
                    i += 1;
                }
            }
            _ => {
                toks.push((Tok::P(c as char), line));
                i += 1;
            }
        }
    }
    Lexed { toks }
}

struct Edge {
    file: PathBuf,
    line: u32,
    from_mod: Vec<String>,
    path: Vec<String>,
    test_only: bool,
}

struct Scanner<'a> {
    t: &'a [(Tok, u32)],
    i: usize,
}

impl Scanner<'_> {
    fn ident(&self, at: usize) -> Option<&str> {
        match self.t.get(at) {
            Some((Tok::Ident(s), _)) => Some(s),
            _ => None,
        }
    }
    fn punct(&self, at: usize, c: char) -> bool {
        matches!(self.t.get(at), Some((Tok::P(p), _)) if *p == c)
    }
    fn colons(&self, at: usize) -> bool {
        self.punct(at, ':') && self.punct(at + 1, ':')
    }

    /// Parse the part of a path or use-tree after a `::`, pushing every leaf.
    fn tree(&mut self, prefix: &[String], out: &mut Vec<Vec<String>>) {
        if self.punct(self.i, '{') {
            self.i += 1;
            loop {
                if self.punct(self.i, '}') {
                    self.i += 1;
                    return;
                }
                if self.i >= self.t.len() {
                    return;
                }
                if self.punct(self.i, ',') {
                    self.i += 1;
                    continue;
                }
                let before = self.i;
                self.tree(prefix, out);
                if self.i == before {
                    self.i += 1;
                }
            }
        }
        if self.punct(self.i, '*') {
            self.i += 1;
            out.push(prefix.to_vec());
            return;
        }
        let Some(name) = self.ident(self.i).map(str::to_string) else {
            out.push(prefix.to_vec());
            return;
        };
        self.i += 1;
        let mut p = prefix.to_vec();
        if name != "self" {
            p.push(name);
        }
        if self.colons(self.i) && !self.punct(self.i + 2, '<') {
            self.i += 2;
            return self.tree(&p, out);
        }
        // `x as y` renames; the new name is not a path.
        if self.ident(self.i) == Some("as") {
            self.i += 2;
        }
        out.push(p);
    }
}

/// Skip the item that follows a `#[cfg(test)]`. Returns the token index after
/// it, and the child module name when the item is `mod name;`.
fn skip_item(t: &[(Tok, u32)], mut i: usize) -> (usize, Option<String>) {
    // Further attributes on the same item.
    while matches!(t.get(i), Some((Tok::P('#'), _))) {
        i += 1;
        if matches!(t.get(i), Some((Tok::P('!'), _))) {
            i += 1;
        }
        i = skip_group(t, i);
    }
    let mut modname = None;
    let mut j = i;
    while j < t.len() && matches!(&t[j].0, Tok::Ident(s) if matches!(s.as_str(), "pub" | "unsafe" | "async" | "extern")) {
        j += 1;
        if matches!(t.get(j), Some((Tok::P('('), _))) {
            j = skip_group(t, j);
        }
    }
    if let (Some((Tok::Ident(m), _)), Some((Tok::Ident(n), _)), Some((Tok::P(';'), _))) = (t.get(j), t.get(j + 1), t.get(j + 2)) {
        if m == "mod" {
            modname = Some(n.clone());
        }
    }
    // To the first `;` at nesting zero, or past the first `{}` block.
    let mut depth = 0i32;
    while i < t.len() {
        match &t[i].0 {
            Tok::P('(') | Tok::P('[') => depth += 1,
            Tok::P(')') | Tok::P(']') => depth -= 1,
            Tok::P(';') if depth <= 0 => return (i + 1, modname),
            Tok::P('{') if depth <= 0 => return (skip_group(t, i), modname),
            _ => {}
        }
        i += 1;
    }
    (i, modname)
}

/// Index after the bracketed group opening at `i`.
fn skip_group(t: &[(Tok, u32)], mut i: usize) -> usize {
    let mut depth = 0i32;
    while i < t.len() {
        match &t[i].0 {
            Tok::P('(') | Tok::P('[') | Tok::P('{') => depth += 1,
            Tok::P(')') | Tok::P(']') | Tok::P('}') => {
                depth -= 1;
                if depth <= 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

fn is_cfg_test(t: &[(Tok, u32)], open: usize) -> bool {
    // `[` cfg ( test ) `]`
    let id = |k: usize, s: &str| matches!(t.get(open + k), Some((Tok::Ident(x), _)) if x == s);
    let p = |k: usize, c: char| matches!(t.get(open + k), Some((Tok::P(x), _)) if *x == c);
    p(0, '[') && id(1, "cfg") && p(2, '(') && id(3, "test") && p(4, ')') && p(5, ']')
}

/// Module path a file defines, relative to its source root.
fn file_module(rel: &Path) -> Vec<String> {
    let mut parts: Vec<String> = rel.iter().map(|s| s.to_string_lossy().into_owned()).collect();
    if let Some(last) = parts.last_mut() {
        if let Some(stem) = last.strip_suffix(".rs") {
            *last = stem.to_string();
        }
    }
    if matches!(parts.last().map(String::as_str), Some("mod" | "lib" | "main")) {
        parts.pop();
    }
    parts
}

/// Scan one file. Returns its edges and the child modules it declares under
/// `#[cfg(test)]`.
fn scan_file(path: &Path, rel_mod: &[String]) -> (Vec<Edge>, Vec<Vec<String>>, bool) {
    let src = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    scan_source(&src, path, rel_mod)
}

fn scan_source(src: &str, path: &Path, rel_mod: &[String]) -> (Vec<Edge>, Vec<Vec<String>>, bool) {
    let lx = lex(src);
    let t = &lx.toks;
    let mut edges = Vec::new();
    let mut test_children = Vec::new();
    let mut whole_file_test = false;
    // Inline `mod name {` nesting, for `super`: (name, brace depth inside).
    let mut stack: Vec<(String, i32)> = Vec::new();
    let mut depth = 0i32;
    let mut sc = Scanner { t, i: 0 };
    while sc.i < t.len() {
        let i = sc.i;
        match &t[i].0 {
            Tok::P('#') => {
                let inner = matches!(t.get(i + 1), Some((Tok::P('!'), _)));
                let open = i + 1 + usize::from(inner);
                if is_cfg_test(t, open) {
                    if inner {
                        whole_file_test = true;
                        sc.i = open + 6;
                    } else {
                        let (next, child) = skip_item(t, open + 6);
                        if let Some(c) = child {
                            let mut m = rel_mod.to_vec();
                            m.extend(stack.iter().map(|(n, _)| n.clone()));
                            m.push(c);
                            test_children.push(m);
                        }
                        // Keep brace depth right: the skipped item is balanced.
                        sc.i = next;
                    }
                    continue;
                }
                sc.i += 1;
            }
            Tok::P('{') => {
                depth += 1;
                sc.i += 1;
            }
            Tok::P('}') => {
                depth -= 1;
                while stack.last().is_some_and(|(_, d)| *d > depth) {
                    stack.pop();
                }
                sc.i += 1;
            }
            Tok::Ident(kw) if kw == "mod" && sc.ident(i + 1).is_some() && sc.punct(i + 2, '{') => {
                stack.push((sc.ident(i + 1).unwrap().to_string(), depth + 1));
                sc.i += 1;
            }
            Tok::Ident(kw) if (kw == "crate" || kw == "super") && sc.colons(i + 1) && !sc.colons_before(i) => {
                let line = t[i].1;
                let mut base: Vec<String> = if kw == "crate" {
                    Vec::new()
                } else {
                    let mut m = rel_mod.to_vec();
                    m.extend(stack.iter().map(|(n, _)| n.clone()));
                    m.pop();
                    m
                };
                sc.i += 3;
                // `super::super::x`
                while kw == "super" && sc.ident(sc.i) == Some("super") && sc.colons(sc.i + 1) {
                    base.pop();
                    sc.i += 3;
                }
                let mut leaves = Vec::new();
                sc.tree(&base, &mut leaves);
                for p in leaves {
                    edges.push(Edge {
                        file: path.to_path_buf(),
                        line,
                        from_mod: rel_mod.to_vec(),
                        path: p,
                        test_only: false,
                    });
                }
            }
            _ => sc.i += 1,
        }
    }
    (edges, test_children, whole_file_test)
}

impl Scanner<'_> {
    /// Whether the token before `at` is `::` or `$` (so `at` is not a path root).
    fn colons_before(&self, at: usize) -> bool {
        at >= 1 && (self.punct(at - 1, '$') || (at >= 2 && self.colons(at - 2)))
    }
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut ents: Vec<_> = fs::read_dir(dir).unwrap().filter_map(Result::ok).collect();
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let p = e.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Source roots: `src/` for the single crate, plus `crates/*/src` once split.
fn source_roots(repo: &Path) -> Vec<PathBuf> {
    let mut roots = vec![repo.join("src")];
    if let Ok(rd) = fs::read_dir(repo.join("crates")) {
        let mut v: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path().join("src")).filter(|p| p.is_dir()).collect();
        v.sort();
        roots.extend(v);
    }
    roots
}

impl Manifest {
    /// Longest listed prefix of `path`, with the key that matched.
    fn lookup(&self, path: &[String]) -> (String, String) {
        for n in (1..=path.len()).rev() {
            let key = path[..n].join("::");
            if let Some(l) = self.modules.get(&key) {
                return (l.clone(), key);
            }
        }
        ("top".to_string(), path.first().cloned().unwrap_or_default())
    }
}

#[test]
fn layers_are_respected() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = fs::read_to_string(repo.join("layers.toml")).expect("layers.toml");
    let m: Manifest = toml::from_str(&text).expect("layers.toml parses");
    for (l, deps) in &m.layers {
        for d in deps {
            assert!(m.layers.contains_key(d), "layer {l} depends on unknown layer {d}");
        }
    }
    for (k, l) in &m.modules {
        assert!(m.layers.contains_key(l), "module {k} is in unknown layer {l}");
    }

    let mut edges = Vec::new();
    let mut test_mods: BTreeSet<Vec<String>> = BTreeSet::new();
    let mut files = 0usize;
    for root in source_roots(repo) {
        let mut rs = Vec::new();
        collect_rs(&root, &mut rs);
        for f in rs {
            let rel = f.strip_prefix(&root).unwrap();
            let md = file_module(rel);
            // Binaries name the library as `iris::`, not `crate::`.
            if md.first().is_some_and(|s| s == "bin") || rel == Path::new("main.rs") {
                continue;
            }
            files += 1;
            let (e, tc, whole) = scan_file(&f, &md);
            if whole {
                test_mods.insert(md.clone());
            }
            test_mods.extend(tc);
            edges.extend(e);
        }
    }
    assert!(files > 100, "found only {files} source files; is the test running from the repository root?");

    let with_tests = std::env::var_os("IRIS_LAYERING_TESTS").is_some();
    let list_only = std::env::var_os("IRIS_LAYERING_LIST").is_some();
    for e in &mut edges {
        e.test_only = test_mods.iter().any(|t| e.from_mod.starts_with(t));
    }

    let mut bad: Vec<(String, u32, String, String, String, String)> = Vec::new();
    let mut tested = 0usize;
    for e in &edges {
        if e.test_only && !with_tests {
            tested += 1;
            continue;
        }
        let (from_layer, _) = m.lookup(&e.from_mod);
        let (to_layer, to_key) = m.lookup(&e.path);
        if from_layer == to_layer || m.layers[&from_layer].contains(&to_layer) {
            continue;
        }
        let rel = e.file.strip_prefix(repo).unwrap().to_string_lossy().into_owned();
        let src_rel = rel.strip_prefix("src/").map(str::to_string).unwrap_or_else(|| {
            // crates/iris-x/src/... -> ...
            rel.split_once("/src/").map(|(_, r)| r.to_string()).unwrap_or(rel.clone())
        });
        bad.push((rel, e.line, src_rel, format!("{from_layer} -> {to_layer}"), to_key, format!("crate::{}", e.path.join("::"))));
    }
    bad.sort();

    // Apply the allow list.
    let mut used = vec![false; m.allow.len()];
    let mut left = Vec::new();
    for b in bad {
        let hit = m.allow.iter().position(|a| a.from == b.2 && a.to == b.4);
        match hit {
            Some(k) => used[k] = true,
            None => left.push(b),
        }
    }

    if list_only {
        for b in &left {
            println!("{}:{}: {}: {}", b.0, b.1, b.3, b.5);
        }
        let mut seen = BTreeSet::new();
        for b in &left {
            if seen.insert((b.2.clone(), b.4.clone())) {
                println!("[[allow]]\nfrom = \"{}\"\nto = \"{}\"\n", b.2, b.4);
            }
        }
        println!("{} violations in {} files ({} test-only edges skipped)", left.len(), files, tested);
        return;
    }

    let mut msg = String::new();
    for b in &left {
        msg.push_str(&format!("  {}:{}: {}: {}\n", b.0, b.1, b.3, b.5));
    }
    for (k, a) in m.allow.iter().enumerate() {
        if !used[k] {
            msg.push_str(&format!("  layers.toml: stale [[allow]] from = {:?} to = {:?}: no such edge any more, remove it\n", a.from, a.to));
        }
    }
    assert!(
        msg.is_empty(),
        "{} layering violation(s); a module may depend only on the layers listed for its own in layers.toml:\n{msg}",
        left.len()
    );
}

#[cfg(test)]
mod scanner_tests {
    use super::*;

    fn paths(src: &str, module: &[&str]) -> Vec<String> {
        let md: Vec<String> = module.iter().map(|s| s.to_string()).collect();
        let (e, _, _) = scan_source(src, Path::new("x.rs"), &md);
        e.iter().map(|e| e.path.join("::")).collect()
    }

    #[test]
    fn use_trees_and_super() {
        let got = paths("use crate::a::{b, c::d, e::{self, f}};\nfn x() { crate::g::h(); }", &["m"]);
        assert_eq!(got, ["a::b", "a::c::d", "a::e", "a::e::f", "g::h"]);
        let got = paths("use super::x::Y;\nmod tests { use super::*; use super::super::z; }", &["a", "b"]);
        assert_eq!(got, ["a::x::Y", "a::b", "a::z"]);
    }

    #[test]
    fn ignores_comments_strings_and_macros_roots() {
        let got = paths("// crate::no\n/* crate::no */ let s = \"crate::no\"; let r = r#\"crate::no\"#; $crate::no!(); 'a'; crate::yes::X", &["m"]);
        assert_eq!(got, ["yes::X"]);
    }

    #[test]
    fn skips_cfg_test_items() {
        let got = paths("#[cfg(test)]\nmod tests { use crate::no; }\n#[cfg(test)] use crate::no2;\nuse crate::yes;\n#[cfg(not(test))] use crate::yes2;", &["m"]);
        assert_eq!(got, ["yes", "yes2"]);
    }
}
