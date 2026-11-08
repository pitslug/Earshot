use earshot_core::identity::{SyncClass, SyncPermitted, SyncSerialize, sync_envelope};
use earshot_core::model::{
    Folder, Meeting, Note, Person, SegmentRow, Speaker, Summary, Track, Voiceprint,
};

fn sync_permitted<T: SyncClass>()
where
    T::Class: SyncPermitted,
{
}

fn send<T: SyncSerialize>(row: &T) {
    let _ = sync_envelope(row);
}

#[allow(dead_code, clippy::too_many_arguments)]
fn all(
    m: &Meeting,
    t: &Track,
    s: &SegmentRow,
    sp: &Speaker,
    n: &Note,
    su: &Summary,
    p: &Person,
    v: &Voiceprint,
    f: &Folder,
) {
    send(m);
    send(t);
    send(s);
    send(sp);
    send(n);
    send(su);
    send(p);
    send(v);
    send(f);
}

fn main() {
    sync_permitted::<Meeting>();
    sync_permitted::<Track>();
    sync_permitted::<SegmentRow>();
    sync_permitted::<Speaker>();
    sync_permitted::<Note>();
    sync_permitted::<Summary>();
    sync_permitted::<Person>();
    sync_permitted::<Voiceprint>();
    sync_permitted::<Folder>();
}
