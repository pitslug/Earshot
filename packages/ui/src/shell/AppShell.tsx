// The app window (docs/ui-design.md "Shape"): the meeting list on the left, the selected
// meeting, the meeting screen (T7.2) or the empty state on the right; while recording it
// can shrink to the compact listening card. A meeting that has ended shows its own page, with
// how its finishing goes at the top (T12.28). "Settings" replaces the window with the
// Settings screen (T7.4f) until "Back to meetings".

import {
  Badge,
  Body1,
  Button,
  Caption1,
  Caption1Strong,
  Dialog,
  DialogActions,
  DialogBody,
  DialogContent,
  DialogSurface,
  DialogTitle,
  Menu,
  MenuDivider,
  MenuItem,
  MenuItemRadio,
  MenuList,
  MenuPopover,
  MenuTrigger,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  Radio,
  RadioGroup,
  Spinner,
  Title2,
  makeStyles,
  mergeClasses,
  tokens,
  useFocusFinders,
  useRestoreFocusTarget,
} from "@fluentui/react-components";
import { type ReactNode, useCallback, useEffect, useId, useRef, useState } from "react";

import {
  type CaptureMode,
  DATA_FOLDER_KIND,
  type FolderListItem,
  IpcFailure,
  type JobKind,
  type MeetingEvent,
  type MeetingListItem,
  NEWER_DATA_KIND,
  type RecoveredRecording,
  useIpc,
} from "../ipc";
import {
  CompactCard,
  MeetingStatus,
  NewMeetingView,
  RecordingView,
} from "../meeting/MeetingViews";
import { failureText } from "../detail/detail";
import { type JobView, MeetingDetailView, isTerminal } from "../detail/MeetingDetailView";
import { FirstRunWizard, NO_MODEL_NOTE } from "../firstRun/FirstRunWizard";
import { useLiveMeeting, useLocalDay } from "../meeting/hooks";
import { type SettingsSection, SettingsView } from "../settings/SettingsView";
import { useThemeChoice } from "../theme/EarshotThemeProvider";
import { type Timer, realTimer } from "../time";
import { MeetingSearchBox, MeetingSearchResults, useMeetingSearch } from "./MeetingSearch";
import { ALL_MEETINGS_KEY, type FolderEdit, FolderGroup, NEW_FOLDER_KEY } from "./Folders";
import { folderDeletionCopy, meetingCount, meetingsInFolder } from "./folderRules";
import { type DayGroup, displayTitle, groupByDay, recoveredText, secondaryLine } from "./meetings";
import { openMenuFromKeyboard } from "./menuKeys";
import { DataFolderScreen } from "./DataFolderScreen";
import { NewerDataScreen } from "./NewerDataScreen";

const useStyles = makeStyles({
  root: {
    display: "flex",
    height: "100%",
    minHeight: 0,
  },
  pane: {
    display: "flex",
    flexDirection: "column",
    width: "240px",
    flexShrink: 0,
    borderRight: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    backgroundColor: tokens.colorNeutralBackground2,
  },
  paneTop: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    padding: tokens.spacingVerticalM,
    paddingLeft: tokens.spacingHorizontalM,
    paddingRight: tokens.spacingHorizontalM,
  },
  fullWidth: {
    width: "100%",
  },
  listArea: {
    flexGrow: 1,
    overflowY: "auto",
    paddingLeft: tokens.spacingHorizontalS,
    paddingRight: tokens.spacingHorizontalS,
  },
  list: {
    listStyleType: "none",
    margin: 0,
    padding: 0,
  },
  group: {
    marginBottom: tokens.spacingVerticalM,
  },
  groupHeading: {
    display: "block",
    margin: 0,
    paddingTop: tokens.spacingVerticalXS,
    paddingBottom: tokens.spacingVerticalXS,
    paddingLeft: tokens.spacingHorizontalS,
    color: tokens.colorNeutralForeground3,
  },
  wizardFrame: {
    display: "flex",
    flexDirection: "column",
    height: "100%",
    minHeight: 0,
  },
  wizardBody: {
    flexGrow: 1,
    minHeight: 0,
  },
  // The row's trash can stays out of the way until the pointer or keyboard focus is on the row.
  // A touch screen has no hover, so it always shows there.
  rowItem: {
    position: "relative",
    ":hover .earshot-row-delete": { opacity: 1, transform: "translateX(0)" },
    ":has(:focus-visible) .earshot-row-delete": { opacity: 1, transform: "translateX(0)" },
    ":hover .earshot-row-title": { paddingRight: "24px" },
    ":has(:focus-visible) .earshot-row-title": { paddingRight: "24px" },
    "@media (hover: none)": {
      "& .earshot-row-delete": { opacity: 1, transform: "translateX(0)" },
      "& .earshot-row-title": { paddingRight: "24px" },
    },
  },
  rowDelete: {
    position: "absolute",
    top: tokens.spacingVerticalXS,
    right: tokens.spacingHorizontalXS,
    opacity: 0,
    transform: "translateX(8px)",
    transitionProperty: "opacity, transform",
    transitionDuration: tokens.durationFaster,
    "@media (prefers-reduced-motion: reduce)": { transitionDuration: "0.01ms" },
  },
  row: {
    width: "100%",
    height: "auto",
    justifyContent: "flex-start",
    textAlign: "left",
    paddingTop: tokens.spacingVerticalS,
    paddingBottom: tokens.spacingVerticalS,
    paddingLeft: tokens.spacingHorizontalS,
    paddingRight: tokens.spacingHorizontalS,
  },
  rowSelected: {
    backgroundColor: tokens.colorSubtleBackgroundSelected,
  },
  rowText: {
    display: "flex",
    flexDirection: "column",
    minWidth: 0,
    width: "100%",
  },
  rowTitle: {
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
  },
  rowMeta: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalXS,
    color: tokens.colorNeutralForeground3,
  },
  recordingDot: {
    width: "8px",
    height: "8px",
    borderRadius: tokens.borderRadiusCircular,
    backgroundColor: tokens.colorStatusDangerBackground3,
    flexShrink: 0,
  },
  paneBottom: {
    padding: tokens.spacingVerticalS,
    borderTop: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
  },
  settings: {
    width: "100%",
    justifyContent: "flex-start",
  },
  updateMark: {
    display: "inline-block",
    width: "8px",
    height: "8px",
    marginLeft: tokens.spacingHorizontalS,
    borderRadius: tokens.borderRadiusCircular,
    backgroundColor: tokens.colorBrandBackground,
    flexShrink: 0,
  },
  detail: {
    flexGrow: 1,
    minWidth: 0,
    display: "flex",
    flexDirection: "column",
    padding: tokens.spacingHorizontalXXL,
    gap: tokens.spacingVerticalM,
    overflowY: "auto",
    // Keyboard focus never lands behind the meeting's sticky header and tabs.
    scrollPaddingTop: "144px",
    // Covers the top padding while the pane scrolls, so nothing shows above a sticky header.
    "::before": {
      content: '""',
      display: "block",
      position: "sticky",
      top: `calc(-1 * ${tokens.spacingHorizontalXXL})`,
      zIndex: 1,
      flexShrink: 0,
      height: tokens.spacingHorizontalXXL,
      marginTop: `calc(-1 * ${tokens.spacingHorizontalXXL})`,
      marginBottom: `calc(-1 * ${tokens.spacingVerticalM})`,
      backgroundColor: tokens.colorNeutralBackground1,
    },
  },
  notice: {
    alignSelf: "flex-start",
  },
  heading: {
    margin: 0,
  },
  meta: {
    color: tokens.colorNeutralForeground3,
  },
  emptyFolder: {
    display: "block",
    paddingLeft: tokens.spacingHorizontalS,
    color: tokens.colorNeutralForeground3,
  },
  empty: {
    flexGrow: 1,
    display: "flex",
    flexDirection: "column",
    alignItems: "center",
    justifyContent: "center",
    gap: tokens.spacingVerticalM,
    textAlign: "center",
  },
});

