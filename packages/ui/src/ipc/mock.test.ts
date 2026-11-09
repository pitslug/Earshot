import { describe, expect, it } from "vitest";

import { createManualTimer } from "../time";
import { commands } from "./bindings";
import { EVENT_METHODS, IpcFailure, type MeetingEvent, type ModelJobEvent, NEWER_DATA_KIND } from "./ipc";
import { POST_STEPS, POST_STEP_MS, SCRIPT_LINES, createMockIpc, sampleMeetings } from "./mock";
import { CHECK_RUNNING_MESSAGE, DOWNLOADING_MESSAGE, MOCK_FALLBACK_MODEL, MODEL_STEP_MS } from "./mockModels";
import { SAMPLE_FOLDERS } from "./mockFolders";
import { createTauriIpc } from "./tauri";

describe("mock IPC", () => {
  it("implements every command the generated bindings expose and the events, nothing else", () => {
    const generated = Object.keys(commands).sort();
    expect(generated.length).toBeGreaterThan(0);
    const expected = [...generated, ...EVENT_METHODS].sort();
    const mock = createMockIpc() as unknown as Record<string, unknown>;
    expect(Object.keys(mock).sort()).toEqual(expected);
    for (const name of expected) expect(typeof mock[name]).toBe("function");
    // The Tauri implementation covers the same commands and events.
    expect(Object.keys(createTauriIpc()).sort()).toEqual(expected);
  });

  it("returns deterministic sample meetings, newest first", async () => {
    const a = await createMockIpc({ now: 1_800_000_000_000 }).listMeetings();
    const b = await createMockIpc({ now: 1_800_000_000_000 }).listMeetings();
    expect(a).toEqual(b);
    expect(a.length).toBeGreaterThanOrEqual(3);
    expect(a.length).toBeLessThanOrEqual(4);
    const starts = a.map((m) => m.started_at_ms);
    expect([...starts].sort((x, y) => y - x)).toEqual(starts);
    expect(a.every((m) => m.title.length > 0 && m.started_at_ms < 1_800_000_000_000)).toBe(true);
    expect(sampleMeetings()).toEqual(sampleMeetings());
  });

  it("serves the meetings it was given, sorted, as copies", async () => {
    const meetings = [
      { id: "1", title: "Old", started_at_ms: 10, duration_ms: 5, state: "Ready" as const, folder_id: null },
      { id: "2", title: "New", started_at_ms: 20, duration_ms: null, state: "Recording" as const, folder_id: null },
    ];
    const ipc = createMockIpc({ meetings });
    const listed = await ipc.listMeetings();
    expect(listed.map((m) => m.id)).toEqual(["2", "1"]);
    listed[0].title = "changed";
    expect((await ipc.listMeetings())[0].title).toBe("New");
    expect(await createMockIpc({ meetings: [] }).listMeetings()).toEqual([]);
  });

  it("answers app info", async () => {
    const info = await createMockIpc({ appInfo: { version: "1.2.3", data_dir: "D" } }).appInfo();
    expect(info).toEqual({ version: "1.2.3", data_dir: "D" });
  });

  it("plays a deterministic scripted meeting on the given timer", async () => {
    const play = async () => {
      const timer = createManualTimer(1_000_000);
      const ipc = createMockIpc({ meetings: [], timer });
      const events: MeetingEvent[] = [];
      await ipc.onMeetingEvent((e) => events.push(e));
      const id = await ipc.startMeeting("Call", "Pump order");
      expect((await ipc.listMeetings())[0]).toMatchObject({ id, state: "Recording", title: "Pump order" });
      timer.advance(10_000);
      await ipc.stopMeeting();
      expect((await ipc.listMeetings())[0]).toMatchObject({ state: "Processing", duration_ms: 10_000 });
      timer.advance((POST_STEPS.length + 1) * POST_STEP_MS);
      expect((await ipc.listMeetings())[0].state).toBe("Ready");
      return { id, events };
    };
    const { id, events } = await play();
    expect((await play()).events).toEqual(events);
    expect(events[0]).toEqual({ type: "Started", meeting: id, mode: "Call", tracks: ["You", "Them"] });
    const landed = events.filter((e) => e.type === "Landed");
    expect(landed).toHaveLength(SCRIPT_LINES.length);
    expect(events.filter((e) => e.type === "Levels")).toHaveLength(100);
    expect(events.some((e) => e.type === "Warning")).toBe(true);
    const ended = events.findIndex((e) => e.type === "Ended");
    expect(events.slice(ended + 1).every((e) => e.type === "PostProgress" || e.type === "Summary")).toBe(true);
    expect(events.at(-1)).toMatchObject({ type: "Summary", result: { kind: "Stored" } });
  });

  it("plays a room meeting with one track and refuses a second start", async () => {
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ meetings: [], timer });
    const events: MeetingEvent[] = [];
    const stop = await ipc.onMeetingEvent((e) => events.push(e));
    await ipc.startMeeting("InRoom", null);
    await expect(ipc.startMeeting("Call", null)).rejects.toMatchObject({ error: { kind: "busy" } });
    timer.advance(10_000);
    expect(events[0]).toMatchObject({ type: "Started", tracks: ["You"] });
    expect(events.filter((e) => e.type === "Levels").every((e) => e.type === "Levels" && e.them_dbfs === null)).toBe(true);
    expect(events.filter((e) => e.type === "Landed").every((e) => e.type === "Landed" && e.track === "You")).toBe(true);
    stop();
    const before = events.length;
    timer.advance(1_000);
    expect(events.length).toBe(before);
  });

  it("fails a start with the given error and a stop without a meeting", async () => {
    const error = { kind: "model", message: "model base.en is missing", retryable: false };
    await expect(createMockIpc({ startError: error }).startMeeting("Call", null)).rejects.toEqual(
      new IpcFailure(error),
    );
    await expect(createMockIpc().stopMeeting()).rejects.toMatchObject({ error: { kind: "not_recording" } });
  });

  it("serves the Models page of a Surface-like and a desktop-like computer", async () => {
    const surface = createMockIpc({ machine: "surface" });
    const hw = await surface.hardware();
    expect(hw.accelerators).toEqual(["Npu", "Processor"]);
    expect(hw.npus).toHaveLength(1);
    expect(hw.processor).toContain("Snapdragon");
    const models = await surface.models();
    expect(models.choices.map((c) => c.role)).toEqual(["Recommended", "Lighter", "European", "EveryLanguage"]);
    for (const c of models.choices) expect(models.transcription.some((m) => m.name === c.model)).toBe(true);
    expect(models.recommended).toMatchObject({ model: "parakeet-tdt-0.6b-v2", runs_on: "Npu" });
    expect(models.summary.map((m) => m.name)).toEqual(["qwen3.5-9b-q4_k_m", "qwen3.5-4b-q4_k_m"]);
    expect(models.summary_recommended).toMatchObject({ model: "qwen3.5-4b-q4_k_m", runs_on: "Processor" });
    expect(models.transcription.find((m) => m.name === MOCK_FALLBACK_MODEL)?.protected).toBe(true);
    expect(models.check).toMatchObject({ model: "parakeet-tdt-0.6b-v2", current: true });
    const desktop = createMockIpc({ machine: "desktop" });
    const dhw = await desktop.hardware();
    expect(dhw.accelerators).toEqual(["Cuda", "Processor"]);
    expect(dhw.gpus[0]).toMatchObject({ name: "NVIDIA GeForce RTX 5080", integrated: false });
    expect(dhw.npus).toEqual([]);
    const dm = await desktop.models();
    expect(dm.recommended).toMatchObject({ runs_on: "Cuda" });
    expect(dm.summary_recommended.model).toBe("qwen3.5-9b-q4_k_m");
    expect(dm.in_use).toMatchObject({ transcription_runs_on: "Cuda", summary_runs_on: "Cuda" });
    // Sizes are byte counts well below 2^53.
    for (const m of [...dm.transcription, ...dm.summary]) expect(Number.isSafeInteger(m.size)).toBe(true);
  });

  it("downloads a model with progress, keeps what arrived on cancel and allows one per model", async () => {
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ timer });
    const events: ModelJobEvent[] = [];
    await ipc.onModelJobEvent((e) => events.push(e));
    await ipc.startDownload("large-v3-turbo");
    await expect(ipc.startDownload("large-v3-turbo")).rejects.toMatchObject({ error: { kind: "busy", retryable: true } });
    await expect(ipc.startDownload("nope")).rejects.toMatchObject({ error: { kind: "unknown_model" } });
    expect((await ipc.models()).transcription.find((m) => m.name === "large-v3-turbo")?.downloading).toBe(true);
    timer.advance(3 * MODEL_STEP_MS);
    await ipc.cancelDownload("large-v3-turbo");
    await expect(ipc.cancelDownload("large-v3-turbo")).rejects.toMatchObject({ error: { kind: "not_downloading" } });
    expect(events.at(-1)).toEqual({ type: "Cancelled", model: "large-v3-turbo" });
    const partly = (await ipc.models()).transcription.find((m) => m.name === "large-v3-turbo");
    expect(partly).toMatchObject({ installed: "Partly", downloading: false });
    expect(partly?.held).toBeGreaterThan(0);
    // Starting again resumes from the bytes held and ends with one Downloaded.
    events.length = 0;
    await ipc.startDownload("large-v3-turbo");
    timer.advance(20 * MODEL_STEP_MS);
    expect(events[0]).toMatchObject({ type: "Progress", done: partly?.held });
    expect(events.filter((e) => e.type === "Downloaded")).toEqual([{ type: "Downloaded", model: "large-v3-turbo" }]);
    const downloadedAt = events.findIndex((e) => e.type === "Downloaded");
    expect(events.slice(0, downloadedAt).every((e) => e.type === "Progress")).toBe(true);
    // A transcription model's download starts the check, as the app does.
    expect(events.slice(downloadedAt + 1).map((e) => e.type)).toEqual(["CheckProgress", "CheckProgress", "Checked"]);
    const done = (await ipc.models()).transcription.find((m) => m.name === "large-v3-turbo");
    expect(done).toMatchObject({ installed: "Yes" });
    expect(done?.held).toBe(done?.size);
  });

  it("starts no check after a summary model's download and one after a transcription model's", async () => {
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ timer, machine: "surface" });
    const events: ModelJobEvent[] = [];
    await ipc.onModelJobEvent((e) => events.push(e));
    await ipc.startDownload("qwen3.5-9b-q4_k_m");
    timer.advance(20 * MODEL_STEP_MS);
    expect(events.filter((e) => e.type !== "Progress")).toEqual([{ type: "Downloaded", model: "qwen3.5-9b-q4_k_m" }]);
    // A transcription model's download starts the check (a check cannot be running
    // meanwhile: a download is refused during one, and a check during a download).
    events.length = 0;
    await ipc.startDownload("tiny.en");
    timer.advance(20 * MODEL_STEP_MS);
    expect(events.filter((e) => e.type === "Downloaded" || e.type === "Warning" || e.type === "Checked")).toEqual([
      { type: "Downloaded", model: "tiny.en" },
      { type: "Checked", model: "parakeet-tdt-0.6b-v2", runs_on: "Npu", trials: 1 },
    ]);
  });

  it("removes models that are not kept and refuses the chosen ones", async () => {
    const ipc = createMockIpc({ machine: "surface" });
    await expect(ipc.removeModel(MOCK_FALLBACK_MODEL)).rejects.toMatchObject({ error: { kind: "model_in_use" } });
    await expect(ipc.removeModel("parakeet-tdt-0.6b-v2")).rejects.toMatchObject({ error: { kind: "model_in_use" } });
    await ipc.setModelSettings({
      transcription: "parakeet-tdt-0.6b-v2",
      transcription_part: "Automatic",
      summary: null,
      summary_part: "Automatic",
      languages: ["en"],
    });
    // The 4B is the summary model of the memory rule here; nothing else keeps the 9B.
    expect(await ipc.removeModel("qwen3.5-9b-q4_k_m")).toEqual({
      bytes_freed: 0,
      kept_shared: [],
    });
    await expect(ipc.removeModel("qwen3.5-4b-q4_k_m")).rejects.toMatchObject({ error: { kind: "model_in_use" } });
  });

  it("validates the model settings and notes a part a model cannot use", async () => {
    const ipc = createMockIpc({ machine: "surface" });
    const base = (await ipc.modelSettings()).value;
    expect(base).toEqual({
      transcription: null,
      transcription_part: "Automatic",
      summary: null,
      summary_part: "Automatic",
      languages: ["en"],
    });
    for (const bad of [
      { ...base, languages: [] },
      { ...base, transcription: "base.en", languages: ["en", "de"] },
      { ...base, summary_part: "Npu" as const },
      { ...base, languages: ["xx"] },
    ]) {
      await expect(ipc.setModelSettings(bad)).rejects.toMatchObject({ error: { kind: "invalid_setting" } });
    }
    expect((await ipc.modelSettings()).value).toEqual(base);
    const stored = await ipc.setModelSettings({
      ...base,
      transcription: "parakeet-tdt-0.6b-v3",
      transcription_part: "Npu",
      languages: ["en", "DE"],
    });
    expect(stored.value.languages).toEqual(["en", "de"]);
    const models = await ipc.models();
    expect(models.in_use).toMatchObject({ transcription: "parakeet-tdt-0.6b-v3", transcription_runs_on: "Processor" });
    expect(models.in_use.notes).toEqual(["Parakeet 0.6B v3 has no NPU version, so it runs on the processor."]);
    expect(models.recommended?.model).toBe("parakeet-tdt-0.6b-v3");
    expect(models.check?.current).toBe(false);
  });

  it("plays the model check and refuses a second one meanwhile", async () => {
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ timer, machine: "desktop" });
    const events: ModelJobEvent[] = [];
    await ipc.onModelJobEvent((e) => events.push(e));
    await ipc.runCheck();
    await expect(ipc.runCheck()).rejects.toMatchObject({ error: { kind: "busy" } });
    timer.advance(3 * MODEL_STEP_MS);
    expect(events.map((e) => e.type)).toEqual(["CheckProgress", "CheckProgress", "Checked"]);
    expect(events.at(-1)).toEqual({ type: "Checked", model: "parakeet-tdt-0.6b-v2", runs_on: "Cuda", trials: 1 });
    await ipc.runCheck();
  });

  it("warns once before the trials when the check cannot load an accelerator's library", async () => {
    const timer = createManualTimer(0);
    const message = "The Cuda library of ONNX Runtime is unavailable, so the model check tests Parakeet models on the processor only.";
    const ipc = createMockIpc({ timer, machine: "desktop", modelCheckWarning: message });
    const events: ModelJobEvent[] = [];
    await ipc.onModelJobEvent((e) => events.push(e));
    await ipc.runCheck();
    timer.advance(3 * MODEL_STEP_MS);
    expect(events.map((e) => e.type)).toEqual(["Warning", "CheckProgress", "CheckProgress", "Checked"]);
    expect(events[0]).toEqual({ type: "Warning", model: null, message });
  });

  it("cancels the model check with one retryable failure", async () => {
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ timer });
    const events: ModelJobEvent[] = [];
    await ipc.onModelJobEvent((e) => events.push(e));
    await expect(ipc.cancelCheck()).rejects.toMatchObject({ error: { kind: "not_checking" } });
    await ipc.runCheck();
    timer.advance(MODEL_STEP_MS);
    await ipc.cancelCheck();
    timer.advance(10 * MODEL_STEP_MS);
    expect(events.map((e) => e.type)).toEqual(["CheckProgress", "Failed"]);
    expect(events.at(-1)).toMatchObject({ type: "Failed", model: null, retryable: true });
    await ipc.runCheck();
  });

  it("refuses downloads and removals while the model check runs", async () => {
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ timer });
    await ipc.runCheck();
    const busy = { error: { kind: "busy", message: CHECK_RUNNING_MESSAGE, retryable: true } };
    await expect(ipc.startDownload("tiny.en")).rejects.toMatchObject(busy);
    await expect(ipc.removeModel("qwen3.5-9b-q4_k_m")).rejects.toMatchObject(busy);
    timer.advance(3 * MODEL_STEP_MS);
    await ipc.startDownload("tiny.en");
    expect(await ipc.removeModel("qwen3.5-9b-q4_k_m")).toEqual({
      bytes_freed: 0,
      kept_shared: [],
    });
  });

  it("refuses the model check while a download runs and starts the last download's check", async () => {
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ timer, machine: "desktop" });
    const events: ModelJobEvent[] = [];
    await ipc.onModelJobEvent((e) => events.push(e));
    await ipc.startDownload("tiny.en");
    await expect(ipc.runCheck()).rejects.toMatchObject({
      error: { kind: "busy", message: DOWNLOADING_MESSAGE, retryable: true },
    });
    await ipc.startDownload("base.en");
    timer.advance(10 * MODEL_STEP_MS);
    const types = events.filter((e) => e.type !== "Progress").map((e) => e.type);
    expect(types.filter((t) => t === "Downloaded")).toHaveLength(2);
    expect(types.filter((t) => t === "Warning")).toHaveLength(1);
    expect(types.filter((t) => t === "CheckProgress")).toHaveLength(0);
    timer.advance(3 * MODEL_STEP_MS);
    expect(events.filter((e) => e.type === "Checked")).toHaveLength(1);
  });

  it("ignores a summary download when deciding whether the model check may start", async () => {
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ timer, machine: "desktop" });
    const events: ModelJobEvent[] = [];
    await ipc.onModelJobEvent((e) => events.push(e));
    await ipc.startDownload("qwen3.5-9b-q4_k_m");
    await ipc.startDownload("tiny.en");
    timer.advance(10 * MODEL_STEP_MS);
    expect(events.filter((e) => e.type === "Warning")).toHaveLength(0);
    timer.advance(3 * MODEL_STEP_MS);
    expect(events.filter((e) => e.type === "Checked")).toHaveLength(1);
    expect(events.filter((e) => e.type === "Downloaded")).toHaveLength(2);
  });
});

