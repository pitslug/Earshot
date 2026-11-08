// Copy and formatting for the Models page (docs/ui-mockups/t7-4.html). Nothing here decides
// where a model runs or which model is used: those come from the IPC (`ModelsView`); this
// file only turns them into words.

import type { ChoiceKind, HardwareView, ModelItem, ModelsView, RunsOn, Verdict } from "../ipc";

/** The name the user sees for model `name`: its `ModelItem.title` from the IPC (the app's
 * one display-name list, `earshot_runtime::catalogue::display_name`); a name the IPC does
 * not list is shown as it is. */
export function titleOf(models: ModelsView, name: string): string {
  return (
    [models.helpers, ...models.transcription, ...models.summary, ...models.npu].find((m) => m.name === name)?.title ??
    name
  );
}

/** Why you would download a model's NPU version (an ARM64 build only). */
export const NPU_WHY =
  "Lets this model run on the NPU, which leaves the processor for your call. Without it, the model runs on the processor.";

/** What the speech helpers are, and that they are not enough on their own. */
export const HELPERS_WHY =
  "Every meeting needs these: voice detection, speaker detection and the ONNX Runtime library they run on. You also need a transcription model, below; which one is your choice.";

/** The three parts of the computer the page talks about. */
export type Part = "processor" | "graphics" | "npu";

/** Which part `on` is. */
export function partOf(on: RunsOn): Part {
  switch (on) {
    case "Cuda":
    case "Vulkan":
      return "graphics";
    case "Npu":
      return "npu";
    default:
      return "processor";
  }
}

const PART_NAME: Record<Part, string> = {
  processor: "processor",
  graphics: "graphics card",
  npu: "NPU",
};

/** "processor", "graphics card" or "NPU". */
export function partName(on: RunsOn): string {
  return PART_NAME[partOf(on)];
}

