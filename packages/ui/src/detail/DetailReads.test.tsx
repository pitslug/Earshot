// What the meeting detail reads and when (T14.17): an answer older than the newest read is
// dropped, and the other computers are asked only when the Re-summarise menu opens.
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type Ipc, IpcFailure, type MeetingDetail, ORDER_COPY, createMockIpc, sampleDetail } from "../ipc";
import { COMPUTERS_UNREAD, LOOKING } from "./ResummariseControl";
import { IpcProvider } from "../ipc/context";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { renderApp } from "../test/app";
import { ITEM, OTHER } from "../test/meetingDetail";
import { MeetingDetailView } from "./MeetingDetailView";

/** A detail whose header says `minutes` long. */
function lasting(minutes: number): MeetingDetail {
  const d = sampleDetail(ITEM);
  return { ...d, header: { ...d.header, duration_ms: minutes * 60_000 } };
}

/** The detail page alone over `ipc`, read again whenever `refresh` changes. */
function detailView(ipc: Ipc, refresh: number, initialTab: "summary" | "transcript" = "summary") {
  return (
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <MeetingDetailView
          item={ITEM}
          refresh={refresh}
          initialTab={initialTab}
          onChanged={() => {}}
          onJobStarted={() => {}}
          onJobDismissed={() => {}}
          onJobCancel={() => {}}
        />
      </IpcProvider>
    </EarshotThemeProvider>
  );
}

