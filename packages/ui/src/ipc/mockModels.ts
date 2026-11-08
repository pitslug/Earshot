// The Models page part of the mock `Ipc`: sample hardware and catalogue for a Surface-like
// machine (NPU) and a desktop-like one (NVIDIA GPU), as the T7.4 mockup shows them, plus
// downloads, removal, settings and the model check played on the given timer.

import type { Timer } from "../time";
import {
  type ChoiceView,
  type HardwareView,
  IpcFailure,
  type ModelItem,
  type ModelJobEvent,
  type ModelSettingsValue,
  type ModelSettingsView,
  type ModelsView,
  type RemovedView,
  type RunsOn,
} from "./ipc";

/** Which sample computer the mock shows. */
export type SampleMachine = "surface" | "desktop";

const GIB = 2 ** 30;
const EN = ["en"];
const PARAKEET_V3 = [
  "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv", "lt", "mt",
  "pl", "pt", "ro", "sk", "sl", "es", "sv", "ru", "uk",
];
const WHISPER = [
  "en", "zh", "de", "es", "ru", "ko", "fr", "ja", "pt", "tr", "pl", "ca", "nl", "ar", "sv", "it",
  "id", "hi", "fi", "vi", "he", "uk", "el", "ms", "cs", "ro", "da", "hu", "ta", "no", "th", "ur",
  "hr", "bg", "lt", "la", "mi", "ml", "cy", "sk", "te", "fa", "lv", "bn", "sr", "az", "sl", "kn",
  "et", "mk", "br", "eu", "is", "hy", "ne", "mn", "bs", "kk", "sq", "sw", "gl", "mr", "pa", "si",
  "km", "sn", "yo", "so", "af", "oc", "ka", "be", "tg", "sd", "gu", "am", "yi", "lo", "uz", "fo",
  "ht", "ps", "tk", "nn", "mt", "sa", "lb", "my", "bo", "tl", "mg", "as", "tt", "haw", "ln",
  "ha", "ba", "jw", "su", "yue",
];

/** The catalogue's transcription models: name, download size (manifest), languages. */
const TRANSCRIPTION: [string, number, string[]][] = [
  ["tiny.en", 77_704_715, EN],
  ["base.en", 147_964_211, EN],
  ["small.en", 487_614_201, EN],
  ["small.en-q5_1", 190_098_681, EN],
  ["large-v3-turbo", 1_624_555_275, WHISPER],
  ["large-v3-turbo-q5_0", 574_041_195, WHISPER],
  ["parakeet-ctc-110m", 458_170_974, EN],
  ["parakeet-tdt-110m", 477_410_591, EN],
  ["parakeet-tdt-0.6b-v2", 2_512_987_040, EN],
  ["parakeet-tdt-0.6b-v3", 2_549_800_429, PARAKEET_V3],
];

/** The shipped local summary models. */
const SUMMARY: [string, number][] = [
  ["qwen3.5-9b-q4_k_m", 5_680_522_464],
  ["qwen3.5-4b-q4_k_m", 2_740_937_888],
];

/** The speech helpers: the catalogue name and the x64 download size (manifest). */
export const MOCK_HELPERS = "speech-helpers";
const HELPERS_SIZE = 121_770_295;

/** The NPU versions an ARM64 build lists (manifest set qnn-models): name, base model, size. */
const NPU_VERSIONS: [string, string, number][] = [
  ["parakeet-tdt-0.6b-v2-npu", "parakeet-tdt-0.6b-v2", 641_079_341],
  ["parakeet-tdt-110m-npu", "parakeet-tdt-110m", 136_729_487],
];

