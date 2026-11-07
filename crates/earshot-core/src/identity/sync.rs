//! Sync metadata and compile-time sync classes (PLAN.md 3.5).
//!
//! Every entity type declares exactly one class through [`SyncClass::Class`] (an
//! associated type, so a type cannot be in two classes). The marker traits
//! [`Syncable`], [`SyncableOptIn`] and [`NeverSync`] follow from it.
//! [`SyncSerialize`] requires a class that is [`SyncPermitted`], which `NeverSyncClass`
//! is not and cannot become (the class markers are sealed), so a `NeverSync` type can
//! neither implement it nor be passed to [`sync_envelope`]. This is checked by the
//! trybuild test `tests/identity_sync_class.rs`.

use serde::{Deserialize, Serialize};

use super::clock::Clock;
use super::hlc::{Hlc, HlcTimestamp};
use super::id::{DeviceId, EntityId};

/// Sync metadata carried by every syncable row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncMeta {
    /// When the row was created (HLC).
    pub created_at: HlcTimestamp,
    /// Last change, including deletion (HLC); last-writer-wins compares this.
    pub updated_at: HlcTimestamp,
    /// Tombstone: deleted rows are kept so the deletion can sync.
    pub deleted: bool,
    /// Device that created the row.
    pub origin_device_id: DeviceId,
}

impl SyncMeta {
    /// Metadata for a row created now on `hlc`'s device.
    pub fn new<C: Clock + ?Sized>(hlc: &mut Hlc, clock: &C) -> Self {
        let now = hlc.now(clock);
        SyncMeta {
            created_at: now,
            updated_at: now,
            deleted: false,
            origin_device_id: hlc.node(),
        }
    }

    /// Records a local change.
    pub fn touch<C: Clock + ?Sized>(&mut self, hlc: &mut Hlc, clock: &C) {
        self.updated_at = hlc.now(clock);
    }

    /// Marks the row deleted (a tombstone) and records the change.
    pub fn mark_deleted<C: Clock + ?Sized>(&mut self, hlc: &mut Hlc, clock: &C) {
        self.deleted = true;
        self.touch(hlc, clock);
    }
}

/// Runtime view of a sync class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncClassKind {
    /// Synced whenever a sync server is configured.
    Syncable,
    /// Synced only when the user opts in (voiceprints).
    SyncableOptIn,
    /// Never leaves the device (audio, raw PCM, export records).
    NeverSync,
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::SyncableClass {}
    impl Sealed for super::SyncableOptInClass {}
    impl Sealed for super::NeverSyncClass {}
}

/// A sync class marker type. Sealed: the three classes below are the only ones.
pub trait ClassMarker: sealed::Sealed {
    /// The class as a value.
    const KIND: SyncClassKind;
}

/// Class marker: always syncable.
#[derive(Debug)]
pub enum SyncableClass {}
/// Class marker: syncable only with explicit user opt-in.
#[derive(Debug)]
pub enum SyncableOptInClass {}
/// Class marker: device-local, never serialised for sync.
#[derive(Debug)]
pub enum NeverSyncClass {}

impl ClassMarker for SyncableClass {
    const KIND: SyncClassKind = SyncClassKind::Syncable;
}
impl ClassMarker for SyncableOptInClass {
    const KIND: SyncClassKind = SyncClassKind::SyncableOptIn;
}
impl ClassMarker for NeverSyncClass {
    const KIND: SyncClassKind = SyncClassKind::NeverSync;
}

/// Classes whose rows may be serialised for sync. Implemented for [`SyncableClass`] and
/// [`SyncableOptInClass`] only; sealed, so it cannot be added to [`NeverSyncClass`].
pub trait SyncPermitted: ClassMarker {}
impl SyncPermitted for SyncableClass {}
impl SyncPermitted for SyncableOptInClass {}

/// Declares an entity type's sync class. Every domain entity implements this.
pub trait SyncClass {
    /// One of [`SyncableClass`], [`SyncableOptInClass`], [`NeverSyncClass`].
    type Class: ClassMarker;
}

/// Marker: the type is always syncable.
pub trait Syncable: SyncClass<Class = SyncableClass> {}
impl<T: SyncClass<Class = SyncableClass> + ?Sized> Syncable for T {}

/// Marker: the type is syncable only with user opt-in.
pub trait SyncableOptIn: SyncClass<Class = SyncableOptInClass> {}
impl<T: SyncClass<Class = SyncableOptInClass> + ?Sized> SyncableOptIn for T {}

/// Marker: the type never leaves the device.
pub trait NeverSync: SyncClass<Class = NeverSyncClass> {}
impl<T: SyncClass<Class = NeverSyncClass> + ?Sized> NeverSync for T {}

/// Serialisation for the sync layer. Only types whose class is [`SyncPermitted`] can
/// implement it; the sync layer accepts nothing else.
pub trait SyncSerialize: SyncClass<Class: SyncPermitted> + Serialize {
    /// Stable entity name used on the wire (for example `"meeting"`).
    const ENTITY: &'static str;
    /// The row's id.
    fn sync_id(&self) -> EntityId;
    /// The row's sync metadata.
    fn sync_meta(&self) -> &SyncMeta;
}

/// A row wrapped for the sync layer: entity name, class, id and metadata, plus the row.
#[derive(Debug, Serialize)]
pub struct SyncEnvelope<'a, T: SyncSerialize> {
    /// [`SyncSerialize::ENTITY`].
    pub entity: &'static str,
    /// The row's sync class.
    pub class: SyncClassKind,
    /// The row's id.
    pub id: EntityId,
    /// The row's sync metadata.
    pub meta: &'a SyncMeta,
    /// The row itself.
    pub record: &'a T,
}

/// The single entry point from domain rows to the sync wire format.
pub fn sync_envelope<T: SyncSerialize>(record: &T) -> SyncEnvelope<'_, T> {
    SyncEnvelope {
        entity: T::ENTITY,
        class: <T::Class as ClassMarker>::KIND,
        id: record.sync_id(),
        meta: record.sync_meta(),
        record,
    }
}
