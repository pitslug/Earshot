//! `cargo xtask public-guard` (T9.1ka): what must never reach a public copy of the source.
//!
//! It enforces in a repo whose `HEAD` tracks the marker file [`MARKER`] at its root, or whose
//! first commit's subject is [`PUBLIC_START`], so deleting the marker does not turn it off. The
//! private archive legitimately holds these things and has neither; anywhere else it prints one
//! visible line and passes. When it enforces, over the files of `HEAD` and over history:
//!
//! - no entry of a names list held outside the repo appears in a file's bytes (UTF-8 and
//!   UTF-16, either byte order and alignment), a tracked path, a commit message, an author or
//!   committer name, or the text of any commit's diff;
//! - every author and committer email ends in [`NOREPLY_SUFFIX`];
//! - no recording or database file outside [`ALLOWED_AUDIO`], nothing under `fixtures/private/`
//!   or `gates/`, and nothing named `LOG.md`, at the tip or anywhere in history.
//!
//! The list's contents never appear in this file, its tests or its output: a finding names a
//! list line number, and a path that itself holds an entry is printed only as a short hash, so
//! a failing run in a public ci log gives nothing away. An entry matches
//! case-insensitively as a whole word (a letter on either side stops a match, a digit or
//! punctuation does not), so a short first name does not fire on every word holding it.
//!
//! A missing list is a visible warning and the other checks still run, so a contributor's own
//! ci passes without one; `--require-list`, which the pre-push hook passes, fails instead.

use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::proc::{CmdOutput, run_with_timeout};

/// The tracked file at the repo root that turns the guard on.
pub const MARKER: &str = "PUBLIC_REPO";

/// The subject of the public repo's first commit, which also turns the guard on.
pub const PUBLIC_START: &str = "Earshot: first public source";

/// What every author and committer email must end in, compared in lower case.
pub const NOREPLY_SUFFIX: &str = "@users.noreply.github.com";

/// Overrides the names list's default path.
pub const LIST_ENV: &str = "EARSHOT_PRIVATE_NAMES";

/// The list's place under the home folder. It is not under `%LOCALAPPDATA%` because agent
/// shells' writes there are redirected and invisible to the user's own shells.
const LIST_IN_HOME: &[&str] = &[".earshot-private", "names.txt"];

/// File types that count as a recording or a database, in lower case without the dot.
pub const RECORDING_EXTENSIONS: &[&str] = &[
    "flac", "wav", "mp3", "m4a", "ogg", "opus", "db", "sqlite", "sqlite3", "db-wal", "db-shm",
    "aac", "wma", "webm", "mp4", "pcm", "raw", "rttm",
];

/// The only recordings a public repo may track: public audio already in the project
/// (LibriSpeech smoke clips and the AMI benchmark meeting, whose licences are in the repo).
/// Exact paths, so a new file in those folders needs a deliberate edit here.
pub const ALLOWED_AUDIO: &[&str] = &[
    "apps/desktop/src-tauri/resources/benchmark/mic.flac",
    "apps/desktop/src-tauri/resources/benchmark/reference.flac",
    "fixtures/smoke/121-121726-0002.flac",
    "fixtures/smoke/121-121726-0004.flac",
    "fixtures/smoke/121-121726-0005.flac",
    "fixtures/smoke/121-121726-0006.flac",
    "fixtures/smoke/121-121726-0008.flac",
    "fixtures/smoke/121-121726-0011.flac",
    "fixtures/smoke/121-121726-0012.flac",
    "fixtures/smoke/121-121726-0013.flac",
    "fixtures/smoke/121-121726-0014.flac",
    "fixtures/smoke/121-127105-0004.flac",
    "fixtures/smoke/61-70968-0000.flac",
    "fixtures/smoke/61-70968-0001.flac",
    "fixtures/smoke/61-70968-0002.flac",
    "fixtures/smoke/61-70968-0003.flac",
    "fixtures/smoke/61-70968-0004.flac",
    "fixtures/smoke/61-70968-0006.flac",
    "fixtures/smoke/61-70968-0007.flac",
    "fixtures/smoke/61-70968-0008.flac",
    "fixtures/smoke/61-70968-0009.flac",
    "fixtures/smoke/61-70968-0012.flac",
];

/// Compressed file types whose bytes are close to random: a short entry would match them by
/// chance, so they are scanned only for entries of at least [`NOISY_MIN_CHARS`] characters,
/// and history's diff text leaves them out (a recording in history fails on its path). The
/// text a PNG or FLAC file carries (metadata) gets the whole list: [`embedded_text`].
const NOISY_EXTENSIONS: &[&str] = &["flac", "png", "ico", "icns"];

/// At this length a chance match in tens of megabytes of compressed bytes is negligible
/// (each case-insensitive letter is about 1 in 128, so six are about 1 in 4 trillion).
const NOISY_MIN_CHARS: usize = 6;

/// The licence at the repo root: a copyright line in it may name a person, and only there.
const LICENCE_FILE: &str = "LICENSE";

/// The most text taken from all of a PNG's compressed chunks; past it the file is refused.
const MAX_INFLATED: u64 = 16 * 1024 * 1024;

/// Deadline for a short git query.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// Deadline for reading all of history's diff text.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(600);

/// How the guard runs.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Fail on a missing or empty names list, instead of warning.
    pub require_list: bool,
    /// The names list's path; `None` when no home folder is known.
    pub list_path: Option<PathBuf>,
}

impl Config {
    /// The configuration from this process's environment.
    pub fn from_env(require_list: bool) -> Self {
        Self {
            require_list,
            list_path: list_path_from(
                std::env::var_os(LIST_ENV),
                std::env::var_os("USERPROFILE"),
                std::env::var_os("HOME"),
            ),
        }
    }
}

/// `over` when set and not empty, else the default place under the first home folder known.
pub fn list_path_from(
    over: Option<OsString>,
    userprofile: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    let non_empty = |v: Option<OsString>| v.filter(|s| !s.is_empty());
    if let Some(path) = non_empty(over) {
        return Some(PathBuf::from(path));
    }
    let mut path = PathBuf::from(non_empty(userprofile).or(non_empty(home))?);
    path.extend(LIST_IN_HOME);
    Some(path)
}

/// One thing that must not go public. Its text never holds a list entry or a personal email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// List line `line` appears in the tracked file or path `path`. A `path` that holds an
    /// entry is already a [`safe_label`].
    NameInFile { line: usize, path: String },
    /// List line `line` appears in a commit's message, author or committer name.
    NameInCommit { line: usize, commit: String },
    /// List line `line` appears in the text of a commit's diff.
    NameInDiff { line: usize, commit: String },
    /// A commit's author or committer email is not a noreply address.
    Identity { commit: String, who: &'static str },
    /// A recording or database file outside [`ALLOWED_AUDIO`].
    Recording { path: String, in_history: bool },
    /// A file the public repo must not hold (`LOG.md`, `gates/`, `fixtures/private/`).
    PrivatePath { path: String, in_history: bool },
    /// A PNG's compressed text is larger than [`MAX_INFLATED`], so it was not all read.
    TooMuchEmbeddedText { path: String },
    /// A copyright line of the root licence holds an email address.
    EmailInLicence,
    /// The repo's first commit says it is the public repo, and the marker is gone.
    MarkerGone,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = |in_history: &bool| if *in_history { " (in history)" } else { "" };
        match self {
            Self::NameInFile { line, path } => {
                write!(f, "names list line {line} found in tracked file {path}")
            }
            Self::NameInCommit { line, commit } => write!(
                f,
                "names list line {line} found in the message or names of commit {commit}"
            ),
            Self::NameInDiff { line, commit } => {
                write!(
                    f,
                    "names list line {line} found in the diff of commit {commit}"
                )
            }
            Self::Identity { commit, who } => write!(
                f,
                "commit {commit}: {who} email does not end in {NOREPLY_SUFFIX}"
            ),
            Self::Recording { path, in_history } => write!(
                f,
                "recording or database file {path}{} is not on the allowlist",
                place(in_history)
            ),
            Self::PrivatePath { path, in_history } => write!(
                f,
                "{path}{} must not be in a public repo",
                place(in_history)
            ),
            Self::TooMuchEmbeddedText { path } => {
                write!(f, "{path} holds more compressed text than the guard reads")
            }
            Self::EmailInLicence => {
                write!(
                    f,
                    "a copyright line of {LICENCE_FILE} holds an email address"
                )
            }
            Self::MarkerGone => write!(f, "{MARKER} is gone from a public repo"),
        }
    }
}

