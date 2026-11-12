// The meeting detail screen (docs/ui-design.md "Meeting detail", T7.3, T9.1o): a header that
// stays in view with Speakers, Export, Copy summary, Re-transcribe and Re-summarise, Summary and
// Transcript tabs, and the Speakers panel (T12.19), which slides in beside them. Deleting starts
// from the meeting list. Every change goes through `useIpc()`; the detail is read again after
// each one.

import {
  Badge,
  Body1,
  Body1Strong,
  Button,
  Caption1,
  CompoundButton,
  Input,
  Link,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  Spinner,
  Subtitle2,
  Tab,
  TabList,
  Textarea,
  Title2,
  Tooltip,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import {
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";

import {
  type Citation,
  type DictionaryWordAdded,
  type TranscriptRow,
  type JobEvent,
  type JobKind,
  type LinkChoice,
  type MeetingDetail,
  type MeetingListItem,
  type PersonItem,
  type SummaryFieldRef,
  type SummaryView,
  useIpc,
} from "../ipc";
import { stepLabel } from "../meeting/liveMeeting";
import type { MessageBarIntent } from "../meeting/types";
import { displayTitle } from "../shell/meetings";
import { type Timer, realTimer } from "../time";
import { KeptLocal } from "./KeptLocal";
import { SpeakersPanel } from "./SpeakersPanel";
import {
  DictionaryBar,
  type DictionaryNote,
  DictionaryNoteLine,
  type MenuTarget,
  WordMenu,
  otherLinesSuggesting,
  pointTarget,
} from "./DictionaryWords";
import { dictionaryWord, inDictionary } from "./dictionaryWord";
import { LineText, WORD_ATTRIBUTE, type WordActions } from "./WordMarks";
import {
  COPIED_MS,
  failureText,
  handoverLine,
  headerLine,
  recordingsGone,
  recordingsGoneReason,
  rowTime,
  savedText,
  summaryMarkdown,
  voicesBadge,
  voicesToName,
} from "./detail";

const useStyles = makeStyles({
  root: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalM,
    minWidth: 0,
  },
  // Stays in view while the pane scrolls: the buttons and the tabs live here. AppShell's pane
  // covers its own top padding, so this sticks just below it, where it starts.
  pinned: {
    position: "sticky",
    top: 0,
    zIndex: 1,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
    backgroundColor: tokens.colorNeutralBackground1,
    borderBottom: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
  },
  header: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "flex-start",
    gap: tokens.spacingHorizontalS,
    paddingBottom: tokens.spacingVerticalXS,
  },
  headerText: {
    flexGrow: 1,
    flexBasis: "240px",
    minWidth: 0,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
  },
  heading: {
    margin: 0,
    overflowWrap: "anywhere",
  },
  titleLine: {
    display: "flex",
    alignItems: "center",
    flexWrap: "wrap",
    gap: tokens.spacingHorizontalS,
    minWidth: 0,
  },
  // The title and transcript editors (T12.16): the field, then Save and Cancel.
  editor: {
    display: "flex",
    alignItems: "flex-start",
    flexWrap: "wrap",
    gap: tokens.spacingHorizontalS,
    minWidth: 0,
  },
  field: {
    flexGrow: 1,
    minWidth: "200px",
  },
  headerButtons: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "flex-start",
    gap: tokens.spacingHorizontalS,
    whiteSpace: "nowrap",
  },
  // Wide enough for "Copy summary", so the header's buttons stay put when it says "Copied".
  copyButton: {
    minWidth: "124px",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
  // The transcript and the Speakers panel side by side: the panel's column grows from
  // nothing when it opens, pushing the transcript aside rather than covering it (T12.19).
  columns: {
    display: "grid",
    gridTemplateColumns: "minmax(0, 1fr) auto",
    alignItems: "start",
  },
  drawer: {
    width: 0,
    overflow: "hidden",
    transitionProperty: "width, margin-left",
    transitionDuration: tokens.durationNormal,
    transitionTimingFunction: tokens.curveEasyEase,
    "@media (prefers-reduced-motion: reduce)": {
      transitionDuration: "0.01ms",
    },
  },
  drawerOpen: {
    width: "320px",
    marginLeft: tokens.spacingHorizontalL,
  },
  // The panel itself slides in from the right as its column opens.
  drawerInner: {
    width: "320px",
    animationName: {
      from: { transform: "translateX(24px)", opacity: 0 },
      to: { transform: "translateX(0)", opacity: 1 },
    },
    animationDuration: tokens.durationNormal,
    animationTimingFunction: tokens.curveDecelerateMid,
    "@media (prefers-reduced-motion: reduce)": {
      animationDuration: "0.01ms",
    },
  },
  // ...and out again the same way while its column closes.
  drawerInnerLeaving: {
    animationName: {
      from: { transform: "translateX(0)", opacity: 1 },
      to: { transform: "translateX(24px)", opacity: 0 },
    },
    animationTimingFunction: tokens.curveAccelerateMid,
    animationFillMode: "forwards",
  },
  badge: {
    marginLeft: tokens.spacingHorizontalXS,
  },
  main: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalM,
    minWidth: 0,
  },
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
  },
  sectionHeading: {
    margin: 0,
  },
  list: {
    margin: 0,
    paddingLeft: tokens.spacingHorizontalL,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
  },
  actions: {
    display: "grid",
    gridTemplateColumns: "120px minmax(0, 1fr)",
    columnGap: tokens.spacingHorizontalM,
    rowGap: tokens.spacingVerticalXS,
    alignItems: "baseline",
  },
  owner: {
    overflowWrap: "anywhere",
  },
  cite: {
    marginLeft: tokens.spacingHorizontalXS,
    fontSize: tokens.fontSizeBase200,
  },
  citeMissing: {
    marginLeft: tokens.spacingHorizontalXS,
    color: tokens.colorNeutralForegroundDisabled,
  },
  summaryButtons: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalM,
    flexWrap: "wrap",
  },
  transcript: {
    display: "flex",
    flexDirection: "column",
  },
  row: {
    display: "grid",
    gridTemplateColumns: "56px 120px minmax(0, 1fr) auto",
    columnGap: tokens.spacingHorizontalM,
    alignItems: "baseline",
    paddingTop: tokens.spacingVerticalXS,
    paddingBottom: tokens.spacingVerticalXS,
    paddingLeft: tokens.spacingHorizontalS,
    paddingRight: tokens.spacingHorizontalS,
    borderRadius: tokens.borderRadiusMedium,
    // The Edit button shows on hover and whenever the row holds focus (T12.16).
    ":hover .earshot-line-edit": { opacity: 1 },
    ":focus-within .earshot-line-edit": { opacity: 1 },
  },
  lineEdit: {
    opacity: 0,
  },
  // A summary text with its Edit buttons at the end (T12.16). Hidden, a button takes no width,
  // so the text and citations read as before; hover or focus shows it.
  editable: {
    ":hover .earshot-inline-edit": {
      opacity: 1,
      maxWidth: "none",
      minWidth: "auto",
      paddingLeft: tokens.spacingHorizontalS,
      paddingRight: tokens.spacingHorizontalS,
    },
    ":focus-within .earshot-inline-edit": {
      opacity: 1,
      maxWidth: "none",
      minWidth: "auto",
      paddingLeft: tokens.spacingHorizontalS,
      paddingRight: tokens.spacingHorizontalS,
    },
  },
  inlineEdit: {
    opacity: 0,
    maxWidth: 0,
    minWidth: 0,
    paddingLeft: 0,
    paddingRight: 0,
    overflow: "hidden",
    whiteSpace: "nowrap",
    marginLeft: tokens.spacingHorizontalXS,
  },
  editedMark: {
    marginLeft: tokens.spacingHorizontalXS,
    color: tokens.colorNeutralForeground3,
  },
  rowCited: {
    backgroundColor: tokens.colorBrandBackground2,
  },
  time: {
    color: tokens.colorNeutralForeground3,
    fontVariantNumeric: "tabular-nums",
  },
  speaker: {
    overflowWrap: "anywhere",
  },
  text: {
    overflowWrap: "anywhere",
  },
  // The Dictionary bar and note under a line (T12.15f), under its words.
  lineDictionary: {
    gridColumn: "3 / 5",
  },
});

