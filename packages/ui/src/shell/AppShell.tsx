// The app window (docs/ui-design.md "Shape"): the meeting list on the left, the selected
// meeting, the meeting screen (T7.2) or the empty state on the right; while recording it
// can shrink to the compact listening card. "Settings" replaces the window with the
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
  MenuItem,
  MenuList,
  MenuPopover,
  MenuTrigger,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  Spinner,
  Title2,
  makeStyles,
  mergeClasses,
  tokens,
  useFocusFinders,
  useRestoreFocusTarget,
} from "@fluentui/react-components";
import { type KeyboardEvent, type ReactNode, useCallback, useEffect, useId, useRef, useState } from "react";

import {
  type CaptureMode,
  IpcFailure,
  type JobKind,
  type MeetingEvent,
  type MeetingListItem,
  useIpc,
} from "../ipc";
import {
  AfterMeetingView,
  CompactCard,
  NewMeetingView,
  RecordingView,
} from "../meeting/MeetingViews";
import { failureText } from "../detail/detail";
import { type JobView, MeetingDetailView, isTerminal } from "../detail/MeetingDetailView";
import { FirstRunWizard } from "../firstRun/FirstRunWizard";
import { useLiveMeeting, useNow } from "../meeting/hooks";
import { type SettingsSection, SettingsView } from "../settings/SettingsView";
import { useThemeChoice } from "../theme/EarshotThemeProvider";
import { type Timer, realTimer } from "../time";
import { MeetingSearchBox, MeetingSearchResults, useMeetingSearch } from "./MeetingSearch";
import { type DayGroup, displayTitle, groupByDay, secondaryLine } from "./meetings";

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
  detail: {
    flexGrow: 1,
    minWidth: 0,
    display: "flex",
    flexDirection: "column",
    padding: tokens.spacingHorizontalXXL,
    gap: tokens.spacingVerticalM,
    overflowY: "auto",
    // Keyboard focus never lands behind the meeting's sticky header.
    scrollPaddingTop: "96px",
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
  /** Also told when "Name voices" on the summary notice opens the meeting's who-is-who
   * panel. */
  onNameVoices?: (meetingId: string) => void;
}

