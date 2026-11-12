//! Whether a meeting shows provisional words (T9.1mb, T9.14): `settings/provisional-words`,
//! device-local, read once per meeting start. With nothing stored it is automatic: for the other
//! people where the machine qualifies ([`crate::backends::previews_refused`], the one rule). "On"
//! (the page's "Other people only") lets a machine below the memory limit try them; it cannot
//! override the rest of that rule. "Everyone" does the same and adds the user's own track in a
//! call where [`crate::backends::second_preview_backend`] allows a second session. The words are
//! display only, so this setting never changes what is stored, and a setting never costs a
//! recording.

use earshot_hardware::Hardware;
use earshot_store::Store;

use crate::RuntimeError;
use crate::backends::{
    Accelerator, PREVIEW_MIN_MEMORY, PREVIEW_MIN_MEMORY_NPU, preview_backend, previews_refused,
    second_preview_backend,
};
use crate::local::DeviceRecord;
use earshot_core::asr::BackendKind;

/// The local-state key.
pub const PREVIEWS_KEY: &str = "settings/provisional-words";

/// What is stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PreviewChoice {
    /// Nothing stored: for the other people, where the machine qualifies.
    #[default]
    Automatic,
    /// For the other people, even with less memory than the limit (stored `on`).
    On,
    /// As [`PreviewChoice::On`], and for the user's own track in a call where a second session
    /// fits (stored `everyone`).
    Everyone,
    /// Off.
    Off,
}

/// What the user can choose, and what the page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewSetting {
    /// No provisional words.
    Off,
    /// The people the user is talking to in a call; everyone in the room.
    OtherPeople,
    /// Also the user's own voice in a call.
    Everyone,
}

/// The stored choice, and the stored text when this version does not know it (read as automatic).
///
/// # Errors
/// Store errors.
pub fn preview_choice(store: &Store) -> Result<(PreviewChoice, Option<String>), RuntimeError> {
    let Some(bytes) = store.local_value(PREVIEWS_KEY)? else {
        return Ok((PreviewChoice::Automatic, None));
    };
    Ok(match bytes.as_slice() {
        b"on" => (PreviewChoice::On, None),
        b"everyone" => (PreviewChoice::Everyone, None),
        b"off" => (PreviewChoice::Off, None),
        other => (
            PreviewChoice::Automatic,
            Some(String::from_utf8_lossy(other).into_owned()),
        ),
    })
}

/// Stores the user's choice.
///
/// # Errors
/// Store errors.
pub fn set_previews(store: &mut Store, setting: PreviewSetting) -> Result<(), RuntimeError> {
    let value: &[u8] = match setting {
        PreviewSetting::Off => b"off",
        PreviewSetting::OtherPeople => b"on",
        PreviewSetting::Everyone => b"everyone",
    };
    crate::local::set(store, &DeviceRecord::Previews.key(), value).map_err(RuntimeError::from)
}

/// What the page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewPage {
    /// The choice that reads selected.
    pub shown: PreviewSetting,
    /// Whether the user can change it here.
    pub can_change: bool,
    /// Something to say about this machine.
    pub note: Option<&'static str>,
}

/// Off by default here, and why.
pub const NOTE_DEFAULT_OFF: &str = "Off by default here because this computer has less than 12 GB of memory. You can turn it on, but meetings may run slower.";
/// On, below the limit.
pub const NOTE_ON_BELOW_LIMIT: &str =
    "This computer has less than 12 GB of memory, so meetings may run slower with this on.";
/// No session can run them, whatever the user chooses (no backend today: T12.12c allowed the NPU).
pub const NOTE_NOT_HERE: &str = "Not available on this computer.";
/// Transcription on the graphics card, which has no room for their session: they run on the
/// processor (T12.3c).
pub const NOTE_CARD_PROCESSOR: &str =
    "Your graphics card has too little memory for provisional words, so they run on the processor.";
