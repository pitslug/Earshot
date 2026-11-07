//! Text helpers: file names, em dashes, YAML scalars, tags, dates and times.

use crate::asr::MeetingTime;

/// Longest title (in characters) kept in a file name.
pub const MAX_TITLE_CHARS: usize = 100;

/// Longest title in UTF-8 bytes: with the 17-byte date prefix, `.md` and a ` (n)` collision
/// suffix the file name stays under the 255-byte limit of ext4 and most file systems.
pub const MAX_TITLE_BYTES: usize = 200;

const EM_DASH: char = '\u{2014}';
const EN_DASH: char = '\u{2013}';

/// A title made safe for a file name on Windows, Android and in Obsidian links: path and
/// reserved characters (`\ / : * ? " < > |`) and link-breaking ones (`# ^ [ ]`) become
/// `-`, em and en dashes become `-`, whitespace and control characters collapse to one
/// space, trailing dots and spaces are removed, and the result is cut to
/// [`MAX_TITLE_CHARS`] and [`MAX_TITLE_BYTES`] on a character boundary. May return an
/// empty string.
#[must_use]
pub fn sanitize_file_name(title: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in title.chars() {
        let c = match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '^' | '[' | ']'
            | EM_DASH | EN_DASH => '-',
            c if c.is_whitespace() || c.is_control() => ' ',
            c => c,
        };
        if c == ' ' {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    let mut cut = String::new();
    for c in out.chars().take(MAX_TITLE_CHARS) {
        if cut.len() + c.len_utf8() > MAX_TITLE_BYTES {
            break;
        }
        cut.push(c);
    }
    cut.trim_end_matches(['.', ' ']).to_owned()
}

/// Replaces each run of em dashes, with the spaces around it, by `" - "` (PLAN.md 3.8,
/// D12). En dashes and hyphens are left alone.
#[must_use]
pub fn replace_em_dash(s: &str) -> String {
    if !s.contains(EM_DASH) {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != EM_DASH {
            out.push(c);
            continue;
        }
        while out.ends_with(' ') {
            out.pop();
        }
        while chars.peek().is_some_and(|&n| n == EM_DASH || n == ' ') {
            chars.next();
        }
        out.push_str(" - ");
    }
    out
}

/// Makes rendered content unable to contain a region marker (or any HTML comment).
pub(super) fn neutralise(s: &str) -> String {
    s.replace("<!--", "&lt;!--")
}

/// A YAML double-quoted scalar.
pub(super) fn yaml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// An Obsidian tag: leading `#` removed, whitespace and characters other than letters,
/// digits, `_`, `-` and `/` replaced by `-`. `None` if nothing is left.
pub(super) fn sanitize_tag(t: &str) -> Option<String> {
    let t = t.trim().trim_start_matches('#').trim();
    let s: String = t
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '/') {
                c
            } else {
                '-'
            }
        })
        .collect();
    (!s.is_empty()).then_some(s)
}

/// A tag as a YAML scalar: plain when unambiguous, otherwise double-quoted.
pub(super) fn yaml_tag(t: &str) -> String {
    const RESERVED: [&str; 10] = [
        "true", "false", "null", "yes", "no", "on", "off", "y", "n", "~",
    ];
    let plain = t.chars().next().is_some_and(char::is_alphabetic)
        && t.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
        && !RESERVED.contains(&t.to_ascii_lowercase().as_str());
    if plain { t.to_owned() } else { yaml_str(t) }
}

/// `H:MM:SS` (hours unpadded) or `HH:MM:SS` with `pad_hours`.
pub(super) fn clock(ms: u64, pad_hours: bool) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if pad_hours {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h}:{m:02}:{s:02}")
    }
}

/// `HH:MM:SS` of a meeting time.
pub(super) fn stamp(t: MeetingTime) -> String {
    clock(t.as_millis(), true)
}

/// Local civil date and time of a Unix time: `(year, month, day, hour, minute)`.
pub(super) fn local_civil(unix_ms: u64, utc_offset_minutes: i32) -> (i64, u32, u32, u32, u32) {
    let secs =
        i64::try_from(unix_ms / 1000).unwrap_or(i64::MAX) + i64::from(utc_offset_minutes) * 60;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, (sod / 3600) as u32, ((sod / 60) % 60) as u32)
}

/// Days since 1970-01-01 to a proleptic Gregorian date. Howard Hinnant, "chrono-Compatible
/// Low-Level Date Algorithms", `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}