/** A download size in decimal units, as the mockup shows them ("2.5 GB", "477 MB"). */
export function formatSize(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`;
  if (bytes >= 1e6) return `${Math.round(bytes / 1e6)} MB`;
  if (bytes >= 1e3) return `${Math.round(bytes / 1e3)} KB`;
  return `${bytes} bytes`;
}

/** Memory in binary units with Windows' labels ("16 GB", "15.6 GB", "512 MB"). */
export function formatMemory(bytes: number): string {
  const gib = bytes / 2 ** 30;
  if (gib >= 1) return `${Number(gib.toFixed(1))} GB`;
  return `${Math.round(bytes / 2 ** 20)} MB`;
}

/** The tag on each transcription choice. */
export const ROLE_LABEL: Record<ChoiceKind, string> = {
  Recommended: "Recommended",
  Lighter: "Lighter",
  European: "Multilingual: European",
  EveryLanguage: "Multilingual: every language",
};

/** Where a model would run, said on its card (from `ModelItem.runs_on`; `best` is the
 * build's first accelerator). */
export function placementLine(on: RunsOn, best: RunsOn): string {
  if (on !== "Processor") return `Runs on the ${partName(on)}.`;
  if (best === "Npu") return "Runs on the processor here (no NPU version).";
  if (best !== "Processor") return "Runs on the processor here.";
  return "Runs on the processor.";
}

/** Why you would pick a transcription choice. */
export function choiceWhy(role: ChoiceKind, item: ModelItem, best: RunsOn): string {
  let why: string;
  switch (role) {
    case "Recommended":
      why = "Best accuracy for English meetings.";
      break;
    case "Lighter":
      why =
        partOf(item.runs_on) === "graphics"
          ? "For when the graphics card is busy with other work, or when space is tight. A little less accurate."
          : "For an older or busy computer, or when space is tight. A little less accurate.";
      break;
    case "European": {
      const others = item.languages.filter((l) => l !== "en").length;
      why = item.languages.includes("en")
        ? `English and ${others} other European languages.`
        : `${others} European languages.`;
      break;
    }
    case "EveryLanguage":
      why = `About ${Math.round(item.languages.length / 10) * 10} languages, including Asian languages. Less accurate and slower than Parakeet: use it only for languages Parakeet v3 does not cover.`;
      break;
  }
  return `${why} ${placementLine(item.runs_on, best)}`;
}

/** Why you would pick a summary model. */
export function summaryWhy(item: ModelItem): string {
  if (item.name.startsWith("qwen3.5-4b")) {
    return item.runs_on === "Processor"
      ? "Runs on the processor."
      : "A shorter wait, slightly less thorough.";
  }
  if (item.name.startsWith("qwen3.5-9b")) return "Slower. More thorough summaries.";
  return "";
}

/** The tag on a summary model's card. */
export function summaryLabel(item: ModelItem, recommended: ModelItem | undefined): string {
  if (recommended === undefined || item.name === recommended.name) return "Recommended";
  return item.size > recommended.size ? "More thorough" : "Faster";
}

/** A stored check's verdict as a tag (nothing for `Unknown`). */
export function verdictTag(
  v: Verdict,
): { text: string; color: "success" | "warning" | "danger"; line?: string } | null {
  switch (v) {
    case "KeepsUp":
      return { text: "Keeps up", color: "success" };
    case "TooSlow":
      return { text: "Too slow here", color: "warning", line: "You can still use it." };
    case "Fails":
      return { text: "Did not work here", color: "danger" };
    default:
      return null;
  }
}

/** One cell of "This is your hardware". */
export interface HardwareRow {
  label: string;
  /** Exact names (one per adapter), or "None found". */
  names: string[];
  /** What runs there now ("" for no line). */
  use: string;
  /** Whether something runs there. */
  used: boolean;
  /** Which libraries a task on the graphics card runs through ("" for none). */
  through?: string;
}

const BACKEND_NAME: Partial<Record<RunsOn, string>> = { Cuda: "CUDA", Vulkan: "Vulkan" };

/** "Transcription through Vulkan, summaries through CUDA", from `in_use`: what runs, not a setting. */
function throughLine(view: ModelsView): string {
  const line = (task: string, on: RunsOn) => {
    const name = BACKEND_NAME[on];
    return name === undefined ? [] : [`${task} through ${name}`];
  };
  const parts = [
    ...line("Transcription", view.in_use.transcription_runs_on),
    ...(view.in_use.summaries_here ? line("summaries", view.in_use.summary_runs_on) : []),
  ].join(", ");
  return parts.charAt(0).toUpperCase() + parts.slice(1);
}

/** "This is your hardware": the facts, and what runs on each part from `in_use`. */
export function hardwareRows(hw: HardwareView, view: ModelsView): HardwareRow[] {
  const uses: Record<Part, string[]> = { processor: ["Speaker detection"], graphics: [], npu: [] };
  uses[partOf(view.in_use.transcription_runs_on)].push("Transcription");
  if (view.in_use.summaries_here) uses[partOf(view.in_use.summary_runs_on)].push("Summaries");
  const line = (p: Part) => (uses[p].length > 0 ? uses[p].join(", ") : "Not used");

  const cores: string[] = [];
  if (hw.physical_cores !== null) cores.push(`${hw.physical_cores} cores`);
  if (hw.logical_cores !== null && hw.logical_cores !== hw.physical_cores) {
    cores.push(`${hw.logical_cores} threads`);
  }
  const processor = [hw.processor ?? "Not known", ...(cores.length > 0 ? [cores.join(", ")] : [])];

  const gpus = hw.gpus.map((g) =>
    g.dedicated_memory !== null && g.dedicated_memory > 0
      ? `${g.name}, ${formatMemory(g.dedicated_memory)} video memory`
      : g.name,
  );
  const npus = hw.npus.map((n) => n.name);
  const sumOnProcessor = view.in_use.summaries_here && partOf(view.in_use.summary_runs_on) === "processor";
  return [
    { label: "Processor", names: processor, use: line("processor"), used: true },
    {
      label: "Graphics",
      names: gpus.length > 0 ? gpus : ["None found"],
      use: gpus.length > 0 || uses.graphics.length > 0 ? line("graphics") : "",
      used: uses.graphics.length > 0,
      through: throughLine(view),
    },
    {
      label: "NPU",
      names: npus.length > 0 ? npus : ["None found"],
      use: npus.length > 0 || uses.npu.length > 0 ? line("npu") : "",
      used: uses.npu.length > 0,
    },
    {
      label: "Memory",
      names: [hw.total_memory !== null ? formatMemory(hw.total_memory) : "Not known"],
      use: sumOnProcessor
        ? "Summaries run on the processor and use it"
        : "Only matters for summaries on the processor",
      used: sumOnProcessor,
    },
  ];
}

/** The language name of `code` in English ("de" -> "German"), or the code. */
export function languageName(code: string): string {
  try {
    return new Intl.DisplayNames(["en"], { type: "language" }).of(code) ?? code;
  } catch {
    return code;
  }
}
