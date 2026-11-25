//! The install's third-party notices (T14.37): [`NOTICES_FILE`] beside `earshot.exe`, written by
//! `cargo xtask build --bundle` from facts, never by hand.
//!
//! - **Rust crates.** The shipped app's normal dependency graph for the target and the features it
//!   is built with ([`tree_args`]: `cargo tree -e normal,no-proc-macro`, so build scripts and
//!   procedural macros, which put no code in the exe, are left out). Each crate's licence
//!   expression comes from cargo, its licence files (`LICENSE*`, `LICENCE*`, `COPYING*`,
//!   `NOTICE*`, `UNLICENSE*`) from its source folder in the build's own `CARGO_HOME`, else the
//!   shell's ([`cargo_homes`]; both copies must agree). `cargo metadata` is not used:
//!   it needs every workspace package, dev-dependencies included, downloaded, and the build's
//!   `CARGO_HOME` holds only what the build needed (measured 2026-11-26), so offline it fails.
//!   Under an `OR` the entry says which licence Earshot takes and quotes only that one's text
//!   ([`choose_where`], [`identify`]); every licence taken needs its text among the files.
//! - **The Rust standard library** ([`rust_std_entry`]).
//! - **Native libraries** compiled from sources a crate carries ([`VENDORED`], [`IN_FILE`],
//!   [`DISCLAIMERS`]) or linked in prebuilt ([`LINKED_PREBUILT`]).
//! - **JavaScript packages** the window is built from: the production dependency closure of
//!   [`NPM_ROOTS`], read from the installed `node_modules` as Node resolves it ([`npm_packages`]),
//!   with the same licence rules as crates.
//! - **Other files** ([`other_entries`]): the Visual C++ runtime, the Vulkan loader (its licence
//!   ships beside it), the QNN files on ARM64 (their notice ships beside them), NSIS's code in the
//!   uninstaller and its plugin, and ONNX Runtime, which the app downloads with its own licence
//!   files; facts from `xtask/bundler-tools.toml` and `models/manifest.toml`.
//! - **Earshot's own `LICENSE`**, first ([`own_licence`]).
//!
//! A package whose licence expression is missing, or satisfies neither [`PERMISSIVE`] nor
//! [`FILE_COPYLEFT_ACCEPTED`], fails the build by name, and so does one that ships no licence
//! file under a licence xtask holds no standard text of ([`CANONICAL`]). MIT and BSD texts name a
//! copyright holder: when a package ships no file, the entry says no holder was found rather than
//! invent one. Output is sorted, with no time and no path of the build machine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

/// The file in an install, beside `earshot.exe`, and as the bundle writes it beside the built exe.
pub const NOTICES_FILE: &str = "third-party-notices.txt";

/// Licences any part of the shipped app may be under: permissive, with no copyleft. Derived from
/// the shipped trees of 2026-11-26 (every id an expression there needs), not a wish list.
pub const PERMISSIVE: &[&str] = &[
    "0BSD",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "bzip2-1.0.6",
    "CC0-1.0",
    "CDLA-Permissive-2.0",
    "ISC",
    "MIT",
    "MIT-0",
    "Unicode-3.0",
    "Unlicense",
    "Zlib",
];

/// File-level copyleft accepted for files used unmodified: the work as a whole stays under its
/// own terms (ground rule 11 forbids only terms that would reach the whole). Each package under
/// one is listed by `cargo xtask build --bundle` for the owner, and its entry says where its
/// source is published, which MPL-2.0 section 3.2 asks of an executable.
pub const FILE_COPYLEFT_ACCEPTED: &[&str] = &["MPL-2.0"];

/// Licence exceptions (`<id> WITH <exception>`) that keep an allowed licence allowed.
pub const EXCEPTIONS: &[&str] = &["LLVM-exception"];

/// Standard texts for packages that ship no licence file: exactly the licences the shipped trees
/// need them for (2026-11-26). The MIT and BSD texts have their copyright line removed.
pub const CANONICAL: &[(&str, &str)] = &[
    ("Apache-2.0", include_str!("licences/Apache-2.0.txt")),
    ("BSD-3-Clause", include_str!("licences/BSD-3-Clause.txt")),
    ("MIT", include_str!("licences/MIT.txt")),
    ("Unlicense", include_str!("licences/Unlicense.txt")),
];

/// Licences whose text names a copyright holder the package itself must supply.
const NEEDS_HOLDER: &[&str] = &["MIT", "BSD-2-Clause", "BSD-3-Clause", "ISC"];

/// The start of a licence file's name, compared without case.
const LICENCE_FILE_PREFIXES: &[&str] = &["LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE"];

/// How long `cargo tree` may take.
const TREE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// A licence file inside a crate's sources that covers code compiled into the app and that the
/// crate's top-level files do not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vendored {
    /// The crate that carries it.
    pub krate: &'static str,
    /// What it covers, as the notices say it.
    pub what: &'static str,
    /// Its path below the crate's folder.
    pub file: &'static str,
}

/// The native libraries built from sources inside a crate, and the code a crate took from
/// elsewhere with its own licence file. A file missing from its crate fails the build, so a
/// version that moves one is noticed.
pub const VENDORED: &[Vendored] = &[
    Vendored {
        krate: "whisper-rs-sys",
        what: "whisper.cpp and the ggml it carries, compiled into earshot.exe",
        file: "whisper.cpp/LICENSE",
    },
    Vendored {
        krate: "llama-cpp-sys-2",
        what: "llama.cpp and the ggml it carries, built as llama.dll and the ggml DLLs beside \
               earshot.exe",
        file: "llama.cpp/LICENSE",
    },
    Vendored {
        krate: "ring",
        what: "code ring took from the once_cell project",
        file: "src/polyfill/once_cell/LICENSE-MIT",
    },
    Vendored {
        krate: "ring",
        what: "code ring took from the once_cell project",
        file: "src/polyfill/once_cell/LICENSE-APACHE",
    },
    Vendored {
        krate: "ring",
        what: "code ring took from the fiat-crypto project",
        file: "third_party/fiat/LICENSE",
    },
];

/// A notice inside a source file compiled into the app, under another holder or licence than its
/// tree's top-level licence: `lines` lines quoted from the first line holding `from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFile {
    /// The crate that carries it.
    pub krate: &'static str,
    /// What it covers.
    pub what: &'static str,
    /// The source file, below the crate's folder.
    pub file: &'static str,
    /// Words on the notice's first line.
    pub from: &'static str,
    /// How many lines the notice runs to.
    pub lines: usize,
}

/// The in-file notices of the native trees compiled into the app (read 2026-11-26 by scanning
/// every compiled source of llama.cpp's and whisper.cpp's `src`, `ggml/src` (base, CPU and Vulkan
/// backends) and `include` for a copyright or licence line: [`in_file_candidates`] keeps that scan
/// honest). A notice that moves or changes fails the build.
pub const IN_FILE: &[InFile] = &[
    InFile {
        krate: "llama-cpp-sys-2",
        what: "llamafile's matrix multiplication in llama.cpp's ggml CPU backend",
        file: "llama.cpp/ggml/src/ggml-cpu/llamafile/sgemm.cpp",
        from: "// Copyright 2024 Mozilla Foundation",
        lines: 21,
    },
    InFile {
        krate: "whisper-rs-sys",
        what: "llamafile's matrix multiplication in whisper.cpp's ggml CPU backend",
        file: "whisper.cpp/ggml/src/ggml-cpu/llamafile/sgemm.cpp",
        from: "// Copyright 2024 Mozilla Foundation",
        lines: 21,
    },
    InFile {
        krate: "llama-cpp-sys-2",
        what: "the YaRN rotary embedding in llama.cpp's ggml CPU backend",
        file: "llama.cpp/ggml/src/ggml-cpu/ops.cpp",
        from: "// YaRN algorithm based on",
        lines: 2,
    },
    InFile {
        krate: "whisper-rs-sys",
        what: "the YaRN rotary embedding in whisper.cpp's ggml CPU backend",
        file: "whisper.cpp/ggml/src/ggml-cpu/ops.cpp",
        from: "// YaRN algorithm based on",
        lines: 2,
    },
    InFile {
        krate: "llama-cpp-sys-2",
        what: "llama.cpp's BPE tokenizer",
        file: "llama.cpp/src/llama-vocab.cpp",
        from: "// adapted from https://github.com/cmp-nct/ggllm.cpp",
        lines: 1,
    },
];

