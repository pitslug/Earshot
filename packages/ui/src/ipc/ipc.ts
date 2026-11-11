// The one IPC interface the UI uses. Components get it from `useIpc()`; only
// `createTauriIpc` (tauri.ts) talks to Tauri.

import type {
  ActionView,
  AppInfo,
  AudioCheckEvent,
  AudioCheckOutcome,
  AudioCheckResultView,
  AudioCheckView,
  AudioPageView,
  AudioSettingsValue,
  ChosenDevice,
  DeviceItem,
  CaptureMode,
  CheckView,
  ChoiceKind,
  ChoiceView,
  Citation,
  CitedText,
  DetailHeader,
  HandoverItem,
  KeptLocalView,
  EndedReason,
  ExportDone,
  GpuView,
  HardwareView,
  InUseView,
  Installed,
  IpcError,
  JobEvent,
  JobKind,
  LinkChoice,
  LiveTrack,
  MeetingDetail,
  MeetingEvent,
  MeetingListItem,
  MeetingState,
  TranscriptState,
  ModelItem,
  ModelJobEvent,
  UpdateEvent,
  UpdatePhase,
  UpdateView,
  ModelSettingsValue,
  ModelSettingsView,
  ModelUse,
  ModelsView,
  NotesView,
  NpuView,
  PartChoice,
  PersonItem,
  PersonRef,
  PostStep,
  ProviderChoice,
  RecommendedView,
  RememberOutcome,
  SavedVoiceResult,
  RemovedView,
  RowTrack,
  RunsOn,
  SearchMatch,
  SearchResultItem,
  SearchResultsView,
  SpeakerCard,
  SpeakerKind,
  SpeakerSource,
  ClaudeCodeState,
  ClaudeCodeStatusView,
  CodexState,
  CodexStatusView,
  EffortChoice,
  SummaryEvent,
  SummarySettingsValue,
  SummarySettingsView,
  SummaryFieldRef,
  SummaryView,
  TranscriptKind,
  TranscriptRow,
  ChangedWordView,
  WordSuggestionView,
  Verdict,
  WarningKind,
  WriterChoice,
  GeneralView,
  PreviewsChoice,
  PreviewsView,
  ShellEvent,
  ObsidianSettingsValue,
  ObsidianSettingsView,
  SpeakerTimingChoice,
  SpeakerTimingOption,
  SpeakersView,
  VoiceEvent,
  VoiceResult,
  VoiceView,
  WindowTheme,
  commands,
} from "./bindings";
import type { BenchmarkCommands, BenchmarkEvent } from "./benchmark";
import type { CudaCommands, CudaEvent } from "./cuda";
import type { FirstRunCommands } from "./firstRun";
import type { FolderCommands } from "./folders";
import type { StorageCommands } from "./storage";
import type { SummaryStyleCommands } from "./summaryStyle";
import type { SummaryVocabularyCommands } from "./summaryVocabulary";

export type {
  ActionView,
  AppInfo,
  AudioCheckEvent,
  AudioCheckOutcome,
  AudioCheckResultView,
  AudioCheckView,
  AudioPageView,
  AudioSettingsValue,
  ChosenDevice,
  DeviceItem,
  CaptureMode,
  CheckView,
  ChoiceKind,
  ChoiceView,
  Citation,
  CitedText,
  DetailHeader,
  HandoverItem,
  KeptLocalView,
  EndedReason,
  ExportDone,
  GpuView,
  HardwareView,
  InUseView,
  Installed,
  IpcError,
  JobEvent,
  JobKind,
  LinkChoice,
  LiveTrack,
  MeetingDetail,
  MeetingEvent,
  MeetingListItem,
  MeetingState,
  TranscriptState,
  ModelItem,
  ModelJobEvent,
  UpdateEvent,
  UpdatePhase,
  UpdateView,
  ModelSettingsValue,
  ModelSettingsView,
  ModelUse,
  ModelsView,
  NotesView,
  NpuView,
  PartChoice,
  PersonItem,
  PersonRef,
  PostStep,
  ProviderChoice,
  RecommendedView,
  RememberOutcome,
  SavedVoiceResult,
  RemovedView,
  RowTrack,
  RunsOn,
  SearchMatch,
  SearchResultItem,
  SearchResultsView,
  SpeakerCard,
  SpeakerKind,
  SpeakerSource,
  ClaudeCodeState,
  ClaudeCodeStatusView,
  CodexState,
  CodexStatusView,
  EffortChoice,
  SummaryEvent,
  SummarySettingsValue,
  SummarySettingsView,
  SummaryFieldRef,
  SummaryView,
  TranscriptKind,
  TranscriptRow,
  ChangedWordView,
  WordSuggestionView,
  Verdict,
  WarningKind,
  WriterChoice,
  GeneralView,
  PreviewsChoice,
  PreviewsView,
  ShellEvent,
  ObsidianSettingsValue,
  ObsidianSettingsView,
  SpeakerTimingChoice,
  SpeakerTimingOption,
  SpeakersView,
  VoiceEvent,
  VoiceResult,
  VoiceView,
  WindowTheme,
};

