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
    assert_eq!(verify(&file, &public_key(&SEED), "x64"), Ok(release()));
}

#[test]
fn update_another_key_is_refused() {
    let file = sign(&release(), &SEED).unwrap();
    assert_eq!(
        verify(&file, &public_key(&OTHER_SEED), "x64"),
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
            verify(&bytes, &public_key(&SEED), "x64"),
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
        verify(&bytes, &public_key(&SEED), "x64"),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn update_malformed_files_are_refused_without_reading_them_as_a_release() {
    let key = public_key(&SEED);
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
    let key = public_key(&SEED);
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
    let unknown = serde_json::to_string(&release()).unwrap().replacen(
        '{',
        r#"{"url":"https://elsewhere.example/","#,
        1,
    );
    assert_eq!(
        verify(&signed_text(&unknown, &SEED), &key, "x64"),
        Err(VerifyError::BadManifest("not a release")),
        "a field this version does not know, such as another host"
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
}