/// The files below `dir` (a native tree's compiled folders, [`COMPILED_NATIVE`]) with a line that
/// names a copyright or a licence, relative to `dir`, sorted: what [`IN_FILE`] and
/// [`NOT_NOTICES`] together must cover.
///
/// # Errors
/// An unreadable folder.
pub fn in_file_candidates(dir: &Path, folders: &[&str]) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for folder in folders {
        let (path, recurse) = match folder.strip_suffix("/*") {
            Some(top) => (dir.join(top), false),
            None => (dir.join(folder), true),
        };
        let files = if recurse {
            super::files_under(&path)?
        } else {
            let mut top = Vec::new();
            for e in std::fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
                let e = e?;
                if e.file_type()?.is_file() {
                    top.push(e.path());
                }
            }
            top
        };
        for f in files {
            let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !["c", "cpp", "h", "hpp", "comp", "glsl", "inc"].contains(&ext) {
                continue;
            }
            let text = String::from_utf8_lossy(&std::fs::read(&f)?).to_lowercase();
            if ["copyright", "license", "licence", "spdx"]
                .iter()
                .any(|w| text.contains(w))
            {
                let rel = f.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
                found.push(rel);
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// The folders of each native tree that are compiled into the shipped app (`/*`: that folder's own
/// files only): llama.cpp's library and ggml's base, CPU and Vulkan backends; whisper.cpp's
/// library and ggml's base, CPU and Vulkan backends (x64 builds whisper.cpp with Vulkan). Optional parts no shipped build turns on (KleidiAI,
/// AMX, CUDA, SYCL, CANN, ...) are not here, nor ggml's public headers (declarations only; the two
/// with notices, `ggml-cann.h` and `ggml-sycl.h`, belong to backends no shipped build compiles).
pub const COMPILED_NATIVE: &[(&str, &[&str])] = &[
    (
        "llama-cpp-sys-2",
        &[
            "llama.cpp/src",
            "llama.cpp/include",
            "llama.cpp/ggml/src/*",
            "llama.cpp/ggml/src/ggml-cpu/*",
            "llama.cpp/ggml/src/ggml-cpu/arch",
            "llama.cpp/ggml/src/ggml-cpu/llamafile",
            "llama.cpp/ggml/src/ggml-vulkan",
        ],
    ),
    (
        "whisper-rs-sys",
        &[
            "whisper.cpp/src",
            "whisper.cpp/include",
            "whisper.cpp/ggml/src/*",
            "whisper.cpp/ggml/src/ggml-cpu/*",
            "whisper.cpp/ggml/src/ggml-cpu/arch",
            "whisper.cpp/ggml/src/ggml-cpu/llamafile",
            "whisper.cpp/ggml/src/ggml-vulkan",
        ],
    ),
];

/// Files [`in_file_candidates`] finds that hold no notice: the words name a model's metadata key.
pub const NOT_NOTICES: &[&str] = &[
    "llama.cpp/src/llama-arch.cpp",
    "llama.cpp/src/llama-arch.h",
    "llama.cpp/src/llama-model-saver.cpp",
];

/// Sources compiled into the app whose authors disclaim copyright, with no licence file to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disclaimer {
    /// The crate that carries it.
    pub krate: &'static str,
    /// What it covers.
    pub what: &'static str,
    /// The source file, below the crate's folder.
    pub file: &'static str,
    /// The words of the file that say so, quoted in the notices and required in the file.
    pub phrase: &'static str,
}

/// SQLite, bundled by `rusqlite`'s `bundled` feature.
pub const DISCLAIMERS: &[Disclaimer] = &[Disclaimer {
    krate: "libsqlite3-sys",
    what: "SQLite, compiled into earshot.exe",
    file: "sqlite3/sqlite3.c",
    phrase: "The author disclaims copyright to this source code",
}];

/// A licence expression (SPDX syntax; `/` read as `OR`, as older crates write it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Licence {
    /// One licence, with an exception when the expression says `WITH`.
    Id(String, Option<String>),
    /// Every part applies.
    And(Vec<Licence>),
    /// Any one part may be chosen.
    Or(Vec<Licence>),
}

/// Parses a licence expression.
///
/// # Errors
/// An empty expression, unbalanced parentheses, or an operator with nothing beside it.
pub fn parse_licence(expr: &str) -> Result<Licence> {
    let spaced = expr
        .replace('/', " OR ")
        .replace('(', " ( ")
        .replace(')', " ) ");
    let tokens: Vec<&str> = spaced.split_whitespace().collect();
    ensure!(!tokens.is_empty(), "an empty licence expression");
    let mut at = 0;
    let parsed = parse_or(&tokens, &mut at)?;
    ensure!(
        at == tokens.len(),
        "unexpected `{}` in licence expression `{expr}`",
        tokens[at]
    );
    Ok(parsed)
}

fn is_op(token: &str, op: &str) -> bool {
    token.eq_ignore_ascii_case(op)
}

fn parse_or(tokens: &[&str], at: &mut usize) -> Result<Licence> {
    let mut parts = vec![parse_and(tokens, at)?];
    while tokens.get(*at).is_some_and(|t| is_op(t, "OR")) {
        *at += 1;
        parts.push(parse_and(tokens, at)?);
    }
    Ok(if parts.len() == 1 {
        parts.remove(0)
    } else {
        Licence::Or(parts)
    })
}

fn parse_and(tokens: &[&str], at: &mut usize) -> Result<Licence> {
    let mut parts = vec![parse_atom(tokens, at)?];
    while tokens.get(*at).is_some_and(|t| is_op(t, "AND")) {
        *at += 1;
        parts.push(parse_atom(tokens, at)?);
    }
    Ok(if parts.len() == 1 {
        parts.remove(0)
    } else {
        Licence::And(parts)
    })
}

fn parse_atom(tokens: &[&str], at: &mut usize) -> Result<Licence> {
    let token = *tokens
        .get(*at)
        .context("a licence expression ends too early")?;
    *at += 1;
    if token == "(" {
        let inner = parse_or(tokens, at)?;
        ensure!(
            tokens.get(*at) == Some(&")"),
            "a licence expression has an unclosed parenthesis"
        );
        *at += 1;
        return Ok(inner);
    }
    ensure!(
        token != ")" && !["OR", "AND", "WITH"].iter().any(|op| is_op(token, op)),
        "a licence expression has `{token}` where a licence should be"
    );
    if tokens.get(*at).is_some_and(|t| is_op(t, "WITH")) {
        let exception = *tokens
            .get(*at + 1)
            .context("a licence expression ends after WITH")?;
        *at += 2;
        return Ok(Licence::Id(token.to_owned(), Some(exception.to_owned())));
    }
    Ok(Licence::Id(token.to_owned(), None))
}

/// One licence of a choice: `id`, with an exception.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// The licence id.
    pub id: String,
    /// `WITH` exception.
    pub with: Option<String>,
}

impl Term {
    fn text(&self) -> String {
        match &self.with {
            Some(w) => format!("{} WITH {w}", self.id),
            None => self.id.clone(),
        }
    }
}

fn allowed_id(id: &str) -> bool {
    PERMISSIVE.contains(&id) || FILE_COPYLEFT_ACCEPTED.contains(&id)
}

