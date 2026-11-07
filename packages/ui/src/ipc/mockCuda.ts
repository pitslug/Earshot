// The CUDA section of the mock `Ipc`, with the rules of `cuda_settings` in the desktop app: the
// desktop-like machine is offered it, the Surface-like one is not. Downloads play on the timer.

import type { Timer } from "../time";
import type { CudaCommands, CudaEvent, CudaPartChoice, CudaPartView, CudaSwitchValue, CudaView, TermsLink } from "./cuda";
import { IpcFailure } from "./ipc";
import type { SampleMachine } from "./mockModels";

/** What a test or the preview sets up. */
export interface CudaMockData {
  /** Why the downloads cannot start yet (default: they can). */
  notAvailable?: string;
  /** Parts already on this computer. */
  installed?: CudaPartChoice[];
  /** The stored settings (default: both off). */
  switch?: CudaSwitchValue;
  /** The settings this run started with (default: `switch`). */
  startedWith?: CudaSwitchValue;
  /** The trial at start ran on CUDA and the folder is loaded. */
  summariesOnCuda?: boolean;
  /** The start is still trying the files, and finishes after this long (then `summariesOnCuda`
   * applies). */
  checkingMs?: number;
  /** A meeting is on. */
  meetingOn?: boolean;
  /** What summaries run on without this part (default: Vulkan). */
  staysOn?: string;
  /** What is in the slot for the measured number. */
  cpuSaved?: string;
  /** Called on a restart. */
  onRestart?: () => void;
  /** Called when a page of terms is opened. */
  onOpenTerms?: (url: string) => void;
}

/** As `cuda_settings::RESTART_DOWNLOADING`. */
const RESTART_DOWNLOADING = "A download is running: restart Earshot when it has finished, or pause it first.";

/** As `cuda_settings::CUDA_CHECKING`. */
const CUDA_CHECKING = "Earshot is still trying these files on your card. Try again when that has finished.";

/** Time between mock download steps (ten steps per download). */
export const CUDA_STEP_MS = 200;

const CUDA_TERMS: TermsLink = {
  terms: "Cuda",
  title: "NVIDIA CUDA EULA",
  url: "https://docs.nvidia.com/cuda/eula/index.html",
};
const CUDNN_TERMS: TermsLink = {
  terms: "Cudnn",
  title: "NVIDIA cuDNN licence",
  url: "https://docs.nvidia.com/deeplearning/cudnn/sla/index.html",
};

const fail = (kind: string, message: string, retryable = false): never => {
  throw new IpcFailure({ kind, message, retryable });
};