describe("meeting detail reads", () => {
  it("drops an answer older than the newest read", async () => {
    const base = createMockIpc({ timer: createManualTimer(10_000), meetings: [ITEM, OTHER] });
    // Every read after the first waits until the test answers it.
    const pending: ((d: MeetingDetail) => void)[] = [];
    let first = true;
    const ipc: Ipc = {
      ...base,
      getMeeting: (id) => {
        if (first) {
          first = false;
          return Promise.resolve(lasting(5));
        }
        void id;
        return new Promise((resolve) => pending.push(resolve));
      },
    };
    const show = (refresh: number) => (
      <EarshotThemeProvider>
        <IpcProvider ipc={ipc}>
          <MeetingDetailView
            item={ITEM}
            refresh={refresh}
            onChanged={() => {}}
            onJobStarted={() => {}}
            onJobDismissed={() => {}}
            onJobCancel={() => {}}
          />
        </IpcProvider>
      </EarshotThemeProvider>
    );
    const view = render(show(0));
    expect(await screen.findByText(/· 5 min ·/)).toBeInTheDocument();
    // Two reads in flight; the newer answers first, then the older.
    view.rerender(show(1));
    view.rerender(show(2));
    await waitFor(() => expect(pending).toHaveLength(2));
    await act(async () => pending[1](lasting(9)));
    expect(await screen.findByText(/· 9 min ·/)).toBeInTheDocument();
    await act(async () => pending[0](lasting(3)));
    // The older answer changes nothing.
    expect(screen.getByText(/· 9 min ·/)).toBeInTheDocument();
    expect(screen.queryByText(/· 3 min ·/)).toBeNull();
  });

  it("shows an older read that succeeded when a later one fails, with the failure", async () => {
    const base = createMockIpc({ timer: createManualTimer(10_000), meetings: [ITEM, OTHER] });
    // The first read waits; the second fails at once.
    let answerFirst: (d: MeetingDetail) => void = () => {};
    let n = 0;
    const ipc: Ipc = {
      ...base,
      getMeeting: () => {
        n += 1;
        if (n === 1) return new Promise((resolve) => (answerFirst = resolve));
        return Promise.reject(new IpcFailure({ kind: "store", message: "The meeting could not be read.", retryable: true }));
      },
    };
    const view = render(detailView(ipc, 0));
    view.rerender(detailView(ipc, 1));
    expect(await screen.findByText("The meeting could not be read.")).toBeInTheDocument();
    await act(async () => answerFirst(lasting(5)));
    // The first load is shown, so the page is not left with no detail, and the newer read's
    // failure still says so.
    expect(await screen.findByText(/· 5 min ·/)).toBeInTheDocument();
    expect(screen.getByText("The meeting could not be read.")).toBeInTheDocument();
  });

  it("closes a line's editor on its new text when a sync read was asked after its own", async () => {
    const base = createMockIpc({ timer: createManualTimer(10_000), meetings: [ITEM, OTHER] });
    // After the first load every read waits until the test answers it with the mock's data.
    let held = false;
    const pending: (() => void)[] = [];
    const ipc: Ipc = {
      ...base,
      getMeeting: (id) => {
        if (!held) return base.getMeeting(id);
        return new Promise((resolve, reject) => pending.push(() => base.getMeeting(id).then(resolve, reject)));
      },
    };
    const user = userEvent.setup();
    const view = render(detailView(ipc, 0, "transcript"));
    await user.click(await screen.findByRole("button", { name: "Edit line 1" }));
    const field = screen.getByRole("textbox", { name: "Line 1" });
    await user.clear(field);
    await user.type(field, "An invented line about the hedge.");
    held = true;
    await user.click(screen.getByRole("button", { name: "Save" }));
    // The edit's own read is out; sync asks for a newer one before it answers.
    await waitFor(() => expect(pending).toHaveLength(1));
    view.rerender(detailView(ipc, 1, "transcript"));
    await waitFor(() => expect(pending).toHaveLength(2));
    await act(async () => pending[0]());
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "Line 1" })).toBeNull());
    const rows = within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row");
    expect(rows[0].textContent).toContain("An invented line about the hedge.");
    await act(async () => pending[1]());
    expect(rows[0].textContent).toContain("An invented line about the hedge.");
  });

  it("says it is looking while no computer is known, and says so when they cannot be read", async () => {
    const timer = createManualTimer(10_000);
    const base = createMockIpc({ timer, machine: "desktop", meetings: [ITEM, OTHER] });
    let answer: () => void = () => {};
    let fail = false;
    const ipc: Ipc = {
      ...base,
      summaryHelpers: () =>
        fail
          ? Promise.reject(new IpcFailure({ kind: "sync", message: "x", retryable: true }))
          : new Promise((resolve, reject) => (answer = () => base.summaryHelpers().then(resolve, reject))),
    };
    renderApp(ipc, { timer });
    await screen.findByText(/TL;DR/);
    const user = userEvent.setup();
    const menuButton = () => screen.getByRole("button", { name: /: choose who writes it$/ });
    await user.click(menuButton());
    // This computer's writers are listed, and the row stays while the computers are read.
    expect(await screen.findByRole("menuitem", { name: ORDER_COPY.steps.local })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: LOOKING })).toBeInTheDocument();
    await act(async () => answer());
    await waitFor(() => expect(screen.queryByRole("menuitem", { name: LOOKING })).toBeNull());
    await user.keyboard("{Escape}");
    fail = true;
    await user.click(menuButton());
    expect(await screen.findByRole("menuitem", { name: COMPUTERS_UNREAD })).toBeInTheDocument();
  });

  it("asks for the other computers only when the Re-summarise menu opens", async () => {
    // Counted at the page's own command (the mock also reads the computers inside other
    // commands, which Rust does not).
    const timer = createManualTimer(10_000);
    const base = createMockIpc({ timer, machine: "desktop", meetings: [ITEM, OTHER] });
    let asked = 0;
    const ipc: Ipc = {
      ...base,
      summaryHelpers: () => {
        asked += 1;
        return base.summaryHelpers();
      },
    };
    renderApp(ipc, { timer });
    await screen.findByText(/TL;DR/);
    // The page's effects have run (it shows the summary): an ask on mount would be counted now.
    await act(async () => {});
    expect(asked).toBe(0);
    await userEvent.setup().click(screen.getByRole("button", { name: /: choose who writes it$/ }));
    await waitFor(() => expect(asked).toBe(1));
  });
});
