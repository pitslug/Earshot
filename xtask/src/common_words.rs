//! `cargo xtask common-words` (T12.15eb): the common English words that the transcript's word
//! suggestions never touch (`earshot_core::vocabulary::suggest`), made reproducibly from the
//! pinned AMI manual annotations.
//!
//! The rule is T12.15b's (docs/vocabulary.md, "Which words are listed"), moved here from the
//! `vocab_eval` example unchanged: the [`COMMON_WORDS`] most frequent tokens of every AMI words
//! file (`<meeting>.<speaker>.words.xml`) outside [`HELD_SERIES`], one text per file read by
//! the reference rule (`asr_eval/refs.rs` `parse_ami_words`: no punctuation, no unfinished
//! fragments, `&#39;` decoded, spelled-letter underscores removed; a word without a start time
//! still counts), tokens from the WER normaliser (`normalize_english`, split on white space),
//! ranked by count, ties in byte order of the token.
//!
//! Without `--check` the list (one token per line, in rank order) and its NOTICE (the AMI
//! attribution and licence, CC BY 4.0, the source archive's SHA-256) are written to
//! [`OUT_DIR`]; with it they are generated in memory and compared byte for byte with the
//! committed ones. The committed list's SHA-256 is pinned in [`FILE_SHA256`] and checked by
//! `common_words_committed_files_match_their_pins` (no fixtures needed). The words files it
//! reads are not members of `ami_annotation_members()` (there are hundreds): their names and
//! bytes are pinned together by [`INPUTS_SHA256`] over [`INPUT_FILES`] files, checked on every
//! generation before anything is counted.

use std::collections::HashMap;
use std::path::Path;

use crate::archive;
use crate::bench_meeting::sha256_hex;
use crate::fixtures::{AMI_ANNOTATIONS_DIR, AMI_ANNOTATIONS_PATH};
use crate::manifest::Manifest;
use crate::t613::ami_refs as refs;
use anyhow::{Context, Result, bail, ensure};
use earshot_core::metrics::normalize::normalize_english;

/// How many of AMI's most frequent tokens are common words.
pub const COMMON_WORDS: usize = 5_000;
/// The AMI series of the scenario-only dev and test partitions, left out of the common words.
pub const HELD_SERIES: [&str; 10] = [
    "ES2003", "ES2011", "IS1008", "TS3004", "TS3006", "ES2004", "ES2014", "IS1009", "TS3003",
    "TS3007",
];
/// Where the files go, relative to the repo root.
pub const OUT_DIR: &str = "crates/earshot-core/src/vocabulary/data";
/// The list's file name.
pub const FILE: &str = "common_words.txt";
/// The attribution file's name.
pub const NOTICE_FILE: &str = "common_words.txt.NOTICE";
/// SHA-256 of the committed list.
pub const FILE_SHA256: &str = "1776b4a476c59092adf7b4dbffbb1294baaf160ece1859edfd8f77d907e86801";

/// How many AMI words files outside [`HELD_SERIES`] the list is made from.
pub const INPUT_FILES: usize = 527;
/// [`inputs_digest`] of those files: they are read outside `ami_annotation_members()`, so
/// their names and bytes are pinned here by one digest instead of one entry each.
pub const INPUTS_SHA256: &str = "4351f547c0b5f8d2727ab853a4f5a769e55610479bdf7965d62e5d2f3243a246";

/// SHA-256 over `<name> <sha256>\n` of every `(file name, SHA-256 of its bytes)` input, in the
/// order given (sorted by name in [`ami_common`]).
#[must_use]
pub fn inputs_digest(inputs: &[(String, String)]) -> String {
    let mut text = String::new();
    for (name, sha) in inputs {
        text.push_str(name);
        text.push(' ');
        text.push_str(sha);
        text.push('\n');
    }
    sha256_hex(text.as_bytes())
}

