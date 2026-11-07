//! Compile-time proof that a DER collar can only be given through a constructor that names
//! its convention (`DerConfig::nist` per side, `DerConfig::pyannote` total width).

#[test]
fn metrics_der_collar_cannot_be_written_directly() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/der_collar_private.rs");
}
