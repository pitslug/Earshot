//! The scan `cargo xtask bundle-check` runs over every installed file (T9.1j): nothing shipped may
//! name the user who built it or their home folder. Release builds use neutral paths
//! ([`crate::build::NeutralPaths`]), and this is what proves it.
//!
//! The bytes are searched as they are, without regard to ASCII case, in UTF-8 and in UTF-16 LE
//! (Windows strings and debug records), so a compressed file proves nothing about what it holds.
//! `uninstall.exe` is one such file: NSIS compresses its data.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::files_under;

/// A name shorter than this counts only as a whole path component: `dev` occurs by chance in any
/// large binary, a longer name does not.
const MIN_BARE_NAME: usize = 4;
/// How many places a finding quotes.
const MAX_QUOTES: usize = 3;
/// How far a quote reaches each way from what was found.
const QUOTE_REACH: usize = 96;

/// What the scan looks for: lower-case text in each encoding, each with what it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needles(Vec<Needle>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Needle {
    text: String,
    bytes: Vec<u8>,
    wide: bool,
}

impl Needles {
    /// The needles for this user and home folder. `home`'s last part counts as a name too, because
    /// a renamed account keeps its old folder.
    ///
    /// # Errors
    /// An empty name or home: a scan with nothing to look for would prove nothing. A name or home
    /// that is not ASCII: bytes are compared without regard to ASCII case only, so another spelling
    /// of it would pass unseen.
    pub fn new(user: &str, home: &str) -> Result<Self> {
        ensure!(!user.trim().is_empty(), "the build user's name is empty");
        ensure!(
            !home.trim().is_empty(),
            "the build user's home folder is empty"
        );
        ensure!(
            user.is_ascii() && home.is_ascii(),
            "the scan for the build user's paths handles ASCII names only, and this user's name or \
             home folder is not ASCII: run bundle-check from an account with an ASCII name"
        );
        let mut texts = Vec::new();
        let unix = home.replace('\\', "/");
        let doubled = home.replace('\\', "\\\\");
        texts.extend([home.to_owned(), unix.clone(), doubled]);
        let leaf = unix.rsplit('/').find(|p| !p.is_empty()).unwrap_or(user);
        for name in [user, leaf] {
            if name.chars().count() >= MIN_BARE_NAME {
                texts.push(name.to_owned());
            } else {
                texts.push(format!("\\{name}\\"));
                texts.push(format!("/{name}/"));
            }
        }
        let mut needles = Vec::new();
        for text in texts {
            let text = text.to_lowercase();
            if needles.iter().any(|n: &Needle| n.text == text) {
                continue;
            }
            needles.push(Needle {
                bytes: text.as_bytes().to_vec(),
                text: text.clone(),
                wide: false,
            });
            needles.push(Needle {
                bytes: text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                text,
                wide: true,
            });
        }
        Ok(Self(needles))
    }

    /// The needles for the user running this process.
    ///
    /// # Errors
    /// `USERNAME` or `USERPROFILE` is not set.
    pub fn of_this_user() -> Result<Self> {
        let var = |name: &str| std::env::var(name).with_context(|| format!("{name} is not set"));
        Self::new(&var("USERNAME")?, &var("USERPROFILE")?)
    }
}

/// One file that names the build machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The file.
    pub file: PathBuf,
    /// How many places, over all needles. A longer needle's place also counts for a shorter one.
    pub places: usize,
    /// What the first few places read, printable characters only.
    pub quotes: Vec<String>,
}

/// The printable text around `at`, with a UTF-16 string's zero bytes dropped.
fn quote(bytes: &[u8], at: usize) -> String {
    let start = at.saturating_sub(QUOTE_REACH);
    let end = (at + QUOTE_REACH).min(bytes.len());
    let printable = |b: &u8| b.is_ascii_graphic() || *b == b' ';
    let around = &bytes[start..end];
    let here = at - start;
    let first = around[..here]
        .iter()
        .rposition(|b| !printable(b) && *b != 0)
        .map_or(0, |i| i + 1);
    let last = around[here..]
        .iter()
        .position(|b| !printable(b) && *b != 0)
        .map_or(around.len(), |i| here + i);
    around[first..last]
        .iter()
        .filter(|b| **b != 0)
        .map(|b| char::from(*b))
        .collect()
}

/// Where `needle` occurs in `haystack`.
fn places_of(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let Some((&head, rest)) = needle.split_first() else {
        return Vec::new();
    };
    let mut places = Vec::new();
    let mut from = 0;
    while let Some(i) = haystack[from..].iter().position(|b| *b == head) {
        let at = from + i;
        if haystack[at + 1..].starts_with(rest) {
            places.push(at);
        }
        from = at + 1;
    }
    places
}

/// What `bytes` say about the build machine, if anything.
#[must_use]
pub fn scan_bytes(file: &Path, bytes: &[u8], needles: &Needles) -> Option<Finding> {
    let lower = bytes.to_ascii_lowercase();
    let mut places = 0;
    let mut quotes: Vec<String> = Vec::new();
    for needle in &needles.0 {
        let found = places_of(&lower, &needle.bytes);
        places += found.len();
        for at in found {
            if quotes.len() == MAX_QUOTES {
                break;
            }
            let q = quote(bytes, at);
            if !quotes.contains(&q) {
                quotes.push(q);
            }
        }
    }
    (places > 0).then(|| Finding {
        file: file.to_path_buf(),
        places,
        quotes,
    })
}