/// `path`, or a short hash and its extension when the path itself holds a list entry, so no
/// name reaches a log. The extension goes too if it is the part that matches.
fn safe_label(path: &str, list: &[Entry]) -> String {
    if hits(path, list).is_empty() {
        return path.to_owned();
    }
    let digest = crate::download::hex(&<sha2::Sha256 as sha2::Digest>::digest(path.as_bytes()));
    let ext = extension(path).filter(|e| hits(e, list).is_empty());
    match ext {
        Some(ext) => format!("<path hidden, sha256 {}, .{ext}>", &digest[..12]),
        None => format!("<path hidden, sha256 {}>", &digest[..12]),
    }
}

/// What a run found.
#[derive(Debug, Default)]
pub struct Report {
    /// False when the repo has no tracked [`MARKER`]: nothing was checked.
    pub enforced: bool,
    /// Why the names were not checked, when they were not (a marked repo with no list).
    pub list_problem: Option<String>,
    /// Everything that must not go public.
    pub findings: Vec<Finding>,
    /// Tracked files looked at.
    pub files: usize,
    /// Commits looked at.
    pub commits: usize,
    /// Names list entries used.
    pub entries: usize,
}

/// The ci step: enforces in a marked repo and warns on a missing list.
pub fn run(root: &Path) -> Result<()> {
    run_with(root, &Config::from_env(false))
}

/// Runs the guard, prints its visible lines and fails on any finding.
pub fn run_with(root: &Path, config: &Config) -> Result<()> {
    let report = check(root, config)?;
    if !report.enforced {
        println!("public-guard: not checked, this repo tracks no {MARKER} marker");
        return Ok(());
    }
    if let Some(problem) = &report.list_problem {
        println!("public-guard: WARNING: {problem}; names were not checked");
    }
    if !report.findings.is_empty() {
        for finding in &report.findings {
            eprintln!("public-guard: {finding}");
        }
        bail!(
            "public-guard: {} problem(s) in a public repo",
            report.findings.len()
        );
    }
    println!(
        "public-guard: ok ({} tracked files, {} commits, {} list entries)",
        report.files, report.commits, report.entries
    );
    Ok(())
}

/// Does the checks and returns what they found; errors are checks that could not be done.
pub fn check(root: &Path, config: &Config) -> Result<Report> {
    let files = tracked_files(root)?;
    let marked = files.iter().any(|f| f == MARKER);
    let public_start = first_commit_says_public(root)?;
    let mut report = Report {
        enforced: marked || public_start,
        files: files.len(),
        ..Report::default()
    };
    if !report.enforced {
        return Ok(report);
    }
    if !marked {
        report.findings.push(Finding::MarkerGone);
    }
    let list = match read_list(config.list_path.as_deref())? {
        Ok(list) => list,
        Err(problem) if config.require_list => {
            bail!("public-guard: {problem}, and this run requires it (--require-list)")
        }
        Err(problem) => {
            report.list_problem = Some(problem);
            Vec::new()
        }
    };
    report.entries = list.len();

    for path in &files {
        check_path(path, false, &list, &mut report.findings);
    }
    let in_history = history_paths(root)?;
    for path in in_history.iter().filter(|p| !files.contains(p)) {
        check_path(path, true, &list, &mut report.findings);
    }

    let identities = commit_identities(root)?;
    report.commits = identities.len();
    report.findings.extend(identity_findings(&identities));

    scan_head(root, &files, &list, &mut report.findings)?;
    if !list.is_empty() {
        for path in in_history.iter().filter(|p| !files.contains(p)) {
            for entry in hits(path, &list) {
                report.findings.push(Finding::NameInFile {
                    line: entry,
                    path: format!("{} (in history)", safe_label(path, &list)),
                });
            }
        }
        scan_commit_text(root, &list, &identities, &mut report.findings)?;
        scan_history_diffs(root, &list, &mut report.findings)?;
    }
    Ok(report)
}

/// Whether any first commit reachable from `HEAD` has the subject [`PUBLIC_START`].
fn first_commit_says_public(root: &Path) -> Result<bool> {
    let subjects = git_output(
        root,
        &["log", "--max-parents=0", "--format=%s", "HEAD"],
        GIT_TIMEOUT,
    )?;
    if subjects.trim().is_empty() {
        bail!("public-guard: git log listed no first commit; refusing to call that clean");
    }
    Ok(subjects.lines().any(|s| s == PUBLIC_START))
}

/// A names list entry, in lower case, with its line number in the file.
#[derive(Clone)]
pub struct Entry {
    line: usize,
    text: String,
}

/// Parses the list: one entry per line, blank lines and `#` comments skipped, a BOM ignored.
pub fn parse_list(text: &str) -> Vec<Entry> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line = line.trim();
            (!line.is_empty() && !line.starts_with('#')).then(|| Entry {
                line: i + 1,
                text: line.to_lowercase(),
            })
        })
        .collect()
}

/// The entries, or the reason there are none. A list that exists but cannot be read as UTF-8
/// is an error whatever the mode, because it was meant to be used.
fn read_list(path: Option<&Path>) -> Result<std::result::Result<Vec<Entry>, String>> {
    let place = format!("%USERPROFILE%\\{} or {LIST_ENV}", LIST_IN_HOME.join("\\"));
    let Some(path) = path else {
        return Ok(Err(format!("no home folder, so no names list ({place})")));
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err(format!("no names list ({place})")));
        }
        Err(e) => return Err(e).context("public-guard: cannot read the names list"),
    };
    let text = String::from_utf8(bytes)
        .context("public-guard: the names list is not UTF-8; save it as UTF-8")?;
    // UTF-16 ASCII without a BOM is valid UTF-8 full of NULs, and would match nothing.
    if text.contains('\0') {
        bail!("public-guard: the names list is not UTF-8 text; save it as UTF-8");
    }
    let list = parse_list(&text);
    if list.is_empty() {
        return Ok(Err(format!("the names list has no entries ({place})")));
    }
    Ok(Ok(list))
}

