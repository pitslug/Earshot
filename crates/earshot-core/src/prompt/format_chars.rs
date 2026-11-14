//! Invisible formatting characters (T12.46): Unicode general category Cf, among them the
//! zero-width characters (U+200B to U+200D, U+2060, U+FEFF) and the text-direction marks,
//! embeddings, overrides and isolates (U+200E, U+200F, U+202A to U+202E, U+2066 to U+2069).
//! They are invisible on the Settings pages yet reach the model, so a Dictionary word or a
//! summary style holding one is refused ([`super::Vocabulary::new`],
//! [`super::SummaryStyle::new`]).
//!
//! The ranges are the Cf entries of the Unicode Character Database 16.0
//! (`DerivedGeneralCategory.txt`); core has no Unicode table crate. The UI mock checks the
//! same rule with JavaScript's `\p{Cf}` (`packages/ui/src/ipc/mockSummaries.ts`).

/// The Cf ranges, inclusive, in code point order.
const FORMAT_RANGES: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

/// Whether `c` is a format character (Unicode general category Cf).
#[must_use]
pub fn is_format_char(c: char) -> bool {
    let c = u32::from(c);
    FORMAT_RANGES
        .iter()
        .any(|&(first, last)| (first..=last).contains(&c))
}

/// Whether `text` holds any format character.
#[must_use]
pub fn has_format_char(text: &str) -> bool {
    text.chars().any(is_format_char)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_chars_cover_zero_width_and_direction_marks() {
        for c in [
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{2060}',
            '\u{FEFF}',
            '\u{200E}',
            '\u{200F}',
            '\u{202A}',
            '\u{202E}',
            '\u{2066}',
            '\u{2069}',
            '\u{00AD}',
            '\u{E0001}',
        ] {
            assert!(is_format_char(c), "{:04X}", u32::from(c));
        }
        // Visible text, white space and control characters are other categories.
        for c in [
            'a', ' ', '\u{00A0}', '\u{2028}', '\t', '\u{85}', '\u{2010}', '\u{2070}', 'é',
        ] {
            assert!(!is_format_char(c), "{:04X}", u32::from(c));
        }
    }

    #[test]
    fn format_ranges_are_sorted_and_disjoint() {
        for pair in FORMAT_RANGES.windows(2) {
            assert!(pair[0].0 <= pair[0].1 && pair[0].1 < pair[1].0, "{pair:X?}");
        }
    }
}