describe("mock speech helpers (T9.1f)", () => {
  it("are one item with a size, never removed, and start the check only with a model to check", async () => {
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ timer, cleanInstall: true });
    const events: ModelJobEvent[] = [];
    await ipc.onModelJobEvent((e) => events.push(e));
    const before = (await ipc.models()).helpers;
    expect(before).toMatchObject({
      name: "speech-helpers",
      title: "Speech helpers",
      kind: "Helpers",
      installed: "No",
      protected: true,
      held: 0,
    });
    expect(before.size).toBeGreaterThan(0);
    await expect(ipc.removeModel("speech-helpers")).rejects.toMatchObject({ error: { kind: "model_in_use" } });

    await ipc.startDownload("speech-helpers");
    expect((await ipc.models()).helpers.downloading).toBe(true);
    timer.advance(20 * MODEL_STEP_MS);
    const after = (await ipc.models()).helpers;
    expect(after).toMatchObject({ installed: "Yes", downloading: false, held: before.size });
    expect(events.filter((e) => e.type === "Downloaded")).toEqual([{ type: "Downloaded", model: "speech-helpers" }]);
    // The sample machine has transcription models installed, so the check follows.
    expect(events.map((e) => e.type)).toContain("Checked");
  });

  it("noModels leaves a machine with no model and no stored check", async () => {
    const m = await createMockIpc({ noModels: true }).models();
    expect([...m.transcription, ...m.summary, ...m.npu].filter((i) => i.installed === "Yes")).toEqual([]);
    expect(m.check).toBeNull();
    expect(m.helpers.installed).toBe("Yes");
  });

  it("folders: the sample meetings sit in the sample folders, listed by name", async () => {
    const ipc = createMockIpc();
    expect((await ipc.listFolders()).map((f) => f.name)).toEqual(["Planning", "Supplier calls"]);
    const filed = (await ipc.listMeetings()).filter((m) => m.folder_id !== null);
    expect(filed.map((m) => m.folder_id).sort()).toEqual(SAMPLE_FOLDERS.map((f) => f.id).sort());
    // Meetings given without folders come with none.
    expect(await createMockIpc({ meetings: [] }).listFolders()).toEqual([]);
  });

  it("folders: create, rename and move refuse what Rust refuses, in its copy", async () => {
    const ipc = createMockIpc({ meetings: [], timer: createManualTimer(0) });
    const id = await ipc.createFolder("  Orchard ");
    expect(await ipc.listFolders()).toEqual([{ id, name: "Orchard" }]);
    const refusal = async (p: Promise<unknown>) => {
      const e = await p.catch((x: unknown) => x);
      expect(e).toBeInstanceOf(IpcFailure);
      return (e as IpcFailure).error;
    };
    expect(await refusal(ipc.createFolder("ORCHARD"))).toMatchObject({
      kind: "folder_name_taken",
      message: "Another folder already has this name.",
    });
    expect((await refusal(ipc.createFolder(" "))).kind).toBe("invalid_name");
    expect((await refusal(ipc.createFolder("a\tb"))).kind).toBe("invalid_name");
    expect((await refusal(ipc.renameFolder("gone", "Pears"))).kind).toBe("folder_not_found");
    await ipc.renameFolder(id, "orchard");
    expect((await ipc.listFolders())[0].name).toBe("orchard");

    const meeting = await ipc.startMeeting("Call", null);
    expect(await refusal(ipc.moveToFolder(meeting, id))).toMatchObject({
      kind: "meeting_recording",
      message: "A meeting can be moved to a folder once it has finished recording.",
    });
    await ipc.stopMeeting();
    await ipc.moveToFolder(meeting, id);
    expect((await ipc.listMeetings())[0].folder_id).toBe(id);
    expect((await refusal(ipc.moveToFolder(meeting, "gone"))).kind).toBe("folder_not_found");
    await ipc.moveToFolder(meeting, null);
    expect((await ipc.listMeetings())[0].folder_id).toBeNull();
  });

  it("folders: deleting keeps or deletes the meetings, and a busy one keeps the folder", async () => {
    const at = (id: string, folder: string | null) => ({
      id,
      title: id,
      started_at_ms: Number(id.slice(1)),
      duration_ms: 60_000,
      state: "Ready" as const,
      folder_id: folder,
    });
    const folders = [{ id: "f1", name: "One" }, { id: "f2", name: "Two" }];
    const ipc = createMockIpc({
      meetings: [at("m1", "f1"), at("m2", "f1"), at("m3", "f2"), at("m4", "f2"), at("m5", null)],
      folders,
      timer: createManualTimer(0),
    });
    expect(await ipc.deleteFolderKeepingMeetings("f1")).toEqual({
      folder_deleted: true,
      meetings_deleted: 0,
      meetings_left: 0,
      meetings_unfiled: 2,
    });
    expect((await ipc.listMeetings()).filter((m) => m.folder_id === null)).toHaveLength(3);

    await ipc.retranscribe("m3");
    expect(await ipc.deleteFolderWithMeetings("f2")).toEqual({
      folder_deleted: false,
      meetings_deleted: 1,
      meetings_left: 1,
      meetings_unfiled: 0,
    });
    expect((await ipc.listFolders()).map((f) => f.id)).toEqual(["f2"]);
    expect((await ipc.listMeetings()).map((m) => m.id)).toEqual(["m5", "m3", "m2", "m1"]);
  });

  it("folders: a newer database refuses the lists with newer_data", async () => {
    const ipc = createMockIpc({ newerData: true });
    for (const p of [ipc.listMeetings(), ipc.listFolders()]) {
      const e = await p.catch((x: unknown) => x);
      expect(e).toBeInstanceOf(IpcFailure);
      expect((e as IpcFailure).error.kind).toBe(NEWER_DATA_KIND);
    }
  });
});