export type Tab = "summary" | "transcript";

export interface Notice {
  key: string;
  intent: MessageBarIntent;
  title?: string;
  text: string;
  busy?: boolean;
}

export interface MeetingDetailViewProps {
  /** The meeting's row in the list (title and state while the detail loads). */
  item: MeetingListItem;
  /** Increments to focus the first voice that needs a name ("Name voices"). */
  nameVoices?: number;
  /** Something the meeting list shows may have changed (title, state). */
  onChanged(): void;
  /** This meeting's job as the app follows it (`AppShell` hears every `JobEvent`, so a job
   * that ends while another meeting is shown is seen on return). */
  job?: JobView;
  /** A job was started from this screen. */
  onJobStarted(job: JobKind): void;
  /** The job's notice was dismissed. */
  onJobDismissed(): void;
  /** Timers (injected by tests). */
  timer?: Timer;
  /** Shown at the top of the page: the meeting that has just ended says how its finishing goes
   * and what came of it (T12.28). */
  status?: ReactNode;
  /** The meeting's final transcript and summary are still being made (T12.28): the page's own
   * Re-transcribe and Summarise wait, because the app would refuse them as busy. */
  finishing?: boolean;
  /** The tab the page opens on (default the summary). */
  initialTab?: Tab;
  /** Changes when the meeting may have changed outside this page (its final transcript or its
   * summary was stored): the page reads it again. */
  refresh?: number;
  /** Changes when the meeting's summary was stored: the page shows it. */
  showSummary?: number;
}

/** Why Re-transcribe and Summarise wait while the meeting finishes. */
const FINISHING_TIP = "Available when the summary is done.";

/** A meeting's job: which one, and its latest event (`null` until the first). `seq` changes
 * with every event. */
export interface JobView {
  job: JobKind;
  last: JobEvent | null;
  seq: number;
}

/** Whether `e` ends its job. */
export function isTerminal(e: JobEvent | null): boolean {
  return e !== null && (e.type === "Retranscribed" || e.type === "Summary" || e.type === "Failed");
}

/** The notice of a job. */
export function jobNotice(view: JobView): Notice {
  const e = view.last;
  if (e === null) {
    return view.job === "Retranscribe"
      ? { key: "job", intent: "info", text: "Re-transcribing from the recording", busy: true }
      : { key: "job", intent: "info", text: "Writing a new summary", busy: true };
  }
  switch (e.type) {
    case "Progress":
      return { key: "job", intent: "info", text: `Re-transcribing: ${stepLabel(e.step)}`, busy: true };
    case "Warning":
      return { key: "job", intent: "warning", text: e.message, busy: true };
    case "Retranscribed":
      return e.no_audio
        ? {
            key: "job",
            intent: "warning",
            text: "The recordings of this meeting are no longer kept, so it cannot be transcribed again.",
          }
        : e.warnings.length > 0
          ? {
              key: "job",
              intent: "warning",
              text: `The meeting was transcribed again. ${e.warnings.join(" ")}`,
            }
          : { key: "job", intent: "success", text: "The meeting was transcribed again." };
    case "Summary":
      switch (e.result.kind) {
        case "Stored":
          return {
            key: "job",
            intent: "success",
            text: "New summary ready. The previous one is kept until the next re-summary.",
          };
        case "Skipped":
          return { key: "job", intent: "info", title: "No summary", text: e.result.reason };
        case "Failed":
          return { key: "job", intent: "error", title: "Summary failed", text: e.result.message };
        // A job has no final pass or recording, so these are only here to be complete.
        case "PostFailed":
        case "RecordingFailed":
          return { key: "job", intent: "error", title: "Summary failed", text: e.result.message };
        case "Cancelled":
        case "PostCancelled":
          return { key: "job", intent: "info", text: "The summary was cancelled." };
        default: {
          const unhandled: never = e.result;
          return unhandled;
        }
      }
    case "Failed":
      return {
        key: "job",
        intent: "error",
        title: e.job === "Retranscribe" ? "Re-transcription failed" : "Summary failed",
        text: e.message,
      };
  }
  return { key: "job", intent: "info", text: "" };
}

