//! T14.57 (docs/sync-spec.md 16.8): the scan of what the compiler cannot prove about the guarded
//! paths. Each root takes a guarded type and holds no connection, SQL or store handle; each
//! public wrapper is one `services.guarded_*` call; every SQL constant of `guarded.rs` is used
//! only by its owner, read through `Col` conversions there, and the by-rowid writers are named.
//! It reads the source files by path from the workspace root, strips comments only (string
//! literals are kept: the SQL it judges lives in them), fails closed on anything it cannot find,
//! and runs its mutation controls on copies of the real files.

use std::collections::BTreeMap;
use std::path::PathBuf;

const STORE: &str = "crates/earshot-store/src/";
const RUNTIME: &str = "crates/earshot-runtime/src/";

/// (file, root function).
const ROOTS: &[(&str, &str)] = &[
    ("crates/earshot-store/src/guarded.rs", "apply_page"),
    ("crates/earshot-store/src/guarded.rs", "chain_end"),
    ("crates/earshot-store/src/guarded.rs", "identity_tx"),
    ("crates/earshot-store/src/guarded.rs", "identity_in"),
    // T14.49a (docs/sync-spec.md 7A.2): P1, P4 and F1 in the page loop, P2, P3.
    ("crates/earshot-store/src/guarded.rs", "tombstone_final"),
    ("crates/earshot-store/src/sync.rs", "converge"),
    ("crates/earshot-store/src/sync.rs", "dead_person_repair"),
    (
        "crates/earshot-store/src/sync.rs",
        "merged_into_removed_repair",
    ),
    (
        "crates/earshot-store/src/sync.rs",
        "unlink_removed_speakers",
    ),
    ("crates/earshot-store/src/sync.rs", "edits_win"),
    ("crates/earshot-store/src/sync.rs", "outbox_sent_in"),
    (
        "crates/earshot-store/src/identities.rs",
        "forget_identity_biometrics",
    ),
    ("crates/earshot-store/src/identities.rs", "forget_identity"),
    // T14.49b (docs/sync-spec.md 7A.2 P5): the helper's guarded entry point and its drop.
    (
        "crates/earshot-store/src/identities.rs",
        "row_relinked_guarded",
    ),
    (
        "crates/earshot-store/src/identities.rs",
        "drop_row_entry_guarded",
    ),
    ("crates/earshot-store/src/delete.rs", "remove_person_in"),
    ("crates/earshot-store/src/delete.rs", "unlink_speakers_tx"),
    ("crates/earshot-store/src/db.rs", "forget_voiceprints_in"),
    (
        "crates/earshot-runtime/src/summary_names.rs",
        "names_before",
    ),
    (
        "crates/earshot-runtime/src/summary_names.rs",
        "names_changed_tx",
    ),
    ("crates/earshot-runtime/src/summary_names.rs", "record_tx"),
    (
        "crates/earshot-runtime/src/sync/engine.rs",
        "meetings_deleted_elsewhere_in",
    ),
    ("crates/earshot-runtime/src/sync/engine.rs", "apply_page_in"),
    ("crates/earshot-runtime/src/sync/engine.rs", "after_page"),
    (
        "crates/earshot-runtime/src/word_choices.rs",
        "drop_stale_in",
    ),
    ("crates/earshot-runtime/src/vocabulary.rs", "reconcile_in"),
    ("crates/earshot-runtime/src/vocabulary.rs", "reconcile_tx"),
    ("crates/earshot-runtime/src/vocabulary.rs", "rebuild_tx"),
    ("crates/earshot-runtime/src/vocabulary.rs", "remove_rows_tx"),
    ("crates/earshot-runtime/src/vocabulary.rs", "add_entry_in"),
    (
        "crates/earshot-runtime/src/vocabulary.rs",
        "remove_entry_in",
    ),
    (
        "crates/earshot-runtime/src/vocabulary.rs",
        "set_vocabulary_in",
    ),
    ("crates/earshot-runtime/src/edit.rs", "remove_person_in"),
    (
        "crates/earshot-runtime/src/edit.rs",
        "forget_person_voices_in",
    ),
    ("crates/earshot-runtime/src/edit.rs", "list_persons_in"),
    ("crates/earshot-runtime/src/edit.rs", "named_after_me_in"),
    (
        "crates/earshot-runtime/src/my_voice.rs",
        "forget_my_voice_in",
    ),
    ("crates/earshot-runtime/src/ignore_copy.rs", "copy_once_in"),
    (
        "crates/earshot-runtime/src/summary_seen.rs",
        "mark_existing_once_in",
    ),
    ("crates/earshot-runtime/src/sync/mod.rs", "observe_vault"),
    ("crates/earshot-runtime/src/sync/mod.rs", "advance_cursor"),
    (
        "crates/earshot-runtime/src/sync/mod.rs",
        "set_sync_status_in",
    ),
];