/** Everything the UI can ask of the app. One method per command in `bindings.ts`. */
export interface IpcCommands
  extends StorageCommands,
    SummaryStyleCommands,
    SummaryVocabularyCommands,
    BenchmarkCommands,
    FirstRunCommands,
    FolderCommands,
    CudaCommands {
  /** The app version and data directory. */
  appInfo(): Promise<AppInfo>;
  /** The meetings, newest first. Rejects with an `IpcFailure`. */
  listMeetings(): Promise<MeetingListItem[]>;
  /** The meetings whose title, transcript or summary contains `query`, newest first. An empty
   * query finds nothing. Rejects with an `IpcFailure`. */
  searchMeetings(query: string): Promise<SearchResultsView>;
  /** Starts a meeting and resolves with its id once it records. Rejects with an
   * `IpcFailure` (kind `busy` while another meeting runs, or a pre-flight failure).
   * `keepLocal` keeps its summary on this computer (T12.13; asked only when it starts). */
  startMeeting(mode: CaptureMode, title: string | null, keepLocal: boolean): Promise<string>;
  /** Stops the recording meeting. Rejects with an `IpcFailure` (`not_recording`). */
  stopMeeting(): Promise<void>;
  /** Stops the final pass and the summary of a meeting whose recording is saved; its `Summary`
   * event says how it ended. A meeting still recording or already finished: nothing happens. */
  cancelFinishing(meeting: string): Promise<void>;
  /** "Keep recording" on the offer to stop when a call seems to have ended. */
  keepRecording(): Promise<void>;
  /** Shows the window as the compact listening card, or the full app. */
  setCompact(compact: boolean): Promise<void>;
  /** Puts the native title bar on a theme, or back on the system's (`null`). */
  setWindowTheme(theme: WindowTheme | null): Promise<void>;
  /** Everything the meeting detail screen shows. Rejects with `meeting_not_found`. */
  getMeeting(meeting: string): Promise<MeetingDetail>;
  /** Gives the meeting a new title ("" makes it untitled). Rejects with `invalid_title`. */
  renameMeeting(meeting: string, title: string): Promise<void>;
  /** Replaces the text of one line of the final transcript; the line keeps its id and times.
   * Rejects with `invalid_text`, `segment_not_found` or `draft_transcript`. */
  editSegment(meeting: string, segment: string, text: string): Promise<void>;
  /** "Use it", or another word from `MeetingDetail.word_list` (T12.15e): word `token` of a
   * final line becomes `word`, its punctuation kept; the line is marked edited. Rejects with
   * `word_not_listed`, `word_not_found`, `segment_not_found` or `draft_transcript`. */
  useWord(meeting: string, segment: string, token: number, word: string): Promise<void>;
  /** "Put back" (T12.15e): a changed word reads as decoded again. Rejects with
   * `no_word_choice`, `segment_not_found` or `draft_transcript`. */
  putBackWord(meeting: string, segment: string, token: number): Promise<void>;
  /** "Keep" (T12.15e): the suggestion goes and does not come back for this meeting; the
   * line is not changed. Rejects with `word_not_found`, `segment_not_found` or
   * `draft_transcript`. */
  keepWord(meeting: string, segment: string, token: number): Promise<void>;
  /** Corrects one text of the current summary `summary` (T12.16); the text before the edit
   * becomes the previous summary. Rejects with `invalid_summary_text` or `summary_changed`. */
  editSummary(meeting: string, summary: string, field: SummaryFieldRef, text: string): Promise<void>;
  /** Renames a speaker. Rejects with an edit refusal (`invalid_name`, ...). */
  renameSpeaker(meeting: string, speaker: string, name: string): Promise<void>;
  /** Merges speaker `merge` into `keep`. */
  mergeSpeakers(meeting: string, keep: string, merge: string): Promise<void>;
  /** Links a speaker to a person; resolves with the person's id. */
  linkSpeaker(meeting: string, speaker: string, choice: LinkChoice): Promise<string>;
  /** "This is me": a voice of an in-room meeting becomes its one "Me" row. Saves no voice.
   * Rejects with `not_in_room` or `meeting_has_me`. */
  markMe(meeting: string, speaker: string): Promise<void>;
  /** "This isn't me": a "Me" row of an in-room meeting goes back to an unnamed voice. */
  unmarkMe(meeting: string, speaker: string): Promise<void>;
  /** Renames a person and their speaker rows in every meeting. */
  renamePerson(person: string, name: string): Promise<number>;
  /** Every person, by name. */
  listPersons(): Promise<PersonItem[]>;
  /** "Save" on a voice that needs a name (T12.19): names it, links it to that person and
   * remembers the voice, in one step. */
  saveVoice(meeting: string, speaker: string, choice: LinkChoice): Promise<SavedVoiceResult>;
  /** "Ignore": the voice no longer needs a name and keeps its "Speaker N" label. Rejects with
   * `not_unnamed` or `no_stored_voice`. */
  ignoreSpeaker(meeting: string, speaker: string): Promise<void>;
  /** Undoes "Ignore": the voice needs a name again. */
  unignoreSpeaker(meeting: string, speaker: string): Promise<void>;
  /** Forgets every voiceprint of a person. */
  forgetPersonVoices(person: string): Promise<number>;
  removePerson(person: string): Promise<void>;
  /** The Updates section's state. */
  updateStatus(): Promise<UpdateView>;
  /** Asks the update host for the latest version; `UpdateEvent`s follow. */
  checkForUpdate(): Promise<void>;
  /** Downloads what the last check found; `UpdateEvent`s follow. */
  downloadUpdate(): Promise<void>;
  /** Stops a download; what arrived is kept. */
  cancelUpdate(): Promise<void>;
  /** Turns the check at each start on or off. */
  setUpdateAutoCheck(on: boolean): Promise<UpdateView>;
  /** Starts the verified installer and exits. Rejects with `busy` or `update_not_ready`. */
  installUpdate(): Promise<void>;
  /** A speaker's sample as a base64 WAV file; `null` when the audio is no longer kept. */
  speakerSample(meeting: string, speaker: string): Promise<string | null>;
  /** Starts a re-transcription; `JobEvent`s follow. Rejects with `busy` / `meeting_busy`. */
  retranscribe(meeting: string): Promise<void>;
  /** Starts a re-summary; `JobEvent`s follow. Rejects with `busy` / `meeting_busy`. */
  resummarise(meeting: string): Promise<void>;
  /** Swaps the current summary with the previous one. */
  undoSummary(meeting: string): Promise<void>;
  /** Lets the summary writer summarise a meeting kept on this computer from now on (T12.13).
   * Sends nothing: the next summary does. */
  allowSummaryWriter(meeting: string): Promise<void>;
  /** Exports to the Obsidian vault. Rejects with `no_vault` while none is set. */
  exportMeeting(meeting: string): Promise<ExportDone>;
  /** Deletes the meeting. Rejects with `meeting_busy` while a job holds it. */
  deleteMeeting(meeting: string): Promise<void>;
  /** This computer's hardware as read and the parts this build runs models on. */
  hardware(): Promise<HardwareView>;
  /** The Models page: every model, the choices, the recommendations, the check, what is used. */
  models(): Promise<ModelsView>;
  /** The Models page settings. */
  modelSettings(): Promise<ModelSettingsView>;
  /** Stores the Models page settings. Rejects with `invalid_setting` (nothing stored). */
  setModelSettings(settings: ModelSettingsValue): Promise<ModelSettingsView>;
  /** Starts downloading a model; `ModelJobEvent`s follow. Rejects with `busy`, `unknown_model`. */
  startDownload(model: string): Promise<void>;
  /** Cancels a download (what arrived is kept). Rejects with `not_downloading`. */
  cancelDownload(model: string): Promise<void>;
  /** Removes a model's files. Rejects with `model_in_use` for a chosen or kept model. */
  removeModel(model: string): Promise<RemovedView>;
  /** Starts the model check in its own process; `ModelJobEvent`s follow. Rejects with
   * `busy` while a check, a meeting or a meeting job runs. */
  runCheck(): Promise<void>;
  /** Cancels the running model check (a `Failed` follows). Rejects with `not_checking`. */
  cancelCheck(): Promise<void>;
  /** The Summaries page settings. */
  summarySettings(): Promise<SummarySettingsView>;
  /** Stores the Summaries page settings. Rejects with `invalid_setting` (nothing stored). */
  setSummarySettings(settings: SummarySettingsValue): Promise<SummarySettingsView>;
  /** Stores the Anthropic API key in Windows Credential Manager; it is never sent back.
   * Rejects with `invalid_setting` (not a key) or `api_key` (Credential Manager failed). */
  setApiKey(key: string): Promise<void>;
  /** Whether an Anthropic API key is stored. Rejects with `api_key`. */
  hasApiKey(): Promise<boolean>;
  /** Removes the stored Anthropic API key. Rejects with `api_key`. */
  removeApiKey(): Promise<void>;
  /** Whether Claude Code is installed and signed in, with how to fix it (T7.4ga). */
  claudeCodeStatus(): Promise<ClaudeCodeStatusView>;
  /** Whether Codex is installed and signed in, with how to fix it (T7.4gc). */
  codexStatus(): Promise<CodexStatusView>;
  /** The Speakers page: when speaker detection runs and whether your voice is saved. */
  speakers(): Promise<SpeakersView>;
  /** Stores when speaker detection runs (from the next meeting). Rejects with
   * `invalid_setting` for the battery choice on a computer without a battery. */
  setSpeakerTiming(timing: SpeakerTimingChoice): Promise<SpeakersView>;
  /** Stores the user's name; empty goes back to none. Refuses an unusable name. */
  setMyName(name: string): Promise<SpeakersView>;
  /** Records a voice sample and saves it as your voiceprint (ask the user first);
   * `VoiceEvent`s follow. Rejects with `busy` while a sample, a meeting or the check runs. */
  startVoiceSample(): Promise<void>;
  /** Cancels the voice sample; nothing is saved. */
  cancelVoiceSample(): Promise<void>;
  /** Forgets your voice; resolves with how many voiceprints were forgotten. */
  forgetMyVoice(): Promise<number>;
  /** The Obsidian page settings. */
  obsidianSettings(): Promise<ObsidianSettingsView>;
  /** Checks and stores the Obsidian page settings. Rejects with `invalid_setting`. */
  setObsidianSettings(settings: ObsidianSettingsValue): Promise<ObsidianSettingsView>;
  /** Opens the Windows folder dialog, starting at `start`; resolves with the folder chosen or
   * null when cancelled. Stores nothing. Rejects with `folder_dialog` when it cannot open. */
  pickObsidianVault(start: string): Promise<string | null>;
  /** The General page: start with Windows, the theme, the version. */
  generalSettings(): Promise<GeneralView>;
  /** Turns "start Earshot with Windows" on or off. Rejects with `autostart`. */
  setStartWithWindows(on: boolean): Promise<GeneralView>;
  /** Whether meetings show provisional words on this computer (T9.1mb). */
  previewsSettings(): Promise<PreviewsView>;
  /** Stores whose provisional words meetings show (T9.14), from the next meeting. */
  setPreviews(choice: PreviewsChoice): Promise<PreviewsView>;
  /** Where the crash logs are (T9.1e), null when the app has no data folder. */
  crashLogsFolder(): Promise<string | null>;
  /** Quits the app: a recording meeting is stopped and finalised first. */
  quitApp(): Promise<void>;
  /** The Audio page: its settings and the devices Windows lists now (T7.4i). */
  audioSettings(): Promise<AudioPageView>;
  /** Stores the Audio page settings. Rejects with `invalid_setting` (nothing stored). */
  setAudioSettings(settings: AudioSettingsValue): Promise<AudioPageView>;
  /** The stored capture mode a new meeting starts with (reads no devices). */
  defaultCaptureMode(): Promise<CaptureMode>;
  /** The last audio check of the microphone and output a meeting would use now (T7.4j). */
  audioCheck(): Promise<AudioCheckView>;
  /** Starts the audio check: it plays short sounds through the output (warn the user
   * first); `AudioCheckEvent`s follow. Rejects with `busy` while a check, a meeting, the
   * model check or the voice sample runs. */
  startAudioCheck(): Promise<void>;
  /** Cancels the audio check; nothing is stored. */
  cancelAudioCheck(): Promise<void>;
}

