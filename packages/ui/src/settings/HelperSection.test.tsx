// Summary offload in the UI (T11.3db): the Summaries page's "Another computer", the mock against
// the shared copy table, the meeting's waiting line and re-summarise "On <device>". Every name is
// invented.

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import offload from "../../../../fixtures/ui/offload_copy_cases.json";
import { type MockIpcData, createMockIpc, sampleMeetings } from "../ipc";
import { IpcProvider } from "../ipc/context";
import { createSyncMock, mockDeviceId, offloadCopy } from "../ipc/mockSync";
import { MeetingStatus } from "../meeting/MeetingViews";
import { reduce, startedMeeting } from "../meeting/liveMeeting";
import { openSettings, renderApp } from "../test/app";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { HELPER_EXPLAINED, HERE_ONLY, helperState } from "./HelperSection";

async function summaries(data: MockIpcData = {}) {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  renderApp(ipc, { timer });
  const user = userEvent.setup();
  await openSettings(user, "Summaries");
  await screen.findByRole("heading", { level: 1, name: "Summaries" });
  return { user, ipc };
}

describe("summary offload: the mock and the shared table", () => {
  it("fills every reason from the table, with the unnamed words when a name is not known", () => {
    for (const template of [...Object.values(offload.unavailable), ...Object.values(offload.written_here)]) {
      expect(template).toMatch(/so this computer wrote/);
      expect(template).not.toMatch(/ {2}/);
      // A name only ever starts a sentence.
      expect(template.indexOf("{device}")).toBeLessThanOrEqual(0);
    }
    expect(offloadCopy(offload.unavailable.offline, null)).toBe(
      "Your other computer is not online, so this computer wrote the summary.",
    );
    expect(offloadCopy(offload.waiting, null, true)).toBe("Waiting for your other computer to write the summary.");
    expect(offloadCopy(offload.waiting, "Studio", true)).toBe("Waiting for Studio to write the summary.");
  });

  it("asks an available computer and refuses one that would not be asked, with the table's words", async () => {
    const sync = createSyncMock({ setUp: true });
    const helpers = await sync.summaryHelpers();
    expect(helpers.sync_ready).toBe(true);
    const studio = helpers.helpers.find((h) => h.name === "Studio");
    const laptop = helpers.helpers.find((h) => h.name === "Laptop");
    expect(studio?.available).toBe(true);
    expect(studio?.writer).toBe(offload.writers.local);
    expect(laptop?.available).toBe(false);
    expect(await sync.askSummaryHelper("m", mockDeviceId("e4a17d30"))).toBe("Waiting for Studio to write the summary.");
    await expect(sync.askSummaryHelper("m", mockDeviceId("c5e09b72"))).rejects.toMatchObject({
      error: { kind: "offload", message: "Laptop is recording a meeting." },
    });
    // Without sync nothing can be asked.
    const off = createSyncMock({ setUp: false, helper: "e4a17d30" });
    const view = await off.summaryHelpers();
    expect(view.sync_ready).toBe(false);
    expect(view.helpers).toHaveLength(1);
    expect(view.helpers[0].chosen).toBe(true);
  });
});

describe("the Summaries page's other computers", () => {
  it("stays away while sync cannot ask another computer", async () => {
    await summaries();
    await screen.findByRole("radiogroup", { name: "Summary model" });
    expect(screen.queryByRole("radiogroup", { name: "Ask another computer to write the summary" })).toBeNull();
  });

  it("lists each computer that writes summaries as On <device> (<writer>), and the choice applies at once", async () => {
    const { user } = await summaries({ sync: { setUp: true } });
    const group = await screen.findByRole("radiogroup", { name: "Ask another computer to write the summary" });
    expect(within(group).getByRole("radio", { name: HERE_ONLY })).toBeChecked();
    const studio = within(group).getByRole("radio", { name: /^On Studio \(local model\)/ });
    within(group).getByRole("radio", { name: /^On Laptop \(Claude Code\)/ });
    expect(screen.getByText(HELPER_EXPLAINED)).toBeInTheDocument();
    expect(within(group).getByText("Recording a meeting now")).toBeInTheDocument();
    await user.click(studio);
    await waitFor(() => expect(studio).toBeChecked());
    await user.click(within(group).getByRole("radio", { name: HERE_ONLY }));
    await waitFor(() => expect(within(group).getByRole("radio", { name: HERE_ONLY })).toBeChecked());
  });

  it("words each state of a computer", () => {
    const base = {
      id: "x",
      name: "D",
      short_id: "00000000",
      writer: "local model",
      offers: true,
      online: true,
      recording: false,
      available: true,
      chosen: false,
    };
    expect(helperState(base)).toBe("Online");
    expect(helperState({ ...base, online: false })).toBe("Not online");
    expect(helperState({ ...base, recording: true })).toBe("Recording a meeting now");
    expect(helperState({ ...base, offers: false })).toBe("No longer writes summaries");
    expect(helperState({ ...base, writer: null })).toBe("No summary writer set up");
  });
});

describe("a meeting written on another computer", () => {
  it("says it waits for the computer in the meeting's status", () => {
    const meeting = reduce(
      startedMeeting("m", "Call", ["You", "Them"], 0),
      { type: "Summary", meeting: "m", result: { kind: "Offloaded", message: "Waiting for Studio to write the summary." } },
      1,
    );
    if (meeting === null) throw new Error("no meeting");
    render(
      <EarshotThemeProvider>
        <IpcProvider ipc={createMockIpc({ meetings: [] })}>
          <MeetingStatus
            meeting={meeting}
            onDismissBanner={() => {}}
            onCancel={() => {}}
            onRetranscribe={() => {}}
            onResummarise={() => {}}
            onDismissResult={() => {}}
          />
        </IpcProvider>
      </EarshotThemeProvider>,
    );
    const status = screen.getByRole("region", { name: "Meeting status" });
    expect(within(status).getByText("Summary on another computer")).toBeInTheDocument();
    expect(within(status).getByText(/Waiting for Studio to write the summary\./)).toBeInTheDocument();
  });

  it("re-summarises here, or asks another computer and shows that it waits", async () => {
    const [item] = sampleMeetings(Date.UTC(2026, 10, 2, 9, 0));
    const timer = createManualTimer(1_000_000);
    const ipc = createMockIpc({ meetings: [item], timer, sync: { setUp: true } });
    renderApp(ipc, { timer });
    const user = userEvent.setup();
    const list = await screen.findByRole("list", { name: "Meetings" });
    await user.click(within(list).getByText(item.title));
    const split = await screen.findByRole("button", { name: /: choose where$/ });
    await user.click(split);
    expect(await screen.findByRole("menuitem", { name: "Here" })).toBeInTheDocument();
    await user.click(await screen.findByRole("menuitem", { name: "On Studio (local model)" }));
    expect(await screen.findByText("Waiting for Studio to write the summary.")).toBeInTheDocument();
    // A computer that would not be asked is not offered.
    await user.click(screen.getByRole("button", { name: /: choose where$/ }));
    await screen.findByRole("menuitem", { name: "Here" });
    expect(screen.queryByRole("menuitem", { name: /On Laptop/ })).toBeNull();
  });
});