/** The names the app shows (the Rust catalogue's `DISPLAY_NAMES`). */
export const MOCK_DISPLAY_NAMES: Record<string, string> = {
  [MOCK_HELPERS]: "Speech helpers",
  "tiny.en": "Whisper tiny.en",
  "base.en": "Whisper base.en",
  "small.en": "Whisper small.en",
  "small.en-q5_1": "Whisper small.en (compact)",
  "large-v3-turbo": "Whisper large-v3-turbo",
  "large-v3-turbo-q5_0": "Whisper large-v3-turbo (compact)",
  "parakeet-ctc-110m": "Parakeet CTC 110M",
  "parakeet-tdt-110m": "Parakeet 110M",
  "parakeet-tdt-0.6b-v2": "Parakeet 0.6B v2",
  "parakeet-tdt-0.6b-v3": "Parakeet 0.6B v3",
  "qwen3.5-9b-q4_k_m": "Qwen 3.5 9B",
  "qwen3.5-4b-q4_k_m": "Qwen 3.5 4B",
  "parakeet-tdt-0.6b-v2-npu": "Parakeet 0.6B v2 (NPU version)",
  "parakeet-tdt-110m-npu": "Parakeet 110M (NPU version)",
};

const title = (m: string) => MOCK_DISPLAY_NAMES[m] ?? m;

/** The model a meeting falls back to; never removable. */
export const MOCK_FALLBACK_MODEL = "base.en";

/** Models with an NPU version (the NPU ladder). */
const NPU_MODELS = ["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m"];
/** Models the CUDA ladders measured. */
const CUDA_MODELS = ["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m", "base.en", "parakeet-tdt-0.6b-v3", "large-v3-turbo"];

interface Machine {
  hardware: HardwareView;
  /** Where a transcription model runs with "where it runs" on automatic, its NPU version
   * installed (without it a model runs on the processor, as the app's `runs_on_here` says). */
  runsOn(model: string): RunsOn;
  /** Whether the build lists NPU versions (an ARM64 build). */
  npuVersions: boolean;
  /** The graphics part transcription can be forced onto, if any. */
  gpu: RunsOn | null;
  installed: string[];
  /** The stored check's fallback model. */
  fallback: string;
  choices: ChoiceView[];
  recommendedReason: string;
  summary: { model: string; runs_on: RunsOn; reason: string };
}

const MACHINES: Record<SampleMachine, Machine> = {
  surface: {
    hardware: {
      processor: "Snapdragon(R) X2 Elite - X2E88100 - Qualcomm(R) Oryon(TM) CPU",
      physical_cores: 12,
      logical_cores: 12,
      gpus: [
        {
          name: "Qualcomm(R) Adreno(TM) X2-90 GPU",
          dedicated_memory: 512 * 2 ** 20,
          driver_version: "31.0.112.0",
          integrated: true,
        },
      ],
      npus: [{ name: "Qualcomm(R) Hexagon(TM) NPU", driver_version: "30.0.143.0" }],
      total_memory: Math.round(15.6 * GIB),
      accelerators: ["Npu", "Processor"],
    },
    runsOn: (m) => (NPU_MODELS.includes(m) ? "Npu" : "Processor"),
    npuVersions: true,
    gpu: null,
    installed: [
      "parakeet-tdt-0.6b-v2",
      "parakeet-tdt-110m",
      "base.en",
      "qwen3.5-4b-q4_k_m",
      "parakeet-tdt-0.6b-v2-npu",
      "parakeet-tdt-110m-npu",
    ],
    fallback: "parakeet-tdt-110m",
    choices: [
      { role: "Recommended", model: "parakeet-tdt-0.6b-v2" },
      { role: "Lighter", model: "parakeet-tdt-110m" },
      { role: "European", model: "parakeet-tdt-0.6b-v3" },
      { role: "EveryLanguage", model: "large-v3-turbo" },
    ],
    recommendedReason: "the most accurate English model for the NPU",
    summary: {
      model: "qwen3.5-4b-q4_k_m",
      runs_on: "Processor",
      reason: "runs on the processor, where Qwen 3.5 9B is too slow",
    },
  },
  desktop: {
    hardware: {
      processor: "AMD Ryzen 9 9950X3D 16-Core Processor",
      physical_cores: 16,
      logical_cores: 32,
      gpus: [
        {
          name: "NVIDIA GeForce RTX 5080",
          dedicated_memory: 16 * GIB,
          driver_version: "32.0.16.1714",
          integrated: false,
        },
        {
          name: "AMD Radeon(TM) Graphics",
          dedicated_memory: 512 * 2 ** 20,
          driver_version: "32.0.21001.9024",
          integrated: true,
        },
      ],
      npus: [],
      total_memory: Math.round(61.6 * GIB),
      accelerators: ["Cuda", "Processor"],
    },
    runsOn: (m) => (CUDA_MODELS.includes(m) ? "Cuda" : "Processor"),
    npuVersions: false,
    gpu: "Cuda",
    installed: ["parakeet-tdt-0.6b-v2", "base.en", "qwen3.5-9b-q4_k_m"],
    fallback: "base.en",
    choices: [
      { role: "Recommended", model: "parakeet-tdt-0.6b-v2" },
      { role: "Lighter", model: "parakeet-tdt-110m" },
      { role: "European", model: "parakeet-tdt-0.6b-v3" },
      { role: "EveryLanguage", model: "large-v3-turbo" },
    ],
    recommendedReason: "the most accurate English model for the NVIDIA graphics card",
    summary: {
      model: "qwen3.5-9b-q4_k_m",
      runs_on: "Cuda",
      reason: "61.6 GiB of memory; Qwen 3.5 9B needs more than 12 GiB",
    },
  },
};

