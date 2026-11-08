// The General, Speakers and Obsidian pages of the mock `Ipc` (T7.4h): the rules and words of
// the app (`speaker_settings.rs`, `obsidian.rs`, `general.rs` and
// `earshot_runtime::export_settings`) over settings kept in memory, and a voice sample
// scripted on the injected timer. The Surface sample machine is a laptop (the battery choice
// is offered), the desktop is not.

import { type Timer } from "../time";
import {
  type GeneralView,
  type PreviewsChoice,
  type PreviewsView,
  IpcFailure,
  type ObsidianSettingsValue,
  type ObsidianSettingsView,
  type SpeakerTimingChoice,
  type SpeakerTimingOption,
  type SpeakersView,
  type VoiceEvent,
  type VoiceResult,
} from "./ipc";
import type { SampleMachine } from "./mockModels";

/** How long the voice sample records (ms; `my_voice::SAMPLE_LENGTH`). */
export const VOICE_SAMPLE_MS = 30_000;
/** How often the mock reports recording progress (ms of audio; `PROGRESS_STEP`). */
export const VOICE_STEP_MS = 500;
/** The subfolder used when none is given (`export_settings::DEFAULT_FOLDER`). */
export const DEFAULT_FOLDER = "Meetings";
/** The app version the General page shows in the mock. */
export const MOCK_VERSION = "0.0.0-preview";

/** What the app says about "after the call" (`speaker_timing::AFTER_CALL_WARNING`). */
export const AFTER_CALL_WARNING =
  "After the call: the final transcript and summary take longer after hang-up, but no cores are used for speaker detection during the meeting.";

/** The choices in the app's order with its words (`speaker_settings::description`). */
export const TIMING_OPTIONS: SpeakerTimingOption[] = [
  {
    value: "DuringCall",
    label: "During the call",
    description:
      "Speakers are found while the meeting runs, so the final transcript and summary are ready soon after you hang up.",
  },
  { value: "AfterCall", label: "After the call", description: AFTER_CALL_WARNING },
  {
    value: "AfterCallOnBattery",
    label: "After the call on battery",
    description:
      "During the call when the computer is plugged in, after the call when it runs on battery, to save power.",
  },
];

/** What the Settings mock starts with. */
export interface SettingsMockData {
  /** The global shortcut could not be registered. */
  shortcutError?: string;
  /** The stored speaker detection setting (default during the call). */
  timing?: SpeakerTimingChoice;
  /** Whether the computer has a battery (default: the Surface does, the desktop not). */
  laptop?: boolean;
  /** Your voice is saved at the start, with this much speech (ms). */
  voiceSpeechMs?: number;
  /** How the scripted voice sample ends (default `Saved`). */
  voiceResult?: VoiceResult;
  /** The name the user already gave. */
  myName?: string;
  /** `startVoiceSample` rejects with this (`busy` while a meeting runs, say). */
  voiceStartError?: { kind: string; message: string; retryable: boolean };
  /** The stored vault (default none). */
  vault?: string;
  /** The stored subfolder (default `Meetings`). */
  folder?: string;
  /** Folders that do not exist (every other full path does). */
  missingFolders?: string[];
  /** What the folder dialog answers (default `PICKED_VAULT`; null: cancelled). */
  pickedVault?: string | null;
  /** The folder dialog cannot open (`folder_dialog`). */
  pickVaultFails?: boolean;
  /** Called with the start folder of every folder dialog. */
  onPickVault?: (start: string) => void;
  /** Earshot starts with Windows at the start. */
  startWithWindows?: boolean;
  /** "Start with Windows" cannot be read or changed: this is why. */
  autostartError?: string;
  /** Provisional words on this computer (default: other people only, nothing to say). */
  previews?: PreviewsView;
  /** Where the crash logs are (default `MOCK_CRASH_LOGS`; null: the app has no data folder). */
  crashLogsFolder?: string | null;
}