/// (file, public wrapper, its root).
const WRAPPERS: &[(&str, &str, &str)] = &[
    (
        "crates/earshot-runtime/src/edit.rs",
        "remove_person",
        "remove_person_in",
    ),
    (
        "crates/earshot-runtime/src/edit.rs",
        "forget_person_voices",
        "forget_person_voices_in",
    ),
    (
        "crates/earshot-runtime/src/edit.rs",
        "list_persons",
        "list_persons_in",
    ),
    (
        "crates/earshot-runtime/src/edit.rs",
        "named_after_me",
        "named_after_me_in",
    ),
    (
        "crates/earshot-runtime/src/my_voice.rs",
        "forget_my_voice",
        "forget_my_voice_in",
    ),
    (
        "crates/earshot-runtime/src/ignore_copy.rs",
        "copy_once",
        "copy_once_in",
    ),
    (
        "crates/earshot-runtime/src/summary_seen.rs",
        "mark_existing_once",
        "mark_existing_once_in",
    ),
    (
        "crates/earshot-runtime/src/vocabulary.rs",
        "reconcile",
        "reconcile_in",
    ),
    (
        "crates/earshot-runtime/src/vocabulary.rs",
        "add_entry",
        "add_entry_in",
    ),
    (
        "crates/earshot-runtime/src/vocabulary.rs",
        "remove_entry",
        "remove_entry_in",
    ),
    (
        "crates/earshot-runtime/src/vocabulary.rs",
        "set_vocabulary",
        "set_vocabulary_in",
    ),
];

/// Engine glue whose store access must be guarded (`apply`'s `self.services.delete_meeting` is
/// the named exception, T14.26's deletion path).
const GLUE: &[&str] = &[
    "tick_into",
    "try_tick",
    "push",
    "send",
    "pull",
    "retry_held",
    "retry_parents",
    "release_holds",
    "holds_rows_for_retry",
    "apply_with",
    "meetings_deleted_elsewhere",
];

/// The runtime functions that hold a `StoreTx` and may call `StoreTx::guarded` (the
/// coordinator, 2026-11-27): (file, function, reason). Nothing else in runtime code may.
const GUARDED_FROM_STORE_TX: &[(&str, &str, &str)] = &[
    (
        "crates/earshot-runtime/src/summary_names.rs",
        "name_changed_tx",
        "a user's naming edit holds its own transaction; only the record write is guarded",
    ),
    (
        "crates/earshot-runtime/src/word_choices.rs",
        "drop_stale_tx",
        "the final pass's write transaction calls the guarded root through it",
    ),
    (
        "crates/earshot-runtime/src/vocabulary.rs",
        "syncing_turned_on_tx",
        "turning syncing on, setting up and joining hold their own transaction",
    ),
];

/// The named by-rowid writers of 16.4.
const ROWID_WRITERS: &[&str] = &[
    "erase_print_by_rowid",
    "erase_sample_by_rowid",
    "unlink_speaker_by_rowid",
    "tombstone_person_by_rowid",
];

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("the workspace root")
        .to_path_buf()
}

