// An in-memory `Ipc` with deterministic sample data, for component tests and the browser
// preview. Starting a meeting plays a scripted meeting on the given timer: levels, voice
// activity, landed lines and a warning while it records; post-meeting progress and a
// summary after it stops.

import { type Timer, realTimer } from "../time";
import {
  type AppInfo,
  type CaptureMode,
  type Ipc,
  type UpdateEvent,
  type UpdatePhase,
  type IpcError,
  IpcFailure,
  type JobEvent,
  type LiveTrack,
  type MeetingDetail,
  type MeetingEvent,
  type MeetingListItem,
  type PersonItem,
  type PostStep,
  type RememberOutcome,
  type SummaryEvent,
  type SummaryView,
} from "./ipc";
import { linked, merged, renamed, resummarised, sampleDetail, samplePersons } from "./mockDetail";
import { type SampleMachine, createModelsMock } from "./mockModels";
import { searchSample } from "./mockSearch";
import { type SettingsMockData, createSettingsMock } from "./mockSettings";
import { type SummariesMockData, createSummariesMock } from "./mockSummaries";
import { type StorageMockData, createStorageMock } from "./mockStorage";
import { type BenchmarkMockData, createBenchmarkMock } from "./mockBenchmark";
import { type CudaMockData, createCudaMock } from "./mockCuda";
import { type AudioMockData, createAudioMock } from "./mockAudio";
import { type AudioCheckMockData, createAudioCheckMock } from "./mockAudioCheck";
import { type FirstRunMockData, createFirstRunMock } from "./mockFirstRun";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** The clock of the sample data when none is given: 2026-11-02 15:00 UTC. */
export const SAMPLE_NOW = Date.UTC(2026, 10, 2, 15, 0);

/** Four sample meetings placed relative to `now`, newest first: two in the last few hours,
 * one about a day earlier and one last week. */
export function sampleMeetings(now: number = SAMPLE_NOW): MeetingListItem[] {
  return [
    {
      id: "0190f1a2-0000-7000-8000-000000000004",
      title: "Design review: meeting list",
      started_at_ms: now - 50 * MINUTE,
      duration_ms: 34 * MINUTE,
      state: "Processing",
    },
    {
      id: "0190f1a2-0000-7000-8000-000000000003",
      title: "Weekly sync with the Lisbon team",
      started_at_ms: now - 3 * HOUR,
      duration_ms: 47 * MINUTE,
      state: "Ready",
    },
    {
      id: "0190f1a2-0000-7000-8000-000000000002",
      title: "Supplier call about the Q1 schedule",
      started_at_ms: now - DAY - 2 * HOUR,
      duration_ms: 22 * MINUTE,
      state: "Ready",
    },
    {
      id: "0190f1a2-0000-7000-8000-000000000001",
      title: "Quarterly planning",
      started_at_ms: now - 6 * DAY,
      duration_ms: 95 * MINUTE,
      state: "Ready",
    },
  ];
}

/** One line of the scripted meeting: when it lands (ms after the start) and what it says. */
export interface ScriptLine {
  at: number;
  track: LiveTrack;
  text: string;
}

/** The scripted meeting's transcript (sample text). */
export const SCRIPT_LINES: ScriptLine[] = [
  { at: 1_500, track: "Them", text: "Thanks for joining. The pump order is the main thing today." },
  { at: 3_000, track: "You", text: "Sounds good. Do we have the revised quote from the supplier yet?" },
  {
    at: 4_500,
    track: "Them",
    text:
      "We do. It came in this morning, about four percent under the first one, and they can ship the first batch in March if we confirm by Friday.",
  },
  { at: 6_000, track: "You", text: "Then let's confirm on Friday. I'll send the order." },
  { at: 7_500, track: "Them", text: "Great. Can you also check the spare parts list with Priya?" },
  { at: 9_000, track: "You", text: "Yes, I'll do that tomorrow." },
];

/** How long before a scripted line lands its provisional words appear (ms). */
const PROVISIONAL_BEFORE = 900;

/** When the scripted warning arrives (ms after the start). */
export const SCRIPT_WARNING_AT = 2_500;

