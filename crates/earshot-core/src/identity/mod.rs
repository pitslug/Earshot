//! Domain identity base (PLAN.md 3.5, decision D10): every entity type is sync-ready.
//!
//! - [`Clock`]: injected wall-clock time (core never reads the system clock).
//! - [`EntityId`]: UUIDv7 built from the injected clock and a caller-supplied RNG.
//! - [`Hlc`] / [`HlcTimestamp`]: hybrid logical clock for `updated_at` ordering.
//! - [`SyncMeta`]: per-row sync metadata (created/updated HLC, tombstone, origin).
//! - [`SyncClass`] with the marker traits [`Syncable`], [`SyncableOptIn`] and
//!   [`NeverSync`], and [`SyncSerialize`], which only syncable classes can implement.

mod clock;
mod hlc;
mod id;
mod sync;

pub use clock::{Clock, ManualClock};
pub use hlc::{Hlc, HlcTimestamp};
pub use id::{DeviceId, EntityId};
pub use sync::{
    ClassMarker, NeverSync, NeverSyncClass, SyncClass, SyncClassKind, SyncEnvelope, SyncMeta,
    SyncPermitted, SyncSerialize, Syncable, SyncableClass, SyncableOptIn, SyncableOptInClass,
    sync_envelope,
};

#[cfg(test)]
mod tests;
