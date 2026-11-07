//! Compile-time proof that every `NeverSync` entity of the data model (PLAN.md 3.5, T5.1)
//! is shut out of the sync path, extending the T0.7 check in `identity_sync_class.rs` from
//! stand-in types to the real entities.

#[test]
fn model_never_sync_entities_cannot_be_sync_serialised() {
    let t = trybuild::TestCases::new();
    // The class gate itself: each NeverSync entity fails a `Class: SyncPermitted` bound.
    t.compile_fail("tests/ui/model_never_sync_gate.rs");
    // And none of them can be passed to the sync serialisation entry point.
    t.compile_fail("tests/ui/model_audio_file_envelope.rs");
    t.compile_fail("tests/ui/model_export_record_envelope.rs");
    // Control: every syncable entity passes the same gate and the envelope.
    t.pass("tests/ui/model_syncable_envelope.rs");
}
