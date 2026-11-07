//! Compile-time proof that `NeverSync` types cannot reach the sync serialisation path
//! (PLAN.md 3.5, T0.7).

#[test]
fn identity_never_sync_types_cannot_be_sync_serialised() {
    let t = trybuild::TestCases::new();
    // The gate: a NeverSync type's class is not `SyncPermitted` (the error names
    // `NeverSyncClass: SyncPermitted`), whatever implements `SyncSerialize`.
    t.compile_fail("tests/ui/never_sync_gate.rs");
    // A NeverSync type cannot be passed to the sync entry point. This fails because it has
    // no `SyncSerialize` impl; the next case shows it cannot get one.
    t.compile_fail("tests/ui/never_sync_envelope.rs");
    // Implementing the sync serialisation trait for a NeverSync type does not compile
    // (`NeverSyncClass: SyncPermitted` unsatisfied).
    t.compile_fail("tests/ui/never_sync_impl.rs");
    // Control: the same shapes compile for a Syncable type.
    t.pass("tests/ui/syncable_envelope.rs");
}
