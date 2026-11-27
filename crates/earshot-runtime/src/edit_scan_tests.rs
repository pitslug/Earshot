//! The sample invariant by construction (T12.77b5, docs/voice-samples.md 3.3): a user's edit
//! changes a speaker row's person only through `edit::set_row_person` and tombstones a row
//! only through `edit::retire_row`, both of which take the voice's samples back. Every name
//! contains `edit`.

use std::path::{Path, PathBuf};

use crate::local::tests::{
    production_files, production_lines, test_support_packages, workspace_members,
};

/// Functions allowed to change a speaker row's person or tombstone one, each with its reason.
/// A path ending in `/` stands for every function in that folder; `*` for every function in the
/// place. An entry that matches nothing fails the test, so the list never outlives its code
/// (sync apply and the constructors of docs/voice-samples.md 3.3 write speaker rows whole, by
/// `put_synced` and field shorthand, which no pattern matches).
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "crates/earshot-runtime/src/edit.rs",
        "set_row_person",
        "the one primitive: writes the row and takes back",
    ),
    (
        "crates/earshot-runtime/src/edit.rs",
        "retire_row",
        "the one tombstone primitive: takes back everything",
    ),
    (
        "crates/earshot-runtime/src/post/",
        "*",
        "the final pass: a re-run keeps samples counting (decision 6)",
    ),
    (
        "crates/earshot-store/src/delete.rs",
        "unlink_speakers_tx",
        "Remove's unlink (T14.57): the identity's samples are forgotten in the same transaction",
    ),
    (
        "crates/earshot-core/src/model/repo.rs",
        "delete_meeting",
        "a deleted meeting's rows; its samples stay by decision (section 8)",
    ),
    (
        "crates/earshot-store/src/repo.rs",
        "speaker_row",
        "the store's row reader",
    ),
    (
        "crates/earshot-core/src/model/entities.rs",
        "new",
        "`Speaker::new` builds a new row with its person (field shorthand)",
    ),
    (
        "crates/earshot-core/src/model/repo_contract.rs",
        "*",
        "the repository contract, run only by tests, writes rows directly",
    ),
];

