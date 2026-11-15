//! Tests of sealing, row keys, the audio guard and pairing codes (T11.2a).

use earshot_core::identity::{DeviceId, EntityId, HlcTimestamp, SyncMeta};
use earshot_core::model::Meeting;
use earshot_core::prompt::Template;

use crate::seal::seal_plaintext;
use crate::*;

const DEVICE: DeviceId =
    DeviceId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 9]);

fn id(n: u8) -> EntityId {
    EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, n])
}

fn stamp(wall_ms: u64, logical: u32) -> HlcTimestamp {
    HlcTimestamp {
        wall_ms,
        logical,
        node: DEVICE,
    }
}

fn key(n: u8) -> VaultKey {
    VaultKey::from_bytes([n; 32])
}

/// An invented meeting.
fn meeting(n: u8, title: &str) -> Meeting {
    let at = stamp(1_800_000_000_000, 0);
    Meeting {
        id: id(n),
        meta: SyncMeta {
            created_at: at,
            updated_at: stamp(1_800_000_000_500, 2),
            deleted: false,
            origin_device_id: DEVICE,
        },
        title: title.into(),
        started_unix_ms: 1_800_000_000_000,
        utc_offset_minutes: 600,
        duration_ms: Some(60_000),
        tags: vec!["planning".into()],
        template: Template::General,
        folder_id: None,
        summary_kept_local: true,
    }
}

fn opened(o: Opened) -> OpenedRow {
    match o {
        Opened::Row(r) => r,
        Opened::NewerFormat(f) => panic!("newer format {f}"),
    }
}

#[test]
fn sync_sealed_row_opens_to_the_same_envelope() {
    let keys = key(1).keys();
    let m = meeting(1, "Invented planning chat");
    let sealed = seal_row(&keys, &m).unwrap();
    assert_eq!(sealed.key, keys.row_key(m.id));
    assert_eq!(sealed.stamp, m.meta.updated_at);
    // The relay sees neither the title nor the id.
    let hay = String::from_utf8_lossy(&sealed.body);
    assert!(!hay.contains("Invented"));
    assert!(!hay.contains(&m.id.to_string()));
    let row = opened(open_row(&keys, sealed.key, sealed.stamp, &sealed.body).unwrap());
    assert_eq!(row.format, ENVELOPE_FORMAT);
    assert_eq!(row.entity, "meeting");
    assert_eq!(row.id, m.id);
    assert_eq!(row.meta, m.meta);
    let back: Meeting = serde_json::from_value(serde_json::json!({
        "id": row.id, "meta": row.meta,
        "title": row.record["title"], "started_unix_ms": row.record["started_unix_ms"],
        "utc_offset_minutes": row.record["utc_offset_minutes"],
        "duration_ms": row.record["duration_ms"], "tags": row.record["tags"],
        "template": row.record["template"], "folder_id": row.record["folder_id"],
        "summary_kept_local": row.record["summary_kept_local"],
    }))
    .unwrap();
    assert_eq!(back, m);
    // Two seals of one row differ (a fresh nonce each time) and both open.
    let again = seal_row(&keys, &m).unwrap();
    assert_ne!(again.body, sealed.body);
    open_row(&keys, again.key, again.stamp, &again.body).unwrap();
}

