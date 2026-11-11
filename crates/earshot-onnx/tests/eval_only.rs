//! Evaluation-only loaders (T12.12) stay out of shipped code.

use std::path::{Path, PathBuf};

fn scan(dir: &Path, names: &[&str], skip: &[&str], found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan(&path, names, skip, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let file = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if names.iter().any(|n| text.contains(n)) && !skip.contains(&file) {
                found.push(path);
            }
        }
    }
}

#[test]
fn eval_only_context_loaders_are_used_by_no_shipped_code() {
    // Split so this file does not match itself.
    let names = [
        concat!("load_with_", "session_entries"),
        concat!("load_with_", "encoder"),
    ];
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut found = Vec::new();
    for dir in [
        "crates/earshot-onnx/src",
        "crates/earshot-runtime/src",
        "crates/earshot-cli/src",
        "apps/desktop/src-tauri/src",
    ] {
        let dir = repo.join(dir);
        assert!(dir.is_dir(), "{} is not a folder", dir.display());
        // The two files that define them.
        scan(&dir, &names, &["runtime.rs", "parakeet.rs"], &mut found);
    }
    assert!(found.is_empty(), "shipped code calls them: {found:?}");
}
