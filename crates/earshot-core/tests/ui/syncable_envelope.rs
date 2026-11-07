use earshot_core::identity::{
    DeviceId, EntityId, Hlc, ManualClock, SyncClass, SyncMeta, SyncSerialize, SyncableClass,
    sync_envelope,
};

#[derive(serde::Serialize)]
struct Note {
    id: EntityId,
    meta: SyncMeta,
    text: String,
}

impl SyncClass for Note {
    type Class = SyncableClass;
}

impl SyncSerialize for Note {
    const ENTITY: &'static str = "note";
    fn sync_id(&self) -> EntityId {
        self.id
    }
    fn sync_meta(&self) -> &SyncMeta {
        &self.meta
    }
}

fn main() {
    let clock = ManualClock::new(1);
    let mut hlc = Hlc::new(DeviceId::from_bytes([1; 16]));
    let note = Note {
        id: EntityId::from_bytes([0; 16]),
        meta: SyncMeta::new(&mut hlc, &clock),
        text: String::new(),
    };
    let _ = sync_envelope(&note);
}