fn files() -> BTreeMap<String, String> {
    let mut paths: Vec<String> = ROOTS.iter().map(|(f, _)| (*f).to_owned()).collect();
    paths.extend(WRAPPERS.iter().map(|(f, _, _)| (*f).to_owned()));
    paths.push(format!("{STORE}delete.rs"));
    paths.push(format!("{STORE}replaced.rs"));
    // Every runtime source, for the `StoreTx::guarded` rule.
    let mut dirs = vec![workspace().join(RUNTIME)];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("the runtime sources") {
            let p = entry.expect("an entry").path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                let rel = p
                    .strip_prefix(workspace())
                    .expect("inside the workspace")
                    .to_string_lossy()
                    .replace('\\', "/");
                paths.push(rel);
            }
        }
    }
    paths.sort();
    paths.dedup();
    let root = workspace();
    paths
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(root.join(&p))
                .unwrap_or_else(|e| panic!("the scan cannot read {p}: {e}"));
            (p, text)
        })
        .collect()
}

/// Comments removed (line and block); string and char literals kept.
fn strip_comments(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            '"' => {
                out.push('"');
                i += 1;
                while i < b.len() && b[i] != '"' {
                    if b[i] == '\\' && i + 1 < b.len() {
                        out.push(b[i]);
                        i += 1;
                    }
                    out.push(b[i]);
                    i += 1;
                }
                out.push('"');
                i += 1;
            }
            '/' if b.get(i + 1) == Some(&'/') => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if b.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The function `name` in `src`: (signature between its parentheses, body between its braces,
/// body start, body end). `None` when not found or not parseable.
fn function(src: &str, name: &str) -> Option<(String, String, usize, usize)> {
    let mut from = 0;
    while let Some(at) = src[from..].find(&format!("fn {name}")) {
        let start = from + at;
        let after = start + 3 + name.len();
        let next = src[after..].chars().next()?;
        if next == '(' || next == '<' {
            let open = after + src[after..].find('(')?;
            let close = matching(src, open, '(', ')')?;
            let brace = close + src[close..].find('{')?;
            let mut depth = 0i32;
            let declaration = src[close..brace].chars().any(|ch| {
                match ch {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                ch == ';' && depth == 0
            });
            if declaration {
                from = after;
                continue;
            }
            let end = matching(src, brace, '{', '}')?;
            return Some((
                src[open + 1..close].to_owned(),
                src[brace + 1..end].to_owned(),
                brace,
                end,
            ));
        }
        from = after;
    }
    None
}

fn matching(src: &str, open: usize, a: char, b: char) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut prev = ' ';
    for (i, c) in src[open..].char_indices() {
        if in_str {
            if c == '"' && prev != '\\' {
                in_str = false;
            }
            prev = if prev == '\\' && c == '\\' { ' ' } else { c };
            continue;
        }
        if c == '"' {
            in_str = true;
        } else if c == a {
            depth += 1;
        } else if c == b {
            depth -= 1;
            if depth == 0 {
                return Some(open + i);
            }
        }
        prev = c;
    }
    None
}

fn has_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// Whether `text` names `word` as a whole word that is not a module path (`services::x`).
fn names_value(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let rest = &text[i + word.len()..];
        let after = rest.chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !rest.starts_with("::")
    })
}

/// The SQL constants of `guarded.rs`: name to text.
fn constants(src: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (i, _) in src.match_indices("const ") {
        let rest = &src[i + 6..];
        let Some((name, tail)) = rest.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !name.ends_with("_SQL") || !tail.trim_start().starts_with("&str") {
            continue;
        }
        let Some(q) = tail.find('"') else { continue };
        let Some(end) = tail[q..].find("\";") else {
            continue;
        };
        out.insert(name.to_owned(), tail[q + 1..q + end].to_owned());
    }
    out
}

/// The `SQL_OWNERS` table of `guarded.rs`.
fn owners(src: &str) -> Option<Vec<(String, String)>> {
    let at = src.find("pub const SQL_OWNERS")?;
    let open = at + src[at..].find("= &[")? + 3;
    let close = matching(src, open, '[', ']')?;
    let mut out = Vec::new();
    for pair in src[open + 1..close].split("),") {
        let parts: Vec<&str> = pair.split('"').collect();
        if parts.len() >= 4 {
            out.push((parts[1].to_owned(), parts[3].to_owned()));
        }
    }
    Some(out)
}

