import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import {
  type Ipc,
  IpcFailure,
  IpcProvider,
  type MockIpcData,
  SUMMARY_TEMPLATES,
  UNKNOWN_TEMPLATE,
  createMockIpc,
} from "../ipc";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { STYLE_HINT, STYLE_PLACEHOLDER, TEMPLATE_HINT } from "./SummaryStyleSection";

interface Setup {
  ipc: Ipc;
  user: ReturnType<typeof userEvent.setup>;
}

async function open(data: MockIpcData = {}, ipcOverride?: (ipc: Ipc) => Ipc): Promise<Setup> {
  const timer = createManualTimer(1_000_000);
  const base = createMockIpc({ meetings: [], timer, ...data });
  const ipc = ipcOverride ? ipcOverride(base) : base;
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
  await user.click(within(nav).getByRole("button", { name: "Summaries" }));
  await screen.findByRole("heading", { level: 1, name: "Summaries" });
  return { ipc, user };
}

const field = () => screen.findByRole("textbox", { name: "How the summary should read" });
const save = () => screen.getByRole("button", { name: "Save" });
const reset = () => screen.getByRole("button", { name: "Reset to default" });

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, Summaries, Summary style", () => {
  it("starts empty with an example, the hint and nothing to save", async () => {
    await open();
    expect(screen.getByRole("heading", { name: "Summary style" })).toBeInTheDocument();
    const box = await field();
    expect(box).toHaveValue("");
    expect(box).toHaveAttribute("placeholder", STYLE_PLACEHOLDER);
    expect(screen.getByText(`${STYLE_HINT} 0 of 1000 characters.`)).toBeInTheDocument();
    expect(save()).toBeDisabled();
    expect(reset()).toBeDisabled();
    expect(screen.queryByText("Coming later")).toBeNull();
    expect(screen.queryByText("Saved.")).toBeNull();
  });

  it("saves the style trimmed, says so, and enables Save only after a change", async () => {
    const s = await open();
    const box = await field();
    await s.user.type(box, "  Short and plain. ");
    expect(screen.getByText(`${STYLE_HINT} 19 of 1000 characters.`)).toBeInTheDocument();
    expect(save()).toBeEnabled();
    await s.user.click(save());
    expect(await screen.findByText("Saved.")).toBeInTheDocument();
    expect(box).toHaveValue("Short and plain.");
    expect((await s.ipc.summaryStyle()).style).toBe("Short and plain.");
    expect(save()).toBeDisabled();
    expect(reset()).toBeEnabled();
    // Spaces after the saved text are not a change (the app would store the same text).
    await s.user.type(box, "  ");
    expect(save()).toBeDisabled();
    // Editing again hides the confirmation until the next save.
    await s.user.type(box, " More.");
    expect(screen.queryByText("Saved.")).toBeNull();
    expect(save()).toBeEnabled();
  });

  it("shows a stored style and resets it to the default", async () => {
    const s = await open({ summaries: { style: "Lead with risks." } });
    const box = await field();
    expect(box).toHaveValue("Lead with risks.");
    expect(save()).toBeDisabled();
    await s.user.click(reset());
    await waitFor(() => expect(box).toHaveValue(""));
    expect((await s.ipc.summaryStyle()).style).toBe("");
    expect(await screen.findByText("Saved.")).toBeInTheDocument();
    expect(reset()).toBeDisabled();
  });

  it("shows a refusal under the field and stores nothing", async () => {
    const s = await open({ summaries: { style: "Keep it short." } });
    const box = await field();
    await s.user.clear(box);
    // Pasting is faster than typing 1001 characters.
    box.focus();
    await s.user.paste("a".repeat(1001));
    await s.user.click(save());
    expect(
      await screen.findByText("The summary style can be at most 1000 characters; this one has 1001."),
    ).toBeInTheDocument();
    expect(screen.queryByText("Saved.")).toBeNull();
    expect((await s.ipc.summaryStyle()).style).toBe("Keep it short.");
    expect(box).toHaveValue("a".repeat(1001));
  });

  it("reports a stored style this version does not use", async () => {
    await open({}, (ipc) => ({
      ...ipc,
      summaryStyle: async () => ({
        style: "",
        unknown: ["The stored setting settings/summary-style is not known to this version."],
        max_chars: 1000,
        template: "standard",
        templates: SUMMARY_TEMPLATES,
      }),
    }));
    expect(await screen.findByText("The stored summary style is not used")).toBeInTheDocument();
    expect(await field()).toHaveValue("");
  });

  it("says when the style cannot be read", async () => {
    await open({}, (ipc) => ({
      ...ipc,
      summaryStyle: async () => {
        throw new IpcFailure({ kind: "store", message: "The database is locked.", retryable: true });
      },
    }));
    expect(await screen.findByText("The summary style could not be read")).toBeInTheDocument();
    expect(screen.getByText("The database is locked.")).toBeInTheDocument();
  });

  it("starts on the Standard template, with every template the app offers", async () => {
    await open();
    const select = await screen.findByRole("combobox", { name: "Template" });
    expect(select).toHaveValue("standard");
    const options = within(select).getAllByRole("option").map((o) => o.textContent);
    expect(options).toEqual(["Standard", "Brief", "Decisions and actions", "Detailed notes"]);
    expect(
      screen.getByText(`Short notes with every decision and action item. ${TEMPLATE_HINT}`),
    ).toBeInTheDocument();
  });

  it("applies a template at once and keeps the text being edited", async () => {
    const s = await open({ summaries: { style: "Spell out acronyms." } });
    const box = await field();
    await s.user.type(box, " Be plain.");
    const select = screen.getByRole("combobox", { name: "Template" });
    await s.user.selectOptions(select, "brief");
    await waitFor(async () => expect((await s.ipc.summaryStyle()).template).toBe("brief"));
    expect(select).toHaveValue("brief");
    expect(
      screen.getByText(`A one-sentence summary and at most three note bullets. ${TEMPLATE_HINT}`),
    ).toBeInTheDocument();
    // The typed text is not saved by the choice, and still waits for Save.
    expect(box).toHaveValue("Spell out acronyms. Be plain.");
    expect((await s.ipc.summaryStyle()).style).toBe("Spell out acronyms.");
    expect(save()).toBeEnabled();
    // The choice sits outside the part Save stores.
    const words = screen.getByRole("group", { name: "Your own words" });
    expect(within(words).queryByRole("combobox")).toBeNull();
    expect(within(words).getByRole("button", { name: "Save" })).toBe(save());
  });

  it("shows a refused template under the choice", async () => {
    await open({}, (ipc) => ({
      ...ipc,
      setSummaryTemplate: async () => {
        throw new IpcFailure({ kind: "invalid_setting", message: UNKNOWN_TEMPLATE, retryable: false });
      },
    }));
    const user = userEvent.setup();
    await user.selectOptions(await screen.findByRole("combobox", { name: "Template" }), "detailed");
    expect(await screen.findByText(UNKNOWN_TEMPLATE)).toBeInTheDocument();
  });

  it("refuses control characters in the mock as the app does", async () => {
    const s = await open();
    await expect(s.ipc.setSummaryStyle("a\u0007b")).rejects.toThrow(IpcFailure);
    expect((await s.ipc.setSummaryStyle("a\r\nb\tc ")).style).toBe("a\nb\tc");
  });
});
