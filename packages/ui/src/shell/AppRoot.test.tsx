// The window's tree keeps its order (T14.14): the app's provider and the app window sit under
// the error boundary, and only the theme sits above it.

import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type ReactElement, isValidElement } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { type Ipc, IpcProvider, type MeetingListItem, type MeetingSnapshot, createMockIpc } from "../ipc";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { AppRoot } from "./AppRoot";
import { AppShell } from "./AppShell";
import { CRASH_COPY, RootErrorBoundary } from "./RootErrorBoundary";

const GOOD: MeetingListItem = { id: "m-good", title: "Compost bays", started_at_ms: 2_000, duration_ms: 60_000, state: "Ready", folder_id: null, new_summary: false };
/** A row the app could never send: its missing title makes the meeting list throw while it
 * renders. It is also the meeting that records. */
const BROKEN = { id: "m-broken", title: null, started_at_ms: 3_000, duration_ms: null, state: "Recording", folder_id: null, new_summary: false } as unknown as MeetingListItem;

/** The one child of `element`, which must be an element. */
function onlyChild(element: ReactElement): ReactElement {
  const child = (element.props as { children?: unknown }).children;
  if (!isValidElement(child)) throw new Error("expected exactly one element as the child");
  return child;
}

describe("the window's tree", () => {
  beforeEach(() => {
    // React reports a caught render error on the console; the test throws on purpose.
    vi.spyOn(console, "error").mockImplementation(() => {});
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("app root: the theme, then the error boundary, then the app's provider, then the app window", () => {
    const ipc = createMockIpc({ meetings: [GOOD] });
    const theme = AppRoot({ ipc });
    expect(theme.type).toBe(EarshotThemeProvider);
    const boundary = onlyChild(theme);
    expect(boundary.type).toBe(RootErrorBoundary);
    // The boundary is handed the app itself, the same one the screens get.
    expect((boundary.props as { ipc: Ipc }).ipc).toBe(ipc);
    const provider = onlyChild(boundary);
    expect(provider.type).toBe(IpcProvider);
    expect((provider.props as { ipc: Ipc }).ipc).toBe(ipc);
    expect(onlyChild(provider).type).toBe(AppShell);
  });

  it("app root: shows the app", async () => {
    render(<AppRoot ipc={createMockIpc({ meetings: [GOOD] })} timer={createManualTimer(10_000)} />);
    expect(await screen.findByRole("heading", { level: 1, name: GOOD.title })).toBeInTheDocument();
    expect(screen.queryByText(CRASH_COPY.title)).toBeNull();
  });

  it("app root: an app window that throws while a meeting records leaves the fixed words and a Stop that works", async () => {
    const stopMeeting = vi.fn(async () => {});
    // The app says the broken row's meeting records (T14.51: the boundary asks the snapshot).
    const meetingSnapshot = async (): Promise<MeetingSnapshot> => ({
      starting: false,
      followed: {
        meeting: BROKEN.id,
        mode: "InRoom",
        tracks: ["You"],
        elapsed_ms: 7_000,
        ended: null,
        banners: [],
        backends: [],
        words: 0,
        lag_seconds: null,
        transcript: "Live",
        call_end_offer: false,
        progress: null,
      },
      behind: [],
    });
    const ipc: Ipc = {
      ...createMockIpc({ meetings: [GOOD] }),
      listMeetings: async () => [BROKEN, GOOD],
      meetingSnapshot,
      stopMeeting,
    };
    const onReload = vi.fn();
    render(<AppRoot ipc={ipc} timer={createManualTimer(10_000)} onReload={onReload} />);
    expect(await screen.findByRole("heading", { level: 1, name: CRASH_COPY.title })).toBeInTheDocument();
    // Nothing of the app window is left, and nothing of what broke it is shown.
    expect(screen.queryByRole("button", { name: "New meeting" })).toBeNull();
    expect(document.body.textContent).not.toMatch(/TypeError|null|trim/);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: CRASH_COPY.stop }));
    await waitFor(() => expect(stopMeeting).toHaveBeenCalledTimes(1));
    await user.click(screen.getByRole("button", { name: CRASH_COPY.reload }));
    expect(onReload).toHaveBeenCalledTimes(1);
  });

  it("app root: what is given as its child takes the app's place under the boundary", async () => {
    function Throws(): never {
      throw new Error("invented preview failure");
    }
    render(
      <AppRoot ipc={createMockIpc({ meetings: [GOOD] })}>
        <Throws />
      </AppRoot>,
    );
    expect(await screen.findByRole("heading", { level: 1, name: CRASH_COPY.title })).toBeInTheDocument();
    expect(document.body.textContent).not.toContain("invented preview failure");
  });
});
