// The Audio page part of the mock `Ipc` (T7.4i): the settings with the Rust rules
// (`earshot_runtime::audio_settings`) and their messages, and the endpoints each sample
// machine lists. The Surface has its own mic and speakers plus a paired Bluetooth headset
// (listed as a hands-free mic); the desktop has an audio interface, a webcam mic and two
// outputs. Whether a choice is connected is the app's rule
// (`earshot_runtime::audio_settings::device_available`), pinned here by the shared case table
// `fixtures/audio/device_available_cases.json`.

import { type AudioPageView, type AudioSettingsValue, type CaptureMode, type DeviceItem, IpcFailure } from "./ipc";
import type { SampleMachine } from "./mockModels";

/** What the Audio mock starts with. */
export interface AudioMockData {
  /** The stored settings (default: the Windows defaults, a call). */
  settings?: Partial<AudioSettingsValue>;
  /** The microphones listed (default: the sample machine's). */
  microphones?: DeviceItem[];
  /** The outputs listed (default: the sample machine's). */
  outputs?: DeviceItem[];
  /** Listing the endpoints fails with this page text. */
  listError?: string;
}

const device = (id: string, name: string, is_default = false, hands_free = false): DeviceItem => ({
  id,
  name,
  is_default,
  hands_free,
});

/** The endpoints each sample machine lists. */
export const SAMPLE_DEVICES: Record<SampleMachine, { microphones: DeviceItem[]; outputs: DeviceItem[] }> = {
  surface: {
    microphones: [
      device(
        "{0.0.1.00000000}.{5b862d27-476a-4b77-8ac5-a4af31878474}",
        "Microphone Array (Qualcomm(R) Aqstic(TM) Audio Device)",
        true,
      ),
      device(
        "{0.0.1.00000000}.{8f0e7c41-2a11-4b3c-9a65-0d0c6f1e2b77}",
        "Headset (Acme Headset Hands-Free)",
        false,
        true,
      ),
    ],
    outputs: [
      device(
        "{0.0.0.00000000}.{23e79b98-1e59-4d5b-bce3-76fa15461197}",
        "Speakers (Qualcomm(R) Aqstic(TM) Audio Device)",
        true,
      ),
      device("{0.0.0.00000000}.{6c2b9f03-77d4-4e8a-b0f1-3a9d2c5e8f10}", "Headphones (Acme Headset)"),
    ],
  },
  desktop: {
    microphones: [
      device("{0.0.1.00000000}.{a1f3c9d2-6b4e-4f80-9c1a-2e7d5b3f8a01}", "Line 1 + 2 (Acme USB Audio)", true),
      device("{0.0.1.00000000}.{c47e2b18-93d5-4a6f-8e02-7b1c4d9f3e52}", "Microphone (Logitech BRIO)"),
    ],
    outputs: [
      device("{0.0.0.00000000}.{e5d8a7c6-1b2f-4d3e-a9c8-6f7e5d4c3b2a}", "Speakers (Acme USB Audio)", true),
      device("{0.0.0.00000000}.{0a9b8c7d-6e5f-4a3b-9c2d-1e0f9a8b7c6d}", "DELL U2723QE (NVIDIA High Definition Audio)"),
    ],
  },
};

/** The Audio page methods of the mock. */
export interface AudioMock {
  audioSettings(): Promise<AudioPageView>;
  setAudioSettings(settings: AudioSettingsValue): Promise<AudioPageView>;
  defaultCaptureMode(): Promise<CaptureMode>;
  /** The devices a meeting would use now (for the audio check mock; not an IPC method). */
  inUse(): { microphone: DeviceItem | undefined; output: DeviceItem | undefined };
}

const fail = (message: string): never => {
  throw new IpcFailure({ kind: "invalid_setting", message, retryable: false });
};

// `earshot_runtime::audio_settings::{MAX_DEVICE_ID, MAX_DEVICE_NAME}`.
const MAX_ID = 512;
const MAX_NAME = 256;
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;

function checked(d: AudioSettingsValue["microphone"]): AudioSettingsValue["microphone"] {
  if (d === null) return null;
  const id = d.id.trim();
  const name = d.name.trim();
  if (id === "" || [...id].length > MAX_ID || id === "default" || CONTROL.test(id)) {
    fail("This device cannot be chosen: its id is not usable.");
  }
  if ([...name].length > MAX_NAME || CONTROL.test(name)) {
    fail(`This device cannot be chosen: its name is longer than ${MAX_NAME} characters or holds a line break.`);
  }
  return { id, name };
}

/** Whether a choice can be recorded now from the endpoints `listed` ([id, is the Windows
 * default]): the default (`chosen` null) when Windows has one, else the endpoint listed by
 * exactly that id. The app's `device_available`, run on the same case table. */
export function deviceAvailable(listed: [string, boolean][], chosen: string | null): boolean {
  return chosen === null ? listed.some(([, isDefault]) => isDefault) : listed.some(([id]) => id === chosen);
}

function has(devices: DeviceItem[], choice: AudioSettingsValue["microphone"]): boolean {
  return deviceAvailable(
    devices.map((d) => [d.id, d.is_default]),
    choice === null ? null : choice.id,
  );
}

/** The Audio page of the mock `Ipc`. */
export function createAudioMock(data: AudioMockData = {}, machine: SampleMachine = "surface"): AudioMock {
  let settings: AudioSettingsValue = {
    microphone: null,
    output: null,
    capture_mode: "Call",
    ...data.settings,
  };
  const microphones = data.microphones ?? SAMPLE_DEVICES[machine].microphones;
  const outputs = data.outputs ?? SAMPLE_DEVICES[machine].outputs;
  const view = (): AudioPageView => {
    const listed = data.listError === undefined;
    return {
      value: {
        ...settings,
        microphone: settings.microphone && { ...settings.microphone },
        output: settings.output && { ...settings.output },
      },
      unknown: [],
      microphones: listed ? microphones.map((d) => ({ ...d })) : [],
      outputs: listed ? outputs.map((d) => ({ ...d })) : [],
      devices_error: listed ? null : (data.listError ?? null),
      microphone_missing: listed && !has(microphones, settings.microphone),
      output_missing: listed && !has(outputs, settings.output),
    };
  };
  const opened = (devices: DeviceItem[], choice: AudioSettingsValue["microphone"]) =>
    (choice === null ? undefined : devices.find((d) => d.id === choice.id)) ?? devices.find((d) => d.is_default);
  return {
    inUse: () => ({
      microphone: data.listError === undefined ? opened(microphones, settings.microphone) : undefined,
      output: data.listError === undefined ? opened(outputs, settings.output) : undefined,
    }),
    audioSettings: async () => view(),
    defaultCaptureMode: async () => settings.capture_mode,
    setAudioSettings: async (s) => {
      settings = { microphone: checked(s.microphone), output: checked(s.output), capture_mode: s.capture_mode };
      return view();
    },
  };
}