/** The commands plus the app's events. */
export interface Ipc extends IpcCommands {
  /** Calls `handler` for every meeting event; resolves with a function that stops it. */
  onMeetingEvent(handler: (event: MeetingEvent) => void): Promise<() => void>;
  /** Calls `handler` for every job event (re-transcribe, re-summarise). */
  onJobEvent(handler: (event: JobEvent) => void): Promise<() => void>;
  /** Calls `handler` for every model job event (downloads, the model check). */
  onModelJobEvent(handler: (event: ModelJobEvent) => void): Promise<() => void>;
  /** Where an update stands, as it changes. */
  onUpdateEvent(handler: (event: UpdateEvent) => void): Promise<() => void>;
  /** Calls `handler` for every voice sample event. */
  onVoiceEvent(handler: (event: VoiceEvent) => void): Promise<() => void>;
  /** Calls `handler` for every audio check event. */
  onAudioCheckEvent(handler: (event: AudioCheckEvent) => void): Promise<() => void>;
  /** Calls `handler` for every benchmark event. */
  onBenchmarkEvent(handler: (event: BenchmarkEvent) => void): Promise<() => void>;
  /** Calls `handler` for every CUDA download event. */
  onCudaEvent(handler: (event: CudaEvent) => void): Promise<() => void>;
  /** Calls `handler` for what the tray, the shortcut and notifications ask of the window. */
  onShellEvent(handler: (event: ShellEvent) => void): Promise<() => void>;
}

