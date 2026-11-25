// An in-memory `Ipc` with deterministic sample data, for component tests and the browser
// preview. Starting a meeting plays a scripted meeting on the given timer: levels, voice
// activity, landed lines and a warning while it records; post-meeting progress and a
// summary after it stops.

import keepLocalCases from "../../../../fixtures/ui/keep_local_cases.json";
import { type Timer, realTimer } from "../time";
import {
  RECOGNISED_ME_COPY,
  SPEAKER_NOT_ON_TRACK_COPY,
  SPLIT_NEEDS_WORD_TIMES_COPY,
  SPLIT_OUT_OF_RANGE_COPY,
  type AppInfo,
  type CaptureMode,
  type DeleteAsk,
  type Ipc,
  type UpdateEvent,
  type UpdatePhase,
  type UpdateView,
  type ScheduleChoice,
  type IpcError,
  IpcFailure,
  type JobEvent,
  type LinkChoice,
  type LiveTrack,
  type MeetingDetail,
  type MeetingEvent,
  type MeetingListItem,
  NEWER_DATA_KIND,
  DATA_FOLDER_KIND,
  type PersonItem,
  type PostStep,
  type RecoveredRecording,
  type RememberOutcome,
  type ShellEvent,
  type SummaryEvent,
  type SummaryView,
  type SyncEvent,
  type TranscriptRow,
} from "./ipc";
import { dictionaryWord, splitToken, tokensOf, withoutEndQuotes } from "../detail/dictionaryWord";
import { DELETE_COPY } from "../shell/deleteCopy";
import { marksAfterEdit, withWord, wordListOf, wordsToOffer } from "./mockWords";
import {
  ignoredVoice,
  linked,
  markedMe,
  merged,
  movableByTrack,
  movedLine,
  newSpeakerCard,
  newSpeakerRole,
  roleOf,
  notThisPersonVoice,
  renamed,
  resummarised,
  sampleDetail,
  sampleInRoomDetail,
  sampleRecognisedInRoomDetail,
  samplePersons,
  splitRow,
  unmarkedMe,
  withLineOffers,
  withUnknownLine,
} from "./mockDetail";
import type { FolderListItem } from "./folders";
import { type SampleMachine, createModelsMock } from "./mockModels";
import { searchSample } from "./mockSearch";
import { createSentRecord } from "./mockSnapshot";
import { type SettingsMockData, createSettingsMock } from "./mockSettings";
import { LIVE_NEEDS_LOCAL, NOT_A_WORD, type SummariesMockData, createSummariesMock } from "./mockSummaries";
import { createSummaryOrderMock, liveAllowed } from "./mockSummaryOrder";
import { ORDER_COPY, type StepChoice, isRemoteWriter } from "./summaryOrder";
import { type StorageMockData, createStorageMock } from "./mockStorage";
import { type BenchmarkMockData, createBenchmarkMock } from "./mockBenchmark";
import { type CudaMockData, createCudaMock } from "./mockCuda";
import { type AudioMockData, createAudioMock } from "./mockAudio";
import { type AudioCheckMockData, createAudioCheckMock } from "./mockAudioCheck";
import { type FirstRunMockData, createFirstRunMock } from "./mockFirstRun";
import { OFFLOAD_COPY, type SyncMockData, createSyncMock, mockDeviceId } from "./mockSync";
import { SAMPLE_FOLDERS, createFoldersMock } from "./mockFolders";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** The clock of the sample data when none is given: 2026-11-02 15:00 UTC. */
export const SAMPLE_NOW = Date.UTC(2026, 10, 2, 15, 0);

/** Four sample meetings placed relative to `now`, newest first: two in the last few hours,
 * one about a day earlier and one last week. The two oldest are filed in `SAMPLE_FOLDERS`. */