/// `src` with every comment, string, raw string, byte string and character literal blanked to
/// spaces (newlines kept, so offsets and line numbers stay), so SQL text or prose never counts.
fn blank(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let wipe = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    while i < b.len() {
        let rest = &b[i..];
        if rest.starts_with(b"//") {
            let end = rest
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |e| i + e);
            wipe(&mut out, i, end);
            i = end;
        } else if rest.starts_with(b"/*") {
            let mut depth = 0;
            let mut j = i;
            while j < b.len() {
                if b[j..].starts_with(b"/*") {
                    depth += 1;
                    j += 2;
                } else if b[j..].starts_with(b"*/") {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            wipe(&mut out, i, j);
            i = j;
        } else if let Some(hashes) = raw_string_start(rest) {
            // r"..", r#".."#, br#".."#: ends at a quote followed by the same hashes.
            let open = rest.iter().position(|&c| c == b'"').unwrap() + 1;
            let close = format!("\"{}", "#".repeat(hashes));
            let end = b[i + open..]
                .windows(close.len())
                .position(|w| w == close.as_bytes())
                .map_or(b.len(), |e| i + open + e + close.len());
            wipe(&mut out, i, end);
            i = end;
        } else if rest[0] == b'"' || rest.starts_with(b"b\"") {
            let mut j = i + if rest[0] == b'"' { 1 } else { 2 };
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            wipe(&mut out, i, (j + 1).min(b.len()));
            i = j + 1;
        } else if rest[0] == b'\'' {
            // A character literal ('x', '\n', '\u{..}'), not a lifetime ('a).
            let close = if rest.get(1) == Some(&b'\\') {
                rest[2..].iter().position(|&c| c == b'\'').map(|p| p + 3)
            } else {
                let ch_len = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                (rest.get(1 + ch_len) == Some(&b'\'')).then_some(2 + ch_len)
            };
            match close {
                Some(n) => {
                    wipe(&mut out, i, i + n);
                    i += n;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// The number of `#` of a raw string starting at `rest` (`r"`, `r#"`, `br#"`), if it is one.
fn raw_string_start(rest: &[u8]) -> Option<usize> {
    let body = rest
        .strip_prefix(b"br")
        .or_else(|| rest.strip_prefix(b"r"))?;
    let hashes = body.iter().take_while(|&&c| c == b'#').count();
    (body.get(hashes) == Some(&b'"')).then_some(hashes)
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Every function of blanked source `src`: its name and the byte range of its body.
fn functions(src: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    for (at, _) in src.match_indices("fn ") {
        if at > 0 && is_ident(b[at - 1]) {
            continue;
        }
        let name: String = src[at + 3..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let mut j = at + 3 + name.len();
        while j < b.len() && b[j] != b'{' && b[j] != b';' {
            j += 1;
        }
        if j >= b.len() || b[j] == b';' {
            continue;
        }
        let open = j;
        let mut depth = 0;
        while j < b.len() {
            match b[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        out.push((name, open..j));
    }
    out
}

/// The innermost function containing byte `at` ("" outside every function).
fn enclosing(fns: &[(String, std::ops::Range<usize>)], at: usize) -> &str {
    fns.iter()
        .filter(|(_, r)| r.contains(&at))
        .min_by_key(|(_, r)| r.len())
        .map_or("", |(n, _)| n.as_str())
}

/// Byte offsets in blanked `src` where a speaker row's person changes or a speaker row is
/// tombstoned (docs/voice-samples.md 3.3), with what matched.
fn hits(src: &str) -> Vec<(usize, &'static str)> {
    let b = src.as_bytes();
    let fns = functions(src);
    let mut out = Vec::new();
    // `person_id = ` (an assignment, not `==` or `=>`).
    for (at, _) in src.match_indices("person_id") {
        if at > 0 && is_ident(b[at - 1]) {
            continue;
        }
        let mut j = at + "person_id".len();
        if j < b.len() && is_ident(b[j]) {
            continue;
        }
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) == Some(&b'=') && !matches!(b.get(j + 1), Some(b'=' | b'>')) {
            out.push((at, "person_id assignment"));
        }
    }
    // `person_id.take()`: the person moved out of the row.
    for (at, _) in src.match_indices("person_id.take(") {
        if at == 0 || !is_ident(b[at - 1]) {
            out.push((at, "person_id taken"));
        }
    }
    // `Speaker { .. person_id: .. }`, with or without a struct update.
    for (at, _) in src.match_indices("Speaker") {
        if at > 0 && is_ident(b[at - 1]) {
            continue;
        }
        let mut j = at + "Speaker".len();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) != Some(&b'{') {
            continue;
        }
        // A type position, not a literal: `-> Speaker {`, `struct Speaker {`, `impl Speaker {`.
        let before = src[..at].trim_end();
        let word = before
            .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()
            .unwrap_or("");
        if before.ends_with("->") || matches!(word, "struct" | "impl" | "for" | "enum") {
            continue;
        }
        let open = j;
        let mut depth = 0;
        while j < b.len() {
            match b[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let inside = &src[open..j.min(src.len())];
        // `person_id: x` or the shorthand `person_id,` / `person_id }`.
        let has = inside.match_indices("person_id").any(|(k, _)| {
            let after = inside[k + "person_id".len()..].trim_start();
            (k == 0 || !is_ident(inside.as_bytes()[k - 1]))
                && (after.starts_with(':') || after.starts_with(',') || after.starts_with('}'))
        });
        if has {
            out.push((at, "Speaker literal with person_id"));
        }
    }
    // A tombstone written in a function that writes speaker rows.
    for pat in ["mark_deleted(", "meta.deleted"] {
        for (at, _) in src.match_indices(pat) {
            if pat == "meta.deleted" {
                let after = src[at + pat.len()..].trim_start();
                let Some(rhs) = after.strip_prefix('=') else {
                    continue;
                };
                if rhs.starts_with('=') || !rhs.trim_start().starts_with("true") {
                    continue;
                }
            }
            let f = fns
                .iter()
                .filter(|(_, r)| r.contains(&at))
                .min_by_key(|(_, r)| r.len());
            if let Some((_, r)) = f
                && src[r.clone()].contains("put_speaker(")
            {
                out.push((at, "tombstone beside put_speaker"));
            }
        }
    }
    out
}

/// Whether exception `entry` covers `function` in `path`.
fn covers(entry: &(&str, &str, &str), path: &str, function: &str) -> bool {
    let (p, f, _) = entry;
    let place = if p.ends_with('/') {
        path.starts_with(p)
    } else {
        path == *p
    };
    place && (*f == "*" || *f == function)
}

#[test]
fn edit_speaker_person_changes_only_through_set_row_person() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let skip = test_support_packages(&root);
    let mut files: Vec<PathBuf> = Vec::new();
    for krate in workspace_members(&root)
        .iter()
        .filter(|m| m.as_str() != "xtask" && !skip.contains(m))
    {
        let dir = root.join("crates").join(krate);
        if dir.join("src").exists() {
            files.extend(production_files(&dir));
        }
    }
    files.extend(production_files(&root.join("apps/desktop/src-tauri")));
    let rel = |f: &Path| {
        f.strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    for reached in [
        "crates/earshot-runtime/src/edit.rs",
        "crates/earshot-store/src/sync.rs",
        "crates/earshot-core/src/model/repo.rs",
        "apps/desktop/src-tauri/src/detail.rs",
    ] {
        assert!(
            files.iter().any(|f| rel(f) == reached),
            "the scan reaches {reached}"
        );
    }
    let mut found = Vec::new();
    let mut used = vec![false; ALLOWED.len()];
    for f in &files {
        let path = rel(f);
        let src = std::fs::read_to_string(f).unwrap();
        // Test-only items left out (`#[cfg(test)]`), lines kept in place.
        let mut kept = vec![String::new(); src.lines().count()];
        for (n, line) in production_lines(&src) {
            kept[n - 1] = line;
        }
        let code = blank(&kept.join("\n"));
        let fns = functions(&code);
        for (at, what) in hits(&code) {
            let function = enclosing(&fns, at);
            let mut covered = false;
            for (k, entry) in ALLOWED.iter().enumerate() {
                if covers(entry, &path, function) {
                    used[k] = true;
                    covered = true;
                }
            }
            if covered {
                continue;
            }
            let line = code[..at].matches('\n').count() + 1;
            found.push(format!("{path}:{line} in fn {function}: {what}"));
        }
    }
    assert!(
        found.is_empty(),
        "a speaker row's person changes only through edit::set_row_person (and a row is \
         tombstoned only through edit::retire_row), which take the voice's samples back \
         (docs/voice-samples.md 3.3); review and list a new exception with its reason:\n{}",
        found.join("\n")
    );
    let stale: Vec<&str> = ALLOWED
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|((p, f, _), _)| if *f == "*" { *p } else { *f })
        .collect();
    assert!(stale.is_empty(), "exceptions that match nothing: {stale:?}");
}

#[test]
fn edit_scan_blanks_strings_and_comments_and_finds_each_pattern() {
    let src = r##"
fn writes(row: &mut Speaker) {
    row.person_id = Some(id);
}
fn sql_only() {
    let q = "UPDATE speaker SET person_id = ?1, deleted = 1";
    let r = r#"meta.deleted = true; put_speaker("#;
    // row.person_id = None; put_speaker(x); mark_deleted(
    /* Speaker { person_id: x } */
    let c = '{';
}
fn compares(row: &Speaker) -> bool {
    row.person_id == Some(id) || matches!(row.person_id, Some(_))
}
fn builds() -> Speaker {
    Speaker { person_id: None, ..base }
}
fn shorthand(person_id: Option<EntityId>) -> Speaker {
    Speaker { id, person_id, ..base }
}
fn takes(row: &mut Speaker) -> Option<EntityId> {
    row.person_id.take()
}
fn tombstones(tx: &mut Tx, mut s: Speaker) {
    s.meta.deleted = true;
    tx.put_speaker(s);
}
fn tombstones_elsewhere(mut n: Note) {
    n.meta.mark_deleted(h, c);
}
"##;
    let code = blank(src);
    assert!(!code.contains("UPDATE") && !code.contains("put_speaker(\""));
    let fns = functions(&code);
    let got: Vec<(&str, &str)> = hits(&code)
        .into_iter()
        .map(|(at, what)| (enclosing(&fns, at), what))
        .collect();
    assert_eq!(
        got,
        [
            ("writes", "person_id assignment"),
            ("takes", "person_id taken"),
            ("builds", "Speaker literal with person_id"),
            ("shorthand", "Speaker literal with person_id"),
            ("tombstones", "tombstone beside put_speaker"),
        ]
    );
}

/// Every shipped source of the workspace (xtask and test-support crates left out), with its
/// path relative to the root and its blanked production code (`#[cfg(test)]` items left out).
fn shipped_code() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let skip = test_support_packages(&root);
    let mut files: Vec<PathBuf> = Vec::new();
    for krate in workspace_members(&root)
        .iter()
        .filter(|m| m.as_str() != "xtask" && !skip.contains(m))
    {
        let dir = root.join("crates").join(krate);
        if dir.join("src").exists() {
            files.extend(production_files(&dir));
        }
    }
    files.extend(production_files(&root.join("apps/desktop/src-tauri")));
    files
        .iter()
        .map(|f| {
            let path = f
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let src = std::fs::read_to_string(f).unwrap();
            let mut kept = vec![String::new(); src.lines().count()];
            for (n, line) in production_lines(&src) {
                kept[n - 1] = line;
            }
            (path, blank(&kept.join("\n")))
        })
        .collect()
}

/// Byte offsets of `word` in blanked `code` as a whole identifier.
fn word_at(code: &str, word: &str) -> Vec<usize> {
    let b = code.as_bytes();
    code.match_indices(word)
        .map(|(at, _)| at)
        .filter(|&at| {
            (at == 0 || !is_ident(b[at - 1]))
                && b.get(at + word.len()).is_none_or(|&c| !is_ident(c))
        })
        .collect()
}

/// docs/sync-spec.md 7A.2 N2: the final pass builds its identity entries in memory and writes
/// its record whole, so it never calls P5's helper, which writes the stored record (a write
/// the pass would overwrite). Control: the scan finds the helper where it is called.
#[test]
fn edit_scan_post_final_pass_never_calls_the_p5_helper() {
    const HELPER: [&str; 3] = [
        "row_relinked_tx",
        "row_relinked_guarded",
        "drop_row_entry_guarded",
    ];
    let code = shipped_code();
    let mut post_files = 0;
    let mut found = Vec::new();
    for (path, src) in &code {
        if !path.starts_with("crates/earshot-runtime/src/post/") {
            continue;
        }
        post_files += 1;
        for name in HELPER {
            for at in word_at(src, name) {
                let line = src[..at].matches('\n').count() + 1;
                found.push(format!("{path}:{line}: {name}"));
            }
        }
    }
    assert!(post_files > 0, "the scan reaches the final pass");
    assert!(
        found.is_empty(),
        "the final pass applies `relinked` to its in-memory entries and never calls the \
         helper (docs/sync-spec.md 7A.2 N2):\n{}",
        found.join("\n")
    );
    let edit = code
        .iter()
        .find(|(p, _)| p == "crates/earshot-runtime/src/edit.rs")
        .map(|(_, s)| s.as_str())
        .expect("edit.rs is scanned");
    assert!(
        !word_at(edit, "row_relinked_tx").is_empty(),
        "control: the scan finds the helper in a user edit"
    );
}

/// docs/sync-spec.md 7A.3: the writers that take an existing person, each with the check that
/// refuses a removed one (a deleted, merged-away or missing person, or a chain that loops).
const WRITERS: &[(&str, &str, &str)] = &[
    (
        "crates/earshot-runtime/src/edit.rs",
        "link_in",
        "live_person(",
    ),
    (
        "crates/earshot-runtime/src/edit.rs",
        "set_row_person",
        "live_person(",
    ),
    (
        "crates/earshot-runtime/src/edit.rs",
        "save_voice_with",
        "resolve_person(",
    ),
    (
        "crates/earshot-runtime/src/edit.rs",
        "merge_into_me",
        "Person::is_live",
    ),
    (
        "crates/earshot-runtime/src/edit.rs",
        "rename_person",
        "Person::is_live",
    ),
    (
        "crates/earshot-core/src/voiceprint/mod.rs",
        "merge_persons",
        "Person::is_live",
    ),
    (
        "crates/earshot-core/src/voiceprint/sample.rs",
        "admissible",
        "is_live()",
    ),
];

/// The functions that change a speaker row's person through `set_row_person` (which resolves
/// the person itself, docs/sync-spec.md 7A.3): a new caller is reviewed and listed here.
const SET_ROW_PERSON_CALLERS: &[&str] = &["link_in", "mark_me", "unmark_me", "not_this_person"];

/// The set of writers that refuse a removed person (docs/sync-spec.md 7A.3), pinned: each
/// listed writer exists and holds its check, and the callers of `set_row_person` are exactly the
/// listed ones.
#[test]
fn edit_scan_writers_refuse_a_removed_person() {
    let code = shipped_code();
    let of = |path: &str| {
        code.iter()
            .find(|(p, _)| p == path)
            .map(|(_, s)| s.as_str())
            .unwrap_or_else(|| panic!("the scan reaches {path}"))
    };
    for (path, function, check) in WRITERS {
        let src = of(path);
        let fns = functions(src);
        let (_, body) = fns
            .iter()
            .find(|(n, _)| n == function)
            .unwrap_or_else(|| panic!("{path}: writer {function} not found"));
        assert!(
            src[body.clone()].contains(check),
            "{path}: {function} must refuse a removed person ({check})"
        );
    }
    let mut callers: Vec<String> = Vec::new();
    for (path, src) in &code {
        let fns = functions(src);
        for at in word_at(src, "set_row_person") {
            let rest = src[at + "set_row_person".len()..].trim_start();
            if src[..at].ends_with("fn ") || !rest.starts_with('(') {
                continue;
            }
            callers.push(format!("{path}::{}", enclosing(&fns, at)));
        }
    }
    callers.sort();
    callers.dedup();
    let mut want: Vec<String> = SET_ROW_PERSON_CALLERS
        .iter()
        .map(|f| format!("crates/earshot-runtime/src/edit.rs::{f}"))
        .collect();
    want.sort();
    assert_eq!(
        callers, want,
        "set_row_person's callers (docs/sync-spec.md 7A.3)"
    );
}

/// Functions allowed to tombstone a person row, each with why it never tombstones the user
/// (docs/sync-spec.md 7A.2 P4).
const PERSON_TOMBSTONES: &[(&str, &str, &str)] = &[
    (
        "crates/earshot-store/src/delete.rs",
        "remove_person_in",
        "Remove: `edit::remove_person` refuses the user and anyone the user was merged into",
    ),
    (
        "crates/earshot-store/src/sync.rs",
        "merged_into_removed_repair",
        "P2: skips a person whose `is_me` is true or does not read",
    ),
    (
        "crates/earshot-store/src/guarded.rs",
        "tombstone_person_by_rowid",
        "the named by-rowid writer (16.4), called only by the two above",
    ),
];

/// Functions allowed to build a person row with an `is_me` value: a new row and the store's
/// reader. Nothing changes `is_me` on an existing row (docs/sync-spec.md 7A.2 P4).
const IS_ME_WRITERS: &[(&str, &str)] = &[
    ("crates/earshot-core/src/voiceprint/mod.rs", "new"),
    ("crates/earshot-store/src/repo.rs", "person_row"),
];

/// Offsets in blanked `src` of a person tombstone: `x.meta.mark_deleted(..)` or
/// `x.meta.deleted = true` on a value the same function then passes to `put_person(x`.
fn person_tombstones(src: &str, fns: &[(String, std::ops::Range<usize>)]) -> Vec<usize> {
    let mut out = Vec::new();
    for pat in [".meta.mark_deleted(", ".meta.deleted"] {
        for (at, _) in src.match_indices(pat) {
            if pat == ".meta.deleted" {
                let after = src[at + pat.len()..].trim_start();
                let Some(rhs) = after.strip_prefix('=') else {
                    continue;
                };
                if rhs.starts_with('=') || !rhs.trim_start().starts_with("true") {
                    continue;
                }
            }
            // The value tombstoned: the identifier before `.meta`.
            let start = src[..at]
                .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
                .map_or(0, |i| i + 1);
            let value = &src[start..at];
            if value.is_empty() {
                continue;
            }
            let f = fns
                .iter()
                .filter(|(_, r)| r.contains(&at))
                .min_by_key(|(_, r)| r.len());
            if let Some((_, r)) = f
                && src[r.clone()].match_indices("put_person(").any(|(k, p)| {
                    let arg = src[r.start + k + p.len()..].trim_start();
                    arg.strip_prefix(value).is_some_and(|rest| {
                        !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                    })
                })
            {
                out.push(at);
            }
        }
    }
    for at in word_at(src, "tombstone_person_by_rowid") {
        if !src[..at].ends_with("fn ") {
            out.push(at);
        }
    }
    out
}

/// Offsets in blanked `src` where a field `is_me` is assigned (`x.is_me = ..`), or set in a
/// `Person { .. }` literal, with what matched.
fn is_me_writes(src: &str) -> Vec<(usize, &'static str)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    for at in word_at(src, "is_me") {
        if at == 0 || b[at - 1] != b'.' {
            continue;
        }
        let mut j = at + "is_me".len();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) == Some(&b'=') && !matches!(b.get(j + 1), Some(b'=' | b'>')) {
            out.push((at, "is_me assigned"));
        }
    }
    for at in word_at(src, "Person") {
        let mut j = at + "Person".len();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) != Some(&b'{') {
            continue;
        }
        let before = src[..at].trim_end();
        let word = before
            .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()
            .unwrap_or("");
        if before.ends_with("->") || matches!(word, "struct" | "impl" | "for" | "enum") {
            continue;
        }
        let open = j;
        let mut depth = 0;
        while j < b.len() {
            match b[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let inside = &src[open..j.min(src.len())];
        let has = inside.match_indices("is_me").any(|(k, _)| {
            let after = inside[k + "is_me".len()..].trim_start();
            (k == 0 || !is_ident(inside.as_bytes()[k - 1]))
                && (after.starts_with(':') || after.starts_with(',') || after.starts_with('}'))
        });
        if has {
            out.push((at, "a person literal sets is_me"));
        }
    }
    out
}

/// P4's pin (docs/sync-spec.md 7A.2, 7A.11): no shipped code tombstones the user's person, and
/// none changes `is_me` on an existing row. Every person tombstone writer is listed with its
/// reason; `is_me` is never assigned, and set in a person literal only by a new row and the
/// store's reader. An entry that matches nothing fails the test.
#[test]
fn edit_scan_no_shipped_code_tombstones_the_user_or_changes_is_me() {
    let code = shipped_code();
    let mut found = Vec::new();
    let mut used_tombstones = vec![false; PERSON_TOMBSTONES.len()];
    let mut used_is_me = vec![false; IS_ME_WRITERS.len()];
    for (path, src) in &code {
        let fns = functions(src);
        for at in person_tombstones(src, &fns) {
            let function = enclosing(&fns, at);
            match PERSON_TOMBSTONES
                .iter()
                .position(|(p, f, _)| p == path && *f == function)
            {
                Some(k) => used_tombstones[k] = true,
                None => {
                    let line = src[..at].matches('\n').count() + 1;
                    found.push(format!(
                        "{path}:{line} in fn {function}: a person tombstone"
                    ));
                }
            }
        }
        for (at, what) in is_me_writes(src) {
            let function = enclosing(&fns, at);
            match IS_ME_WRITERS
                .iter()
                .position(|(p, f)| p == path && *f == function)
            {
                Some(k) if what != "is_me assigned" => used_is_me[k] = true,
                _ => {
                    let line = src[..at].matches('\n').count() + 1;
                    found.push(format!("{path}:{line} in fn {function}: {what}"));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "no shipped code tombstones the user's person or changes is_me on an existing row \
         (docs/sync-spec.md 7A.2 P4); review and list a new writer with its reason:\n{}",
        found.join("\n")
    );
    let stale: Vec<&str> = PERSON_TOMBSTONES
        .iter()
        .zip(&used_tombstones)
        .filter(|(_, u)| !**u)
        .map(|((_, f, _), _)| *f)
        .chain(
            IS_ME_WRITERS
                .iter()
                .zip(&used_is_me)
                .filter(|(_, u)| !**u)
                .map(|((_, f), _)| *f),
        )
        .collect();
    assert!(stale.is_empty(), "entries that match nothing: {stale:?}");
}

/// The P4 scan's patterns on invented code: an assignment, a literal and a tombstone beside
/// `put_person` are found; a comparison, a view struct and prose are not.
#[test]
fn edit_scan_p4_patterns_find_each_write() {
    let src = r##"
fn assigns(p: &mut Person) {
    p.is_me = true;
}
fn compares(p: &Person) -> bool {
    p.is_me == true
}
fn literal() -> Person {
    Person { id, meta, display_name, is_me: false, merged_into: None }
}
fn view(p: &Person) -> PersonView {
    PersonView { is_me: p.is_me }
}
fn tombstones(tx: &mut Tx, mut p: Person) {
    p.meta.mark_deleted(h, c);
    tx.put_person(p);
}
fn prose() {
    // p.is_me = true; Person { is_me: true }; mark_deleted( put_person(
}
fn closure() {
    let is_me = |k: usize| k == 0;
}
fn other_tombstone(tx: &mut Tx, mut s: Summary, p: Person) {
    s.meta.deleted = true;
    tx.put_summary(s);
    tx.put_person(p);
}
"##;
    let code = blank(src);
    let fns = functions(&code);
    let got: Vec<(&str, &str)> = is_me_writes(&code)
        .into_iter()
        .map(|(at, what)| (enclosing(&fns, at), what))
        .chain(
            person_tombstones(&code, &fns)
                .into_iter()
                .map(|at| (enclosing(&fns, at), "tombstone")),
        )
        .collect();
    assert_eq!(
        got,
        [
            ("assigns", "is_me assigned"),
            ("literal", "a person literal sets is_me"),
            ("tombstones", "tombstone"),
        ]
    );
}