/// What makes a choice worse, compared in order: parts the package ships no text of (`has_text`),
/// copyleft parts, parts with no standard text, parts needing a holder the package may not name,
/// then more parts. This is the fixed preference by which one side of an `OR` is taken.
fn cost(terms: &[Term], has_text: &dyn Fn(&Term) -> bool) -> (usize, usize, usize, usize, usize) {
    let count = |f: &dyn Fn(&Term) -> bool| terms.iter().filter(|t| f(t)).count();
    (
        count(&|t| !has_text(t)),
        count(&|t| FILE_COPYLEFT_ACCEPTED.contains(&t.id.as_str())),
        count(&|t| canonical(&t.id).is_none()),
        count(&|t| NEEDS_HOLDER.contains(&t.id.as_str())),
        terms.len(),
    )
}

/// The licences that satisfy `licence` within the allow-lists, the least demanding alternative of
/// each `OR` taken (the first on a tie), or `None` when nothing satisfies it.
#[must_use]
pub fn choose(licence: &Licence) -> Option<Vec<Term>> {
    choose_where(licence, &|_| true)
}

/// [`choose`], preferring first the alternatives whose every licence the package ships a text of
/// (`has_text`).
#[must_use]
pub fn choose_where(licence: &Licence, has_text: &dyn Fn(&Term) -> bool) -> Option<Vec<Term>> {
    match licence {
        Licence::Id(id, with) => {
            (allowed_id(id) && with.as_deref().is_none_or(|w| EXCEPTIONS.contains(&w))).then(|| {
                vec![Term {
                    id: id.clone(),
                    with: with.clone(),
                }]
            })
        }
        Licence::And(parts) => {
            let mut all = Vec::new();
            for p in parts {
                all.extend(choose_where(p, has_text)?);
            }
            Some(all)
        }
        Licence::Or(parts) => {
            let mut best: Option<Vec<Term>> = None;
            for choice in parts.iter().filter_map(|p| choose_where(p, has_text)) {
                if best
                    .as_ref()
                    .is_none_or(|b| cost(&choice, has_text) < cost(b, has_text))
                {
                    best = Some(choice);
                }
            }
            best
        }
    }
}

/// The standard text of licence `id`, when xtask holds one.
#[must_use]
pub fn canonical(id: &str) -> Option<&'static str> {
    CANONICAL.iter().find(|(c, _)| *c == id).map(|(_, t)| *t)
}

/// A text as the notices hold it: line ends `\n`, nothing trailing.
#[must_use]
pub fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_owned()
}

/// Reads `path` as text (lossily, so a stray byte cannot stop a build), normalised.
fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(normalise(&String::from_utf8_lossy(&bytes)))
}

/// The licence files directly in `dir`, sorted by name, each normalised. SPDX documents
/// (`LICENSE.spdx`) are metadata, not texts, and empty files say nothing.
///
/// # Errors
/// An unreadable folder or file.
pub fn licence_files(dir: &Path) -> Result<Vec<(String, String)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let upper = name.to_ascii_uppercase();
        if !LICENCE_FILE_PREFIXES.iter().any(|p| upper.starts_with(p)) || upper.ends_with(".SPDX") {
            continue;
        }
        let text = read_text(&entry.path())?;
        if !text.is_empty() {
            found.push((name, text));
        }
    }
    found.sort();
    Ok(found)
}

/// The licences a licence file holds the text of, from its words (compared without case or line
/// breaks) and its name (`LICENSE-MIT` and the like). A file that names none, such as a `NOTICE`
/// or a file pointing at others, is a notice, not a text.
#[must_use]
pub fn identify(name: &str, text: &str) -> Vec<&'static str> {
    // Comment markers at line starts (a licence kept as a source comment) are not words.
    let t = text
        .to_lowercase()
        .lines()
        .map(|l| l.trim().trim_start_matches(['/', '#', '*', ';']))
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let has = |s: &str| t.contains(s);
    let mut ids = Vec::new();
    let mut add = |id: &'static str, yes: bool| {
        if yes && !ids.contains(&id) {
            ids.push(id);
        }
    };
    add("Apache-2.0", has("apache license") && has("version 2.0"));
    add(
        "MIT",
        has("permission is hereby granted, free of charge") && !has("unicode license"),
    );
    add("Unlicense", has("free and unencumbered software"));
    add(
        "BSD-3-Clause",
        has("redistribution and use in source and binary forms") && has("neither the name"),
    );
    add(
        "BSD-2-Clause",
        has("redistribution and use in source and binary forms")
            && !has("neither the name")
            && !has("bzip2"),
    );
    add(
        "ISC",
        has("with or without fee is hereby granted, provided that the above copyright notice"),
    );
    add(
        "0BSD",
        has("with or without fee is hereby granted.")
            && !has("provided that the above copyright notice"),
    );
    add(
        "Zlib",
        has("altered source versions must be plainly marked") && !has("bzip2"),
    );
    add("Unicode-3.0", has("unicode license"));
    add("CC0-1.0", has("cc0 1.0 universal"));
    add(
        "CDLA-Permissive-2.0",
        has("community data license agreement"),
    );
    add(
        "bzip2-1.0.6",
        has("bzip2") && has("redistribution and use in source and binary forms"),
    );
    add("MPL-2.0", has("mozilla public license"));
    add("GPL", has("gnu general public license"));
    add("LGPL", has("gnu lesser general public license"));
    let upper = name.to_ascii_uppercase().replace('_', "-");
    for (part, id) in [
        ("-MIT", "MIT"),
        ("-APACHE", "Apache-2.0"),
        ("-ZLIB", "Zlib"),
        ("-ISC", "ISC"),
        ("-0BSD", "0BSD"),
    ] {
        add(id, upper.contains(part) && !upper.contains("THIRD"));
    }
    add("Unlicense", upper.starts_with("UNLICENSE"));
    ids
}

/// Whether a file holding the texts `ids` holds `term`'s (MIT-0's text reads as MIT's).
fn holds(ids: &[&str], term: &Term) -> bool {
    ids.iter()
        .any(|id| *id == term.id || (term.id == "MIT-0" && *id == "MIT"))
}

/// A package whose notice is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Its name.
    pub name: String,
    /// Its version.
    pub version: String,
    /// Its licence expression as it declares it, if it does.
    pub licence: Option<String>,
    /// Its source folder.
    pub dir: PathBuf,
    /// Where its source is published.
    pub url: String,
}

/// One entry of the notices: its line, its texts and what it says besides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's first line.
    pub line: String,
    /// Its licence texts, in order.
    pub texts: Vec<String>,
    /// Lines said under it.
    pub notes: Vec<String>,
    /// The file-level copyleft licences it is under ([`FILE_COPYLEFT_ACCEPTED`]).
    pub copyleft: Vec<String>,
}

/// The line a package's entry starts with, which `bundle-check` looks for.
#[must_use]
pub fn package_line_start(name: &str, version: &str) -> String {
    format!("{name} {version} |")
}

/// `licence` of `who`, trimmed, with the licences chosen from it within the allow-lists.
///
/// # Errors
/// By `who`: no licence, one that does not parse, or one no allowed licence satisfies.
pub fn checked_licence(who: &str, licence: Option<&str>) -> Result<(String, Vec<Term>)> {
    checked_licence_where(who, licence, &|_| true)
}

/// Whether `l` offers a choice anywhere.
fn has_or(l: &Licence) -> bool {
    match l {
        Licence::Id(..) => false,
        Licence::Or(_) => true,
        Licence::And(parts) => parts.iter().any(has_or),
    }
}

/// [`checked_licence`] with [`choose_where`].
///
/// # Errors
/// As [`checked_licence`].
pub fn checked_licence_where(
    who: &str,
    licence: Option<&str>,
    has_text: &dyn Fn(&Term) -> bool,
) -> Result<(String, Vec<Term>)> {
    let expr = licence
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .with_context(|| {
            format!("{who} declares no licence: a package with no licence cannot ship")
        })?;
    let parsed = parse_licence(expr).with_context(|| format!("{who}: licence `{expr}`"))?;
    let chosen = choose_where(&parsed, has_text).with_context(|| {
        format!(
            "{who} is under `{expr}`, which no licence on the allow-list satisfies \
             (bundle::notices::PERMISSIVE, FILE_COPYLEFT_ACCEPTED): nothing copyleft may ship \
             (AGENTS.md ground rule 11); ask the owner"
        )
    })?;
    Ok((expr.to_owned(), chosen))
}