/** The Settings page methods of the mock, plus what export reads. */
export interface SettingsMock {
  speakers(): Promise<SpeakersView>;
  setSpeakerTiming(timing: SpeakerTimingChoice): Promise<SpeakersView>;
  setMyName(name: string): Promise<SpeakersView>;
  startVoiceSample(): Promise<void>;
  cancelVoiceSample(): Promise<void>;
  forgetMyVoice(): Promise<number>;
  obsidianSettings(): Promise<ObsidianSettingsView>;
  setObsidianSettings(settings: ObsidianSettingsValue): Promise<ObsidianSettingsView>;
  pickObsidianVault(start: string): Promise<string | null>;
  generalSettings(): Promise<GeneralView>;
  setStartWithWindows(on: boolean): Promise<GeneralView>;
  previewsSettings(): Promise<PreviewsView>;
  setPreviews(choice: PreviewsChoice): Promise<PreviewsView>;
  crashLogsFolder(): Promise<string | null>;
  onVoiceEvent(handler: (event: VoiceEvent) => void): Promise<() => void>;
  /** The stored Obsidian settings (export uses them). */
  exportTarget(): ObsidianSettingsValue;
}

/** The folder the mock's folder dialog chooses by default. */
export const PICKED_VAULT = "C:\\Users\\you\\Documents\\Obsidian\\Notes";

/** Where the mock's crash logs are by default. */
export const MOCK_CRASH_LOGS = "C:\\Users\\you\\AppData\\Local\\Earshot\\crash-logs";

const fail = (kind: string, message: string, retryable = false): never => {
  throw new IpcFailure({ kind, message, retryable });
};

/** The page's words for a sample's end (`speaker_settings::outcome_event`). */
export function voiceMessage(result: VoiceResult, speechMs: number): string {
  switch (result) {
    case "Saved":
      return `Your voice is saved (${Math.floor(speechMs / 1000)} seconds of speech). Earshot will name you "Me" when it hears you.`;
    case "NoSpeech":
      return "No speech was heard, so nothing was saved. Check the microphone and try again.";
    case "SeveralVoices":
      return "More than one voice was heard, so nothing was saved. Record again somewhere quiet, speaking alone.";
    case "NotEnoughSpeech":
      return "Only 6 seconds of speech were heard and 10 are needed, so nothing was saved. Read the text aloud without long pauses and try again.";
    case "Uncalibrated":
      return "The speaker model on this computer cannot recognise voices yet, so nothing was saved.";
  }
}

const MAX_PATH_CHARS = 1024;

/** `export_settings::check_folder` and its messages. */
function checkFolder(folder: string): string {
  const t = folder.trim().replace(/^[/\\]+|[/\\]+$/g, "");
  if ([...t].length > MAX_PATH_CHARS) fail("invalid_setting", `The path is longer than ${MAX_PATH_CHARS} characters.`);
  if (t === "") return "";
  const bad = (why: string): never =>
    fail("invalid_setting", `The subfolder must be a folder inside the vault (${why}).`);
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f-\u009f]/.test(t)) bad("control character");
  const norm = t.replace(/\\/g, "/");
  if (norm.startsWith("/")) bad("absolute");
  if (norm.includes(":")) bad("drive or stream prefix");
  for (const c of norm.split("/")) {
    if (c === "") bad("empty component");
    if (c === "." || c === "..") bad("dot component");
  }
  return norm;
}

