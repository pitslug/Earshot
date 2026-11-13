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
  /** T9.24's scenarios in one go, from a freshly loaded page (about 80 s, window in view):
   * idle list, open the 3 h meeting, idle on its summary, open and idle on its transcript,
   * type a 14-character search, then a live call just started and with 3 h landed. The
   * result is also kept in `window.__profileResult` while it runs. */
  scenarios: async () => {
    const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
    const out: Record<string, unknown> = {};
    const w = window as unknown as { __profileResult: unknown };
    // Rates are over the scenario's whole window, not between its first and last commit, so a
    // window with no commits reads as 0 and one with two commits is not a burst rate.
    const summary = (windowMs: number) => {
      const s = stats();
      const r = (x: number | null, d = 1) => (x === null ? null : Number(x.toFixed(d)));
      const seconds = windowMs / 1000;
      return {
        commits: s.commits,
        commits_per_s: r(s.commits / seconds, 2),
        median_ms: r(s.median_ms),
        p95_ms: r(s.p95_ms),
        react_ms_per_s: r(s.total_ms / seconds, 0),
        visible: document.visibilityState === "visible",
      };
    };
    const during = async (name: string, ms: number, action?: () => void) => {
      commits.length = 0;
      action?.();
      await sleep(ms);
      out[name] = summary(ms);
      w.__profileResult = out;
    };
    const byText = (selector: string, re: RegExp) =>
      [...document.querySelectorAll<HTMLElement>(selector)].find((e) => re.test(e.textContent ?? ""));
    const click = (selector: string, re: RegExp) => {
      const el = byText(selector, re);
      if (!el) throw new Error(`nothing matches ${re}`);
      el.click();
    };
    await sleep(2_000);
    await during("idle_list", 10_000);
    await during("open_summary", 4_000, () => click("button", /^Weekly sync with/));
    await during("idle_summary", 10_000);
    await during("open_transcript", 5_000, () => click('[role="tab"]', /^Transcript/));
    await during("idle_transcript", 10_000);
    const input = document.querySelector<HTMLInputElement>('input[aria-label="Search meetings"]');
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
    if (!input || !setValue) throw new Error("no search box");
    const type = (text: string) => {
      setValue.call(input, text);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    };
    commits.length = 0;
    input.focus();
    const query = "the budget plan";
    for (let i = 1; i <= 14; i += 1) {
      type(query.slice(0, i));
      await sleep(150);
    }
    await sleep(1_500);
    out.typing = summary(14 * 150 + 1_500);
    type("");
    await sleep(1_000);
    click("button", /^New meeting$/);
    await sleep(800);
    click("button", /^Start$/);
    await sleep(2_500);
    await during("live_start", 8_000);
    (window.__profile as { landLive(h: number): number }).landLive(3);
    await sleep(2_000);
    await during("live_3h", 8_000);
    byText("button", /^Stop meeting$/)?.click();
    out.done = true;
    w.__profileResult = out;
    return out;
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
