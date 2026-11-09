import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, IpcProvider, type MockIpcData, type PreviewsChoice, createMockIpc } from "../ipc";
import { AFTER_CALL_WARNING } from "../ipc/mockSettings";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { NEXT_MEETING } from "./PeoplePage";
import {
  PREVIEWS_EVERYONE_EXPLAINED,
  PREVIEWS_EXPLAINED,
  PREVIEWS_HEADING,
  PREVIEWS_OTHER_PEOPLE_EXPLAINED,
} from "./TranscriptionPage";

async function open(data: MockIpcData = {}): Promise<{ ipc: Ipc; user: ReturnType<typeof userEvent.setup> }> {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "Settings" }));
  const nav = await screen.findByRole("navigation", { name: "Settings sections" });
  await user.click(within(nav).getByRole("button", { name: "Transcription" }));
  await screen.findByRole("heading", { level: 1, name: "Transcription" });
  await screen.findByRole("radiogroup", { name: PREVIEWS_HEADING });
  await screen.findByRole("radiogroup", { name: "When speakers are found" });
  return { ipc, user };
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, Transcription, provisional words", () => {
  const group = async () => screen.findByRole("radiogroup", { name: PREVIEWS_HEADING });
  const radio = async (name: string) => within(await group()).getByRole("radio", { name }) as HTMLInputElement;
  const view = (choice: PreviewsChoice, can_change = true, note: string | null = null) => ({
    previews: { choice, can_change, note, unknown: null },
  });

  it("offers Off, Other people only and Everyone, says what each shows, and stores a change", async () => {
    const s = await open({ settings: view("other_people") });
    const labels = within(await group())
      .getAllByRole("radio")
      .map((r) => (r as HTMLInputElement).labels?.[0].textContent);
    expect(labels).toEqual(["Off", "Other people only", "Everyone"]);
    expect((await radio("Other people only")).checked).toBe(true);
    expect(screen.getByText(PREVIEWS_EXPLAINED)).toBeTruthy();
    expect(screen.getByText(PREVIEWS_OTHER_PEOPLE_EXPLAINED)).toBeTruthy();
    expect(screen.getByText(PREVIEWS_EVERYONE_EXPLAINED)).toBeTruthy();
    await s.user.click(await radio("Everyone"));
    await waitFor(async () => expect((await s.ipc.previewsSettings()).choice).toBe("everyone"));
    expect((await radio("Everyone")).checked).toBe(true);
    await s.user.click(await radio("Off"));
    await waitFor(async () => expect((await s.ipc.previewsSettings()).choice).toBe("off"));
    expect((await radio("Off")).checked).toBe(true);
    await s.user.click(await radio("Other people only"));
    await waitFor(async () => expect((await s.ipc.previewsSettings()).choice).toBe("other_people"));
  });

  it("shows the note for a small machine, and lets them be turned on", async () => {
    const note = "A note about this computer.";
    const s = await open({ settings: view("off", true, note) });
    expect((await radio("Off")).checked).toBe(true);
    expect(screen.getByText(note)).toBeTruthy();
    expect((await radio("Other people only")).disabled).toBe(false);
    await s.user.click(await radio("Other people only"));
    await waitFor(async () => expect((await s.ipc.previewsSettings()).choice).toBe("other_people"));
  });

  it("shows the note when Everyone is chosen but this computer runs them for the other people only", async () => {
    const note = "This computer shows them for the other people only.";
    await open({ settings: view("everyone", true, note) });
    expect((await radio("Everyone")).checked).toBe(true);
    expect(screen.getByText(note)).toBeTruthy();
  });

  it("cannot be changed where nothing can run them", async () => {
    await open({ settings: view("off", false, "Not here.") });
    for (const name of ["Off", "Other people only", "Everyone"]) {
      expect((await radio(name)).disabled).toBe(true);
    }
    expect(screen.getByText("Not here.")).toBeTruthy();
  });
});

describe("Settings, Transcription, when speakers are found", () => {
  const group = () => screen.getByRole("radiogroup", { name: "When speakers are found" });
  const radio = (name: RegExp) => within(group()).getByRole("radio", { name });

  it("offers the battery choice on a laptop only, with the warning text", async () => {
    await open({ machine: "surface" });
    expect(radio(/^During the call/)).toBeChecked();
    expect(radio(/^After the call$/)).not.toBeChecked();
    expect(radio(/^After the call on battery/)).toBeInTheDocument();
    expect(screen.getByText(AFTER_CALL_WARNING)).toBeInTheDocument();
    expect(screen.getByText(NEXT_MEETING)).toBeInTheDocument();
    cleanup();
    await open({ machine: "desktop" });
    expect(within(group()).queryByRole("radio", { name: /^After the call on battery/ })).toBeNull();
    expect(within(group()).getAllByRole("radio")).toHaveLength(2);
  });

  it("saves a new timing at once", async () => {
    const s = await open();
    await s.user.click(radio(/^After the call on battery/));
    await waitFor(async () => expect((await s.ipc.speakers()).timing).toBe("AfterCallOnBattery"));
    expect(radio(/^After the call on battery/)).toBeChecked();
    await s.user.click(radio(/^After the call$/));
    await waitFor(async () => expect((await s.ipc.speakers()).timing).toBe("AfterCall"));
  });
});