/** The detail of a stored meeting. */
export function MeetingDetailView({
  item,
  nameVoices = 0,
  onChanged,
  job,
  onJobStarted,
  onJobDismissed,
  timer = realTimer,
  status,
  finishing = false,
  initialTab = "summary",
  refresh = 0,
  showSummary = 0,
}: MeetingDetailViewProps) {
  const styles = useStyles();
  const ipc = useIpc();
  const id = item.id;
  const [detail, setDetail] = useState<MeetingDetail | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [persons, setPersons] = useState<PersonItem[]>([]);
  const [tab, setTab] = useState<Tab>(initialTab);
  const [cited, setCited] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [noAudio, setNoAudio] = useState<Set<string>>(new Set());
  const [editingTitle, setEditingTitle] = useState(false);
  // The Speakers panel: `null` until the meeting loads, then open by itself only when a
  // voice needs a name (T12.19); the header's Speakers button toggles it.
  const [speakersOpen, setSpeakersOpen] = useState<boolean | null>(null);
  // A closed panel stays drawn, hidden from assistive technology and inert, while its column
  // closes, so it slides out as it slid in; the column's transition end removes it.
  // Set during render (React's "adjusting state when a value changes"), not in an effect, so
  // the panel is never unmounted for one commit between open and leaving.
  const [speakersLeaving, setSpeakersLeaving] = useState(false);
  const [speakersWere, setSpeakersWere] = useState(speakersOpen);
  if (speakersWere !== speakersOpen) {
    setSpeakersWere(speakersOpen);
    setSpeakersLeaving(speakersWere === true && speakersOpen !== true);
  }
  const panelId = useId();
  const voicesBefore = useRef(0);
  const speakersButton = useRef<HTMLButtonElement | null>(null);
  const rows = useRef(new Map<string, HTMLDivElement>());
  const nameField = useRef<HTMLInputElement | null>(null);
  const view = useRef<HTMLElement | null>(null);
  const audio = useRef<HTMLAudioElement | null>(null);
  const audioUrl = useRef<string | null>(null);

  const reload = useCallback(async () => {
    try {
      const [d, p] = await Promise.all([ipc.getMeeting(id), ipc.listPersons()]);
      setDetail(d);
      // Decided with each detail, in the same render: the first opens it exactly when a voice
      // needs a name; a later one opens it when more voices need a name than before (a
      // re-transcription found new ones). Never closed by itself. A voice recognised from a
      // voiceprint has a name, so it never opens it.
      const needing = voicesToName(d);
      const before = voicesBefore.current;
      voicesBefore.current = needing;
      setSpeakersOpen((open) => (open === null ? needing > 0 : needing > before ? true : open));
      setPersons(p);
      setLoadError(null);
    } catch (e) {
      setLoadError(failureText(e));
    }
  }, [ipc, id]);

  useEffect(() => {
    void reload();
  }, [reload]);

  useEffect(
    () => () => {
      if (audioUrl.current) URL.revokeObjectURL?.(audioUrl.current);
    },
    [],
  );

  // A job of this meeting ended (now, or while another meeting was shown): read again.
  const ended = job !== undefined && isTerminal(job.last) ? job.seq : null;
  useEffect(() => {
    if (ended !== null) void reload();
  }, [ended, reload]);

  // The meeting changed outside this page (its finishing stored something): read again, and show
  // a summary that was just stored. The values the page opened with were read by the first load.
  const seenRefresh = useRef(refresh);
  useEffect(() => {
    if (seenRefresh.current === refresh) return;
    seenRefresh.current = refresh;
    void reload();
  }, [refresh, reload]);
  const seenSummary = useRef(showSummary);
  useEffect(() => {
    if (seenSummary.current === showSummary) return;
    seenSummary.current = showSummary;
    setCited(null);
    setTab("summary");
  }, [showSummary]);

  // "Name voices": open the panel and focus the first name field.
  useEffect(() => {
    if (nameVoices > 0) setSpeakersOpen(true);
  }, [nameVoices]);
  useEffect(() => {
    if (nameVoices > 0 && detail !== null && speakersOpen === true) nameField.current?.focus();
  }, [nameVoices, detail, speakersOpen]);

  // A citation shows its row. Any other tab change opens at the top: the pane's scroll position
  // would otherwise carry over, and the end of a long transcript is past the end of a summary.
  useEffect(() => {
    const row = tab === "transcript" && cited !== null ? rows.current.get(cited) : undefined;
    if (row !== undefined) row.scrollIntoView?.({ block: "center" });
    else view.current?.scrollIntoView?.({ block: "start" });
  }, [tab, cited]);

  const run = async (f: () => Promise<unknown>, failTitle: string) => {
    try {
      await f();
      await reload();
      onChanged();
    } catch (e) {
      setNotice({ key: "edit", intent: "error", title: failTitle, text: failureText(e) });
    }
  };

  /** Saves a typed edit (T12.16); false when it was refused, so the editor stays open. */
  const save = async (f: () => Promise<unknown>, failTitle: string): Promise<boolean> => {
    try {
      await f();
    } catch (e) {
      setNotice({ key: "edit", intent: "error", title: failTitle, text: failureText(e) });
      return false;
    }
    setNotice((n) => (n?.key === "edit" ? null : n));
    await reload();
    onChanged();
    return true;
  };

  const openCitation = (c: Citation) => {
    setCited(c.segment_id);
    setTab("transcript");
  };

  const play = async (speaker: string) => {
    try {
      const wav = await ipc.speakerSample(id, speaker);
      if (wav === null) {
        setNoAudio((s) => new Set(s).add(speaker));
        return;
      }
      const bytes = Uint8Array.from(atob(wav), (ch) => ch.charCodeAt(0));
      if (typeof URL.createObjectURL !== "function" || audio.current === null) return;
      if (audioUrl.current) URL.revokeObjectURL(audioUrl.current);
      audioUrl.current = URL.createObjectURL(new Blob([bytes], { type: "audio/wav" }));
      audio.current.src = audioUrl.current;
      await audio.current.play();
    } catch (e) {
      setNotice({ key: "edit", intent: "error", title: "The sample could not be played", text: failureText(e) });
    }
  };

  const exportNote = async () => {
    try {
      const done = await ipc.exportMeeting(id);
      setNotice({ key: "export", intent: "success", text: `Exported to ${done.note_path}` });
    } catch (e) {
      setNotice({ key: "export", intent: "warning", text: failureText(e) });
    }
  };

  const startJob = async (job: "retranscribe" | "resummarise") => {
    try {
      if (job === "retranscribe") {
        await ipc.retranscribe(id);
        onJobStarted("Retranscribe");
      } else {
        await ipc.resummarise(id);
        onJobStarted("Resummarise");
      }
      setNotice(null);
      onChanged();
    } catch (e) {
      setNotice({ key: "job", intent: "error", text: failureText(e) });
    }
  };

  const title = displayTitle(item);
  const jobBar = job !== undefined ? jobNotice(job) : null;
  const voices = detail ? voicesToName(detail) : 0;
  // A new summary being written replaces the one shown, so there is nothing settled to copy.
  const writingSummary = job !== undefined && job.job === "Resummarise" && !isTerminal(job.last);
  const summaryText = detail?.summary && !writingSummary ? summaryMarkdown(detail.summary) : "";

  return (
    <section ref={view} className={styles.root} aria-label="Meeting detail">
      <div className={styles.pinned}>
        <div className={styles.header}>
          <div className={styles.headerText}>
            {editingTitle && detail !== null ? (
              <TextEditor
                label="Meeting title"
                initial={detail.header.title}
                multiline={false}
                onSave={async (text) => {
                  if (await save(() => ipc.renameMeeting(id, text), "The title was not saved")) {
                    setEditingTitle(false);
                  }
                }}
                onCancel={() => setEditingTitle(false)}
              />
            ) : (
              <div className={styles.titleLine}>
                <Title2 as="h1" className={styles.heading}>
                  {title}
                </Title2>
                {detail !== null && (
                  <Button size="small" appearance="subtle" onClick={() => setEditingTitle(true)}>
                    Rename
                  </Button>
                )}
              </div>
            )}
            {detail !== null && <Caption1 className={styles.muted}>{headerLine(detail)}</Caption1>}
          </div>
          <div className={styles.headerButtons}>
            <Button
              ref={speakersButton}
              aria-expanded={speakersOpen === true}
              aria-controls={speakersOpen === true ? panelId : undefined}
              aria-label={voices > 0 ? `Speakers, ${voicesBadge(voices)}` : "Speakers"}
              disabled={detail === null}
              onClick={() => setSpeakersOpen((open) => open !== true)}
            >
              Speakers
              {voices > 0 && (
                <Badge className={styles.badge} appearance="filled" color="brand" size="small">
                  {voicesBadge(voices)}
                </Badge>
              )}
            </Button>
            <Button onClick={() => void exportNote()} disabled={detail === null}>
              Export to Obsidian
            </Button>
            {summaryText !== "" && (
              <CopySummaryButton
                text={summaryText}
                timer={timer}
                onCopied={() => setNotice((n) => (n?.key === "copy" ? null : n))}
                onFailed={() =>
                  setNotice({ key: "copy", intent: "error", text: "The summary could not be copied." })
                }
              />
            )}
            {detail !== null && recordingsGone(detail) ? (
              // Focusable, so a keyboard reaches the reason.
              <CompoundButton disabledFocusable secondaryContent={recordingsGoneReason(detail)}>
                Re-transcribe
              </CompoundButton>
            ) : finishing ? (
              <Tooltip content={FINISHING_TIP} relationship="description">
                <Button disabledFocusable>Re-transcribe</Button>
              </Tooltip>
            ) : (
              <Button
                disabled={detail === null}
                onClick={() => void startJob("retranscribe")}
              >
                Re-transcribe
              </Button>
            )}
            {finishing ? (
              <Tooltip content={FINISHING_TIP} relationship="description">
                <Button disabledFocusable>
                  {detail !== null && detail.summary !== null ? "Re-summarise" : "Summarise"}
                </Button>
              </Tooltip>
            ) : (
              <Button disabled={detail === null} onClick={() => void startJob("resummarise")}>
                {detail !== null && detail.summary === null ? "Summarise" : "Re-summarise"}
              </Button>
            )}
          </div>
        </div>
        {detail !== null && (
          <TabList selectedValue={tab} onTabSelect={(_, d) => setTab(d.value as Tab)}>
            <Tab value="summary">Summary</Tab>
            <Tab value="transcript">Transcript</Tab>
          </TabList>
        )}
      </div>

      {status}
      {jobBar !== null && (
        <MessageBar intent={jobBar.intent} layout="multiline">
          <MessageBarBody>
            {jobBar.title !== undefined && <MessageBarTitle>{jobBar.title}</MessageBarTitle>}
            {jobBar.text}
          </MessageBarBody>
          <MessageBarActions>
            {jobBar.busy ? (
              <Spinner size="extra-tiny" aria-label="Working" />
            ) : (
              <Button size="small" appearance="transparent" onClick={onJobDismissed}>
                Dismiss
              </Button>
            )}
          </MessageBarActions>
        </MessageBar>
      )}
      {notice !== null && (
        <MessageBar intent={notice.intent} layout="multiline">
          <MessageBarBody>
            {notice.title !== undefined && <MessageBarTitle>{notice.title}</MessageBarTitle>}
            {notice.text}
          </MessageBarBody>
          <MessageBarActions>
            {notice.busy ? (
              <Spinner size="extra-tiny" aria-label="Working" />
            ) : (
              <Button size="small" appearance="transparent" onClick={() => setNotice(null)}>
                Dismiss
              </Button>
            )}
          </MessageBarActions>
        </MessageBar>
      )}

      {loadError !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>The meeting could not be loaded</MessageBarTitle>
            {loadError}
          </MessageBarBody>
        </MessageBar>
      )}
      {detail === null && loadError === null && <Spinner label="Loading the meeting" />}

      {detail !== null && (
        <>
          <div className={styles.columns}>
            <div className={styles.main}>
              {tab === "summary" ? (
                <SummaryTab
                  detail={detail}
                  onCite={openCitation}
                  onEdit={
                    writingSummary
                      ? undefined
                      : (summary, field, text) =>
                          save(() => ipc.editSummary(id, summary, field, text), "The summary was not changed")
                  }
                  onResummarise={() => void startJob("resummarise")}
                  onUndo={() => void run(() => ipc.undoSummary(id), "The previous summary could not be restored")}
                  onAllowWriter={() =>
                    void run(() => ipc.allowSummaryWriter(id), "The meeting could not be changed")
                  }
                />
              ) : (
                <TranscriptTab
                  detail={detail}
                  cited={cited}
                  onEdit={async (segment, text) => {
                    let offer: string[] = [];
                    const saved = await save(async () => {
                      offer = await ipc.editSegment(id, segment, text);
                    }, "The line was not saved");
                    return saved ? offer : null;
                  }}
                  onAddWord={async (token) => {
                    try {
                      const added = await ipc.addDictionaryWord(token);
                      await reload();
                      return added;
                    } catch (e) {
                      return { error: failureText(e) };
                    }
                  }}
                  onRemoveWord={async (word) => {
                    try {
                      await ipc.removeDictionaryWord(word);
                      await reload();
                      return null;
                    } catch (e) {
                      return { error: failureText(e) };
                    }
                  }}
                  wordActions={(segment) => ({
                    onUse: (token, word) =>
                      void save(() => ipc.useWord(id, segment, token, word), "The word was not changed"),
                    onPutBack: (token) =>
                      void save(() => ipc.putBackWord(id, segment, token), "The word was not put back"),
                    onKeep: (token) => void save(() => ipc.keepWord(id, segment, token), "The word was not kept"),
                  })}
                  rowRef={(segment, el) => {
                    if (el) rows.current.set(segment, el);
                    else rows.current.delete(segment);
                  }}
                />
              )}
            </div>
            <div
              className={mergeClasses(styles.drawer, speakersOpen === true && styles.drawerOpen)}
              onTransitionEnd={(e) => {
                if (e.target === e.currentTarget && speakersOpen !== true) setSpeakersLeaving(false);
              }}
              onTransitionCancel={(e) => {
                if (e.target === e.currentTarget && speakersOpen !== true) setSpeakersLeaving(false);
              }}
            >
              {(speakersOpen === true || speakersLeaving) && (
                <div
                  className={mergeClasses(
                    styles.drawerInner,
                    speakersOpen !== true && styles.drawerInnerLeaving,
                  )}
                  aria-hidden={speakersOpen !== true || undefined}
                  inert={speakersOpen !== true || undefined}
                >
                  <SpeakersPanel
                    id={panelId}
                    detail={detail}
                    persons={persons}
                    noAudio={noAudio}
                    locked={finishing}
                    onClose={() => {
                      setSpeakersOpen(false);
                      // The Close button leaves with the panel: focus goes back where it opens.
                      speakersButton.current?.focus();
                    }}
                    onPlay={(s) => void play(s)}
                    onRename={(s, name) => run(() => ipc.renameSpeaker(id, s, name), "The name was not saved")}
                    onRenamePerson={(person, name) =>
                      run(() => ipc.renamePerson(person, name), "The person was not renamed")
                    }
                    onLink={(s, choice: LinkChoice) =>
                      run(() => ipc.linkSpeaker(id, s, choice), "The speaker was not linked")
                    }
                    onMarkMe={(s) => run(() => ipc.markMe(id, s), "The voice was not marked as you")}
                    onUnmarkMe={(s) => run(() => ipc.unmarkMe(id, s), "The voice was not changed")}
                    onMerge={(keep, merge) =>
                      run(() => ipc.mergeSpeakers(id, keep, merge), "The speakers were not merged")
                    }
                    onForget={(person) =>
                      run(async () => {
                        await ipc.forgetPersonVoices(person);
                        setNotice({ key: "edit", intent: "success", text: "The voice was forgotten." });
                      }, "The voice was not forgotten")
                    }
                    onSave={(s, choice, name) =>
                      run(async () => {
                        const saved = await ipc.saveVoice(id, s, choice);
                        const kept = saved.outcome === "Remembered" || saved.outcome === "AlreadyRemembered";
                        setNotice({
                          key: "edit",
                          intent: kept ? "success" : "info",
                          text: savedText(name, saved.outcome),
                        });
                      }, "The voice was not saved")
                    }
                    onIgnore={(s) => run(() => ipc.ignoreSpeaker(id, s), "The voice was not ignored")}
                    onUnignore={(s) => run(() => ipc.unignoreSpeaker(id, s), "The voice was not changed")}
                    firstNameField={(el) => {
                      nameField.current = el;
                    }}
                  />
                </div>
              )}
            </div>
          </div>
        </>
      )}

      {/* The play-sample audio (blob: URL, CSP media-src blob:). */}
      <audio ref={audio} hidden />
    </section>
  );
}

