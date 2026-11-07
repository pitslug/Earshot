use earshot_core::identity::{NeverSyncClass, SyncClass, SyncPermitted};

struct RawPcm;

impl SyncClass for RawPcm {
    type Class = NeverSyncClass;
}

fn sync_permitted<T: SyncClass>()
where
    T::Class: SyncPermitted,
{
}

fn main() {
    // The gate itself: a NeverSync type's class is not SyncPermitted (independent of
    // whether anything implements SyncSerialize for it).
    sync_permitted::<RawPcm>();
}