/// The standard texts of `chosen`, for `who`, which ships no licence file.
///
/// # Errors
/// By `who`: a licence xtask holds no standard text of.
fn standard_texts(who: &str, chosen: &[Term]) -> Result<Vec<String>> {
    chosen
        .iter()
        .map(|term| {
            canonical(&term.id).map(normalise).with_context(|| {
                format!(
                    "{who} ships no licence file and xtask holds no standard text of {} \
                     (bundle::notices::CANONICAL)",
                    term.id
                )
            })
        })
        .collect()
}

/// What an entry using [`standard_texts`] says about them; `what` names the thing that ships no
/// file.
fn standard_text_notes(what: &str, chosen: &[Term]) -> Vec<String> {
    let ids: Vec<String> = chosen.iter().map(Term::text).collect();
    let mut notes = vec![format!(
        "The {what} ships no licence file: the standard text of {} is used.",
        ids.join(" and ")
    )];
    if chosen.iter().any(|t| NEEDS_HOLDER.contains(&t.id.as_str())) {
        notes.push(format!("No file in the {what} names its copyright holder."));
    }
    notes
}

/// The entry of `p`: its licence expression checked against the allow-lists, its licence files,
/// or the standard texts of the licences chosen when it ships none.
///
/// # Errors
/// By the package's name and version: no licence expression, one not allowed, or no licence file
/// and no standard text to use.
pub fn package_entry(p: &Package) -> Result<Entry> {
    let who = format!("{} {}", p.name, p.version);
    let files = licence_files(&p.dir).with_context(|| format!("{who}'s licence files"))?;
    let found: Vec<Vec<&str>> = files.iter().map(|(n, t)| identify(n, t)).collect();
    let shipped = |term: &Term| files.is_empty() || found.iter().any(|ids| holds(ids, term));
    let (expr, chosen) = checked_licence_where(&who, p.licence.as_deref(), &shipped)?;
    let mut notes = Vec::new();
    if parse_licence(&expr).is_ok_and(|l| has_or(&l)) {
        let ids: Vec<String> = chosen.iter().map(Term::text).collect();
        notes.push(format!(
            "Earshot uses it under {}, and only that licence's text is given.",
            ids.join(" and ")
        ));
    }
    let texts = if files.is_empty() {
        let texts = standard_texts(&who, &chosen)?;
        notes.extend(standard_text_notes("package", &chosen));
        texts
    } else {
        // Every licence taken needs its text among the files (AND as much as one licence).
        let missing: Vec<String> = chosen
            .iter()
            .filter(|t| !found.iter().any(|ids| holds(ids, t)))
            .map(Term::text)
            .collect();
        ensure!(
            missing.is_empty(),
            "{who} is under `{expr}` but none of its licence files ({}) holds the text of {}: \
             find the text or ask the owner",
            files
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            missing.join(" and ")
        );
        // A file holding only licences not taken (the other side of an OR) is left out.
        files
            .into_iter()
            .zip(&found)
            .filter(|(_, ids)| ids.is_empty() || chosen.iter().any(|t| holds(ids, t)))
            .map(|((_, t), _)| t)
            .collect()
    };
    let expr = expr.as_str();
    let copyleft: Vec<String> = chosen
        .iter()
        .filter(|t| FILE_COPYLEFT_ACCEPTED.contains(&t.id.as_str()))
        .map(|t| t.id.clone())
        .collect();
    for id in &copyleft {
        notes.push(format!(
            "Used unmodified under {id}; its source code is at {}.",
            p.url
        ));
    }
    Ok(Entry {
        line: format!(
            "{} {expr} | {}",
            package_line_start(&p.name, &p.version),
            p.url
        ),
        texts,
        notes,
        copyleft,
    })
}

/// A crate of the shipped tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TreeCrate {
    /// Its name.
    pub name: String,
    /// Its version, without the `v`.
    pub version: String,
    /// Its licence expression, if it declares one.
    pub licence: Option<String>,
}