/// Whether `lower` (already in lower case) holds `needle` as a whole word. A letter next to
/// the match stops it, except a letter that is an escape's (`\n`, `\t`) in source text.
fn holds_word(lower: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(at) = lower[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        let mut before = lower[..start].chars().rev();
        let letter_before = match before.next() {
            Some(c) if c.is_alphabetic() => before.next() != Some('\\'),
            _ => false,
        };
        let letter_after = lower[end..].chars().next().is_some_and(char::is_alphabetic);
        if !letter_before && !letter_after {
            return true;
        }
        from = start + lower[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// The line numbers of the entries found in `text`.
fn hits(text: &str, list: &[Entry]) -> Vec<usize> {
    let lower = text.to_lowercase();
    list.iter()
        .filter(|e| holds_word(&lower, &e.text))
        .map(|e| e.line)
        .collect()
}

/// `bytes` read as UTF-16 starting at `offset`, unpaired units replaced.
fn utf16_view(bytes: &[u8], offset: usize, little_endian: bool) -> String {
    let (units, _) = bytes[offset.min(bytes.len())..].as_chunks::<2>();
    char::decode_utf16(units.iter().map(|&pair| {
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    }))
    .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
    .collect()
}

/// The line numbers of the entries found in `bytes` as UTF-8 or as UTF-16 in either byte
/// order at either alignment, sorted and without repeats.
pub fn hits_in_bytes(bytes: &[u8], list: &[Entry]) -> Vec<usize> {
    if list.is_empty() {
        return Vec::new();
    }
    let mut found = hits(&String::from_utf8_lossy(bytes), list);
    for offset in [0, 1] {
        for little_endian in [true, false] {
            found.extend(hits(&utf16_view(bytes, offset, little_endian), list));
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

fn extension(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let (_, ext) = name.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

/// Whether `path` is a file a public repo must not hold, at any depth and in any case.
fn is_private_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('/').collect();
    let (name, dirs) = parts.split_last().expect("split gives at least one part");
    *name == "log.md"
        || dirs.contains(&"gates")
        || dirs.windows(2).any(|w| w == ["fixtures", "private"])
}

/// The path rules, which hold at the tip and in history alike.
fn check_path(path: &str, in_history: bool, list: &[Entry], findings: &mut Vec<Finding>) {
    if is_private_path(path) {
        findings.push(Finding::PrivatePath {
            path: safe_label(path, list),
            in_history,
        });
    } else if extension(path).is_some_and(|e| RECORDING_EXTENSIONS.contains(&e.as_str()))
        && !ALLOWED_AUDIO.contains(&path)
    {
        findings.push(Finding::Recording {
            path: safe_label(path, list),
            in_history,
        });
    }
}

/// A temporary file, removed on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        Self(std::env::temp_dir().join(format!(
            "earshot-public-guard-{}-{}.tar",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Scans every file of `HEAD`, read through one `git archive`, so an edited or deleted working
/// copy cannot hide anything. A tracked file the archive left out (an `export-ignore`
/// attribute) fails closed.
fn scan_head(
    root: &Path,
    files: &[String],
    list: &[Entry],
    findings: &mut Vec<Finding>,
) -> Result<()> {
    let tar_file = TempFile::new();
    let out = tar_file.0.to_string_lossy().into_owned();
    git_output(
        root,
        &["archive", "--format=tar", "-o", &out, "HEAD"],
        HISTORY_TIMEOUT,
    )?;
    let opened =
        std::fs::File::open(&tar_file.0).context("public-guard: cannot read the archive")?;
    let mut archive = tar::Archive::new(opened);
    let mut seen = std::collections::HashSet::new();
    for entry in archive
        .entries()
        .context("public-guard: reading the archive")?
    {
        let mut entry = entry.context("public-guard: reading the archive")?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_symlink()) {
            continue;
        }
        let path = entry
            .path()
            .context("public-guard: reading the archive")?
            .to_string_lossy()
            .replace('\\', "/");
        let mut bytes = Vec::new();
        if kind.is_symlink() {
            let target = entry.link_name_bytes().unwrap_or_default();
            bytes.extend_from_slice(&target);
        } else {
            entry
                .read_to_end(&mut bytes)
                .context("public-guard: reading the archive")?;
        }
        scan_file(&path, &bytes, list, findings);
        seen.insert(path);
    }
    if let Some(left_out) = files.iter().find(|f| !seen.contains(*f)) {
        bail!(
            "public-guard: git archive left out {}; failing closed",
            safe_label(left_out, list)
        );
    }
    Ok(())
}

/// Scans one file's path and bytes.
fn scan_file(path: &str, bytes: &[u8], list: &[Entry], findings: &mut Vec<Finding>) {
    let mut found = hits(path, list);
    let ext = extension(path).unwrap_or_default();
    if path == LICENCE_FILE {
        if copyright_lines_hold_an_email(bytes) {
            findings.push(Finding::EmailInLicence);
        }
        found.extend(hits_in_bytes(&without_copyright_lines(bytes), list));
    } else if NOISY_EXTENSIONS.contains(&ext.as_str()) {
        let long: Vec<Entry> = list
            .iter()
            .filter(|e| e.text.chars().count() >= NOISY_MIN_CHARS)
            .cloned()
            .collect();
        found.extend(hits_in_bytes(bytes, &long));
        let (texts, complete) = embedded_text(&ext, bytes);
        for text in texts {
            found.extend(hits_in_bytes(&text, list));
        }
        if !complete {
            findings.push(Finding::TooMuchEmbeddedText {
                path: safe_label(path, list),
            });
        }
    } else {
        found.extend(hits_in_bytes(bytes, list));
    }
    found.sort_unstable();
    found.dedup();
    let label = safe_label(path, list);
    findings.extend(found.into_iter().map(|line| Finding::NameInFile {
        line,
        path: label.clone(),
    }));
}

/// The text a compressed file carries beside its compressed data: a PNG's text chunks and a
/// FLAC's metadata blocks. Anything malformed ends the walk; the bytes as a whole were scanned
/// for long entries anyway. ICO and ICNS have no text of their own to read this way. The flag
/// is false when a file holds more text than the guard reads, which fails closed.
fn embedded_text(ext: &str, bytes: &[u8]) -> (Vec<Vec<u8>>, bool) {
    match ext {
        "png" => png_text(bytes),
        "flac" => (flac_metadata(bytes), true),
        _ => (Vec::new(), true),
    }
}

fn png_text(bytes: &[u8]) -> (Vec<Vec<u8>>, bool) {
    let mut out = Vec::new();
    let mut left = MAX_INFLATED;
    let Some(mut rest) = bytes.strip_prefix(b"\x89PNG\r\n\x1a\n".as_slice()) else {
        return (out, true);
    };
    while let Some((head, body)) = rest.split_at_checked(8) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(data) = body.get(..len) else {
            break;
        };
        // After the keyword's NUL: zTXt has a method byte, iTXt a flag and a method byte.
        let after_key = data.iter().position(|&b| b == 0).map_or(0, |i| i + 1);
        match &head[4..8] {
            b"tEXt" => out.push(data.to_vec()),
            b"zTXt" => {
                out.push(data[..after_key].to_vec());
                let Some(text) = inflate(data.get(after_key + 1..).unwrap_or_default(), &mut left)
                else {
                    return (out, false);
                };
                out.push(text);
            }
            b"iTXt" => {
                let compressed = data.get(after_key) == Some(&1);
                if compressed {
                    let tail = data.get(after_key + 2..).unwrap_or_default();
                    let mut parts = tail.splitn(3, |&b| b == 0);
                    let (lang, translated) = (parts.next(), parts.next());
                    out.push(data[..after_key].to_vec());
                    out.extend(lang.into_iter().chain(translated).map(<[u8]>::to_vec));
                    let Some(text) = inflate(parts.next().unwrap_or_default(), &mut left) else {
                        return (out, false);
                    };
                    out.push(text);
                } else {
                    out.push(data.to_vec());
                }
            }
            _ => {}
        }
        let Some(next) = body.get(len + 4..) else {
            break;
        };
        rest = next;
    }
    (out, true)
}

/// What inflates, even when the stream is damaged part of the way through, taken from the
/// file's allowance `left`; `None` when the text is larger than what is left.
fn inflate(bytes: &[u8], left: &mut u64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let _ = flate2::read::ZlibDecoder::new(bytes)
        .take(left.saturating_add(1))
        .read_to_end(&mut out);
    let used = out.len() as u64;
    if used > *left {
        return None;
    }
    *left -= used;
    Some(out)
}

fn flac_metadata(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let Some(mut rest) = bytes.strip_prefix(b"fLaC".as_slice()) else {
        return out;
    };
    while let Some((head, body)) = rest.split_at_checked(4) {
        let len = u32::from_be_bytes([0, head[1], head[2], head[3]]) as usize;
        let Some(data) = body.get(..len) else {
            break;
        };
        out.push(data.to_vec());
        if head[0] & 0x80 != 0 {
            break;
        }
        rest = &body[len..];
    }
    out
}

/// A licence line that may name a holder: starts `Copyright`, in the file or in a diff.
fn is_copyright_line(line: &[u8]) -> bool {
    line.trim_ascii_start().starts_with(b"Copyright")
}

/// The licence's text without its copyright lines, which name a holder by design. A line with
/// an email address stays in the scan.
fn without_copyright_lines(bytes: &[u8]) -> Vec<u8> {
    bytes
        .split_inclusive(|&b| b == b'\n')
        .filter(|line| !is_copyright_line(line) || line.contains(&b'@'))
        .flatten()
        .copied()
        .collect()
}

fn copyright_lines_hold_an_email(bytes: &[u8]) -> bool {
    bytes
        .split(|&b| b == b'\n')
        .any(|line| is_copyright_line(line) && line.contains(&b'@'))
}

/// One commit's diff text with the root licence's copyright lines taken out.
fn diff_without_licence_copyright(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_licence = false;
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            in_licence = line.trim_end() == format!("diff --git a/{LICENCE_FILE} b/{LICENCE_FILE}");
        }
        let body = line.strip_prefix(['+', '-', ' ']).unwrap_or(line);
        if !(in_licence && is_copyright_line(body.as_bytes()) && !body.contains('@')) {
            out.push_str(line);
        }
    }
    out
}

/// A `git` command in `root` that ignores any repo the environment names (a hook's
/// `GIT_DIR`), and does not change what it prints for this user's settings.
fn git(root: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX"] {
        cmd.env_remove(name);
    }
    cmd.arg("-C")
        .arg(root)
        .args(["-c", "core.quotepath=false", "-c", "log.showroot=true"])
        .args(args);
    cmd
}

/// Runs git and returns its complete stdout; anything else fails closed.
fn git_output(root: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let what = format!("public-guard: git {}", args.first().unwrap_or(&""));
    let out: CmdOutput = run_with_timeout(&mut git(root, args), timeout)
        .with_context(|| format!("{what}: could not run, or passed its deadline"))?;
    out.require_complete(&what)?;
    if !out.success {
        bail!("{what}: failed\n{}", out.describe());
    }
    Ok(out.stdout)
}

/// Splits a NUL-separated path list, leaving out the newlines git leaves between commits.
fn split_paths(text: &str) -> Vec<String> {
    text.split('\0')
        .map(|p| p.trim_matches('\n'))
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

fn tracked_files(root: &Path) -> Result<Vec<String>> {
    let files = split_paths(&git_output(
        root,
        &["ls-tree", "-r", "-z", "--name-only", "HEAD"],
        GIT_TIMEOUT,
    )?);
    if files.is_empty() {
        bail!("public-guard: git ls-tree listed nothing; refusing to call that clean");
    }
    Ok(files)
}

/// Every path any commit touched, each once.
fn history_paths(root: &Path) -> Result<Vec<String>> {
    let text = git_output(
        root,
        &[
            "log",
            "--all",
            "-m",
            "--no-renames",
            "--name-only",
            "--format=",
            "-z",
        ],
        GIT_TIMEOUT,
    )?;
    let mut paths = split_paths(&text);
    paths.sort_unstable();
    paths.dedup();
    if paths.is_empty() {
        bail!("public-guard: git log listed no paths; refusing to call that clean");
    }
    Ok(paths)
}

/// One commit's id and emails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIdentity {
    /// The commit's full id.
    pub commit: String,
    /// The author's email, as recorded.
    pub author: String,
    /// The committer's email, as recorded.
    pub committer: String,
}

/// Parses `%H NUL %ae NUL %ce` lines. `expected` is the commit count from a separate query:
/// a line that does not parse, or a different number of lines, fails closed.
pub fn parse_identities(text: &str, expected: usize) -> Result<Vec<CommitIdentity>> {
    let mut commits = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\0');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(commit), Some(author), Some(committer), None)
                if commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                commits.push(CommitIdentity {
                    commit: commit.into(),
                    author: author.into(),
                    committer: committer.into(),
                });
            }
            _ => bail!("public-guard: git log printed a line it should not have; failing closed"),
        }
    }
    if commits.is_empty() {
        bail!("public-guard: git log listed no commits; refusing to call that clean");
    }
    if commits.len() != expected {
        bail!(
            "public-guard: git log listed {} commits, git rev-list counted {expected}; failing closed",
            commits.len()
        );
    }
    Ok(commits)
}

