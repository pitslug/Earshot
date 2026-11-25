use super::*;

const SEED: [u8; 32] = [7; 32];
const OTHER_SEED: [u8; 32] = [8; 32];

fn release() -> Release {
    Release {
        product: PRODUCT.into(),
        arch: "x64".into(),
        version: "1.2.3".into(),
        installer: "Earshot_1.2.3_x64-setup.exe".into(),
        size: 1000,
        sha256: "a".repeat(64),
    }
}

/// A file signed exactly as `sign` does, around any manifest text, so a test can sign what `sign`
/// itself refuses to.
fn signed_text(manifest: &str, seed: &[u8; 32]) -> Vec<u8> {
    use ed25519_dalek::Signer as _;
    let signature = ed25519_dalek::SigningKey::from_bytes(seed).sign(&signed_bytes(manifest));
    serde_json::to_vec(&SignedFile {
        manifest: manifest.to_owned(),
        signature: to_hex(&signature.to_bytes()),
    })
    .unwrap()
}

#[test]
fn update_a_signed_release_verifies_and_reads_back() {
    let file = sign(&release(), &SEED).unwrap();
    assert_eq!(verify(&file, &[public_key(&SEED)], "x64"), Ok(release()));
}

#[test]
fn update_another_key_is_refused() {
    let file = sign(&release(), &SEED).unwrap();
    assert_eq!(
        verify(&file, &[public_key(&OTHER_SEED)], "x64"),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn update_any_changed_byte_of_the_manifest_is_refused() {
    let file = sign(&release(), &SEED).unwrap();
    let parsed: SignedFile = serde_json::from_slice(&file).unwrap();
    for (from, to) in [("1.2.3", "9.9.9"), ("1000", "1001"), ("aaaa", "aaab")] {
        let changed = SignedFile {
            manifest: parsed.manifest.replacen(from, to, 1),
            signature: parsed.signature.clone(),
        };
        assert_ne!(changed.manifest, parsed.manifest, "{from}");
        let bytes = serde_json::to_vec(&changed).unwrap();
        assert_eq!(
            verify(&bytes, &[public_key(&SEED)], "x64"),
            Err(VerifyError::BadSignature),
            "{from}"
        );
    }
}

#[test]
fn update_a_signature_over_the_manifest_without_the_domain_is_refused() {
    use ed25519_dalek::Signer as _;
    let manifest = serde_json::to_string(&release()).unwrap();
    let signature = ed25519_dalek::SigningKey::from_bytes(&SEED).sign(manifest.as_bytes());
    let bytes = serde_json::to_vec(&SignedFile {
        manifest,
        signature: to_hex(&signature.to_bytes()),
    })
    .unwrap();
    assert_eq!(
        verify(&bytes, &[public_key(&SEED)], "x64"),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn update_malformed_files_are_refused_without_reading_them_as_a_release() {
    let key = [public_key(&SEED)];
    for bytes in [
        &b""[..],
        b"not json",
        b"{}",
        br#"{"manifest":"{}"}"#,
        br#"{"manifest":"{}","signature":"zz"}"#,
        br#"{"manifest":"{}","signature":"00","extra":1}"#,
    ] {
        assert_eq!(
            verify(bytes, &key, "x64"),
            Err(VerifyError::Malformed),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn update_a_signed_file_breaking_a_rule_is_refused() {
    let key = [public_key(&SEED)];
    let with = |change: &dyn Fn(&mut Release)| {
        let mut r = release();
        change(&mut r);
        signed_text(&serde_json::to_string(&r).unwrap(), &SEED)
    };
    let cases: [(&str, Vec<u8>); 8] = [
        ("product", with(&|r| r.product = "other".into())),
        ("arch", with(&|r| r.arch = "arm64".into())),
        ("version", with(&|r| r.version = "1.2.3-rc1".into())),
        (
            "a path as the installer",
            with(&|r| r.installer = r"..\..\evil.exe".into()),
        ),
        (
            "another version's installer",
            with(&|r| r.installer = "Earshot_1.2.4_x64-setup.exe".into()),
        ),
        ("empty", with(&|r| r.size = 0)),
        ("huge", with(&|r| r.size = MAX_INSTALLER_BYTES + 1)),
        ("checksum", with(&|r| r.sha256 = "A".repeat(64))),
    ];
    for (name, bytes) in cases {
        assert!(
            matches!(
                verify(&bytes, &key, "x64"),
                Err(VerifyError::BadManifest(_))
            ),
            "{name}"
        );
    }
    // A field this version does not know does not excuse a known one breaking a rule.
    let mut other_product = release();
    other_product.product = "other".into();
    let unknown = serde_json::to_string(&other_product).unwrap().replacen(
        '{',
        r#"{"url":"https://elsewhere.example/","#,
        1,
    );
    assert!(matches!(
        verify(&signed_text(&unknown, &SEED), &key, "x64"),
        Err(VerifyError::BadManifest(_))
    ));
}

/// The release's JSON with `extra` written in front of its first field.
fn with_extra_field(release: &Release, extra: &str) -> String {
    serde_json::to_string(release)
        .unwrap()
        .replacen('{', &format!("{{{extra},"), 1)
}

#[test]
fn update_a_manifest_field_this_version_does_not_know_is_ignored_and_names_nothing() {
    let key = [public_key(&SEED)];
    // Such as another host: it is signed, but never read, so the release is exactly the known
    // fields and the installer's URL can only follow from them.
    for extra in [
        r#""url":"https://elsewhere.example/evil.exe""#,
        r#""notes":{"nested":[1,2,{"deep":true}]}"#,
        r#""Version":"9.9.9""#,
    ] {
        let manifest = with_extra_field(&release(), extra);
        assert_eq!(
            verify(&signed_text(&manifest, &SEED), &key, "x64"),
            Ok(release()),
            "{extra}"
        );
    }
}

#[test]
fn update_an_unknown_manifest_field_is_still_covered_by_the_signature() {
    let key = [public_key(&SEED)];
    let manifest = with_extra_field(&release(), r#""url":"https://a.example/""#);
    let file = signed_text(&manifest, &SEED);
    let parsed: SignedFile = serde_json::from_slice(&file).unwrap();
    let changed = SignedFile {
        manifest: parsed.manifest.replacen("a.example", "b.example", 1),
        signature: parsed.signature,
    };
    assert_eq!(
        verify(&serde_json::to_vec(&changed).unwrap(), &key, "x64"),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn update_a_known_manifest_field_given_twice_is_refused() {
    let key = [public_key(&SEED)];
    for extra in [r#""version":"9.9.9""#, r#""sha256":"bb""#, r#""size":5"#] {
        let manifest = with_extra_field(&release(), extra);
        assert_eq!(
            verify(&signed_text(&manifest, &SEED), &key, "x64"),
            Err(VerifyError::BadManifest("not a release")),
            "{extra}"
        );
    }
}

#[test]
fn update_a_field_beside_the_manifest_and_signature_is_ignored_and_unsigned() {
    let key = [public_key(&SEED)];
    let file = sign(&release(), &SEED).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&file).unwrap();
    // Such as a second signature, by a key this copy does not know: it is not read.
    value["signature_backup"] = serde_json::Value::String("00".repeat(64));
    let with_extra = serde_json::to_vec(&value).unwrap();
    assert_eq!(verify(&with_extra, &key, "x64"), Ok(release()));
    // The signature that counts is still the one in `signature`.
    let other = sign(&release(), &OTHER_SEED).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&other).unwrap();
    value["signature_backup"] =
        serde_json::from_slice::<serde_json::Value>(&file).unwrap()["signature"].clone();
    assert_eq!(
        verify(&serde_json::to_vec(&value).unwrap(), &key, "x64"),
        Err(VerifyError::BadSignature)
    );
    // A known field given twice is still malformed.
    let twice = String::from_utf8(file)
        .unwrap()
        .replacen('{', r#"{"manifest":"{}","#, 1);
    assert_eq!(
        verify(twice.as_bytes(), &key, "x64"),
        Err(VerifyError::Malformed)
    );
}

/// The reader of copies up to 0.4.2, as it was (`git show v0.4.2:crates/earshot-update/src/lib.rs`):
/// both objects `deny_unknown_fields`, one key, `verify_strict` over the domain and the manifest.
mod reader_0_4_2 {
    use ed25519_dalek::{Signature, VerifyingKey};
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Eq, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Release {
        pub product: String,
        pub arch: String,
        pub version: String,
        pub installer: String,
        pub size: u64,
        pub sha256: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SignedFile {
        manifest: String,
        signature: String,
    }

    /// `None` where 0.4.2 refuses the file.
    pub fn verify(bytes: &[u8], key: &[u8; 32]) -> Option<Release> {
        let file: SignedFile = serde_json::from_slice(bytes).ok()?;
        let signature = super::from_hex::<64>(&file.signature)?;
        let key = VerifyingKey::from_bytes(key).ok()?;
        let message = [b"earshot-update-v1\n".as_slice(), file.manifest.as_bytes()].concat();
        key.verify_strict(&message, &Signature::from_bytes(&signature))
            .ok()?;
        serde_json::from_str(&file.manifest).ok()
    }
}

#[test]
fn update_a_copy_from_0_4_2_reads_what_sign_writes() {
    let file = sign(&release(), &SEED).unwrap();
    let old = reader_0_4_2::verify(&file, &public_key(&SEED)).expect("0.4.2 reads it");
    let r = release();
    assert_eq!(
        old,
        reader_0_4_2::Release {
            product: r.product,
            arch: r.arch,
            version: r.version,
            installer: r.installer,
            size: r.size,
            sha256: r.sha256,
        }
    );
    // The shape is exactly today's: two fields outside, six inside, nothing else.
    let outer: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&file).unwrap();
    let mut keys: Vec<&str> = outer.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["manifest", "signature"]);
    let inner: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(outer["manifest"].as_str().unwrap()).unwrap();
    let mut keys: Vec<&str> = inner.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["arch", "installer", "product", "sha256", "size", "version"]
    );
    // And the domain is still the one 0.4.2 signs over.
    assert_eq!(DOMAIN, b"earshot-update-v1\n");
}

#[test]
fn update_a_copy_from_0_4_2_refuses_a_grown_file_which_is_why_sign_never_writes_one() {
    // The control for the test above: the 0.4.2 reader is strict, so a grown file would strand it.
    let key = public_key(&SEED);
    let grown = signed_text(&with_extra_field(&release(), r#""notes":"x""#), &SEED);
    assert!(reader_0_4_2::verify(&grown, &key).is_none());
    assert_eq!(verify(&grown, &[key], "x64"), Ok(release()));
}

#[test]
fn update_the_release_keys_are_pinned() {
    // Adding the backup key: append its public half to RELEASE_KEYS and its hex here, together.
    let pinned = ["303648c90818b9ff31bc64494e0b00cda75cf46ac7e2c277fbfbaf1cb952ac32"];
    let listed: Vec<String> = RELEASE_KEYS.iter().map(|k| to_hex(k)).collect();
    assert_eq!(listed, pinned);
    assert_eq!(RELEASE_KEYS.first().copied(), RELEASE_PUBLIC_KEY);
    for key in RELEASE_KEYS {
        assert!(usable_key(key).is_some(), "{}", to_hex(key));
    }
    let mut unique = RELEASE_KEYS.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), RELEASE_KEYS.len(), "no key listed twice");
}

#[test]
fn update_a_file_signed_by_a_key_outside_the_built_in_list_is_refused() {
    let file = sign(&release(), &SEED).unwrap();
    assert_eq!(
        verify(&file, RELEASE_KEYS, "x64"),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn update_any_key_of_the_list_may_sign() {
    let first = public_key(&SEED);
    let backup = public_key(&OTHER_SEED);
    for seed in [&SEED, &OTHER_SEED] {
        let file = sign(&release(), seed).unwrap();
        assert_eq!(verify(&file, &[first, backup], "x64"), Ok(release()));
        assert_eq!(verify(&file, &[backup, first], "x64"), Ok(release()));
    }
    let third = sign(&release(), &[9; 32]).unwrap();
    assert_eq!(
        verify(&third, &[first, backup], "x64"),
        Err(VerifyError::BadSignature)
    );
}

/// The encoding of the identity point, a public key of order 1. With the signature `(identity, 0)`
/// the plain Ed25519 equation `[0]B = identity + [k]identity` holds for every message.
fn identity_point() -> [u8; 32] {
    let mut key = [0u8; 32];
    key[0] = 1;
    key
}

#[test]
fn update_an_empty_or_unusable_key_slot_verifies_nothing() {
    let file = sign(&release(), &SEED).unwrap();
    assert_eq!(verify(&file, &[], "x64"), Err(VerifyError::BadSignature));
    // A signature that a weak key would accept without the strict checks.
    let forged = serde_json::to_vec(&SignedFile {
        manifest: serde_json::to_string(&release()).unwrap(),
        signature: to_hex(&[identity_point(), [0; 32]].concat()),
    })
    .unwrap();
    let not_a_point = (0u8..=255)
        .map(|b| [b; 32])
        .find(|k| VerifyingKey::from_bytes(k).is_err())
        .expect("some repeated byte is not a point");
    for slot in [[0u8; 32], identity_point(), not_a_point] {
        assert!(usable_key(&slot).is_none(), "{}", to_hex(&slot));
        for bytes in [&file, &forged] {
            assert_eq!(
                verify(bytes, &[slot], "x64"),
                Err(VerifyError::BadSignature),
                "{}",
                to_hex(&slot)
            );
        }
    }
    // An unusable slot beside a good key takes nothing away from it.
    assert_eq!(
        verify(&file, &[[0; 32], public_key(&SEED)], "x64"),
        Ok(release())
    );
}

#[test]
fn update_sign_refuses_what_verify_would_refuse() {
    let mut bad = release();
    bad.installer = "evil.exe".into();
    assert!(sign(&bad, &SEED).is_err());
}

#[test]
fn update_versions_order_by_number_and_have_one_reading() {
    let v = |s: &str| s.parse::<Version>().unwrap();
    assert!(v("0.10.0") > v("0.9.9"));
    assert!(v("1.0.0") > v("0.99.99"));
    assert!(v("0.1.1") > v("0.1.0"));
    assert_eq!(v("0.1.0"), v("0.1.0"));
    for bad in [
        "",
        "1",
        "1.2",
        "1.2.3.4",
        "v1.2.3",
        "1.2.x",
        "01.2.3",
        "1.2.3-rc1",
        " 1.2.3",
        "+1.2.3",
        "1..3",
        "99999999999.0.0",
    ] {
        assert!(bad.parse::<Version>().is_err(), "{bad:?}");
    }
}

#[test]
fn update_names_follow_the_architecture() {
    assert_eq!(version_file_name("x64"), "earshot-update-x64.json");
    assert_eq!(
        installer_name("0.2.0", "arm64"),
        "Earshot_0.2.0_arm64-setup.exe"
    );
    assert!(["x64", "arm64"].contains(&this_arch()));
}

#[test]
fn update_hex_round_trips_and_refuses_the_wrong_length_or_digits() {
    assert_eq!(from_hex::<2>("00ff"), Some([0, 255]));
    assert_eq!(from_hex::<2>(&to_hex(&[171, 205])), Some([171, 205]));
    assert_eq!(from_hex::<2>("00f"), None);
    assert_eq!(from_hex::<2>("00fg"), None);
    assert_eq!(from_hex::<1>("é"), None);
}

#[test]
fn update_the_built_in_key_is_the_owners_and_usable() {
    let key = RELEASE_PUBLIC_KEY.expect("a release build needs the key");
    assert_eq!(
        to_hex(&key),
        "303648c90818b9ff31bc64494e0b00cda75cf46ac7e2c277fbfbaf1cb952ac32"
    );
    assert!(VerifyingKey::from_bytes(&key).is_ok());
    assert!(usable_key(&key).is_some());
}