/// Normalised tokens of `text` (the WER normaliser, split on white space).
fn tokens(text: &str) -> Vec<String> {
    normalize_english(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// The `n` most frequent tokens of `texts`, most frequent first (ties in byte order).
#[must_use]
pub fn most_frequent(texts: impl IntoIterator<Item = String>, n: usize) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for text in texts {
        for t in tokens(&text) {
            *counts.entry(t).or_default() += 1;
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(n).map(|(t, _)| t).collect()
}

/// Whether an AMI words file (`<meeting>.<speaker>.words.xml`) belongs to a held series.
#[must_use]
pub fn held(file_name: &str) -> bool {
    HELD_SERIES.iter().any(|s| file_name.starts_with(s))
}

/// The common words of the AMI words files in `words_dir`, most frequent first.
///
/// # Errors
/// An unreadable folder or file, a file the reference rule cannot parse, or fewer than 100
/// words files (an incomplete extraction).
pub fn ami_common(words_dir: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(words_dir)
        .with_context(|| words_dir.display().to_string())?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".words.xml") && !held(n))
        .collect();
    names.sort();
    ensure!(
        names.len() >= 100,
        "{}: only {} AMI words files; run `cargo xtask fetch-fixtures --set ami`",
        words_dir.display(),
        names.len()
    );
    let mut texts = Vec::with_capacity(names.len());
    let mut inputs = Vec::with_capacity(names.len());
    for name in &names {
        let p = words_dir.join(name);
        let bytes = std::fs::read(&p).with_context(|| p.display().to_string())?;
        inputs.push((name.clone(), sha256_hex(&bytes)));
        let words = refs::parse_ami_words(&refs::with_start_times(&refs::latin1(&bytes)))
            .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
        texts.push(
            words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    let digest = inputs_digest(&inputs);
    ensure!(
        inputs.len() == INPUT_FILES && digest == INPUTS_SHA256,
        "{}: the AMI words files read ({} files, digest {digest}) are not the pinned ones \
         ({INPUT_FILES} files, digest {INPUTS_SHA256}); run `cargo xtask fetch-fixtures --set ami \
         --verify`",
        words_dir.display(),
        inputs.len()
    );
    let common = most_frequent(texts, COMMON_WORDS);
    ensure!(
        common.len() == COMMON_WORDS,
        "{}: only {} distinct tokens",
        words_dir.display(),
        common.len()
    );
    Ok(common)
}

/// The list file: one token per line, LF.
#[must_use]
pub fn render(words: &[String]) -> String {
    let mut s = String::new();
    for w in words {
        s.push_str(w);
        s.push('\n');
    }
    s
}

/// The attribution written beside the list.
#[must_use]
pub fn notice(archive_sha256: &str, file_sha256: &str, count: usize) -> String {
    format!(
        "{FILE} - data derived from a third-party corpus\n\
         \n\
         Source:   AMI Meeting Corpus manual annotations v1.6.2 (Carletta et al., 2005), \
         University of Edinburgh and the AMI Consortium, https://groups.inf.ed.ac.uk/ami/corpus/\n\
         Fetched:  https://groups.inf.ed.ac.uk/ami/AMICorpusAnnotations/ami_public_manual_1.6.2.zip\n\
         Archive:  SHA-256 {archive_sha256} (fixtures/manifest.toml, `{AMI_ANNOTATIONS_PATH}`)\n\
         SHA-256:  {file_sha256}\n\
         Entries:  {count} tokens, one per line, most frequent first\n\
         Made by:  cargo xtask common-words (xtask/src/common_words.rs); --check regenerates it\n\
         Used by:  crates/earshot-core/src/vocabulary/suggest.rs (include_str!)\n\
         \n\
         Changes: the {COMMON_WORDS} most frequent tokens of every words file \
         (words/<meeting>.<speaker>.words.xml) outside the series {held}, after the WER \
         normaliser; ties in byte order. Only the tokens are kept, never their counts or \
         the text they came from.\n\
         \n\
         Licence: Creative Commons Attribution 4.0 International (CC BY 4.0), \
         https://creativecommons.org/licenses/by/4.0/ (corpus licence: \
         https://groups.inf.ed.ac.uk/ami/corpus/license.shtml).\n",
        held = HELD_SERIES.join(", "),
    )
}

/// The list and its NOTICE from the fixture cache, after checking that the extracted
/// annotations came from the archive the manifest pins.
fn generate(root: &Path) -> Result<(String, String)> {
    let manifest = Manifest::load(&root.join("fixtures").join("manifest.toml"))?;
    let archive_sha = &manifest
        .artefacts
        .iter()
        .find(|a| a.path == AMI_ANNOTATIONS_PATH)
        .with_context(|| format!("fixtures/manifest.toml has no {AMI_ANNOTATIONS_PATH}"))?
        .sha256;
    let fixtures = crate::fixtures::cache_dir()?;
    let into = fixtures.join(AMI_ANNOTATIONS_DIR);
    if !archive::is_extracted(&into, archive_sha) {
        bail!(
            "{} is not an extraction of the pinned annotations; run `cargo xtask fetch-fixtures --set ami`",
            into.display()
        );
    }
    let words = ami_common(&into.join("words"))?;
    let list = render(&words);
    let file_sha = sha256_hex(list.as_bytes());
    let notice = notice(archive_sha, &file_sha, words.len());
    Ok((list, notice))
}

/// Writes the files, or with `check` compares a fresh generation with the committed ones.
///
/// # Errors
/// Missing fixtures, or with `check` any byte of difference.
pub fn run(root: &Path, check: bool) -> Result<()> {
    let (list, notice) = generate(root)?;
    let out = root.join(OUT_DIR);
    if !check {
        std::fs::create_dir_all(&out)?;
        std::fs::write(out.join(FILE), &list)?;
        std::fs::write(out.join(NOTICE_FILE), &notice)?;
        println!(
            "common-words: wrote {} ({} tokens, sha256 {})",
            out.join(FILE).display(),
            list.lines().count(),
            sha256_hex(list.as_bytes())
        );
        return Ok(());
    }
    for (name, fresh) in [(FILE, &list), (NOTICE_FILE, &notice)] {
        let committed = std::fs::read(out.join(name))
            .with_context(|| format!("{} is not committed", out.join(name).display()))?;
        if fresh.as_bytes() != committed.as_slice() {
            bail!("{name} differs from a fresh generation; run `cargo xtask common-words`");
        }
    }
    println!(
        "common-words: the committed files equal a fresh generation ({} tokens)",
        list.lines().count()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits in the repo root")
            .to_path_buf()
    }

    #[test]
    fn common_words_inputs_digest_covers_every_name_and_byte() {
        let one = |name: &str, bytes: &[u8]| (name.to_owned(), sha256_hex(bytes));
        let inputs = [one("a.A.words.xml", b"x"), one("b.B.words.xml", b"y")];
        let base = inputs_digest(&inputs);
        assert_eq!(base.len(), 64);
        // A changed byte, a renamed file, a missing file or another order: another digest.
        for other in [
            vec![one("a.A.words.xml", b"z"), inputs[1].clone()],
            vec![one("c.A.words.xml", b"x"), inputs[1].clone()],
            vec![inputs[0].clone()],
            vec![inputs[1].clone(), inputs[0].clone()],
        ] {
            assert_ne!(inputs_digest(&other), base, "{other:?}");
        }
        assert_eq!(INPUTS_SHA256.len(), 64);
    }

    #[test]
    fn common_words_committed_files_match_their_pins() {
        let out = repo().join(OUT_DIR);
        let list = std::fs::read_to_string(out.join(FILE)).unwrap();
        assert_eq!(sha256_hex(list.as_bytes()), FILE_SHA256);
        let words: Vec<&str> = list.lines().collect();
        assert_eq!(words.len(), COMMON_WORDS);
        assert!(list.ends_with('\n') && !list.contains('\r'));
        let distinct: std::collections::HashSet<&str> = words.iter().copied().collect();
        assert_eq!(distinct.len(), COMMON_WORDS);
        assert!(
            words
                .iter()
                .all(|w| !w.is_empty() && !w.contains(char::is_whitespace))
        );
        // The NOTICE names the archive the manifest pins, the list's own digest and its size.
        let manifest = Manifest::load(&repo().join("fixtures").join("manifest.toml")).unwrap();
        let archive_sha = &manifest
            .artefacts
            .iter()
            .find(|a| a.path == AMI_ANNOTATIONS_PATH)
            .unwrap()
            .sha256;
        assert_eq!(
            std::fs::read_to_string(out.join(NOTICE_FILE)).unwrap(),
            notice(archive_sha, FILE_SHA256, COMMON_WORDS)
        );
    }

    #[test]
    fn common_words_most_frequent_ranks_by_count_then_text() {
        let ranked = most_frequent(["b a a c".to_owned(), "c b".to_owned()], 2);
        // a 2, b 2, c 2: text order keeps a and b.
        assert_eq!(ranked, ["a", "b"]);
        let ranked = most_frequent(["z z z y".to_owned(), "x y".to_owned()], 2);
        assert_eq!(ranked, ["z", "y"]);
        assert_eq!(render(&ranked), "z\ny\n");
    }

    #[test]
    fn common_words_ami_words_without_a_start_time_still_count() {
        let xml = "<w nite:id=\"a\" starttime=\"1.5\" endtime=\"2\">Uh</w>\
                   <w nite:id=\"b\">T_V_</w><w nite:id=\"c\" punc=\"true\">.</w>\
                   <w nite:id=\"d\" trunc=\"true\">Defin</w><w nite:id=\"e\">don&#39;t</w>";
        let words = refs::parse_ami_words(&refs::with_start_times(xml)).unwrap();
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Uh", "TV", "don't"]);
        assert_eq!(words[0].start, 1.5);
        assert!(refs::parse_ami_words(xml).is_err());
    }

    #[test]
    fn common_words_held_series_are_the_ten_dev_and_test_series() {
        assert!(held("ES2003a.A.words.xml"));
        assert!(held("TS3007d.D.words.xml"));
        assert!(!held("ES2002a.A.words.xml"));
        assert!(!held("EN2001a.B.words.xml"));
    }
}
