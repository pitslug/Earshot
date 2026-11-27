//! Source scans for T14.67 (docs/sync-spec.md 18.6 test 5): who may ask for this computer's
//! UTC offset at a meeting's start, who may read a stored offset for output, and who calls the
//! zone seams. The recording funnel opens real devices, so it is pinned by source.
//!
//! Host sources are every `.rs` file under the runtime's, the desktop app's, the CLI's and the
//! worker's `src`; shipped sources add core's and the store's (which define, store and render
//! the offset, so only the callers of core's `export::render` are pinned there). Test files
//! (`*tests.rs`, `testutil.rs`, this file, anything under a `tests` folder) and an inline
//! `#[cfg(test)] mod tests` block are left out, and comment lines are skipped. Names are
//! matched as whole identifiers, so a `use ... as` alias, a glob import's bare call, `Self::`,
//! a field access, a struct literal and a destructuring pattern all count.

use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

const HOST_DIRS: [&str; 4] = [
    "crates/earshot-runtime/src",
    "apps/desktop/src-tauri/src",
    "crates/earshot-cli/src",
    "crates/earshot-worker/src",
];

/// Core and the store, scanned only for the callers of core's `export::render`.
const MODEL_DIRS: [&str; 2] = ["crates/earshot-core/src", "crates/earshot-store/src"];

/// `(path relative to the repo, code text)` of every host source file.
fn shipped_sources() -> Vec<(String, String)> {
    sources_under(&HOST_DIRS)
}

/// [`shipped_sources`] plus core's and the store's.
fn shipped_with_model() -> Vec<(String, String)> {
    let mut all = sources_under(&HOST_DIRS);
    all.extend(sources_under(&MODEL_DIRS));
    all
}