/** The names of the generated commands; `IpcCommands` must have exactly these methods. */
export type CommandName = keyof typeof commands;

/** The event subscriptions of `Ipc` (not commands). */
export const EVENT_METHODS = [
  "onMeetingEvent",
  "onJobEvent",
  "onModelJobEvent",
  "onUpdateEvent",
  "onVoiceEvent",
  "onAudioCheckEvent",
  "onBenchmarkEvent",
  "onCudaEvent",
  "onShellEvent",
] as const;

// Compile-time check: a command added in Rust without an `Ipc` method (or the reverse)
// fails `tsc`.
type Missing =
  | Exclude<CommandName, keyof IpcCommands>
  | Exclude<keyof IpcCommands, CommandName>
  | Exclude<keyof Ipc, CommandName | (typeof EVENT_METHODS)[number]>;
export const everyCommandIsInIpc: [Missing] extends [never] ? true : never = true;

/** The error kind every command fails with when the database was saved by a newer Earshot
 * (`ipc::NEWER_DATA_KIND` in Rust): the app shows its own screen for it. */
export const NEWER_DATA_KIND = "newer_data";

/** A failed IPC call: carries the app's `IpcError`. */
export class IpcFailure extends Error {
  readonly error: IpcError;

  constructor(error: IpcError) {
    super(error.message);
    this.name = "IpcFailure";
    this.error = error;
  }
}
