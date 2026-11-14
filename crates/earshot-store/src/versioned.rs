//! The framing of a versioned device-local record (T12.52): `version (u32 LE) | payload`.
//!
//! Most records keep their version outside a JSON payload, so a reader checks the version
//! before parsing anything. Each record keeps its own version constant, the versions it reads
//! and what it does with a value it cannot read; this module only writes and splits the
//! header, so every record's stored bytes stay what they were.

use serde::Serialize;

/// `version` (u32 LE) followed by the compact JSON of `value`.
///
/// # Errors
/// When `value` does not serialise.
pub fn encode_versioned<T: Serialize + ?Sized>(
    version: u32,
    value: &T,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut out = version.to_le_bytes().to_vec();
    serde_json::to_writer(&mut out, value)?;
    Ok(out)
}

/// The version and the payload after it; `None` when `bytes` is too short for a version.
#[must_use]
pub fn split_version(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let (head, payload) = bytes.split_first_chunk::<4>()?;
    Some((u32::from_le_bytes(*head), payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_record_is_the_version_then_compact_json() {
        let bytes = encode_versioned(3, &serde_json::json!({ "a": [1, 2] })).unwrap();
        assert_eq!(bytes, b"\x03\x00\x00\x00{\"a\":[1,2]}");
        assert_eq!(split_version(&bytes), Some((3, &b"{\"a\":[1,2]}"[..])));
        let big = encode_versioned(0x0102_0304, &[true]).unwrap();
        assert_eq!(big, b"\x04\x03\x02\x01[true]");
    }

    #[test]
    fn versioned_record_too_short_for_a_version_splits_to_nothing() {
        assert_eq!(split_version(&[]), None);
        assert_eq!(split_version(&[1, 0, 0]), None);
        assert_eq!(split_version(&[1, 0, 0, 0]), Some((1, &[][..])));
    }
}