/** The CUDA methods of the mock on `machine`, playing downloads on `timer`. */
export function createCudaMock(
  machine: SampleMachine,
  timer: Timer,
  data: CudaMockData = {},
): CudaCommands & { onCudaEvent(handler: (event: CudaEvent) => void): Promise<() => void> } {
  const offered = machine === "desktop";
  const listeners = new Set<(event: CudaEvent) => void>();
  const emit = (e: CudaEvent) => {
    for (const l of [...listeners]) l(e);
  };
  const part = (name: CudaPartChoice, download: number, disk: number, terms: TermsLink[]): CudaPartView => {
    const installed = data.installed?.includes(name) ?? false;
    return {
      part: name,
      not_available: data.notAvailable ?? null,
      download_size: download,
      disk_size: disk,
      held: installed ? download : 0,
      installed: installed ? "Yes" : "No",
      downloading: false,
      on: false,
      removal_refused: null,
      terms,
    };
  };
  const parts: Record<CudaPartChoice, CudaPartView> = {
    Summaries: part("Summaries", 483_700_000, 622_700_000, [CUDA_TERMS]),
    Transcription: part("Transcription", 1_600_000_000, 800_000_000, [CUDA_TERMS, CUDNN_TERMS]),
  };
  let stored: CudaSwitchValue = data.switch ?? { summaries: false, transcription: false };
  const startedWith = data.startedWith ?? stored;
  const downloads = new Map<CudaPartChoice, () => void>();
  let checking = data.checkingMs !== undefined;
  if (data.checkingMs !== undefined) {
    timer.setTimeout(() => {
      checking = false;
      emit({ type: "StartChecked" });
    }, data.checkingMs);
  }

  const refusal = (name: CudaPartChoice): string | null => {
    if (checking) return CUDA_CHECKING;
    if (name === "Summaries" ? stored.summaries : stored.transcription) return "Switch it off first.";
    if (name === "Summaries" && data.summariesOnCuda) {
      return "Earshot is using these files. Restart Earshot first, then remove them.";
    }
    if (name === "Transcription" && startedWith.transcription) {
      return "Earshot is using these files. Restart Earshot first, then remove them.";
    }
    if (name === "Summaries" && parts.Transcription.installed !== "No") {
      return "Transcription on CUDA needs these files. Remove that part first.";
    }
    return null;
  };

  const view = (): CudaView => {
    const shape = (name: CudaPartChoice): CudaPartView => ({
      ...parts[name],
      on: name === "Summaries" ? stored.summaries : stored.transcription,
      downloading: downloads.has(name),
      removal_refused: refusal(name),
    });
    return {
      offered,
      card: offered ? "NVIDIA GeForce RTX 5080" : null,
      summaries: shape("Summaries"),
      transcription: shape("Transcription"),
      switch: { ...stored },
      unknown: [],
      this_run: { summaries_on_cuda: !checking && (data.summariesOnCuda ?? false), checking },
      restart_needed:
        stored.summaries !== startedWith.summaries || stored.transcription !== startedWith.transcription,
      restart_blocked: data.meetingOn
        ? "A meeting is on: restart Earshot when it has finished."
        : downloads.size > 0
          ? RESTART_DOWNLOADING
          : null,
      summaries_stay_on: data.staysOn ?? "Vulkan",
      transcription_cpu_saved: data.cpuSaved ?? null,
    };
  };

  return {
    cudaSettings: async () => view(),
    setCudaSettings: async (value) => {
      if (value.transcription && !value.summaries) {
        fail("invalid_setting", "Transcription on CUDA needs summaries on CUDA switched on.");
      }
      const wanted: [CudaPartChoice, boolean, boolean][] = [
        ["Summaries", value.summaries, stored.summaries],
        ["Transcription", value.transcription, stored.transcription],
      ];
      for (const [name, on, was] of wanted) {
        if (on && !was) {
          if (!offered) fail("invalid_setting", "This computer's graphics card cannot use the CUDA libraries.");
          if (parts[name].installed !== "Yes") fail("invalid_setting", "Download this part first, then switch it on.");
        }
      }
      stored = { ...value };
      return { ...stored };
    },
    startCudaDownload: async (name, acceptedTerms) => {
      if (!acceptedTerms) fail("terms_not_accepted", "Accept NVIDIA's terms before this download.");
      if (!offered) fail("not_offered", "This computer's graphics card cannot use the CUDA libraries.");
      if (data.notAvailable !== undefined) fail("not_available", data.notAvailable);
      if (name === "Transcription" && parts.Summaries.installed !== "Yes") {
        fail("needs_summaries", "Download summaries on CUDA first: this part needs its files.");
      }
      if (downloads.has(name)) fail("busy", "This part is already downloading.", true);
      const p = parts[name];
      let step = Math.floor((p.held / p.download_size) * 10);
      const stop = timer.setInterval(() => {
        step += 1;
        p.held = Math.min(p.download_size, Math.round((p.download_size * step) / 10));
        emit({ type: "Progress", part: name, done: p.held, total: p.download_size });
        if (step >= 10) {
          stop();
          downloads.delete(name);
          p.installed = "Yes";
          emit({ type: "Downloaded", part: name });
        } else p.installed = "Partly";
      }, CUDA_STEP_MS);
      downloads.set(name, stop);
    },
    cancelCudaDownload: async (name) => {
      const stop = downloads.get(name) ?? fail("not_downloading", "This part is not downloading.");
      stop();
      downloads.delete(name);
      emit({ type: "Cancelled", part: name });
    },
    removeCudaPart: async (name) => {
      if (checking) fail("busy", CUDA_CHECKING, true);
      if (downloads.size > 0) {
        fail("busy", "These files are still downloading. Cancel the download or wait for it, then try again.", true);
      }
      const why = refusal(name);
      if (why !== null) fail("part_in_use", why);
      const p = parts[name];
      p.held = 0;
      p.installed = "No";
      return { bytes_freed: p.disk_size, left_in_folder: [] };
    },
    openCudaTerms: async (terms) => {
      data.onOpenTerms?.(terms === "Cuda" ? CUDA_TERMS.url : CUDNN_TERMS.url);
    },
    restartApp: async () => {
      if (downloads.size > 0) fail("busy", RESTART_DOWNLOADING, true);
      if (data.meetingOn) fail("busy", "A meeting or another job is running. Restart Earshot when it has finished.", true);
      data.onRestart?.();
    },
    onCudaEvent: async (handler) => {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
  };
}
