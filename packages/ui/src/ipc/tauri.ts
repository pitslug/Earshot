// The Tauri implementation of `Ipc`: the only module that calls the generated bindings
// (and through them `@tauri-apps/api`).

import { commands, events } from "./bindings";
import { type Ipc, type IpcError, IpcFailure } from "./ipc";

type Result<T> = { status: "ok"; data: T } | { status: "error"; error: IpcError };

function unwrap<T>(result: Result<T>): T {
  if (result.status === "error") throw new IpcFailure(result.error);
  return result.data;
}

/** True when the page runs inside the Tauri webview. */
export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** `Ipc` over the app's Rust commands and events. */
export function createTauriIpc(): Ipc {
  return {
    appInfo: () => commands.appInfo(),
    listMeetings: async () => unwrap(await commands.listMeetings()),
    startMeeting: async (mode, title) => unwrap(await commands.startMeeting(mode, title)),
    stopMeeting: async () => {
      unwrap(await commands.stopMeeting());
    },
    keepRecording: async () => {
      await commands.keepRecording();
    },
    setCompact: async (compact) => {
      unwrap(await commands.setCompact(compact));
    },
    setWindowTheme: async (theme) => {
      unwrap(await commands.setWindowTheme(theme));
    },
    getMeeting: async (meeting) => unwrap(await commands.getMeeting(meeting)),
    renameSpeaker: async (meeting, speaker, name) => {
      unwrap(await commands.renameSpeaker(meeting, speaker, name));
    },
    mergeSpeakers: async (meeting, keep, merge) => {
      unwrap(await commands.mergeSpeakers(meeting, keep, merge));
    },
    linkSpeaker: async (meeting, speaker, choice) =>
      unwrap(await commands.linkSpeaker(meeting, speaker, choice)),
    renamePerson: async (person, name) => unwrap(await commands.renamePerson(person, name)),
    listPersons: async () => unwrap(await commands.listPersons()),
    rememberVoices: async (meeting, speakers) =>
      unwrap(await commands.rememberVoices(meeting, speakers)),
    forgetPersonVoices: async (person) => unwrap(await commands.forgetPersonVoices(person)),
    removePerson: async (person) => {
      unwrap(await commands.removePerson(person));
    },
    speakerSample: async (meeting, speaker) =>
      unwrap(await commands.speakerSample(meeting, speaker)),
    retranscribe: async (meeting) => {
      unwrap(await commands.retranscribe(meeting));
    },
    resummarise: async (meeting) => {
      unwrap(await commands.resummarise(meeting));
    },
    undoSummary: async (meeting) => {
      unwrap(await commands.undoSummary(meeting));
    },
    exportMeeting: async (meeting) => unwrap(await commands.exportMeeting(meeting)),
    deleteMeeting: async (meeting) => {
      unwrap(await commands.deleteMeeting(meeting));
    },
    hardware: async () => unwrap(await commands.hardware()),
    models: async () => unwrap(await commands.models()),
    modelSettings: async () => unwrap(await commands.modelSettings()),
    setModelSettings: async (settings) => unwrap(await commands.setModelSettings(settings)),
    startDownload: async (model) => {
      unwrap(await commands.startDownload(model));
    },
    cancelDownload: async (model) => {
      unwrap(await commands.cancelDownload(model));
    },
    removeModel: async (model) => unwrap(await commands.removeModel(model)),
    runCheck: async () => {
      unwrap(await commands.runCheck());
    },
    cancelCheck: async () => {
      unwrap(await commands.cancelCheck());
    },
    summarySettings: async () => unwrap(await commands.summarySettings()),
    setSummarySettings: async (settings) => unwrap(await commands.setSummarySettings(settings)),
    setApiKey: async (key) => {
      unwrap(await commands.setApiKey(key));
    },
    hasApiKey: async () => unwrap(await commands.hasApiKey()),
    claudeCodeStatus: async () => unwrap(await commands.claudeCodeStatus()),
    codexStatus: async () => unwrap(await commands.codexStatus()),
    storageSettings: async () => unwrap(await commands.storageSettings()),
    previewRetention: async (retention) => unwrap(await commands.previewRetention(retention)),
    setStorageSettings: async (retention) => unwrap(await commands.setStorageSettings(retention)),
    firstRunState: async () => unwrap(await commands.firstRunState()),
    finishFirstRun: async () => unwrap(await commands.finishFirstRun()),
    setModelDeclined: async (model, declined) => unwrap(await commands.setModelDeclined(model, declined)),
    summaryStyle: async () => unwrap(await commands.summaryStyle()),
    setSummaryStyle: async (style) => unwrap(await commands.setSummaryStyle(style)),
    audioSettings: async () => unwrap(await commands.audioSettings()),
    setAudioSettings: async (settings) => unwrap(await commands.setAudioSettings(settings)),
    defaultCaptureMode: async () => unwrap(await commands.defaultCaptureMode()),
    audioCheck: async () => unwrap(await commands.audioCheck()),
    startAudioCheck: async () => {
      unwrap(await commands.startAudioCheck());
    },
    cancelAudioCheck: async () => {
      await commands.cancelAudioCheck();
    },
    benchmark: async () => unwrap(await commands.benchmark()),
    startBenchmark: async () => {
      unwrap(await commands.startBenchmark());
    },
    cancelBenchmark: async () => {
      await commands.cancelBenchmark();
    },
    removeApiKey: async () => {
      unwrap(await commands.removeApiKey());
    },
    speakers: async () => unwrap(await commands.speakers()),
    setSpeakerTiming: async (timing) => unwrap(await commands.setSpeakerTiming(timing)),
    setMyName: async (name) => unwrap(await commands.setMyName(name)),
    startVoiceSample: async () => {
      unwrap(await commands.startVoiceSample());
    },
    cancelVoiceSample: async () => {
      await commands.cancelVoiceSample();
    },
    forgetMyVoice: async () => unwrap(await commands.forgetMyVoice()),
    obsidianSettings: async () => unwrap(await commands.obsidianSettings()),
    setObsidianSettings: async (settings) => unwrap(await commands.setObsidianSettings(settings)),
    pickObsidianVault: async (start) => unwrap(await commands.pickObsidianVault(start)),
    generalSettings: async () => unwrap(await commands.generalSettings()),
    setStartWithWindows: async (on) => unwrap(await commands.setStartWithWindows(on)),
    previewsSettings: async () => unwrap(await commands.previewsSettings()),
    setPreviews: async (on) => unwrap(await commands.setPreviews(on)),
    crashLogsFolder: () => commands.crashLogsFolder(),
    onVoiceEvent: (handler) => events.voiceEvent.listen((event) => handler(event.payload)),
    onAudioCheckEvent: (handler) => events.audioCheckEvent.listen((event) => handler(event.payload)),
    onBenchmarkEvent: (handler) => events.benchmarkEvent.listen((event) => handler(event.payload)),
    onShellEvent: (handler) => events.shellEvent.listen((event) => handler(event.payload)),
    onUpdateEvent: (handler) => events.updateEvent.listen((event) => handler(event.payload)),
    updateStatus: async () => unwrap(await commands.updateStatus()),
    checkForUpdate: async () => {
      unwrap(await commands.checkForUpdate());
    },
    downloadUpdate: async () => {
      unwrap(await commands.downloadUpdate());
    },
    cancelUpdate: async () => {
      unwrap(await commands.cancelUpdate());
    },
    setUpdateAutoCheck: async (on) => unwrap(await commands.setUpdateAutoCheck(on)),
    installUpdate: async () => {
      unwrap(await commands.installUpdate());
    },
    cudaSettings: async () => unwrap(await commands.cudaSettings()),
    setCudaSettings: async (value) => unwrap(await commands.setCudaSettings(value)),
    startCudaDownload: async (part, acceptedTerms) => {
      unwrap(await commands.startCudaDownload(part, acceptedTerms));
    },
    cancelCudaDownload: async (part) => {
      unwrap(await commands.cancelCudaDownload(part));
    },
    removeCudaPart: async (part) => unwrap(await commands.removeCudaPart(part)),
    openCudaTerms: async (terms) => {
      unwrap(await commands.openCudaTerms(terms));
    },
    restartApp: async () => {
      unwrap(await commands.restartApp());
    },
    onCudaEvent: (handler) => events.cudaEvent.listen((event) => handler(event.payload)),
    quitApp: async () => {
      unwrap(await commands.quitApp());
    },
    onJobEvent: (handler) => events.jobEvent.listen((event) => handler(event.payload)),
    onModelJobEvent: (handler) =>
      events.modelJobEvent.listen((event) => handler(event.payload)),
    onMeetingEvent: (handler) =>
      events.meetingEvent.listen((event) => handler(event.payload)),
  };
}
