use earshot_core::identity::sync_envelope;
use earshot_core::model::ExportRecord;

fn send(record: &ExportRecord) {
    let _ = sync_envelope(record);
}

fn main() {}
