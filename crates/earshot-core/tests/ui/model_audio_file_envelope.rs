use earshot_core::identity::sync_envelope;
use earshot_core::model::AudioFile;

fn send(file: &AudioFile) {
    let _ = sync_envelope(file);
}

fn main() {}