/** The scripted post-meeting steps, in order, one every `POST_STEP_MS`. */
export const POST_STEPS: PostStep[] = [
  { kind: "Loading" },
  { kind: "Starting" },
  { kind: "Transcribed", track: "You" },
  { kind: "Diarised", track: "You" },
  { kind: "Transcribed", track: "Them" },
  { kind: "Diarised", track: "Them" },
  { kind: "EchoChecked" },
  { kind: "Stored" },
  { kind: "Summarising" },
];

/** Time between scripted post-meeting steps (and before the summary). */
export const POST_STEP_MS = 400;

const DEFAULT_SUMMARY: SummaryEvent = {
  kind: "Stored",
  title: "Vendor review: pumps",
  actions: 4,
  decisions: 2,
  voices_to_name: 2,
};

export interface MockIpcData {
  /** The meetings (default: `sampleMeetings(now)`). */
  meetings?: MeetingListItem[];
  /** The clock the default sample meetings are placed against (default `SAMPLE_NOW`). */
  now?: number;
  /** The app info (default: version 0.0.0-preview, no data directory). */
  appInfo?: AppInfo;
  /** The timer the scripted meeting runs on (default: real time). */
  timer?: Timer;
  /** `startMeeting` rejects with this error (a pre-flight failure). */
  startError?: IpcError;
  /** The scripted meeting's summary result (default: a stored summary). */
  summary?: SummaryEvent;
  /** What the final pass warns of in a re-transcription. */
  retranscribeWarnings?: string[];
  /** Called with every `setCompact` request. */
  onSetCompact?: (compact: boolean) => void;
  /** Called when the app is asked to quit. */
  onQuit?: () => void;
  /** Meeting details by id (default: `sampleDetail` of each meeting). */
  details?: Record<string, MeetingDetail>;
  /** The persons (default: `samplePersons()`). */
  persons?: PersonItem[];
  /** The vault exports go to (the stored Obsidian vault); without one `exportMeeting`
   * rejects with `no_vault`. */
  vault?: string;
  /** What `speakerSample` answers (default: a short silent WAV). */
  sample?: string | null;
  /** What `rememberVoices` answers per speaker (default `Remembered`). */
  rememberOutcomes?: Record<string, RememberOutcome>;
  /** Called with every `rememberVoices` request. */
  onRememberVoices?: (meeting: string, speakers: string[]) => void;
  /** Called with every edit and job request (`["renameSpeaker", meeting, ...]`). */
  onCall?: (call: unknown[]) => void;
  /** Where an update stands when the app opens (default: nothing asked), and a refusal of install. */
  update?: { phase?: UpdatePhase; failInstall?: string };
  /** The sample computer of the Models page (default: the Surface-like one). */
  machine?: SampleMachine;
  /** The model check first warns with this (an accelerator's library could not load). */
  modelCheckWarning?: string;
  /** A machine straight after installing: no speech helpers yet. */
  cleanInstall?: boolean;
  /** The Summaries page's stored settings and API key (default: on this computer, no key). */
  summaries?: SummariesMockData;
  /** The Storage page's stored choice and kept recordings (default: 7 days, three). */
  storage?: StorageMockData;
  /** The General, Speakers and Obsidian pages (the vault defaults to `vault`). */
  settings?: SettingsMockData;
  /** The Audio page's stored settings and the devices listed (default: the machine's). */
  audio?: AudioMockData;
  /** How the scripted audio check goes (default: it hears the speakers). */
  audioCheck?: AudioCheckMockData;
  /** How the scripted benchmark goes (default: it keeps up and summarises). */
  benchmark?: BenchmarkMockData;
  /** The CUDA section (default: nothing installed, off, downloads available). */
  cuda?: CudaMockData;
  /** The first-run wizard (default: already done, so other screens' tests never meet it). */
  firstRun?: FirstRunMockData;
  /** Offers to stop this long into a meeting (the preview's `?callEnded=1`). */
  callEndOfferAfterMs?: number;
}

/** A 0.1 s silent 16 kHz mono WAV file, base64 (the mock's speaker sample). */
export const SILENT_WAV_BASE64 = (() => {
  const samples = 1600;
  const bytes = new Uint8Array(44 + samples * 2);
  const view = new DataView(bytes.buffer);
  const text = (at: number, s: string) => {
    for (let i = 0; i < s.length; i++) bytes[at + i] = s.charCodeAt(i);
  };
  text(0, "RIFF");
  view.setUint32(4, 36 + samples * 2, true);
  text(8, "WAVE");
  text(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, 16_000, true);
  view.setUint32(28, 32_000, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  text(36, "data");
  view.setUint32(40, samples * 2, true);
  let binary = "";
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary);
})();

