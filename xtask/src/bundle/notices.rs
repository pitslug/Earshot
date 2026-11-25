//! The install's third-party notices (T14.37): [`NOTICES_FILE`] beside `earshot.exe`, written by
//! `cargo xtask build --bundle` from facts, never by hand.
//!
//! - **Rust crates.** The shipped app's normal dependency graph for the target and the features it
//!   is built with ([`tree_args`]: `cargo tree -e normal,no-proc-macro`, so build scripts and
//!   procedural macros, which put no code in the exe, are left out). Each crate's licence
//!   expression comes from cargo, its licence files (`LICENSE*`, `LICENCE*`, `COPYING*`,
//!   `NOTICE*`, `UNLICENSE*`) from its source folder in the build's own `CARGO_HOME`
//!   ([`cargo_home`]), which holds exactly what the build compiled. `cargo metadata` is not used:
//!   it needs every workspace package, dev-dependencies included, downloaded, and the build's
//!   `CARGO_HOME` holds only what the build needed (measured 2026-11-26), so offline it fails.
//! - **Native libraries** compiled from sources a crate carries ([`VENDORED`], [`DISCLAIMERS`]).
//! - **Earshot's own `LICENSE`**, first.
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

/// What makes a choice worse, compared in order: copyleft parts, parts with no standard text,
/// parts needing a holder the package may not name, then more parts.
fn cost(terms: &[Term]) -> (usize, usize, usize, usize) {
    let count = |f: &dyn Fn(&Term) -> bool| terms.iter().filter(|t| f(t)).count();
    (
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
                all.extend(choose(p)?);
            }
            Some(all)
        }
        Licence::Or(parts) => {
            let mut best: Option<Vec<Term>> = None;
            for choice in parts.iter().filter_map(choose) {
                if best.as_ref().is_none_or(|b| cost(&choice) < cost(b)) {
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

/// The entry of `p`: its licence expression checked against the allow-lists, its licence files,
/// or the standard texts of the licences chosen when it ships none.
///
/// # Errors
/// By the package's name and version: no licence expression, one not allowed, or no licence file
/// and no standard text to use.
pub fn package_entry(p: &Package) -> Result<Entry> {
    let who = format!("{} {}", p.name, p.version);
    let expr = p
        .licence
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .with_context(|| {
            format!("{who} declares no licence: a package with no licence cannot ship")
        })?;
    let parsed = parse_licence(expr).with_context(|| format!("{who}: licence `{expr}`"))?;
    let chosen = choose(&parsed).with_context(|| {
        format!(
            "{who} is under `{expr}`, which no licence on the allow-list satisfies \
             (bundle::notices::PERMISSIVE, FILE_COPYLEFT_ACCEPTED): nothing copyleft may ship \
             (AGENTS.md ground rule 11); ask the owner"
        )
    })?;
    let mut notes = Vec::new();
    let files = licence_files(&p.dir).with_context(|| format!("{who}'s licence files"))?;
    let texts = if files.is_empty() {
        let mut texts = Vec::new();
        for term in &chosen {
            let text = canonical(&term.id).with_context(|| {
                format!(
                    "{who} ships no licence file and xtask holds no standard text of {} \
                     (bundle::notices::CANONICAL)",
                    term.id
                )
            })?;
            texts.push(normalise(text));
        }
        let ids: Vec<String> = chosen.iter().map(Term::text).collect();
        notes.push(format!(
            "The package ships no licence file: the standard text of {} is used.",
            ids.join(" and ")
        ));
        if chosen.iter().any(|t| NEEDS_HOLDER.contains(&t.id.as_str())) {
            notes.push("No file in the package names its copyright holder.".to_owned());
        }
        texts
    } else {
        files.into_iter().map(|(_, t)| t).collect()
    };
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

/// The `CARGO_HOME` the release build used ([`crate::build::neutral_cargo_home`]), when it holds
/// registry sources, else the shell's own.
#[must_use]
pub fn cargo_home() -> PathBuf {
    let neutral = crate::build::neutral_cargo_home(std::env::var_os("SystemDrive"));
    if neutral.join("registry").join("src").is_dir() {
        return neutral;
    }
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return PathBuf::from(home);
    }
    PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join(".cargo")
}

/// The crates of the shipped app for `target` and `features`, from `cargo tree` with
/// `cargo_home`.
///
/// # Errors
/// A failed, cut off or unreadable `cargo tree`.
pub fn shipped_crates(
    root: &Path,
    target: &str,
    features: &[String],
    cargo_home: &Path,
) -> Result<Vec<TreeCrate>> {
    let args = tree_args(target, features);
    let mut cmd = crate::ci::cargo_command();
    cmd.args(&args)
        .current_dir(root)
        .env("CARGO_HOME", cargo_home);
    let what = format!("`cargo {}`", args.join(" "));
    let out = crate::proc::run_with_timeout(&mut cmd, TREE_TIMEOUT)
        .with_context(|| format!("{what} could not start or took too long"))?;
    out.require_complete(&what)?;
    ensure!(
        out.success,
        "{what} failed (run the release build first, so its crates are in {}): {}",
        cargo_home.display(),
        out.describe()
    );
    parse_tree(&out.stdout, root)
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

/// The crates of `tree` as packages, their folders below `cargo_home`.
///
/// # Errors
/// A crate whose folder is missing.
pub fn crate_packages(tree: &[TreeCrate], cargo_home: &Path) -> Result<Vec<Package>> {
    tree.iter()
        .map(|c| {
            Ok(Package {
                name: c.name.clone(),
                version: c.version.clone(),
                licence: c.licence.clone(),
                dir: crate_dir(cargo_home, &c.name, &c.version)?,
                url: format!("https://crates.io/crates/{}/{}", c.name, c.version),
            })
        })
        .collect()
}

/// The entries of [`VENDORED`] and [`DISCLAIMERS`] for the crates of `packages`.
///
/// # Errors
/// A listed file missing from its crate, or a disclaimer whose words are not in its file.
pub fn vendored_entries(packages: &[Package]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
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

/// Where Earshot's own copyright line is, said in place of it.
pub const OWN_COPYRIGHT: &str = "Earshot's copyright line is in LICENSE in its source repository, \
                                 https://github.com/pitslug/Earshot.";

/// Earshot's `LICENSE` as the notices hold it: without its copyright lines, which name the owner
/// by the account name the release is built under, a name the install scan refuses in any shipped
/// file ([`super::paths`], T9.1j). [`OWN_COPYRIGHT`] says where the line is instead.
#[must_use]
pub fn own_licence(licence: &str) -> String {
    let mut text = normalise(licence)
        .lines()
        .filter(|l| !l.trim_start().starts_with("Copyright"))
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    format!("{}\n\n{OWN_COPYRIGHT}", text.trim())
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

/// The notices of the app for `target` built with `features`, and the packages under file-level
/// copyleft (by `name version`), for the owner.
///
/// # Errors
/// As [`shipped_crates`], [`crate_packages`], [`package_entry`] and [`vendored_entries`]; an
/// unreadable `LICENSE`.
pub fn notices(root: &Path, target: &str, features: &[String]) -> Result<(String, Vec<String>)> {
    let home = cargo_home();
    let tree = shipped_crates(root, target, features, &home)?;
    let packages = crate_packages(&tree, &home)?;
    let mut entries = Vec::new();
    let mut copyleft = Vec::new();
    for p in &packages {
        let e = package_entry(p)?;
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
    let sections = vec![
        Section {
            heading: "Rust crates compiled into earshot.exe".to_owned(),
            intro: "Each crate's licence files as it publishes them; where it publishes none, \
                    the standard text of its licence."
                .to_owned(),
            entries,
        },
        Section {
            heading: "Native libraries built from source".to_owned(),
            intro: "Libraries compiled from the sources a crate above carries, with the licence \
                    files those sources hold."
                .to_owned(),
            entries: vendored_entries(&packages)?,
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
) -> Result<PathBuf> {
    let (text, copyleft) = notices(root, target, features)?;
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
/// As [`shipped_crates`].
pub fn expected_names(
    root: &Path,
    target: &str,
    features: &[String],
) -> Result<Vec<(String, String)>> {
    let tree = shipped_crates(root, target, features, &cargo_home())?;
    Ok(tree.into_iter().map(|c| (c.name, c.version)).collect())
}

#[cfg(test)]
mod tests;