/** The sample hardware of `machine`. */
export function sampleHardware(machine: SampleMachine = "surface"): HardwareView {
  return structuredClone(MACHINES[machine].hardware);
}

/** The Models page methods of the mock, with their event subscription. */
export interface ModelsMock {
  hardware(): Promise<HardwareView>;
  models(): Promise<ModelsView>;
  modelSettings(): Promise<ModelSettingsView>;
  setModelSettings(settings: ModelSettingsValue): Promise<ModelSettingsView>;
  startDownload(model: string): Promise<void>;
  cancelDownload(model: string): Promise<void>;
  removeModel(model: string): Promise<RemovedView>;
  runCheck(): Promise<void>;
  cancelCheck(): Promise<void>;
  onModelJobEvent(handler: (event: ModelJobEvent) => void): Promise<() => void>;
}

/** Why the check is refused while a model downloads (as `model_jobs::DOWNLOADING_MESSAGE`). */
export const DOWNLOADING_MESSAGE = "A model is still downloading. Run the model check when the downloads have finished.";

/** Why a download or a removal is refused while the check runs (as `model_jobs::CHECK_RUNNING_MESSAGE`). */
export const CHECK_RUNNING_MESSAGE = "The model check is running. Wait for it to finish or cancel it, then try again.";

/** Time between mock download steps (ten steps per download) and check steps. */
export const MODEL_STEP_MS = 200;

const fail = (kind: string, message: string, retryable = false): never => {
  throw new IpcFailure({ kind, message, retryable });
};

/** The Models page of the mock `Ipc` on `machine`, playing jobs on `timer`. With
 * `checkWarning` the check first warns that an accelerator's library could not load. The speech
 * helpers are there unless `cleanInstall` (a machine straight after installing). */