#[test]
fn sync_sealed_row_refuses_any_change() {
    let keys = key(1).keys();
    let m = meeting(1, "Invented");
    let s = seal_row(&keys, &m).unwrap();
    // Every byte flipped, one at a time: the leading byte is the version, the rest the tag.
    for i in 0..s.body.len() {
        let mut body = s.body.clone();
        body[i] ^= 0x01;
        let want = if i == 0 {
            Unreadable::Version
        } else {
            Unreadable::Tag
        };
        assert_eq!(
            open_row(&keys, s.key, s.stamp, &body),
            Err(want),
            "byte {i}"
        );
    }
    // Another vault, another row key, another stamp: all refused by the tag.
    assert_eq!(
        open_row(&key(2).keys(), s.key, s.stamp, &s.body),
        Err(Unreadable::Tag)
    );
    assert_eq!(
        open_row(&keys, keys.row_key(id(2)), s.stamp, &s.body),
        Err(Unreadable::Tag)
    );
    let other_node = HlcTimestamp {
        node: DeviceId::from_bytes([7; 16]),
        ..s.stamp
    };
    for other in [
        stamp(1_800_000_000_501, 2),
        stamp(1_800_000_000_500, 3),
        other_node,
    ] {
        assert_eq!(open_row(&keys, s.key, other, &s.body), Err(Unreadable::Tag));
    }
    // Cut short.
    assert_eq!(
        open_row(&keys, s.key, s.stamp, &s.body[..20]),
        Err(Unreadable::Version)
    );
    // Control: the untouched body opens.
    open_row(&keys, s.key, s.stamp, &s.body).unwrap();
}

#[test]
fn sync_row_filed_under_another_id_or_stamp_is_refused() {
    let keys = key(1).keys();
    let m = meeting(1, "Invented");
    let plaintext = {
        let mut v = serde_json::to_value(earshot_core::identity::sync_envelope(&m)).unwrap();
        v["format"] = 1.into();
        serde_json::to_vec(&v).unwrap()
    };
    // Sealed consistently, but filed under another row's key: the id check catches it.
    let s = seal_plaintext(&keys, keys.row_key(id(2)), m.meta.updated_at, &plaintext).unwrap();
    assert_eq!(
        open_row(&keys, s.key, s.stamp, &s.body),
        Err(Unreadable::Id)
    );
    // Filed at another stamp than its own updated_at: the stamp check catches it.
    let later = stamp(1_800_000_009_000, 0);
    let s = seal_plaintext(&keys, keys.row_key(m.id), later, &plaintext).unwrap();
    assert_eq!(
        open_row(&keys, s.key, s.stamp, &s.body),
        Err(Unreadable::Stamp)
    );
    // Control: filed where it belongs, it opens.
    let s = seal_plaintext(&keys, keys.row_key(m.id), m.meta.updated_at, &plaintext).unwrap();
    opened(open_row(&keys, s.key, s.stamp, &s.body).unwrap());
}