/// Scans every file below `dir` except the paths in `skip` (relative to it), and returns the number
/// of files scanned and what names the build machine.
///
/// # Errors
/// A folder or file that cannot be read.
pub fn scan_tree(dir: &Path, skip: &[PathBuf], needles: &Needles) -> Result<(usize, Vec<Finding>)> {
    let mut scanned = 0;
    let mut findings = Vec::new();
    for file in &files_under(dir)? {
        let rel = file.strip_prefix(dir).unwrap_or(file);
        if skip.iter().any(|s| s == rel) {
            continue;
        }
        let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
        scanned += 1;
        findings.extend(scan_bytes(rel, &bytes, needles));
    }
    Ok((scanned, findings))
}

/// One line per finding, for the verdict.
#[must_use]
pub fn describe(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| format!("{}: {} places, {:?}", f.file.display(), f.places, f.quotes))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = r"C:\Users\Build Person";

    fn needles() -> Needles {
        Needles::new("buildperson", HOME).unwrap()
    }

    fn tree(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("earshot-paths-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn wide(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn build_bundle_scan_catches_a_planted_path_in_any_form_it_is_stored() {
        let planted: Vec<(&str, Vec<u8>)> = vec![
            (
                "cargo path",
                br"\0assert C:\Users\Build Person\.cargo\registry\ggml.c".to_vec(),
            ),
            ("forward slashes", b"c:/users/build person/x.rs".to_vec()),
            (
                "doubled backslashes",
                br#""C:\\Users\\Build Person\\x""#.to_vec(),
            ),
            ("upper case", br"C:\USERS\BUILD PERSON\X".to_vec()),
            ("wide", wide(r"C:\Users\Build Person\x.pdb")),
            ("the name alone", b"built by BuildPerson today".to_vec()),
            ("the name, wide", wide("buildperson")),
            ("the folder's name alone", b"home=Build Person".to_vec()),
        ];
        let dir = tree("planted");
        std::fs::write(dir.join("clean.dll"), b"nothing of note, C:\\eb\\host\\x.c").unwrap();
        for (i, (what, bytes)) in planted.iter().enumerate() {
            let name = format!("file{i}.dll");
            let file = dir.join("sub").join(&name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, bytes).unwrap();
            let (scanned, found) = scan_tree(&dir, &[], &needles()).unwrap();
            assert_eq!(scanned, 2, "{what}");
            assert_eq!(found.len(), 1, "{what}: {found:?}");
            assert_eq!(found[0].file, Path::new("sub").join(&name), "{what}");
            assert!(found[0].places >= 1, "{what}");
            // Only the file named is left out, and it does not count as scanned.
            let skip = [Path::new("sub").join(&name)];
            assert_eq!(scan_tree(&dir, &skip, &needles()).unwrap(), (1, vec![]));
            std::fs::remove_file(&file).unwrap();
        }
        assert_eq!(scan_tree(&dir, &[], &needles()).unwrap().1, []);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_bundle_scan_quotes_where_a_path_came_from() {
        let mut bytes = vec![0xff, 0xfe, 0x01];
        bytes.extend_from_slice(br"C:\Users\Build Person\.cargo\ggml.c");
        bytes.extend_from_slice(&[0x02, 0x03]);
        bytes.extend(wide(r"C:\Users\Build Person\a.pdb"));
        let found = scan_bytes(Path::new("x.dll"), &bytes, &needles()).unwrap();
        assert!(
            found
                .quotes
                .iter()
                .any(|q| q == r"C:\Users\Build Person\.cargo\ggml.c"),
            "{found:?}"
        );
        assert!(
            found
                .quotes
                .iter()
                .any(|q| q.contains(r"C:\Users\Build Person\a.pdb")),
            "{found:?}"
        );
        assert!(found.quotes.len() <= MAX_QUOTES);
        assert!(describe(&[found]).contains("x.dll"));
    }

    #[test]
    fn build_bundle_scan_counts_a_short_name_only_as_a_path_part() {
        let short = Needles::new("dev", r"C:\Users\dev").unwrap();
        assert!(scan_bytes(Path::new("a"), b"device developer", &short).is_none());
        assert!(scan_bytes(Path::new("a"), br"C:\Users\dev\x", &short).is_some());
        assert!(scan_bytes(Path::new("a"), br"D:\work\dev\x", &short).is_some());
        assert!(scan_bytes(Path::new("a"), b"/home/dev/x", &short).is_some());
    }

    #[test]
    fn build_bundle_scan_needs_something_to_look_for() {
        assert!(Needles::new("", HOME).is_err());
        assert!(Needles::new("  ", HOME).is_err());
        assert!(Needles::new("x", "").is_err());
        // One it could miss is refused rather than scanned for.
        for (user, home) in [("Zoë", r"C:\Users\Zoe"), ("Zoe", r"C:\Users\Zoë")] {
            let e = Needles::new(user, home).unwrap_err().to_string();
            assert!(e.contains("ASCII"), "{e}");
        }
        // A file that cannot be read is an error, not a pass.
        let dir = tree("unreadable");
        let open = dir.join("held.dll");
        std::fs::write(&open, b"x").unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let held = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&open)
                .unwrap();
            assert!(scan_tree(&dir, &[], &needles()).is_err());
            drop(held);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
