use earshot_core::identity::{EntityId, NeverSyncClass, SyncClass, SyncMeta, SyncSerialize};

#[derive(serde::Serialize)]
struct AudioFile {
    id: EntityId,
    meta: SyncMeta,
    path: String,
}

impl SyncClass for AudioFile {
    type Class = NeverSyncClass;
}

impl SyncSerialize for AudioFile {
    const ENTITY: &'static str = "audio_file";
    fn sync_id(&self) -> EntityId {
        self.id
    }
    fn sync_meta(&self) -> &SyncMeta {
        &self.meta
    }
}

fn main() {}