/** Copies `text` to the clipboard. The webview allows this without a plugin: the page is a
 * secure context and writing needs no permission. */
function CopySummaryButton({
  text,
  timer,
  onCopied,
  onFailed,
}: {
  text: string;
  timer: Timer;
  onCopied(): void;
  onFailed(): void;
}) {
  const styles = useStyles();
  const [copied, setCopied] = useState(false);
  const cancelReset = useRef<(() => void) | null>(null);

  useEffect(() => () => cancelReset.current?.(), []);

  const copy = async () => {
    cancelReset.current?.();
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      setCopied(false);
      onFailed();
      return;
    }
    setCopied(true);
    onCopied();
    cancelReset.current = timer.setTimeout(() => setCopied(false), COPIED_MS);
  };

  return (
    <Button className={styles.copyButton} onClick={() => void copy()}>
      <span aria-live="polite">{copied ? "Copied" : "Copy summary"}</span>
    </Button>
  );
}

function Citations({ citations, onCite }: { citations: Citation[]; onCite(c: Citation): void }) {
  const styles = useStyles();
  return (
    <>
      {citations.map((c) =>
        c.row === null ? (
          <Caption1 key={c.segment_id} className={styles.citeMissing} title="This line is no longer in the transcript">
            [unavailable]
          </Caption1>
        ) : (
          <Link
            key={c.segment_id}
            className={styles.cite}
            aria-label={`Show line ${c.row + 1} in the transcript`}
            onClick={() => onCite(c)}
          >
            [{c.row + 1}]
          </Link>
        ),
      )}
    </>
  );
}

