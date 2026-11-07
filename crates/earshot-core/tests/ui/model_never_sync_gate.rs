use earshot_core::identity::{SyncClass, SyncPermitted};
use earshot_core::model::{AudioFile, ExportRecord};

fn sync_permitted<T: SyncClass>()
where
    T::Class: SyncPermitted,
{
}

fn main() {
    sync_permitted::<AudioFile>();
    sync_permitted::<ExportRecord>();
}
