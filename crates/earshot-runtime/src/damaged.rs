//! Damaged items (T14.65, docs/sync-spec.md 17.1 to 17.3).
//!
//! **The scan** ([`scan`]) is the only authority on what is damaged: every row of every synced
//! table that does not read by its table's decoder (`earshot_store::damage_scan`), one page of
//! [`earshot_store::DAMAGE_SCAN_PAGE`] rows per short guarded read, so a recording meeting waits
//! at most one page. It reads only and reports nothing anywhere else, so it cannot start a scan
//! of its own. The host runs every scan through one gate (the desktop's `damage::Scans`).
//!
//! **The record** `damaged-items` ([`DAMAGED_ITEMS_KEY`], `version u32 LE | JSON`): the six
//! counts of the last scan and the six counts showing when the banner was last dismissed. The
//! scan ([`record_scan`]) and the dismiss ([`dismiss`]) each write it in one read-merge-write
//! transaction keeping the other's half; a scan that finds nothing also clears the dismissed
//! counts, so damage found later shows the banner again. An unreadable record counts as nothing
//! found and calls for a scan. The banner reads only this record ([`DamagedItems::shown`]).
//!
//! Counts only: nothing here names a damaged row or a value in it.

use earshot_store::{
    DamageCounts, DamageScan, StoreError, damage_scan, encode_versioned, split_version,
};
use serde::{Deserialize, Serialize};

use crate::local::{self, DeviceRecord};
use crate::services::Services;

/// The device record of damaged items.
pub const DAMAGED_ITEMS_KEY: &str = "damaged-items";

/// The version outside the record's JSON.
pub const DAMAGED_ITEMS_VERSION: u32 = 1;

/// What the record holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamagedItems {
    /// What the last scan found.
    pub found: DamageCounts,
    /// What the last scan had found when the banner was dismissed; `None` when it was not
    /// dismissed since a scan last found nothing.
    #[serde(default)]
    pub dismissed: Option<DamageCounts>,
}

impl DamagedItems {
    /// Whether the banner shows: the last scan found something, and it was not dismissed, or a
    /// scan since found more in any kind than when it was (docs/sync-spec.md 17.1).
    #[must_use]
    pub fn shown(&self) -> bool {
        self.found.total() > 0
            && self
                .dismissed
                .is_none_or(|d| self.found.more_in_any_kind_than(&d))
    }
}

/// The stored record, read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoredItems {
    /// The record; the default (nothing found) when absent or unreadable.
    pub items: DamagedItems,
    /// The record was there and did not read: it counts as nothing found, and a scan is due.
    pub unreadable: bool,
}

/// The record's bytes.
#[must_use]
pub fn encode(items: &DamagedItems) -> Vec<u8> {
    encode_versioned(DAMAGED_ITEMS_VERSION, items).expect("counts serialise")
}

fn decode(bytes: Option<Vec<u8>>) -> StoredItems {
    let Some(bytes) = bytes else {
        return StoredItems::default();
    };
    match split_version(&bytes)
        .filter(|(v, _)| *v == DAMAGED_ITEMS_VERSION)
        .and_then(|(_, payload)| serde_json::from_slice::<DamagedItems>(payload).ok())
    {
        Some(items) => StoredItems {
            items,
            unreadable: false,
        },
        None => StoredItems {
            items: DamagedItems::default(),
            unreadable: true,
        },
    }
}

/// The stored record (no scan).
///
/// # Errors
/// Store errors.
pub fn stored(services: &Services) -> Result<StoredItems, StoreError> {
    let r = services.guarded_read();
    Ok(decode(r.read().local_value(DAMAGED_ITEMS_KEY)?))
}

/// A whole damage scan, one page per short guarded read (module docs). Writes nothing.
///
/// # Errors
/// Store errors other than a row that does not read.
pub fn scan(services: &Services) -> Result<DamageScan, StoreError> {
    let mut scan = DamageScan::default();
    while !damage_scan(&services.guarded_read().read(), &mut scan)? {}
    Ok(scan)
}

/// Stores what a scan found, keeping the dismissed counts (cleared when it found nothing), in
/// one read-merge-write transaction. Returns the record as stored.
///
/// # Errors
/// Store errors.
pub fn record_scan(services: &Services, found: DamageCounts) -> Result<DamagedItems, StoreError> {
    merge(services, |items| {
        items.found = found;
        if found.total() == 0 {
            items.dismissed = None;
        }
    })
}

/// Dismisses the banner: the dismissed counts become what the last scan found, keeping that, in
/// one read-merge-write transaction. Returns the record as stored.
///
/// # Errors
/// Store errors.
pub fn dismiss(services: &Services) -> Result<DamagedItems, StoreError> {
    merge(services, |items| items.dismissed = Some(items.found))
}

fn merge(
    services: &Services,
    change: impl FnOnce(&mut DamagedItems),
) -> Result<DamagedItems, StoreError> {
    let mut db = services.store();
    db.transaction(|tx| -> Result<DamagedItems, StoreError> {
        let mut items = decode(tx.local_value(DAMAGED_ITEMS_KEY)?).items;
        change(&mut items);
        local::set_tx(tx, &DeviceRecord::DamagedItems.key(), &encode(&items))?;
        Ok(items)
    })
}

#[cfg(test)]
pub(crate) fn decode_for_test(bytes: Option<Vec<u8>>) -> StoredItems {
    decode(bytes)
}