/** One text of a summary the user may correct (T12.16). */
interface EditField {
  /** Names the field: "Edit {label}" and the editor's label. */
  label: string;
  /** The button's own text. */
  button: string;
  text: string;
  onSave(text: string): Promise<boolean>;
}

/** Summary content with Edit buttons at its end, shown on hover and focus; a field being
 * edited replaces the content with its editor. With no fields it is the content alone. */
function Editable({ fields, children }: { fields: EditField[]; children: ReactNode }) {
  const styles = useStyles();
  const [editing, setEditing] = useState<EditField | null>(null);
  if (editing !== null) {
    return (
      <TextEditor
        label={editing.label}
        initial={editing.text}
        multiline
        onSave={async (t) => {
          if (await editing.onSave(t)) setEditing(null);
        }}
        onCancel={() => setEditing(null)}
      />
    );
  }
  return (
    <span className={styles.editable}>
      {children}
      {fields.map((f) => (
        <Button
          key={f.label}
          size="small"
          appearance="subtle"
          className={mergeClasses(styles.inlineEdit, "earshot-inline-edit")}
          aria-label={`Edit ${f.label}`}
          onClick={() => setEditing(f)}
        >
          {f.button}
        </Button>
      ))}
    </span>
  );
}

function SummaryBody({
  summary,
  onCite,
  onEdit,
}: {
  summary: SummaryView;
  onCite(c: Citation): void;
  /** Saves one text; false when it was refused. Absent: read only. */
  onEdit?: (field: SummaryFieldRef, text: string) => Promise<boolean>;
}) {
  const styles = useStyles();
  /** The field as an Edit button, or none when read only. */
  const edit = (field: SummaryFieldRef, label: string, text: string, button = "Edit"): EditField[] =>
    onEdit === undefined ? [] : [{ label, button, text, onSave: (t) => onEdit(field, t) }];
  return (
    <>
      <div className={styles.section}>
        <Subtitle2 as="h2" className={styles.sectionHeading}>
          TL;DR
        </Subtitle2>
        <Body1>
          <Editable fields={edit({ kind: "Tldr" }, "TL;DR", summary.tldr)}>{summary.tldr}</Editable>
        </Body1>
      </div>
      {summary.decisions.length > 0 && (
        <div className={styles.section}>
          <Subtitle2 as="h2" className={styles.sectionHeading}>
            Decisions
          </Subtitle2>
          <ul className={styles.list} aria-label="Decisions">
            {summary.decisions.map((d, i) => (
              <li key={i}>
                <Editable fields={edit({ kind: "Decision", index: i }, `decision ${i + 1}`, d.text)}>
                  <Body1>{d.text}</Body1>
                  <Citations citations={d.citations} onCite={onCite} />
                </Editable>
              </li>
            ))}
          </ul>
        </div>
      )}
      {summary.actions.length > 0 && (
        <div className={styles.section}>
          <Subtitle2 as="h2" className={styles.sectionHeading}>
            Actions
          </Subtitle2>
          <div role="table" aria-label="Actions" className={styles.actions}>
            {summary.actions.map((a, i) => (
              <div role="row" key={i} style={{ display: "contents" }}>
                <Body1Strong role="cell" className={styles.owner}>
                  <Editable
                    fields={edit({ kind: "ActionOwner", index: i }, `who does action ${i + 1}`, a.owner)}
                  >
                    {a.owner}
                  </Editable>
                </Body1Strong>
                <span role="cell">
                  <Editable
                    fields={[
                      ...edit({ kind: "ActionTask", index: i }, `action ${i + 1}`, a.task),
                      ...(a.due === null
                        ? []
                        : edit({ kind: "ActionDue", index: i }, `when action ${i + 1} is due`, a.due, "Edit date")),
                    ]}
                  >
                    <Body1>
                      {a.task}
                      {a.due !== null && ` (by ${a.due})`}
                    </Body1>
                    <Citations citations={a.citations} onCite={onCite} />
                  </Editable>
                </span>
              </div>
            ))}
          </div>
          <Caption1 className={styles.muted}>Tick actions in the Obsidian note.</Caption1>
        </div>
      )}
      {summary.notes.map((n, i) => (
        <div className={styles.section} key={`notes-${i}`}>
          <Subtitle2 as="h2" className={styles.sectionHeading}>
            <Editable fields={edit({ kind: "NoteHeading", section: i }, `heading ${n.heading}`, n.heading)}>
              {n.heading}
            </Editable>
          </Subtitle2>
          <ul className={styles.list}>
            {n.bullets.map((b, j) => (
              <li key={j}>
                <Editable
                  fields={edit(
                    { kind: "NoteBullet", section: i, bullet: j },
                    `note ${j + 1} under ${n.heading}`,
                    b.text,
                  )}
                >
                  <Body1>{b.text}</Body1>
                  <Citations citations={b.citations} onCite={onCite} />
                </Editable>
              </li>
            ))}
          </ul>
        </div>
      ))}
      {summary.open_questions.length > 0 && (
        <div className={styles.section}>
          <Subtitle2 as="h2" className={styles.sectionHeading}>
            Open questions
          </Subtitle2>
          <ul className={styles.list}>
            {summary.open_questions.map((q, i) => (
              <li key={i}>
                <Editable fields={edit({ kind: "OpenQuestion", index: i }, `open question ${i + 1}`, q)}>
                  <Body1>{q}</Body1>
                </Editable>
              </li>
            ))}
          </ul>
        </div>
      )}
    </>
  );
}