/** What the mock's update host offers. */
const UPDATE_VERSION = "9.9.9";
const UPDATE_SIZE = 25_000_000;

/** How long the mock's catch-up after a held start lasts. */
export const CATCH_UP_MS = 4_000;

/** An `Ipc` that answers from memory. */
export function createMockIpc(data: MockIpcData = {}): Ipc {
  const meetings = data.meetings ? data.meetings.map((m) => ({ ...m })) : sampleMeetings(data.now);
  const appInfo = data.appInfo ?? { version: "0.0.0-preview", data_dir: "" };
  const timer = data.timer ?? realTimer;
  const listeners = new Set<(event: MeetingEvent) => void>();
  const jobListeners = new Set<(event: JobEvent) => void>();
  const updateListeners = new Set<(event: UpdateEvent) => void>();
  let updatePhase: UpdatePhase = data.update?.phase ?? { state: "Idle" };
  let updateAuto = false;
  const emitUpdate = (phase: UpdatePhase) => {
    updatePhase = phase;
    for (const l of [...updateListeners]) l({ phase });
  };
  const emitJob = (event: JobEvent) => {
    for (const l of [...jobListeners]) l(event);
  };
  const details = new Map<string, MeetingDetail>(Object.entries(data.details ?? {}));
  const previous = new Map<string, SummaryView>();
  const jobs = new Set<string>();
  const persons: PersonItem[] = (data.persons ?? samplePersons()).map((p) => ({ ...p }));
  let personSeq = 0;
  const call = (...args: unknown[]) => data.onCall?.(args);
  const emit = (event: MeetingEvent) => {
    for (const l of [...listeners]) l(event);
  };
  let seq = 0;
  // The running meeting: its row and the timers to cancel at stop.
  let live: { row: MeetingListItem; cancel: (() => void)[] } | null = null;
  let processing = false;
  const modelsMock = createModelsMock(data.machine ?? "surface", timer, data.modelCheckWarning, data.cleanInstall);
  const summariesMock = createSummariesMock(data.summaries, data.machine ?? "surface");
  const { exportTarget, ...settingsMock } = createSettingsMock(
    { vault: data.vault, ...data.settings },
    data.machine ?? "surface",
    timer,
  );
  const { inUse, ...audioMock } = createAudioMock(data.audio, data.machine ?? "surface");
  const audioCheckMock = createAudioCheckMock(data.audioCheck, timer, inUse);

  const fail = (kind: string, message: string, retryable = false): never => {
    throw new IpcFailure({ kind, message, retryable });
  };

  const record = (row: MeetingListItem, mode: CaptureMode, held: boolean) => {
    const call = mode === "Call";
    const tracks: LiveTrack[] = call ? ["You", "Them"] : ["You"];
    const cancel: (() => void)[] = [];
    let words = 0;
    let tick = 0;
    const speaking: Record<LiveTrack, boolean> = { You: false, Them: false };
    const voice = (track: LiveTrack, on: boolean) => {
      if (speaking[track] === on) return;
      speaking[track] = on;
      emit({ type: "VoiceActivity", track, speaking: on });
    };
    cancel.push(
      timer.setTimeout(() => {
        emit({ type: "Started", meeting: row.id, mode, tracks });
        if (held) {
          emit({ type: "Transcript", state: "WaitingForPreviousMeeting" });
        } else {
          for (const track of tracks) emit({ type: "Backend", track, label: "GPU (CUDA)" });
        }
      }, 0),
    );
    // Levels every 100 ms, voice activity on a 12 s cycle: Them talks, then You.
    cancel.push(
      timer.setInterval(() => {
        tick += 1;
        const phase = (tick * 100) % 12_000;
        const them = call && phase < 5_000;
        const you = phase >= 5_500 && phase < 9_000;
        voice("You", you);
        if (call) voice("Them", them);
        const wave = Math.abs(Math.sin(tick / 3));
        emit({
          type: "Levels",
          you_dbfs: you ? -32 + 14 * wave : -62 + 4 * wave,
          them_dbfs: call ? (them ? -26 + 12 * wave : -70) : null,
        });
        if (tick % 10 === 0) {
          emit({ type: "Lag", seconds: Math.min(38, 4 + tick / 20) });
        }
      }, 100),
    );
    for (const line of SCRIPT_LINES) {
      if (!call && line.track === "Them") continue;
      const start = Math.max(0, line.at - 1_400);
      const said = line.text.split(/\s+/).filter((w) => w !== "");
      // Its first words as a provisional guess while it is being said (T9.1m).
      cancel.push(
        timer.setTimeout(() => {
          emit({
            type: "Provisional",
            track: line.track,
            words: said
              .slice(0, Math.ceil(said.length / 2))
              .map((text, i) => ({ start_ms: start + i * 50, text })),
          });
        }, line.at - PROVISIONAL_BEFORE),
      );
      cancel.push(
        timer.setTimeout(() => {
          words += said.length;
          emit({
            type: "Landed",
            track: line.track,
            start_ms: start,
            end_ms: line.at,
            text: line.text,
          });
          emit({ type: "Words", total: words });
        }, line.at),
      );
    }
    cancel.push(
      timer.setTimeout(() => {
        emit({
          type: "Warning",
          key: "bluetooth",
          kind: "BluetoothHandsFree",
          message:
            "Headset (Acme Headset) is a Bluetooth headset in hands-free mode, so Windows records it at phone quality. Use the computer's own mic and keep the headset for listening.",
        });
      }, SCRIPT_WARNING_AT),
    );
    return { cancel, speaking, tracks };
  };

  // What Cancel does to each meeting past its recording, until its summary event.
  const finishing = new Map<string, () => void>();
  // A meeting that started while an older one finishes: its transcript starts when that ends.
  let afterOlder: (() => void) | null = null;
  let speakingNow: Record<LiveTrack, boolean> | null = null;

  const detailOf = (id: string): MeetingDetail => {
    const row = meetings.find((m) => m.id === id);
    if (!row) return fail("meeting_not_found", "This meeting no longer exists.");
    let d = details.get(id);
    if (!d) {
      d = sampleDetail(row);
      details.set(id, d);
    }
    return d;
  };
  const update = (id: string, f: (d: MeetingDetail) => MeetingDetail) => {
    details.set(id, f(detailOf(id)));
  };
  const speakerOf = (d: MeetingDetail, speaker: string) =>
    d.speakers.find((s) => s.id === speaker) ??
    fail("speaker_not_found", "This speaker is no longer in the meeting.");
  const checkName = (name: string) => {
    const n = name.trim();
    if (n === "") fail("invalid_name", "Enter a name.");
    if (n.length > 100) fail("invalid_name", "Names can be up to 100 characters long.");
    return n;
  };
  const startJob = (meeting: string) => {
    detailOf(meeting);
    if (jobs.has(meeting)) {
      fail("busy", "This meeting is already being re-transcribed or re-summarised.", true);
    }
    jobs.add(meeting);
  };

  return {
    ...modelsMock,
    ...summariesMock,
    models: async () => {
      const [view, summaries] = await Promise.all([modelsMock.models(), summariesMock.summarySettings()]);
      return {
        ...view,
        in_use: { ...view.in_use, summaries_here: summaries.value.writer === "OnThisComputer" },
      };
    },
    ...createStorageMock(data.storage),
    ...settingsMock,
    ...audioMock,
    ...audioCheckMock,
    ...createBenchmarkMock(data.benchmark, timer),
    ...createCudaMock(data.machine ?? "surface", timer, data.cuda),
    ...createFirstRunMock(data.firstRun),
    appInfo: async () => ({ ...appInfo }),
    listMeetings: async () =>
      [...meetings]
        .sort((a, b) => b.started_at_ms - a.started_at_ms || b.id.localeCompare(a.id))
        .map((m) => ({ ...m })),
    searchMeetings: async (query) =>
      searchSample(meetings, (row) => details.get(row.id) ?? sampleDetail(row), query),
    startMeeting: async (mode, title) => {
      if (data.startError) throw new IpcFailure(data.startError);
      if (live) fail("busy", "a meeting is already recording", true);
      const held = processing;
      seq += 1;
      const row: MeetingListItem = {
        id: `0190f1a2-0000-7000-8000-1000000000${String(seq).padStart(2, "0")}`,
        title: title ?? "",
        started_at_ms: timer.now(),
        duration_ms: null,
        state: "Recording",
      };
      meetings.push(row);
      const { cancel, speaking, tracks } = record(row, mode, held);
      if (data.callEndOfferAfterMs !== undefined && mode === "Call") {
        cancel.push(
          timer.setTimeout(
            () => emit({ type: "CallEndOffer", open: true }),
            data.callEndOfferAfterMs,
          ),
        );
      }
      speakingNow = speaking;
      live = { row, cancel };
      if (held) {
        afterOlder = () => {
          for (const track of tracks) emit({ type: "Backend", track, label: "GPU (CUDA)" });
          emit({ type: "Transcript", state: "CatchingUp" });
          cancel.push(
            timer.setTimeout(() => emit({ type: "Transcript", state: "Live" }), CATCH_UP_MS),
          );
        };
      }
      return row.id;
    },
    keepRecording: async () => {
      // The offer closes on the screen; the mock has nothing else to do.
    },
    stopMeeting: async () => {
      if (!live) return fail("not_recording", "no meeting is recording");
      const { row, cancel } = live;
      live = null;
      const held = afterOlder !== null;
      afterOlder = null;
      processing = true;
      for (const c of cancel) c();
      for (const track of ["You", "Them"] as LiveTrack[]) {
        if (speakingNow?.[track]) emit({ type: "VoiceActivity", track, speaking: false });
      }
      row.state = "Processing";
      row.duration_ms = Math.max(0, timer.now() - row.started_at_ms);
      emit({ type: "Ended", meeting: row.id, reason: "Stopped" });
      // Behind an older meeting it waits for that to end; the mock runs its steps at once then.
      if (held) row.state = "Waiting";
      let done = 0;
      let total: number | null = null;
      let stored = false;
      const steps = POST_STEPS.map((step, i) =>
        timer.setTimeout(
          () => {
            if (step.kind === "Starting") total = 6;
            else if (step.kind !== "Loading" && step.kind !== "Summarising") done += 1;
            if (step.kind === "Stored") stored = true;
            emit({ type: "PostProgress", meeting: row.id, step, done, total });
          },
          (i + 1) * POST_STEP_MS,
        ),
      );
      const end = (result: SummaryEvent) => {
        finishing.delete(row.id);
        processing = false;
        row.state = result.kind === "PostCancelled" ? "Unprocessed" : "Ready";
        // The detail read during processing was the draft; the next read is the final one.
        if (result.kind !== "PostCancelled") details.delete(row.id);
        if (result.kind === "Stored" && row.title === "") row.title = result.title;
        emit({ type: "Summary", meeting: row.id, result });
        afterOlder?.();
        afterOlder = null;
      };
      const last = timer.setTimeout(
        () => end(data.summary ?? DEFAULT_SUMMARY),
        (POST_STEPS.length + 1) * POST_STEP_MS,
      );
      finishing.set(row.id, () => {
        for (const c of steps) c();
        last();
        // A second press has nothing more to stop.
        finishing.set(row.id, () => {});
        // The model call in progress ends first.
        timer.setTimeout(
          () => end({ kind: stored ? "Cancelled" : "PostCancelled" }),
          POST_STEP_MS,
        );
      });
    },
    cancelFinishing: async (meeting) => {
      finishing.get(meeting)?.();
    },
    setCompact: async (compact) => {
      data.onSetCompact?.(compact);
    },
    // The preview has no native title bar.
    setWindowTheme: async (theme) => {
      call("setWindowTheme", theme);
    },
    quitApp: async () => {
      data.onQuit?.();
    },
    onUpdateEvent: async (handler) => {
      updateListeners.add(handler);
      return () => updateListeners.delete(handler);
    },
    updateStatus: async () => ({
      current: appInfo.version,
      phase: updatePhase,
      auto_check: updateAuto,
    }),
    checkForUpdate: async () => {
      call("checkForUpdate");
      emitUpdate({ state: "Available", version: UPDATE_VERSION, size: UPDATE_SIZE });
    },
    downloadUpdate: async () => {
      call("downloadUpdate");
      emitUpdate({ state: "Ready", version: UPDATE_VERSION });
    },
    cancelUpdate: async () => {
      call("cancelUpdate");
      emitUpdate({ state: "Available", version: UPDATE_VERSION, size: UPDATE_SIZE });
    },
    setUpdateAutoCheck: async (on) => {
      updateAuto = on;
      return { current: appInfo.version, phase: updatePhase, auto_check: updateAuto };
    },
    installUpdate: async () => {
      call("installUpdate");
      if (data.update?.failInstall !== undefined) fail("busy", data.update.failInstall, true);
    },
    // The tray, the shortcut and notifications do not exist in the preview: nothing is sent.
    onShellEvent: async () => () => {},
    getMeeting: async (meeting) => {
      const row = meetings.find((m) => m.id === meeting);
      const d = detailOf(meeting);
      // The header follows the list row (title from a summary, state).
      return structuredClone({
        ...d,
        state: row?.state ?? d.state,
        header: { ...d.header, title: row?.title ?? d.header.title },
      });
    },
    renameSpeaker: async (meeting, speaker, name) => {
      call("renameSpeaker", meeting, speaker, name);
      const n = checkName(name);
      speakerOf(detailOf(meeting), speaker);
      update(meeting, (d) => renamed(d, speaker, n));
    },
    mergeSpeakers: async (meeting, keep, merge) => {
      call("mergeSpeakers", meeting, keep, merge);
      const d = detailOf(meeting);
      if (keep === merge) fail("same_speaker", "A speaker cannot be merged into itself.");
      speakerOf(d, keep);
      if (speakerOf(d, merge).kind === "Me") {
        fail(
          "merge_away_me",
          "Your own row cannot be merged away. Merge the other speaker into it instead.",
        );
      }
      update(meeting, (x) => merged(x, keep, merge));
    },
    linkSpeaker: async (meeting, speaker, choice) => {
      call("linkSpeaker", meeting, speaker, choice);
      const card = speakerOf(detailOf(meeting), speaker);
      let person: PersonItem;
      if (choice.kind === "New") {
        personSeq += 1;
        person = {
          id: `9e000000-0000-7000-8000-1000000000${String(personSeq).padStart(2, "0")}`,
          name: checkName(choice.name),
          has_voiceprint: false,
          is_me: false,
        };
        persons.push(person);
        persons.sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
      } else {
        person =
          persons.find((p) => p.id === choice.person) ??
          fail("person_not_found", "This person no longer exists.");
      }
      if (person.is_me && card.kind !== "Me") {
        fail("not_the_user", "Only your own row can be linked to you.");
      }
      if (!person.is_me && card.kind === "Me") {
        fail("me_is_the_user", "Your own row can only be linked to you.");
      }
      update(meeting, (d) => linked(d, speaker, person));
      return person.id;
    },
    renamePerson: async (person, name) => {
      call("renamePerson", person, name);
      const n = checkName(name);
      const p =
        persons.find((x) => x.id === person) ??
        fail("person_not_found", "This person no longer exists.");
      p.name = n;
      let changed = 0;
      for (const [id, d] of details) {
        for (const s of d.speakers) {
          if (s.person?.id === person && s.name !== n) {
            changed += 1;
            details.set(id, linked(details.get(id) ?? d, s.id, p));
          }
        }
      }
      return changed;
    },
    listPersons: async () => persons.map((p) => ({ ...p })),
    rememberVoices: async (meeting, speakers) => {
      data.onRememberVoices?.(meeting, speakers);
      const d = detailOf(meeting);
      const results = speakers.map((id) => {
        const s = speakerOf(d, id);
        if (s.kind === "Me") fail("me_voice", "Your own voice is set up in Settings, not from a meeting.");
        if (s.person === null) fail("no_person", "Name this voice or link it to a person first.");
        return { speaker: id, outcome: data.rememberOutcomes?.[id] ?? ("Remembered" as const) };
      });
      const saved = results.filter((r) => r.outcome === "Remembered").map((r) => r.speaker);
      for (const s of d.speakers) {
        const p = persons.find((x) => x.id === s.person?.id);
        if (p && saved.includes(s.id)) p.has_voiceprint = true;
      }
      update(meeting, (x) => ({
        ...x,
        speakers: x.speakers.map((s) =>
          saved.includes(s.id) && s.person ? { ...s, person: { ...s.person, has_voiceprint: true } } : s,
        ),
      }));
      return results;
    },
    forgetPersonVoices: async (person) => {
      call("forgetPersonVoices", person);
      const p =
        persons.find((x) => x.id === person) ??
        fail("person_not_found", "This person no longer exists.");
      const had = p.has_voiceprint ? 1 : 0;
      p.has_voiceprint = false;
      for (const [id, d] of details) {
        details.set(id, {
          ...d,
          speakers: d.speakers.map((s) =>
            s.person?.id === person ? { ...s, person: { ...s.person, has_voiceprint: false } } : s,
          ),
        });
      }
      return had;
    },
    removePerson: async (person) => {
      call("removePerson", person);
      const at = persons.findIndex((x) => x.id === person);
      if (at < 0) fail("person_not_found", "This person no longer exists.");
      persons.splice(at, 1);
      for (const [id, d] of details) {
        const taken = d.speakers.filter((s) => s.person?.id !== person).map((s) => s.name);
        details.set(id, {
          ...d,
          speakers: d.speakers.map((s) => {
            if (s.person?.id !== person) return s;
            let n = 1;
            while (taken.includes(`Speaker ${n}`)) n += 1;
            taken.push(`Speaker ${n}`);
            return { ...s, name: `Speaker ${n}`, person: null, needs_name: true };
          }),
        });
      }
    },
    speakerSample: async (meeting, speaker) => {
      speakerOf(detailOf(meeting), speaker);
      return data.sample === undefined ? SILENT_WAV_BASE64 : data.sample;
    },
    retranscribe: async (meeting) => {
      call("retranscribe", meeting);
      startJob(meeting);
      const steps: PostStep[] = POST_STEPS.filter((s) => s.kind !== "Summarising");
      steps.forEach((step, i) => {
        timer.setTimeout(
          () => emitJob({ type: "Progress", meeting, step, done: Math.max(0, i - 1), total: 6 }),
          (i + 1) * POST_STEP_MS,
        );
      });
      timer.setTimeout(
        () => {
          jobs.delete(meeting);
          emitJob({
            type: "Retranscribed",
            meeting,
            missing_tracks: 0,
            no_audio: false,
            summary_stale: detailOf(meeting).summary_stale,
            warnings: data.retranscribeWarnings ?? [],
          });
        },
        (steps.length + 1) * POST_STEP_MS,
      );
    },
    resummarise: async (meeting) => {
      call("resummarise", meeting);
      startJob(meeting);
      timer.setTimeout(() => {
        jobs.delete(meeting);
        const d = detailOf(meeting);
        if (d.summary) previous.set(meeting, d.summary);
        const next = d.summary ? resummarised(d.summary) : null;
        details.set(meeting, {
          ...d,
          summary: next,
          can_undo_summary: d.summary !== null,
          summary_stale: false,
        });
        const voices = d.speakers.filter((s) => s.needs_name).length;
        emitJob({
          type: "Summary",
          meeting,
          result: next
            ? {
                kind: "Stored",
                title: next.title,
                actions: next.actions.length,
                decisions: next.decisions.length,
                voices_to_name: voices,
              }
            : { kind: "Skipped", reason: "Summaries are turned off." },
        });
      }, POST_STEP_MS);
    },
    undoSummary: async (meeting) => {
      call("undoSummary", meeting);
      const d = detailOf(meeting);
      const prev = previous.get(meeting);
      if (!prev || !d.can_undo_summary) fail("no_previous_summary", "There is no previous summary.");
      if (d.summary) previous.set(meeting, d.summary);
      details.set(meeting, { ...d, summary: prev ?? null });
    },
    exportMeeting: async (meeting) => {
      call("exportMeeting", meeting);
      const d = detailOf(meeting);
      const target = exportTarget();
      if (target.vault === "") fail("no_vault", "Set your Obsidian vault in Settings to export.");
      const title = d.header.title.trim() === "" ? "Untitled meeting" : d.header.title;
      const folder = target.folder === "" ? "" : `${target.folder}/`;
      return { note_path: `${folder}${title.replace(/[:\\/]/g, " -")}.md` };
    },
    deleteMeeting: async (meeting) => {
      call("deleteMeeting", meeting);
      detailOf(meeting);
      if (jobs.has(meeting) || (live !== null && live.row.id === meeting)) {
        fail(
          "meeting_busy",
          `meeting ${meeting} is being recorded, processed, exported or deleted; try again when it has finished`,
          true,
        );
      }
      const i = meetings.findIndex((m) => m.id === meeting);
      meetings.splice(i, 1);
      details.delete(meeting);
    },
    onJobEvent: async (handler) => {
      jobListeners.add(handler);
      return () => {
        jobListeners.delete(handler);
      };
    },
    onMeetingEvent: async (handler) => {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
  };
}
