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
