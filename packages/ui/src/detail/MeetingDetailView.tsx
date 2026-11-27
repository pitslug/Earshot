// The meeting detail screen (docs/ui-design.md "Meeting detail", T7.3, T9.1o): a header that
// stays in view with Speakers, Export, Copy summary, Re-transcribe and Re-summarise, Summary and
// Transcript tabs, and the Speakers panel (T12.19), which slides in beside them. Deleting starts
// from the meeting list. Every change goes through `useIpc()`; the detail is read again after
// each one.

import {
  Body1,
  Body1Strong,
  Button,
  type ButtonProps,
  Caption1,
  CounterBadge,
  Divider,
  Input,
  Label,
  Link,
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
  Subtitle2,
  Tab,
  TabList,
  Textarea,
  Title2,
  Tooltip,
  makeStyles,
  mergeClasses,
  tokens,
  useRestoreFocusTarget,
} from "@fluentui/react-components";
import {
  type KeyboardEvent,
  type MouseEvent,
  type ReactElement,
  type ReactNode,
  type RefObject,
  memo,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import {
  type Citation,
  type DictionaryWordAdded,
  type TranscriptRow,
  type JobEvent,
  type JobKind,
  IpcFailure,
  ORDER_COPY,
  fillCopy,
  SPLIT_NEEDS_WORD_TIMES_COPY,
  type StepChoice,
  type LinkChoice,
  type MeetingDetail,
  type MeetingListItem,
  type MovableSpeaker,
  type PersonItem,
  type SummaryFieldRef,
  type SummaryView,
  useIpc,
} from "../ipc";
import { stepLabel } from "../meeting/liveMeeting";
import type { MessageBarIntent } from "../meeting/types";
import { displayTitle } from "../shell/meetings";
import { type Timer, realTimer } from "../time";
import { type ActionItem, actionGroups, joinNames, shownOwners } from "./actionGroups";
import { hidesByItself, useAutoHide } from "./autoHide";
import { KeptLocal } from "./KeptLocal";
import { keepUnchanged } from "./keepUnchanged";
import { scrollLineIntoView } from "./scrollLine";
import { ReexportQuestion } from "./ReexportQuestion";
import { ResummariseControl } from "./ResummariseControl";
import { SpeakersPanel, userNamed } from "./SpeakersPanel";
import {
  DictionaryBar,
  type DictionaryNote,
  DictionaryNoteLine,
  type MenuTarget,
  WordMenu,
  otherLinesSuggesting,
  pointTarget,
} from "./DictionaryWords";
import { dictionaryWord, inDictionary, tokensOf } from "./dictionaryWord";
import { useWindowWatched } from "../shell/windowWatched";
import { LineText, WORD_ATTRIBUTE, type WordActions } from "./WordMarks";
import {
  FindButton,
  type LineFind,
  TranscriptFind,
  sameLineFind,
  useFindMarks,
} from "./TranscriptFind";
import {
  FIND_PAUSE_MS,
  type FindResult,
  findCount,
  findCountText,
  findInTranscript,
  findLine,
  findSpeakers,
  speakerKey,
} from "./findMatches";
import {
  COPIED_MS,
  failureText,
  handoverLine,
  headerLine,
  RECORDING_GONE_TIP,
  type SummaryWriting,
  recordingsGone,
  rowTime,
  summaryMarkdown,
  voicesBadge,
  voicesToName,
} from "./detail";
import {
  linkedText,
  markedMeText,
  mergedText,
  notThisPersonText,
  savedVoiceText,
} from "./voiceSampleCopy";

/** Where the pinned Speakers panel sticks: the pinned header's height plus a gap. */
const PANEL_TOP = "--earshot-panel-top";
/** The tallest the pinned Speakers panel may be: what the pane shows below the header. */
const PANEL_HEIGHT = "--earshot-panel-height";

/** The nearest ancestor that scrolls vertically (AppShell's meeting pane), or null. */
function scrollingAncestor(el: HTMLElement): HTMLElement | null {
  for (let p = el.parentElement; p !== null; p = p.parentElement) {
    const y = getComputedStyle(p).overflowY;
    if (y === "auto" || y === "scroll") return p;
  }
  return null;
}

/** The least height the Speakers panel is given, in pixels (T14.58): with several banners pinned
 * on a short window the space left below the header can shrink towards nothing; the panel keeps
 * this much and the page scrolls instead. A judgement: a voice row and its buttons. */
export const PANEL_MIN_HEIGHT = 200;

/** The Speakers panel's height: what the pane shows below the pinned header (`top`), never less
 * than `PANEL_MIN_HEIGHT`. `paneInner` is the pane's height inside its padding. */
export function panelHeight(paneInner: number, top: number): number {
  return Math.max(PANEL_MIN_HEIGHT, paneInner - top);
}

/**
 * Keeps `PANEL_TOP` and `PANEL_HEIGHT` on `root` up to date, so the Speakers panel sticks just
 * below the pinned header and fits in the pane whatever the header's height and the window's.
 */
function usePanelSpace(
  root: RefObject<HTMLElement | null>,
  pinned: RefObject<HTMLElement | null>,
  gap: number,
) {
  useLayoutEffect(() => {
    const view = root.current;
    const header = pinned.current;
    if (view === null || header === null) return;
    const pane = scrollingAncestor(view);
    const measure = () => {
      const top = header.offsetHeight + gap;
      view.style.setProperty(PANEL_TOP, `${top}px`);
      if (pane !== null) {
        const style = getComputedStyle(pane);
        const padding = parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
        const height = panelHeight(pane.clientHeight - (Number.isNaN(padding) ? 0 : padding), top);
        view.style.setProperty(PANEL_HEIGHT, `${height}px`);
      }
    };
    measure();
    const watch = new ResizeObserver(measure);
    watch.observe(header);
    if (pane !== null) watch.observe(pane);
    return () => watch.disconnect();
  }, [root, pinned, gap]);
}

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
  // The title and caption take the full width; the buttons sit on their own row below them,
  // above the tabs, at every width (the owner, 2026-11-28, T14.66).
  header: {
    display: "flex",
    flexDirection: "column",
    alignItems: "stretch",
    gap: tokens.spacingVerticalS,
    paddingBottom: tokens.spacingVerticalXS,
  },
  headerText: {
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
  // A shown label takes its own line above the editor's field.
  editorLabel: {
    flexBasis: "100%",
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
  // Fluent's own icon-only Button size (medium), for a button whose only child is hidden text.
  iconOnlyButton: {
    minWidth: "32px",
    maxWidth: "32px",
    paddingLeft: "5px",
    paddingRight: "5px",
    // The hidden text is positioned against the button, not the page.
    position: "relative",
  },
  // An icon-only header button's icon, with the count of voices that need a name on its corner.
  iconWithCount: {
    position: "relative",
    display: "inline-flex",
  },
  iconCount: {
    position: "absolute",
    top: "-8px",
    right: "-10px",
  },
  // Read by assistive technology, not drawn (the usual clip pattern).
  srOnly: {
    position: "absolute",
    width: "1px",
    height: "1px",
    margin: "-1px",
    padding: 0,
    overflow: "hidden",
    clip: "rect(0 0 0 0)",
    whiteSpace: "nowrap",
    border: 0,
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
  // The panel stays in view while the transcript scrolls: it sticks just below the pinned header
  // and scrolls on its own when it is taller than the space left (`usePanelSpace`).
  drawer: {
    position: "sticky",
    top: `var(${PANEL_TOP}, 0px)`,
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
    maxHeight: `var(${PANEL_HEIGHT}, none)`,
    overflowY: "auto",
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
  // The "Notes" section (T14.70): its heading, then each topic with its sub-heading.
  notes: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  // The thin line above "Notes": the main column's own gap keeps it apart from both sides.
  notesDivider: {
    flexGrow: 0,
  },
  list: {
    margin: 0,
    paddingLeft: tokens.spacingHorizontalL,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
  },
  // One group of actions (T14.60): its heading, then its list.
  actionGroup: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
  },
  // Who else does an action, after its citations: small secondary text.
  actionOwners: {
    color: tokens.colorNeutralForeground3,
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
  // Read by assistive technology only: the transcript's keys (T14.19).
  hiddenText: {
    position: "absolute",
    width: "1px",
    height: "1px",
    margin: "-1px",
    padding: 0,
    overflow: "hidden",
    clipPath: "inset(50%)",
    whiteSpace: "nowrap",
    border: 0,
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
    // A line off screen is not laid out or painted (T14.18): an hours-long transcript costs what
    // its visible lines cost. The size stands in until the line has been drawn once ("auto"
    // then keeps its last real size), so the scrollbar and scrolling to a far line stay close.
    // Focus, find's highlights and menus are unaffected: menus and popovers are portalled, and
    // a line holding focus or in view is always drawn.
    contentVisibility: "auto",
    containIntrinsicSize: "auto 32px",
    // A line focused from the keyboard (T14.19) shows the focus ring.
    ":focus-visible": {
      outline: `${tokens.strokeWidthThick} solid ${tokens.colorStrokeFocus2}`,
      outlineOffset: "-2px",
    },
    // The Edit button shows on hover and whenever the row holds focus (T12.16).
    ":hover .earshot-line-edit": { opacity: 1 },
    ":focus-within .earshot-line-edit": { opacity: 1 },
  },
  lineEdit: {
    opacity: 0,
  },
  // The line's Edit and More buttons, side by side (T12.90).
  lineActions: {
    display: "flex",
    columnGap: tokens.spacingHorizontalXXS,
  },
  // The More button is one character wide, so the two buttons cost a line little more room
  // than the Edit button alone did (T12.90).
  lineMore: {
    minWidth: "28px",
    paddingLeft: tokens.spacingHorizontalXS,
    paddingRight: tokens.spacingHorizontalXS,
  },
  // A line's words as split points (T12.90).
  splitPoints: {
    display: "flex",
    flexDirection: "column",
    rowGap: tokens.spacingVerticalXS,
  },
  splitFooter: {
    display: "flex",
    alignItems: "center",
    justifyContent: "flex-start",
  },
  splitWords: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "baseline",
    columnGap: tokens.spacingHorizontalXS,
    rowGap: tokens.spacingVerticalXS,
  },
  // A summary text with its edit icons (T12.16, T12.105): the item keeps a gutter on its right
  // (its padding, set per item for its number of icons) and the icons sit in it at the top, so
  // hovering never moves a line and an icon never wraps alone; hover or focus shows them.
  editable: {
    display: "block",
    position: "relative",
    ":hover .earshot-inline-edit": { opacity: 1 },
    ":focus-within .earshot-inline-edit": { opacity: 1 },
  },
  editIcons: {
    position: "absolute",
    top: 0,
    right: 0,
    display: "flex",
  },
  inlineEdit: {
    opacity: 0,
    width: "24px",
    minWidth: "24px",
    height: "24px",
    paddingLeft: 0,
    paddingRight: 0,
  },
  editedMark: {
    marginLeft: tokens.spacingHorizontalXS,
    color: tokens.colorNeutralForeground3,
  },
  rowCited: {
    backgroundColor: tokens.colorBrandBackground2,
  },
  // T12.98: a line of the speaker find looks for, and the turn it points at.
  rowFound: {
    backgroundColor: tokens.colorNeutralBackground3,
  },
  rowCurrentTurn: {
    outline: `${tokens.strokeWidthThick} solid ${tokens.colorBrandStroke1}`,
    outlineOffset: "-2px",
  },
  // The tabs, with the find button at the end of the row (T12.98).
  tabRow: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: tokens.spacingHorizontalS,
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
  /** The user asked to cancel the running job (T12.41). */
  onJobCancel(): void;
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
  /** Changes when a sync tick changed the meeting here (T12.81): the page reads it again. */
  synced?: number;
  /** Changes when the meeting's summary was stored: the page shows it. */
  showSummary?: number;
  /** The Speakers panel's "Manage people": opens Settings on the People page (T12.104). */
  onManagePeople?(): void;
}

/** The title of the waiting banner (T11.3db). */
export const WAITING_TITLE = "Summary on another computer";

/** Whether a job ended by asking another computer for the summary: the page's waiting banner
 * (`MeetingDetail.waiting_for`) says so, once, so the job adds no notice of its own (T12.81). */
function jobAskedAnotherComputer(job: JobView): boolean {
  const e = job.last;
  return e !== null && (e.type === "Offloaded" || (e.type === "Summary" && e.result.kind === "Offloaded"));
}

/** Why Re-transcribe and Summarise wait while the meeting finishes. */
/** A header button's tooltip while the meeting finishes, led by the button's own label. */
const finishingTip = (label: string) => fillCopy(ORDER_COPY.finishing_tip, { label });

/** A meeting's job: which one, and its latest event (`null` until the first). `seq` changes
 * with every event. `cancelling`: the user asked to cancel it and its last event has not come. */
export interface JobView {
  job: JobKind;
  last: JobEvent | null;
  seq: number;
  cancelling?: boolean;
}

/** Whether `e` ends its job. */
export function isTerminal(e: JobEvent | null): boolean {
  return (
    e !== null &&
    (e.type === "Retranscribed" ||
      e.type === "Summary" ||
      e.type === "Offloaded" ||
      e.type === "Settled" ||
      e.type === "Cancelled" ||
      e.type === "Failed")
  );
}

/** What a job's notice says while it stops (as the meeting's own Cancel finishing says). */
export const JOB_STOPPING_TEXT = "Stopping. This can take a moment if a model is still working or loading.";

/** What a re-summary's notice says once its summary is stored. */
export const STORED_TEXT = "New summary ready.";

/** The notice of a job. */
export function jobNotice(view: JobView): Notice {
  const e = view.last;
  if (view.cancelling === true && !isTerminal(e)) {
    return { key: "job", intent: "info", text: JOB_STOPPING_TEXT, busy: true };
  }
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
    // Nothing to go on with (T12.63b): the shell drops the job, so this never shows.
    case "Settled":
      return { key: "job", intent: "info", text: "" };
    // A re-summary that asked one of the user's other computers (T12.63b).
    case "Offloaded":
      return { key: "job", intent: "info", title: ORDER_COPY.resummarise.offloaded_title, text: e.message };
    case "Summary":
      switch (e.result.kind) {
        // Short whatever the order did: what became of each step (`e.note`) is the Summary tab's
        // run note (`MeetingDetail.summary_run`, the same `steps_text`), and the previous
        // summary is its "Switch to previous summary" link.
        case "Stored":
          return { key: "job", intent: "success", text: STORED_TEXT };
        case "Skipped":
          return {
            key: "job",
            intent: "info",
            title: ORDER_COPY.resummarise.no_summary_title,
            text: e.note ?? e.result.reason,
          };
        case "Failed":
          return { key: "job", intent: "error", title: "Summary failed", text: e.result.message };
        // A job has no final pass or recording, so these are only here to be complete.
        case "PostFailed":
        case "RecordingFailed":
          return { key: "job", intent: "error", title: "Summary failed", text: e.result.message };
        case "Cancelled":
        case "PostCancelled":
          return { key: "job", intent: "info", text: "The summary was cancelled." };
        // Another computer writes it (T11.3db); the summary arrives by sync.
        case "Offloaded":
          return {
            key: "job",
            intent: "info",
            title: ORDER_COPY.resummarise.offloaded_title,
            text: e.result.message,
          };
        default: {
          const unhandled: never = e.result;
          return unhandled;
        }
      }
    case "Cancelled":
      return {
        key: "job",
        intent: "info",
        text:
          e.job === "Retranscribe"
            ? "The re-transcription was cancelled. The transcript has not changed."
            : "The new summary was cancelled. Nothing was changed.",
      };
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
  onJobCancel,
  timer = realTimer,
  status,
  finishing = false,
  initialTab = "summary",
  refresh = 0,
  synced = 0,
  showSummary = 0,
  onManagePeople,
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
  const [askingReexport, setAskingReexport] = useState(false);
  // The meeting this page exported since its detail was read (T14.63).
  const [exportedId, setExportedId] = useState<string | null>(null);
  const restoreFocus = useRestoreFocusTarget();
  const [noAudio, setNoAudio] = useState<Set<string>>(new Set());
  const [editingTitle, setEditingTitle] = useState(false);
  // Separate persons named after the user, offered one at a time after "This is me" (T12.44).
  const [mergeOffers, setMergeOffers] = useState<PersonItem[]>([]);
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
  // Bumped when the user opens the panel with its button: focus goes to the panel's heading.
  // Each press moves focus once: a later automatic open never does.
  const [focusPanel, setFocusPanel] = useState(0);
  const focusedPanel = useRef(0);
  useEffect(() => {
    if (focusPanel === focusedPanel.current || speakersOpen !== true) return;
    focusedPanel.current = focusPanel;
    document.getElementById(panelId)?.querySelector<HTMLElement>("h2")?.focus();
  }, [focusPanel, speakersOpen, panelId]);
  const voicesBefore = useRef(0);
  const speakersButton = useRef<HTMLButtonElement | null>(null);
  const rows = useRef(new Map<string, HTMLDivElement>());
  const nameField = useRef<HTMLInputElement | null>(null);
  const view = useRef<HTMLElement | null>(null);
  const pinnedHeader = useRef<HTMLDivElement | null>(null);
  // 12px is the root's gap (spacingVerticalM): the panel sticks where its column starts.
  usePanelSpace(view, pinnedHeader, 12);
  const audio = useRef<HTMLAudioElement | null>(null);
  const audioUrl = useRef<string | null>(null);
  // Find in the transcript (T12.98): page state only, cleared when the bar closes.
  const [findOpen, setFindOpen] = useState(false);
  const [findSpeaker, setFindSpeaker] = useState<string | null>(null);
  // The box's text, and the text find looks for: the box's once typing pauses (T14.17).
  const [findText, setFindText] = useState("");
  const [findLooked, setFindLooked] = useState("");
  const findPause = useRef<(() => void) | null>(null);
  const [findAt, setFindAt] = useState(0);
  // Bumped by every open (the button or Ctrl+F), so focus goes to the text box each time.
  const [findFocus, setFindFocus] = useState(0);
  const findBarId = useId();
  const findOpenRef = useRef(findOpen);
  findOpenRef.current = findOpen;
  const findButton = useRef<HTMLButtonElement | null>(null);
  const findInput = useRef<HTMLInputElement | null>(null);

  // Each read's number (T14.17): `asked` is the newest read asked, `applied` the newest whose
  // answer the page shows. An answer older than one already shown is dropped, so a slow older
  // read never puts back what an edit or sync just changed; any newer success is shown even
  // while a still newer read is out. A failure is shown only for the newest read asked when
  // nothing newer is shown. A read resolves once its own answer is shown, dropped or failed, so
  // an edit that awaits its read finds the page holding at least that read.
  const asked = useRef(0);
  const applied = useRef(0);
  // `afterSync`: read again because sync changed the meeting (T12.81). A meeting deleted on
  // another computer then goes quietly: no load error, and the list (read again) moves on.
  const reload = useCallback(async (afterSync = false) => {
    const read = ++asked.current;
    try {
      const [d, p] = await Promise.all([ipc.getMeeting(id), ipc.listPersons()]);
      if (read <= applied.current) return;
      applied.current = read;
      // Unchanged rows, word lists and movable lists keep their objects, so after an edit only
      // the lines that changed render again (T14.18).
      setDetail((before) => keepUnchanged(before, d));
      // Decided with each detail, in the same render: the first opens it exactly when a voice
      // needs a name; a later one opens it when more voices need a name than before (a
      // re-transcription found new ones). Never closed by itself. A voice recognised from a
      // voiceprint has a name, so it never opens it.
      const needing = voicesToName(d);
      const before = voicesBefore.current;
      voicesBefore.current = needing;
      setSpeakersOpen((open) => (open === null ? needing > 0 : needing > before ? true : open));
      setPersons(p.persons);
      // A newer read that failed still says so.
      if (read === asked.current) setLoadError(null);
    } catch (e) {
      if (read !== asked.current || applied.current > read) return;
      if (afterSync && e instanceof IpcFailure && e.error.kind === "meeting_not_found") {
        changedRef.current();
        return;
      }
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
  // Sync changed it here (another computer's summary, title or lines): read again (T12.81).
  const seenSynced = useRef(synced);
  useEffect(() => {
    if (seenSynced.current === synced) return;
    seenSynced.current = synced;
    void reload(true);
  }, [synced, reload]);
  const seenSummary = useRef(showSummary);
  useEffect(() => {
    if (seenSummary.current === showSummary) return;
    seenSummary.current = showSummary;
    setCited(null);
    setTab("summary");
  }, [showSummary]);

  // The summary this page shows counts as read on this computer (T12.76) once the window is
  // watched (visible and focused, as notifications judge it): the list's "New summary" mark goes.
  // Asked again when another summary is shown or the window comes back into view; the list is
  // read again only when something changed, also after the page has gone.
  const watched = useWindowWatched();
  const shownSummary = watched && tab === "summary" ? (detail?.summary?.id ?? null) : null;
  const changedRef = useRef(onChanged);
  changedRef.current = onChanged;
  useEffect(() => {
    if (shownSummary === null) return;
    ipc.markSummarySeen(id, shownSummary).then(
      (changed) => {
        if (changed) changedRef.current();
      },
      () => {},
    );
  }, [ipc, id, shownSummary]);

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
    if (row !== undefined) return scrollLineIntoView(row);
    view.current?.scrollIntoView?.({ block: "start" });
  }, [tab, cited]);

  // Find in the transcript (T12.98). The bar shows only on the Transcript tab: leaving it
  // closes and clears it, as close does.
  if (findOpen && tab !== "transcript") {
    setFindOpen(false);
    setFindSpeaker(null);
    setFindText("");
    setFindLooked("");
    setFindAt(0);
  }
  // A pause still running when the bar closes or the page goes looks for nothing.
  useEffect(() => {
    if (findOpen) return;
    findPause.current?.();
    findPause.current = null;
  }, [findOpen]);
  useEffect(() => () => findPause.current?.(), []);
  /** The box's text changed: find looks once typing pauses, at once when the box is cleared. */
  const typeFind = (t: string) => {
    setFindText(t);
    findPause.current?.();
    findPause.current = null;
    const look = () => {
      findPause.current = null;
      setFindLooked(t);
      setFindAt(0);
    };
    if (t.trim() === "") look();
    else findPause.current = timer.setTimeout(look, FIND_PAUSE_MS);
  };
  const findSpeakerList = useMemo(() => (detail === null ? [] : findSpeakers(detail.transcript)), [detail]);
  // A chosen speaker whose lines are gone (an edit moved them) is no longer offered: any speaker.
  const findChosen = findSpeakerList.some((s) => s.key === findSpeaker) ? findSpeaker : null;
  const findResult = useMemo<FindResult>(
    () => (findOpen && detail !== null ? findInTranscript(detail.transcript, findChosen, findLooked) : IDLE),
    [findOpen, detail, findChosen, findLooked],
  );
  const findTotal = findCount(findResult);
  const findIndex = findTotal === 0 ? 0 : Math.min(findAt, findTotal - 1);
  const findView = useMemo<FindView | null>(
    () => (findOpen ? { result: findResult, at: findIndex, speaker: findChosen } : null),
    [findOpen, findResult, findIndex, findChosen],
  );
  const openFind = useCallback(() => {
    setTab("transcript");
    setFindOpen(true);
    setFindFocus((n) => n + 1);
  }, []);
  const closeFind = () => {
    setFindOpen(false);
    setFindSpeaker(null);
    setFindText("");
    setFindLooked("");
    setFindAt(0);
    findButton.current?.focus();
  };
  const stepFind = (by: 1 | -1) => {
    // Enter before the pause is over looks at once, at the first place.
    if (findPause.current !== null) {
      findPause.current();
      findPause.current = null;
      setFindLooked(findText);
      setFindAt(0);
      return;
    }
    if (findTotal === 0) return;
    setFindAt((findIndex + by + findTotal) % findTotal);
  };

  // Ctrl+F opens find (T12.98) and never the webview's own; in the find box it selects its
  // text. In any other text field (a line or title being edited, a name field), a dialog or a
  // menu it does nothing at all: no bar, and focus stays where it is.
  // Esc closes and clears an open bar from anywhere in the detail but those places (the bar
  // handles its own, where an open speaker list closes first).
  const shown = detail !== null;
  const closeFindRef = useRef(closeFind);
  closeFindRef.current = closeFind;
  useEffect(() => {
    if (!shown) return;
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.defaultPrevented || e.altKey) return;
      const at = e.target instanceof Element ? e.target : null;
      if (e.key === "Escape") {
        if (!findOpenRef.current || e.ctrlKey || e.metaKey || e.shiftKey || at === null) return;
        const inDetail = at === document.body || (view.current?.contains(at) ?? false);
        if (!inDetail || at.closest(FIND_KEY_LEFT_ALONE)) return;
        if (at.getAttribute("aria-expanded") === "true") return;
        e.preventDefault();
        closeFindRef.current();
        return;
      }
      if (e.key.toLowerCase() !== "f" || !(e.ctrlKey || e.metaKey) || e.shiftKey) return;
      e.preventDefault();
      if (at !== null && at === findInput.current) {
        findInput.current.select();
        return;
      }
      if (at?.closest(FIND_KEY_LEFT_ALONE)) return;
      openFind();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [shown, openFind]);
  useEffect(() => {
    if (findFocus === 0) return;
    findInput.current?.focus();
    findInput.current?.select();
  }, [findFocus]);
  // The place find points at scrolls into the middle of the pane when it moves (not when the
  // meeting is read again, so an edit with the bar open keeps the pane where it is).
  const findRow = findLine(findResult, findIndex);
  const findRowId = findRow === null ? null : (detail?.transcript[findRow]?.segment_id ?? null);
  useEffect(() => {
    const row = findRowId === null ? undefined : rows.current.get(findRowId);
    if (row !== undefined) return scrollLineIntoView(row);
  }, [findRowId, findIndex]);

  /** The name of the person speaker `s` is linked to now, before an edit changes it. */
  const personNameOf = (s: string): string | null =>
    detail?.speakers.find((c) => c.id === s)?.person?.name ?? null;
  /** The name `choice` links to: a new person's typed name, or the listed person's. */
  const choiceName = (choice: LinkChoice): string =>
    choice.kind === "New" ? choice.name.trim() : (persons.find((p) => p.id === choice.person)?.name ?? "");

  /** Runs an edit and reads the meeting again; false when it was refused (the notice says why),
   * so a caller can keep what the user typed. */
  const run = async (f: () => Promise<unknown>, failTitle: string): Promise<boolean> => {
    try {
      await f();
    } catch (e) {
      setNotice({ key: "edit", intent: "error", title: failTitle, text: failureText(e) });
      return false;
    }
    await reload();
    onChanged();
    return true;
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
      setExportedId(id);
      setNotice({ key: "export", intent: "success", text: `Exported to ${done.note_path}` });
    } catch (e) {
      setNotice({ key: "export", intent: "warning", text: failureText(e) });
    }
  };
  // T14.63: a re-export replaces the whole note, so a meeting exported before (by its record,
  // or by an export from this page since the detail was read) asks first.
  const exportedBefore = (detail?.exported ?? false) || exportedId === id;
  const onExport = () => {
    if (exportedBefore) setAskingReexport(true);
    else void exportNote();
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

  // Re-summarise by one writer on this computer (T12.63b): no next step.
  const resummariseWith = async (step: StepChoice) => {
    try {
      await ipc.resummariseWith(id, step);
      onJobStarted("Resummarise");
      setNotice(null);
      onChanged();
    } catch (e) {
      setNotice({ key: "job", intent: "error", text: failureText(e) });
    }
  };

  // Re-summarise on another computer (T11.3db): the meeting shows it waits.
  // The waiting banner's Stop (T14.66): stopped, the page reads the meeting again (its run note
  // says so); otherwise the reason nothing was stopped.
  const stopWaiting = async () => {
    try {
      const why = await ipc.stopWaitingSummary(id);
      if (why !== null) setNotice({ key: "stop", intent: "info", text: why });
      await reload();
      onChanged();
    } catch {
      setNotice({ key: "stop", intent: "error", text: ORDER_COPY.stop.failed });
    }
  };

  const askHelper = async (device: string) => {
    try {
      await ipc.askSummaryHelper(id, device);
      // The page's waiting banner says it, read with the meeting (T12.81).
      setNotice((n) => (n?.key === "job" ? null : n));
      await reload();
      onChanged();
    } catch (e) {
      setNotice({ key: "job", intent: "error", text: failureText(e) });
    }
  };

  const title = displayTitle(item);
  const jobBar = job !== undefined && !jobAskedAnotherComputer(job) ? jobNotice(job) : null;
  const voices = detail ? voicesToName(detail) : 0;
  // Whether an action owner's name is the user, by the Speakers panel's one rule (`userNamed`):
  // null when the user has no name set (their own name then names no one), so the summary's
  // actions are one list.
  const me = persons.find((p) => p.is_me);
  const isUser =
    me !== undefined && userNamed(me.name, persons) !== null
      ? (name: string) => userNamed(name, persons) !== null
      : null;
  // A new summary being written replaces the one shown, so there is nothing settled to copy.
  const writingSummary = job !== undefined && job.job === "Resummarise" && !isTerminal(job.last);
  const summaryText = detail?.summary && !writingSummary ? summaryMarkdown(detail.summary) : "";
  // A new summary is being written, here or on another computer (T14.66): the header says so and
  // the out-of-date notes wait for it.
  const summaryWriting: SummaryWriting | null = writingSummary
    ? { on: null }
    : detail?.waiting_for != null
      ? { on: detail.waiting_device ?? ORDER_COPY.unnamed }
      : null;
  const summaryInProgress = summaryWriting !== null;
  // The runtime refuses line and word edits while the meeting is busy (recording, finishing, a
  // job on it): they are off here too, and a refusal that still comes is shown as before.
  const linesLocked =
    finishing || item.state === "Recording" || (job !== undefined && !isTerminal(job.last));
  const summariseLabel = detail !== null && detail.summary === null ? "Summarise" : "Re-summarise";
  const gone = detail !== null && recordingsGone(detail);
  const speakersName = voices > 0 ? `Speakers, ${voicesBadge(voices)}` : "Speakers";
  // Success and info banners with nothing but Dismiss hide by themselves (T14.59): a notice, and
  // a finished job's bar (a new job event, a new `seq`, starts the countdown again).
  // A countdown runs only while the window is watched, so a person away from it still finds the
  // banner when they come back. A summary job that wrote nothing ("Skipped", with its reason)
  // never hides by itself: it stays until dismissed.
  const skippedSummary = job?.last?.type === "Summary" && job.last.result.kind === "Skipped";
  const noticeHold = useAutoHide(timer, watched && notice !== null && hidesByItself(notice), notice, () =>
    setNotice(null),
  );
  const jobHold = useAutoHide(
    timer,
    watched &&
      jobBar !== null &&
      job !== undefined &&
      isTerminal(job.last) &&
      !skippedSummary &&
      hidesByItself(jobBar),
    jobBar === null ? null : job?.seq,
    onJobDismissed,
  );

  return (
    <section ref={view} className={styles.root} aria-label="Meeting detail">
      <div ref={pinnedHeader} className={styles.pinned}>
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
            {detail !== null && (
              <Caption1 className={styles.muted}>{headerLine(detail, summaryWriting)}</Caption1>
            )}
          </div>
          <div className={styles.headerButtons} role="group" aria-label="Meeting actions">
            <IconTip content={speakersName}>
              <Button
                ref={speakersButton}
                aria-expanded={speakersOpen === true}
                aria-controls={speakersOpen === true ? panelId : undefined}
                aria-label={speakersName}
                disabled={detail === null}
                icon={
                  <span className={styles.iconWithCount}>
                    <PeopleIcon />
                    {voices > 0 && (
                      <CounterBadge
                        className={styles.iconCount}
                        count={voices}
                        appearance="filled"
                        color="brand"
                        size="small"
                      />
                    )}
                  </span>
                }
                onClick={() => {
                  // Opened by the user: focus goes into the panel, which follows the transcript in
                  // the page (T14.19). An automatic open moves nothing.
                  if (speakersOpen !== true) setFocusPanel((n) => n + 1);
                  setSpeakersOpen((open) => open !== true);
                }}
              />
            </IconTip>
            <IconTip content="Export to Obsidian">
              <Button
                aria-label="Export to Obsidian"
                icon={<ExportIcon />}
                onClick={onExport}
                disabled={detail === null}
                {...restoreFocus}
              />
            </IconTip>
            <ReexportQuestion
              open={askingReexport}
              onExport={() => void exportNote()}
              onClose={() => setAskingReexport(false)}
            />
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
            {gone ? (
              // Focusable, so a keyboard reaches the reason.
              <Tooltip content={RECORDING_GONE_TIP} relationship="description">
                <Button disabledFocusable aria-label="Re-transcribe" icon={<RetranscribeIcon />} />
              </Tooltip>
            ) : finishing ? (
              <Tooltip content={finishingTip("Re-transcribe")} relationship="description">
                <Button disabledFocusable aria-label="Re-transcribe" icon={<RetranscribeIcon />} />
              </Tooltip>
            ) : (
              <IconTip content="Re-transcribe">
                <Button
                  aria-label="Re-transcribe"
                  icon={<RetranscribeIcon />}
                  disabled={detail === null}
                  onClick={() => void startJob("retranscribe")}
                />
              </IconTip>
            )}
            {finishing ? (
              <Tooltip content={finishingTip(summariseLabel)} relationship="description">
                <Button disabledFocusable aria-label={summariseLabel} icon={<ResummariseIcon />} />
              </Tooltip>
            ) : (
              // Its tooltip is on the split button itself (ResummariseControl).
              <ResummariseControl
                meeting={id}
                label={summariseLabel}
                icon={<ResummariseIcon />}
                disabled={detail === null}
                onOrder={() => void startJob("resummarise")}
                onWriter={(step) => void resummariseWith(step)}
                onAsk={(device) => void askHelper(device)}
              />
            )}
          </div>
        </div>
        {detail !== null && (
          <div className={styles.tabRow}>
            <TabList selectedValue={tab} onTabSelect={(_, d) => setTab(d.value as Tab)}>
              <Tab value="summary">Summary</Tab>
              <Tab value="transcript">Transcript</Tab>
            </TabList>
            <FindButton
              ref={findButton}
              expanded={findOpen}
              controls={findOpen ? findBarId : undefined}
              // A second press closes it (and clears it, as Esc does), so aria-expanded holds.
              onClick={() => (findOpen ? closeFind() : openFind())}
            />
          </div>
        )}
        {detail !== null && findOpen && (
          <TranscriptFind
            id={findBarId}
            speakers={findSpeakerList}
            speaker={findChosen}
            text={findText}
            count={findTotal}
            countText={
              // While typing pauses nothing has been looked for yet: no hint that says to type.
              findResult.kind === "idle" && findText !== findLooked && findText.trim() !== ""
                ? ""
                : findCountText(findResult, findIndex, findLooked)
            }
            inputRef={findInput}
            onSpeaker={(k) => {
              setFindSpeaker(k);
              setFindAt(0);
            }}
            onText={typeFind}
            onStep={stepFind}
            onClose={closeFind}
          />
        )}
        {/* Every banner stays in view with the buttons while the page scrolls (T14.58). */}
        {status}
        {/* The one banner for a summary written on another computer (T12.81): on either tab, and
            gone once the summary arrives or the request settles, by sync or here. */}
        {detail?.waiting_for != null && (
          <MessageBar intent="info" role="status">
            <MessageBarBody>
              <MessageBarTitle>{WAITING_TITLE}</MessageBarTitle>
              {detail.waiting_for}
            </MessageBarBody>
            <MessageBarActions>
              {/* T14.66: withdraws this computer's request and ends the run. */}
              <Button
                size="small"
                aria-label={fillCopy(ORDER_COPY.stop.label, {
                  device: detail.waiting_device ?? ORDER_COPY.unnamed,
                })}
                onClick={() => void stopWaiting()}
              >
                {ORDER_COPY.stop.button}
              </Button>
            </MessageBarActions>
          </MessageBar>
        )}
        {jobBar !== null && (
          <MessageBar intent={jobBar.intent} {...jobHold}>
            <MessageBarBody>
              {jobBar.title !== undefined && <MessageBarTitle>{jobBar.title}</MessageBarTitle>}
              {jobBar.text}
            </MessageBarBody>
            <MessageBarActions>
              {jobBar.busy ? (
                <>
                  <Spinner size="extra-tiny" aria-label="Working" />
                  {/* Focusable while disabled, so focus stays on it after the press. */}
                  <Button
                    size="small"
                    aria-label={job?.job === "Resummarise" ? "Cancel the new summary" : "Cancel the re-transcription"}
                    disabledFocusable={job?.cancelling === true}
                    onClick={onJobCancel}
                  >
                    Cancel
                  </Button>
                </>
              ) : (
                <Button size="small" appearance="transparent" onClick={onJobDismissed}>
                  Dismiss
                </Button>
              )}
            </MessageBarActions>
          </MessageBar>
        )}
        {notice !== null && (
          <MessageBar intent={notice.intent} {...noticeHold}>
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
        {detail !== null && tab === "summary" && (
          <SummaryBanners
            detail={detail}
            inProgress={summaryInProgress}
            onResummarise={() => void startJob("resummarise")}
            onAllowWriter={() => void run(() => ipc.allowSummaryWriter(id), "The meeting could not be changed")}
          />
        )}
      </div>

      {detail === null && loadError === null && <Spinner label="Loading the meeting" />}

      {detail !== null && (
        <>
          <div className={styles.columns}>
            <div className={styles.main}>
              {tab === "summary" ? (
                <SummaryTab
                  detail={detail}
                  isUser={isUser}
                  onCite={openCitation}
                  onEdit={
                    writingSummary
                      ? undefined
                      : (summary, field, text) =>
                          save(() => ipc.editSummary(id, summary, field, text), "The summary was not changed")
                  }
                  onUndo={() => void run(() => ipc.undoSummary(id), "The previous summary could not be restored")}
                />
              ) : (
                <TranscriptTab
                  detail={detail}
                  cited={cited}
                  find={findView}
                  locked={linesLocked}
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
                  onMoveLine={(segment, speaker) =>
                    void save(() => ipc.moveLine(id, segment, speaker), "The line was not moved")
                  }
                  onSplitLine={(segment, token) =>
                    save(() => ipc.splitLine(id, segment, token), "The line was not split")
                  }
                  wordActions={(segment) => ({
                    onUse: (token, word) =>
                      void save(() => ipc.useWord(id, segment, token, word), "The word was not changed"),
                    onPutBack: (token) =>
                      void save(() => ipc.putBackWord(id, segment, token), "The word was not put back"),
                    onKeep: (token) => save(() => ipc.keepWord(id, segment, token), "The word was not kept"),
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
                    linesLocked={linesLocked}
                    onClose={() => {
                      setSpeakersOpen(false);
                      // The Close button leaves with the panel: focus goes back where it opens.
                      speakersButton.current?.focus();
                    }}
                    onPlay={(s) => void play(s)}
                    onManagePeople={onManagePeople}
                    onLink={(s, choice: LinkChoice) =>
                      run(async () => {
                        const was = personNameOf(s);
                        const result = await ipc.linkSpeaker(id, s, choice);
                        const text = linkedText(was, choiceName(choice), result);
                        if (text !== null) setNotice({ key: "edit", intent: "success", text });
                      }, "The speaker was not linked")
                    }
                    onMarkMe={(s) =>
                      run(async () => {
                        const was = personNameOf(s);
                        const marked = await ipc.markMe(id, s);
                        setMergeOffers(marked.offers);
                        const text = markedMeText(was, marked);
                        if (text !== null) setNotice({ key: "edit", intent: "success", text });
                      }, "The voice was not marked as you")
                    }
                    mergeOffer={mergeOffers[0] ?? null}
                    onMergeIntoMe={(person) =>
                      run(async () => {
                        await ipc.mergeIntoMe(person);
                        const name = mergeOffers.find((p) => p.id === person)?.name ?? "";
                        setMergeOffers((offers) => offers.filter((p) => p.id !== person));
                        setNotice({ key: "edit", intent: "success", text: `${name} is now merged into you.` });
                      }, "The person was not merged into you")
                    }
                    onKeepSeparate={() => setMergeOffers((offers) => offers.slice(1))}
                    onUnmarkMe={(s) =>
                      run(async () => {
                        await ipc.unmarkMe(id, s);
                        setMergeOffers([]);
                      }, "The voice was not changed")
                    }
                    onMerge={(keep, merge) =>
                      run(async () => {
                        const was = personNameOf(merge);
                        const result = await ipc.mergeSpeakers(id, keep, merge);
                        const text = mergedText(was, result);
                        if (text !== null) setNotice({ key: "edit", intent: "success", text });
                      }, "The speakers were not merged")
                    }
                    onSave={(s, choice, name) =>
                      run(async () => {
                        const was = personNameOf(s);
                        const saved = await ipc.saveVoice(id, s, choice);
                        const kept =
                          saved.outcome === "Remembered" ||
                          saved.outcome === "Replaced" ||
                          saved.outcome === "AlreadyRemembered";
                        setNotice({
                          key: "edit",
                          intent: kept ? "success" : "info",
                          text: savedVoiceText(was, name, saved),
                        });
                      }, "The voice was not saved")
                    }
                    onIgnore={(s) => run(() => ipc.ignoreSpeaker(id, s), "The voice was not ignored")}
                    onUnignore={(s) => run(() => ipc.unignoreSpeaker(id, s), "The voice was not changed")}
                    onNotThisPerson={(s) =>
                      run(async () => {
                        const was = personNameOf(s) ?? "";
                        const result = await ipc.notThisPerson(id, s);
                        setNotice({ key: "edit", intent: "success", text: notThisPersonText(was, result) });
                      }, "The voice was not changed")
                    }
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

  const label = copied ? "Copied" : "Copy summary";
  // One button at every width, so a change of form never moves focus off it.
  return (
    <IconTip content={label}>
      <Button
        // The live text is a child, so Button's own icon-only sizing does not apply: the same
        // size is set here.
        className={styles.iconOnlyButton}
        icon={<CopyIcon />}
        onClick={() => void copy()}
      >
        <span aria-live="polite" className={styles.srOnly} data-sr-only="">
          {label}
        </span>
      </Button>
    </IconTip>
  );
}

/**
 * A header button's tooltip: its full label, shown on hover and focus (every header button shows
 * only its icon, the owner 2026-11-28). The button names itself (`aria-label`), so the tooltip
 * adds nothing for a screen reader.
 */
function IconTip({ content, children }: { content: string; children: ReactElement }) {
  return (
    <Tooltip content={content} relationship="inaccessible">
      {children}
    </Tooltip>
  );
}

/** Two arrows in a circle, for Re-transcribe. */
function RetranscribeIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 20 20" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M15.5 8A6 6 0 0 0 4.75 6.5M4.5 12a6 6 0 0 0 10.75 1.5" strokeLinecap="round" />
      <path d="M4.5 3.5v3h3M15.5 16.5v-3h-3" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

/** A page with lines and a spark, for Re-summarise. */
function ResummariseIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 20 20" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M11 3.5H5.5A1.5 1.5 0 0 0 4 5v10a1.5 1.5 0 0 0 1.5 1.5h9A1.5 1.5 0 0 0 16 15V9" strokeLinecap="round" />
      <path d="M7 10h6M7 13h4" strokeLinecap="round" />
      <path d="M15 2.5v4M13 4.5h4" strokeLinecap="round" />
    </svg>
  );
}

/** Two people, for Speakers when it shows only its icon. */
function PeopleIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 20 20" fill="none" stroke="currentColor" aria-hidden="true">
      <circle cx="7.5" cy="6.5" r="2.75" />
      <path d="M2.5 16c0-2.75 2.25-4.75 5-4.75s5 2 5 4.75" strokeLinecap="round" />
      <circle cx="13.75" cy="7.25" r="2.25" />
      <path d="M13.5 11.5c2.25 0 4 1.75 4 4.25" strokeLinecap="round" />
    </svg>
  );
}

/** A box with an arrow leaving it, for Export to Obsidian when it shows only its icon. */
function ExportIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 20 20" fill="none" stroke="currentColor" aria-hidden="true">
      <path
        d="M8.5 4H5a1.5 1.5 0 0 0-1.5 1.5v9.5A1.5 1.5 0 0 0 5 16.5h9.5A1.5 1.5 0 0 0 16 15v-3.5"
        strokeLinecap="round"
      />
      <path d="M11.5 3.5h5v5M16.5 3.5L9.5 10.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

/** Two sheets, for Copy summary when it shows only its icon. */
function CopyIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 20 20" fill="none" stroke="currentColor" aria-hidden="true">
      <rect x="6.5" y="6.5" width="10" height="10.5" rx="1.5" />
      <path d="M13.5 6.5V4.5A1.5 1.5 0 0 0 12 3H5A1.5 1.5 0 0 0 3.5 4.5v8A1.5 1.5 0 0 0 5 14h1.5" />
    </svg>
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
  /** The icon of its button: a pencil, or a calendar for a due date. */
  icon: EditIcon;
  /** A label shown above the editor, for a field whose editor hides what it belongs to. */
  shown?: string;
  text: string;
  onSave(text: string): Promise<boolean>;
}

/** The width of one summary edit icon, which its item keeps free on its right. */
const EDIT_ICON_PX = 24;

/** Which icon a summary field's edit button shows. */
type EditIcon = "pencil" | "calendar";

/** The small icons of the summary's edit buttons (T12.105). */
function EditFieldIcon({ icon }: { icon: EditIcon }) {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      {icon === "pencil" ? (
        <path d="M10.5 2.5l3 3L6 13H3v-3l7.5-7.5zM9 4l3 3" />
      ) : (
        <path d="M3 4.5h10v8.5H3zM3 7h10M5.5 3v3M10.5 3v3" />
      )}
    </svg>
  );
}

/** Summary content with edit icons at its end, shown on hover and focus in space they always
 * keep; a field being edited replaces the content with its editor. With no fields it is the
 * content alone. */
function Editable({ fields, children }: { fields: EditField[]; children: ReactNode }) {
  const styles = useStyles();
  const [editing, setEditing] = useState<EditField | null>(null);
  if (editing !== null) {
    return (
      <TextEditor
        label={editing.label}
        shown={editing.shown}
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
    <span
      className={styles.editable}
      style={fields.length > 0 ? { paddingRight: `${fields.length * EDIT_ICON_PX + 4}px` } : undefined}
    >
      {children}
      {fields.length > 0 && (
        <span className={styles.editIcons}>
          {fields.map((f) => (
            <Tooltip key={f.label} content={`Edit ${f.label}`} relationship="label">
              <Button
                size="small"
                appearance="subtle"
                icon={<EditFieldIcon icon={f.icon} />}
                className={mergeClasses(styles.inlineEdit, "earshot-inline-edit")}
                onClick={() => setEditing(f)}
              />
            </Tooltip>
          ))}
        </span>
      )}
    </span>
  );
}

function SummaryBody({
  summary,
  isUser,
  onCite,
  onEdit,
}: {
  summary: SummaryView;
  /** Whether an owner's name is the user (`userNamed`); null when the user has set no name
   * (the actions are then one list). */
  isUser: ((name: string) => boolean) | null;
  onCite(c: Citation): void;
  /** Saves one text; false when it was refused. Absent: read only. */
  onEdit?: (field: SummaryFieldRef, text: string) => Promise<boolean>;
}) {
  const styles = useStyles();
  /** The field as an Edit button, or none when read only. */
  const edit = (
    field: SummaryFieldRef,
    label: string,
    text: string,
    icon: EditIcon = "pencil",
    shown?: string,
  ): EditField[] => (onEdit === undefined ? [] : [{ label, icon, shown, text, onSave: (t) => onEdit(field, t) }]);

  // Actions (T14.60): "My actions" and "Other actions" when the user has a name, else one list.
  // Each owner is split into names (`ownerNames`), and a name is the user by `userNamed`.
  const groups = actionGroups(summary.actions, isUser);
  /** One group's list. `show`: "with" names the user's partners after the citations, "owners"
   * names every owner. Editing: each pencil edits one stored action. An item that merges several
   * stored actions offers each one's pencils, named with its owner, rather than one pencil that
   * would write several actions in turn: those writes are separate requests, so a refusal or a
   * failure part way would leave the actions half changed. After such an edit the actions differ
   * and show as separate items. A render function, not a component, so an open editor survives
   * the page's re-renders. */
  const actionList = (label: string, items: ActionItem[], show: "with" | "owners", heading: boolean) => (
    <div className={styles.actionGroup} key={label}>
      {heading && (
        <Body1Strong as="h3" className={styles.sectionHeading}>
          {label}
        </Body1Strong>
      )}
      <ul className={styles.list} aria-label={label}>
        {items.map((item) => {
          const merged = item.indices.length > 1;
          const fields = item.indices.flatMap((i) => {
            const a = summary.actions[i];
            const whose = merged ? ` for ${a.owner}` : "";
            return [
              ...edit({ kind: "ActionTask", index: i }, `action ${i + 1}${whose}`, a.task),
              ...(a.due === null
                ? []
                : edit(
                    { kind: "ActionDue", index: i },
                    `when action ${i + 1} is due${whose}`,
                    a.due,
                    "calendar",
                    // The editor replaces the action's text, so the date alone needs a label.
                    `When action ${i + 1} is due`,
                  )),
              ...edit({ kind: "ActionOwner", index: i }, `who does action ${i + 1}${whose}`, a.owner),
            ];
          });
          const names = shownOwners(item, show === "with", isUser);
          const owners =
            show === "owners" ? joinNames(names) : names.length > 0 ? `with ${joinNames(names)}` : null;
          return (
            <li key={item.indices[0]}>
              <Editable fields={fields}>
                <Body1>
                  {item.task}
                  {item.due !== null && ` (by ${item.due})`}
                </Body1>
                <Citations citations={item.citations} onCite={onCite} />
                {owners !== null && <Caption1 className={styles.actionOwners}>{` · ${owners}`}</Caption1>}
              </Editable>
            </li>
          );
        })}
      </ul>
    </div>
  );
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
          {groups.kind === "split" ? (
            <>
              {groups.mine.length > 0 && actionList("My actions", groups.mine, "with", true)}
              {groups.others.length > 0 && actionList("Other actions", groups.others, "owners", true)}
            </>
          ) : (
            actionList("Actions", groups.all, "owners", false)
          )}
        </div>
      )}
      {/* The topic sections (T14.70): under one "Notes" heading after a thin divider, each a
          sub-heading like "My actions"; no Notes heading when there is no topic. The Obsidian
          note follows ("## Notes", each topic "###"). */}
      {summary.notes.length > 0 && (
        <>
          <Divider className={styles.notesDivider} />
          <div className={styles.notes}>
            <Subtitle2 as="h2" className={styles.sectionHeading}>
              Notes
            </Subtitle2>
            {summary.notes.map((n, i) => (
              <div className={styles.actionGroup} key={`notes-${i}`}>
                <Body1Strong as="h3" className={styles.sectionHeading}>
                  <Editable
                    fields={edit({ kind: "NoteHeading", section: i }, `heading ${n.heading}`, n.heading)}
                  >
                    {n.heading}
                  </Editable>
                </Body1Strong>
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
          </div>
        </>
      )}
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

/** The Summary tab's banners, pinned at the top of the page with the header while that tab
 * shows (T14.58). */
function SummaryBanners({
  detail,
  inProgress,
  onResummarise,
  onAllowWriter,
}: {
  detail: MeetingDetail;
  /** A new summary is being written (T14.66): the out-of-date notes wait for it. */
  inProgress: boolean;
  onResummarise(): void;
  onAllowWriter(): void;
}) {
  return (
    <>
      {detail.summary_stale && !inProgress && (
        <MessageBar intent="warning">
          <MessageBarBody>
            {detail.summary_stale_names
              ? "This summary uses a speaker's earlier name. Re-summarise to use the current names."
              : "This summary refers to lines of an earlier transcript. Re-summarise to match the current one."}
          </MessageBarBody>
          <MessageBarActions>
            <Button size="small" onClick={onResummarise}>
              Re-summarise
            </Button>
          </MessageBarActions>
        </MessageBar>
      )}
      {detail.summary_run !== null && detail.waiting_for === null && (
        <MessageBar intent="info" role="status">
          <MessageBarBody>{detail.summary_run}</MessageBarBody>
        </MessageBar>
      )}
      {/* Only where it offers something (T12.81): its "Let <writer> summarise it". Not while
          another computer writes the summary, whose banner is the page's one word on it. */}
      <KeptLocal
        keptLocal={
          detail.waiting_for === null && detail.kept_local?.writer != null ? detail.kept_local : null
        }
        onAllow={onAllowWriter}
      />
    </>
  );
}

function SummaryTab({
  detail,
  isUser,
  onCite,
  onEdit,
  onUndo,
}: {
  detail: MeetingDetail;
  /** Whether an owner's name is the user; null when the user has set no name. */
  isUser: ((name: string) => boolean) | null;
  onCite(c: Citation): void;
  /** Saves one text of summary `summary`; false when refused. Absent while a new summary is
   * being written. */
  onEdit?: (summary: string, field: SummaryFieldRef, text: string) => Promise<boolean>;
  onUndo(): void;
}) {
  const styles = useStyles();
  const s = detail.summary;
  return (
    <>
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
          isUser={isUser}
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
  shown,
  initial,
  multiline,
  onSave,
  onCancel,
}: {
  label: string;
  /** A visible label, when the editor needs one. */
  shown?: string;
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
  const fieldId = useId();
  return (
    <div className={styles.editor}>
      {shown !== undefined && (
        <Label className={styles.editorLabel} htmlFor={fieldId}>
          {shown}
        </Label>
      )}
      {multiline ? (
        <Textarea
          id={fieldId}
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
          id={fieldId}
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
  find,
  onEdit,
  wordActions,
  onAddWord,
  onRemoveWord,
  onMoveLine,
  onSplitLine,
  rowRef,
  locked,
}: {
  detail: MeetingDetail;
  cited: string | null;
  /** What find shows (T12.98), `null` while its bar is closed. */
  find: FindView | null;
  /** Line edits and word choices are off: the meeting is busy. */
  locked: boolean;
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
  /** Moves a line to `speaker` (one of its track's `movable_by_track`), or to a new speaker for `null`
   * (T12.90; also gives a line with an unknown speaker one, T12.65); the detail is read again
   * after it. */
  onMoveLine(segment: string, speaker: string | null): void;
  /** Splits a line before token `token` (T12.90); whether it was split (the detail is read
   * again first). */
  onSplitLine(segment: string, token: number): Promise<boolean>;
  rowRef(segment: string, el: HTMLDivElement | null): void;
}) {
  const styles = useStyles();
  const [editing, setEditing] = useState<string | null>(null);
  // T12.90: the line whose words are offered as split points.
  const [splitting, setSplitting] = useState<string | null>(null);
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
  // `edit`: the line's Edit button, only when focus fell to the page (the control that had it
  // left the DOM). `line`: the line itself, the transcript's tab stop, on the same condition
  // (after its editor closed, T14.19).
  const [focusNext, setFocusNext] = useState<
    { undo: true } | { word: string } | { edit: string } | { line: string } | null
  >(null);
  const undoRef = useRef<HTMLButtonElement | null>(null);
  const tableRef = useRef<HTMLDivElement | null>(null);
  const keysId = useId();
  // The transcript is one tab stop (T14.19, roving tabindex): the line last focused, else the
  // first. A line's buttons are reached from it with the arrow keys (`lineKeys`).
  const [active, setActive] = useState<string | null>(null);
  const activeId =
    active !== null && detail.transcript.some((r) => r.segment_id === active)
      ? active
      : (detail.transcript[0]?.segment_id ?? null);
  useEffect(() => {
    if (focusNext === null) return;
    if ("undo" in focusNext) undoRef.current?.focus();
    else if ("edit" in focusNext || "line" in focusNext) {
      const at = document.activeElement;
      if (at === null || at === document.body || !at.isConnected) {
        if ("edit" in focusNext) lineEditButton(tableRef.current, focusNext.edit)?.focus();
        else lineRow(tableRef.current, focusNext.line)?.focus();
      }
    } else
      tableRef.current
        ?.querySelector<HTMLElement>(`[data-offered-word="${CSS.escape(focusNext.word)}"]`)
        ?.focus();
    setFocusNext(null);
  }, [focusNext]);
  // The line holding focus went away (an action removed it, or a read no longer has it): focus
  // goes to the line now at its place, else the last, so the keyboard is never left on the page.
  const activeIndex = useRef(0);
  // Focus was last in the transcript: set by a line taking focus, cleared when it moves to
  // something outside the table.
  const focusInside = useRef(false);
  useEffect(() => {
    if (!focusInside.current) return;
    if (active === null || detail.transcript.some((r) => r.segment_id === active)) return;
    const at = document.activeElement;
    if (at !== null && at !== document.body && at.isConnected) return;
    const lines = tableRef.current?.querySelectorAll<HTMLElement>(':scope > [role="row"]');
    if (lines === undefined || lines.length === 0) return;
    lines[Math.min(activeIndex.current, lines.length - 1)].focus();
  }, [detail.transcript, active]);

  // What a line does, read at the moment it is used: the rows get one object that never
  // changes, so a row renders only when its own props do (T9.24).
  const latest = useRef<Omit<LineHandlers, "rowRef"> | null>(null);
  // A row's ref is attached before the layout effect below sets `latest` on the first commit,
  // so it has its own ref, which starts as this render's (it only touches the parent's map).
  const rowRefLatest = useRef(rowRef);
  const handlers = useMemo<LineHandlers>(
    () => ({
      contextMenu: (r, e) => latest.current?.contextMenu(r, e),
      keyDown: (r, e) => latest.current?.keyDown(r, e),
      focusRow: (segment) => latest.current?.focusRow(segment),
      startEdit: (segment) => latest.current?.startEdit(segment),
      saveEdit: async (segment, text) => latest.current?.saveEdit(segment, text),
      cancelEdit: (segment) => latest.current?.cancelEdit(segment),
      toggle: (segment, word) => latest.current?.toggle(segment, word),
      addFromBar: (segment, word) => latest.current?.addFromBar(segment, word),
      closeBar: (segment) => latest.current?.closeBar(segment),
      keep: (segment, token) => latest.current?.keep(segment, token),
      undo: (segment, word) => latest.current?.undo(segment, word),
      move: (segment, speaker) => latest.current?.move(segment, speaker),
      startSplit: (segment) => latest.current?.startSplit(segment),
      split: (segment, token) => latest.current?.split(segment, token),
      cancelSplit: (segment) => latest.current?.cancelSplit(segment),
      rowRef: (segment, el) => rowRefLatest.current(segment, el),
      wordActions: (segment) =>
        latest.current?.wordActions(segment) ?? { onUse: noop, onPutBack: noop, onKeep: noop },
    }),
    [],
  );

  // Only the final transcript can be corrected: the final pass replaces a draft.
  const canEdit = detail.header.transcript === "Final";

  // Each line's find state (T12.98). A line keeps the object it had while its highlight is
  // unchanged, so a keystroke renders only the lines whose highlight changes (T9.24).
  const lastFinds = useRef(new Map<string, LineFind>());
  const lineFinds = useMemo(() => {
    const next = lineFindsOf(detail.transcript, find);
    for (const [id, f] of next) {
      const before = lastFinds.current.get(id);
      if (before !== undefined && sameLineFind(before, f)) next.set(id, before);
    }
    lastFinds.current = next;
    return next;
  }, [detail.transcript, find]);

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
    for (const t of tokensOf(r.text)) {
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
    // A plain word takes no focus: the menu gives it back to the line's Edit button.
    const back = el.tagName === "BUTTON" ? el : lineEditButton(e.currentTarget, r.segment_id);
    openMenu(r.segment_id, [word], pointTarget(e.clientX, e.clientY), back);
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
  /** Moving through the transcript from the keyboard (T14.19): on a line, up and down (Home,
   * End) go to other lines, right or Enter to its first button (Enter: its Edit button; an edit
   * still starts only from that button); on a line's button, left and right go through the
   * line's buttons and back to the line, up and down to the other lines. A line being edited or
   * split, its Dictionary bar and anything portalled (menus, a word's popover) keep their keys. */
  const lineKeys = (r: TranscriptRow, e: KeyboardEvent<HTMLDivElement>) => {
    if (e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return;
    if (editing === r.segment_id || splitting === r.segment_id) return;
    const row = e.currentTarget;
    const at = e.target as HTMLElement;
    if (!row.contains(at)) return;
    const onRow = at === row;
    const controls = [...row.querySelectorAll<HTMLElement>(LINE_CONTROL)];
    const k = controls.indexOf(at);
    if (!onRow && k < 0) return;
    // A menu button opens its menu with up and down (Fluent's trigger or `LineMenu`), and an
    // open popup keeps its keys: those never move to another line.
    const vertical = e.key === "ArrowDown" || e.key === "ArrowUp";
    if (vertical && (at.getAttribute("aria-haspopup") === "menu" || at.getAttribute("aria-expanded") === "true")) {
      return;
    }
    const lines = [...(tableRef.current?.querySelectorAll<HTMLElement>(':scope > [role="row"]') ?? [])];
    const i = lines.indexOf(row);
    let to: HTMLElement | null | undefined;
    switch (e.key) {
      case "ArrowDown":
        to = lines[i + 1];
        break;
      case "ArrowUp":
        to = i > 0 ? lines[i - 1] : undefined;
        break;
      case "PageDown":
      case "PageUp": {
        const by = linesPerPage(row);
        to = lines[Math.max(0, Math.min(lines.length - 1, i + (e.key === "PageDown" ? by : -by)))];
        break;
      }
      case "Home":
      case "End":
        if (!onRow) return;
        to = e.key === "Home" ? lines[0] : lines.at(-1);
        break;
      case "ArrowRight":
        to = onRow ? controls[0] : controls[k + 1];
        break;
      case "ArrowLeft":
        if (onRow) return;
        to = k === 0 ? row : controls[k - 1];
        break;
      case "Enter":
        if (!onRow) return;
        to = lineEditButton(row, r.segment_id) ?? controls[0];
        break;
      default:
        return;
    }
    e.preventDefault();
    to?.focus();
  };

  // Set once a render is committed: a render React discards leaves the committed handlers.
  useLayoutEffect(() => {
    rowRefLatest.current = rowRef;
    latest.current = {
      contextMenu: (r, e) => onRowContextMenu(r)(e),
      keyDown: (r, e) => {
        onRowKeyDown(r)(e);
        lineKeys(r, e);
      },
      focusRow: (segment) => {
        focusInside.current = true;
        activeIndex.current = Math.max(
          0,
          detail.transcript.findIndex((r) => r.segment_id === segment),
        );
        setActive(segment);
      },
      startEdit: (segment) => {
        setEditing(segment);
        setSplitting(null);
        setOffer(null);
        setNote(null);
      },
      saveEdit: async (segment, text) => {
        const offered = await onEdit(segment, text);
        if (offered === null) return;
        setEditing(null);
        setOffer(offered.length > 0 ? { segment, words: offered } : null);
        setFocusNext({ line: segment });
      },
      cancelEdit: (segment) => {
        setEditing(null);
        setFocusNext({ line: segment });
      },
      toggle,
      addFromBar: (segment, word) => void add(segment, word, true),
      closeBar: (segment) => {
        setOffer(null);
        setNote(null);
        setFocusNext({ edit: segment });
      },
      keep: (segment, token) =>
        void Promise.resolve(wordActions(segment).onKeep(token)).then(() => setFocusNext({ edit: segment })),
      undo: (segment, word) => void undo(segment, word),
      move: onMoveLine,
      startSplit: (segment) => {
        setSplitting(segment);
        setEditing(null);
      },
      split: (segment, token) =>
        void onSplitLine(segment, token).then((done) => {
          if (done) {
            setSplitting(null);
            setFocusNext({ edit: segment });
          }
        }),
      cancelSplit: (segment) => {
        setSplitting(null);
        setFocusNext({ edit: segment });
      },
      wordActions,
    };
  });
  if (detail.transcript.length === 0) {
    return <Body1 className={styles.muted}>No speech was transcribed in this meeting.</Body1>;
  }

  return (
    <>
    <span id={keysId} className={styles.hiddenText}>
      {TRANSCRIPT_KEYS_COPY}
    </span>
    {/* A table, not a grid: lines are read as rows with their time, speaker and words as
        cells, and the keys move by line, not by cell, which a grid would promise (T14.19). */}
    <div
      role="table"
      aria-label="Transcript"
      aria-describedby={keysId}
      className={styles.transcript}
      ref={tableRef}
      onBlur={(e) => {
        const to = e.relatedTarget;
        if (to instanceof Node && !e.currentTarget.contains(to)) focusInside.current = false;
      }}
    >
      {detail.transcript.map((r, i) => {
        const lineNote = note?.segment === r.segment_id ? note : null;
        return (
          <TranscriptLine
            key={r.segment_id}
            row={r}
            index={i}
            tabStop={r.segment_id === activeId}
            movable={detail.movable_by_track[r.track_id] ?? NO_SPEAKERS}
            cited={r.segment_id === cited}
            // A line being edited or split shows no highlights.
            find={editing === r.segment_id || splitting === r.segment_id ? undefined : lineFinds.get(r.segment_id)}
            editing={editing === r.segment_id}
            splitting={splitting === r.segment_id}
            canEdit={canEdit}
            locked={locked}
            wordList={detail.word_list}
            dictionary={detail.dictionary}
            offered={offer?.segment === r.segment_id ? offer.words : null}
            note={lineNote}
            others={
              lineNote?.kind === "added"
                ? otherLinesSuggesting(detail.transcript, r.segment_id, lineNote.word)
                : 0
            }
            undoRef={undoRef}
            handlers={handlers}
          />
        );
      })}
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
    </>
  );
}

const noop = () => {};

/** No speaker to move a line to: one array, so a line's props stay equal (T9.24). */
const NO_SPEAKERS: MovableSpeaker[] = [];

/** Find with nothing chosen (T12.98). */
const IDLE: FindResult = { kind: "idle" };

/** Where Ctrl+F opens nothing and Esc leaves find open (T12.98): other text fields, dialogs
 * and menus. */
const FIND_KEY_LEFT_ALONE =
  'input, textarea, select, [contenteditable]:not([contenteditable="false"]), [role="dialog"], [role="alertdialog"], [role="menu"]';

/** What find shows in the transcript (T12.98): its result, the place it points at and the
 * chosen speaker. */
interface FindView {
  result: FindResult;
  at: number;
  speaker: string | null;
}

/** Each line's find state by segment id; only lines find touches have one. */
function lineFindsOf(rows: TranscriptRow[], find: FindView | null): Map<string, LineFind> {
  const out = new Map<string, LineFind>();
  if (find === null) return out;
  const get = (i: number): LineFind => {
    const id = rows[i].segment_id;
    let f = out.get(id);
    if (f === undefined) {
      f = { speaker: false, currentTurn: false, matches: [], current: -1 };
      out.set(id, f);
    }
    return f;
  };
  if (find.speaker !== null) {
    rows.forEach((r, i) => {
      if (speakerKey(r) === find.speaker) get(i).speaker = true;
    });
  }
  if (find.result.kind === "turns") {
    const line = find.result.lines[find.at];
    if (line !== undefined) get(line).currentTurn = true;
  } else if (find.result.kind === "matches") {
    find.result.matches.forEach((m, k) => {
      const f = get(m.line);
      if (k === find.at) f.current = f.matches.length;
      (f.matches as [number, number][]).push([m.start, m.end]);
    });
  }
  return out;
}

/** A line's menu, mounted only once its button is first pressed (T12.90a): a transcript of
 * thousands of lines mounts no Fluent menu (each with its positioning and focus hooks) until one
 * is wanted. Until then the button stands alone; pressing it, or the down arrow on it, mounts the
 * menu already open on it, and it stays mounted for that line. `items` runs only while mounted. */
function LineMenu({ button, items }: { button: ButtonProps; items: () => ReactNode }) {
  const [mounted, setMounted] = useState(false);
  if (!mounted) {
    return (
      <Button
        {...button}
        // As Fluent's trigger marks a closed menu's button.
        aria-haspopup="menu"
        onClick={() => setMounted(true)}
        onKeyDown={(e: { key: string; preventDefault(): void }) => {
          if (e.key !== "ArrowDown") return;
          e.preventDefault();
          setMounted(true);
        }}
      />
    );
  }
  return (
    <Menu defaultOpen>
      <MenuTrigger disableButtonEnhancement>
        <Button {...button} />
      </MenuTrigger>
      <MenuPopover>
        <MenuList>{items()}</MenuList>
      </MenuPopover>
    </Menu>
  );
}

/** A line's words as split points (T12.90): from the second word on, each word is a button
 * that splits the line before it. Escape or Cancel leaves without splitting. */
function SplitPoints({
  index,
  text,
  locked,
  onSplit,
  onCancel,
}: {
  index: number;
  text: string;
  locked: boolean;
  onSplit(token: number): void;
  onCancel(): void;
}) {
  const styles = useStyles();
  const words = tokensOf(text);
  const captionId = useId();
  const first = useRef<HTMLButtonElement | null>(null);
  useEffect(() => first.current?.focus(), []);
  return (
    <div
      className={styles.splitPoints}
      onKeyDown={(e) => {
        if (e.key === "Escape" && e.currentTarget.contains(e.target as Node)) {
          e.preventDefault();
          onCancel();
        }
      }}
    >
      <Caption1 id={captionId} className={styles.muted}>
        Choose the word the new line starts with.
      </Caption1>
      <span
        role="group"
        aria-label={`Split line ${index + 1}`}
        aria-describedby={captionId}
        className={styles.splitWords}
      >
        {words.map((w, t) =>
          t === 0 ? (
            <span key={t}>{w}</span>
          ) : (
            <Button
              key={t}
              ref={t === 1 ? first : undefined}
              size="small"
              appearance="outline"
              aria-label={`Split before word ${t + 1}: ${w}`}
              disabledFocusable={locked}
              onClick={() => onSplit(t)}
            >
              {w}
            </Button>
          ),
        )}
      </span>
      <div className={styles.splitFooter}>
        <Button size="small" appearance="subtle" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </div>
  );
}


/** Line `segment` inside `root`: the transcript's tab stop, where focus goes when its editor
 * closes (T14.19). */
function lineRow(root: Element | null, segment: string): HTMLElement | null {
  return root?.querySelector<HTMLElement>(`[data-line="${CSS.escape(segment)}"]`) ?? null;
}

/** The Edit button of line `segment` inside `root`: where focus goes when the control that had
 * it is gone or took none. */
function lineEditButton(root: Element | null, segment: string): HTMLElement | null {
  return root?.querySelector<HTMLElement>(`[data-line-edit="${CSS.escape(segment)}"]`) ?? null;
}

/** A line's own buttons, reached from the line with the arrow keys and never by Tab (T14.19):
 * its Edit, More and Choose buttons and its word marks (`WordMarks.tsx`). */
const LINE_CONTROL = "[data-line-control]";
/** How many lines Page Up and Page Down move when the page has no layout to measure (jsdom). */
export const PAGE_LINES = 10;

/** About a screenful of lines from `row`: the window's height over the line's, less one so a
 * line stays in view, at least one; `PAGE_LINES` when nothing is laid out. */
function linesPerPage(row: HTMLElement): number {
  const line = row.getBoundingClientRect().height;
  const view = window.innerHeight;
  if (!(line > 0) || !(view > 0)) return PAGE_LINES;
  return Math.max(1, Math.floor(view / line) - 1);
}

/** What the transcript says about its keys to assistive technology (T14.19). */
export const TRANSCRIPT_KEYS_COPY =
  "Use the up and down arrow keys to move between lines. Enter or the right arrow reaches a line's buttons.";

/** What makes a button one of a line's own (`LINE_CONTROL`). */
const LINE_CONTROL_PROPS = { tabIndex: -1, "data-line-control": "" } as const;

/** What a transcript line does; each call reaches `TranscriptTab`'s current state. */
interface LineHandlers {
  contextMenu(r: TranscriptRow, e: MouseEvent<HTMLDivElement>): void;
  keyDown(r: TranscriptRow, e: KeyboardEvent<HTMLDivElement>): void;
  /** Focus went into line `segment`: it becomes the transcript's tab stop (T14.19). */
  focusRow(segment: string): void;
  startEdit(segment: string): void;
  saveEdit(segment: string, text: string): Promise<void>;
  /** Closes line `segment`'s editor; focus goes back to the line. */
  cancelEdit(segment: string): void;
  toggle(segment: string, word: string): void;
  addFromBar(segment: string, word: string): void;
  closeBar(segment: string): void;
  /** "Keep" on a suggestion: the mark leaves the line, so focus goes to its Edit button. */
  keep(segment: string, token: number): void;
  undo(segment: string, word: string): void;
  /** Moves a line to `speaker`, or to a new speaker for `null` (T12.90). */
  move(segment: string, speaker: string | null): void;
  /** Offers the line's words as split points (T12.90). */
  startSplit(segment: string): void;
  /** Splits the line before token `token`. */
  split(segment: string, token: number): void;
  /** Leaves the split points; focus goes back to the line's Edit button. */
  cancelSplit(segment: string): void;
  rowRef(segment: string, el: HTMLDivElement | null): void;
  wordActions(segment: string): WordActions;
}

/** One line of the meeting's transcript. Memoised: typing a search, a citation or an edit on
 * another line leaves it alone, which with hours of transcript was most of each commit (T9.24).
 * Every prop is either the line's own or never changes (`handlers`, `undoRef`). */
const TranscriptLine = memo(function TranscriptLine({
  row: r,
  index: i,
  tabStop,
  movable,
  cited,
  find,
  editing,
  splitting,
  canEdit,
  locked,
  wordList,
  dictionary,
  offered,
  note,
  others,
  undoRef,
  handlers: h,
}: {
  row: TranscriptRow;
  index: number;
  /** The line is the transcript's one tab stop (T14.19). */
  tabStop: boolean;
  /** The speakers lines of this line's track can move to (T12.90, the detail's
   * `movable_by_track`), its own speaker included: the menu leaves it out. */
  movable: MovableSpeaker[];
  cited: boolean;
  /** What find highlights on the line (T12.98); `undefined` for a line it leaves alone. */
  find: LineFind | undefined;
  editing: boolean;
  /** The line's words are offered as split points (T12.90). */
  splitting: boolean;
  canEdit: boolean;
  /** The meeting is busy: Edit, Move, Split and the word choices are off. */
  locked: boolean;
  wordList: string[];
  dictionary: string[];
  /** The words the bar under this line offers, `null` without the bar. */
  offered: string[] | null;
  /** The note under this line after a Dictionary action. */
  note: DictionaryNote | null;
  /** How many other lines suggest the word `note` added. */
  others: number;
  undoRef: RefObject<HTMLButtonElement | null>;
  handlers: LineHandlers;
}) {
  const styles = useStyles();
  const findMarks = useFindMarks();
  const segment = r.segment_id;
  const actions = useMemo<WordActions>(
    () => ({
      onUse: (token, word) => h.wordActions(segment).onUse(token, word),
      onPutBack: (token) => h.wordActions(segment).onPutBack(token),
      onKeep: (token) => h.keep(segment, token),
    }),
    [h, segment],
  );
  const lineDictionary = useMemo(
    () => ({ entries: dictionary, onToggle: (w: string) => h.toggle(segment, w) }),
    [dictionary, h, segment],
  );
  const rowRef = useCallback((el: HTMLDivElement | null) => h.rowRef(segment, el), [h, segment]);
  // T12.90: where the line can move (the runtime's list for its track without its own speaker,
  // names included: a speaker left with no line has no card), then a new speaker. Built only
  // while a menu is open (`LineMenu`).
  const moveItems = () => (
    <>
      {movable
        .filter((s) => s.id !== r.speaker_id)
        .map((s) => (
          <MenuItem key={s.id} onClick={() => h.move(segment, s.id)}>
            {s.name}
          </MenuItem>
        ))}
      <MenuItem onClick={() => h.move(segment, null)}>New speaker</MenuItem>
    </>
  );
  const quiet = !editing && !splitting;
  return (
    <div
      role="row"
      ref={rowRef}
      aria-current={cited ? "true" : undefined}
      data-find-speaker={find?.speaker ? "" : undefined}
      data-find-turn={find?.currentTurn ? "current" : undefined}
      className={mergeClasses(
        styles.row,
        find?.speaker && styles.rowFound,
        cited && styles.rowCited,
        find?.currentTurn && styles.rowCurrentTurn,
      )}
      // One tab stop for the whole transcript (T14.19); focus anywhere in the line moves it here.
      tabIndex={tabStop ? 0 : -1}
      data-line={segment}
      onFocus={() => h.focusRow(segment)}
      onContextMenu={(e) => h.contextMenu(r, e)}
      onKeyDown={(e) => h.keyDown(r, e)}
    >
      <Caption1 role="cell" className={styles.time}>
        {rowTime(r.start_ms)}
      </Caption1>
      <Body1Strong role="cell" className={styles.speaker}>
        {r.speaker}
        {r.unknown_speaker && canEdit && quiet && (
          <LineMenu
            button={{
              size: "small",
              appearance: "subtle",
              "aria-label": `Choose who said line ${i + 1}`,
              disabledFocusable: locked,
              ...LINE_CONTROL_PROPS,
              children: "Choose",
            }}
            items={moveItems}
          />
        )}
      </Body1Strong>
      {editing ? (
        <div role="cell" className={styles.text}>
          <TextEditor
            label={`Line ${i + 1}`}
            initial={r.text}
            multiline
            onSave={(text) => h.saveEdit(segment, text)}
            onCancel={() => h.cancelEdit(segment)}
          />
        </div>
      ) : splitting ? (
        <div role="cell" className={styles.text}>
          <SplitPoints
            index={i}
            text={r.text}
            locked={locked}
            onSplit={(token) => h.split(segment, token)}
            onCancel={() => h.cancelSplit(segment)}
          />
        </div>
      ) : (
        <Body1 role="cell" className={styles.text}>
          {canEdit ? (
            <LineText
              row={r}
              wordList={wordList}
              actions={actions}
              locked={locked}
              dictionary={lineDictionary}
              tinted={offered?.filter((w) => !inDictionary(dictionary, w))}
              find={find}
            />
          ) : (
            findMarks(r.text, 0, find)
          )}
          {r.edited && <Caption1 className={styles.editedMark}>(edited)</Caption1>}
        </Body1>
      )}
      {/* Not a cell: a row's cells are its time, speaker and words. */}
      <span className={styles.lineActions}>
        {canEdit && quiet && (
          <Button
            size="small"
            appearance="subtle"
            className={mergeClasses(styles.lineEdit, "earshot-line-edit")}
            aria-label={`Edit line ${i + 1}`}
            data-line-edit={segment}
            {...LINE_CONTROL_PROPS}
            // Focusable, so focus that comes back to it after a word's choice stays in the line.
            disabledFocusable={locked}
            onClick={() => h.startEdit(segment)}
          >
            Edit
          </Button>
        )}
        {canEdit && quiet && (
          <LineMenu
            button={{
              size: "small",
              appearance: "subtle",
              className: mergeClasses(styles.lineEdit, styles.lineMore, "earshot-line-edit"),
              "aria-label": `More actions for line ${i + 1}`,
              disabledFocusable: locked,
              ...LINE_CONTROL_PROPS,
              children: "⋯",
            }}
            items={() => (
              <>
                <Menu>
                  <MenuTrigger disableButtonEnhancement>
                    <MenuItem>Move to speaker</MenuItem>
                  </MenuTrigger>
                  <MenuPopover>
                    <MenuList>{moveItems()}</MenuList>
                  </MenuPopover>
                </Menu>
                {r.splittable ? (
                  <MenuItem onClick={() => h.startSplit(segment)}>Split line</MenuItem>
                ) : (
                  // A line of two words or more whose words were edited: why it cannot be split
                  // is the item's own text, so it reads out with it. One word has nothing to split.
                  tokensOf(r.text).length >= 2 && (
                    <MenuItem disabled subText={SPLIT_NEEDS_WORD_TIMES_COPY}>
                      Split line
                    </MenuItem>
                  )
                )}
              </>
            )}
          />
        )}
      </span>
      {canEdit && !editing && (offered !== null || note !== null) && (
        <div className={styles.lineDictionary}>
          {offered !== null && (
            <DictionaryBar
              words={offered}
              dictionary={dictionary}
              onAdd={(w) => h.addFromBar(segment, w)}
              onClose={() => h.closeBar(segment)}
            />
          )}
          {note !== null && (
            <DictionaryNoteLine
              note={note}
              others={others}
              undoRef={undoRef}
              onUndo={() => note.kind === "added" && h.undo(segment, note.word)}
            />
          )}
        </div>
      )}
    </div>
  );
});