#[test]
fn sync_newer_format_is_kept_not_refused_and_bad_json_is_unreadable() {
    let keys = key(1).keys();
    let k = keys.row_key(id(1));
    let at = stamp(1_800_000_000_500, 2);
    // A newer Earshot may change the envelope's shape: only the format is read.
    let s = seal_plaintext(&keys, k, at, br#"{"format":2,"anything":["new"]}"#).unwrap();
    assert_eq!(open_row(&keys, k, at, &s.body), Ok(Opened::NewerFormat(2)));
    for bad in [
        &br#"{"format":1}"#[..],
        b"[]",
        b"not json",
        br#"{"format":"1"}"#,
    ] {
        let s = seal_plaintext(&keys, k, at, bad).unwrap();
        assert_eq!(
            open_row(&keys, k, at, &s.body),
            Err(Unreadable::Json),
            "{bad:?}"
        );
    }
}

#[test]
fn sync_row_keys_and_key_checks_depend_on_the_vault_and_the_id() {
    let (a, b) = (key(1).keys(), key(2).keys());
    assert_eq!(a.row_key(id(1)), key(1).keys().row_key(id(1)));
    assert_ne!(a.row_key(id(1)), a.row_key(id(2)));
    assert_ne!(a.row_key(id(1)), b.row_key(id(1)));
    assert_eq!(a.key_check(), key(1).keys().key_check());
    assert_ne!(a.key_check(), b.key_check());
    // The row key is not the id: the relay learns no creation time.
    assert_ne!(&a.row_key(id(1)).0, id(1).as_uuid().as_bytes());
    // Known answers: the wire format, computed independently of this crate with Python's
    // standard library (RFC 5869 HKDF-SHA256 with an empty salt, then HMAC-SHA256; the
    // vault key 32 bytes of 0x01, the id `id(1)`). A change here makes every vault unreadable.
    assert_eq!(hex(&a.row_key(id(1)).0), ROW_KEY_PIN);
    assert_eq!(hex(&a.key_check()), KEY_CHECK_PIN);
}

const ROW_KEY_PIN: &str = "91d0cac5483a56b6d884fc127ea3645a";
const KEY_CHECK_PIN: &str = "df1680d315d39ba6cbbda7f847ed496208a7e0bb769486868a6bbd27d67d2ea6";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The associated data and the sealed layout are wire format too: seal and open share the
/// code, so a change of field order, byte order or label would pass every round trip. These
/// bytes are written out by hand from docs/sync-spec.md section 2.3.
#[test]
fn sync_associated_data_and_sealed_layout_are_the_spec_bytes() {
    let key = RowKey([0xAA; 16]);
    let at = HlcTimestamp {
        wall_ms: 0x0102_0304_0506_0708,
        logical: 0x090A_0B0C,
        node: DeviceId::from_bytes([0xBB; 16]),
    };
    let mut want = b"earshot-row-v1".to_vec();
    want.extend([0xAA; 16]);
    want.extend([1, 2, 3, 4, 5, 6, 7, 8]);
    want.extend([9, 10, 11, 12]);
    want.extend([0xBB; 16]);
    assert_eq!(crate::seal::row_aad(&key, &at), want);
    let mut want = b"earshot-presence-v1".to_vec();
    want.extend(DEVICE.as_uuid().as_bytes());
    assert_eq!(crate::seal::presence_aad(DEVICE), want);
    // 0x01 | 24-byte nonce | ciphertext | 16-byte tag.
    let keys = key_of(1);
    let s = seal_presence(&keys, DEVICE, b"abc").unwrap();
    assert_eq!(s[0], 0x01);
    assert_eq!(s.len(), 1 + 24 + 3 + 16);
}

fn key_of(n: u8) -> Keys {
    key(n).keys()
}

#[test]
fn sync_presence_opens_only_for_its_own_device() {
    let keys = key(1).keys();
    let blob = br#"{"v":1,"name":"Desk"}"#;
    let s = seal_presence(&keys, DEVICE, blob).unwrap();
    assert_eq!(open_presence(&keys, DEVICE, &s).unwrap(), blob);
    let other = DeviceId::from_bytes([7; 16]);
    assert_eq!(open_presence(&keys, other, &s), Err(Unreadable::Tag));
    // A presence blob is not a row, even under the same vault.
    assert_eq!(
        open_row(&keys, keys.row_key(id(1)), stamp(0, 0), &s),
        Err(Unreadable::Tag)
    );
}

#[test]
fn sync_audio_and_oversized_plaintext_are_refused_before_sealing() {
    let keys = key(1).keys();
    for title in [
        "fLaC",
        "before OggS after",
        // Printable size bytes, and a real header's, which JSON escapes.
        "RIFF1234WAVE",
        "RIFF\u{24}\u{8}\u{0}\u{0}WAVEfmt ",
        "RIFF\u{ff}\u{ff}\u{ff}\u{7f}WAVE",
        "[102,76,97,67,0,0,0,34]",
        "[82,73,70,70,36,0]",
        "[79,103,103,83]",
        "ZkxhQwAAACI=",
        "UklGRiQAAABXQVZF",
        "T2dnUwACAAAA",
    ] {
        assert_eq!(
            seal_row(&keys, &meeting(1, title)),
            Err(SealError::Refused(PlaintextRefused::AudioSignature)),
            "{title:?}"
        );
    }
    let big = "a".repeat(MAX_PLAINTEXT);
    assert_eq!(
        seal_row(&keys, &meeting(1, &big)),
        Err(SealError::Refused(PlaintextRefused::TooLarge))
    );
    // Words a transcript can hold are not audio: "RIFF" alone, a lower-case "flac", numbers.
    for title in [
        "A RIFF on the old plan",
        "we exported it as flac",
        "82, 73, 70 and 70 are the totals",
    ] {
        seal_row(&keys, &meeting(1, title)).unwrap();
    }
    assert_eq!(check_plaintext(&vec![b'a'; MAX_PLAINTEXT]), Ok(()));
    assert_eq!(
        check_plaintext(&vec![b'a'; MAX_PLAINTEXT + 1]),
        Err(PlaintextRefused::TooLarge)
    );
}

#[test]
fn sync_pairing_code_round_trips_as_typed() {
    for _ in 0..64 {
        let code = PairingCode::generate().unwrap();
        let shown = code.to_string();
        let groups: Vec<&str> = shown.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            [5, 5, 5, 5, 6],
            "{shown}"
        );
        assert!(
            shown
                .chars()
                .all(|c| c == '-' || "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c))
        );
        // Lower case, no dashes, spaces: the same code.
        let typed = shown.to_lowercase().replace('-', " ");
        let back = PairingCode::parse(&typed).unwrap();
        assert_eq!(back.slot(), code.slot());
        assert_eq!(back.to_string(), shown);
    }
    // Crockford's look-alikes: I and L read as 1, O as 0.
    let ones = PairingCode::parse("11111-00000-11111-00000-111110").unwrap();
    let typed = PairingCode::parse("iIlLl-oOoOo-11111-00000-11111o").unwrap();
    assert_eq!(ones.slot(), typed.slot());
    assert_eq!(
        PairingCode::parse("U1111-00000-11111-00000-111110").err(),
        Some(BadCode::Character)
    );
    assert_eq!(
        PairingCode::parse("11111-00000").err(),
        Some(BadCode::Length)
    );
    assert_eq!(
        PairingCode::parse("11111-00000-11111-00000-1111100").err(),
        Some(BadCode::Length)
    );
    // The last character holds 3 bits: one with a spare bit set is a typing mistake.
    assert_eq!(
        PairingCode::parse("11111-00000-11111-00000-111111").err(),
        Some(BadCode::Padding)
    );
}

#[test]
fn sync_pairing_code_wraps_the_vault_key_for_the_joining_device() {
    let vault = VaultKey::generate().unwrap();
    let code = PairingCode::generate().unwrap();
    let wrapped = code.wrap(&vault).unwrap();
    assert!(!wrapped.windows(32).any(|w| w == vault.as_bytes()));
    let joined = PairingCode::parse(&code.to_string())
        .unwrap()
        .unwrap_key(&wrapped)
        .unwrap();
    assert_eq!(joined.as_bytes(), vault.as_bytes());
    assert_eq!(joined.keys().key_check(), vault.keys().key_check());
    // Another code opens nothing.
    let wrong = PairingCode::generate().unwrap();
    assert_eq!(wrong.unwrap_key(&wrapped).err(), Some(Unreadable::Tag));
    // The slot names the code without revealing the wrapping key or the vault key.
    let slot = code.slot();
    assert!(!vault.as_bytes().windows(SLOT_LEN).any(|w| w == slot));
}

#[test]
fn sync_debug_never_shows_key_material() {
    let vault = key(0xAB);
    let code = PairingCode::parse("ABCDE-FGHJK-MNPQR-STVWX-YZ0120").unwrap();
    for shown in [
        format!("{vault:?}"),
        format!("{:?}", vault.keys()),
        format!("{code:?}"),
    ] {
        assert!(
            !shown.contains("ab") && !shown.contains("AB") && !shown.contains("171"),
            "{shown}"
        );
        assert!(shown.ends_with("(..)"), "{shown}");
    }
}