export interface AppShellProps {
  /** The clock used to group meetings by day (default: the timer's). */
  now?: () => number;
  /** The clock and timers of the meeting screen (injected so tests never use real time). */
  timer?: Timer;
  /** A small notice shown at the top of the meeting pane (the browser preview's badge). */
  notice?: ReactNode;
  /** Also told when "Settings" opens the Settings screen. */
  onOpenSettings?: () => void;
  /** Also told when a notification's "Name voices" opens the meeting's Speakers panel. */
  onNameVoices?: (meetingId: string) => void;
}

type Load =
  | { status: "loading" }
  | { status: "failed"; message: string }
  // The database was saved by a newer Earshot: nothing can be read until it is installed.
  | { status: "newer" }
  // The data folder could not be opened at the start (T9.23): Try again restarts Earshot.
  | { status: "dataFolder"; message: string }
  | { status: "loaded"; meetings: MeetingListItem[]; folders: FolderListItem[] };

/** A problem with a folder change, shown above the meeting pane until dismissed. */
interface FolderNotice {
  title: string;
  text: string;
}

/** What the right-hand pane shows. */
type View = "detail" | "start" | "meeting" | "settings" | "first-run";

const noop = () => {};

/** What "Quit Earshot" says during a meeting. */
const QUIT_COPY = {
  recording: {
    title: "Quit while recording?",
    body: "Earshot stops the recording, saves it and quits. The final transcript and summary are not made now; you can start them from the meeting later.",
    keep: "Keep recording",
    quit: "Stop and quit",
  },
  finishing: {
    title: "Quit before the summary is ready?",
    body: "The recording is saved. The final transcript and summary stop here; you can start them from the meeting later.",
    keep: "Keep working",
    quit: "Quit",
  },
} as const;

/** Events after which the meeting list is read again. */
const RELOAD_ON: MeetingEvent["type"][] = ["Started", "Ended", "Summary"];