type Load =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; meetings: MeetingListItem[] };

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
  const [compact, setCompact] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [stopError, setStopError] = useState<string | null>(null);
  const [startError, setStartError] = useState<string | null>(null);
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
  const refresh = useCallback(() => setReload((n) => n + 1), []);
  const search = useMeetingSearch(ipc, timer, reload);
  // Jobs on stored meetings, followed here so an ending is seen whatever meeting is shown.
  const [jobs, setJobs] = useState<Record<string, JobView>>({});
  const jobSeq = useRef(0);

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
      setSelectedId(event.meeting);
      // Also a meeting the tray or the shortcut started.
      setView("meeting");
      setStartError(null);
    }
    // An older meeting's end says nothing about the stop button of this one.
    if (event.type === "Ended" && event.meeting === liveMeetingId.current) setStopping(false);
  });
  const meeting = live.meeting;
  liveMeetingId.current = meeting?.meetingId ?? null;
  const active = meeting !== null && meeting.phase !== "done";
  const clockNow = useNow(timer, 1000);

  // The native title bar follows the theme chosen in Settings. A title bar that stays on the
  // system's colours changes nothing else, so a refusal is not worth a banner.
  const { choice: themeChoice } = useThemeChoice();
  useEffect(() => {
    ipc.setWindowTheme(themeChoice === "system" ? null : themeChoice).catch(noop);
  }, [ipc, themeChoice]);

  useEffect(() => {
    let current = true;
    ipc.listMeetings().then(
      (meetings) => {
        if (!current) return;
        setLoad({ status: "loaded", meetings });
        setSelectedId((sel) =>
          sel !== null && meetings.some((m) => m.id === sel) ? sel : (meetings[0]?.id ?? null),
        );
      },
      (error: unknown) => {
        if (!current) return;
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

  useEffect(() => {
    const gone = refocusAfterDelete.current;
    if (gone === null || load.status !== "loaded" || load.meetings.some((m) => m.id === gone)) return;
    refocusAfterDelete.current = null;
    const row = selectedId !== null ? rowButtons.current.get(selectedId) : undefined;
    (row ?? newMeetingButton.current)?.focus();
  }, [load, selectedId]);

  const meetings = load.status === "loaded" ? load.meetings : [];
  const selected = meetings.find((m) => m.id === selectedId) ?? null;

  // Only a recording meeting keeps the next one out; one that is finishing goes on behind it.
  const newMeeting = () => setView(meeting?.phase === "recording" ? "meeting" : "start");
  const start = async (mode: CaptureMode, title: string | null) => {
    live.reset();
    setStopError(null);
    const id = await ipc.startMeeting(mode, title);
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
  const select = (id: string) => {
    setSelectedId(id);
    setView(meeting !== null && meeting.meetingId === id && active ? "meeting" : "detail");
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

  // What the tray, the shortcut and notifications ask of the window. The latest handlers are
  // read through a ref so the subscription lasts the window's life.
  const shellHandlers = useRef({ open, openNames, goCompact });
  shellHandlers.current = { open, openNames, goCompact };
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
        now={clockNow}
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
            onDone={() => {
              refresh();
              setView(setupFromSettings ? "settings" : active ? "meeting" : "detail");
            }}
          />
        </div>
        {quitDialog}
      </div>
    );
  }

  if (view === "settings") {
    return (
      <>
        <SettingsView
          initialSection={settingsSection}
          onBack={() => setView(active ? "meeting" : "detail")}
          onOpenSetup={() => {
            setSetupFromSettings(true);
            setSettingsSection("General");
            setView("first-run");
          }}
        />
        {quitDialog}
      </>
    );
  }

  const showMeeting = view === "meeting";
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
          {!search.active && meetings.length > 0 && (
            <ul className={styles.list} aria-label="Meetings">
              {groupByDay(meetings, now()).map((group) => (
                <MeetingGroup
                  key={group.label}
                  group={group}
                  selectedId={view === "start" ? null : selectedId}
                  onSelect={select}
                  onDelete={setDeleting}
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
            onClick={() => {
              setSettingsSection(undefined);
              setView("settings");
              onOpenSettings();
            }}
          >
            Settings
          </Button>
        </div>
      </nav>
      <main className={styles.detail}>
        {notice !== undefined && (
          <Badge appearance="tint" color="informative" className={styles.notice}>
            {notice}
          </Badge>
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
        {view === "start" && <NewMeetingView onStart={start} onCancel={() => setView("detail")} />}
        {showMeeting && meeting === null && <Spinner label="Starting the meeting" />}
        {showMeeting && meeting !== null && meeting.phase === "recording" && (
          <RecordingView
            meeting={meeting}
            now={clockNow}
            onStop={stop}
            onCompact={() => goCompact(true)}
            onDismiss={live.dismissBanner}
            onKeepRecording={keepRecording}
            stopping={stopping}
          />
        )}
        {showMeeting && meeting !== null && meeting.phase !== "recording" && (
          <AfterMeetingView
            meeting={meeting}
            onOpen={open}
            onNameVoices={openNames}
            onDismissBanner={live.dismissBanner}
          />
        )}
        {view === "detail" && (
          <Detail
            load={load}
            selected={selected}
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
    </div>
  );
}

/** The Menu key and Shift+F10 open the row's menu, under the row. Windows normally turns them into
 * a `contextmenu` event itself; doing it here does not depend on that, and opening an open menu
 * changes nothing. */
function openMenuFromKeyboard(e: KeyboardEvent<HTMLElement>) {
  if (e.key !== "ContextMenu" && !(e.shiftKey && e.key === "F10")) return;
  e.preventDefault();
  const r = e.currentTarget.getBoundingClientRect();
  e.currentTarget.dispatchEvent(
    new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: r.left + 16, clientY: r.bottom }),
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
  onSelect,
  onDelete,
  rowRef,
}: {
  group: DayGroup;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onDelete: (item: MeetingListItem) => void;
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

function Detail({
  load,
  selected,
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
  return (
    <MeetingDetailView
      key={selected.id}
      item={selected}
      nameVoices={nameVoices}
      onChanged={onChanged}
      timer={timer}
      job={jobs[selected.id]}
      onJobStarted={(job) => onJobStarted(selected.id, job)}
      onJobDismissed={() => onJobDismissed(selected.id)}
    />
  );
}
