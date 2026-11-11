export { IpcProvider, useIpc } from "./context";
export { EVENT_METHODS, IpcFailure, NEWER_DATA_KIND, everyCommandIsInIpc } from "./ipc";
export type {
  ActionView,
  AudioCheckEvent,
  AudioCheckOutcome,
  AudioCheckResultView,
  AudioCheckView,
  AudioPageView,
  AudioSettingsValue,
  ChosenDevice,
  DeviceItem,
  Citation,
  CitedText,
  DetailHeader,
  HandoverItem,
  KeptLocalView,
  ExportDone,
  JobEvent,
  JobKind,
  LinkChoice,
  MeetingDetail,
  NotesView,
  PersonItem,
  PersonRef,
  RememberOutcome,
  SavedVoiceResult,
  RowTrack,
  SearchMatch,
  SearchResultItem,
  SearchResultsView,
  SpeakerCard,
  SpeakerKind,
  SpeakerSource,
  SummaryFieldRef,
  SummaryView,
  TranscriptKind,
  TranscriptRow,
  DictionaryWordAdded,
  AppInfo,
  CaptureMode,
  CommandName,
  EndedReason,
  Ipc,
  IpcCommands,
  IpcError,
  LiveTrack,
  MeetingEvent,
  MeetingListItem,
  MeetingState,
  PostStep,
  SummaryEvent,
  TranscriptState,
  WarningKind,
  CheckView,
  ChoiceKind,
  ChoiceView,
  GpuView,
  HardwareView,
  InUseView,
  Installed,
  ModelItem,
  ModelJobEvent,
  UpdateEvent,
  UpdatePhase,
  UpdateView,
  ModelSettingsValue,
  ModelSettingsView,
  ModelUse,
  ModelsView,
  NpuView,
  PartChoice,
  RecommendedView,
  RemovedView,
  RunsOn,
  Verdict,
  ClaudeCodeState,
  ClaudeCodeStatusView,
  CodexState,
  CodexStatusView,
  EffortChoice,
  ProviderChoice,
  SummarySettingsValue,
  SummarySettingsView,
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
} from "./ipc";
export {
  POST_STEPS,
  CATCH_UP_MS,
  POST_STEP_MS,
  SAMPLE_NOW,
  SCRIPT_LINES,
  SILENT_WAV_BASE64,
  SCRIPT_WARNING_AT,
  createMockIpc,
  sampleMeetings,
} from "./mock";
export type { MockIpcData, ScriptLine } from "./mock";
export { MISSING_SEGMENT, SAMPLE_SPEAKERS, sampleDetail, samplePersons } from "./mockDetail";
export { MOCK_FALLBACK_MODEL, MODEL_STEP_MS, createModelsMock, sampleHardware } from "./mockModels";
export type { ModelsMock, SampleMachine } from "./mockModels";
export {
  CLAUDE_CODE_NOT_INSTALLED,
  CLAUDE_CODE_STATUS,
  CODEX_NOT_INSTALLED,
  CODEX_STATUS,
  MAX_STYLE_CHARS,
  MAX_VOCABULARY_CHARS,
  MAX_VOCABULARY_WORDS,
  MAX_VOCABULARY_WORD_CHARS,
  MOCK_ANTHROPIC_URL,
  createSummariesMock,
} from "./mockSummaries";
export type { SummariesMock, SummariesMockData } from "./mockSummaries";
export { SOME_LEFT_MESSAGE, createStorageMock } from "./mockStorage";
export type { MockKept, StorageMockData } from "./mockStorage";
export { RETENTION_CHOICES } from "./retentionChoices";
export type { RetentionCount, RetentionOption, StorageCommands, StorageSaved, StorageView } from "./storage";
export type {
  BenchmarkCommands,
  BenchmarkEvent,
  BenchmarkPhase,
  BenchmarkResultView,
  BenchmarkRow,
  BenchmarkRunning,
  BenchmarkView,
} from "./benchmark";
export { BENCHMARK_MEETING_MS, BENCHMARK_STEP_MS, MOCK_BENCHMARK_FAILURE, createBenchmarkMock, mockBenchmarkResult } from "./mockBenchmark";
export type { BenchmarkMockData, MockBenchmarkEnd } from "./mockBenchmark";
export type { SummaryStyleCommands, SummaryStyleView } from "./summaryStyle";
export type { SummaryVocabularyCommands, SummaryVocabularyView } from "./summaryVocabulary";
export type { FirstRunCommands, FirstRunView } from "./firstRun";
export type { FolderCommands, FolderDeleted, FolderListItem } from "./folders";
export { SAMPLE_FOLDERS } from "./mockFolders";
export type { FoldersMockData } from "./mockFolders";
export type {
  CudaCommands,
  CudaEvent,
  CudaPartChoice,
  CudaPartView,
  CudaRemoved,
  CudaRunView,
  CudaSwitchValue,
  CudaTerms,
  CudaView,
  TermsLink,
} from "./cuda";
export { CUDA_STEP_MS, createCudaMock } from "./mockCuda";
export type { CudaMockData } from "./mockCuda";
export { createTauriIpc, isTauri } from "./tauri";
