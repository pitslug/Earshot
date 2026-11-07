use earshot_core::identity::{NeverSyncClass, SyncClass, sync_envelope};

#[derive(serde::Serialize)]
struct RawPcm {
    samples: Vec<f32>,
}

impl SyncClass for RawPcm {
    type Class = NeverSyncClass;
}

fn main() {
    let pcm = RawPcm { samples: vec![0.0] };
    let _ = sync_envelope(&pcm);
}