/** The app window: meeting list and the selected meeting or the meeting screen. */
export function AppShell({
  timer = realTimer,
  now = timer.now,
  notice,
  onOpenSettings = noop,
  onNameVoices = noop,
}: AppShellProps) {
  // Holds a tabster reference for the life of the window. Tabster (Fluent's focus and modal
  // library) is reference-counted by the components that use it and is disposed when the last
  // one unmounts, without undoing the aria-hidden its open modal put on the page. Deleting a
  // meeting unmounts the whole meeting view, its confirmation dialog included, so without
  // this the page stayed hidden from screen readers after a delete.
  useFocusFinders();
  const styles = useStyles();
  const ipc = useIpc();
  const [load, setLoad] = useState<Load>({ status: "loading" });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [view, setView] = useState<View>("detail");
  // Whether the app has said if the first-run wizard shows, so the meeting list does not flash
  // up before it.
  const [firstRunKnown, setFirstRunKnown] = useState(false);
  const [settingsSection, setSettingsSection] = useState<SettingsSection | undefined>(undefined);
  const [setupFromSettings, setSetupFromSettings] = useState(false);
  // The wizard ended with no transcription model: said once it is gone, until dismissed.
  const [noModelNote, setNoModelNote] = useState(false);
  const [compact, setCompact] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [stopError, setStopError] = useState<string | null>(null);
  // Re-transcribe or Re-summarise asked for on the finished meeting's page (T9.23).
  const [retrying, setRetrying] = useState(false);
  const [retryError, setRetryError] = useState<string | null>(null);
  // The ended meeting whose result the user dismissed (or answered with a job) on its page.
  const [resultDismissed, setResultDismissed] = useState<string | null>(null);
  // Change when the ended meeting's final transcript or summary is stored, so its page reads it
  // again (and shows a stored summary).
  const [detailRefresh, setDetailRefresh] = useState(0);
  const [summaryStored, setSummaryStored] = useState(0);
  // The meeting a Cancel was pressed for, until its result: a newer meeting differs.
  const [cancellingId, setCancellingId] = useState<string | null>(null);
  // An older meeting still finishing behind a newer recording, until its result. The meeting
  // screen follows the newer one, so its Cancel is offered here.
  const [behindId, setBehindId] = useState<string | null>(null);
  // The last meeting the screen followed, kept across the reset a new start makes.
  const lastMeeting = useRef<{ id: string; finishing: boolean } | null>(null);
  const [startError, setStartError] = useState<string | null>(null);
  // Recordings Earshot was closed during whose audio was saved at this start (T9.18), each told
  // once: the app hands each over one time only, so a result is never dropped.
  const [recovered, setRecovered] = useState<RecoveredRecording[]>([]);
  const [recoveredAsk, setRecoveredAsk] = useState(0);
  const [quitAsked, setQuitAsked] = useState<keyof typeof QUIT_COPY | null>(null);
  const [quitting, setQuitting] = useState(false);
  const [quitError, setQuitError] = useState<string | null>(null);
  const [reload, setReload] = useState(0);
  const [nameVoices, setNameVoices] = useState(0);
  // The meeting the delete dialog asks about, and why a delete was refused.
  const [deleting, setDeleting] = useState<MeetingListItem | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  // A delete on its way: the ref refuses a second click before the buttons have re-rendered (the
  // second call would fail on a meeting now gone and report a delete that worked as refused).
  const [deletePending, setDeletePending] = useState(false);
  const deleteRunning = useRef(false);
  // A deleted row took keyboard focus with it: focus moves on once the list is read again.
  const refocusAfterDelete = useRef<string | null>(null);
  const newMeetingButton = useRef<HTMLButtonElement | null>(null);
  const rowButtons = useRef(new Map<string, HTMLButtonElement>());
  // The folder shown: "All meetings" (`null`) each time the app opens, never stored.
  const [selectedFolder, setSelectedFolder] = useState<string | null>(null);
  const [folderEdit, setFolderEdit] = useState<FolderEdit | null>(null);
  // The folder the delete dialog asks about, and the choice in it (keeping the meetings is the
  // default).
  const [deletingFolder, setDeletingFolder] = useState<FolderListItem | null>(null);
  const [deleteFolderMeetings, setDeleteFolderMeetings] = useState(false);
  const [folderDeletePending, setFolderDeletePending] = useState(false);
  const folderDeleteRunning = useRef(false);
  const [folderNotice, setFolderNotice] = useState<FolderNotice | null>(null);
  const folderButtons = useRef(new Map<string, HTMLButtonElement>());
  // Where focus goes once the folders are drawn again: a `folderButtons` key or `meeting:<id>`.
  const focusAfterFolders = useRef<string | null>(null);
  const refresh = useCallback(() => setReload((n) => n + 1), []);
  const search = useMeetingSearch(ipc, timer, reload);
  // Jobs on stored meetings, followed here so an ending is seen whatever meeting is shown.
  const [jobs, setJobs] = useState<Record<string, JobView>>({});
  const jobSeq = useRef(0);
  // Whether an update waits for the user (available, downloading or ready): a mark beside Settings.
  const [updateWaiting, setUpdateWaiting] = useState(false);

  useEffect(() => {
    ipc.recoveredRecordings().then(
      (r) => {
        if (r.length === 0) return;
        setRecovered((all) => [...all, ...r]);
        refresh();
      },
      () => {},
    );
  }, [ipc, recoveredAsk, refresh]);

  useEffect(() => {
    let live = true;
    const waiting = (state: string) => state === "Available" || state === "Downloading" || state === "Ready";
    ipc.updateStatus().then(
      (v) => {
        if (live) setUpdateWaiting(waiting(v.phase.state));
      },
      () => {},
    );
    const stop = ipc.onUpdateEvent((e) => {
      if (live) setUpdateWaiting(waiting(e.phase.state));
    });
    return () => {
      live = false;
      void stop.then((f) => f());
    };
  }, [ipc]);

  useEffect(() => {
    let live = true;
    let stop: (() => void) | null = null;
    void ipc
      .onJobEvent((e) => {
        if (!live) return;
        jobSeq.current += 1;
        const seq = jobSeq.current;
        setJobs((all) => {
          const kind =
            all[e.meeting]?.job ??
            (e.type === "Failed" ? e.job : e.type === "Summary" ? "Resummarise" : "Retranscribe");
          return { ...all, [e.meeting]: { job: kind, last: e, seq } };
        });
        if (isTerminal(e)) refresh();
      })
      .then((s) => {
        if (live) stop = s;
        else s();
      });
    return () => {
      live = false;
      stop?.();
    };
  }, [ipc, refresh]);

  useEffect(() => {
    let current = true;
    // A state that cannot be read never keeps anyone from their meetings.
    const known = (show: boolean) => {
      if (!current) return;
      setFirstRunKnown(true);
      // A meeting the tray or the shortcut already started keeps the window.
      if (show) setView((v) => (v === "detail" ? "first-run" : v));
    };
    ipc.firstRunState().then(
      (s) => known(s.show),
      () => known(false),
    );
    return () => {
      current = false;
    };
  }, [ipc]);

  const goCompact = useCallback(
    (on: boolean) => {
      setCompact(on);
      ipc.setCompact(on).catch(() => setCompact(false));
    },
    [ipc],
  );

  const liveMeetingId = useRef<string | null>(null);
  const live = useLiveMeeting(ipc, timer, (event) => {
    if (RELOAD_ON.includes(event.type)) setReload((n) => n + 1);
    if (event.type === "Started") {
      const older = lastMeeting.current;
      if (older !== null && older.finishing && older.id !== event.meeting) setBehindId(older.id);
      setSelectedId(event.meeting);
      // Also a meeting the tray or the shortcut started.
      setView("meeting");
      setStartError(null);
    }
    // An older meeting's end says nothing about the stop button of this one.
    if (event.type === "Ended" && event.meeting === liveMeetingId.current) setStopping(false);
    // The page of the meeting that has ended reads it again when its finishing stored something.
    if (
      event.type === "PostProgress" &&
      event.step.kind === "Stored" &&
      event.meeting === liveMeetingId.current
    ) {
      setDetailRefresh((n) => n + 1);
    }
    if (event.type === "Summary" && event.meeting === liveMeetingId.current) {
      setDetailRefresh((n) => n + 1);
      if (event.result.kind === "Stored") setSummaryStored((n) => n + 1);
    }
    if (event.type === "Summary") {
      setBehindId((id) => (id === event.meeting ? null : id));
      setCancellingId((id) => (id === event.meeting ? null : id));
    }
  });
  const meeting = live.meeting;
  liveMeetingId.current = meeting?.meetingId ?? null;
  if (meeting !== null) {
    lastMeeting.current = { id: meeting.meetingId, finishing: meeting.phase === "processing" };
  }
  const recording = meeting !== null && meeting.phase === "recording";
  // The list groups by day: it renders when the day changes, never on a clock tick (T9.24).
  const today = useLocalDay(timer, now);

  // The native title bar follows the theme chosen in Settings. A title bar that stays on the
  // system's colours changes nothing else, so a refusal is not worth a banner.
  const { choice: themeChoice } = useThemeChoice();
  useEffect(() => {
    ipc.setWindowTheme(themeChoice === "system" ? null : themeChoice).catch(noop);
  }, [ipc, themeChoice]);

  useEffect(() => {
    let current = true;
    Promise.all([ipc.listMeetings(), ipc.listFolders()]).then(
      ([meetings, folders]) => {
        if (!current) return;
        setLoad({ status: "loaded", meetings, folders });
        setSelectedId((sel) =>
          sel !== null && meetings.some((m) => m.id === sel) ? sel : (meetings[0]?.id ?? null),
        );
        // A folder deleted (here or by another device) shows "All meetings" again.
        setSelectedFolder((f) => (f !== null && folders.some((x) => x.id === f) ? f : null));
      },
      (error: unknown) => {
        if (!current) return;
        if (error instanceof IpcFailure && error.error.kind === NEWER_DATA_KIND) {
          setLoad({ status: "newer" });
          return;
        }
        if (error instanceof IpcFailure && error.error.kind === DATA_FOLDER_KIND) {
          setLoad({ status: "dataFolder", message: error.error.message });
          return;
        }
        const message =
          error instanceof IpcFailure || error instanceof Error ? error.message : String(error);
        setLoad({ status: "failed", message });
      },
    );
    return () => {
      current = false;
    };
  }, [ipc, reload]);

  // The compact card is for recording only: once the recording ends, the full window
  // returns to show the progress and the summary.
  useEffect(() => {
    if (compact && meeting?.phase !== "recording") goCompact(false);
  }, [compact, meeting?.phase, goCompact]);

  // Once the recording ends, whoever was watching it sees the meeting's own page: its draft
  // transcript at once, how its finishing goes at the top, then its summary (T12.28).
  const endedId = meeting !== null && meeting.phase !== "recording" ? meeting.meetingId : null;
  useEffect(() => {
    if (view !== "meeting" || endedId === null) return;
    setSelectedId(endedId);
    setView("detail");
  }, [view, endedId]);

  const meetings = load.status === "loaded" ? load.meetings : [];
  const folders = load.status === "loaded" ? load.folders : [];
  const visible = meetingsInFolder(meetings, selectedFolder);
  const selected = meetings.find((m) => m.id === selectedId) ?? null;

  // A deleted row, or one moved out of the folder shown, took keyboard focus with it.
  useEffect(() => {
    const gone = refocusAfterDelete.current;
    if (gone === null || load.status !== "loaded" || visible.some((m) => m.id === gone)) return;
    refocusAfterDelete.current = null;
    const row = selectedId !== null ? rowButtons.current.get(selectedId) : undefined;
    (row ?? newMeetingButton.current)?.focus();
  });

  // After a folder input closes or a folder is deleted, focus goes where it makes sense.
  useEffect(() => {
    const target = focusAfterFolders.current;
    if (target === null) return;
    const el = target.startsWith("meeting:")
      ? rowButtons.current.get(target.slice("meeting:".length))
      : (folderButtons.current.get(target) ??
        (target === ALL_MEETINGS_KEY ? folderButtons.current.get(NEW_FOLDER_KEY) : undefined));
    if (el === undefined) return;
    focusAfterFolders.current = null;
    el.focus();
  });

  // Only a recording meeting keeps the next one out; one that is finishing goes on behind it.
  const newMeeting = () => setView(meeting?.phase === "recording" ? "meeting" : "start");
  const start = async (mode: CaptureMode, title: string | null, keepLocal: boolean) => {
    live.reset();
    setRetryError(null);
    setStopError(null);
    const id = await ipc.startMeeting(mode, title, keepLocal);
    setSelectedId(id);
    setView("meeting");
  };
  const stop = () => {
    setStopping(true);
    setStopError(null);
    ipc.stopMeeting().catch((e: unknown) => {
      setStopping(false);
      setStopError(e instanceof Error ? e.message : String(e));
    });
  };
  const cancelFinishing = (id: string) => {
    setCancellingId(id);
    setStopError(null);
    ipc.cancelFinishing(id).catch((e: unknown) => {
      setCancellingId(null);
      setStopError(e instanceof Error ? e.message : String(e));
    });
  };
  // Only a recording meeting has its own screen; one that has ended shows its page.
  const select = (id: string) => {
    setSelectedId(id);
    setView(meeting !== null && meeting.meetingId === id && recording ? "meeting" : "detail");
  };
  const open = (id: string) => {
    setSelectedId(id);
    setView("detail");
  };
  // A result can outlast its meeting: reading the list again takes the result away too.
  const pickResult = (id: string) => {
    if (meetings.some((m) => m.id === id)) select(id);
    else refresh();
  };
  // The job starts here and the meeting's page shows its progress, as its own buttons do.
  const startAfterMeetingJob = async (id: string, job: JobKind) => {
    setRetrying(true);
    setRetryError(null);
    try {
      if (job === "Retranscribe") await ipc.retranscribe(id);
      else await ipc.resummarise(id);
      jobSeq.current += 1;
      const seq = jobSeq.current;
      setJobs((all) => ({ ...all, [id]: { job, last: null, seq } }));
      // The job's own notice on the page takes over from the result.
      setResultDismissed(id);
      open(id);
    } catch (e: unknown) {
      setRetryError(e instanceof Error ? e.message : String(e));
    } finally {
      setRetrying(false);
    }
  };
  const openNames = (id: string) => {
    open(id);
    setNameVoices((n) => n + 1);
    onNameVoices(id);
  };
  const confirmDelete = async (item: MeetingListItem) => {
    if (deleteRunning.current) return;
    deleteRunning.current = true;
    setDeletePending(true);
    try {
      await ipc.deleteMeeting(item.id);
      setDeleting(null);
      setDeleteError(null);
      refocusAfterDelete.current = item.id;
      setSelectedId((sel) => (sel === item.id ? null : sel));
      // The finished meeting's own screen has nothing left to show.
      if (meeting?.meetingId === item.id) {
        live.reset();
        setView((v) => (v === "meeting" ? "detail" : v));
      }
      refresh();
    } catch (e) {
      setDeleting(null);
      setDeleteError(failureText(e));
    } finally {
      deleteRunning.current = false;
      setDeletePending(false);
    }
  };

  const submitFolderName = async (name: string): Promise<string | null> => {
    const edit = folderEdit;
    if (edit === null) return null;
    try {
      if (edit.kind === "new") {
        const id = await ipc.createFolder(name);
        if (edit.moveMeeting !== null) {
          try {
            await ipc.moveToFolder(edit.moveMeeting, id);
          } catch (e) {
            setFolderNotice({ title: "The meeting was not moved", text: failureText(e) });
          }
        }
        focusAfterFolders.current = id;
      } else {
        await ipc.renameFolder(edit.folder.id, name);
        focusAfterFolders.current = edit.folder.id;
      }
      setFolderEdit(null);
      refresh();
      return null;
    } catch (e) {
      return failureText(e);
    }
  };
  const cancelFolderEdit = () => {
    const edit = folderEdit;
    if (edit !== null) {
      focusAfterFolders.current =
        edit.kind === "rename"
          ? edit.folder.id
          : edit.moveMeeting !== null
            ? `meeting:${edit.moveMeeting}`
            : NEW_FOLDER_KEY;
    }
    setFolderEdit(null);
  };
  const moveMeeting = async (item: MeetingListItem, folder: string | null) => {
    try {
      await ipc.moveToFolder(item.id, folder);
      if (selectedFolder !== null && folder !== selectedFolder) refocusAfterDelete.current = item.id;
      refresh();
    } catch (e) {
      setFolderNotice({ title: "The meeting was not moved", text: failureText(e) });
    }
  };
  const askDeleteFolder = (folder: FolderListItem) => {
    setDeleteFolderMeetings(false);
    setDeletingFolder(folder);
  };
  const confirmDeleteFolder = async (folder: FolderListItem) => {
    if (folderDeleteRunning.current) return;
    folderDeleteRunning.current = true;
    setFolderDeletePending(true);
    try {
      const done = deleteFolderMeetings
        ? await ipc.deleteFolderWithMeetings(folder.id)
        : await ipc.deleteFolderKeepingMeetings(folder.id);
      setDeletingFolder(null);
      const left = folderDeletionCopy(done);
      setFolderNotice(left === null ? null : { title: "The folder was not deleted", text: left });
      if (done.folder_deleted) {
        setSelectedFolder((f) => (f === folder.id ? null : f));
        focusAfterFolders.current = ALL_MEETINGS_KEY;
      }
      refresh();
    } catch (e) {
      setDeletingFolder(null);
      setFolderNotice({ title: "The folder was not deleted", text: failureText(e) });
    } finally {
      folderDeleteRunning.current = false;
      setFolderDeletePending(false);
    }
  };
  const deletingCount =
    deletingFolder === null ? 0 : meetings.filter((m) => m.folder_id === deletingFolder.id).length;

  // What the tray, the shortcut and notifications ask of the window. The latest handlers are
  // read through a ref so the subscription lasts the window's life.
  const askRecovered = () => setRecoveredAsk((n) => n + 1);
  const shellHandlers = useRef({ open, openNames, goCompact, askRecovered });
  shellHandlers.current = { open, openNames, goCompact, askRecovered };
  useEffect(() => {
    let current = true;
    let stopListening: (() => void) | null = null;
    void ipc
      .onShellEvent((e) => {
        if (!current) return;
        const h = shellHandlers.current;
        switch (e.type) {
          case "OpenMeeting":
            h.goCompact(false);
            (e.name_voices ? h.openNames : h.open)(e.meeting);
            break;
          case "QuitAsked":
            h.goCompact(false);
            setQuitError(null);
            setQuitAsked(e.recording ? "recording" : "finishing");
            break;
          case "RecordingsRecovered":
            h.askRecovered();
            break;
          case "StartFailed":
            setStartError(e.message);
            // The wizard stays, whatever step it is on: it says why above itself.
            setView((v) => (v === "first-run" ? v : "detail"));
            break;
        }
      })
      .then((s) => {
        if (current) stopListening = s;
        else s();
      });
    return () => {
      current = false;
      stopListening?.();
    };
  }, [ipc]);

  const quit = () => {
    setQuitting(true);
    setQuitError(null);
    ipc.quitApp().catch((e: unknown) => {
      setQuitting(false);
      setQuitError(e instanceof Error ? e.message : String(e));
    });
  };
  const quitDialog = quitAsked !== null && (
    <Dialog open modalType="alert">
      <DialogSurface>
        <DialogBody>
          <DialogTitle>{QUIT_COPY[quitAsked].title}</DialogTitle>
          <DialogContent>
            {quitting ? <Spinner label="Saving and quitting" /> : QUIT_COPY[quitAsked].body}
            {quitError !== null && (
              <MessageBar intent="error">
                <MessageBarBody>{quitError}</MessageBarBody>
              </MessageBar>
            )}
          </DialogContent>
          <DialogActions>
            <Button appearance="secondary" disabled={quitting} onClick={() => setQuitAsked(null)}>
              {QUIT_COPY[quitAsked].keep}
            </Button>
            <Button appearance="primary" disabled={quitting} onClick={quit}>
              {QUIT_COPY[quitAsked].quit}
            </Button>
          </DialogActions>
        </DialogBody>
      </DialogSurface>
    </Dialog>
  );

  // Only the offer closes: the recording goes on, so a failure here costs nothing.
  const keepRecording = () => {
    live.closeCallEndOffer();
    ipc.keepRecording().catch(() => {});
  };

  if (compact && meeting !== null && meeting.phase === "recording") {
    return (
      <CompactCard
        meeting={meeting}
        timer={timer}
        levels={live.levels}
        onStop={stop}
        onExpand={() => goCompact(false)}
        onKeepRecording={keepRecording}
        stopping={stopping}
      />
    );
  }

  if (!firstRunKnown) return quitDialog || null;

  if (view === "first-run") {
    return (
      <div className={styles.wizardFrame}>
        {startError !== null && (
          <MessageBar intent="error" layout="multiline">
            <MessageBarBody>
              <MessageBarTitle>The meeting could not start</MessageBarTitle>
              {startError}
            </MessageBarBody>
            <MessageBarActions>
              <Button size="small" appearance="transparent" onClick={() => setStartError(null)}>
                Dismiss
              </Button>
            </MessageBarActions>
          </MessageBar>
        )}
        <div className={styles.wizardBody}>
          <FirstRunWizard
            onDone={(noModel) => {
              refresh();
              setNoModelNote(noModel);
              setView(setupFromSettings ? "settings" : recording ? "meeting" : "detail");
            }}
          />
        </div>
        {quitDialog}
      </div>
    );
  }

  const noModelBar = noModelNote && (
    <MessageBar intent="warning" layout="multiline">
      <MessageBarBody>{NO_MODEL_NOTE}</MessageBarBody>
      <MessageBarActions>
        <Button size="small" appearance="transparent" onClick={() => setNoModelNote(false)}>
          Dismiss
        </Button>
      </MessageBarActions>
    </MessageBar>
  );

  if (view === "settings") {
    const settings = (
      <SettingsView
        initialSection={settingsSection}
        onBack={() => setView(recording ? "meeting" : "detail")}
        onOpenSetup={() => {
          setSetupFromSettings(true);
          setSettingsSection("General");
          setView("first-run");
        }}
      />
    );
    return noModelNote ? (
      <div className={styles.wizardFrame}>
        {noModelBar}
        <div className={styles.wizardBody}>{settings}</div>
        {quitDialog}
      </div>
    ) : (
      <>
        {settings}
        {quitDialog}
      </>
    );
  }

  if (load.status === "newer") {
    return (
      <>
        <NewerDataScreen
          onCheckForUpdates={() => {
            setSettingsSection("General");
            setView("settings");
            onOpenSettings();
          }}
        />
        {quitDialog}
      </>
    );
  }

  if (load.status === "dataFolder") {
    return (
      <>
        <DataFolderScreen message={load.message} onTryAgain={() => ipc.restartApp()} />
        {quitDialog}
      </>
    );
  }

  const showMeeting = view === "meeting";
  // The meeting that has ended, for its own page: how its finishing goes and what came of it.
  const ended =
    meeting !== null && meeting.phase !== "recording"
      ? {
          id: meeting.meetingId,
          finishing: meeting.summary === null,
          status: (
            <MeetingStatus
              meeting={meeting}
              onDismissBanner={live.dismissBanner}
              onCancel={() => cancelFinishing(meeting.meetingId)}
              cancelling={cancellingId === meeting.meetingId}
              onRetranscribe={(id) => void startAfterMeetingJob(id, "Retranscribe")}
              onResummarise={(id) => void startAfterMeetingJob(id, "Resummarise")}
              onDismissResult={() => {
                setResultDismissed(meeting.meetingId);
                setRetryError(null);
              }}
              showResult={resultDismissed !== meeting.meetingId}
              retrying={retrying}
              retryError={retryError}
            />
          ),
        }
      : null;
  return (
    <div className={styles.root}>
      {quitDialog}
      <nav className={styles.pane} aria-label="Meetings and settings">
        <div className={styles.paneTop}>
          <Button
            ref={newMeetingButton}
            appearance="primary"
            className={styles.fullWidth}
            onClick={newMeeting}
          >
            New meeting
          </Button>
          {/* Nothing to search on a computer with no meetings yet. */}
          {(load.status !== "loaded" || meetings.length > 0 || search.query !== "") && (
            <MeetingSearchBox query={search.query} onChange={search.setQuery} />
          )}
        </div>
        <div className={styles.listArea}>
          {search.active && (
            <MeetingSearchResults
              search={search}
              selectedId={view === "start" ? null : selectedId}
              onPick={pickResult}
            />
          )}
          {!search.active && load.status === "loaded" && (meetings.length > 0 || folders.length > 0) && (
            <FolderGroup
              folders={folders}
              selected={selectedFolder}
              onSelect={setSelectedFolder}
              edit={folderEdit}
              onNewFolder={() => setFolderEdit({ kind: "new", moveMeeting: null })}
              onRename={(folder) => setFolderEdit({ kind: "rename", folder })}
              onDelete={askDeleteFolder}
              onSubmitName={submitFolderName}
              onCancelEdit={cancelFolderEdit}
              buttonRef={(key, el) => {
                if (el) folderButtons.current.set(key, el);
                else folderButtons.current.delete(key);
              }}
            />
          )}
          {!search.active && selectedFolder !== null && visible.length === 0 && (
            <Caption1 className={styles.emptyFolder}>No meetings in this folder.</Caption1>
          )}
          {!search.active && visible.length > 0 && (
            <ul className={styles.list} aria-label="Meetings">
              {groupByDay(visible, today).map((group) => (
                <MeetingGroup
                  key={group.label}
                  group={group}
                  selectedId={view === "start" ? null : selectedId}
                  folders={folders}
                  onSelect={select}
                  onDelete={setDeleting}
                  onMove={(item, folder) => void moveMeeting(item, folder)}
                  onNewFolderFor={(item) => setFolderEdit({ kind: "new", moveMeeting: item.id })}
                  rowRef={(id, el) => {
                    if (el) rowButtons.current.set(id, el);
                    else rowButtons.current.delete(id);
                  }}
                />
              ))}
            </ul>
          )}
        </div>
        <div className={styles.paneBottom}>
          <Button
            appearance="subtle"
            className={styles.settings}
            aria-label={updateWaiting ? "Settings, update available" : undefined}
            onClick={() => {
              setSettingsSection(undefined);
              setView("settings");
              onOpenSettings();
            }}
          >
            Settings
            {updateWaiting && <span aria-hidden="true" data-testid="update-mark" className={styles.updateMark} />}
          </Button>
        </div>
      </nav>
      <main className={styles.detail}>
        {notice !== undefined && (
          <Badge appearance="tint" color="informative" className={styles.notice}>
            {notice}
          </Badge>
        )}
        {noModelBar}
        {recovered.length > 0 && (
          <MessageBar intent="info" layout="multiline">
            <MessageBarBody>
              <MessageBarTitle>Recording recovered</MessageBarTitle>
              {recoveredText(recovered[0].saved_ms)}
            </MessageBarBody>
            <MessageBarActions>
              <Button
                size="small"
                onClick={() => {
                  open(recovered[0].meeting);
                  setRecovered((all) => all.slice(1));
                }}
              >
                Open meeting
              </Button>
              <Button size="small" appearance="transparent" onClick={() => setRecovered((all) => all.slice(1))}>
                Dismiss
              </Button>
            </MessageBarActions>
          </MessageBar>
        )}
        {startError !== null && (
          <MessageBar intent="error">
            <MessageBarBody>
              <MessageBarTitle>The meeting could not start</MessageBarTitle>
              {startError}
            </MessageBarBody>
          </MessageBar>
        )}
        {stopError !== null && (
          <MessageBar intent="error">
            <MessageBarBody>
              <MessageBarTitle>The meeting could not be stopped</MessageBarTitle>
              {stopError}
            </MessageBarBody>
          </MessageBar>
        )}
        {folderNotice !== null && (
          <MessageBar intent="warning" layout="multiline">
            <MessageBarBody>
              <MessageBarTitle>{folderNotice.title}</MessageBarTitle>
              {folderNotice.text}
            </MessageBarBody>
            <MessageBarActions>
              <Button size="small" appearance="transparent" onClick={() => setFolderNotice(null)}>
                Dismiss
              </Button>
            </MessageBarActions>
          </MessageBar>
        )}
        {deleteError !== null && (
          <MessageBar intent="error" layout="multiline">
            <MessageBarBody>
              <MessageBarTitle>The meeting was not deleted</MessageBarTitle>
              {deleteError}
            </MessageBarBody>
            <MessageBarActions>
              <Button size="small" appearance="transparent" onClick={() => setDeleteError(null)}>
                Dismiss
              </Button>
            </MessageBarActions>
          </MessageBar>
        )}
        {behindId !== null && (
          <MessageBar intent="info" layout="multiline">
            <MessageBarBody>
              <MessageBarTitle>Your previous meeting is still finishing</MessageBarTitle>
              <span aria-live="polite">
                {cancellingId === behindId
                  ? "Stopping. This can take a moment if a model is still working or loading."
                  : "Its final transcript and summary are being made. Cancel keeps the recording; you can re-transcribe it later."}
              </span>
            </MessageBarBody>
            <MessageBarActions>
              <Button
                size="small"
                disabled={cancellingId === behindId}
                onClick={() => cancelFinishing(behindId)}
              >
                Cancel finishing
              </Button>
            </MessageBarActions>
          </MessageBar>
        )}
        {view === "start" && <NewMeetingView onStart={start} onCancel={() => setView("detail")} />}
        {showMeeting && meeting === null && <Spinner label="Starting the meeting" />}
        {showMeeting && meeting !== null && meeting.phase === "recording" && (
          <RecordingView
            meeting={meeting}
            timer={timer}
            levels={live.levels}
            onStop={stop}
            onCompact={() => goCompact(true)}
            onDismiss={live.dismissBanner}
            onKeepRecording={keepRecording}
            stopping={stopping}
          />
        )}
        {view === "detail" && (
          <Detail
            load={load}
            selected={selected}
            ended={ended}
            refresh={detailRefresh}
            summaryStored={summaryStored}
            onNewMeeting={newMeeting}
            nameVoices={nameVoices}
            onChanged={refresh}
            timer={timer}
            jobs={jobs}
            onJobStarted={(id, job) => {
              jobSeq.current += 1;
              const seq = jobSeq.current;
              setJobs((all) => ({ ...all, [id]: { job, last: null, seq } }));
            }}
            onJobDismissed={(id) =>
              setJobs((all) => {
                const { [id]: _, ...rest } = all;
                return rest;
              })
            }
          />
        )}
      </main>
      <Dialog
        modalType="alert"
        open={deleting !== null}
        onOpenChange={(_, d) => {
          if (!d.open && !deleteRunning.current) setDeleting(null);
        }}
      >
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Delete &quot;{deleting !== null ? displayTitle(deleting) : ""}&quot;?</DialogTitle>
            <DialogContent>
              This deletes the recording, transcript, summary and speaker names of this meeting
              from this computer. The note in your Obsidian vault stays.
            </DialogContent>
            <DialogActions>
              {/* Focusable while disabled: focus must not fall out of the open modal. */}
              <Button
                appearance="primary"
                disabledFocusable={deletePending}
                onClick={() => {
                  if (deleting !== null) void confirmDelete(deleting);
                }}
              >
                Delete meeting
              </Button>
              <Button disabledFocusable={deletePending} onClick={() => setDeleting(null)}>
                Cancel
              </Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
      {/* Here, not in the Folders group: a delete unmounts the folder's row, and a dialog that
          unmounts with its own screen leaves the page hidden from screen readers (T7.3d). */}
      <Dialog
        modalType="alert"
        open={deletingFolder !== null}
        onOpenChange={(_, d) => {
          if (!d.open && !folderDeleteRunning.current) setDeletingFolder(null);
        }}
      >
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Delete the folder &quot;{deletingFolder?.name ?? ""}&quot;?</DialogTitle>
            <DialogContent>
              {deletingCount === 0 ? (
                "The folder is empty, so only the folder goes."
              ) : (
                <>
                  <Body1 as="p">This folder holds {meetingCount(deletingCount)}.</Body1>
                  <RadioGroup
                    aria-label="What happens to its meetings"
                    value={deleteFolderMeetings ? "delete" : "keep"}
                    onChange={(_, d) => setDeleteFolderMeetings(d.value === "delete")}
                  >
                    <Radio value="keep" label="Keep the meetings. They stay in All meetings." />
                    <Radio
                      value="delete"
                      label="Delete the meetings too, with their recordings, transcripts, summaries and speaker names. Notes in your Obsidian vault stay."
                    />
                  </RadioGroup>
                </>
              )}
            </DialogContent>
            <DialogActions>
              {/* Focusable while disabled: focus must not fall out of the open modal. */}
              <Button
                appearance="primary"
                disabledFocusable={folderDeletePending}
                onClick={() => {
                  if (deletingFolder !== null) void confirmDeleteFolder(deletingFolder);
                }}
              >
                {deleteFolderMeetings && deletingCount > 0 ? "Delete folder and meetings" : "Delete folder"}
              </Button>
              <Button disabledFocusable={folderDeletePending} onClick={() => setDeletingFolder(null)}>
                Cancel
              </Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
    </div>
  );
}

function TrashIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M3 4.5h10M6.5 4.5V3a.5.5 0 0 1 .5-.5h2a.5.5 0 0 1 .5.5v1.5M4.5 4.5l.6 8.1a1 1 0 0 0 1 .9h3.8a1 1 0 0 0 1-.9l.6-8.1M7 7v4M9 7v4" />
    </svg>
  );
}

function MeetingGroup({
  group,
  selectedId,
  folders,
  onSelect,
  onDelete,
  onMove,
  onNewFolderFor,
  rowRef,
}: {
  group: DayGroup;
  selectedId: string | null;
  folders: FolderListItem[];
  onSelect: (id: string) => void;
  onDelete: (item: MeetingListItem) => void;
  onMove: (item: MeetingListItem, folder: string | null) => void;
  onNewFolderFor: (item: MeetingListItem) => void;
  rowRef: (id: string, el: HTMLButtonElement | null) => void;
}) {
  const styles = useStyles();
  const headingId = useId();
  // The delete dialog opens with no trigger of its own: on Cancel it hands focus back to where it
  // was, the row or its trash can.
  const restoreFocus = useRestoreFocusTarget();
  return (
    <li className={styles.group}>
      <div role="group" aria-labelledby={headingId}>
        <Caption1Strong as="h2" id={headingId} className={styles.groupHeading}>
          {group.label}
        </Caption1Strong>
        <ul className={styles.list}>
          {group.meetings.map((m) => {
            const isSelected = m.id === selectedId;
            return (
              <li key={m.id} className={styles.rowItem}>
                {/* A right-click, or the Menu key or Shift+F10 on the focused row. */}
                <Menu openOnContext>
                  <MenuTrigger disableButtonEnhancement>
                    <Button
                      {...restoreFocus}
                      ref={(el: HTMLButtonElement | null) => rowRef(m.id, el)}
                      appearance="subtle"
                      className={mergeClasses(styles.row, isSelected && styles.rowSelected)}
                      aria-current={isSelected ? "true" : undefined}
                      onClick={() => onSelect(m.id)}
                      onKeyDown={openMenuFromKeyboard}
                    >
                      <span className={styles.rowText}>
                        <Body1 className={mergeClasses("earshot-row-title", styles.rowTitle)}>
                          {displayTitle(m)}
                        </Body1>
                        <Caption1 className={styles.rowMeta}>
                          {m.state === "Recording" && (
                            <span className={styles.recordingDot} aria-hidden="true" />
                          )}
                          {(m.state === "Processing" || m.state === "Waiting") && (
                            <Spinner size="extra-tiny" aria-label={m.state} />
                          )}
                          {secondaryLine(m)}
                        </Caption1>
                      </span>
                    </Button>
                  </MenuTrigger>
                  <MenuPopover>
                    <MenuList>
                      {/* A meeting is filed once it has finished recording. */}
                      <Menu>
                        <MenuTrigger disableButtonEnhancement>
                          <MenuItem disabled={m.state === "Recording"}>Move to folder</MenuItem>
                        </MenuTrigger>
                        <MenuPopover>
                          <MenuList
                            checkedValues={{ folder: [m.folder_id ?? ""] }}
                            onCheckedValueChange={(_, d) => {
                              const to = d.checkedItems[0] ?? "";
                              if (to !== (m.folder_id ?? "")) onMove(m, to === "" ? null : to);
                            }}
                          >
                            {folders.map((f) => (
                              <MenuItemRadio key={f.id} name="folder" value={f.id}>
                                {f.name}
                              </MenuItemRadio>
                            ))}
                            {folders.length > 0 && <MenuDivider />}
                            <MenuItem onClick={() => onNewFolderFor(m)}>New folder...</MenuItem>
                            <MenuItemRadio name="folder" value="">
                              No folder
                            </MenuItemRadio>
                          </MenuList>
                        </MenuPopover>
                      </Menu>
                      <MenuItem onClick={() => onDelete(m)}>Delete meeting...</MenuItem>
                    </MenuList>
                  </MenuPopover>
                </Menu>
                <Button
                  appearance="subtle"
                  size="small"
                  icon={<TrashIcon />}
                  {...restoreFocus}
                  aria-label={`Delete ${displayTitle(m)}`}
                  className={mergeClasses("earshot-row-delete", styles.rowDelete)}
                  onClick={() => onDelete(m)}
                />
              </li>
            );
          })}
        </ul>
      </div>
    </li>
  );
}

