//! The CUDA switch's rules over fake hardware and a temp store.

use super::*;
use crate::local::DeviceRecord;
use crate::testutil::{TempDir, services};
use earshot_hardware::{ComputeCapability, Gpu, NVIDIA_VENDOR_ID};

fn gpu(vendor_id: u32, cap: Option<(u32, u32)>, driver: &str, integrated: bool) -> Gpu {
    Gpu {
        name: "A card".into(),
        vendor_id,
        dedicated_memory: Some(8 << 30),
        driver_version: Some(driver.into()),
        integrated: Some(integrated),
        compute_capability: cap.map(|(major, minor)| ComputeCapability { major, minor }),
    }
}

fn machine(gpus: Vec<Gpu>) -> Hardware {
    Hardware {
        gpus,
        ..Hardware::default()
    }
}

const NEW: &str = "32.0.16.1714";
const OLD: &str = "32.0.15.5612";

#[test]
fn cuda_switch_the_offer_appears_only_with_a_suitable_gpu() {
    let radeon = gpu(0x1002, None, "32.0.21001.9024", true);
    let refused = [
        ("no card", vec![]),
        ("another vendor", vec![radeon.clone()]),
        (
            "GTX 10 class",
            vec![gpu(NVIDIA_VENDOR_ID, Some((6, 1)), NEW, false)],
        ),
        (
            "old driver",
            vec![gpu(NVIDIA_VENDOR_ID, Some((12, 0)), OLD, false)],
        ),
        (
            "unknown capability",
            vec![gpu(NVIDIA_VENDOR_ID, None, NEW, false)],
        ),
    ];
    for (what, gpus) in refused {
        assert!(!offered_on(&machine(gpus), true), "{what}");
    }
    let accepted = [
        (
            "RTX 20",
            vec![gpu(NVIDIA_VENDOR_ID, Some((7, 5)), NEW, false)],
        ),
        (
            "RTX 50",
            vec![gpu(NVIDIA_VENDOR_ID, Some((12, 0)), NEW, false)],
        ),
        (
            "integrated plus discrete",
            vec![radeon, gpu(NVIDIA_VENDOR_ID, Some((8, 6)), NEW, false)],
        ),
    ];
    for (what, gpus) in accepted {
        let hw = machine(gpus);
        assert!(offered_on(&hw, true), "{what}");
        // The files are x64 builds: a suitable card on ARM64 is not offered them.
        assert!(!offered_on(&hw, false), "{what}");
    }
}

#[test]
fn cuda_switch_is_off_by_default_and_round_trips_in_one_write() {
    let dir = TempDir::new("cuda-switch");
    let (svc, _) = services(&dir);
    let mut store = svc.store();
    assert_eq!(
        cuda_switch(&store).unwrap(),
        (CudaSwitch::default(), vec![])
    );
    let on = CudaSwitch {
        summaries: true,
        transcription: true,
    };
    assert_eq!(set_cuda_switch(&mut store, on).unwrap(), on);
    assert_eq!(cuda_switch(&store).unwrap(), (on, vec![]));
    // Switching back is one setting; the other stays.
    let back = CudaSwitch {
        summaries: true,
        transcription: false,
    };
    set_cuda_switch(&mut store, back).unwrap();
    assert_eq!(cuda_switch(&store).unwrap(), (back, vec![]));
    assert_eq!(
        DeviceRecord::CudaSummaries.key().as_str(),
        CUDA_SUMMARIES_KEY
    );
    assert_eq!(
        DeviceRecord::CudaTranscription.key().as_str(),
        CUDA_TRANSCRIPTION_KEY
    );
}

#[test]
fn cuda_switch_transcription_cannot_be_on_without_summaries() {
    let dir = TempDir::new("cuda-switch");
    let (svc, _) = services(&dir);
    let mut store = svc.store();
    let bad = CudaSwitch {
        summaries: false,
        transcription: true,
    };
    assert!(matches!(
        set_cuda_switch(&mut store, bad),
        Err(CudaSwitchError::NeedsSummaries)
    ));
    // Nothing was stored.
    assert_eq!(store.local_value(CUDA_SUMMARIES_KEY).unwrap(), None);
    assert_eq!(store.local_value(CUDA_TRANSCRIPTION_KEY).unwrap(), None);
    // Stored that way anyway, it reads as off and is reported.
    crate::local::set(
        &mut store,
        &DeviceRecord::CudaTranscription.key(),
        ON.as_bytes(),
    )
    .unwrap();
    let (switch, unknown) = cuda_switch(&store).unwrap();
    assert_eq!(switch, CudaSwitch::default());
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].key, CUDA_TRANSCRIPTION_KEY);
}

#[test]
fn cuda_switch_an_unknown_stored_value_reads_as_off_and_is_reported() {
    let dir = TempDir::new("cuda-switch");
    let (svc, _) = services(&dir);
    let mut store = svc.store();
    crate::local::set(&mut store, &DeviceRecord::CudaSummaries.key(), b"maybe").unwrap();
    let (switch, unknown) = cuda_switch(&store).unwrap();
    assert_eq!(switch, CudaSwitch::default());
    assert_eq!(
        unknown,
        vec![UnknownSetting {
            key: CUDA_SUMMARIES_KEY,
            stored: "maybe".into(),
        }]
    );
}

#[test]
fn cuda_switch_removal_refuses_what_is_on_loaded_or_needed() {
    use CudaPart::{Summaries, Transcription};
    use PartState::{Installed, Missing, Partial};
    let off = CudaSwitch::default();
    let summaries_on = CudaSwitch {
        summaries: true,
        transcription: false,
    };
    assert_eq!(removal_refused(Summaries, off, &[], Missing), None);
    assert_eq!(removal_refused(Transcription, off, &[], Installed), None);
    assert_eq!(
        removal_refused(Summaries, summaries_on, &[], Missing),
        Some(RemovalRefused::SwitchedOn)
    );
    // Switched off, but this run still holds the library.
    assert_eq!(
        removal_refused(Summaries, off, &[Summaries], Missing),
        Some(RemovalRefused::LoadedThisRun)
    );
    // Cuda's other part needs cuBLAS, also while it is half downloaded.
    for state in [Installed, Partial] {
        assert_eq!(
            removal_refused(Summaries, off, &[], state),
            Some(RemovalRefused::NeededByTranscription)
        );
    }
    // The transcription part is free to go with summaries on.
    assert_eq!(
        removal_refused(Transcription, summaries_on, &[Summaries], Installed),
        None
    );
}

#[test]
fn cuda_transcription_fits_only_a_card_with_room_for_a_call() {
    let card = |memory| Hardware {
        gpus: vec![earshot_hardware::Gpu {
            name: "A card".to_owned(),
            vendor_id: 0x10de,
            dedicated_memory: memory,
            driver_version: None,
            integrated: Some(false),
            compute_capability: None,
        }],
        ..Hardware::default()
    };
    assert_eq!(TRANSCRIPTION_MIN_CARD_MEMORY, 12 * 1024 * 1024 * 1024);
    assert!(transcription_fits_card(&card(Some(
        TRANSCRIPTION_MIN_CARD_MEMORY
    ))));
    assert!(!transcription_fits_card(&card(Some(
        TRANSCRIPTION_MIN_CARD_MEMORY - 1
    ))));
    assert!(!transcription_fits_card(&card(Some(
        8 * 1024 * 1024 * 1024
    ))));
    // Unknown is no offer, and no card is no offer.
    assert!(!transcription_fits_card(&card(None)));
    assert!(!transcription_fits_card(&Hardware::default()));
}