/// `cargo tree`'s arguments for the shipped app's crates on `target` built with `features`: the
/// features [`crate::build::app_build_args`] passes, normal edges only, procedural macros left
/// out, from the lockfile and the local sources only.
#[must_use]
pub fn tree_args(target: &str, features: &[String]) -> Vec<String> {
    let build = crate::build::app_build_args(Some(target), features);
    let at = build
        .iter()
        .position(|a| a == "--features")
        .map_or(String::new(), |i| build[i + 1].clone());
    [
        "tree",
        "-p",
        "earshot-desktop",
        "-e",
        "normal,no-proc-macro",
        "--target",
        target,
        "--features",
        &at,
        "--prefix",
        "none",
        "--format",
        "{p}|{l}",
        "--locked",
        "--offline",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The crates of a `cargo tree` run with [`tree_args`], each once, sorted, without the
/// workspace's own (any package from a folder under `root`). A package from anywhere else that is
/// not the registry (a git or path dependency) fails: its licence files are not where the notices
/// look.
///
/// # Errors
/// Output not rooted at `earshot-desktop`, a line of another shape, or a package of another source.
pub fn parse_tree(stdout: &str, root: &Path) -> Result<Vec<TreeCrate>> {
    let first = stdout.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    crate::purity::require_tree_root("earshot-desktop", first)?;
    let root_key = path_key(root);
    let mut crates = std::collections::BTreeSet::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        // A repeat is marked at the end of the whole line.
        let line = line.trim_end().trim_end_matches(" (*)");
        let (package, licence) = line
            .rsplit_once('|')
            .with_context(|| format!("a `cargo tree` line of another shape: {line}"))?;
        let package = package.trim_end().trim_end_matches(" (*)").trim_end();
        let mut words = package.splitn(3, ' ');
        let (Some(name), Some(version)) = (words.next(), words.next()) else {
            bail!("a `cargo tree` line of another shape: {line}");
        };
        let version = version
            .strip_prefix('v')
            .with_context(|| format!("a `cargo tree` line with no version: {line}"))?;
        if let Some(source) = words.next() {
            let inner = source
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .with_context(|| format!("a `cargo tree` line of another shape: {line}"))?;
            if path_key(Path::new(inner)).starts_with(&root_key) {
                continue;
            }
            bail!(
                "{name} {version} comes from a source other than the registry; the notices read \
                 licence files only from registry sources"
            );
        }
        let licence = Some(licence.trim().to_owned()).filter(|l| !l.is_empty());
        crates.insert(TreeCrate {
            name: name.to_owned(),
            version: version.to_owned(),
            licence,
        });
    }
    Ok(crates.into_iter().collect())
}

/// A path compared without case or separator style, ending in a separator.
fn path_key(path: &Path) -> String {
    let mut key = path.to_string_lossy().to_lowercase().replace('/', "\\");
    if !key.ends_with('\\') {
        key.push('\\');
    }
    key
}

/// The `CARGO_HOME`s that may hold the crates' sources, in the order they are asked: the one the
/// release build used ([`crate::build::neutral_cargo_home`]), then the shell's own, each only when
/// it holds registry sources. After a `Cargo.lock` change the neutral one lacks the new crates
/// until the next release build; the shell's (which `cargo xtask ci` fills) has them.
#[must_use]
pub fn cargo_homes() -> Vec<PathBuf> {
    let neutral = crate::build::neutral_cargo_home(std::env::var_os("SystemDrive"));
    let shell = std::env::var_os("CARGO_HOME").map_or_else(
        || PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join(".cargo"),
        PathBuf::from,
    );
    let mut homes = Vec::new();
    for home in [neutral, shell] {
        if home.join("registry").join("src").is_dir() && !homes.contains(&home) {
            homes.push(home);
        }
    }
    homes
}

/// The crates of the shipped app for `target` and `features`, from `cargo tree` (offline) with the
/// first of `homes` it succeeds with.
///
/// # Errors
/// No home given, or `cargo tree` failed, was cut off or unreadable with every one.
pub fn shipped_crates(
    root: &Path,
    target: &str,
    features: &[String],
    homes: &[PathBuf],
) -> Result<Vec<TreeCrate>> {
    let args = tree_args(target, features);
    let what = format!("`cargo {}`", args.join(" "));
    let mut failures = Vec::new();
    for home in homes {
        let mut cmd = crate::ci::cargo_command();
        cmd.args(&args).current_dir(root).env("CARGO_HOME", home);
        let out = crate::proc::run_with_timeout(&mut cmd, TREE_TIMEOUT)
            .with_context(|| format!("{what} could not start or took too long"))?;
        out.require_complete(&what)?;
        if out.success {
            return parse_tree(&out.stdout, root);
        }
        failures.push(format!("with {}: {}", home.display(), out.describe()));
    }
    bail!(
        "{what} failed with every cargo home (run the release build or `cargo xtask ci` first, so \
         the crates are there): {}",
        if failures.is_empty() {
            "no cargo home holds registry sources".to_owned()
        } else {
            failures.join("\n")
        }
    )
}

/// The source folder of `name` `version` below `cargo_home`'s registry sources: the first
/// registry (by name) that holds it.
///
/// # Errors
/// No registry holds it.
pub fn crate_dir(cargo_home: &Path, name: &str, version: &str) -> Result<PathBuf> {
    let src = cargo_home.join("registry").join("src");
    let mut registries: Vec<PathBuf> = std::fs::read_dir(&src)
        .with_context(|| format!("read {}", src.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    registries.sort();
    registries
        .into_iter()
        .map(|r| r.join(format!("{name}-{version}")))
        .find(|d| d.join("Cargo.toml").is_file())
        .with_context(|| {
            format!(
                "{name} {version} has no sources under {}: run the release build first",
                src.display()
            )
        })
}

/// The crates of `tree` as packages, their folders in the first of `homes` that holds them. Where
/// another home holds the crate too, its licence files must be the same bytes, so the notices are
/// the same whichever home supplied them.
///
/// # Errors
/// A crate no home holds, or two homes whose copies' licence files differ.
pub fn crate_packages(tree: &[TreeCrate], homes: &[PathBuf]) -> Result<Vec<Package>> {
    tree.iter()
        .map(|c| {
            let dirs: Vec<PathBuf> = homes
                .iter()
                .filter_map(|h| crate_dir(h, &c.name, &c.version).ok())
                .collect();
            let dir = dirs.first().cloned().with_context(|| {
                format!(
                    "{} {} has no sources in any cargo home ({}): run the release build or \
                     `cargo xtask ci` first",
                    c.name,
                    c.version,
                    homes
                        .iter()
                        .map(|h| h.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            let first = licence_digest(&dir)?;
            for other in &dirs[1..] {
                ensure!(
                    licence_digest(other)? == first,
                    "{} {}'s licence files differ between {} and {}",
                    c.name,
                    c.version,
                    dir.display(),
                    other.display()
                );
            }
            Ok(Package {
                name: c.name.clone(),
                version: c.version.clone(),
                licence: c.licence.clone(),
                dir,
                url: format!("https://crates.io/crates/{}/{}", c.name, c.version),
            })
        })
        .collect()
}

/// A SHA-256 over the names and bytes of the licence files directly in `dir`.
///
/// # Errors
/// An unreadable folder or file.
pub fn licence_digest(dir: &Path) -> Result<String> {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    for (name, text) in licence_files(dir)? {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(text.as_bytes());
        hash.update([0]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The entries of [`VENDORED`] and [`DISCLAIMERS`] for the crates of `packages`.
///
/// # Errors
/// A listed file missing from its crate, or a disclaimer whose words are not in its file.
pub fn vendored_entries(packages: &[Package], target: &str) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let arch = if target == crate::build::ARM64_WINDOWS {
        "arm64"
    } else {
        "x64"
    };
    for l in LINKED_PREBUILT {
        for p in packages.iter().filter(|p| p.name == l.krate) {
            let file = l.file.replace("{arch}", arch);
            ensure!(
                p.dir.join(&file).is_file(),
                "{} {} no longer has {file} (bundle::notices::LINKED_PREBUILT)",
                p.name,
                p.version
            );
            entries.push(Entry {
                line: format!(
                    "{} | {} {}, file {file} | {}",
                    l.what, p.name, p.version, p.url
                ),
                texts: Vec::new(),
                notes: vec![format!(
                    "Copyright {}. Used under {}. No copy of that licence comes with the library \
                     in {}, so none is quoted here.",
                    l.holder, l.terms, p.name
                )],
                copyleft: Vec::new(),
            });
        }
    }
    for v in VENDORED {
        for p in packages.iter().filter(|p| p.name == v.krate) {
            let path = p.dir.join(v.file);
            ensure!(
                path.is_file(),
                "{} {} no longer has {} (bundle::notices::VENDORED): find where its licence moved",
                p.name,
                p.version,
                v.file
            );
            entries.push(Entry {
                line: format!(
                    "{} | {} {}, file {} | {}",
                    v.what, p.name, p.version, v.file, p.url
                ),
                texts: vec![read_text(&path)?],
                notes: Vec::new(),
                copyleft: Vec::new(),
            });
        }
    }
    for n in IN_FILE {
        for p in packages.iter().filter(|p| p.name == n.krate) {
            let path = p.dir.join(n.file);
            let text = read_text(&path)?;
            let lines: Vec<&str> = text.lines().collect();
            let at = lines
                .iter()
                .position(|l| l.contains(n.from))
                .with_context(|| {
                    format!(
                        "{} {}'s {} no longer has its notice starting \"{}\" \
                     (bundle::notices::IN_FILE)",
                        p.name, p.version, n.file, n.from
                    )
                })?;
            let quoted = lines
                .get(at..at + n.lines)
                .with_context(|| format!("{} {}'s {} ends early", p.name, p.version, n.file))?;
            entries.push(Entry {
                line: format!(
                    "{} | {} {}, file {} | {}",
                    n.what, p.name, p.version, n.file, p.url
                ),
                texts: vec![quoted.join("\n")],
                notes: vec!["The notice in the source file, as it stands.".to_owned()],
                copyleft: Vec::new(),
            });
        }
    }
    for d in DISCLAIMERS {
        for p in packages.iter().filter(|p| p.name == d.krate) {
            let path = p.dir.join(d.file);
            let text = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            ensure!(
                String::from_utf8_lossy(&text).contains(d.phrase),
                "{} {}'s {} no longer says \"{}\" (bundle::notices::DISCLAIMERS)",
                p.name,
                p.version,
                d.file,
                d.phrase
            );
            entries.push(Entry {
                line: format!(
                    "{} | {} {}, file {} | {}",
                    d.what, p.name, p.version, d.file, p.url
                ),
                texts: Vec::new(),
                notes: vec![format!(
                    "Its source has no licence; it says: \"{}\".",
                    d.phrase
                )],
                copyleft: Vec::new(),
            });
        }
    }
    Ok(entries)
}

/// A part of the notices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Its heading.
    pub heading: String,
    /// What it holds, said once under the heading.
    pub intro: String,
    /// Its entries, in order.
    pub entries: Vec<Entry>,
}

/// The notices' text: `title`, Earshot's own licence as text 1, every section's entries naming
/// their texts by number, then every distinct text once, in order of first use.
#[must_use]
pub fn render(title: &str, own_licence: &str, sections: &[Section]) -> String {
    let mut texts: Vec<String> = vec![normalise(own_licence)];
    let mut numbers: BTreeMap<String, usize> = BTreeMap::new();
    numbers.insert(texts[0].clone(), 1);
    let mut number = |text: &String| -> usize {
        if let Some(n) = numbers.get(text) {
            return *n;
        }
        texts.push(text.clone());
        numbers.insert(text.clone(), texts.len());
        texts.len()
    };
    let mut out = format!(
        "{title}\n\n\
         Earshot is under the MIT licence, text 1 below. It is built with the software listed \
         here, each under its own licence. Each entry gives a name, version, licence and where \
         its source is published, then the numbers of the licence texts that follow the list.\n"
    );
    for s in sections {
        out.push_str(&format!("\n\n## {}\n\n{}\n\n", s.heading, s.intro));
        for e in &s.entries {
            out.push_str(&e.line);
            out.push('\n');
            if !e.texts.is_empty() {
                let ns: Vec<String> = e.texts.iter().map(|t| number(t).to_string()).collect();
                out.push_str(&format!("    Licence texts: {}\n", ns.join(", ")));
            }
            for n in &e.notes {
                out.push_str(&format!("    {n}\n"));
            }
        }
    }
    out.push_str("\n\n## Licence texts\n");
    for (i, t) in texts.iter().enumerate() {
        out.push_str(&format!("\n\n### Text {}\n\n{t}\n", i + 1));
    }
    out
}

/// Earshot's copyright line, exactly as `LICENSE` holds it (pinned by a test). It names the owner
/// by the account name the release is built under, a name the install scan refuses in any shipped
/// file ([`super::paths`], T9.1j), so the scan allows this exact sentence and nothing else (the
/// owner's decision, 2026-11-26).
pub const OWN_COPYRIGHT: &str = "Copyright (c) 2026 Sluggy";

/// Earshot's `LICENSE` as the notices hold it: its first copyright line written as
/// [`OWN_COPYRIGHT`] (so the scan's one allowed sentence is the only form of the name), any other
/// copyright line dropped.
#[must_use]
pub fn own_licence(licence: &str) -> String {
    let mut placed = false;
    let mut text = normalise(licence)
        .lines()
        .filter_map(|l| {
            if l.trim_start().starts_with("Copyright") {
                if placed {
                    return None;
                }
                placed = true;
                return Some(OWN_COPYRIGHT);
            }
            Some(l.trim_end())
        })
        .collect::<Vec<_>>()
        .join("\n");
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    if placed {
        text.trim().to_owned()
    } else {
        format!("{}\n\n{OWN_COPYRIGHT}", text.trim())
    }
}

/// The `name version` of every package in `expected` whose entry line `text` lacks.
#[must_use]
pub fn missing_names(text: &str, expected: &[(String, String)]) -> Vec<String> {
    expected
        .iter()
        .filter(|(n, v)| !text.contains(&format!("\n{}", package_line_start(n, v))))
        .map(|(n, v)| format!("{n} {v}"))
        .collect()
}

/// The folders whose production dependencies make the app's window (`vite build` of
/// `apps/desktop`, which imports `packages/ui`).
pub const NPM_ROOTS: &[&str] = &["apps/desktop", "packages/ui"];

/// A `package.json`, as far as the notices read it.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct PackageJson {
    /// Its name.
    #[serde(default)]
    pub name: String,
    /// Its version.
    #[serde(default)]
    pub version: String,
    /// Its licence: an SPDX string, or the older `{ "type": ... }` object.
    #[serde(default)]
    pub license: Option<serde_json::Value>,
    /// The oldest form: a list of `{ "type": ... }` objects, any of which may be chosen.
    #[serde(default)]
    pub licenses: Option<Vec<serde_json::Value>>,
    /// Needed at run time.
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    /// Needed when installed.
    #[serde(default, rename = "optionalDependencies")]
    pub optional: BTreeMap<String, String>,
    /// Provided by whoever depends on it.
    #[serde(default, rename = "peerDependencies")]
    pub peer: BTreeMap<String, String>,
}

impl PackageJson {
    /// Its licence expression, from whichever form it uses.
    #[must_use]
    pub fn licence(&self) -> Option<String> {
        let of = |v: &serde_json::Value| -> Option<String> {
            v.as_str()
                .or_else(|| v.get("type").and_then(|t| t.as_str()))
                .map(str::to_owned)
        };
        if let Some(l) = self.license.as_ref().and_then(of) {
            return Some(l);
        }
        let all: Vec<String> = self.licenses.iter().flatten().filter_map(of).collect();
        (!all.is_empty()).then(|| all.join(" OR "))
    }
}

/// Reads `<dir>/package.json`.
///
/// # Errors
/// An unreadable or malformed file.
pub fn read_package_json(dir: &Path) -> Result<PackageJson> {
    let path = dir.join("package.json");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Where Node finds `name` from the package in `from`: `node_modules/<name>` in `from` or the
/// nearest folder above it that has one, followed to the real folder (pnpm links each package's
/// dependencies beside it in its own `node_modules`).
#[must_use]
pub fn node_resolve(from: &Path, name: &str) -> Option<PathBuf> {
    from.ancestors().find_map(|dir| {
        let mut path = dir.join("node_modules");
        path.extend(name.split('/'));
        path.join("package.json")
            .is_file()
            .then(|| path.canonicalize().ok())
            .flatten()
    })
}

/// Whether the real folder `dir` is a package installed from the registry, rather than one of the
/// workspace's own (whose licence is Earshot's).
fn installed(dir: &Path) -> bool {
    dir.components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("node_modules"))
}

/// The packages the app's window is built from: the production dependency closure of
/// [`NPM_ROOTS`] (`dependencies`, and `optionalDependencies` and `peerDependencies` that are
/// installed), each once, sorted, without the workspace's own.
///
/// # Errors
/// A root or package that cannot be read, or a dependency that is not installed (run
/// `pnpm install`).
pub fn npm_packages(root: &Path) -> Result<Vec<Package>> {
    let mut pending: Vec<PathBuf> = Vec::new();
    for r in NPM_ROOTS {
        let dir = root.join(r);
        pending.push(
            dir.canonicalize()
                .with_context(|| format!("find {}", dir.display()))?,
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut found: BTreeMap<(String, String), Package> = BTreeMap::new();
    while let Some(dir) = pending.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        let json = read_package_json(&dir)?;
        for name in json.dependencies.keys() {
            pending.push(node_resolve(&dir, name).with_context(|| {
                format!(
                    "{} needs {name}, which is not installed: run `pnpm install --frozen-lockfile`",
                    json.name
                )
            })?);
        }
        for name in json.optional.keys().chain(json.peer.keys()) {
            pending.extend(node_resolve(&dir, name));
        }
        if installed(&dir) {
            found
                .entry((json.name.clone(), json.version.clone()))
                .or_insert_with(|| Package {
                    licence: json.licence(),
                    url: format!(
                        "https://www.npmjs.com/package/{}/v/{}",
                        json.name, json.version
                    ),
                    name: json.name,
                    version: json.version,
                    dir,
                });
        }
    }
    Ok(found.into_values().collect())
}

/// The entries of `packages`, and the file-level copyleft among them (by `name version`).
///
/// # Errors
/// As [`package_entry`].
fn entries_of(packages: &[Package], copyleft: &mut Vec<String>) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut failed = Vec::new();
    for p in packages {
        let e = match package_entry(p) {
            Ok(e) => e,
            Err(e) => {
                failed.push(format!("{e:#}"));
                continue;
            }
        };
        if !e.copyleft.is_empty() {
            copyleft.push(format!(
                "{} {} ({})",
                p.name,
                p.version,
                e.copyleft.join(", ")
            ));
        }
        entries.push(e);
    }
    ensure!(
        failed.is_empty(),
        "{} packages cannot ship as they are:\n{}",
        failed.len(),
        failed.join("\n")
    );
    Ok(entries)
}

/// An artefact of `manifest` by id.
fn artefact<'a>(
    manifest: &'a crate::manifest::Manifest,
    id: &str,
) -> Result<&'a crate::manifest::Artefact> {
    manifest
        .artefacts
        .iter()
        .find(|a| a.id == id)
        .with_context(|| format!("no artefact {id} in its manifest"))
}

/// The entries for the files an install holds that no package above made, and for what the app
/// downloads beside it, from the manifests that pin them: the Vulkan loader (x64), the Visual C++
/// runtime, the QNN files (ARM64), the uninstaller's NSIS code and plugin (NSIS's `COPYING` from
/// `tools`, the bundle's placed NSIS) and ONNX Runtime.
///
/// # Errors
/// An unreadable manifest, an artefact missing from it, a licence not allowed or with no text, or
/// an ONNX Runtime whose licence files are not pinned members.
pub fn other_entries(
    root: &Path,
    target: &str,
    features: &[String],
    tools: &Path,
) -> Result<Vec<Entry>> {
    let bundler = crate::manifest::Manifest::load(&root.join(super::TOOLS_MANIFEST))?;
    let models = crate::manifest::Manifest::load(&crate::models::manifest_path(root))?;
    let mut entries = Vec::new();
    let vc = super::vc_runtime_dlls(target);
    entries.push(Entry {
        line: format!(
            "{} | Microsoft Visual C++ runtime, copied from Visual Studio's VC\\Redist folder",
            vc.join(", ")
        ),
        texts: Vec::new(),
        notes: vec![
            "Redistributed under the Microsoft Visual Studio licence terms, which list these \
             files as redistributable."
                .to_owned(),
        ],
        copyleft: Vec::new(),
    });
    if super::has_vulkan_summaries(features) {
        let a = artefact(&bundler, "vulkan-runtime")?;
        checked_licence(&a.attribution, Some(&a.licence))?;
        entries.push(Entry {
            line: format!(
                "{} | {} | {}",
                super::VULKAN_LOADER,
                a.licence,
                a.attribution
            ),
            texts: Vec::new(),
            notes: vec![format!(
                "Its licence text is {} beside earshot.exe.",
                super::VULKAN_LICENCE
            )],
            copyleft: Vec::new(),
        });
    }
    if target == crate::build::ARM64_WINDOWS {
        let a = artefact(&models, super::QNN_ARTEFACT)?;
        entries.push(Entry {
            line: format!(
                "onnxruntime_providers_qnn.dll and the Qualcomm AI Runtime libraries | {} | {}",
                a.licence, a.attribution
            ),
            texts: Vec::new(),
            notes: vec![format!(
                "Their notice is {} beside earshot.exe, with {}, {} and Qualcomm_LICENSE.pdf.",
                super::QNN_NOTICE,
                super::QNN_LICENSE_IN_INSTALL,
                super::QNN_NOTICES_IN_INSTALL
            )],
            copyleft: Vec::new(),
        });
    }
    // NSIS is not judged by the allow-list: its terms (zlib/libpng, with bzip2 and CPL-1.0 for
    // the compression modules, the LZMA stub in every installer) were read for the installer's
    // approval (`xtask/bundler-tools.toml`), and the uninstaller carries its code unmodified.
    let nsis = artefact(&bundler, "nsis")?;
    let copying = tools.join("NSIS").join("COPYING");
    entries.push(Entry {
        line: format!("uninstall.exe | {} | {}", nsis.licence, nsis.attribution),
        texts: vec![read_text(&copying)?],
        notes: vec![
            "The installer and uninstaller are made with NSIS and carry its code, unchanged."
                .to_owned(),
            "The source code of NSIS, its LZMA compression module included, is available from \
             the NSIS project at https://nsis.sourceforge.io."
                .to_owned(),
        ],
        copyleft: Vec::new(),
    });
    let utils = artefact(&bundler, "nsis-tauri-utils")?;
    let (_, chosen) = checked_licence(&utils.attribution, Some(&utils.licence))?;
    let mut notes = vec!["A plugin of the installer and uninstaller.".to_owned()];
    notes.extend(standard_text_notes("plugin", &chosen));
    entries.push(Entry {
        line: format!(
            "nsis_tauri_utils.dll | {} | {}",
            utils.licence, utils.attribution
        ),
        texts: standard_texts(&utils.attribution, &chosen)?,
        notes,
        copyleft: Vec::new(),
    });
    let arch = if target == crate::build::ARM64_WINDOWS {
        "arm64"
    } else {
        "x64"
    };
    let ort = artefact(&models, &format!("onnxruntime-win-{arch}"))?;
    let members: Vec<&str> = ort
        .extract
        .iter()
        .flat_map(|e| e.members.iter().map(|m| m.path.as_str()))
        .collect();
    for file in ["LICENSE", "ThirdPartyNotices.txt"] {
        ensure!(
            members.iter().any(|m| m.ends_with(&format!("/{file}"))),
            "{} no longer pins its {file} (models/manifest.toml)",
            ort.id
        );
    }
    entries.push(Entry {
        line: format!(
            "onnxruntime.dll, downloaded after installation | {} | {}",
            ort.licence, ort.attribution
        ),
        texts: Vec::new(),
        notes: vec![
            "Downloaded into the models folder with the speech helpers, together with its LICENSE \
             and ThirdPartyNotices.txt."
                .to_owned(),
        ],
        copyleft: Vec::new(),
    });
    Ok(entries)
}

/// The sections' headings, in order: what an installed file is judged by when the tree cannot be
/// read ([`installed_file_problems`]).
pub const SECTION_CRATES: &str = "Rust crates compiled into earshot.exe";
/// See [`SECTION_CRATES`].
pub const SECTION_STD: &str = "The Rust standard library";
/// See [`SECTION_CRATES`].
pub const SECTION_NATIVE: &str = "Native libraries";
/// See [`SECTION_CRATES`].
pub const SECTION_NPM: &str = "JavaScript packages in the app's window";
/// See [`SECTION_CRATES`].
pub const SECTION_OTHER: &str = "Other files";
/// Every heading of the file, in order.
pub const SECTIONS: [&str; 6] = [
    SECTION_CRATES,
    SECTION_STD,
    SECTION_NATIVE,
    SECTION_NPM,
    SECTION_OTHER,
    "Licence texts",
];

/// What is wrong with an installed notices `text` judged by itself, for a machine that cannot
/// read the shipped tree (`bundle-check --installer` without crate sources or `node_modules`):
/// Earshot's licence first, every heading in order, every package section holding entries, and
/// every licence text an entry names present. Empty means none found.
#[must_use]
pub fn installed_file_problems(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if text.trim().is_empty() {
        return vec!["the file is empty or missing".to_owned()];
    }
    if !(text.contains("### Text 1\n\nMIT License") && text.contains(OWN_COPYRIGHT)) {
        problems.push("Earshot's own licence is not text 1".to_owned());
    }
    let mut at = 0;
    let mut starts = Vec::new();
    for heading in SECTIONS {
        match text[at..].find(&format!("\n## {heading}\n")) {
            Some(i) => {
                at += i + 1;
                starts.push(at);
            }
            None => problems.push(format!("no section `{heading}` in order")),
        }
    }
    if starts.len() == SECTIONS.len() {
        for (i, heading) in [SECTION_CRATES, SECTION_NPM].iter().enumerate() {
            let k = if i == 0 { 0 } else { 3 };
            let body = &text[starts[k]..starts[k + 1]];
            if !body
                .lines()
                .any(|l| l.contains(" | ") && !l.starts_with(' '))
            {
                problems.push(format!("section `{heading}` lists nothing"));
            }
        }
    }
    let texts = text.matches("\n### Text ").count();
    for line in text.lines() {
        if let Some(list) = line.trim_start().strip_prefix("Licence texts: ") {
            for n in list.split(", ") {
                if n.parse::<usize>().map_or(true, |n| n == 0 || n > texts) {
                    problems.push(format!("an entry names text {n}, which is not there"));
                }
            }
        }
    }
    problems
}

/// The Rust standard library's entry: dual-licensed MIT or Apache-2.0 (as every Rust toolchain's
/// `share\doc\rust\COPYRIGHT-library.html` says), with those two texts read from the pinned
/// toolchain's `licenses` folder when `rustc --print sysroot` finds it, else by reference.
///
/// # Errors
/// An unreadable `rust-toolchain.toml`.
pub fn rust_std_entry(root: &Path) -> Result<Entry> {
    let toolchain: toml::Table = std::fs::read_to_string(root.join("rust-toolchain.toml"))
        .context("read rust-toolchain.toml")?
        .parse()
        .context("parse rust-toolchain.toml")?;
    let channel = toolchain
        .get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(|c| c.as_str())
        .context("rust-toolchain.toml has no channel")?
        .to_owned();
    let mut cmd = std::process::Command::new("rustc");
    cmd.args(["--print", "sysroot"]).current_dir(root);
    let docs = crate::proc::run_with_timeout(&mut cmd, Duration::from_secs(60))
        .filter(|o| o.success && o.complete())
        .map(|o| {
            PathBuf::from(o.stdout.trim())
                .join("share")
                .join("doc")
                .join("rust")
        });
    let licences = docs.as_ref().map(|d| d.join("licenses"));
    let texts: Option<Vec<String>> = licences.as_ref().and_then(|l| {
        ["MIT.txt", "Apache-2.0.txt"]
            .iter()
            .map(|f| read_text(&l.join(f)).ok())
            .collect()
    });
    let mut notes = vec![
        "Earshot uses it under MIT, as every Rust program may; its own notices for the code it \
         is built from are COPYRIGHT-library.html in the Rust toolchain's documentation \
         (share\\doc\\rust)."
            .to_owned(),
    ];
    let texts = texts.unwrap_or_else(|| {
        notes.push(
            "The toolchain's own licence files were not found where this file was made: the \
             texts are those of https://github.com/rust-lang/rust, LICENSE-MIT and \
             LICENSE-APACHE."
                .to_owned(),
        );
        Vec::new()
    });
    // Only the side taken, as for crates: MIT.
    let texts: Vec<String> = texts
        .into_iter()
        .filter(|t| identify("MIT.txt", t).contains(&"MIT"))
        .collect();
    Ok(Entry {
        line: format!(
            "Rust standard library {channel} | MIT OR Apache-2.0 | https://github.com/rust-lang/rust"
        ),
        texts,
        notes,
        copyleft: Vec::new(),
    })
}

/// A library a crate links in prebuilt, whose licence text the crate does not carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkedPrebuilt {
    /// The crate that carries it.
    pub krate: &'static str,
    /// What it is.
    pub what: &'static str,
    /// The file linked, below the crate's folder, `{arch}` standing for `x64` or `arm64`.
    pub file: &'static str,
    /// Who holds its copyright, as its own version resource says.
    pub holder: &'static str,
    /// Where its terms are published.
    pub terms: &'static str,
}

/// Microsoft's WebView2 loader, which `webview2-com-sys`'s build script links into the exe
/// (`WebView2LoaderStatic.lib`). Neither the crate nor this machine (no NuGet cache) holds the
/// WebView2 SDK's licence text (looked for 2026-11-26), so the entry names the component, its
/// holder and where its terms are published, and quotes no text.
pub const LINKED_PREBUILT: &[LinkedPrebuilt] = &[LinkedPrebuilt {
    krate: "webview2-com-sys",
    what: "Microsoft Edge WebView2 loader (WebView2LoaderStatic.lib), linked into earshot.exe",
    file: "{arch}/WebView2LoaderStatic.lib",
    holder: "Microsoft Corporation",
    terms: "the licence of the Microsoft.Web.WebView2 SDK, published with it at \
            https://www.nuget.org/packages/Microsoft.Web.WebView2",
}];

/// The notices of the app for `target` built with `features`, and the packages under file-level
/// copyleft (by `name version`), for the owner. `tools` is the bundle's tools folder
/// (`<target-dir>\.tauri`), whose NSIS the installer is made with.
///
/// # Errors
/// As [`shipped_crates`], [`crate_packages`], [`npm_packages`], [`package_entry`],
/// [`vendored_entries`] and [`other_entries`]; an unreadable `LICENSE`.
pub fn notices(
    root: &Path,
    target: &str,
    features: &[String],
    tools: &Path,
) -> Result<(String, Vec<String>)> {
    let homes = cargo_homes();
    let tree = shipped_crates(root, target, features, &homes)?;
    let packages = crate_packages(&tree, &homes)?;
    let mut copyleft = Vec::new();
    let crates = entries_of(&packages, &mut copyleft)?;
    let npm = entries_of(&npm_packages(root)?, &mut copyleft)?;
    let sections = vec![
        Section {
            heading: SECTION_CRATES.to_owned(),
            intro: "Each crate's licence files as it publishes them; where it publishes none, \
                    the standard text of its licence. Where a crate offers a choice of licences, \
                    its entry says which one Earshot uses and gives only that one's text."
                .to_owned(),
            entries: crates,
        },
        Section {
            heading: SECTION_STD.to_owned(),
            intro: "Rust's standard library, compiled into earshot.exe with every Rust program."
                .to_owned(),
            entries: vec![rust_std_entry(root)?],
        },
        Section {
            heading: SECTION_NATIVE.to_owned(),
            intro: "Libraries compiled from the sources a crate above carries, with the licence \
                    files those sources hold and the notices inside their source files, and \
                    libraries a crate above links in as it publishes them."
                .to_owned(),
            entries: vendored_entries(&packages, target)?,
        },
        Section {
            heading: SECTION_NPM.to_owned(),
            intro: "The packages the window's page is built from, each with its licence files \
                    as it publishes them; where it publishes none, the standard text of its \
                    licence."
                .to_owned(),
            entries: npm,
        },
        Section {
            heading: SECTION_OTHER.to_owned(),
            intro: "Files beside earshot.exe that no package above made, and ONNX Runtime, which \
                    the app downloads."
                .to_owned(),
            entries: other_entries(root, target, features, tools)?,
        },
    ];
    let own = own_licence(&std::fs::read_to_string(root.join("LICENSE")).context("read LICENSE")?);
    let version = super::workspace_version(root)?;
    Ok((
        render(
            &format!("# Third-party notices: Earshot {version} for {target}"),
            &own,
            &sections,
        ),
        copyleft,
    ))
}

/// Writes [`notices`] beside the built exe as [`NOTICES_FILE`] and prints the packages under
/// file-level copyleft. Returns the file's path.
///
/// # Errors
/// As [`notices`]; a failed write.
pub fn write_notices(
    root: &Path,
    exe_dir: &Path,
    target: &str,
    features: &[String],
    tools: &Path,
) -> Result<PathBuf> {
    let (text, copyleft) = notices(root, target, features, tools)?;
    for p in &copyleft {
        println!("xtask build: {NOTICES_FILE}: under file-level copyleft, used unmodified: {p}");
    }
    let path = exe_dir.join(NOTICES_FILE);
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// `(name, version)` of every package the notices for `target` must name, by the same rules as
/// [`notices`]: what `bundle-check` requires of an installed file.
///
/// # Errors
/// As [`shipped_crates`] and [`npm_packages`].
pub fn expected_names(
    root: &Path,
    target: &str,
    features: &[String],
) -> Result<Vec<(String, String)>> {
    let tree = shipped_crates(root, target, features, &cargo_homes())?;
    let mut names: Vec<(String, String)> = tree.into_iter().map(|c| (c.name, c.version)).collect();
    names.extend(npm_packages(root)?.into_iter().map(|p| (p.name, p.version)));
    Ok(names)
}

#[cfg(test)]
mod tests;