fn commit_identities(root: &Path) -> Result<Vec<CommitIdentity>> {
    let count = git_output(root, &["rev-list", "--count", "--all"], GIT_TIMEOUT)?;
    let expected: usize = count
        .trim()
        .parse()
        .context("public-guard: git rev-list printed something other than a count")?;
    let text = git_output(
        root,
        &["log", "--all", "--format=%H%x00%ae%x00%ce"],
        GIT_TIMEOUT,
    )?;
    parse_identities(&text, expected)
}

/// Commits whose author or committer email is not a noreply address.
pub fn identity_findings(commits: &[CommitIdentity]) -> Vec<Finding> {
    let personal = |email: &str| !email.to_ascii_lowercase().ends_with(NOREPLY_SUFFIX);
    let mut findings = Vec::new();
    for c in commits {
        for (who, email) in [("author", &c.author), ("committer", &c.committer)] {
            if personal(email) {
                findings.push(Finding::Identity {
                    commit: c.commit.clone(),
                    who,
                });
            }
        }
    }
    findings
}

/// Splits log output into `(commit, text)` at each `\x01<id>` start, and fails closed unless
/// every commit is there.
fn commit_sections<'a>(
    text: &'a str,
    commits: &[CommitIdentity],
) -> Result<Vec<(&'a str, &'a str)>> {
    let sections: Vec<(&str, &str)> = text
        .split('\u{1}')
        .skip(1)
        .map(|s| s.split_at(s.find(['\0', '\n']).unwrap_or(s.len())))
        .collect();
    let mut seen: Vec<&str> = sections.iter().map(|s| s.0).collect();
    seen.sort_unstable();
    seen.dedup();
    let mut wanted: Vec<&str> = commits.iter().map(|c| c.commit.as_str()).collect();
    wanted.sort_unstable();
    if seen != wanted {
        bail!(
            "public-guard: git log showed {} commits' text, not the {} it listed; failing closed",
            seen.len(),
            commits.len()
        );
    }
    Ok(sections)
}

