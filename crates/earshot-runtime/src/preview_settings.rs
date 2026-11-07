//! Whether a meeting shows provisional words (T9.1mb): `settings/provisional-words`, device-local,
//! read once per meeting start. With nothing stored it is automatic: on where the machine qualifies
//! ([`crate::backends::previews_refused`], the one rule). "On" lets a machine below the memory limit
//! try them; it cannot override the rest of that rule. The words are display only, so this setting
//! never changes what is stored, and a setting never costs a recording.

use earshot_hardware::Hardware;
use earshot_store::Store;

use crate::RuntimeError;
use crate::backends::{Accelerator, PREVIEW_MIN_MEMORY, previews_refused};
use crate::local::DeviceRecord;
use earshot_core::asr::BackendKind;

/// The local-state key.
pub const PREVIEWS_KEY: &str = "settings/provisional-words";

/// What the user chose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PreviewChoice {
    /// Nothing stored: on where the machine qualifies.
    #[default]
    Automatic,
    /// On, even with less memory than the limit.
    On,
    /// Off.
    Off,
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
pub fn set_previews(store: &mut Store, on: bool) -> Result<(), RuntimeError> {
    let value: &[u8] = if on { b"on" } else { b"off" };
    crate::local::set(store, &DeviceRecord::Previews.key(), value).map_err(RuntimeError::from)
}

/// What the page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewPage {
    /// Whether the switch reads on.
    pub on: bool,
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
/// No session can run them: the NPU.
pub const NOTE_NPU: &str = "Not available on this computer's NPU yet.";
/// No session can run them: the card.
pub const NOTE_CARD: &str = "Your graphics card has too little memory for this.";

/// The page for a machine whose transcription would lead with `lead`.
#[must_use]
pub fn preview_page(choice: PreviewChoice, lead: Accelerator, hardware: &Hardware) -> PreviewPage {
    let backend = match lead {
        Accelerator::Qnn => BackendKind::Qnn,
        Accelerator::Cuda => BackendKind::Cuda,
        Accelerator::Cpu | Accelerator::Vulkan => BackendKind::Cpu,
    };
    // Refused even when asked for: nothing a user's "on" could change.
    if previews_refused(backend, hardware, PreviewChoice::On).is_some() {
        let note = if backend == BackendKind::Qnn {
            NOTE_NPU
        } else {
            NOTE_CARD
        };
        return PreviewPage {
            on: false,
            can_change: false,
            note: Some(note),
        };
    }
    let below_limit = hardware.total_memory.is_none_or(|m| m < PREVIEW_MIN_MEMORY);
    let (on, note) = match choice {
        PreviewChoice::Off => (false, None),
        PreviewChoice::On => (true, below_limit.then_some(NOTE_ON_BELOW_LIMIT)),
        PreviewChoice::Automatic => (
            previews_refused(backend, hardware, choice).is_none(),
            below_limit.then_some(NOTE_DEFAULT_OFF),
        ),
    };
    PreviewPage {
        on,
        can_change: true,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{PREVIEW_MIN_CARD_MEMORY, PREVIEW_MIN_MEMORY_CUDA};
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
                on: true,
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
                on: false,
                can_change: true,
                note: Some(NOTE_DEFAULT_OFF)
            }
        );
        assert!(previews_refused(BackendKind::Cpu, &m, PreviewChoice::Automatic).is_some());
        let on = preview_page(PreviewChoice::On, Accelerator::Cpu, &m);
        assert_eq!(
            on,
            PreviewPage {
                on: true,
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
        assert!(!preview_page(PreviewChoice::Off, Accelerator::Cpu, &big).on);
    }

    #[test]
    fn preview_on_cannot_override_the_npu_the_card_or_a_model_without_them() {
        let big = machine(Some(64 * GIB), Some(16 * GIB));
        let small_card = machine(Some(64 * GIB), Some(8 * GIB));
        assert!(previews_refused(BackendKind::Qnn, &big, PreviewChoice::On).is_some());
        assert!(previews_refused(BackendKind::Vulkan, &big, PreviewChoice::On).is_some());
        assert!(previews_refused(BackendKind::Cuda, &small_card, PreviewChoice::On).is_some());
        let npu = preview_page(PreviewChoice::On, Accelerator::Qnn, &big);
        assert_eq!(
            (npu.on, npu.can_change, npu.note),
            (false, false, Some(NOTE_NPU))
        );
        let card = preview_page(PreviewChoice::On, Accelerator::Cuda, &small_card);
        assert_eq!(
            (card.on, card.can_change, card.note),
            (false, false, Some(NOTE_CARD))
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
    fn preview_unknown_memory_is_off_by_default() {
        let m = machine(None, None);
        assert!(previews_refused(BackendKind::Cpu, &m, PreviewChoice::Automatic).is_some());
        assert_eq!(
            previews_refused(BackendKind::Cpu, &m, PreviewChoice::On),
            None
        );
    }
}
