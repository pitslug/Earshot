//! Source scans for T14.67 (docs/sync-spec.md 18.6 test 5): who may ask for this computer's
//! UTC offset at a meeting's start, who may read a stored offset for output, and who calls the
//! zone seams. The recording funnel opens real devices, so it is pinned by source.
//!
//! Shipped sources are every `.rs` file under the runtime's, the desktop app's, the CLI's and
//! the worker's `src`, less test files (`*tests.rs`, `testutil.rs`, this file, anything under a
//! `tests` folder) and less an inline `#[cfg(test)] mod tests` block. Comment lines are skipped.
//! Names are matched as whole identifiers, so a `use ... as` alias or `Self::` still counts.

use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

const SHIPPED_DIRS: [&str; 4] = [
    "crates/earshot-runtime/src",
    "apps/desktop/src-tauri/src",
    "crates/earshot-cli/src",
    "crates/earshot-worker/src",
];

/// `(path relative to the repo, code text)` of every shipped source file.
fn shipped_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "tests") {
                    walk(&path, out);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = repo();
    let mut files = Vec::new();
    for dir in SHIPPED_DIRS {
        let before = files.len();
        walk(&root.join(dir), &mut files);
        assert!(files.len() > before, "no sources under {dir}");
    }
    files
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            if name.ends_with("tests.rs") || name == "testutil.rs" || name == "utc_offset_scan.rs" {
                return None;
            }
            let text = std::fs::read_to_string(&path).ok()?;
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            Some((rel, code_of(&text)))
        })
        .collect()
}

/// `text` up to an inline `#[cfg(test)] mod tests {` block, without comment lines.
fn code_of(text: &str) -> String {
    let mut out = String::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim();
        if t == "#[cfg(test)]" && lines.peek().is_some_and(|n| n.trim() == "mod tests {") {
            break;
        }
        if !t.starts_with("//") {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offsets of `name` in `code` as a whole identifier.
fn idents(code: &str, name: &str) -> Vec<usize> {
    code.match_indices(name)
        .filter(|(at, _)| {
            let before = code[..*at].chars().next_back();
            let after = code[at + name.len()..].chars().next();
            !before.is_some_and(ident_char) && !after.is_some_and(ident_char)
        })
        .map(|(at, _)| at)
        .collect()
}

/// Byte offsets of a field access `.name` in `code`, white space after the dot allowed.
fn field_accesses(code: &str, name: &str) -> Vec<usize> {
    idents(code, name)
        .into_iter()
        .filter(|at| code[..*at].trim_end().ends_with('.'))
        .collect()
}

/// The byte range of the item that starts with `header` in `code`, to its closing line.
fn item(code: &str, header: &str) -> std::ops::Range<usize> {
    let start = code.find(header).unwrap_or_else(|| panic!("no `{header}`"));
    let end = code[start..]
        .find("\n}\n")
        .map_or(code.len(), |i| start + i + 3);
    start..end
}

fn source(sources: &[(String, String)], rel: &str) -> String {
    sources
        .iter()
        .find(|(p, _)| p == rel)
        .unwrap_or_else(|| panic!("{rel} is not scanned"))
        .1
        .clone()
}

/// Every shipped file naming `name` (as an identifier), with its count.
fn naming(sources: &[(String, String)], name: &str, field: bool) -> Vec<(String, usize)> {
    sources
        .iter()
        .map(|(p, code)| {
            let n = if field {
                field_accesses(code, name).len()
            } else {
                idents(code, name).len()
            };
            (p.clone(), n)
        })
        .filter(|(_, n)| *n > 0)
        .collect()
}

#[test]
fn utc_offset_scan_finds_aliases_self_and_fields_and_skips_comments_and_tests() {
    // The scanner's own rules, on invented text (the control for the scans below).
    let code = code_of(
        "use crate::live::UtcOffset::ThisComputer as Here;\n\
         // ThisComputer in a comment\n\
         fn f() { Self::ThisComputer; ThisComputerX; x . utc_offset_minutes = 1; y.utc_offset_minutes_at(); }\n\
         #[cfg(test)]\nmod tests {\n ThisComputer\n}\n",
    );
    assert_eq!(idents(&code, "ThisComputer").len(), 2);
    assert_eq!(field_accesses(&code, "utc_offset_minutes").len(), 1);
}

#[test]
fn utc_offset_this_computer_is_asked_for_only_by_the_recording_funnel() {
    let sources = shipped_sources();
    let found = naming(&sources, "ThisComputer", false);
    let funnel = "crates/earshot-runtime/src/backends.rs";
    let defined = "crates/earshot-runtime/src/live/mod.rs";
    for (path, n) in &found {
        assert!(
            path == defined || (path == funnel && *n == 1),
            "{path} names ThisComputer {n} time(s): {found:?}"
        );
    }
    assert!(found.iter().any(|(p, _)| p == defined), "{found:?}");
    // Exactly once in backends.rs, inside the funnel, beside the QPC clock domain.
    let code = source(&sources, funnel);
    let at = idents(&code, "ThisComputer");
    assert_eq!(at.len(), 1, "{found:?}");
    let span = item(&code, "pub fn start_wasapi_session_on(");
    assert!(span.contains(&at[0]), "ThisComputer is outside the funnel");
    assert!(code[span].contains("clock_domain: earshot_core::model::ClockDomain::Qpc,"));
}

#[test]
fn utc_offset_stored_offset_is_read_for_output_only_through_the_rule() {
    let sources = shipped_sources();
    let found = naming(&sources, "utc_offset_minutes", true);
    let mut want = vec![
        // The export's copy of the meeting, set from `shown_offset_of`.
        ("crates/earshot-runtime/src/export.rs".to_owned(), 1),
        // `view::shown_offset_of`, the one reader.
        ("crates/earshot-runtime/src/view.rs".to_owned(), 1),
    ];
    want.sort();
    let mut found = found;
    found.sort();
    assert_eq!(found, want);
    let view = source(&sources, "crates/earshot-runtime/src/view.rs");
    let span = item(&view, "pub fn shown_offset_of(");
    assert!(span.contains(&field_accesses(&view, "utc_offset_minutes")[0]));
}

#[test]
fn utc_offset_zone_seams_are_called_only_by_their_production_wrappers() {
    let sources = shipped_sources();
    for (seam, file, wrapper) in [
        (
            "rendered_with",
            "crates/earshot-runtime/src/export.rs",
            "fn rendered(",
        ),
        (
            "input_of_with",
            "crates/earshot-runtime/src/summary.rs",
            "fn input_of(",
        ),
    ] {
        let found = naming(&sources, seam, false);
        // The definition and one call.
        assert_eq!(found, [(file.to_owned(), 2)], "{seam}");
        let code = source(&sources, file);
        let span = item(&code, wrapper);
        let calls: Vec<usize> = idents(&code, seam)
            .into_iter()
            .filter(|at| !code[..*at].ends_with("fn "))
            .collect();
        assert_eq!(calls.len(), 1, "{seam}");
        assert!(
            span.contains(&calls[0]),
            "{seam} is called outside {wrapper}"
        );
    }
}