/** The meeting that has ended, as its page shows it. */
interface EndedMeeting {
  id: string;
  /** Its final transcript and summary are still being made. */
  finishing: boolean;
  status: ReactNode;
}

function Detail({
  load,
  selected,
  ended,
  refresh,
  summaryStored,
  onNewMeeting,
  nameVoices,
  onChanged,
  timer,
  jobs,
  onJobStarted,
  onJobDismissed,
}: {
  load: Load;
  selected: MeetingListItem | null;
  ended: EndedMeeting | null;
  refresh: number;
  summaryStored: number;
  onNewMeeting: () => void;
  nameVoices: number;
  onChanged: () => void;
  timer: Timer;
  jobs: Record<string, JobView>;
  onJobStarted: (id: string, job: JobKind) => void;
  onJobDismissed: (id: string) => void;
}) {
  const styles = useStyles();
  if (load.status === "loading") {
    return <Spinner label="Loading meetings" />;
  }
  // The whole window shows the newer-data screen instead.
  if (load.status === "newer" || load.status === "dataFolder") return null;
  if (load.status === "failed") {
    return (
      <MessageBar intent="error">
        <MessageBarBody>
          <MessageBarTitle>Meetings could not be loaded</MessageBarTitle>
          {load.message}
        </MessageBarBody>
      </MessageBar>
    );
  }
  if (load.meetings.length === 0 || selected === null) {
    return (
      <div className={styles.empty}>
        <Title2 as="h1" className={styles.heading}>
          Record your first meeting
        </Title2>
        <Body1>Earshot writes the transcript and a summary while you talk.</Body1>
        <Button appearance="primary" onClick={onNewMeeting}>
          New meeting
        </Button>
      </div>
    );
  }
  // Only the page of the meeting that has ended says how it finishes; the draft transcript is
  // what there is to read meanwhile.
  const own = ended !== null && ended.id === selected.id ? ended : null;
  return (
    <MeetingDetailView
      key={selected.id}
      item={selected}
      status={own?.status}
      finishing={own?.finishing ?? false}
      initialTab={own?.finishing ? "transcript" : "summary"}
      refresh={own !== null ? refresh : 0}
      showSummary={own !== null ? summaryStored : 0}
      nameVoices={nameVoices}
      onChanged={onChanged}
      timer={timer}
      job={jobs[selected.id]}
      onJobStarted={(job) => onJobStarted(selected.id, job)}
      onJobDismissed={() => onJobDismissed(selected.id)}
    />
  );
}