/// `(path relative to the repo, code text)` of every non-test source file under `dirs`.
fn sources_under(dirs: &[&str]) -> Vec<(String, String)> {
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
    for dir in dirs {
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

/// `text` up to an inline `#[cfg(test)] mod tests {` block, without comment lines, then with
/// string literals and trailing comments blanked ([`blank_strings`]).
fn code_of(text: &str) -> String {
    let mut kept = String::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim();
        if t == "#[cfg(test)]" && lines.peek().is_some_and(|n| n.trim() == "mod tests {") {
            break;
        }
        if !t.starts_with("//") {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    blank_strings(&kept)
}

/// `text` with the contents of every string literal (plain, byte and raw) and every trailing
/// `//` comment replaced by spaces, byte for byte, so a name inside a message or a comment is
/// not code. A `'"'` or `'\"'` char literal is not a string.
fn blank_strings(text: &str) -> String {
    fn blank(out: &mut String, c: char) {
        if c == '\n' {
            out.push('\n');
        } else {
            out.extend(std::iter::repeat_n(' ', c.len_utf8()));
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let at = |i: usize| chars.get(i).copied();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' && at(i + 1) == Some('"') && at(i + 2) == Some('\'') {
            out.push_str("' '");
            i += 3;
        } else if c == '\''
            && at(i + 1) == Some('\\')
            && at(i + 2) == Some('"')
            && at(i + 3) == Some('\'')
        {
            out.push_str("'  '");
            i += 4;
        } else if c == '/' && at(i + 1) == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                blank(&mut out, chars[i]);
                i += 1;
            }
        } else if c == 'r' && !(i > 0 && ident_char(chars[i - 1]) && chars[i - 1] != 'b') && {
            let hashes = chars[i + 1..].iter().take_while(|&&h| h == '#').count();
            at(i + 1 + hashes) == Some('"')
        } {
            let hashes = chars[i + 1..].iter().take_while(|&&h| h == '#').count();
            out.push('r');
            out.extend(std::iter::repeat_n('#', hashes));
            out.push('"');
            i += 2 + hashes;
            while i < chars.len() {
                let closes = chars[i] == '"' && (1..=hashes).all(|k| at(i + k) == Some('#'));
                if closes {
                    break;
                }
                blank(&mut out, chars[i]);
                i += 1;
            }
            if i < chars.len() {
                out.push('"');
                out.extend(std::iter::repeat_n('#', hashes));
                i += 1 + hashes;
            }
        } else if c == '"' {
            out.push('"');
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    blank(&mut out, chars[i]);
                    i += 1;
                }
                blank(&mut out, chars[i]);
                i += 1;
            }
            if i < chars.len() {
                out.push('"');
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
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

/// Every file of `sources` naming `name` (as an identifier), with its count, sorted by path.
fn naming(sources: &[(String, String)], name: &str) -> Vec<(String, usize)> {
    let mut found: Vec<(String, usize)> = sources
        .iter()
        .map(|(p, code)| (p.clone(), idents(code, name).len()))
        .filter(|(_, n)| *n > 0)
        .collect();
    found.sort();
    found
}

/// Like [`naming`] for core's `render`: [`render_uses`] instead of every identifier.
fn naming_render(sources: &[(String, String)]) -> Vec<(String, usize)> {
    let mut found: Vec<(String, usize)> = sources
        .iter()
        .map(|(p, code)| (p.clone(), render_uses(code).len()))
        .filter(|(_, n)| *n > 0)
        .collect();
    found.sort();
    found
}

/// Offsets of the identifier `render` in `code` less a local variable of that name: a binding
/// (`let render`, `let mut render`) or a method receiver (`render.`, or `render` then `.` on
/// the next line). Every way to reach core's function (a `use`, an alias, a path, a bare call
/// after a glob import) still counts.
fn render_uses(code: &str) -> Vec<usize> {
    idents(code, "render")
        .into_iter()
        .filter(|at| {
            let before = code[..*at].trim_end();
            let after = code[at + "render".len()..].trim_start();
            !(before.ends_with("let") || before.ends_with("let mut") || after.starts_with('.'))
        })
        .collect()
}

/// `want` as owned, sorted pairs, to compare with [`naming`].
fn table(want: &[(&str, usize)]) -> Vec<(String, usize)> {
    let mut t: Vec<(String, usize)> = want.iter().map(|(p, n)| ((*p).to_owned(), *n)).collect();
    t.sort();
    t
}

/// Asserts every occurrence of `name` in `file` lies inside the item starting with `header`.
fn all_inside(sources: &[(String, String)], file: &str, name: &str, header: &str) {
    let code = source(sources, file);
    let span = item(&code, header);
    for at in idents(&code, name) {
        assert!(span.contains(&at), "{name} in {file} is outside `{header}`");
    }
}

#[test]
fn utc_offset_scan_finds_aliases_self_and_fields_and_skips_comments_and_tests() {
    // The scanner's own rules, on invented text (the control for the scans below): each form
    // a reader could take is counted, and comments, tests and longer names are not.
    let code = code_of(concat!(
        "use crate::live::UtcOffset::ThisComputer as Here;\n",
        "use earshot_core::export::render as draw;\n",
        "use earshot_core::export::*;\n",
        "// ThisComputer and render in a comment\n",
        "fn f() { Self::ThisComputer; ThisComputerX; renderer(); render(input); }\n",
        "fn g(m: &Meeting) { x . utc_offset_minutes = 1; let Meeting { utc_offset_minutes, .. } = m; }\n",
        "fn h() { Meeting { utc_offset_minutes: 0 }; earshot_proc::utc_offset_minutes_at(0); }\n",
        "fn k() { let mut render = open(); render.rate(); render\n .stop(); say(\"default render\"); }\n",
        "#[cfg(test)]\nmod tests {\n ThisComputer render utc_offset_minutes\n}\n",
    ));
    assert_eq!(idents(&code, "ThisComputer").len(), 2);
    // The alias's `use` and the glob import's bare call; not `renderer`, not a local
    // variable named `render`, not a word in a string.
    assert_eq!(render_uses(&code).len(), 2);
    assert_eq!(idents(&code, "render").len(), 5);
    // A field access, a destructuring pattern and a struct literal; not the `_at` function.
    assert_eq!(idents(&code, "utc_offset_minutes").len(), 3);
    assert_eq!(idents(&code, "utc_offset_minutes_at").len(), 1);
    let sources = vec![("f.rs".to_owned(), code.clone())];
    assert_eq!(naming_render(&sources), table(&[("f.rs", 2)]));
    let span = item(&code, "fn g(");
    assert!(span.contains(&idents(&code, "utc_offset_minutes")[1]));
}

#[test]
fn utc_offset_this_computer_is_asked_for_only_by_the_recording_funnel() {
    let sources = shipped_with_model();
    let funnel = "crates/earshot-runtime/src/backends.rs";
    let defined = "crates/earshot-runtime/src/live/mod.rs";
    // The variant and `resolve`'s arm in live/mod.rs; the funnel's one use in backends.rs.
    assert_eq!(
        naming(&sources, "ThisComputer"),
        table(&[(defined, 2), (funnel, 1)])
    );
    let live = source(&sources, defined);
    let (enum_span, impl_span) = (
        item(&live, "pub enum UtcOffset {"),
        item(&live, "impl UtcOffset {"),
    );
    let uses = idents(&live, "ThisComputer");
    assert!(enum_span.contains(&uses[0]), "the variant");
    assert!(impl_span.contains(&uses[1]), "resolve's arm");
    all_inside(
        &sources,
        funnel,
        "ThisComputer",
        "pub fn start_wasapi_session_on(",
    );
    let code = source(&sources, funnel);
    let span = item(&code, "pub fn start_wasapi_session_on(");
    assert!(code[span].contains("clock_domain: earshot_core::model::ClockDomain::Qpc,"));
}

#[test]
fn utc_offset_stored_offset_is_read_for_output_only_through_the_rule() {
    let sources = shipped_sources();
    // Every use of the field in host code, accesses, literals and patterns alike: the export's
    // copy (set from `shown_offset_of`) and `view::shown_offset_of`, the one reader.
    let export = "crates/earshot-runtime/src/export.rs";
    let view = "crates/earshot-runtime/src/view.rs";
    assert_eq!(
        naming(&sources, "utc_offset_minutes"),
        table(&[(export, 1), (view, 1)])
    );
    all_inside(&sources, export, "utc_offset_minutes", "fn rendered_with(");
    all_inside(
        &sources,
        view,
        "utc_offset_minutes",
        "pub fn shown_offset_of(",
    );
}

#[test]
fn utc_offset_core_render_is_called_only_by_the_runtime_export() {
    // Core's `export::render` turns the stored offset into the note's name and dates, so its
    // only shipped caller is the runtime's `rendered_with`, which hands it the resolved copy.
    // In core, `export/mod.rs` declares and re-exports it (`mod render;`,
    // `pub use render::{.. render`) and `render.rs` defines it; `sim/mod.rs` has a private
    // `render` of its own (three uses), a different function. The store names it only in its
    // tests.
    let sources = shipped_with_model();
    let export = "crates/earshot-runtime/src/export.rs";
    assert_eq!(
        naming_render(&sources),
        table(&[
            ("crates/earshot-core/src/export/mod.rs", 3),
            ("crates/earshot-core/src/export/render.rs", 1),
            ("crates/earshot-core/src/sim/mod.rs", 3),
            // The `use` and the one call.
            (export, 2),
        ])
    );
    let code = source(&sources, export);
    let span = item(&code, "fn rendered_with(");
    let calls: Vec<usize> = render_uses(&code)
        .into_iter()
        .filter(|at| code[at + "render".len()..].starts_with('('))
        .collect();
    assert_eq!(calls.len(), 1);
    assert!(
        span.contains(&calls[0]),
        "render is called outside rendered_with"
    );
}

#[test]
fn utc_offset_this_computers_zone_is_read_only_by_the_three_readers() {
    // `earshot_proc::utc_offset_minutes_at` is passed by the two production wrappers and read
    // by `live::zone_reader`, the start's one reader.
    let sources = shipped_with_model();
    let (export, summary, live) = (
        "crates/earshot-runtime/src/export.rs",
        "crates/earshot-runtime/src/summary.rs",
        "crates/earshot-runtime/src/live/mod.rs",
    );
    let name = "utc_offset_minutes_at";
    assert_eq!(
        naming(&sources, name),
        table(&[(export, 1), (summary, 1), (live, 1)])
    );
    all_inside(&sources, export, name, "fn rendered(");
    all_inside(&sources, summary, name, "fn input_of(");
    all_inside(&sources, live, name, "fn zone_reader(");
}

#[test]
fn utc_offset_zone_seams_are_called_only_by_their_production_wrappers() {
    let sources = shipped_with_model();
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
        // The definition and one call.
        assert_eq!(naming(&sources, seam), table(&[(file, 2)]), "{seam}");
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