export function createModelsMock(
  machine: SampleMachine,
  timer: Timer,
  checkWarning?: string,
  cleanInstall = false,
): ModelsMock {
  const helpersInstalled = !cleanInstall;
  const m = MACHINES[machine];
  const listeners = new Set<(event: ModelJobEvent) => void>();
  const emit = (e: ModelJobEvent) => {
    for (const l of [...listeners]) l(e);
  };
  const all = new Map<string, ModelItem>();
  for (const [name, size, languages] of TRANSCRIPTION) {
    const on = m.runsOn(name);
    const installed = m.installed.includes(name);
    all.set(name, {
      name,
      title: title(name),
      kind: "Transcription",
      size,
      languages,
      installed: installed ? "Yes" : "No",
      held: installed ? size : 0,
      runs_on: on,
      check: name === "parakeet-tdt-0.6b-v2" ? "KeepsUp" : "Unknown",
      protected: false,
      downloading: false,
    });
  }
  for (const [name, size] of SUMMARY) {
    const installed = m.installed.includes(name);
    all.set(name, {
      name,
      title: title(name),
      kind: "Summary",
      size,
      languages: [],
      installed: installed ? "Yes" : "No",
      held: installed ? size : 0,
      runs_on: m.summary.runs_on,
      check: "Unknown",
      protected: false,
      downloading: false,
    });
  }
  all.set(MOCK_HELPERS, {
    name: MOCK_HELPERS,
    title: title(MOCK_HELPERS),
    kind: "Helpers",
    size: HELPERS_SIZE,
    languages: [],
    installed: helpersInstalled ? "Yes" : "No",
    held: helpersInstalled ? HELPERS_SIZE : 0,
    runs_on: "Processor",
    check: "Unknown",
    protected: true,
    downloading: false,
  });
  for (const [name, , size] of m.npuVersions ? NPU_VERSIONS : []) {
    const installed = m.installed.includes(name);
    all.set(name, {
      name,
      title: title(name),
      kind: "NpuVersion",
      size,
      languages: [...EN],
      installed: installed ? "Yes" : "No",
      held: installed ? size : 0,
      runs_on: "Npu",
      check: "Unknown",
      protected: false,
      downloading: false,
    });
  }
  /** The NPU version of `model`, if this build lists one. */
  const npuVersionOf = (model: string) =>
    NPU_VERSIONS.find(([name, base]) => base === model && all.has(name))?.[0] ?? null;
  /** Where `model` runs here: the NPU only while its NPU version is installed. */
  const runsOnHere = (model: string): RunsOn => {
    const on = m.runsOn(model);
    if (on !== "Npu") return on;
    const v = npuVersionOf(model);
    return v !== null && all.get(v)?.installed === "Yes" ? "Npu" : "Processor";
  };
  let settings: ModelSettingsValue = {
    transcription: null,
    transcription_part: "Automatic",
    summary: null,
    summary_part: "Automatic",
    languages: ["en"],
  };
  let check: ModelsView["check"] = {
    model: "parakeet-tdt-0.6b-v2",
    runs_on: m.runsOn("parakeet-tdt-0.6b-v2"),
    fallback: m.fallback,
    current: true,
  };
  const downloads = new Map<string, () => void>();
  let checking = false;
  let checkTimers: (() => void)[] = [];

  const transcriptionModel = () => settings.transcription ?? check?.model ?? MOCK_FALLBACK_MODEL;
  const summaryModel = () => settings.summary ?? m.summary.model;
  const protectedModels = () => {
    const keep = [MOCK_HELPERS, MOCK_FALLBACK_MODEL, transcriptionModel(), summaryModel(), check?.fallback ?? ""];
    // A kept model's NPU version is kept too (`host::with_npu_versions`).
    const npu = keep.flatMap((k) => npuVersionOf(k) ?? []);
    return new Set([...keep, ...npu]);
  };
  const placement = (): { asr: RunsOn; sum: RunsOn; notes: string[] } => {
    const model = transcriptionModel();
    const notes: string[] = [];
    let asr = runsOnHere(model);
    if (settings.transcription_part === "Automatic" && m.runsOn(model) === "Npu" && asr !== "Npu") {
      notes.push(`${title(model)} has no NPU version installed, so it runs on the processor.`);
    }
    switch (settings.transcription_part) {
      case "Processor":
        asr = "Processor";
        break;
      case "Npu":
        if (!m.hardware.accelerators.includes("Npu")) {
          asr = "Processor";
          notes.push(`This computer has no NPU Earshot can use, so ${title(model)} runs on the processor.`);
        } else if (!NPU_MODELS.includes(model)) {
          asr = "Processor";
          notes.push(`${title(model)} has no NPU version, so it runs on the processor.`);
        } else if (runsOnHere(model) !== "Npu") {
          asr = "Processor";
          notes.push(`${title(model)} has no NPU version installed, so it runs on the processor.`);
        } else asr = "Npu";
        break;
      case "GraphicsCard":
        if (m.gpu === null) {
          asr = "Processor";
          notes.push(
            `This computer has no graphics card Earshot can use, so ${title(model)} runs on the processor.`,
          );
        } else asr = m.gpu;
        break;
      default:
        break;
    }
    let sum: RunsOn = settings.summary_part === "Processor" ? "Processor" : m.summary.runs_on;
    if (settings.summary_part === "GraphicsCard" && m.gpu === null) {
      sum = "Processor";
      notes.push(
        "This version of Earshot cannot run summaries on a graphics card here, so they run on the processor.",
      );
    }
    return { asr, sum, notes };
  };
  const item = (name: string) => all.get(name) ?? fail("unknown_model", `${name} is not a model of this version`);
  const view = (): ModelsView => {
    const keep = protectedModels();
    const { asr, sum, notes } = placement();
    const list = [...all.values()].map((i) => ({
      ...i,
      languages: [...i.languages],
      protected: keep.has(i.name),
      downloading: downloads.has(i.name),
      runs_on: i.kind === "Summary" ? sum : i.kind === "Transcription" ? runsOnHere(i.name) : i.runs_on,
    }));
    const english = settings.languages.every((l) => l === "en");
    const european = settings.languages.every((l) => PARAKEET_V3.includes(l));
    const recommended = english
      ? { model: "parakeet-tdt-0.6b-v2", runs_on: m.runsOn("parakeet-tdt-0.6b-v2"), reason: m.recommendedReason }
      : european
        ? {
            model: "parakeet-tdt-0.6b-v3",
            runs_on: m.runsOn("parakeet-tdt-0.6b-v3"),
            reason: `the most accurate multilingual model for ${m.gpu ? "the NVIDIA graphics card" : "the processor"}`,
          }
        : {
            model: "large-v3-turbo",
            runs_on: m.runsOn("large-v3-turbo"),
            reason: `the most accurate model that transcribes ${settings.languages.join(", ")}`,
          };
    return {
      languages: [...settings.languages],
      recommended,
      choices: m.choices.map((c) => ({ ...c })),
      summary_recommended: { ...m.summary },
      transcription: list.filter((i) => i.kind === "Transcription"),
      summary: list.filter((i) => i.kind === "Summary"),
      helpers: list.find((i) => i.kind === "Helpers") ?? fail("unknown_model", "no speech helpers"),
      npu: list.filter((i) => i.kind === "NpuVersion"),
      check: check ? { ...check } : null,
      in_use: {
        transcription: transcriptionModel(),
        transcription_runs_on: asr,
        transcription_source: settings.transcription ? "chosen in Settings" : "the model check",
        summary: summaryModel(),
        summary_runs_on: sum,
        // `createMockIpc` overrides this from the Summaries page's writer.
        summaries_here: true,
        notes,
      },
    };
  };
  const validate = (s: ModelSettingsValue) => {
    const languages = [...new Set(s.languages.map((l) => l.trim().split(/[-_]/)[0].toLowerCase()).filter((l) => l !== ""))];
    if (languages.length === 0) fail("invalid_setting", "choose at least one meeting language");
    for (const l of languages) {
      if (!WHISPER.includes(l) && !PARAKEET_V3.includes(l)) {
        fail("invalid_setting", `no transcription model understands "${l}"`);
      }
    }
    if (s.transcription !== null) {
      const t = all.get(s.transcription);
      if (!t || t.kind !== "Transcription") {
        fail("invalid_setting", `${s.transcription} is not a transcription model of this version`);
      } else {
        const missing = languages.filter((l) => !t.languages.includes(l));
        if (missing.length > 0) {
          fail("invalid_setting", `${t.title} does not transcribe ${missing.join(", ")}; choose a multilingual model`);
        }
      }
    }
    if (s.summary !== null && all.get(s.summary)?.kind !== "Summary") {
      fail("invalid_setting", `${s.summary} is not a summary model of this version`);
    }
    if (s.summary_part === "Npu") fail("invalid_setting", "summaries cannot run on the NPU");
    return { ...s, languages };
  };

  const startCheck = () => {
    if (checking) fail("busy", "The model check is already running.", true);
    if ([...downloads.keys()].some((d) => all.get(d)?.kind !== "Summary")) fail("busy", DOWNLOADING_MESSAGE, true);
    checking = true;
    const model = transcriptionModel();
    const runs_on = runsOnHere(model);
    checkTimers = [];
    if (checkWarning !== undefined) {
      checkTimers.push(timer.setTimeout(() => emit({ type: "Warning", model: null, message: checkWarning }), 0));
    }
    checkTimers.push(
      timer.setTimeout(() => emit({ type: "CheckProgress", model, runs_on, verdict: null }), MODEL_STEP_MS),
    );
    checkTimers.push(
      timer.setTimeout(() => emit({ type: "CheckProgress", model, runs_on, verdict: "KeepsUp" }), 2 * MODEL_STEP_MS),
    );
    checkTimers.push(timer.setTimeout(() => {
      checking = false;
      check = { model, runs_on, fallback: check?.fallback ?? null, current: true };
      const i = all.get(model);
      if (i) i.check = "KeepsUp";
      emit({ type: "Checked", model, runs_on, trials: 1 });
    }, 3 * MODEL_STEP_MS));
  };

  return {
    hardware: async () => sampleHardware(machine),
    models: async () => view(),
    modelSettings: async () => ({ value: { ...settings, languages: [...settings.languages] }, unknown: [] }),
    setModelSettings: async (s) => {
      settings = validate(s);
      if (check) check = { ...check, current: false };
      return { value: { ...settings, languages: [...settings.languages] }, unknown: [] };
    },
    startDownload: async (model) => {
      const i = item(model);
      if (downloads.has(model)) fail("busy", "This model is already downloading.", true);
      if (checking) fail("busy", CHECK_RUNNING_MESSAGE, true);
      let step = Math.floor((i.held / i.size) * 10);
      emit({ type: "Progress", model, done: i.held, total: i.size });
      const stop = timer.setInterval(() => {
        step += 1;
        i.held = Math.min(i.size, Math.round((i.size * step) / 10));
        emit({ type: "Progress", model, done: i.held, total: i.size });
        if (step >= 10) {
          stop();
          downloads.delete(model);
          i.installed = "Yes";
          if (check) check = { ...check, current: false };
          emit({ type: "Downloaded", model });
          // As the app does: a transcription model's download starts the check, and so do the
          // speech helpers' when a transcription model is there to check.
          const canCheck = [...all.values()].some((o) => o.kind === "Transcription" && o.installed === "Yes");
          if (i.kind === "Transcription" || (i.kind === "Helpers" && canCheck)) {
            try {
              startCheck();
            } catch (e) {
              const why = e instanceof IpcFailure ? e.error.message : String(e);
              emit({ type: "Warning", model, message: `The model check did not start after the download. ${why}` });
            }
          }
        } else i.installed = "Partly";
      }, MODEL_STEP_MS);
      downloads.set(model, stop);
    },
    cancelDownload: async (model) => {
      const stop = downloads.get(model) ?? fail("not_downloading", `${model} is not downloading`);
      stop();
      downloads.delete(model);
      emit({ type: "Cancelled", model });
    },
    removeModel: async (model) => {
      const i = item(model);
      if (checking) fail("busy", CHECK_RUNNING_MESSAGE, true);
      if (downloads.has(model)) fail("busy", "This model is downloading; cancel the download first.", true);
      if (protectedModels().has(model)) {
        fail("model_in_use", `${title(model)} is in use; choose another model first`);
      }
      const freed = i.held;
      i.held = 0;
      i.installed = "No";
      return { bytes_freed: freed, kept_shared: [] };
    },
    runCheck: async () => startCheck(),
    cancelCheck: async () => {
      if (!checking) fail("not_checking", "the model check is not running");
      for (const c of checkTimers) c();
      checking = false;
      emit({ type: "Failed", model: null, message: "The model check was cancelled.", retryable: true });
    },
    onModelJobEvent: async (handler) => {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
  };
}