export function sampleMeetings(now: number = SAMPLE_NOW): MeetingListItem[] {
  return [
    {
      id: "0190f1a2-0000-7000-8000-000000000004",
      title: "Design review: meeting list",
      started_at_ms: now - 50 * MINUTE,
      duration_ms: 34 * MINUTE,
      state: "Processing",
      folder_id: null,
      new_summary: false,
    },
    {
      id: "0190f1a2-0000-7000-8000-000000000003",
      title: "Weekly sync with the Lisbon team",
      started_at_ms: now - 3 * HOUR,
      duration_ms: 47 * MINUTE,
      state: "Ready",
      folder_id: null,
      // An unread summary (T12.76).
      new_summary: true,
    },
    {
      id: "0190f1a2-0000-7000-8000-000000000002",
      title: "Supplier call about the Q1 schedule",
      started_at_ms: now - DAY - 2 * HOUR,
      duration_ms: 22 * MINUTE,
      state: "Ready",
      folder_id: SAMPLE_FOLDERS[1].id,
      new_summary: false,
    },
    {
      id: "0190f1a2-0000-7000-8000-000000000001",
      title: "Quarterly planning",
      started_at_ms: now - 6 * DAY,
      duration_ms: 95 * MINUTE,
      state: "Ready",
      folder_id: SAMPLE_FOLDERS[0].id,
      new_summary: false,
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

/** The mock's banners for provisional words that are off or waiting (T12.33), as
 * `plain_text::provisional_words_copy` words them. */
export const PROVISIONAL_WORDS_COPY = {
  battery:
    "Provisional words are paused because the computer is on battery. They start again when it is plugged in.",
  waiting: "Provisional words start once the live transcript has caught up.",
} as const;

/** When the mock's provisional words start after a wait (`provisionalWords: "waiting"`). */
export const PROVISIONAL_WORDS_START_AT = 4_000;

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
/** How long after a meeting's summary was handed to another computer its summary arrives by
 * sync in the preview (T12.81). */
export const MOCK_OFFLOAD_ANSWER_MS = 8_000;

/** The refusals of a meeting deletion (T12.38): the UI's one copy of `deletion.rs`'s words. */
export const MOCK_DELETE_COPY = DELETE_COPY;

/** `detail::edit_error`'s copy for `EditError::MeetingStillWorking` (T12.45), read from the case
 * table the desktop's Rust tests check that copy against. */
export const MOCK_STILL_WORKING_COPY = keepLocalCases.still_working.message;

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
  /** Meetings whose summary is kept on this computer (T12.13), by id. */
  keptLocal?: string[];
  /** Recordings start-up recovery saved (T9.18): returned by the first `recoveredRecordings`
   * only, as the app takes them. */
  recovered?: RecoveredRecording[];
  /** Meeting details by id (default: `sampleDetail` of each meeting). Their `speakers` are every
   * live speaker row of the meeting: a card with no line is kept and not served (T12.90). */
  details?: Record<string, MeetingDetail>;
  /** The recogniser's words of a line, by segment id (default: the tokens of its text), which a
   * split needs to equal its tokens (T12.90). */
  lineWords?: Record<string, string[]>;
  /** Finished meetings without a detail of their own were recorded in the room, with no
   * saved voice (`sampleInRoomDetail`). */
  inRoom?: boolean | "recognised";
  /** Finished meetings without a detail of their own have this line (from 0) left without a
   * speaker by the final pass (T12.65, `withUnknownLine`). */
  unknownLine?: number;
  /** The persons (default: `samplePersons()`). */
  persons?: PersonItem[];
  /** The vault exports go to (the stored Obsidian vault); without one `exportMeeting`
   * rejects with `no_vault`. */
  vault?: string;
  /** What `speakerSample` answers (default: a short silent WAV). */
  sample?: string | null;
  /** What remembering a voice saved with `saveVoice` did, per speaker (default
   * `Remembered`). */
  rememberOutcomes?: Record<string, RememberOutcome>;
  /** Speakers whose voice was saved on this computer before voice samples existed: "Not"
   * says it stays in the person's saved voice (T12.77b7). */
  savedBeforeSamples?: string[];
  /** Called with every edit and job request (`["renameSpeaker", meeting, ...]`). */
  onCall?: (call: unknown[]) => void;
  /** Where an update stands when the app opens (default: nothing asked), and a refusal of install. */
  update?: {
    phase?: UpdatePhase;
    failInstall?: string;
    /** Automatic checks on (default off). */
    autoCheck?: boolean;
    /** The stored schedule (default daily). */
    schedule?: ScheduleChoice;
    /** A stored schedule this version does not know. */
    scheduleUnknown?: string;
  };
  /** The sample computer of the Models page (default: the Surface-like one). */
  machine?: SampleMachine;
  /** The model check first warns with this (an accelerator's library could not load). */
  modelCheckWarning?: string;
  /** A machine straight after installing: no speech helpers yet. */
  cleanInstall?: boolean;
  /** The scripted meeting has no provisional words and says why (T12.33): `battery` for the
   * whole meeting, `waiting` until `PROVISIONAL_WORDS_START_AT`, when the banner is taken back. */
  provisionalWords?: keyof typeof PROVISIONAL_WORDS_COPY;
  /** No model is on this machine and there is no stored check. */
  noModels?: boolean;
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
  /** The Sync page (default: this computer is not set up). */
  sync?: SyncMockData;
  /** The order of summary writers (T12.63b; default: the stored writer alone). */
  summaryOrder?: StepChoice[];
  /** The meeting detail's account of a meeting's last summary run, by meeting id (T12.63b:
   * fixture data; the mock runs no order). */
  summaryRuns?: Record<string, string>;
  /** The meeting detail's waiting banner of a meeting another computer writes the summary of,
   * by meeting id (T12.81: fixture data). */
  waitingFor?: Record<string, string>;
  /** Offers to stop this long into a meeting (the preview's `?callEnded=1`). */
  callEndOfferAfterMs?: number;
  /** Asks whether to record a call this long after the window listens for the tray's events, when
   * the offer is on and nothing records (T12.86; the preview's `?callStarted=1`). */
  callStartOfferAfterMs?: number;
  /** The General page's "Offer to record when a call starts" as stored (default: on). */
  callStartOffer?: boolean;
  /** The folders (default: `SAMPLE_FOLDERS` with the sample meetings, none with given ones). */
  folders?: FolderListItem[];
  /** The database was saved by a newer Earshot: the meeting and folder lists reject with
   * `newer_data`, as every store command does in the app (the preview's `?newerData=1`). */
  newerData?: boolean;
  /** The data folder could not be opened at the start: the meeting and folder lists reject with
   * `data_folder` (the preview's `?dataFolder=1`). */
  dataFolderFails?: boolean;
  /** A call was recording this many ms when the window loaded (T14.51, the preview's
   * `?reloaded=1`): what it sent before went to no window, and `meetingSnapshot` tells. */
  recordingBeforeLoad?: number;
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

/** What the app says when its database was saved by a newer Earshot (`ipc::NEWER_DATA_COPY`). */
export const NEWER_DATA_COPY =
  "This data was saved by a newer version of Earshot. Install the latest version to open it.";

/** What the app says when its data folder could not be opened (`ipc::DATA_FOLDER_COPY`). */
export const DATA_FOLDER_COPY =
  "Earshot could not open its data folder on this computer. Check that the disk has free space and that another program (a backup or antivirus tool) is not holding it, then try again.";

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
  let updateAuto = data.update?.autoCheck ?? false;
  let updateSchedule: ScheduleChoice = data.update?.schedule ?? "daily";
  // A stored schedule the app does not know reads as daily and is reported.
  let updateScheduleUnknown: string | null = data.update?.scheduleUnknown ?? null;
  const updateView = (): UpdateView => ({
    current: appInfo.version,
    phase: updatePhase,
    auto_check: updateAuto,
    schedule: updateScheduleUnknown === null ? updateSchedule : "daily",
    schedule_unknown: updateScheduleUnknown,
  });
  const emitUpdate = (phase: UpdatePhase) => {
    updatePhase = phase;
    for (const l of [...updateListeners]) l({ phase });
  };
  const emitJob = (event: JobEvent) => {
    for (const l of [...jobListeners]) l(event);
  };
  const details = new Map<string, MeetingDetail>(Object.entries(data.details ?? {}));
  const recovered: RecoveredRecording[] = (data.recovered ?? []).map((r) => ({ ...r }));
  const previous = new Map<string, SummaryView>();
  // Whether the previous summary's text was edited (`summary_edits`: the label travels with
  // the text, so undo swaps it too).
  const previousEdited = new Map<string, boolean>();
  const jobs = new Map<string, "Retranscribing" | "Resummarising">();
  const jobTimers = new Map<string, (() => void)[]>();
  const persons: PersonItem[] = (data.persons ?? samplePersons()).map((p) => ({ ...p }));
  let personSeq = 0;
  // `edit::named_after_me`: separate persons with the user's own name (not the default "Me"),
  // case ignored.
  const namedAfterMe = (): PersonItem[] => {
    const user = persons.find((p) => p.is_me);
    const name = user?.name.trim().toLowerCase() ?? "";
    if (name === "" || user?.name.trim() === "Me") return [];
    return persons.filter((p) => !p.is_me && p.name.trim().toLowerCase() === name);
  };
  const call = (...args: unknown[]) => data.onCall?.(args);
  // What was sent, for a window that subscribes late (T14.51): folded in before it goes out, as
  // the app does. `pastMs` back-dates what a meeting sent before the window loaded.
  const sent = createSentRecord();
  let pastMs = 0;
  const emit = (event: MeetingEvent) => {
    sent.apply(event, timer.now() - pastMs);
    for (const l of [...listeners]) l(event);
  };
  // How long the meeting the mock starts at once had recorded when the window loaded.
  let loadedLate = data.recordingBeforeLoad ?? 0;
  let seq = 0;
  // The running meeting: its row and the timers to cancel at stop.
  let live: { row: MeetingListItem; cancel: (() => void)[] } | null = null;
  let processing = false;
  // T12.86: the offer to record a call, and the question while it is open.
  let callStartOn = data.callStartOffer ?? true;
  let callStartOpen = false;
  let callStartScheduled = false;
  // T14.52: whether a call's recording stops by itself, read when a meeting starts (off); turning
  // it off reaches the recording meeting at once, its open question included.
  let callEndStopOn = false;
  let liveCallEnd: { stops: boolean; open: boolean } | null = null;
  const shellListeners = new Set<(event: ShellEvent) => void>();
  const emitShell = (event: ShellEvent) => {
    for (const l of [...shellListeners]) l(event);
  };
  const modelsMock = createModelsMock(
    data.machine ?? "surface",
    timer,
    data.modelCheckWarning,
    data.cleanInstall,
    data.noModels,
  );
  const summariesMock = createSummariesMock(data.summaries, data.machine ?? "surface", () => persons);
  const syncMock = createSyncMock(data.sync);
  // T12.63b: the order of summary writers, with the old writer setting mirrored from it.
  const orderMock = createSummaryOrderMock(
    {
      summaries: summariesMock,
      helpers: () => syncMock.summaryHelpers(),
      thisDevice: (data.sync?.devices?.find((d) => d.this_device) ?? { id: mockDeviceId("3f2a9c1e") }).id,
      keptLocal: (meeting) => keptLocal.has(meeting),
    },
    data.summaryOrder,
  );
  const summaryRuns = new Map(Object.entries(data.summaryRuns ?? {}));
  // The summary each meeting's detail last showed (`summary-seen/<meeting>`, T12.76).
  const seenSummaries = new Map<string, string>();
  // Meetings asked of another computer, with their waiting line (T11.3db).
  const waitingFor = new Map<string, string>(Object.entries(data.waitingFor ?? {}));
  // The window's sync subscriptions (T12.81): told when the other computer's summary arrives.
  const syncListeners = new Set<(event: SyncEvent) => void>();
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
  const newerData = () => data.newerData === true;
  // The lists' failure at the start, as the app's store commands give it.
  const storeFailure = (): never | null => {
    if (newerData()) fail(NEWER_DATA_KIND, NEWER_DATA_COPY);
    if (data.dataFolderFails === true) fail(DATA_FOLDER_KIND, DATA_FOLDER_COPY);
    return null;
  };

  const record = (row: MeetingListItem, mode: CaptureMode, held: boolean, already = 0) => {
    // `ms` into the meeting: at once (dated in the past) for what came before the window loaded.
    const later = (fn: () => void, ms: number): (() => void) => {
      if (already > 0 && ms <= already) {
        pastMs = already - ms;
        fn();
        pastMs = 0;
        return () => {};
      }
      return timer.setTimeout(fn, ms - already);
    };
    const call = mode === "Call";
    const tracks: LiveTrack[] = call ? ["You", "Them"] : ["You"];
    const cancel: (() => void)[] = [];
    let words = 0;
    let tick = 0;
    // While held behind an older meeting nothing is transcribed: the lines said meanwhile land
    // at once when the hold ends (the catch-up), and no provisional words show until then.
    let holding = held;
    const waiting: (() => void)[] = [];
    const release = () => {
      holding = false;
      for (const land of waiting.splice(0)) land();
    };
    const speaking: Record<LiveTrack, boolean> = { You: false, Them: false };
    const voice = (track: LiveTrack, on: boolean) => {
      if (speaking[track] === on) return;
      speaking[track] = on;
      emit({ type: "VoiceActivity", track, speaking: on });
    };
    cancel.push(
      later(() => {
        emit({ type: "Started", meeting: row.id, mode, tracks });
        if (held) {
          emit({ type: "Transcript", state: "WaitingForPreviousMeeting" });
        } else {
          for (const track of tracks) emit({ type: "Backend", track, label: "GPU (CUDA)" });
        }
        if (data.provisionalWords !== undefined) {
          emit({
            type: "Warning",
            key: "provisional-words",
            kind: "ProvisionalWords",
            message: PROVISIONAL_WORDS_COPY[data.provisionalWords],
          });
        }
      }, 0),
    );
    if (data.provisionalWords === "waiting") {
      cancel.push(
        later(() => {
          emit({ type: "WarningCleared", key: "provisional-words" });
        }, PROVISIONAL_WORDS_START_AT),
      );
    }
    // Whether the script's guess at `at` is shown: not while provisional words are off.
    const provisionalAt = (at: number) =>
      data.provisionalWords === undefined ||
      (data.provisionalWords === "waiting" && at >= PROVISIONAL_WORDS_START_AT);
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
      if (provisionalAt(line.at - PROVISIONAL_BEFORE)) {
        cancel.push(
          later(() => {
            if (holding) return;
            emit({
              type: "Provisional",
              track: line.track,
              words: said
                .slice(0, Math.ceil(said.length / 2))
                .map((text, i) => ({ start_ms: start + i * 50, text })),
            });
          }, line.at - PROVISIONAL_BEFORE),
        );
      }
      const land = () => {
        words += said.length;
        emit({
          type: "Landed",
          track: line.track,
          start_ms: start,
          end_ms: line.at,
          text: line.text,
        });
        emit({ type: "Words", total: words });
      };
      cancel.push(later(() => (holding ? waiting.push(land) : land()), line.at));    }
    cancel.push(
      later(() => {
        emit({
          type: "Warning",
          key: "bluetooth",
          kind: "BluetoothHandsFree",
          message:
            "Headset (Acme Headset) is a Bluetooth headset in hands-free mode, so Windows records it at phone quality. Use the computer's own mic and keep the headset for listening.",
        });
      }, SCRIPT_WARNING_AT),
    );
    return { cancel, speaking, tracks, release };
  };

  // What Cancel does to each meeting past its recording, until its summary event.
  // Cancels a meeting's finishing; `now`: as a deletion that waits for it, the cancelled end comes at once.
  const finishing = new Map<string, (now?: boolean) => void>();
  // A meeting that started while an older one finishes: its transcript starts when that ends.
  let afterOlder: (() => void) | null = null;
  let speakingNow: Record<LiveTrack, boolean> | null = null;

  // Meetings kept on this computer (T12.13); "Allow" takes one out.
  const keptLocal = new Set(data.keptLocal ?? []);
  const detailOf = (id: string): MeetingDetail => {
    const row = meetings.find((m) => m.id === id);
    if (!row) return fail("meeting_not_found", "This meeting no longer exists.");
    let d = details.get(id);
    if (!d) {
      d =
        data.inRoom && row.state === "Ready"
          ? data.inRoom === "recognised"
            ? sampleRecognisedInRoomDetail(row)
            : sampleInRoomDetail(row)
          : sampleDetail(row);
      if (data.unknownLine !== undefined && row.state === "Ready") d = withUnknownLine(d, data.unknownLine);
      details.set(id, d);
    }
    return d;
  };
  // The writer "Allow" would let summarise a kept meeting, as `keep_local::remote_writer_name`.
  const remoteWriter = async (): Promise<string | null> => {
    const { writer } = (await summariesMock.summarySettings()).value;
    switch (writer) {
      case "ClaudeCode":
        return "Claude Code";
      case "Codex":
        return "Codex";
      case "OwnEndpoint":
        return "your own server";
      default:
        return null;
    }
  };
  const update = (id: string, f: (d: MeetingDetail) => MeetingDetail) => {
    details.set(id, f(detailOf(id)));
  };
  // T12.90: each line's recogniser words (`Segment::words`), which a split needs to equal the
  // line's white-space tokens: a line not listed has the words of its text (the sample keeps no
  // words of its own, so it reads as freshly decoded); a text change (an edit, a word choice)
  // clears them, as `edit::put_line_text` does, and a split shares them out. And the ids a
  // split or a new speaker takes.
  const lineWords = new Map<string, string[]>(Object.entries(data.lineWords ?? {}));
  const wordsOf = (row: TranscriptRow): string[] => lineWords.get(row.segment_id) ?? tokensOf(row.text);
  let lineIds = 0;
  // T12.15e: the word list, as `word_choices::word_list` builds it.
  const wordList = async () => wordListOf((await summariesMock.summaryVocabulary()).words, persons);
  // Suggestions a "Use it" took, so "Put back" shows them again (the mock runs no matcher).
  const usedSuggestions = new Map<string, TranscriptRow["suggestions"][number]>();
  // A final line, by `edit::final_line`'s refusals.
  const lineOf = (meeting: string, segment: string): TranscriptRow => {
    const d = detailOf(meeting);
    if (d.header.transcript === "Draft") {
      fail("draft_transcript", "The words can be corrected once the final transcript is ready.");
    }
    return (
      d.transcript.find((r) => r.segment_id === segment) ??
      fail("segment_not_found", "This line is no longer in the transcript. It may have been transcribed again.")
    );
  };
  // A line and the core of its token `token`, by `edit::word_at`'s refusal.
  const wordAt = (meeting: string, segment: string, token: number): [TranscriptRow, string] => {
    const row = lineOf(meeting, segment);
    const t = tokensOf(row.text)[token];
    const core = t === undefined ? "" : splitToken(t)[1];
    if (core === "") fail("word_not_found", "This word is no longer in the line. The line may have been edited.");
    return [row, core];
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
  // `linkSpeaker` and the link of `saveVoice` (`label` names the call): refusals before
  // anything is written, as the runtime's one transaction.
  const linkIn = (meeting: string, speaker: string, choice: LinkChoice, label: string): string => {
    call(label, meeting, speaker, choice);
    const card = speakerOf(detailOf(meeting), speaker);
    let person: PersonItem;
    if (choice.kind === "New") {
      if (card.kind === "Me") fail("me_is_the_user", "Your own row can only be linked to you.");
      personSeq += 1;
      person = {
        id: `9e000000-0000-7000-8000-1000000000${String(personSeq).padStart(2, "0")}`,
        name: checkName(choice.name),
        has_voiceprint: false,
        is_me: false,
        meetings: 1,
      };
      persons.push(person);
      persons.sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
    } else {
      person =
        persons.find((p) => p.id === choice.person) ??
        fail("person_not_found", "This person no longer exists.");
      if (person.is_me && card.kind !== "Me") {
        fail("not_the_user", "Only your own row can be linked to you.");
      }
      if (!person.is_me && card.kind === "Me") {
        fail("me_is_the_user", "Your own row can only be linked to you.");
      }
    }
    const linkedTo = person;
    // A voice saved here as a sample for someone else stops being theirs (T12.77b5).
    const was = card.person?.id ?? null;
    takenBack = card.voice_saved_here && was !== person.id ? 1 : 0;
    // The person is in one more meeting when no other voice here was theirs (`list_persons`).
    if (choice.kind === "Existing" && !detailOf(meeting).speakers.some((s) => s.person?.id === linkedTo.id)) {
      linkedTo.meetings += 1;
    }
    update(meeting, (d) => linked(d, speaker, linkedTo));
    if (takenBack > 0) {
      update(meeting, (d) => ({
        ...d,
        speakers: d.speakers.map((s) => (s.id === speaker ? { ...s, voice_saved_here: false } : s)),
      }));
    }
    return person.id;
  };
  // What the last `linkIn` took back (the runtime's `TakenBack`).
  let takenBack = 0;
  const deletionAsk = (meeting: string): DeleteAsk => {
    if (live !== null && live.row.id === meeting) return "Recording";
    const job = jobs.get(meeting);
    if (job !== undefined) return job;
    // Only a meeting this mock is finishing: a sample row's state may be older than its events.
    return finishing.has(meeting) ? "StopsFinishing" : "Free";
  };
  const startJob = (meeting: string, kind: "Retranscribing" | "Resummarising") => {
    detailOf(meeting);
    if (jobs.has(meeting)) {
      fail("busy", "This meeting is already being re-transcribed or re-summarised.", true);
    }
    jobs.set(meeting, kind);
    jobTimers.set(meeting, []);
  };
  // A job's timer, cancelled with the job (T12.41).
  const jobTimer = (meeting: string, fn: () => void, ms: number) => {
    jobTimers.get(meeting)?.push(timer.setTimeout(fn, ms));
  };
  // Cancels the job of `meeting`, as `JobRunner::cancel` does: its timers stop and its one last
  // event says it was cancelled. No job changes nothing.
  const cancelJob = (meeting: string) => {
    const kind = jobs.get(meeting);
    if (kind === undefined) return;
    for (const stop of jobTimers.get(meeting) ?? []) stop();
    jobTimers.delete(meeting);
    jobs.delete(meeting);
    emitJob({ type: "Cancelled", meeting, job: kind === "Retranscribing" ? "Retranscribe" : "Resummarise" });
  };

  const foldersMock = createFoldersMock(meetings, data.folders ?? (data.meetings ? [] : SAMPLE_FOLDERS), {
    call,
    recording: (id) => live !== null && live.row.id === id,
    busy: (id) => jobs.has(id) || (live !== null && live.row.id === id),
    remove: (id) => {
      meetings.splice(
        meetings.findIndex((m) => m.id === id),
        1,
      );
      details.delete(id);
    },
  });

  // A re-summary (by the order or a named writer): the mock writes the next sample summary.
  function runResummary(meeting: string) {
    startJob(meeting, "Resummarising");
    jobTimer(meeting, () => {
      jobs.delete(meeting);
      jobTimers.delete(meeting);
      const d = detailOf(meeting);
      if (d.summary) previous.set(meeting, d.summary);
      if (d.summary) previousEdited.set(meeting, d.header.summary_edited);
      const next = d.summary ? resummarised(d.summary) : null;
      const row = meetings.find((m) => m.id === meeting);
      if (next !== null && row !== undefined) row.new_summary = true;
      details.set(meeting, {
        ...d,
        header: { ...d.header, summary_edited: false },
        summary: next,
        can_undo_summary: d.summary !== null,
        summary_stale: false,
        summary_stale_names: false,
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
        note: null,
      });
    }, POST_STEP_MS);
  }

  // The mock's whole-list Dictionary write is not an `Ipc` command (T11.3e).
  const { setSummaryVocabulary: _wholeList, ...summaryCommands } = summariesMock;
  const ipc: Ipc = {
    ...modelsMock,
    ...summaryCommands,
    // The live switch follows the order (`live_summaries_allowed`), and a writer saved by the page
    // takes the order's first writer step (`SummaryOrder::with_writer`).
    summarySettings: async () => {
      const v = await summariesMock.summarySettings();
      return { ...v, live_unavailable: liveAllowed(await orderMock.steps()) ? null : LIVE_NEEDS_LOCAL };
    },
    setSummarySettings: async (s) => {
      const v = await summariesMock.setSummarySettings(s);
      await orderMock.writerSaved(v.value.writer);
      return { ...v, live_unavailable: liveAllowed(await orderMock.steps()) ? null : LIVE_NEEDS_LOCAL };
    },
    summaryOrder: () => orderMock.summaryOrder(),
    setSummaryOrder: (steps, settings) => orderMock.setSummaryOrder(steps, settings),
    resummariseWriters: async (meeting) => {
      detailOf(meeting);
      return orderMock.resummariseWriters(meeting);
    },
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
    ...syncMock,
    // T11.3db: a meeting asked of another computer shows that it waits.
    askSummaryHelper: async (meeting, device) => {
      const line = await syncMock.askSummaryHelper(meeting, device);
      // A named run: nothing follows if the computer does not answer (T12.81).
      waitingFor.set(meeting, `${line} ${OFFLOAD_COPY.if_no_answer.nothing}`);
      return line;
    },
    ...foldersMock,
    listFolders: async () => storeFailure() ?? foldersMock.listFolders(),
    appInfo: async () => ({ ...appInfo }),
    listMeetings: async () =>
      storeFailure() ??
      [...meetings]
        .sort((a, b) => b.started_at_ms - a.started_at_ms || b.id.localeCompare(a.id))
        .map((m) => ({ ...m })),
    searchMeetings: async (query) =>
      searchSample(meetings, (row) => details.get(row.id) ?? sampleDetail(row), query),
    startMeeting: async (mode, title, keepLocal) => {
      if (data.startError) throw new IpcFailure(data.startError);
      if (live) fail("busy", "a meeting is already recording", true);
      const held = processing;
      const already = loadedLate;
      loadedLate = 0;
      seq += 1;
      const row: MeetingListItem = {
        id: `0190f1a2-0000-7000-8000-1000000000${String(seq).padStart(2, "0")}`,
        title: title ?? "",
        started_at_ms: timer.now() - already,
        duration_ms: null,
        state: "Recording",
        folder_id: null,
        new_summary: false,
      };
      meetings.push(row);
      if (keepLocal) keptLocal.add(row.id);
      const { cancel, speaking, tracks, release } = record(row, mode, held, already);
      if (data.callEndOfferAfterMs !== undefined && mode === "Call") {
        const callEnd = { stops: callEndStopOn, open: false };
        liveCallEnd = callEnd;
        cancel.push(
          timer.setTimeout(() => {
            callEnd.open = true;
            emit({ type: "CallEndOffer", open: true, stops: callEnd.stops });
          },
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
          release();
          cancel.push(
            timer.setTimeout(() => emit({ type: "Transcript", state: "Live" }), CATCH_UP_MS),
          );
        };
      }
      return row.id;
    },
    keepRecording: async () => {
      // The offer closes on the screen; the mock only notes it is closed.
      if (liveCallEnd) liveCallEnd.open = false;
    },
    meetingSnapshot: async () => sent.snapshot(timer.now()),
    stopMeeting: async () => {
      if (!live) return fail("not_recording", "no meeting is recording");
      const { row, cancel } = live;
      live = null;
      liveCallEnd = null;
      const held = afterOlder !== null;
      afterOlder = null;
      processing = true;
      for (const c of cancel) c();
      // As the runtime's stop of a held session: the live work that never started is abandoned,
      // so the lines said during the hold never land; the final pass transcribes them.
      if (held) emit({ type: "Transcript", state: "Stopped" });
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
        const unprocessed =
          result.kind === "PostCancelled" || result.kind === "PostFailed" || result.kind === "RecordingFailed";
        row.state = unprocessed ? "Unprocessed" : "Ready";
        // The detail read during processing was the draft; the next read is the final one.
        if (!unprocessed) details.delete(row.id);
        if (result.kind === "Stored" && row.title === "") row.title = result.title;
        // A stored summary is new until the meeting's detail shows it (T12.76).
        if (result.kind === "Stored") row.new_summary = true;
        // Another computer writes it (T11.3db): the page waits, and the summary arrives by sync
        // a while later (T12.81).
        if (result.kind === "Offloaded") {
          waitingFor.set(row.id, `${result.message} ${OFFLOAD_COPY.if_no_answer.writes_here}`);
          update(row.id, (d) => ({ ...d, summary: null }));
          timer.setTimeout(() => {
            if (!meetings.includes(row)) return;
            waitingFor.delete(row.id);
            details.delete(row.id);
            row.new_summary = true;
            for (const l of [...syncListeners]) l({ meetings: [row.id], people: false });
          }, MOCK_OFFLOAD_ANSWER_MS);
        }
        emit({ type: "Summary", meeting: row.id, result });
        afterOlder?.();
        afterOlder = null;
      };
      const last = timer.setTimeout(
        () => end(data.summary ?? DEFAULT_SUMMARY),
        (POST_STEPS.length + 1) * POST_STEP_MS,
      );
      finishing.set(row.id, (now) => {
        for (const c of steps) c();
        last();
        const result: SummaryEvent = { kind: stored ? "Cancelled" : "PostCancelled" };
        if (now) {
          end(result);
          return;
        }
        // The model call in progress ends first.
        const pending = timer.setTimeout(() => end(result), POST_STEP_MS);
        // A second press has nothing more to stop; a deletion waits for the end.
        finishing.set(row.id, (again) => {
          if (!again) return;
          pending();
          end(result);
        });
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
    updateStatus: async () => updateView(),
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
      return updateView();
    },
    // Stores the choice only, as the app does: no check starts from here.
    setUpdateSchedule: async (schedule) => {
      updateSchedule = schedule;
      updateScheduleUnknown = null;
      return updateView();
    },
    installUpdate: async () => {
      call("installUpdate");
      if (data.update?.failInstall !== undefined) fail("busy", data.update.failInstall, true);
    },
    // The tray, the shortcut and notifications do not exist in the preview: the only thing sent is
    // the scripted offer to record a call (T12.86), once.
    onShellEvent: async (handler) => {
      shellListeners.add(handler);
      if (data.callStartOfferAfterMs !== undefined && !callStartScheduled) {
        callStartScheduled = true;
        timer.setTimeout(() => {
          if (!callStartOn || live !== null) return;
          callStartOpen = true;
          emitShell({ type: "CallStartOffer", open: true });
        }, data.callStartOfferAfterMs);
      }
      return () => {
        shellListeners.delete(handler);
      };
    },
    // As the app: a closed question takes no answer, and the app starts the meeting itself.
    answerCallStart: async (start) => {
      call("answerCallStart", start);
      if (!callStartOpen) return false;
      callStartOpen = false;
      emitShell({ type: "CallStartOffer", open: false });
      // The meeting starts in the background; a refusal reaches the window as the app's StartFailed.
      if (start) ipc.startMeeting("Call", null, false).catch(() => {});
      return true;
    },
    callEndStop: async () => ({ on: callEndStopOn, unknown: [] }),
    setCallEndStop: async (on) => {
      call("setCallEndStop", on);
      callEndStopOn = on;
      // Off reaches the recording meeting at once, and an open question is asked again in the off
      // wording; on applies from the next meeting.
      if (!on && liveCallEnd?.stops) {
        liveCallEnd.stops = false;
        if (liveCallEnd.open) emit({ type: "CallEndOffer", open: true, stops: false });
      }
      return { on: callEndStopOn, unknown: [] };
    },
    callStartOffer: async () => callStartOn,
    setCallStartOffer: async (on) => {
      call("setCallStartOffer", on);
      callStartOn = on;
      return callStartOn;
    },
    // The only tick that changes anything in the preview brings the summary another computer
    // was asked for (T12.81).
    onSyncEvent: async (handler) => {
      syncListeners.add(handler);
      return () => {
        syncListeners.delete(handler);
      };
    },
    getMeeting: async (meeting) => {
      const row = meetings.find((m) => m.id === meeting);
      const d = detailOf(meeting);
      const kept_local = keptLocal.has(meeting) ? { writer: await remoteWriter() } : null;
      // The header follows the list row (title from a summary, state).
      const final = d.header.transcript === "Final";
      const word_list = final ? await wordList() : [];
      // T12.15f: the Dictionary's entries, for Add or Remove on a word.
      const dictionary = final ? (await summariesMock.summaryVocabulary()).words : [];
      return structuredClone({
        ...withLineOffers(d, wordsOf),
        kept_local,
        word_list,
        dictionary,
        waiting_for: waitingFor.get(meeting) ?? d.waiting_for,
        summary_run: summaryRuns.get(meeting) ?? d.summary_run,
        state: row?.state ?? d.state,
        header: { ...d.header, title: row?.title ?? d.header.title },
      });
    },
    // T12.16: the rules of `edit::clean_title` and `edit::clean_segment_text`.
    renameMeeting: async (meeting, title) => {
      call("renameMeeting", meeting, title);
      const t = title.trim();
      if (t.length > 200) fail("invalid_title", "Titles can be up to 200 characters long.");
      if (/\p{Cc}/u.test(t)) fail("invalid_title", "Titles cannot contain line breaks or tabs.");
      detailOf(meeting);
      const row = meetings.find((m) => m.id === meeting);
      if (row) row.title = t;
      update(meeting, (d) => ({ ...d, header: { ...d.header, title: t } }));
    },
    editSegment: async (meeting, segment, text) => {
      call("editSegment", meeting, segment, text);
      const t = text.trim();
      if (t === "") fail("invalid_text", "Enter the words of this line. A line cannot be left empty.");
      if (t.length > 5_000) fail("invalid_text", "A line can be up to 5,000 characters long.");
      if (/\p{Cc}/u.test(t)) fail("invalid_text", "A line cannot contain line breaks or tabs.");
      const d = detailOf(meeting);
      if (d.header.transcript === "Draft") {
        fail("draft_transcript", "The words can be corrected once the final transcript is ready.");
      }
      if (!d.transcript.some((r) => r.segment_id === segment)) {
        fail("segment_not_found", "This line is no longer in the transcript. It may have been transcribed again.");
      }
      // T12.15f: what the bar under the line offers (`vocabulary::words_to_offer`).
      const before = d.transcript.find((r) => r.segment_id === segment)?.text ?? t;
      const offer = before === t ? [] : wordsToOffer(before, t, await wordList());
      update(meeting, (d) => {
        const row = d.transcript.find((r) => r.segment_id === segment);
        if (!row || row.text === t) return d;
        lineWords.set(segment, []);
        return marksAfterEdit(
          {
            ...d,
            transcript: d.transcript.map((r) => (r.segment_id === segment ? { ...r, text: t, edited: true } : r)),
            summary_stale: d.summary !== null || d.summary_stale,
            summary_stale_names: false,
          },
          segment,
        );
      });
      return offer;
    },
    // T12.90: `edit::move_line` and `edit::split_line`, refusals first (`edit::final_line`'s,
    // then their own, in the runtime's order), each as its one transaction. The stored detail
    // keeps every live speaker row's card (`withLineOffers` serves those with a line), so the
    // rules read the same state the runtime's do (`fixtures/ui/line_edit_cases.json`).
    moveLine: async (meeting, segment, speaker) => {
      call("moveLine", meeting, segment, speaker);
      const row = lineOf(meeting, segment);
      const d = detailOf(meeting);
      const own = d.speakers.find((s) => s.id === row.speaker_id);
      if (speaker === null) {
        // The line already is the only line of an unnamed voice other than the user's: nothing.
        const alone = !d.transcript.some((r) => r.segment_id !== segment && r.speaker_id === row.speaker_id);
        if (own !== undefined && roleOf(d, own) !== "Me" && own.person === null && alone) return;
        const role = newSpeakerRole(own ? roleOf(d, own) : null, d.header.mode === "InRoom", row.track);
        lineIds += 1;
        const id = `5a000000-0000-7000-8000-${String(0x900 + lineIds).padStart(12, "0")}`;
        update(meeting, (d) => movedLine(d, segment, newSpeakerCard(d, id, role)));
        return;
      }
      if (row.speaker_id === speaker) return;
      const card =
        d.speakers.find((s) => s.id === speaker) ??
        fail("speaker_not_found", "This speaker is no longer in the meeting.");
      if (!(movableByTrack(d)[row.track_id] ?? []).some((m) => m.id === speaker)) {
        fail("speaker_not_on_track", SPEAKER_NOT_ON_TRACK_COPY);
      }
      update(meeting, (d) => movedLine(d, segment, card));
    },
    splitLine: async (meeting, segment, token) => {
      call("splitLine", meeting, segment, token);
      const row = lineOf(meeting, segment);
      const words = wordsOf(row);
      const tokens = tokensOf(row.text);
      if (words.length === 0 || words.length !== tokens.length || tokens.some((t, i) => t !== words[i])) {
        fail("split_needs_word_times", SPLIT_NEEDS_WORD_TIMES_COPY);
      }
      if (token < 1 || token >= tokens.length) {
        fail("split_out_of_range", SPLIT_OUT_OF_RANGE_COPY);
      }
      lineIds += 1;
      const second = `5e000000-0000-7000-8000-${String(0x900 + lineIds).padStart(12, "0")}`;
      lineWords.set(segment, words.slice(0, token));
      lineWords.set(second, words.slice(token));
      update(meeting, (d) => splitRow(d, segment, token, second));
      return second;
    },
    // T12.15f: `vocabulary::add_word` and `remove_word`, through the Dictionary page's own
    // one-word commands (T11.3e: its checks, messages and the refusal while words are not in
    // use).
    addDictionaryWord: async (token) => {
      call("addDictionaryWord", token);
      const word = dictionaryWord(token) ?? fail("invalid_setting", NOT_A_WORD);
      const words = (await summariesMock.summaryVocabulary()).words;
      const there = words.find((w) => w.toLowerCase() === word.toLowerCase());
      if (there !== undefined) return { word: there, added: false };
      await summariesMock.addSummaryVocabularyWord(word);
      return { word, added: true };
    },
    removeDictionaryWord: async (word) => {
      call("removeDictionaryWord", word);
      const wanted = tokensOf(withoutEndQuotes(word)).join(" ").toLowerCase();
      const words = (await summariesMock.summaryVocabulary()).words;
      if (wanted === "" || !words.some((w) => w.toLowerCase() === wanted)) return false;
      await summariesMock.removeSummaryVocabularyWord(wanted);
      return true;
    },
    // T12.15e: the rules of `edit::use_word`, `put_back_word` and `keep_word` (mockWords.ts).
    useWord: async (meeting, segment, token, word) => {
      call("useWord", meeting, segment, token, word);
      const [row, current] = wordAt(meeting, segment, token);
      const wanted = word.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, "").toLowerCase();
      const listed =
        (await wordList()).find((w) => w.toLowerCase() === wanted) ??
        fail("word_not_listed", "That word is not in your dictionary or among the people you saved.");
      const earlier = row.changed_words.find((c) => c.token === token);
      const decoded = earlier?.decoded ?? current;
      if (earlier === undefined && listed !== decoded) {
        const suggestion = row.suggestions.find((s) => s.token === token);
        if (suggestion) usedSuggestions.set(`${meeting}/${segment}/${token}`, suggestion);
      }
      update(meeting, (d) => {
        const next = withWord(d, segment, token, listed, decoded);
        const changed = next.transcript.find((r) => r.segment_id === segment)?.text !== row.text;
        if (changed) lineWords.set(segment, []);
        return changed
          ? { ...next, summary_stale: d.summary !== null || d.summary_stale, summary_stale_names: false }
          : next;
      });
    },
    putBackWord: async (meeting, segment, token) => {
      call("putBackWord", meeting, segment, token);
      const row = lineOf(meeting, segment);
      const choice =
        row.changed_words.find((c) => c.token === token) ??
        fail("no_word_choice", "There is nothing to put back for this word. The line may have been edited.");
      const key = `${meeting}/${segment}/${token}`;
      const suggestion = usedSuggestions.get(key);
      usedSuggestions.delete(key);
      lineWords.set(segment, []);
      update(meeting, (d) => {
        const next = withWord(d, segment, token, choice.decoded, choice.decoded);
        return {
          ...next,
          summary_stale: d.summary !== null || d.summary_stale,
          summary_stale_names: false,
          transcript: next.transcript.map((r) =>
            r.segment_id === segment && suggestion
              ? { ...r, suggestions: [...r.suggestions, suggestion].sort((a, b) => a.token - b.token) }
              : r,
          ),
        };
      });
    },
    keepWord: async (meeting, segment, token) => {
      call("keepWord", meeting, segment, token);
      wordAt(meeting, segment, token);
      update(meeting, (d) => ({
        ...d,
        transcript: d.transcript.map((r) =>
          r.segment_id === segment ? { ...r, suggestions: r.suggestions.filter((s) => s.token !== token) } : r,
        ),
      }));
    },
    // The rules of `edit::edit_summary`: the current summary only, the text before the edit
    // becomes the previous summary.
    editSummary: async (meeting, summary, field, text) => {
      call("editSummary", meeting, summary, field, text);
      const t = text.trim();
      const d = detailOf(meeting);
      const changed = () =>
        fail("summary_changed", "The summary changed while you were editing it. Check the new text and edit again.");
      const s = d.summary;
      if (s === null || s.id !== summary) return changed();
      if (t === "" && field.kind !== "ActionDue") {
        fail("invalid_summary_text", "Enter some words. Only a due date can be left empty.");
      }
      if (t.length > (field.kind === "Title" ? 200 : 2_000)) {
        fail("invalid_summary_text", "This text is too long for the summary.");
      }
      if (/\p{Cc}/u.test(t)) fail("invalid_summary_text", "Summary text cannot contain line breaks or tabs.");
      const next: SummaryView = structuredClone(s);
      const item = <T,>(list: T[], i: number): T => list[i] ?? changed();
      switch (field.kind) {
        case "Title":
          next.title = t;
          break;
        case "Tldr":
          next.tldr = t;
          break;
        case "Decision":
          item(next.decisions, field.index).text = t;
          break;
        case "ActionOwner":
          item(next.actions, field.index).owner = t;
          break;
        case "ActionTask":
          item(next.actions, field.index).task = t;
          break;
        case "ActionDue":
          item(next.actions, field.index).due = t === "" ? null : t;
          break;
        case "NoteHeading":
          item(next.notes, field.section).heading = t;
          break;
        case "NoteBullet":
          item(item(next.notes, field.section).bullets, field.bullet).text = t;
          break;
        case "OpenQuestion":
          item(next.open_questions, field.index);
          next.open_questions[field.index] = t;
          break;
      }
      if (JSON.stringify(next) === JSON.stringify(s)) return;
      previous.set(meeting, s);
      previousEdited.set(meeting, d.header.summary_edited);
      details.set(meeting, {
        ...d,
        header: { ...d.header, summary_edited: true },
        summary: next,
        can_undo_summary: true,
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
          "Your own row cannot be merged away. Give the other voice your name to merge it into your row instead.",
        );
      }
      const samples_taken_back = speakerOf(d, merge).voice_saved_here ? 1 : 0;
      update(meeting, (x) => merged(x, keep, merge));
      return { samples_taken_back };
    },
    linkSpeaker: async (meeting, speaker, choice) => {
      const person = linkIn(meeting, speaker, choice, "linkSpeaker");
      return { person, taken_back: takenBack };
    },
    saveVoice: async (meeting, speaker, choice) => {
      const card = speakerOf(detailOf(meeting), speaker);
      const user = persons.find((p) => p.is_me);
      // The runtime refuses before anything is written: the user's voice is never saved here.
      if (card.kind === "Me" && choice.kind === "Existing" && choice.person === user?.id) {
        call("saveVoice", meeting, speaker, choice);
        fail("me_voice", "Your own voice is set up in Settings, not from a meeting.");
      }
      const person = linkIn(meeting, speaker, choice, "saveVoice");
      const taken_back = takenBack;
      const outcome = data.rememberOutcomes?.[speaker] ?? ("Remembered" as const);
      if (outcome === "Remembered" || outcome === "Replaced") {
        const p = persons.find((x) => x.id === person);
        if (p) p.has_voiceprint = true;
        update(meeting, (x) => ({
          ...x,
          speakers: x.speakers.map((s) =>
            s.id === speaker && s.person
              ? { ...s, person: { ...s.person, has_voiceprint: true }, voice_saved_here: true }
              : s,
          ),
        }));
      }
      const moved = taken_back > 0 && (outcome === "Remembered" || outcome === "Replaced");
      return { person, outcome, taken_back, moved };
    },
    ignoreSpeaker: async (meeting, speaker) => {
      call("ignoreSpeaker", meeting, speaker);
      const card = speakerOf(detailOf(meeting), speaker);
      if (card.kind === "Me" || card.person !== null) {
        fail("not_unnamed", "Only a voice that needs a name can be ignored.");
      }
      update(meeting, (x) => ignoredVoice(x, speaker, true));
    },
    unignoreSpeaker: async (meeting, speaker) => {
      call("unignoreSpeaker", meeting, speaker);
      speakerOf(detailOf(meeting), speaker);
      update(meeting, (x) => ignoredVoice(x, speaker, false));
    },
    notThisPerson: async (meeting, speaker) => {
      call("notThisPerson", meeting, speaker);
      const card = speakerOf(detailOf(meeting), speaker);
      // The card's person is the resolved one: a person merged into the user shows as the user.
      if (card.kind === "Me" || persons.some((p) => p.is_me && p.id === card.person?.id)) {
        fail("user_row", "This voice is you. Only a voice named as someone else can be put back as an unnamed speaker.");
      }
      if (card.person === null) fail("no_person", "Name this voice or link it to a person first.");
      const result = {
        taken_back: card.voice_saved_here ? 1 : 0,
        saved_before_samples: !card.voice_saved_here && (data.savedBeforeSamples ?? []).includes(speaker),
      };
      update(meeting, (x) => notThisPersonVoice(x, speaker));
      return result;
    },
    markMe: async (meeting, speaker) => {
      call("markMe", meeting, speaker);
      const d = detailOf(meeting);
      speakerOf(d, speaker);
      if (d.header.mode !== "InRoom") {
        fail(
          "not_in_room",
          "Only a voice in an in-room meeting can be marked as you. In a call, your voice comes from your microphone.",
        );
      }
      if (d.speakers.some((s) => s.kind === "Me")) {
        fail("meeting_has_me", 'One voice in this meeting is already you. Choose "This isn\'t me" on it first.');
      }
      let user = persons.find((p) => p.is_me);
      if (!user) {
        personSeq += 1;
        user = {
          id: `9e000000-0000-7000-8000-1000000000${String(personSeq).padStart(2, "0")}`,
          name: "Me",
          has_voiceprint: false,
          is_me: true,
          meetings: 1,
        };
        persons.push(user);
        persons.sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
      }
      const me = user;
      const taken_back = speakerOf(d, speaker).voice_saved_here ? 1 : 0;
      update(meeting, (x) => markedMe(x, speaker, me));
      return { offers: namedAfterMe().map((p) => ({ ...p })), taken_back };
    },
    mergeIntoMe: async (person) => {
      call("mergeIntoMe", person);
      const p =
        persons.find((x) => x.id === person) ??
        fail("person_not_found", "This person no longer exists.");
      if (!namedAfterMe().includes(p)) {
        fail("not_named_after_user", "Only a person with your name can be merged into you.");
      }
      const user = persons.find((x) => x.is_me);
      if (!user) return fail("not_named_after_user", "Only a person with your name can be merged into you.");
      if (p.has_voiceprint) user.has_voiceprint = true;
      persons.splice(persons.indexOf(p), 1);
      // As `detail::speaker_cards`: a link to a merged-away person stands for the user now.
      const me = { id: user.id, name: user.name, has_voiceprint: user.has_voiceprint };
      for (const [id, d] of details) {
        details.set(id, {
          ...d,
          speakers: d.speakers.map((s) =>
            s.person?.id === person || s.person?.id === user.id
              ? { ...s, person: me, can_not_this_person: false }
              : s,
          ),
        });
      }
    },
    unmarkMe: async (meeting, speaker) => {
      call("unmarkMe", meeting, speaker);
      const d = detailOf(meeting);
      const card = speakerOf(d, speaker);
      if (d.header.mode !== "InRoom") {
        fail(
          "not_in_room",
          "Only a voice in an in-room meeting can be marked as you. In a call, your voice comes from your microphone.",
        );
      }
      if (card.kind !== "Me") fail("not_me", "This voice is not marked as you.");
      if (card.recognised_me) fail("recognised_me", RECOGNISED_ME_COPY);
      update(meeting, (x) => unmarkedMe(x, speaker));
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
            return {
              ...s,
              name: `Speaker ${n}`,
              person: null,
              needs_name: true,
              can_ignore: true,
              can_not_this_person: false,
            };
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
      startJob(meeting, "Retranscribing");
      const steps: PostStep[] = POST_STEPS.filter((s) => s.kind !== "Summarising");
      steps.forEach((step, i) => {
        jobTimer(
          meeting,
          () => emitJob({ type: "Progress", meeting, step, done: Math.max(0, i - 1), total: 6 }),
          (i + 1) * POST_STEP_MS,
        );
      });
      jobTimer(
        meeting,
        () => {
          jobs.delete(meeting);
          jobTimers.delete(meeting);
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
    recoveredRecordings: async () => {
      call("recoveredRecordings");
      return recovered.splice(0);
    },
    allowSummaryWriter: async (meeting) => {
      call("allowSummaryWriter", meeting);
      detailOf(meeting);
      // `keep_local::allow_writer` (T12.45): refused while the meeting is still worked on.
      if (deletionAsk(meeting) !== "Free") fail("meeting_still_working", MOCK_STILL_WORKING_COPY);
      keptLocal.delete(meeting);
    },
    resummariseWith: async (meeting, step) => {
      call("resummariseWith", meeting, step);
      // A named remote writer on a meeting kept here is refused (`summary_kept_local`).
      if (keptLocal.has(meeting) && isRemoteWriter(step)) {
        fail("summary_kept_local", ORDER_COPY.resummarise.kept_local);
      }
      runResummary(meeting);
    },
    resummarise: async (meeting) => {
      call("resummarise", meeting);
      runResummary(meeting);
    },
    cancelJob: async (meeting) => {
      call("cancelJob", meeting);
      cancelJob(meeting);
    },
    undoSummary: async (meeting) => {
      call("undoSummary", meeting);
      const d = detailOf(meeting);
      const prev = previous.get(meeting);
      if (!prev || !d.can_undo_summary) fail("no_previous_summary", "There is no previous summary.");
      if (d.summary) previous.set(meeting, d.summary);
      const wasEdited = previousEdited.get(meeting) ?? false;
      previousEdited.set(meeting, d.header.summary_edited);
      details.set(meeting, {
        ...d,
        header: { ...d.header, summary_edited: wasEdited },
        summary: prev ?? null,
      });
    },
    // `summary_seen::mark_seen` (T12.76): only the live summary of a live meeting is recorded, and
    // the answer says whether the record changed.
    markSummarySeen: async (meeting, summary) => {
      const row = meetings.find((m) => m.id === meeting);
      if (row === undefined || detailOf(meeting).summary?.id !== summary) return false;
      const wasNew = row.new_summary;
      row.new_summary = false;
      const seen = seenSummaries.get(meeting);
      if (seen === summary) return false;
      seenSummaries.set(meeting, summary);
      // A summary the list does not call new was seen already: the app marks every summary that
      // existed at its first start (`summary_seen::mark_existing_once`), and every later one is new
      // until shown. Saying "changed" for it made the window read its list again for nothing, at
      // a moment no test or user chose (T14.51).
      return wasNew || seen !== undefined;
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
    // T12.38: the rules of `deletion::ask` and `deletion::delete`, with their copy.
    meetingDeletion: async (meeting) => {
      call("meetingDeletion", meeting);
      return deletionAsk(meeting);
    },
    deleteMeeting: async (meeting, stopFinishing) => {
      if (stopFinishing === true) call("deleteMeeting", meeting, true);
      else call("deleteMeeting", meeting);
      detailOf(meeting);
      const ask = deletionAsk(meeting);
      if (ask === "Recording") fail("meeting_recording", MOCK_DELETE_COPY.recording);
      if (ask === "Retranscribing" || ask === "Resummarising") {
        if (stopFinishing !== true) {
          fail(
            "meeting_job",
            ask === "Retranscribing" ? MOCK_DELETE_COPY.retranscribing : MOCK_DELETE_COPY.resummarising,
            true,
          );
        }
        cancelJob(meeting);
      }
      if (ask === "StopsFinishing") {
        if (stopFinishing !== true) fail("meeting_finishing", MOCK_DELETE_COPY.finishing, true);
        finishing.get(meeting)?.(true);
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
  // A meeting that was recording before the window loaded (T14.51): what it sent until now went
  // to no window, and only the snapshot tells.
  if (data.recordingBeforeLoad !== undefined) {
    ipc.startMeeting("Call", "Weekly planning", false).catch(() => {});
  }
  return ipc;
}