/// Commit messages, author names and committer names.
fn scan_commit_text(
    root: &Path,
    list: &[Entry],
    commits: &[CommitIdentity],
    findings: &mut Vec<Finding>,
) -> Result<()> {
    let text = git_output(
        root,
        &["log", "--all", "--format=%x01%H%x00%an%x00%cn%x00%B"],
        GIT_TIMEOUT,
    )?;
    for (commit, body) in commit_sections(&text, commits)? {
        for line in hits(body, list) {
            findings.push(Finding::NameInCommit {
                line,
                commit: commit.into(),
            });
        }
    }
    Ok(())
}

/// The text of every commit's diff, so a name added and later removed is still found. NUL
/// characters are dropped, which reads UTF-16 text in a diff as plain text. Noisy files are
/// left out (their paths still appear in the file list check).
fn scan_history_diffs(root: &Path, list: &[Entry], findings: &mut Vec<Finding>) -> Result<()> {
    let excludes: Vec<String> = NOISY_EXTENSIONS
        .iter()
        .map(|e| format!(":(exclude,icase,glob)**/*.{e}"))
        .collect();
    let mut args = vec![
        "log",
        "--all",
        "-m",
        "-p",
        "--text",
        "--no-color",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--format=%x01%H",
        "--",
        ".",
    ];
    args.extend(excludes.iter().map(String::as_str));
    let text = git_output(root, &args, HISTORY_TIMEOUT)?;
    // A commit that touched only noisy files prints nothing with a pathspec, so this query
    // cannot be held to every commit; the commit-text scan above is.
    for section in text.split('\u{1}').skip(1) {
        let (commit, body) = section.split_at(section.find('\n').unwrap_or(section.len()));
        let plain = diff_without_licence_copyright(&body.replace('\0', ""));
        for line in hits(&plain, list) {
            findings.push(Finding::NameInDiff {
                line,
                commit: commit.into(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Invented values only: nothing here is or resembles a real person or address.
    const NAME: &str = "Zephyrine Quolloby";
    const EMAIL: &str = "zq.example@mail.invalid";
    const NOREPLY: &str = "1+octo@users.noreply.github.com";

    /// A temporary git repo, removed on drop, isolated from this user's git settings.
    struct Repo {
        dir: PathBuf,
        list: PathBuf,
    }

    impl Repo {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let base = std::env::temp_dir().join(format!(
                "earshot-public-guard-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let dir = base.join("repo");
            std::fs::create_dir_all(&dir).unwrap();
            let list = base.join("names.txt");
            std::fs::write(&list, format!("# a comment\n\n{NAME}\n{EMAIL}\n")).unwrap();
            let repo = Self { dir, list };
            repo.git(&["init", "-q", "-b", "main"]);
            repo
        }

        fn git(&self, args: &[&str]) -> CmdOutput {
            let mut cmd = git(&self.dir, args);
            cmd.env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", self.dir.join("no-such-config"));
            let out = run_with_timeout(&mut cmd, GIT_TIMEOUT).expect("git runs");
            out.require_complete("test git").unwrap();
            assert!(out.success, "git {args:?}\n{}", out.describe());
            out
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let full = self.dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, bytes).unwrap();
        }

        fn commit_as(&self, author: &str, committer: &str, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&[
                "-c",
                "user.name=octo",
                "-c",
                &format!("user.email={committer}"),
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                message,
                &format!("--author=octo <{author}>"),
            ]);
        }

        fn commit(&self, message: &str) {
            self.commit_as(NOREPLY, NOREPLY, message);
        }

        /// A marked repo with one clean commit.
        fn public() -> Self {
            let repo = Self::new();
            repo.write(MARKER, b"");
            repo.write("src/main.rs", b"fn main() {}\n");
            repo.commit("start");
            repo
        }

        fn config(&self, require_list: bool) -> Config {
            Config {
                require_list,
                list_path: Some(self.list.clone()),
            }
        }

        fn findings(&self) -> Vec<Finding> {
            check(&self.dir, &self.config(true)).unwrap().findings
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.dir.parent().unwrap());
        }
    }

    fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(|u| {
                if little_endian {
                    u.to_le_bytes()
                } else {
                    u.to_be_bytes()
                }
            })
            .collect()
    }

    #[test]
    fn public_guard_clean_repo_passes() {
        let repo = Repo::public();
        let report = check(&repo.dir, &repo.config(true)).unwrap();
        assert!(report.enforced);
        assert_eq!(report.findings, []);
        assert_eq!((report.files, report.commits, report.entries), (2, 1, 2));
        run_with(&repo.dir, &repo.config(true)).unwrap();
    }

    #[test]
    fn public_guard_catches_a_planted_email_in_a_file() {
        let repo = Repo::public();
        repo.write(
            "docs/a.md",
            format!("write to {EMAIL} any time\n").as_bytes(),
        );
        repo.commit("add a doc");
        let found = repo.findings();
        assert!(
            found.contains(&Finding::NameInFile {
                line: 4,
                path: "docs/a.md".into()
            }),
            "{found:?}"
        );
    }

    #[test]
    fn public_guard_catches_a_planted_name_in_any_case() {
        let repo = Repo::public();
        repo.write("src/a.rs", NAME.to_uppercase().as_bytes());
        repo.commit("add");
        assert!(repo.findings().contains(&Finding::NameInFile {
            line: 3,
            path: "src/a.rs".into()
        }));
    }

    #[test]
    fn public_guard_catches_a_planted_name_in_utf16_files() {
        for little_endian in [true, false] {
            let repo = Repo::public();
            let mut bytes = if little_endian {
                vec![0xFF, 0xFE]
            } else {
                vec![0xFE, 0xFF]
            };
            bytes.extend(utf16(&format!("said by {NAME}."), little_endian));
            repo.write("notes.txt", &bytes);
            repo.commit("add");
            assert!(
                repo.findings().contains(&Finding::NameInFile {
                    line: 3,
                    path: "notes.txt".into()
                }),
                "little_endian {little_endian}"
            );
        }
    }

    #[test]
    fn public_guard_catches_utf16_text_at_an_odd_offset() {
        let list = parse_list(NAME);
        let mut bytes = vec![0x41];
        bytes.extend(utf16(NAME, true));
        assert_eq!(hits_in_bytes(&bytes, &list), [1]);
    }

    #[test]
    fn public_guard_catches_a_planted_name_in_a_commit_message() {
        let repo = Repo::public();
        repo.commit(&format!("as {NAME} asked"));
        assert!(
            repo.findings()
                .iter()
                .any(|f| matches!(f, Finding::NameInCommit { line: 3, .. })),
        );
    }

    #[test]
    fn public_guard_catches_a_name_as_the_author_name() {
        let repo = Repo::public();
        repo.git(&[
            "-c",
            "user.name=octo",
            "-c",
            &format!("user.email={NOREPLY}"),
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "plain",
            &format!("--author={NAME} <{NOREPLY}>"),
        ]);
        assert!(
            repo.findings()
                .iter()
                .any(|f| matches!(f, Finding::NameInCommit { line: 3, .. })),
        );
    }

    #[test]
    fn public_guard_catches_a_name_only_in_history() {
        let repo = Repo::public();
        repo.write("a.txt", NAME.as_bytes());
        repo.commit("add");
        repo.write("a.txt", b"nothing");
        repo.commit("remove");
        let found = repo.findings();
        assert!(
            found
                .iter()
                .any(|f| matches!(f, Finding::NameInDiff { line: 3, .. })),
            "{found:?}"
        );
        assert!(
            !found
                .iter()
                .any(|f| matches!(f, Finding::NameInFile { .. }))
        );
    }

    #[test]
    fn public_guard_catches_a_utf16_name_only_in_history() {
        let repo = Repo::public();
        repo.write("a.txt", &utf16(NAME, true));
        repo.commit("add");
        repo.write("a.txt", b"nothing");
        repo.commit("remove");
        assert!(
            repo.findings()
                .iter()
                .any(|f| matches!(f, Finding::NameInDiff { line: 3, .. })),
        );
    }

    #[test]
    fn public_guard_catches_a_name_in_a_path() {
        let repo = Repo::public();
        repo.write(&format!("docs/{NAME}.md"), b"x");
        repo.commit("add");
        assert!(repo.findings().iter().any(
            |f| matches!(f, Finding::NameInFile { line: 3, path } if path.starts_with("<path hidden"))
        ));
    }

    #[test]
    fn public_guard_catches_personal_identities() {
        let repo = Repo::public();
        repo.commit_as(EMAIL, NOREPLY, "by a person");
        let who = |found: &[Finding]| {
            found
                .iter()
                .filter_map(|f| match f {
                    Finding::Identity { who, .. } => Some(*who),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(who(&repo.findings()), ["author"]);

        let repo = Repo::public();
        repo.commit_as(NOREPLY, EMAIL, "committed by a person");
        assert_eq!(who(&repo.findings()), ["committer"]);
    }

    #[test]
    fn public_guard_identity_rule_needs_the_whole_noreply_domain() {
        let commit = |email: &str| CommitIdentity {
            commit: "a".repeat(40),
            author: email.into(),
            committer: NOREPLY.into(),
        };
        let bad = |email: &str| !identity_findings(&[commit(email)]).is_empty();
        assert!(!bad(NOREPLY));
        assert!(!bad("1+OCTO@USERS.NOREPLY.GITHUB.COM"));
        assert!(bad("noreply@github.com"));
        assert!(bad("x@users.noreply.github.com.example.invalid"));
        assert!(bad("xusers.noreply.github.com"));
        assert!(bad(""));
    }

    #[test]
    fn public_guard_catches_planted_recordings() {
        for ext in ["flac", "WAV", "mp3", "m4a", "ogg", "opus", "db", "sqlite"] {
            let repo = Repo::public();
            let path = format!("notes/meeting.{ext}");
            repo.write(&path, b"RIFF");
            repo.commit("add");
            assert_eq!(
                repo.findings(),
                [Finding::Recording {
                    path,
                    in_history: false
                }]
            );
        }
    }

    #[test]
    fn public_guard_a_recording_in_an_allowed_folder_is_not_allowed() {
        let repo = Repo::public();
        repo.write("fixtures/smoke/new-clip.flac", b"fLaC");
        repo.commit("add");
        assert!(matches!(
            repo.findings().as_slice(),
            [Finding::Recording { .. }]
        ));
    }

    #[test]
    fn public_guard_allows_the_listed_public_audio() {
        let repo = Repo::public();
        repo.write(ALLOWED_AUDIO[0], b"fLaC");
        repo.commit("add");
        assert_eq!(repo.findings(), []);
    }

    #[test]
    fn public_guard_catches_a_recording_only_in_history() {
        let repo = Repo::public();
        repo.write("a.wav", b"RIFF");
        repo.commit("add");
        repo.git(&["rm", "-q", "a.wav"]);
        repo.commit("remove");
        assert_eq!(
            repo.findings(),
            [Finding::Recording {
                path: "a.wav".into(),
                in_history: true
            }]
        );
    }

    #[test]
    fn public_guard_catches_private_paths() {
        for path in [
            "LOG.md",
            "docs/log.md",
            "gates/GATE-01.md",
            "fixtures/private/notes.txt",
        ] {
            let repo = Repo::public();
            repo.write(path, b"x");
            repo.commit("add");
            assert_eq!(
                repo.findings(),
                [Finding::PrivatePath {
                    path: path.into(),
                    in_history: false
                }],
                "{path}"
            );
        }
    }

    #[test]
    fn public_guard_without_the_marker_checks_nothing_and_passes() {
        let repo = Repo::new();
        repo.write("LOG.md", NAME.as_bytes());
        repo.write("a.wav", b"RIFF");
        repo.commit_as(EMAIL, EMAIL, NAME);
        let report = check(&repo.dir, &repo.config(true)).unwrap();
        assert!(!report.enforced);
        assert_eq!(report.findings, []);
        run_with(&repo.dir, &repo.config(true)).unwrap();
    }

    #[test]
    fn public_guard_an_untracked_marker_does_not_turn_it_on() {
        let repo = Repo::new();
        repo.write("LOG.md", b"x");
        repo.commit("start");
        repo.write(MARKER, b"");
        assert!(!check(&repo.dir, &repo.config(true)).unwrap().enforced);
    }

    #[test]
    fn public_guard_missing_list_warns_and_the_other_checks_still_run() {
        let repo = Repo::public();
        repo.write(&format!("{NAME}.txt"), NAME.as_bytes());
        repo.write("LOG.md", b"x");
        repo.commit("add");
        std::fs::remove_file(&repo.list).unwrap();
        let report = check(&repo.dir, &repo.config(false)).unwrap();
        assert!(report.list_problem.is_some());
        assert_eq!(
            report.findings,
            [Finding::PrivatePath {
                path: "LOG.md".into(),
                in_history: false
            }]
        );
    }

    #[test]
    fn public_guard_a_clean_repo_passes_without_a_list() {
        let repo = Repo::public();
        std::fs::remove_file(&repo.list).unwrap();
        run_with(&repo.dir, &repo.config(false)).unwrap();
    }

    #[test]
    fn public_guard_require_list_fails_on_a_missing_or_empty_list() {
        let repo = Repo::public();
        std::fs::write(&repo.list, "# only a comment\n\n").unwrap();
        assert!(check(&repo.dir, &repo.config(true)).is_err());
        std::fs::remove_file(&repo.list).unwrap();
        assert!(check(&repo.dir, &repo.config(true)).is_err());
        let no_home = Config {
            require_list: true,
            list_path: None,
        };
        assert!(check(&repo.dir, &no_home).is_err());
    }

    #[test]
    fn public_guard_a_list_that_is_not_utf8_fails_even_without_require_list() {
        let repo = Repo::public();
        std::fs::write(&repo.list, utf16(NAME, true)).unwrap();
        assert!(check(&repo.dir, &repo.config(false)).is_err());
    }

    /// Every finding kind, with a list entry in the path where the kind has one.
    #[test]
    fn public_guard_output_never_quotes_the_list() {
        let repo = Repo::public();
        repo.write("a.txt", NAME.as_bytes());
        repo.write(&format!("docs/{NAME}/note.txt"), b"x");
        repo.write(&format!("notes/{NAME}.m4a"), b"RIFF");
        repo.write(&format!("gates/{NAME}.md"), b"x");
        repo.write(&format!("{NAME}.wav"), b"RIFF");
        repo.write(
            LICENCE_FILE,
            format!("Copyright (c) 2026 {EMAIL}\n").as_bytes(),
        );
        repo.commit(&format!("from {EMAIL}"));
        repo.write(&format!("old/{NAME}.txt"), b"x");
        repo.write(&format!("old/{NAME}.flac"), b"fLaC");
        repo.commit("add");
        repo.git(&["rm", "-q", "-r", "old", "--", MARKER]);
        repo.commit_as(EMAIL, EMAIL, "plain");
        // With the marker gone and another first commit nothing is enforced, so it comes back.
        let report_of = |repo: &Repo| check(&repo.dir, &repo.config(true)).unwrap();
        assert!(!report_of(&repo).enforced);
        repo.write(MARKER, b"");
        repo.commit("back");
        let found = repo.findings();
        let has = |f: &dyn Fn(&Finding) -> bool| found.iter().any(f);
        assert!(has(&|f| matches!(f, Finding::NameInFile { .. })));
        assert!(has(&|f| matches!(f, Finding::NameInCommit { .. })));
        assert!(has(&|f| matches!(f, Finding::NameInDiff { .. })));
        assert!(has(&|f| matches!(f, Finding::Identity { .. })));
        assert!(has(&|f| matches!(
            f,
            Finding::Recording {
                in_history: false,
                ..
            }
        )));
        assert!(has(&|f| matches!(
            f,
            Finding::Recording {
                in_history: true,
                ..
            }
        )));
        assert!(has(&|f| matches!(f, Finding::PrivatePath { .. })));
        assert!(has(&|f| matches!(f, Finding::EmailInLicence)));
        let mut texts: Vec<String> = found.iter().map(ToString::to_string).collect();
        texts.push(Finding::MarkerGone.to_string());
        let hidden = safe_label(&format!("{NAME}.png"), &parse_list(NAME));
        texts.push(Finding::TooMuchEmbeddedText { path: hidden }.to_string());
        for text in texts {
            let lower = text.to_lowercase();
            assert!(
                !lower.contains(&NAME.to_lowercase()) && !lower.contains(EMAIL),
                "{text}"
            );
        }
    }

    #[test]
    fn public_guard_a_safe_path_is_printed_and_a_named_one_is_hashed() {
        let list = parse_list(NAME);
        assert_eq!(safe_label("docs/a.md", &list), "docs/a.md");
        let hidden = safe_label(&format!("x/{NAME}.m4a"), &list);
        assert!(hidden.starts_with("<path hidden, sha256 "), "{hidden}");
        assert!(hidden.ends_with(", .m4a>"), "{hidden}");
        let list = parse_list("secretext");
        let hidden = safe_label("x/a.secretext", &list);
        assert!(!hidden.contains("secretext"), "{hidden}");
    }

    #[test]
    fn public_guard_the_tip_scan_reads_head_not_the_working_tree() {
        let repo = Repo::public();
        repo.write("a.txt", NAME.as_bytes());
        repo.write("b.txt", NAME.as_bytes());
        repo.commit("add");
        repo.write("a.txt", b"edited clean");
        std::fs::remove_file(repo.dir.join("b.txt")).unwrap();
        let found = repo.findings();
        for path in ["a.txt", "b.txt"] {
            assert!(
                found.contains(&Finding::NameInFile {
                    line: 3,
                    path: path.into()
                }),
                "{path}: {found:?}"
            );
        }
    }

    #[test]
    fn public_guard_a_file_the_archive_leaves_out_fails_closed() {
        let repo = Repo::public();
        repo.write(".gitattributes", b"hidden.txt export-ignore\n");
        repo.write("hidden.txt", NAME.as_bytes());
        repo.commit("add");
        let err = check(&repo.dir, &repo.config(true)).unwrap_err();
        assert!(err.to_string().contains("left out"), "{err}");
    }

    #[test]
    fn public_guard_private_paths_match_at_any_depth_and_case() {
        for path in [
            "LOG.md",
            "a/b/Log.MD",
            "gates/x.md",
            "a/GATES/x.md",
            "fixtures/private/x",
            "a/Fixtures/Private/b/x",
        ] {
            assert!(is_private_path(path), "{path}");
        }
        for path in [
            "gates.rs",
            "a/gates",
            "fixtures/smoke/x",
            "private/x",
            "a/catalog.md",
        ] {
            assert!(!is_private_path(path), "{path}");
        }
    }

    #[test]
    fn public_guard_catches_the_new_recording_types() {
        for ext in [
            "sqlite3", "db-wal", "db-shm", "aac", "wma", "webm", "mp4", "pcm", "raw", "rttm",
        ] {
            let repo = Repo::public();
            let path = format!("a.{ext}");
            repo.write(&path, b"x");
            repo.commit("add");
            assert_eq!(
                repo.findings(),
                [Finding::Recording {
                    path,
                    in_history: false
                }]
            );
        }
    }

    #[test]
    fn public_guard_activates_on_the_public_start_commit_without_the_marker() {
        let repo = Repo::new();
        repo.write(MARKER, b"");
        repo.write("a.txt", b"x");
        repo.commit(PUBLIC_START);
        assert_eq!(repo.findings(), []);
        repo.git(&["rm", "-q", "--", MARKER]);
        repo.write("LOG.md", b"x");
        repo.commit("hide it");
        let report = check(&repo.dir, &repo.config(true)).unwrap();
        assert!(report.enforced);
        assert!(report.findings.contains(&Finding::MarkerGone));
        assert!(
            report
                .findings
                .iter()
                .any(|f| matches!(f, Finding::PrivatePath { .. }))
        );
    }

    /// The private archive: any other first commit, and it passes whatever its name or remote.
    #[test]
    fn public_guard_another_first_commit_does_not_activate_it() {
        let repo = Repo::new();
        repo.write("LOG.md", b"x");
        repo.commit("start the project");
        repo.git(&[
            "remote",
            "add",
            "origin",
            "https://github.invalid/pitslug/Earshot.git",
        ]);
        assert!(!check(&repo.dir, &repo.config(true)).unwrap().enforced);
    }

    #[test]
    fn public_guard_a_licence_copyright_line_never_holds_an_email() {
        let repo = Repo::public();
        repo.write(
            LICENCE_FILE,
            b"MIT\nCopyright (c) 2026 Someone <someone@mail.invalid>\n",
        );
        repo.commit("add");
        assert!(repo.findings().contains(&Finding::EmailInLicence));
    }

    #[test]
    fn public_guard_history_of_the_licence_is_scanned_under_the_same_rule() {
        let repo = Repo::public();
        repo.write(LICENCE_FILE, format!("MIT\nCopyright {NAME}\n").as_bytes());
        repo.commit("copyright only");
        assert_eq!(repo.findings(), []);
        repo.write(
            LICENCE_FILE,
            format!("MIT\nAsk {NAME}\nCopyright x\n").as_bytes(),
        );
        repo.commit("name outside the line");
        repo.write(LICENCE_FILE, b"MIT\nCopyright x\n");
        repo.commit("gone again");
        let found = repo.findings();
        assert!(
            found
                .iter()
                .any(|f| matches!(f, Finding::NameInDiff { line: 3, .. })),
            "{found:?}"
        );
        assert!(
            !found
                .iter()
                .any(|f| matches!(f, Finding::NameInFile { .. }))
        );
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0; 4]);
        out
    }

    #[test]
    fn public_guard_short_entries_are_found_in_png_and_flac_text() {
        use std::io::Write;
        let mut zipped = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        zipped.write_all(b"by zeb").unwrap();
        let zipped = zipped.finish().unwrap();

        let start = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ztxt = b"Author\0\0".to_vec();
        ztxt.extend_from_slice(&zipped);
        let mut itxt = b"Author\0\x01\0en\0\0".to_vec();
        itxt.extend_from_slice(&zipped);
        let mut flac = b"fLaC\x84\0\0\x06by zeb".to_vec();
        flac.extend_from_slice(&[0xFF; 8]);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "a.png",
                [start.clone(), png_chunk(b"tEXt", b"Author\0by zeb")].concat(),
            ),
            ("b.png", [start.clone(), png_chunk(b"zTXt", &ztxt)].concat()),
            (
                "c.png",
                [
                    start.clone(),
                    png_chunk(b"iTXt", b"Author\0\0\0en\0\0by zeb"),
                ]
                .concat(),
            ),
            ("d.png", [start.clone(), png_chunk(b"iTXt", &itxt)].concat()),
            ("e.flac", flac),
        ];
        for (path, bytes) in cases {
            let repo = Repo::public();
            std::fs::write(&repo.list, "Zeb\n").unwrap();
            repo.write(path, &bytes);
            repo.commit("add");
            assert!(
                repo.findings()
                    .iter()
                    .any(|f| matches!(f, Finding::NameInFile { .. })),
                "{path}"
            );
        }
    }

    /// Two chunks that fit the cap apart and exceed it together.
    #[test]
    fn public_guard_compressed_png_text_past_the_cap_fails_closed() {
        use std::io::Write;
        let mut zipped = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        let half = usize::try_from(MAX_INFLATED / 2 + 1).unwrap();
        zipped.write_all(&vec![b' '; half]).unwrap();
        let zipped = zipped.finish().unwrap();
        let mut ztxt = b"Author\0\0".to_vec();
        ztxt.extend_from_slice(&zipped);
        let start = b"\x89PNG\r\n\x1a\n".to_vec();
        let once = [start.clone(), png_chunk(b"zTXt", &ztxt)].concat();
        assert!(png_text(&once).1);
        let twice = [once, png_chunk(b"zTXt", &ztxt)].concat();
        assert!(!png_text(&twice).1);

        let repo = Repo::public();
        repo.write("a.png", &twice);
        repo.commit("add");
        assert_eq!(
            repo.findings(),
            [Finding::TooMuchEmbeddedText {
                path: "a.png".into()
            }]
        );
    }

    #[test]
    fn public_guard_a_damaged_png_or_flac_ends_the_walk_quietly() {
        assert!(
            png_text(b"\x89PNG\r\n\x1a\n\xFF\xFF\xFF\xFFtEXt")
                .0
                .is_empty()
        );
        assert!(png_text(b"not a png").0.is_empty());
        assert!(flac_metadata(b"fLaC\0\xFF\xFF\xFF").is_empty());
    }

    #[test]
    fn public_guard_not_a_repo_fails_closed() {
        let repo = Repo::new();
        let plain = repo.dir.parent().unwrap().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // The folder sits under the system temp folder, outside any repo.
        assert!(check(&plain, &repo.config(false)).is_err());
    }

    #[test]
    fn public_guard_a_repo_with_nothing_tracked_fails_closed() {
        let repo = Repo::new();
        assert!(check(&repo.dir, &repo.config(false)).is_err());
    }

    #[test]
    fn public_guard_empty_or_cut_off_git_output_fails_closed() {
        let good = format!("{}\0{NOREPLY}\0{NOREPLY}\n", "a".repeat(40));
        assert_eq!(parse_identities(&good, 1).unwrap().len(), 1);
        assert!(parse_identities("", 0).is_err());
        assert!(parse_identities("", 1).is_err());
        assert!(parse_identities(&good, 2).is_err());
        assert!(parse_identities(&format!("{}\0{NOREPLY}\n", "a".repeat(40)), 1).is_err());
        assert!(parse_identities(&format!("abc\0{NOREPLY}\0{NOREPLY}\n"), 1).is_err());
        let commits = parse_identities(&good, 1).unwrap();
        assert!(commit_sections("", &commits).is_err());
        assert!(commit_sections("\u{1}", &commits).is_err());
        assert_eq!(
            commit_sections(&format!("\u{1}{}\0body", "a".repeat(40)), &commits)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn public_guard_matches_whole_words_only() {
        let word = |text: &str| holds_word(&text.to_lowercase(), "quill");
        for yes in [
            "quill",
            "Quill said",
            r"C:\Users\Quill\x",
            "quill2",
            "quill_x",
            "a.quill@x",
            r"line\nQuill said",
        ] {
            assert!(word(yes), "{yes}");
        }
        for no in ["quills", "bequill", "an unrelated line"] {
            assert!(!word(no), "{no}");
        }
        // A rejected match earlier in the text does not hide a real one after it.
        assert!(word("quills then quill"));
        // Multi-byte text next to a rejected match keeps the scan on character boundaries.
        assert!(word("équills é quill"));
    }

    #[test]
    fn public_guard_short_entries_skip_noisy_files_but_long_ones_do_not() {
        let repo = Repo::public();
        std::fs::write(&repo.list, "Zeb\nQuolloby\n").unwrap();
        repo.write("icon.png", b"\x89PNG zeb quolloby");
        repo.commit("add");
        assert_eq!(
            repo.findings(),
            [Finding::NameInFile {
                line: 2,
                path: "icon.png".into()
            }]
        );
    }

    #[test]
    fn public_guard_list_path_rule() {
        let os = |s: &str| Some(OsString::from(s));
        let home = PathBuf::from("h")
            .join(".earshot-private")
            .join("names.txt");
        assert_eq!(
            list_path_from(os("x.txt"), os("h"), None),
            Some(PathBuf::from("x.txt"))
        );
        assert_eq!(list_path_from(os(""), os("h"), None), Some(home.clone()));
        assert_eq!(list_path_from(None, None, os("h")), Some(home));
        assert_eq!(list_path_from(None, None, None), None);
    }

    #[test]
    fn public_guard_list_parsing_skips_comments_and_a_bom() {
        let list = parse_list("\u{feff}Alpha\r\n# no\n\n  Beta  \n");
        let got: Vec<(usize, &str)> = list.iter().map(|e| (e.line, e.text.as_str())).collect();
        assert_eq!(got, [(1, "alpha"), (4, "beta")]);
    }

    #[test]
    fn public_guard_only_the_root_licence_copyright_line_may_name_a_person() {
        let repo = Repo::public();
        repo.write(
            LICENCE_FILE,
            format!("MIT\n\n  Copyright (c) 2026 {NAME}\n").as_bytes(),
        );
        repo.commit("add");
        assert_eq!(repo.findings(), []);

        repo.write(LICENCE_FILE, format!("MIT, ask {NAME}\n").as_bytes());
        repo.commit("change");
        assert!(repo.findings().contains(&Finding::NameInFile {
            line: 3,
            path: LICENCE_FILE.into()
        }));

        let repo = Repo::public();
        repo.write("docs/LICENSE", format!("Copyright {NAME}\n").as_bytes());
        repo.commit("add");
        assert!(!repo.findings().is_empty());
    }

    /// The allowlist is exactly the audio this repo tracks: a new recording fails here too.
    #[test]
    fn public_guard_allowlist_is_the_audio_this_repo_tracks() {
        let tracked = tracked_files(&crate::repo_root()).unwrap();
        let mut audio: Vec<&str> = tracked
            .iter()
            .filter(|p| extension(p).is_some_and(|e| RECORDING_EXTENSIONS.contains(&e.as_str())))
            .map(String::as_str)
            .collect();
        audio.sort_unstable();
        let mut allowed = ALLOWED_AUDIO.to_vec();
        allowed.sort_unstable();
        assert_eq!(audio, allowed);
    }

    /// The hook must keep demanding the list and must stay LF, or sh cannot run it.
    #[test]
    fn public_guard_pre_push_hook_requires_the_list() {
        let hook =
            std::fs::read_to_string(crate::repo_root().join(".githooks").join("pre-push")).unwrap();
        assert!(hook.starts_with("#!/bin/sh\n"));
        assert!(hook.contains("cargo xtask public-guard --require-list"));
        assert!(
            hook.contains("read -r "),
            "the hook reads what is being pushed"
        );
        assert!(!hook.contains('\r'));
    }
}