function SummaryTab({
  detail,
  onCite,
  onEdit,
  onResummarise,
  onUndo,
  onAllowWriter,
}: {
  detail: MeetingDetail;
  onCite(c: Citation): void;
  /** Saves one text of summary `summary`; false when refused. Absent while a new summary is
   * being written. */
  onEdit?: (summary: string, field: SummaryFieldRef, text: string) => Promise<boolean>;
  onResummarise(): void;
  onUndo(): void;
  onAllowWriter(): void;
}) {
  const styles = useStyles();
  const s = detail.summary;
  return (
    <>
      {detail.summary_stale && (
        <MessageBar intent="warning" layout="multiline">
          <MessageBarBody>
            This summary refers to lines of an earlier transcript. Re-summarise to match the
            current one.
          </MessageBarBody>
          <MessageBarActions>
            <Button size="small" onClick={onResummarise}>
              Re-summarise
            </Button>
          </MessageBarActions>
        </MessageBar>
      )}
      <KeptLocal keptLocal={detail.kept_local} onAllow={onAllowWriter} />
      {handoverLine(detail) !== null && <Caption1 className={styles.muted}>{handoverLine(detail)}</Caption1>}
      {s === null ? (
        <Body1 className={styles.muted}>
          {detail.state === "Processing"
            ? "The summary is being written."
            : detail.state === "Waiting"
              ? "The summary will be written when the previous meeting has finished."
              : "This meeting has no summary yet."}
        </Body1>
      ) : (
        <SummaryBody
          summary={s}
          onCite={onCite}
          onEdit={onEdit === undefined ? undefined : (field, text) => onEdit(s.id, field, text)}
        />
      )}
      {detail.can_undo_summary && (
        <div className={styles.summaryButtons}>
          <Link onClick={onUndo}>Switch to previous summary</Link>
        </div>
      )}
    </>
  );
}