/// The `EXTERNAL_SQL_OWNERS` table of `guarded.rs`: (file, constant, owner).
fn external_owners(src: &str) -> Option<Vec<(String, String, String)>> {
    let at = src.find("pub const EXTERNAL_SQL_OWNERS")?;
    let open = at + src[at..].find("= &[")? + 3;
    let close = matching(src, open, '[', ']')?;
    let mut out = Vec::new();
    for entry in src[open + 1..close].split("),") {
        let parts: Vec<&str> = entry.split('"').collect();
        if parts.len() >= 6 {
            out.push((
                parts[1].to_owned(),
                parts[3].to_owned(),
                parts[5].to_owned(),
            ));
        }
    }
    Some(out)
}

/// Every failure the scan finds in `files` (empty: the guarded paths hold).
fn check(files: &BTreeMap<String, String>) -> Vec<String> {
    let mut bad = Vec::new();
    let stripped: BTreeMap<&str, String> = files
        .iter()
        .map(|(p, t)| (p.as_str(), strip_comments(t)))
        .collect();
    // Roots: a guarded parameter, no store handle, no connection or SQL.
    for (file, root) in ROOTS {
        let Some(src) = stripped.get(file) else {
            bad.push(format!("{file} not found"));
            continue;
        };
        let Some((sig, body, _, _)) = function(src, root) else {
            bad.push(format!("{file}: root {root} not found"));
            continue;
        };
        let guarded = ["GuardedTx", "GuardedRead", "ServicesRead", "ServicesWrite"]
            .iter()
            .any(|t| sig.contains(t));
        if !guarded {
            bad.push(format!("{file}: root {root} takes no guarded type"));
        }
        for t in ["Services", "Store", "StoreTx"] {
            if has_word(&sig, t) {
                bad.push(format!("{file}: root {root} takes a {t}"));
            }
        }
        for t in [
            ".conn",
            "execute(",
            "query(",
            "query_map(",
            "query_row(",
            "prepare(",
            "prepare_cached(",
            ".store()",
            ".parts()",
            "edit::write(",
            "&**db",
        ] {
            if body.contains(t) {
                bad.push(format!("{file}: root {root} holds {t}"));
            }
        }
        for t in ["services", "Store", "StoreTx", "Services"] {
            if names_value(&body, t) {
                bad.push(format!("{file}: root {root} names {t}"));
            }
        }
    }
    // Wrappers: one `services.guarded_*` call that calls the root.
    for (file, wrapper, root) in WRAPPERS {
        let Some(src) = stripped.get(file) else {
            bad.push(format!("{file} not found"));
            continue;
        };
        let Some((_, body, _, _)) =
            function(src, wrapper).filter(|(sig, _, _, _)| sig.contains("services"))
        else {
            bad.push(format!("{file}: wrapper {wrapper} not found"));
            continue;
        };
        let body: String = body.split_whitespace().collect();
        let one = body.starts_with("services.guarded_write(")
            || body.contains("(&services.guarded_read().read())");
        let calls = body.matches('(').count();
        if !one || !body.contains(root) || body.contains(';') || calls > 4 {
            bad.push(format!(
                "{file}: wrapper {wrapper} is more than one guarded call"
            ));
        }
    }
    // Engine glue: store access only through `services.guarded_*`.
    if let Some(src) = stripped.get(&format!("{RUNTIME}sync/engine.rs")[..]) {
        for glue in GLUE {
            let Some((_, body, _, _)) = function(src, glue) else {
                bad.push(format!("engine: glue {glue} not found"));
                continue;
            };
            for t in [".store()", ".parts()", "StoreTx", "&**db"] {
                if body.contains(t) {
                    bad.push(format!("engine: glue {glue} holds {t}"));
                }
            }
        }
    } else {
        bad.push("engine.rs not found".into());
    }
    // SQL constants and their owners.
    let guarded_path = format!("{STORE}guarded.rs");
    let Some(guarded) = stripped.get(&guarded_path[..]) else {
        bad.push("guarded.rs not found".into());
        return bad;
    };
    let consts = constants(guarded);
    let Some(owner_list) = owners(guarded) else {
        bad.push("SQL_OWNERS not found".into());
        return bad;
    };
    for name in consts.keys() {
        if !owner_list.iter().any(|(c, _)| c == name) {
            bad.push(format!("{name} has no owner"));
        }
    }
    for (c, owner) in &owner_list {
        if !consts.contains_key(c) {
            bad.push(format!("owner entry {c} matches no constant"));
        }
        let Some((_, _, start, end)) = function(guarded, owner) else {
            bad.push(format!("owner {owner} of {c} not found"));
            continue;
        };
        for (path, src) in stripped
            .iter()
            .filter(|(p, _)| !p.ends_with("sync_damaged_scan.rs"))
        {
            for (i, _) in src.match_indices(c.as_str()) {
                if !has_word(
                    &src[i.saturating_sub(1)..(i + c.len() + 1).min(src.len())],
                    c,
                ) {
                    continue;
                }
                let defining = src[..i].ends_with("const ");
                let listed = src[..i].ends_with('"');
                // The module's own test that pins a constant to the repository's.
                let pinned = src[..i].ends_with("super::");
                let inside = *path == guarded_path && i > start && i < end;
                if !defining && !listed && !inside && !pinned {
                    bad.push(format!("{c} used outside its owner {owner} ({path})"));
                }
            }
        }
        if let Some((_, body, _, _)) = function(guarded, owner) {
            for t in [
                "r.get::<",
                "r.get(",
                "get_id(",
                "get_opt_id(",
                "speaker_row",
                "person_row",
            ] {
                if body.contains(t) {
                    bad.push(format!("owner {owner} reads a column strictly ({t})"));
                }
            }
        }
        let text = consts.get(c).cloned().unwrap_or_default();
        if text.contains("UPDATE")
            && text.contains("rowid")
            && !ROWID_WRITERS.contains(&owner.as_str())
        {
            bad.push(format!(
                "{c} writes by rowid but {owner} is not a named writer"
            ));
        }
        if text.contains("zeroblob(") && owner != "erase_print_by_rowid" {
            bad.push(format!(
                "{c} zeroes a vector but {owner} is not erase_print_by_rowid"
            ));
        }
    }
    // No inline or formatted SQL in guarded.rs.
    for call in [
        "prepare_cached(",
        "prepare(",
        "query_row(",
        "execute_batch(",
    ] {
        for (i, _) in guarded.match_indices(call) {
            let arg: String = guarded[i + call.len()..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !consts.contains_key(&arg) && !guarded[..i].ends_with("fn ") {
                bad.push(format!(
                    "guarded.rs: {call} takes {arg:?}, not one of its constants"
                ));
            }
        }
    }
    for (i, _) in guarded.match_indices(".execute(") {
        let rest = guarded[i + 9..].trim_start();
        if !(rest.starts_with("params!") || rest.starts_with("[]")) {
            bad.push("guarded.rs: execute takes inline SQL".into());
        }
    }
    // The statements kept in their own module: registered, used only by their owners, and no
    // inline SQL in those owners.
    match external_owners(guarded) {
        None => bad.push("EXTERNAL_SQL_OWNERS not found".into()),
        Some(list) => {
            for (file, c, owner) in list {
                let path = format!("{STORE}{file}");
                let Some(src) = stripped.get(&path[..]) else {
                    bad.push(format!("{path} not found"));
                    continue;
                };
                if !src.contains(&format!("const {c}: &str")) {
                    bad.push(format!("owner entry {c} matches no constant in {file}"));
                }
                let Some((_, body, start, end)) = function(src, &owner) else {
                    bad.push(format!("owner {owner} of {c} not found"));
                    continue;
                };
                // A private constant: its own file is the only place it can be named.
                for (p, s) in stripped.iter().filter(|(p, _)| **p == path) {
                    for (i, _) in s.match_indices(c.as_str()) {
                        let word = !s[..i].ends_with(|ch: char| ch.is_alphanumeric() || ch == '_')
                            && !s[i + c.len()..]
                                .starts_with(|ch: char| ch.is_alphanumeric() || ch == '_');
                        if !word || s[..i].ends_with("const ") || s[..i].ends_with('"') {
                            continue;
                        }
                        if !(*p == path && i > start && i < end) {
                            bad.push(format!("{c} used outside its owner {owner} ({p})"));
                        }
                    }
                }
                for kw in ["\"SELECT", "\"UPDATE", "\"INSERT", "\"DELETE"] {
                    if body.contains(kw) {
                        bad.push(format!("owner {owner} runs inline SQL"));
                    }
                }
            }
        }
    }
    // `StoreTx::guarded` from runtime code only in its named exceptions.
    for (path, src) in &stripped {
        if !path.starts_with(RUNTIME) || path.ends_with("sync_damaged_scan.rs") {
            continue;
        }
        for (i, _) in src.match_indices(".guarded()") {
            let allowed = GUARDED_FROM_STORE_TX.iter().any(|(f, name, _)| {
                *path == *f && function(src, name).is_some_and(|(_, _, s, e)| i > s && i < e)
            });
            if !allowed {
                bad.push(format!(
                    "{path}: StoreTx::guarded outside its named exceptions"
                ));
            }
        }
    }
    // T14.26's one exception outside guarded.rs, used only by its writer.
    if let Some(delete) = stripped.get(&format!("{STORE}delete.rs")[..]) {
        let uses = delete.matches("ERASE_BY_ROWID").count();
        if uses != 2 {
            bad.push(format!("delete.rs: ERASE_BY_ROWID used {uses} times"));
        }
    }
    bad
}

#[test]
fn sync_damaged_rows_never_stop_the_guarded_paths() {
    let real = files();
    let found = check(&real);
    assert!(found.is_empty(), "{found:#?}");

    let mutate = |file: &str, f: &dyn Fn(&str) -> String| {
        let mut copy = real.clone();
        let text = f(&copy[file]);
        copy.insert(file.to_owned(), text);
        check(&copy)
    };
    let guarded = format!("{STORE}guarded.rs");
    let sync = format!("{STORE}sync.rs");
    let identities = format!("{STORE}identities.rs");
    let edit = format!("{RUNTIME}edit.rs");
    let engine = format!("{RUNTIME}sync/engine.rs");
    let ignore = format!("{RUNTIME}ignore_copy.rs");
    let insert_in = |src: &str, fn_name: &str, line: &str| -> String {
        let s = src.find(&format!("fn {fn_name}")).expect("fn");
        let b = s + src[s..].find('{').expect("body");
        format!("{}{{\n    {line}\n{}", &src[..b], &src[b + 1..])
    };
    // Each control fails for its own reason (what names it in the failure text).
    let fails = |found: Vec<String>, what: &str| {
        assert!(
            found.iter().any(|f| f.contains(what)),
            "the control did not fail with {what:?}: {found:#?}"
        );
    };
    // (1) a strict column read in an owner.
    fails(
        mutate(&guarded, &|s| {
            insert_in(
                s,
                "chain_step",
                "let _ = query(c, CHAIN_STEP_SQL, &[], |r| get_id(r, 0));",
            )
        }),
        "owner chain_step reads a column strictly",
    );
    // (2) a connection and SQL in a root.
    fails(
        mutate(&sync, &|s| {
            insert_in(s, "converge", "let _ = self.conn.prepare(\"SELECT 1\");")
        }),
        "root converge holds",
    );
    // (3) a root that is not guarded.
    fails(
        mutate(&identities, &|s| {
            s.replacen(
                "g: &mut GuardedTx<'_, '_>,\n    persons: &[EntityId],",
                "g: &mut StoreTx<'_>,\n    persons: &[EntityId],",
                1,
            )
        }),
        "root forget_identity takes",
    );
    // (4) a root renamed.
    fails(
        mutate(&sync, &|s| s.replace("fn edits_win", "fn edits_won")),
        "root edits_win not found",
    );
    // (5) zeroblob in another writer's statement, and the print eraser's statement used elsewhere.
    fails(
        mutate(&guarded, &|s| {
            s.replace(
                "display_name = coalesce(?5, display_name)",
                "display_name = coalesce(?5, display_name), x = zeroblob(1)",
            )
        }),
        "UNLINK_SPEAKER_BY_ROWID_SQL zeroes a vector",
    );
    fails(
        mutate(&guarded, &|s| {
            insert_in(
                s,
                "unlink_speaker_by_rowid",
                "let _ = ERASE_PRINT_BY_ROWID_SQL;",
            )
        }),
        "ERASE_PRINT_BY_ROWID_SQL used outside its owner",
    );
    // (6) a wrapper of more than one call, and the glue taking the store's parts.
    fails(
        mutate(&edit, &|s| {
            let at = s.find("pub fn remove_person(").expect("wrapper");
            let b = at + s[at..].find('{').expect("body");
            format!(
                "{}{{\n    let _ = services.store().person(person);\n{}",
                &s[..b],
                &s[b + 1..]
            )
        }),
        "wrapper remove_person is more than one",
    );
    fails(
        mutate(&engine, &|s| {
            insert_in(
                s,
                "apply_with",
                "let (store, hlc) = self.services.store().parts();",
            )
        }),
        "glue apply_with holds",
    );
    // ... while the glue's other uses of `Services` pass.
    assert!(
        mutate(&engine, &|s| insert_in(
            s,
            "apply_with",
            "let _ = self.services.leased_meetings();"
        ))
        .is_empty(),
        "store access only is judged"
    );
    // (7) the ignore copy rewritten to the old route.
    fails(
        mutate(&ignore, &|s| {
            let at = s.find("pub fn copy_once(").expect("wrapper");
            let b = at + s[at..].find('{').expect("body");
            let e = matching(s, b, '{', '}').expect("end");
            format!(
                "{}{{\n    crate::edit::write(services, |tx, stamp| {{ Ok(tx.meetings()?.len()) }})\n{}",
                &s[..b],
                &s[e..]
            )
        }),
        "wrapper copy_once is more than one",
    );
    // (9) `StoreTx::guarded` from runtime code outside its named exceptions.
    let words = format!("{RUNTIME}word_choices.rs");
    fails(
        mutate(&edit, &|s| {
            insert_in(s, "rename_speaker", "let _ = tx.guarded();")
        }),
        "StoreTx::guarded outside its named exceptions",
    );
    assert!(
        mutate(&words, &|s| insert_in(
            s,
            "drop_stale_tx",
            "let _ = tx.guarded();"
        ))
        .is_empty(),
        "a named exception passes"
    );
    // (10) a kept-in-place statement used from another function, and inline SQL in its owner.
    let replaced = format!("{STORE}replaced.rs");
    fails(
        mutate(&replaced, &|s| {
            insert_in(s, "rowid_of", "let _ = RESTAMP_BY_ROWID_SQL;")
        }),
        "RESTAMP_BY_ROWID_SQL used outside its owner",
    );
    fails(
        mutate(&replaced, &|s| {
            insert_in(
                s,
                "restamp_cleared",
                "let _ = c.execute(\"UPDATE segment SET deleted = 1\", []);",
            )
        }),
        "owner restamp_cleared runs inline SQL",
    );
    // (11) the tick's glue reading the store directly.
    fails(
        mutate(&engine, &|s| {
            insert_in(s, "pull", "let _ = self.services.store().outbox_len();")
        }),
        "glue pull holds .store()",
    );
    // (8) a root that calls a helper taking `&Services`.
    fails(
        mutate(&edit, &|s| {
            let s = s.replacen(
                "pub(crate) fn remove_person_in(\n    w: &mut crate::services::ServicesWrite<'_>,",
                "pub(crate) fn remove_person_in(\n    services: &Services,\n    w: &mut crate::services::ServicesWrite<'_>,",
                1,
            );
            insert_in(&s, "remove_person_in", "let _ = names_of(services);")
        }),
        "root remove_person_in takes a Services",
    );
}
