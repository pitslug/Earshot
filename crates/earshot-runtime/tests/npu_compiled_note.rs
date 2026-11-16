//! The process's `npu-compiled` note when its first refusal was one the helper answered by
//! preparing the copy again (T12.64 review). Its own test binary, because the note state is
//! process-wide and shown once per process: the lib's own note test records an ordinary-load
//! refusal first.

use earshot_runtime::npu_compiled::{
    How, NOTE, NOTE_PREPARED_AGAIN, Refusal, Step, record, take_note,
};

#[test]
fn npu_compiled_a_helper_prepared_again_refusal_shows_its_own_note_once() {
    assert_eq!(take_note(), None);
    // Loads that refused nothing leave no note.
    record(&How::Compiled);
    record(&How::CompiledByHelper { refused: None });
    assert_eq!(take_note(), None);
    // The first refusal: the helper prepared the copy again and it loaded compiled.
    record(&How::CompiledByHelper {
        refused: Some(Refusal {
            step: Step::NpuFit,
            why: "the NPU asks for a new compile".to_owned(),
        }),
    });
    // A later ordinary-load refusal does not replace the first refusal's note.
    record(&How::Ordinary {
        steps: vec![Step::Compile],
        why: "x".to_owned(),
    });
    assert_eq!(take_note(), Some(NOTE_PREPARED_AGAIN));
    assert_ne!(NOTE_PREPARED_AGAIN, NOTE);
    assert_eq!(take_note(), None, "once per process");
}