/** A typed text with Save and Cancel (T12.16): Enter saves, Escape cancels. A title or a
 * transcript line holds no line break, so Enter never adds one. */
function TextEditor({
  label,
  initial,
  multiline,
  onSave,
  onCancel,
}: {
  label: string;
  initial: string;
  multiline: boolean;
  onSave(text: string): Promise<void>;
  onCancel(): void;
}) {
  const styles = useStyles();
  const [text, setText] = useState(initial);
  const [saving, setSaving] = useState(false);
  const submit = async () => {
    if (saving) return;
    setSaving(true);
    try {
      await onSave(text);
    } finally {
      setSaving(false);
    }
  };
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void submit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onCancel();
    }
  };
  return (
    <div className={styles.editor}>
      {multiline ? (
        <Textarea
          className={styles.field}
          aria-label={label}
          value={text}
          resize="vertical"
          autoFocus
          onChange={(_, d) => setText(d.value)}
          onKeyDown={onKeyDown}
        />
      ) : (
        <Input
          className={styles.field}
          aria-label={label}
          value={text}
          autoFocus
          onChange={(_, d) => setText(d.value)}
          onKeyDown={onKeyDown}
        />
      )}
      <Button appearance="primary" disabled={saving} onClick={() => void submit()}>
        Save
      </Button>
      <Button disabled={saving} onClick={onCancel}>
        Cancel
      </Button>
    </div>
  );
}