/** The Settings part of the mock `Ipc`. */
export function createSettingsMock(
  data: SettingsMockData = {},
  machine: SampleMachine,
  timer: Timer,
): SettingsMock {
  const laptop = data.laptop ?? machine === "surface";
  let timing: SpeakerTimingChoice = data.timing ?? "DuringCall";
  let speechMs = data.voiceSpeechMs ?? 0;
  let target: ObsidianSettingsValue = { vault: data.vault ?? "", folder: data.folder ?? DEFAULT_FOLDER };
  let autostart = data.startWithWindows ?? false;
  let previews: PreviewsView = data.previews ?? { choice: "other_people", can_change: true, note: null, unknown: null };
  const listeners = new Set<(event: VoiceEvent) => void>();
  const emit = (event: VoiceEvent) => {
    for (const l of [...listeners]) l(event);
  };
  // The running sample's timer, cancelled by `cancelVoiceSample`.
  let sample: (() => void) | null = null;

    let myName = data.myName ?? "";
const voice = () => ({ saved: speechMs > 0, speech_ms: speechMs, sample_ms: VOICE_SAMPLE_MS, name: myName });
  const speakers = (): SpeakersView => ({
    timing,
    options: TIMING_OPTIONS.filter((o) => laptop || o.value !== "AfterCallOnBattery" || o.value === timing).map(
      (o) => ({ ...o }),
    ),
    unknown: [],
    laptop,
    voice: voice(),
  });
  const general = (): GeneralView => ({
    start_with_windows: data.autostartError === undefined ? autostart : null,
    start_error: data.autostartError ?? null,
    version: MOCK_VERSION,
    shortcut: "Ctrl+Alt+Shift+R",
    shortcut_error: data.shortcutError ?? null,
  });
  const obsidian = (): ObsidianSettingsView => ({
    value: { ...target },
    unknown: [],
    default_folder: DEFAULT_FOLDER,
  });

  return {
    speakers: async () => speakers(),
    setSpeakerTiming: async (t) => {
      if (t === "AfterCallOnBattery" && !laptop) {
        fail("invalid_setting", "This computer has no battery, so this choice would work as during the call.");
      }
      timing = t;
      return speakers();
    },
    setMyName: async (name) => {
      const trimmed = name.trim();
      if (trimmed.length > 100) fail("invalid_name", "That name is too long.");
      myName = trimmed;
      return speakers();
    },
    startVoiceSample: async () => {
      if (data.voiceStartError) throw new IpcFailure(data.voiceStartError);
      if (sample !== null) fail("busy", "Your voice sample is already being recorded.", true);
      let recorded = 0;
      emit({ type: "Recording", recorded_ms: 0, total_ms: VOICE_SAMPLE_MS });
      sample = timer.setInterval(() => {
        recorded += VOICE_STEP_MS;
        if (recorded < VOICE_SAMPLE_MS) {
          emit({ type: "Recording", recorded_ms: recorded, total_ms: VOICE_SAMPLE_MS });
          return;
        }
        sample?.();
        emit({ type: "Recording", recorded_ms: VOICE_SAMPLE_MS, total_ms: VOICE_SAMPLE_MS });
        emit({ type: "Saving" });
        sample = timer.setTimeout(() => {
          sample = null;
          const result = data.voiceResult ?? "Saved";
          const heard = 26_000;
          if (result === "Saved") speechMs += heard;
          emit({ type: "Done", result, message: voiceMessage(result, heard), voice: voice() });
        }, VOICE_STEP_MS);
      }, VOICE_STEP_MS);
    },
    cancelVoiceSample: async () => {
      if (sample === null) return;
      sample();
      sample = null;
      emit({ type: "Cancelled" });
    },
    forgetMyVoice: async () => {
      const n = speechMs > 0 ? 1 : 0;
      speechMs = 0;
      return n;
    },
    obsidianSettings: async () => obsidian(),
    setObsidianSettings: async (s) => {
      const folder = checkFolder(s.folder);
      const vault = s.vault.trim();
      if ([...vault].length > MAX_PATH_CHARS) fail("invalid_setting", `The path is longer than ${MAX_PATH_CHARS} characters.`);
      if (vault !== "") {
        if (!/^([A-Za-z]:[\\/]|\\\\)/.test(vault)) {
          fail("invalid_setting", "The vault must be a full folder path, for example C:\\Users\\you\\Notes.");
        }
        if ((data.missingFolders ?? []).includes(vault)) fail("invalid_setting", "This folder does not exist.");
      }
      target = { vault, folder };
      return obsidian();
    },
    pickObsidianVault: async (start) => {
      data.onPickVault?.(start);
      if (data.pickVaultFails === true) {
        fail("folder_dialog", "The folder window could not open. Type the vault folder instead.", true);
      }
      return data.pickedVault === undefined ? PICKED_VAULT : data.pickedVault;
    },
    generalSettings: async () => general(),
    setStartWithWindows: async (on) => {
      if (data.autostartError !== undefined) fail("autostart", data.autostartError);
      autostart = on;
      return general();
    },
    previewsSettings: async () => ({ ...previews }),
    setPreviews: async (choice) => {
      previews = { ...previews, choice };
      return { ...previews };
    },
    crashLogsFolder: async () => (data.crashLogsFolder === undefined ? MOCK_CRASH_LOGS : data.crashLogsFolder),
    onVoiceEvent: async (handler) => {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
    exportTarget: () => ({ ...target }),
  };
}
