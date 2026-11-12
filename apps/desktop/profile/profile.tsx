// T9.24's before-profile (evaluation only, never built: Vite's build reads index.html alone and
// tsconfig does not include this folder). Served by the Vite preview at /profile/index.html:
// the app on mock data with a 300-meeting store, one finished 3-hour meeting, and hooks to make
// a live meeting 3 hours long. Every commit of the app is timed by React's Profiler; the numbers
// are read from `window.__profile` in the browser. Development build of React (the Profiler
// does not run in a production build), so absolute times are larger than the app's: compare
// scenarios with each other.
import {
  AppShell,
  EarshotThemeProvider,
  IpcProvider,
  createMockIpc,
  sampleDetail,
  sampleMeetings,
  type Ipc,
  type MeetingDetail,
  type MeetingEvent,
  type MeetingListItem,
  type TranscriptRow,
} from "@earshot/ui";
import { Profiler, StrictMode, type ProfilerOnRenderCallback } from "react";
import { createRoot } from "react-dom/client";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const NOW = Date.now();
const MEETINGS = 300;
const LONG_MS = 3 * HOUR;
/** One transcript line every this many ms in the 3-hour meetings. */
const LINE_EVERY_MS = 6_000;

const id = (n: number) => `0190f1a2-0000-7000-8000-${n.toString(16).padStart(12, "0")}`;
const segId = (n: number) => `0190f1a2-0000-7000-9000-${n.toString(16).padStart(12, "0")}`;

// The four sample meetings first (the long one is "Weekly sync"), then generated finished ones.
const samples = sampleMeetings(NOW);
const meetings: MeetingListItem[] = [...samples];
for (let i = meetings.length; i < MEETINGS; i += 1) {
  meetings.push({
    id: id(1_000 + i),
    title: `Generated meeting ${i} about the release plan`,
    started_at_ms: NOW - i * 7 * HOUR,
    duration_ms: 45 * MINUTE,
    state: "Ready",
    folder_id: null,
  });
}
const longRow = meetings.find((m) => m.id === "0190f1a2-0000-7000-8000-000000000003") as MeetingListItem;
longRow.duration_ms = LONG_MS;

/** `detail` with its transcript repeated, shifted, until it covers `length_ms`. */
function stretched(detail: MeetingDetail, length_ms: number): MeetingDetail {
  const base = detail.transcript;
  const rows: TranscriptRow[] = [...base];
  let n = 0;
  for (let t = (base.at(-1)?.start_ms ?? 0) + LINE_EVERY_MS; t < length_ms; t += LINE_EVERY_MS) {
    const from = base[n % base.length];
    rows.push({ ...from, segment_id: segId(n), start_ms: t, suggestions: [], changed_words: [] });
    n += 1;
  }
  return { ...detail, transcript: rows, header: { ...detail.header, duration_ms: length_ms } };
}

const mock = createMockIpc({
  now: NOW,
  meetings,
  details: { [longRow.id]: stretched(sampleDetail(longRow), LONG_MS) },
});

// The meeting screen's listeners, so a live meeting can be made 3 hours long at once.
const meetingListeners = new Set<(e: MeetingEvent) => void>();
const ipc: Ipc = {
  ...mock,
  onMeetingEvent: async (listener) => {
    meetingListeners.add(listener);
    const off = await mock.onMeetingEvent(listener);
    return () => {
      meetingListeners.delete(listener);
      off();
    };
  },
} as Ipc;

interface Commit {
  phase: string;
  actual: number;
  base: number;
  at: number;
}
const commits: Commit[] = [];
const onRender: ProfilerOnRenderCallback = (_id, phase, actual, base, _start, commitTime) => {
  commits.push({ phase, actual, base, at: commitTime });
};
const stats = (from = 0) => {
  const c = commits.filter((x) => x.at >= from);
  const a = c.map((x) => x.actual).sort((x, y) => x - y);
  const sum = a.reduce((s, x) => s + x, 0);
  const pick = (q: number) => (a.length ? a[Math.min(a.length - 1, Math.floor(q * a.length))] : null);
  const span = c.length > 1 ? (c[c.length - 1].at - c[0].at) / 1000 : null;
  return {
    commits: c.length,
    total_ms: Math.round(sum * 10) / 10,
    median_ms: pick(0.5),
    p95_ms: pick(0.95),
    max_ms: a.at(-1) ?? null,
    span_s: span,
    commits_per_s: span ? c.length / span : null,
    ms_per_s: span ? sum / span : null,
  };
};

declare global {
  interface Window {
    __profile: unknown;
  }
}
window.__profile = {
  commits,
  /** Forget what was recorded. */
  reset: () => {
    commits.length = 0;
  },
  /** Summary of the commits since `from` (performance.now() ms; 0: all). */
  stats,
  /** Land `hours` of transcript lines on the live meeting at once (both tracks in turn). */
  landLive: (hours = 3) => {
    const n = Math.floor((hours * HOUR) / LINE_EVERY_MS);
    for (let i = 0; i < n; i += 1) {
      const start_ms = i * LINE_EVERY_MS;
      const event: MeetingEvent = {
        type: "Landed",
        track: i % 2 === 0 ? "Them" : "You",
        start_ms,
        end_ms: start_ms + 4_000,
        text: `Line ${i} of the long meeting, about the schedule, the budget and who does what next.`,
      };
      for (const l of [...meetingListeners]) l(event);
    }
    return n;
  },
  meetings: MEETINGS,
  long_meeting: longRow.id,
};

const root = document.getElementById("root");
if (!root) throw new Error("no #root");
createRoot(root).render(
  <StrictMode>
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <Profiler id="app" onRender={onRender}>
          <AppShell notice="Profile: generated data" />
        </Profiler>
      </IpcProvider>
    </EarshotThemeProvider>
  </StrictMode>,
);