function TranscriptTab({
  detail,
  cited,
  onEdit,
  wordActions,
  onAddWord,
  onRemoveWord,
  rowRef,
}: {
  detail: MeetingDetail;
  cited: string | null;
  /** Saves a line's new text; the words the Dictionary bar offers, or `null` when it was
   * refused. */
  onEdit(segment: string, text: string): Promise<string[] | null>;
  /** What the marks of a line's suggested and changed words do (T12.15e). */
  wordActions(segment: string): WordActions;
  /** "Add to Dictionary" (T12.15f); the detail is read again before it resolves. */
  onAddWord(token: string): Promise<DictionaryWordAdded | { error: string }>;
  /** "Remove from Dictionary" and "Undo" (T12.15f); the detail is read again before it
   * resolves. */
  onRemoveWord(word: string): Promise<{ error: string } | null>;
  rowRef(segment: string, el: HTMLDivElement | null): void;
}) {
  const styles = useStyles();
  const [editing, setEditing] = useState<string | null>(null);
  // T12.15f: the bar under a line just edited, the note after a Dictionary action and the word
  // menu. The bar and the note stay until the user moves on (another edit, a Dictionary action
  // on another line, Not now).
  const [offer, setOffer] = useState<{ segment: string; words: string[] } | null>(null);
  const [note, setNote] = useState<DictionaryNote | null>(null);
  const [menu, setMenu] = useState<{
    segment: string;
    words: string[];
    target: MenuTarget;
    back: HTMLElement | null;
  } | null>(null);
  // Where focus goes once the detail shows an action's result (the clicked button may be gone).
  const [focusNext, setFocusNext] = useState<{ undo: true } | { word: string } | null>(null);
  const undoRef = useRef<HTMLButtonElement | null>(null);
  const tableRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (focusNext === null) return;
    if ("undo" in focusNext) undoRef.current?.focus();
    else
      tableRef.current
        ?.querySelector<HTMLElement>(`[data-offered-word="${CSS.escape(focusNext.word)}"]`)
        ?.focus();
    setFocusNext(null);
  }, [focusNext]);

  // Only the final transcript can be corrected: the final pass replaces a draft.
  const canEdit = detail.header.transcript === "Final";
  if (detail.transcript.length === 0) {
    return <Body1 className={styles.muted}>No speech was transcribed in this meeting.</Body1>;
  }

  /** Moving on to another line closes what was open under an earlier one. */
  const moveTo = (segment: string) => {
    if (offer !== null && offer.segment !== segment) setOffer(null);
    if (note !== null && note.segment !== segment) setNote(null);
  };
  const add = async (segment: string, word: string, fromBar: boolean) => {
    moveTo(segment);
    const r = await onAddWord(word);
    if ("error" in r) setNote({ segment, kind: "error", text: r.error });
    else {
      setNote({ segment, kind: r.added ? "added" : "already", word: r.word });
      if (fromBar && r.added) setFocusNext({ undo: true });
    }
  };
  const remove = async (segment: string, word: string) => {
    moveTo(segment);
    const r = await onRemoveWord(word);
    setNote(r === null ? { segment, kind: "removed", word } : { segment, kind: "error", text: r.error });
  };
  const toggle = (segment: string, word: string) =>
    void (inDictionary(detail.dictionary, word) ? remove(segment, word) : add(segment, word, false));
  const undo = async (segment: string, word: string) => {
    const r = await onRemoveWord(word);
    if (r === null) {
      setNote(null);
      if (offer?.segment === segment && offer.words.includes(word)) setFocusNext({ word });
    } else setNote({ segment, kind: "error", text: r.error });
  };
  /** The words of line `r` a menu offers, each once (case ignored), in line order. */
  const wordsOf = (r: TranscriptRow): string[] => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const t of r.text.split(/\p{White_Space}+/u)) {
      const w = dictionaryWord(t);
      if (w === null || seen.has(w.toLowerCase())) continue;
      seen.add(w.toLowerCase());
      out.push(w);
    }
    return out;
  };
  // Opening a menu moves nothing: what is open under another line goes only when the menu's
  // action runs (`add`, `remove`), so the line under the pointer stays where it is.
  const openMenu = (segment: string, words: string[], target: MenuTarget, back: HTMLElement | null) =>
    setMenu({ segment, words, target, back });
  /** A right-click on a word of a final line opens its menu at the pointer. */
  const onRowContextMenu = (r: TranscriptRow) => (e: MouseEvent<HTMLDivElement>) => {
    // A popover is portalled, but React still delivers its events here.
    if (!canEdit || editing === r.segment_id || !e.currentTarget.contains(e.target as Node)) return;
    const el = (e.target as Element).closest<HTMLElement>(`[${WORD_ATTRIBUTE}]`);
    const word = el?.getAttribute(WORD_ATTRIBUTE);
    if (!el || !word) return;
    e.preventDefault();
    openMenu(r.segment_id, [word], pointTarget(e.clientX, e.clientY), el.tagName === "BUTTON" ? el : null);
  };
  /** The Menu key or Shift+F10: on a marked word, that word's menu; anywhere else in the row
   * (its Edit button), a menu of every word of the line. */
  const onRowKeyDown = (r: TranscriptRow) => (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== "ContextMenu" && !(e.shiftKey && e.key === "F10")) return;
    if (!canEdit || editing === r.segment_id || !e.currentTarget.contains(e.target as Node)) return;
    const el = e.target as HTMLElement;
    const word = el.getAttribute(WORD_ATTRIBUTE);
    const words = word ? [word] : wordsOf(r);
    if (words.length === 0) return;
    e.preventDefault();
    openMenu(r.segment_id, words, el, el);
  };

  return (
    <div role="table" aria-label="Transcript" className={styles.transcript} ref={tableRef}>
      {detail.transcript.map((r, i) => (
        <div
          role="row"
          key={r.segment_id}
          ref={(el) => rowRef(r.segment_id, el)}
          aria-current={r.segment_id === cited ? "true" : undefined}
          className={mergeClasses(styles.row, r.segment_id === cited && styles.rowCited)}
          onContextMenu={onRowContextMenu(r)}
          onKeyDown={onRowKeyDown(r)}
        >
          <Caption1 role="cell" className={styles.time}>
            {rowTime(r.start_ms)}
          </Caption1>
          <Body1Strong role="cell" className={styles.speaker}>
            {r.speaker}
          </Body1Strong>
          {editing === r.segment_id ? (
            <div role="cell" className={styles.text}>
              <TextEditor
                label={`Line ${i + 1}`}
                initial={r.text}
                multiline
                onSave={async (text) => {
                  const offered = await onEdit(r.segment_id, text);
                  if (offered === null) return;
                  setEditing(null);
                  setOffer(offered.length > 0 ? { segment: r.segment_id, words: offered } : null);
                }}
                onCancel={() => setEditing(null)}
              />
            </div>
          ) : (
            <Body1 role="cell" className={styles.text}>
              {canEdit ? (
                <LineText
                  row={r}
                  wordList={detail.word_list}
                  actions={wordActions(r.segment_id)}
                  dictionary={{
                    entries: detail.dictionary,
                    onToggle: (w) => toggle(r.segment_id, w),
                  }}
                  tinted={
                    offer?.segment === r.segment_id
                      ? offer.words.filter((w) => !inDictionary(detail.dictionary, w))
                      : undefined
                  }
                />
              ) : (
                r.text
              )}
              {r.edited && <Caption1 className={styles.editedMark}>(edited)</Caption1>}
            </Body1>
          )}
          {/* Not a cell: a row's cells are its time, speaker and words. */}
          <span>
            {canEdit && editing !== r.segment_id && (
              <Button
                size="small"
                appearance="subtle"
                className={mergeClasses(styles.lineEdit, "earshot-line-edit")}
                aria-label={`Edit line ${i + 1}`}
                onClick={() => {
                  setEditing(r.segment_id);
                  setOffer(null);
                  setNote(null);
                }}
              >
                Edit
              </Button>
            )}
          </span>
          {canEdit &&
            editing !== r.segment_id &&
            (offer?.segment === r.segment_id || note?.segment === r.segment_id) && (
              <div className={styles.lineDictionary}>
                {offer?.segment === r.segment_id && (
                  <DictionaryBar
                    words={offer.words}
                    dictionary={detail.dictionary}
                    onAdd={(w) => void add(r.segment_id, w, true)}
                    onClose={() => {
                      setOffer(null);
                      setNote(null);
                    }}
                  />
                )}
                {note?.segment === r.segment_id && (
                  <DictionaryNoteLine
                    note={note}
                    others={
                      note.kind === "added" ? otherLinesSuggesting(detail.transcript, r.segment_id, note.word) : 0
                    }
                    undoRef={undoRef}
                    onUndo={() => note.kind === "added" && void undo(r.segment_id, note.word)}
                  />
                )}
              </div>
            )}
        </div>
      ))}
      <WordMenu
        words={menu?.words ?? []}
        dictionary={detail.dictionary}
        target={menu?.target ?? null}
        onToggle={(w) => {
          if (menu !== null) toggle(menu.segment, w);
        }}
        onClose={() => {
          const back = menu?.back;
          setMenu(null);
          back?.focus();
        }}
      />
    </div>
  );
}