/// Everyone chosen, and no room for the second session.
pub const NOTE_EVERYONE_MEMORY: &str = "This computer shows them for the other people only: it needs more memory to run them for you too.";
/// Everyone chosen with transcription on the NPU, which runs no session for the user's own voice
/// (T12.12c, `backends::OWN_VOICE_NPU_REFUSED`).
pub const NOTE_EVERYONE_NPU: &str = "On the NPU they show for the other people only for now: some provisional words of your own voice appeared there and then vanished, which is not understood yet.";
/// Everyone chosen with transcription on the graphics card: the user's own words run on the
/// processor (T12.3a).
pub const NOTE_EVERYONE_PROCESSOR: &str = "Provisional words for your own voice run on the processor, so the graphics card keeps its room for the rest of the meeting.";

/// The page for a machine whose transcription would lead with `lead`.
#[must_use]
pub fn preview_page(choice: PreviewChoice, lead: Accelerator, hardware: &Hardware) -> PreviewPage {
    let backend = match lead {
        Accelerator::Qnn => BackendKind::Qnn,
        Accelerator::Cuda => BackendKind::Cuda,
        Accelerator::Cpu | Accelerator::Vulkan => BackendKind::Cpu,
    };
    // Refused even when asked for: nothing a user's choice could change. No backend a lead maps
    // to refuses "on" today (T12.12c allowed the NPU); this stays for one that would.
    let Ok(first) = preview_backend(backend, hardware, PreviewChoice::On) else {
        return PreviewPage {
            shown: PreviewSetting::Off,
            can_change: false,
            note: Some(NOTE_NOT_HERE),
        };
    };
    // T12.3c: a card too small for their session moves them to the processor.
    let card_note = (first != backend).then_some(NOTE_CARD_PROCESSOR);
    // The limit the notes name (12 GB) for where they run: the NPU has its own constant.
    let limit = if first == BackendKind::Qnn {
        PREVIEW_MIN_MEMORY_NPU
    } else {
        PREVIEW_MIN_MEMORY
    };
    let below_limit = hardware.total_memory.is_none_or(|m| m < limit);
    let (shown, note) = match choice {
        PreviewChoice::Off => (PreviewSetting::Off, None),
        PreviewChoice::On => (
            PreviewSetting::OtherPeople,
            if below_limit {
                Some(NOTE_ON_BELOW_LIMIT)
            } else {
                card_note
            },
        ),
        PreviewChoice::Everyone => {
            let note = match second_preview_backend(backend, hardware, choice) {
                Ok(_) if card_note.is_some() => card_note,
                Ok(second) if second != backend => Some(NOTE_EVERYONE_PROCESSOR),
                Ok(_) => below_limit.then_some(NOTE_ON_BELOW_LIMIT),
                // T12.12c: the NPU runs no session for the user's own voice, whatever the memory.
                Err(_) if backend == BackendKind::Qnn => Some(NOTE_EVERYONE_NPU),
                Err(_) => Some(NOTE_EVERYONE_MEMORY),
            };
            (PreviewSetting::Everyone, note)
        }
        PreviewChoice::Automatic => {
            if previews_refused(backend, hardware, choice).is_none() {
                (
                    PreviewSetting::OtherPeople,
                    if below_limit {
                        Some(NOTE_DEFAULT_OFF)
                    } else {
                        card_note
                    },
                )
            } else {
                (PreviewSetting::Off, below_limit.then_some(NOTE_DEFAULT_OFF))
            }
        }
    };
    PreviewPage {
        shown,
        can_change: true,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{
        PREVIEW_BOTH_MIN_MEMORY, PREVIEW_MIN_CARD_MEMORY, PREVIEW_MIN_MEMORY_CUDA,
    };
    use earshot_hardware::Gpu;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn machine(memory: Option<u64>, card: Option<u64>) -> Hardware {
        Hardware {
            total_memory: memory,
            gpus: vec![Gpu {
                name: "A card".to_owned(),
                vendor_id: 0x10de,
                dedicated_memory: card,
                driver_version: None,
                integrated: Some(false),
                compute_capability: None,
            }],
            ..Hardware::default()
        }
    }

    #[test]
    fn preview_limits_are_the_chosen_ones() {
        assert_eq!(PREVIEW_MIN_MEMORY, 12 * GIB);
        // The notes say "12 GB" for the NPU too.
        assert_eq!(PREVIEW_MIN_MEMORY_NPU, 12 * GIB);
        assert_eq!(PREVIEW_MIN_MEMORY_CUDA, 20 * GIB);
        assert_eq!(PREVIEW_MIN_CARD_MEMORY, 14 * GIB);
    }

    #[test]
    fn preview_a_16_gb_machine_on_the_cpu_has_them_by_default() {
        // A 16 GB machine reports less than 16 GiB (AGENTS), and passes.
        let m = machine(Some(15 * GIB), None);
        let page = preview_page(PreviewChoice::Automatic, Accelerator::Cpu, &m);
        assert_eq!(
            page,
            PreviewPage {
                shown: PreviewSetting::OtherPeople,
                can_change: true,
                note: None
            }
        );
    }

    #[test]
    fn preview_below_the_limit_is_off_by_default_and_can_be_turned_on_with_a_note() {
        let m = machine(Some(8 * GIB), None);
        let off = preview_page(PreviewChoice::Automatic, Accelerator::Cpu, &m);
        assert_eq!(
            off,
            PreviewPage {
                shown: PreviewSetting::Off,
                can_change: true,
                note: Some(NOTE_DEFAULT_OFF)
            }
        );
        assert!(previews_refused(BackendKind::Cpu, &m, PreviewChoice::Automatic).is_some());
        let on = preview_page(PreviewChoice::On, Accelerator::Cpu, &m);
        assert_eq!(
            on,
            PreviewPage {
                shown: PreviewSetting::OtherPeople,
                can_change: true,
                note: Some(NOTE_ON_BELOW_LIMIT)
            }
        );
        assert_eq!(
            previews_refused(BackendKind::Cpu, &m, PreviewChoice::On),
            None
        );
    }

    #[test]
    fn preview_off_is_off_everywhere() {
        let big = machine(Some(64 * GIB), Some(16 * GIB));
        for kind in [BackendKind::Cpu, BackendKind::Cuda] {
            assert!(previews_refused(kind, &big, PreviewChoice::Off).is_some());
        }
        assert_eq!(
            preview_page(PreviewChoice::Off, Accelerator::Cpu, &big).shown,
            PreviewSetting::Off
        );
    }

    #[test]
    fn preview_on_cannot_override_a_model_without_them() {
        let big = machine(Some(64 * GIB), Some(16 * GIB));
        assert!(previews_refused(BackendKind::Vulkan, &big, PreviewChoice::On).is_some());
    }

    /// An NPU machine: `memory` of system memory, an NPU, no graphics card.
    fn npu_machine(memory: Option<u64>) -> Hardware {
        Hardware {
            total_memory: memory,
            npus: vec![earshot_hardware::Npu {
                name: "A test NPU".to_owned(),
                driver_version: Some("1.0.0.0".to_owned()),
            }],
            ..Hardware::default()
        }
    }

    /// T12.12c: on the NPU the page is the CPU's: on by default above the limit (a 16 GB Surface
    /// passes), off by default below it with the same notes, and the user can change it.
    #[test]
    fn preview_the_npu_page_has_them_by_default_and_follows_the_memory_limit() {
        let surface = npu_machine(Some(15 * GIB + GIB / 2));
        assert_eq!(
            preview_page(PreviewChoice::Automatic, Accelerator::Qnn, &surface),
            PreviewPage {
                shown: PreviewSetting::OtherPeople,
                can_change: true,
                note: None
            }
        );
        assert_eq!(
            preview_page(PreviewChoice::On, Accelerator::Qnn, &surface),
            PreviewPage {
                shown: PreviewSetting::OtherPeople,
                can_change: true,
                note: None
            }
        );
        assert_eq!(
            preview_page(PreviewChoice::Off, Accelerator::Qnn, &surface),
            PreviewPage {
                shown: PreviewSetting::Off,
                can_change: true,
                note: None
            }
        );
        for memory in [Some(PREVIEW_MIN_MEMORY_NPU - 1), None] {
            let small = npu_machine(memory);
            assert_eq!(
                preview_page(PreviewChoice::Automatic, Accelerator::Qnn, &small),
                PreviewPage {
                    shown: PreviewSetting::Off,
                    can_change: true,
                    note: Some(NOTE_DEFAULT_OFF)
                },
                "{memory:?}"
            );
            assert_eq!(
                preview_page(PreviewChoice::On, Accelerator::Qnn, &small),
                PreviewPage {
                    shown: PreviewSetting::OtherPeople,
                    can_change: true,
                    note: Some(NOTE_ON_BELOW_LIMIT)
                },
                "{memory:?}"
            );
        }
    }

    /// T12.3c: a graphics card too small for a preview session no longer turns them off: they run
    /// on the processor, and the page says so for every choice that shows them.
    #[test]
    fn preview_beside_a_small_card_runs_on_the_processor_and_says_so() {
        for card in [Some(8 * GIB), Some(PREVIEW_MIN_CARD_MEMORY - 1), None] {
            let m = machine(Some(64 * GIB), card);
            assert_eq!(
                previews_refused(BackendKind::Cuda, &m, PreviewChoice::On),
                None
            );
            for choice in [
                PreviewChoice::Automatic,
                PreviewChoice::On,
                PreviewChoice::Everyone,
            ] {
                let page = preview_page(choice, Accelerator::Cuda, &m);
                assert!(page.can_change, "{choice:?} {card:?}");
                assert_eq!(page.note, Some(NOTE_CARD_PROCESSOR), "{choice:?} {card:?}");
                assert_ne!(page.shown, PreviewSetting::Off, "{choice:?} {card:?}");
            }
            assert_eq!(
                preview_page(PreviewChoice::Everyone, Accelerator::Cuda, &m).shown,
                PreviewSetting::Everyone
            );
            assert_eq!(
                preview_page(PreviewChoice::Off, Accelerator::Cuda, &m),
                PreviewPage {
                    shown: PreviewSetting::Off,
                    can_change: true,
                    note: None
                }
            );
        }
        // A card with room keeps them on the card: no note.
        let big = machine(Some(64 * GIB), Some(PREVIEW_MIN_CARD_MEMORY));
        assert_eq!(
            preview_page(PreviewChoice::On, Accelerator::Cuda, &big).note,
            None
        );
    }

    /// T12.3c: beside a small card the CPU's memory limit applies, with the CPU's notes.
    #[test]
    fn preview_beside_a_small_card_keeps_the_cpus_memory_notes() {
        let small_card = Some(8 * GIB);
        let below = machine(Some(PREVIEW_MIN_MEMORY - 1), small_card);
        assert_eq!(
            preview_page(PreviewChoice::Automatic, Accelerator::Cuda, &below),
            PreviewPage {
                shown: PreviewSetting::Off,
                can_change: true,
                note: Some(NOTE_DEFAULT_OFF)
            }
        );
        assert_eq!(
            preview_page(PreviewChoice::On, Accelerator::Cuda, &below),
            PreviewPage {
                shown: PreviewSetting::OtherPeople,
                can_change: true,
                note: Some(NOTE_ON_BELOW_LIMIT)
            }
        );
        let no_room_for_both = machine(Some(PREVIEW_BOTH_MIN_MEMORY - 1), small_card);
        assert_eq!(
            preview_page(
                PreviewChoice::Everyone,
                Accelerator::Cuda,
                &no_room_for_both
            )
            .note,
            Some(NOTE_EVERYONE_MEMORY)
        );
        // At the CPU's limit (below CUDA's) they run by default.
        let at_limit = machine(Some(PREVIEW_MIN_MEMORY), small_card);
        assert_eq!(
            preview_page(PreviewChoice::Automatic, Accelerator::Cuda, &at_limit),
            PreviewPage {
                shown: PreviewSetting::OtherPeople,
                can_change: true,
                note: Some(NOTE_CARD_PROCESSOR)
            }
        );
    }

    #[test]
    fn preview_cuda_keeps_its_higher_memory_limit() {
        let m = machine(Some(16 * GIB), Some(16 * GIB));
        assert!(previews_refused(BackendKind::Cuda, &m, PreviewChoice::Automatic).is_some());
        assert_eq!(
            previews_refused(BackendKind::Cuda, &m, PreviewChoice::On),
            None
        );
        let big = machine(Some(PREVIEW_MIN_MEMORY_CUDA), Some(PREVIEW_MIN_CARD_MEMORY));
        assert_eq!(
            previews_refused(BackendKind::Cuda, &big, PreviewChoice::Automatic),
            None
        );
    }

    #[test]
    fn preview_every_stored_value_reads_as_its_choice() {
        let mut store = Store::open_in_memory().unwrap();
        assert_eq!(
            preview_choice(&store).unwrap(),
            (PreviewChoice::Automatic, None)
        );
        for (setting, stored, read) in [
            (PreviewSetting::Off, &b"off"[..], PreviewChoice::Off),
            (PreviewSetting::OtherPeople, b"on", PreviewChoice::On),
            (
                PreviewSetting::Everyone,
                b"everyone",
                PreviewChoice::Everyone,
            ),
        ] {
            set_previews(&mut store, setting).unwrap();
            assert_eq!(
                store.local_value(PREVIEWS_KEY).unwrap().as_deref(),
                Some(stored)
            );
            assert_eq!(preview_choice(&store).unwrap(), (read, None));
        }
        // An unknown value reads as automatic, with its text reported.
        crate::local::set(&mut store, &DeviceRecord::Previews.key(), b"sometimes").unwrap();
        assert_eq!(
            preview_choice(&store).unwrap(),
            (PreviewChoice::Automatic, Some("sometimes".to_owned()))
        );
    }

    #[test]
    fn preview_an_older_on_reads_as_other_people_only() {
        let m = machine(Some(64 * GIB), None);
        let page = preview_page(PreviewChoice::On, Accelerator::Cpu, &m);
        assert_eq!(page.shown, PreviewSetting::OtherPeople);
        let auto = preview_page(PreviewChoice::Automatic, Accelerator::Cpu, &m);
        assert_eq!(auto.shown, PreviewSetting::OtherPeople);
    }

    #[test]
    fn preview_everyone_on_a_big_cpu_machine_shows_everyone_without_a_note() {
        let m = machine(Some(64 * GIB), None);
        assert_eq!(
            preview_page(PreviewChoice::Everyone, Accelerator::Cpu, &m),
            PreviewPage {
                shown: PreviewSetting::Everyone,
                can_change: true,
                note: None
            }
        );
    }

    #[test]
    fn preview_everyone_without_memory_for_both_says_so() {
        let m = machine(Some(PREVIEW_BOTH_MIN_MEMORY - 1), None);
        assert_eq!(
            preview_page(PreviewChoice::Everyone, Accelerator::Cpu, &m),
            PreviewPage {
                shown: PreviewSetting::Everyone,
                can_change: true,
                note: Some(NOTE_EVERYONE_MEMORY)
            }
        );
        let unknown = machine(None, None);
        assert_eq!(
            preview_page(PreviewChoice::Everyone, Accelerator::Cpu, &unknown).note,
            Some(NOTE_EVERYONE_MEMORY)
        );
    }

    /// T12.3a: beside CUDA the user's own words run on the processor, and the page says so.
    #[test]
    fn preview_everyone_beside_the_graphics_card_says_your_words_run_on_the_processor() {
        let m = machine(Some(64 * GIB), Some(PREVIEW_MIN_CARD_MEMORY));
        assert_eq!(
            preview_page(PreviewChoice::Everyone, Accelerator::Cuda, &m),
            PreviewPage {
                shown: PreviewSetting::Everyone,
                can_change: true,
                note: Some(NOTE_EVERYONE_PROCESSOR)
            }
        );
        // On the CPU nothing moves, so nothing is said.
        assert_eq!(
            preview_page(PreviewChoice::Everyone, Accelerator::Cpu, &m).note,
            None
        );
        // "Other people only" beside CUDA runs no second session: no note.
        assert_eq!(
            preview_page(PreviewChoice::On, Accelerator::Cuda, &m).note,
            None
        );
    }

    /// Beside CUDA the second session needs the CPU's memory limit, as on the CPU.
    #[test]
    fn preview_everyone_beside_the_graphics_card_without_memory_for_both_says_so() {
        let card = Some(PREVIEW_MIN_CARD_MEMORY);
        for m in [
            machine(Some(PREVIEW_BOTH_MIN_MEMORY - 1), card),
            machine(None, card),
        ] {
            assert_eq!(
                preview_page(PreviewChoice::Everyone, Accelerator::Cuda, &m),
                PreviewPage {
                    shown: PreviewSetting::Everyone,
                    can_change: true,
                    note: Some(NOTE_EVERYONE_MEMORY)
                },
                "{:?}",
                m.total_memory
            );
        }
    }

    /// T12.12c: "Everyone" on the NPU shows the other people's words only, and says why, whatever
    /// the memory (never the memory note).
    #[test]
    fn preview_everyone_on_the_npu_shows_the_other_people_only_and_says_why() {
        for memory in [Some(64 * GIB), Some(15 * GIB), Some(8 * GIB), None] {
            let m = npu_machine(memory);
            assert_eq!(
                preview_page(PreviewChoice::Everyone, Accelerator::Qnn, &m),
                PreviewPage {
                    shown: PreviewSetting::Everyone,
                    can_change: true,
                    note: Some(NOTE_EVERYONE_NPU)
                },
                "{memory:?}"
            );
            assert_eq!(
                crate::backends::previews_refused(BackendKind::Qnn, &m, PreviewChoice::Everyone),
                None
            );
        }
        assert!(NOTE_EVERYONE_NPU.contains("other people only"));
        assert!(NOTE_EVERYONE_NPU.contains("vanished"));
    }

    /// No note names a model by its manifest id, and none says "not measured" any more (T12.12c).
    #[test]
    fn preview_notes_name_no_model_and_nothing_unmeasured() {
        for note in [
            NOTE_DEFAULT_OFF,
            NOTE_ON_BELOW_LIMIT,
            NOTE_NOT_HERE,
            NOTE_CARD_PROCESSOR,
            NOTE_EVERYONE_MEMORY,
            NOTE_EVERYONE_NPU,
            NOTE_EVERYONE_PROCESSOR,
        ] {
            assert_eq!(crate::catalogue::manifest_id_in(note), None, "{note}");
            assert!(!note.contains("not measured"), "{note}");
        }
    }

    #[test]
    fn preview_notes_have_no_em_dash() {
        for note in [
            NOTE_DEFAULT_OFF,
            NOTE_ON_BELOW_LIMIT,
            NOTE_NOT_HERE,
            NOTE_CARD_PROCESSOR,
            NOTE_EVERYONE_MEMORY,
            NOTE_EVERYONE_NPU,
            NOTE_EVERYONE_PROCESSOR,
        ] {
            assert!(!note.contains('\u{2014}'), "{note}");
        }
    }

    #[test]
    fn preview_unknown_memory_is_off_by_default() {
        let m = machine(None, None);
        assert!(previews_refused(BackendKind::Cpu, &m, PreviewChoice::Automatic).is_some());
        assert_eq!(
            previews_refused(BackendKind::Cpu, &m, PreviewChoice::On),
            None
        );
    }
}
