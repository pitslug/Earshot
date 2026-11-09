import { describe, expect, it } from "vitest";

import { type RunsOn, createMockIpc } from "../ipc";
import { automaticLabel } from "./ModelsParts";
import { formatMemory, formatSize, hardwareRows, languageName, partName, placementLine, titleOf, verdictTag } from "./copy";

describe("Models page copy", () => {
  it("formats download sizes in decimal units as the mockup does", () => {
    expect(formatSize(2_512_987_040)).toBe("2.5 GB");
    expect(formatSize(5_680_522_464)).toBe("5.7 GB");
    expect(formatSize(477_410_591)).toBe("477 MB");
    expect(formatSize(0)).toBe("0 bytes");
  });

  it("formats memory in binary units with Windows' labels", () => {
    expect(formatMemory(16 * 2 ** 30)).toBe("16 GB");
    expect(formatMemory(Math.round(15.6 * 2 ** 30))).toBe("15.6 GB");
    expect(formatMemory(512 * 2 ** 20)).toBe("512 MB");
  });

  it("names models by the IPC's titles and parts, and keeps an unknown model's name", async () => {
    const models = await createMockIpc().models();
    expect(titleOf(models, "parakeet-tdt-0.6b-v2")).toBe("Parakeet 0.6B v2");
    expect(titleOf(models, "qwen3.5-9b-q4_k_m")).toBe("Qwen 3.5 9B");
    expect(titleOf(models, "some-new-model")).toBe("some-new-model");
    expect(partName("Cuda")).toBe("graphics card");
    expect(partName("Vulkan")).toBe("graphics card");
    expect(partName("Npu")).toBe("NPU");
    expect(partName("Processor")).toBe("processor");
    expect(languageName("de")).toBe("German");
  });

  it("says where a model runs against the build's best part", () => {
    expect(placementLine("Npu", "Npu")).toBe("Runs on the NPU.");
    expect(placementLine("Processor", "Npu")).toBe("Runs on the processor here (no NPU version).");
    expect(placementLine("Processor", "Cuda")).toBe("Runs on the processor here.");
    expect(placementLine("Processor", "Processor")).toBe("Runs on the processor.");
  });

  it("does not put summaries on any part when this computer does not write them", async () => {
    const ipc = createMockIpc({ machine: "desktop", summaries: { settings: { writer: "NoSummary" } } });
    const [hw, models] = [await ipc.hardware(), await ipc.models()];
    expect(models.in_use.summaries_here).toBe(false);
    const rows = hardwareRows(hw, models);
    expect(rows.some((r) => r.use.includes("Summaries") && r.label !== "Memory")).toBe(false);
    expect(rows.find((r) => r.label === "Memory")?.used).toBe(false);
    expect(rows.find((r) => r.label === "Graphics")?.through).not.toContain("summaries");
    const here = createMockIpc({ machine: "desktop" });
    expect((await here.models()).in_use.summaries_here).toBe(true);
    expect(hardwareRows(await here.hardware(), await here.models()).some((r) => r.use.includes("Summaries"))).toBe(true);
  });

  it("names the libraries on the graphics card from what is in use", async () => {
    const ipc = createMockIpc({ machine: "desktop" });
    const [hw, models] = [await ipc.hardware(), await ipc.models()];
    const through = (transcription: RunsOn, summary: RunsOn) =>
      hardwareRows(hw, {
        ...models,
        in_use: { ...models.in_use, transcription_runs_on: transcription, summary_runs_on: summary },
      }).find((r) => r.label === "Graphics")?.through;
    expect(through("Cuda", "Cuda")).toBe("Transcription through CUDA, summaries through CUDA");
    expect(through("Processor", "Vulkan")).toBe("Summaries through Vulkan");
    expect(through("Vulkan", "Processor")).toBe("Transcription through Vulkan");
    expect(through("Npu", "Cuda")).toBe("Summaries through CUDA");
    expect(through("Processor", "Processor")).toBe("");
  });

  it("tags only a stored verdict", () => {
    expect(verdictTag("KeepsUp")?.text).toBe("Keeps up with live speech on this computer");
    expect(verdictTag("TooSlow")).toEqual({ text: "Too slow for live speech on this computer", color: "warning", line: "You can still use it." });
    expect(verdictTag("Fails")?.text).toBe("Did not work on this computer");
    expect(verdictTag("Unknown")).toBeNull();
  });

  it("says in plain words what Automatic picks from", () => {
    expect(automaticLabel(["processor"])).toBe("Automatic (processor)");
    expect(automaticLabel(["graphics card", "processor"])).toBe("Automatic (graphics card or processor)");
    expect(automaticLabel(["graphics card", "graphics card", "NPU", "processor"])).toBe(
      "Automatic (graphics card, NPU or processor)",
    );
  });
});
