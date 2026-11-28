import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type Ipc, type MockIpcData, ORDER_COPY, type ServiceChoice, createMockIpc, fillCopy } from "../ipc";
import {
  CLAUDE_CODE_EFFORT_NEEDS_NEWER,
  CLAUDE_CODE_NOT_INSTALLED,
  CLAUDE_CODE_STATUS,
  CODEX_NOT_INSTALLED,
  CODEX_STATUS,
  MOCK_ANTHROPIC_URL,
} from "../ipc/mockSummaries";
import { openSettings, openSetup, renderApp, setupToggle } from "../test/app";
import { createManualTimer } from "../time";
import {
  CLAUDE_CODE_PRIVACY,
  CLAUDE_MODEL_PLACEHOLDER,
  CODEX_MODEL_PLACEHOLDER,
  CODEX_PRIVACY,
  LIVE_DESCRIPTION,
  MODEL_PLACEHOLDER,
  ONLINE_ROW_KEY,
  SETUP_COPY,
  SET_UP,
  SetupSection,
  effortStateText,
  otherKeepsText,
  remoteWarning,
  setupHeaderName,
  setupState,
} from "./SummariesPage";
import { partName, titleOf } from "./copy";

const SECRET = "sk-ant-api03-ui-test-secret-0042";

/** The endpoint's name in its setup section header (T12.96), as the picker names the service
 * (T14.72). */
const SERVER_NAME = ORDER_COPY.steps.endpoint_unset;
const ANTHROPIC_NAME = ORDER_COPY.steps.endpoint_anthropic;
/** A setup section header's spoken name: the writer, then its state. */
const header = (name: string, parts: string[]) => setupHeaderName(name, setupState(parts));

interface Setup {
  ipc: Ipc;
  user: ReturnType<typeof userEvent.setup>;
}

async function open(data: MockIpcData = {}): Promise<Setup> {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  renderApp(ipc, { timer });
  const user = userEvent.setup();
  await openSettings(user, "Summaries");
  await screen.findByRole("heading", { level: 1, name: "Summaries" });
  await screen.findByRole("radiogroup", { name: "Who writes the summary" });
  return { ipc, user };
}

const radio = (name: RegExp) => screen.getByRole("radio", { name });
/** A writer's tick box in the list (T12.68). */
const tick = (writer: string) => screen.getByRole("checkbox", { name: fillCopy(ORDER_COPY.page.use, { writer }) });
const moveUp = (writer: string) =>
  screen.getByRole("button", { name: fillCopy(ORDER_COPY.page.move_up, { writer }) });
const LOCAL = ORDER_COPY.steps.local;
const CLAUDE = ORDER_COPY.steps.claude_code;
const CODEX = ORDER_COPY.steps.codex;
/** Ticks `writer` and moves it to the top of the order. */
async function makeFirst(s: Setup, writer: string): Promise<void> {
  await waitFor(() => expect(tick(writer)).toBeEnabled());
  await s.user.click(tick(writer));
  await waitFor(() => expect(moveUp(writer)).toBeEnabled());
  await s.user.click(moveUp(writer));
}
/** Picks `service` with the Online service row's picker (T14.72) and waits for it to be stored. */
async function pickService(s: Setup, service: ServiceChoice): Promise<void> {
  const picker = screen.getByRole("combobox", { name: ORDER_COPY.page.service_label });
  await waitFor(() => expect(picker).toBeEnabled());
  await s.user.selectOptions(picker, service);
  await waitFor(async () => expect((await s.ipc.summaryOrder()).online.service).toBe(service));
}
/** The Online service row. */
const onlineRow = () =>
  screen.getByRole("list", { name: ORDER_COPY.page.list }).querySelector<HTMLElement>(`li[data-step="${ONLINE_ROW_KEY}"]`);
const optionTexts = (select: HTMLElement) => [...select.querySelectorAll("option")].map((o) => o.textContent);
const live = () => screen.getByRole("switch", { name: "Summarise during the meeting" });

function pageText(): string {
  return document.body.textContent ?? "";
}

describe("Settings, Summaries", () => {
  it("shows on this computer with the model in use on both sample machines", async () => {
    for (const machine of ["surface", "desktop"] as const) {
      const s = await open({ machine });
      await waitFor(() => expect(tick(LOCAL)).toBeChecked());
      const models = await s.ipc.models();
      const detail = `${titleOf(models, models.in_use.summary)} on the ${partName(models.in_use.summary_runs_on)}`;
      expect(await screen.findByText(detail)).toBeInTheDocument();
      expect(screen.getByText(ORDER_COPY.destinations.local)).toBeInTheDocument();
      // No online service picked: no setup section until one is (T14.72). Picked, the endpoint
      // is not set up: its form shows so it can be, nothing filled in.
      expect(screen.queryByRole("group", { name: SERVER_NAME })).toBeNull();
      await pickService(s, "OwnServer");
      const form = await openSetup(s.user, SERVER_NAME);
      expect(within(form).getByLabelText("Address")).toHaveValue("");
      expect(live()).not.toBeChecked();
      expect(live()).toBeEnabled();
      expect(screen.getByText(LIVE_DESCRIPTION)).toBeInTheDocument();
      expect(screen.getByRole("heading", { name: "Summary style" })).toBeInTheDocument();
      cleanup();
    }
  });

  it("saves no summary at once and then shows the live switch disabled with the reason", async () => {
    const s = await open();
    await s.user.click(radio(/^No summary/));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("NoSummary"));
    expect(radio(/^No summary/)).toBeChecked();
    expect(live()).toBeDisabled();
    expect(
      screen.getByText(
        'Summarising during the meeting needs a local summary model. Choose "On this computer" to use it.',
      ),
    ).toBeInTheDocument();
    await s.user.click(radio(new RegExp(`^${ORDER_COPY.page.write}`)));
    await waitFor(() => expect(live()).toBeEnabled());
    await s.user.click(live());
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.live).toBe(true));
    expect(live()).toBeChecked();
  });

  it("uses a local network endpoint only once saved, without the remote warning", async () => {
    const s = await open();
    await pickService(s, "OwnServer");
    const form = await openSetup(s.user, SERVER_NAME);
    // Not set up: the endpoint cannot be ticked, and nothing is stored until it is saved.
    expect(tick(ORDER_COPY.steps.endpoint_unset)).toBeDisabled();
    expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer");
    await s.user.type(within(form).getByLabelText("Address"), "http://192.168.1.20:8080/v1");
    await s.user.type(within(form).getByLabelText("Model"), "qwen");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    const server = fillCopy(ORDER_COPY.steps.endpoint_server, { host: "192.168.1.20" });
    await waitFor(() => expect(tick(server)).toBeChecked());
    const stored = (await s.ipc.summarySettings()).value;
    expect(stored).toMatchObject({ provider: "OpenAiCompatible", url: "http://192.168.1.20:8080/v1", model: "qwen" });
    expect(await within(form).findByText("Saved.")).toBeInTheDocument();
    expect(screen.queryByText(/^The meeting's transcript, title, speaker names and notes are sent to /)).toBeNull();
    expect(within(form).getByLabelText("Model")).toHaveAttribute("placeholder", MODEL_PLACEHOLDER.OpenAiCompatible);
    expect(within(form).queryByLabelText("API key")).toBeNull();
    // The endpoint alone writes the summary: no summarising during the meeting.
    await s.user.click(tick(LOCAL));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("OwnEndpoint"));
    expect(live()).toBeDisabled();
    // This computer first again: the stored endpoint stays as the next writer.
    await makeFirst(s, LOCAL);
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer"));
    expect(tick(server)).toBeChecked();
  });

  it("shows a refused endpoint as a message and stores nothing", async () => {
    const s = await open();
    await pickService(s, "OwnServer");
    const form = await openSetup(s.user, SERVER_NAME);
    await s.user.type(within(form).getByLabelText("Address"), "http://llm.example.com/v1");
    await s.user.type(within(form).getByLabelText("Model"), "m");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    expect(await screen.findByText("This setting was not saved")).toBeInTheDocument();
    expect(
      screen.getByText(
        "This address cannot be used: plain http is only allowed to this computer or the local network; use https.",
      ),
    ).toBeInTheDocument();
    expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer");
    // Without a model the endpoint is refused too.
    await s.user.clear(within(form).getByLabelText("Address"));
    await s.user.type(within(form).getByLabelText("Address"), "https://llm.example.com/v1");
    await s.user.clear(within(form).getByLabelText("Model"));
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    expect(await screen.findByText("Enter the model name the endpoint expects.")).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("This setting was not saved")).toBeNull();
  });

  it("refuses an address holding the path the client appends", async () => {
    const s = await open();
    await pickService(s, "AnthropicApi");
    const form = await openSetup(s.user, ANTHROPIC_NAME);
    // Anthropic's own address is filled in as a draft; "/v1" after it is a path.
    await s.user.type(within(form).getByLabelText("Address"), "/v1");
    await s.user.type(within(form).getByLabelText("Model"), "claude-opus-5-5");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    expect(
      await screen.findByText(
        "This address cannot be used: use the address without a path, for example https://api.anthropic.com.",
      ),
    ).toBeInTheDocument();
    expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer");
    await pickService(s, "OwnServer");
    await waitFor(() => expect(within(form).getByLabelText("Address")).toHaveValue(""));
    await s.user.type(within(form).getByLabelText("Model"), "m");
    await s.user.type(within(form).getByLabelText("Address"), "http://127.0.0.1:8080/v1/chat/completions");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    expect(
      await screen.findByText(
        "This address cannot be used: use the address without /chat/completions, for example http://127.0.0.1:8080/v1.",
      ),
    ).toBeInTheDocument();
    expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer");
  });

  it("keeps the Anthropic key write-only: save shows Key saved, remove brings the field back", async () => {
    const s = await open();
    await pickService(s, "AnthropicApi");
    const form = await openSetup(s.user, ANTHROPIC_NAME);
    // Filled in as a draft: nothing stored yet.
    expect(within(form).getByLabelText("Address")).toHaveValue(MOCK_ANTHROPIC_URL);
    expect((await s.ipc.summarySettings()).value.url).toBe("");
    // A placeholder only: nothing is filled in or stored for the model.
    expect(within(form).getByLabelText("Model")).toHaveAttribute("placeholder", "claude-opus-5-5");
    expect(within(form).getByLabelText("Model")).toHaveValue("");
    await s.user.type(within(form).getByLabelText("Model"), "claude-sonnet-4-5");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    // The remote warning names the host; without a key the page says summaries cannot run.
    expect(await screen.findByText(remoteWarning("api.anthropic.com"))).toBeInTheDocument();
    expect(
      screen.getByText("No API key is saved, so Anthropic cannot write the summary. Save your key."),
    ).toBeInTheDocument();
    const field = within(form).getByLabelText("API key");
    expect(field).toHaveAttribute("type", "password");
    await s.user.type(field, SECRET);
    await s.user.click(within(form).getByRole("button", { name: "Save key" }));
    expect(await within(form).findByText("Key saved")).toBeInTheDocument();
    expect(await s.ipc.hasApiKey()).toBe(true);
    expect(within(form).queryByLabelText("API key")).toBeNull();
    expect(screen.queryByText(/No API key is saved/)).toBeNull();
    expect(pageText()).not.toContain(SECRET);
    for (const input of document.querySelectorAll("input")) expect(input.value).not.toContain(SECRET);
    await s.user.click(within(form).getByRole("button", { name: "Remove key" }));
    expect(await within(form).findByLabelText("API key")).toHaveValue("");
    expect(await s.ipc.hasApiKey()).toBe(false);
    expect(screen.getByText(/No API key is saved/)).toBeInTheDocument();
  });

  it("refuses a pasted key with spaces and tells a Credential Manager failure", async () => {
    const s = await open({
      summaries: {
        settings: { writer: "OwnEndpoint", provider: "Anthropic", url: MOCK_ANTHROPIC_URL, model: "claude" },
      },
    });
    const form = await openSetup(s.user, ANTHROPIC_NAME);
    await s.user.type(within(form).getByLabelText("API key"), "two words");
    await s.user.click(within(form).getByRole("button", { name: "Save key" }));
    expect(await screen.findByText("The API key was not saved")).toBeInTheDocument();
    expect(
      screen.getByText("This API key cannot be saved: it holds spaces or line breaks; paste only the key."),
    ).toBeInTheDocument();
    cleanup();
    const t = await open({
      summaries: {
        settings: { writer: "OwnEndpoint", provider: "Anthropic", url: MOCK_ANTHROPIC_URL, model: "claude" },
        keyStoreError: "access denied",
      },
    });
    // A set-up endpoint whose key cannot be read: its section is opened to read why.
    await openSetup(t.user, ANTHROPIC_NAME);
    expect(await screen.findByText("Earshot cannot tell whether a key is saved")).toBeInTheDocument();
    expect(screen.getByText("Windows Credential Manager failed: access denied.")).toBeInTheDocument();
  });

  it("starts with a stored key as Key saved and shows the remote warning for the stored endpoint", async () => {
    const s = await open({
      summaries: {
        settings: { writer: "OwnEndpoint", provider: "Anthropic", url: MOCK_ANTHROPIC_URL, model: "claude" },
        apiKeySaved: true,
      },
    });
    await waitFor(() => expect(tick(ORDER_COPY.steps.endpoint_anthropic)).toBeChecked());
    expect(tick(LOCAL)).not.toBeChecked();
    // Set up and able to run: its section starts closed, its header saying what is stored.
    const form = await screen.findByRole("group", { name: ANTHROPIC_NAME });
    expect(setupToggle(form)).toHaveAttribute("aria-expanded", "false");
    expect(setupToggle(form)).toHaveAccessibleName(header(ANTHROPIC_NAME, ["claude"]));
    await openSetup(s.user, ANTHROPIC_NAME);
    expect(await screen.findByText("Key saved")).toBeInTheDocument();
    expect(screen.getByText(remoteWarning("api.anthropic.com"))).toBeInTheDocument();
    expect(live()).toBeDisabled();
  });

  it("has no em dash, please or exclamation mark in any state", async () => {
    const texts: string[] = [];
    const s = await open({ sync: { setUp: true } });
    texts.push(pageText());
    await pickService(s, "AnthropicApi");
    const form = await openSetup(s.user, ANTHROPIC_NAME);
    texts.push(pageText());
    await s.user.type(within(form).getByLabelText("Model"), "claude");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    await screen.findByText(remoteWarning("api.anthropic.com"));
    texts.push(pageText());
    await s.user.click(tick(LOCAL));
    await s.user.click(tick(ORDER_COPY.steps.endpoint_anthropic));
    await waitFor(async () => expect((await s.ipc.summaryOrder()).none).toBe(true));
    await waitFor(() => expect(live()).toBeDisabled());
    texts.push(pageText());
    await s.user.click(radio(new RegExp(`^${ORDER_COPY.page.write}`)));
    // Claude Code picked: its card and the online row's notes.
    await pickService(s, "ClaudeCode");
    await waitFor(() => expect(tick(CLAUDE)).toBeEnabled());
    // A fourth tick's refusal too.
    await s.user.click(tick(CLAUDE));
    const studio = fillCopy(ORDER_COPY.steps.computer, { device: "Studio", writer: "local model" });
    const laptop = fillCopy(ORDER_COPY.steps.computer, { device: "Laptop", writer: "Claude Code" });
    await waitFor(() => expect(tick(studio)).toBeEnabled());
    await s.user.click(tick(studio));
    await waitFor(() => expect(tick(laptop)).toBeEnabled());
    await s.user.click(tick(laptop));
    await screen.findByText(fillCopy(ORDER_COPY.page.too_many, { n: "3" }));
    await screen.findByText(CLAUDE_CODE_STATUS.surface.message);
    texts.push(pageText());
    for (const t of texts) expect(t).not.toMatch(/—|please|!/i);
  });

  it("offers Claude through Claude Code with the privacy line and asks Claude Code only when shown", async () => {
    const s = await open({ machine: "desktop" });
    const asked = { n: 0 };
    const status = s.ipc.claudeCodeStatus.bind(s.ipc);
    s.ipc.claudeCodeStatus = async () => {
      asked.n += 1;
      return status();
    };
    // Before it is picked: no card and no question to Claude Code.
    expect(screen.queryByRole("group", { name: "Claude through Claude Code" })).toBeNull();
    expect(screen.queryByText(CLAUDE_CODE_PRIVACY)).toBeNull();
    expect(asked.n).toBe(0);
    // Picked and not yet ticked (PLAN 3.9: the disclosure comes before the choice): where the
    // transcript goes and the privacy line on its row; its card shows, so Claude Code is asked.
    await pickService(s, "ClaudeCode");
    expect(tick(CLAUDE)).not.toBeChecked();
    expect(within(onlineRow() as HTMLElement).getByText(ORDER_COPY.destinations.claude_code)).toBeInTheDocument();
    expect(within(onlineRow() as HTMLElement).getByText(CLAUDE_CODE_PRIVACY)).toBeInTheDocument();
    expect(tick(CLAUDE)).toHaveAccessibleDescription(
      `${ORDER_COPY.destinations.claude_code} ${CLAUDE_CODE_PRIVACY}`,
    );
    await screen.findByRole("group", { name: "Claude through Claude Code" });
    await waitFor(() => expect(asked.n).toBe(1));
    await makeFirst(s, CLAUDE);
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("ClaudeCode"));
    const card = await openSetup(s.user, "Claude through Claude Code");
    expect(await within(card).findByText("Claude Code 2.1.290 is installed and signed in.")).toBeInTheDocument();
    expect(asked.n).toBe(1);
    expect(live()).toBeDisabled();
    // The model: a list of Claude Code's aliases (they follow the latest model), applied at
    // once with no Save button.
    const model = within(card).getByRole("combobox", { name: "Model" });
    expect(optionTexts(model)).toEqual(["Claude Code's default", "Fable", "Opus", "Sonnet", "Haiku", "Other"]);
    expect(model).toHaveDisplayValue("Claude Code's default");
    expect(within(card).queryByRole("button", { name: "Save model" })).toBeNull();
    expect(within(card).queryByRole("textbox")).toBeNull();
    for (const [label, alias] of [
      ["Fable", "fable"],
      ["Opus", "opus"],
      ["Haiku", "haiku"],
      ["Sonnet", "sonnet"],
    ] as const) {
      await s.user.selectOptions(model, label);
      await waitFor(async () => expect((await s.ipc.summarySettings()).value.claude_model).toBe(alias));
    }
    expect(within(card).queryByRole("button", { name: "Save model" })).toBeNull();
    // Other: a typed name, saved with its button; choosing Other alone stores nothing.
    await s.user.selectOptions(model, "Other");
    const other = within(card).getByRole("group", { name: "Other model" });
    const name = within(other).getByRole("textbox", { name: /^Model name/ });
    expect(name).toHaveAttribute("placeholder", CLAUDE_MODEL_PLACEHOLDER);
    expect(name).toHaveValue("");
    expect((await s.ipc.summarySettings()).value.claude_model).toBe("sonnet");
    await s.user.type(name, "claude-opus-4-1-20250805");
    await s.user.click(within(other).getByRole("button", { name: "Save model" }));
    await waitFor(async () =>
      expect((await s.ipc.summarySettings()).value.claude_model).toBe("claude-opus-4-1-20250805"),
    );
    expect(await within(other).findByText("Saved.")).toBeInTheDocument();
    expect(model).toHaveDisplayValue("Other");
    // A name Claude Code cannot take is refused with the reason; nothing is stored.
    await s.user.clear(name);
    await s.user.type(name, "--tools");
    await s.user.click(within(other).getByRole("button", { name: "Save model" }));
    expect(await screen.findByText("This setting was not saved")).toBeInTheDocument();
    expect(pageText()).toContain("must not start with a hyphen");
    expect((await s.ipc.summarySettings()).value.claude_model).toBe("claude-opus-4-1-20250805");
    // Back to Claude Code's default at once; the typed name goes away.
    await s.user.selectOptions(model, "Claude Code's default");
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.claude_model).toBe(""));
    expect(within(card).queryByRole("group", { name: "Other model" })).toBeNull();
    await s.user.click(within(card).getByRole("button", { name: "Check again" }));
    await waitFor(() => expect(asked.n).toBe(2));
  });

  it("keeps Save model off while the Other name is empty", async () => {
    const s = await open({ summaries: { settings: { writer: "ClaudeCode", claude_model: "sonnet" } } });
    const card = await openSetup(s.user, "Claude through Claude Code");
    await s.user.selectOptions(within(card).getByRole("combobox", { name: "Model" }), "Other");
    const other = within(card).getByRole("group", { name: "Other model" });
    const save = within(other).getByRole("button", { name: "Save model" });
    expect(save).toBeDisabled();
    await s.user.type(within(other).getByRole("textbox", { name: /^Model name/ }), "   ");
    expect(save).toBeDisabled();
    expect((await s.ipc.summarySettings()).value.claude_model).toBe("sonnet");
    // T12.45: while the name is empty the card says which model is still stored, and no
    // "Saved." beside an empty name.
    expect(within(other).getByText(otherKeepsText("Sonnet"))).toBeInTheDocument();
    expect(within(other).queryByText("Saved.")).toBeNull();
    const name = within(other).getByRole("textbox", { name: /^Model name/ });
    await s.user.type(name, "claude-x");
    expect(within(other).queryByText(otherKeepsText("Sonnet"))).toBeNull();
    await s.user.clear(name);
    expect(within(other).getByText(otherKeepsText("Sonnet"))).toBeInTheDocument();
  });

  it("names a stored typed model or the CLI's default under an empty Other name", async () => {
    const s = await open({ summaries: { settings: { writer: "ClaudeCode", claude_model: "claude-x-1" } } });
    const card = await openSetup(s.user, "Claude through Claude Code");
    const other = within(card).getByRole("group", { name: "Other model" });
    expect(within(other).queryByText(otherKeepsText("claude-x-1"))).toBeNull();
    await s.user.clear(within(other).getByRole("textbox", { name: /^Model name/ }));
    expect(within(other).getByText(otherKeepsText("claude-x-1"))).toBeInTheDocument();
    cleanup();
    const t = await open({ summaries: { settings: { writer: "Codex" } } });
    const codex = await openSetup(t.user, "OpenAI through Codex");
    await t.user.selectOptions(within(codex).getByRole("combobox", { name: "Model" }), "Other");
    const codexOther = within(codex).getByRole("group", { name: "Other model" });
    expect(within(codexOther).getByText(otherKeepsText("Codex's default"))).toBeInTheDocument();
    expect(otherKeepsText("Sonnet")).not.toMatch(/—|please|!/i);
  });

  it("chooses Claude Code's effort from a list, applied at once", async () => {
    const s = await open({ machine: "desktop", summaries: { settings: { writer: "ClaudeCode", claude_model: "opus" } } });
    const card = await openSetup(s.user, "Claude through Claude Code");
    await within(card).findByText(CLAUDE_CODE_STATUS.desktop.message);
    const effort = within(card).getByRole("combobox", { name: "Effort" });
    expect(optionTexts(effort)).toEqual(["Claude Code's default", "Low", "Medium", "High", "Extra high", "Max"]);
    expect(effort).toHaveDisplayValue("Claude Code's default");
    for (const [label, choice] of [
      ["Low", "Low"],
      ["Medium", "Medium"],
      ["High", "High"],
      ["Max", "Max"],
      ["Extra high", "ExtraHigh"],
    ] as const) {
      await s.user.selectOptions(effort, label);
      await waitFor(async () => expect((await s.ipc.summarySettings()).value.claude_effort).toBe(choice));
    }
    // The model is kept; no Save button is needed for either list.
    expect((await s.ipc.summarySettings()).value.claude_model).toBe("opus");
    expect(within(card).queryByRole("button", { name: /^Save/ })).toBeNull();
    expect(within(card).queryByText(CLAUDE_CODE_EFFORT_NEEDS_NEWER)).toBeNull();
    await s.user.selectOptions(effort, "Claude Code's default");
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.claude_effort).toBeNull());
  });

  it("says effort needs a newer Claude Code when the installed one has none", async () => {
    const s = await open({
      summaries: {
        settings: { writer: "ClaudeCode", claude_effort: "High" },
        claudeCode: { ...CLAUDE_CODE_STATUS.desktop, effort_note: CLAUDE_CODE_EFFORT_NEEDS_NEWER },
      },
    });
    const card = await openSetup(s.user, "Claude through Claude Code");
    expect(await within(card).findByText(CLAUDE_CODE_EFFORT_NEEDS_NEWER)).toBeInTheDocument();
    // The choice is kept for when Claude Code is updated.
    expect(within(card).getByRole("combobox", { name: "Effort" })).toHaveDisplayValue("High");
  });

  it("shows a stored model that is not in the list under Other", async () => {
    const s = await open({ summaries: { settings: { writer: "ClaudeCode", claude_model: "claude-opus-4-1-20250805" } } });
    const card = await openSetup(s.user, "Claude through Claude Code");
    expect(within(card).getByRole("combobox", { name: "Model" })).toHaveDisplayValue("Other");
    const other = within(card).getByRole("group", { name: "Other model" });
    expect(within(other).getByRole("textbox", { name: /^Model name/ })).toHaveValue("claude-opus-4-1-20250805");
    expect(within(other).getByText("Saved.")).toBeInTheDocument();
    cleanup();
    const t = await open({ summaries: { settings: { writer: "Codex", codex_model: "gpt-5.6-luna" } } });
    const codex = await openSetup(t.user, "OpenAI through Codex");
    expect(within(codex).getByRole("combobox", { name: "Model" })).toHaveDisplayValue("Other");
    expect(
      within(within(codex).getByRole("group", { name: "Other model" })).getByRole("textbox", { name: /^Model name/ }),
    ).toHaveValue("gpt-5.6-luna");
  });

  it("says how to sign in to Claude Code or install it", async () => {
    await open({ machine: "surface", summaries: { settings: { writer: "ClaudeCode" } } });
    await waitFor(() => expect(tick(CLAUDE)).toBeChecked());
    const card = await screen.findByRole("group", { name: "Claude through Claude Code" });
    expect(await within(card).findByText(CLAUDE_CODE_STATUS.surface.message)).toBeInTheDocument();
    expect(within(card).getByText(CLAUDE_CODE_STATUS.surface.fix ?? "")).toBeInTheDocument();
    cleanup();
    await open({ summaries: { settings: { writer: "ClaudeCode" }, claudeCode: CLAUDE_CODE_NOT_INSTALLED } });
    const none = await screen.findByRole("group", { name: "Claude through Claude Code" });
    expect(await within(none).findByText("Claude Code is not installed on this computer.")).toBeInTheDocument();
    expect(within(none).getByText(CLAUDE_CODE_NOT_INSTALLED.fix ?? "")).toBeInTheDocument();
  });

  it("offers OpenAI through Codex with the privacy line and asks Codex only when shown", async () => {
    const s = await open({ machine: "desktop" });
    const asked = { codex: 0, claude: 0 };
    const status = s.ipc.codexStatus.bind(s.ipc);
    s.ipc.codexStatus = async () => {
      asked.codex += 1;
      return status();
    };
    const claude = s.ipc.claudeCodeStatus.bind(s.ipc);
    s.ipc.claudeCodeStatus = async () => {
      asked.claude += 1;
      return claude();
    };
    expect(screen.queryByRole("group", { name: "OpenAI through Codex" })).toBeNull();
    expect(screen.queryByText(CODEX_PRIVACY)).toBeNull();
    expect(asked.codex).toBe(0);
    await pickService(s, "Codex");
    expect(tick(CODEX)).not.toBeChecked();
    expect(within(onlineRow() as HTMLElement).getByText(ORDER_COPY.destinations.codex)).toBeInTheDocument();
    expect(within(onlineRow() as HTMLElement).getByText(CODEX_PRIVACY)).toBeInTheDocument();
    expect(CODEX_PRIVACY).toContain("handed to Codex, which sends them to OpenAI on your ChatGPT sign-in");
    await screen.findByRole("group", { name: "OpenAI through Codex" });
    await waitFor(() => expect(asked.codex).toBe(1));
    await makeFirst(s, CODEX);
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("Codex"));
    const card = await openSetup(s.user, "OpenAI through Codex");
    expect(await within(card).findByText(CODEX_STATUS.desktop.message)).toBeInTheDocument();
    expect(asked.codex).toBe(1);
    // Claude Code is not asked for the Codex choice.
    expect(asked.claude).toBe(0);
    expect(screen.queryByRole("group", { name: "Claude through Claude Code" })).toBeNull();
    expect(live()).toBeDisabled();
    // The model: Codex's default or Other, with no effort list (not confirmed on Codex).
    const model = within(card).getByRole("combobox", { name: "Model" });
    expect(optionTexts(model)).toEqual(["Codex's default", "Other"]);
    expect(model).toHaveDisplayValue("Codex's default");
    expect(within(card).queryByRole("combobox", { name: "Effort" })).toBeNull();
    expect(within(card).queryByRole("button", { name: "Save model" })).toBeNull();
    // Other: a typed name with a placeholder, saved with its button.
    await s.user.selectOptions(model, "Other");
    const other = within(card).getByRole("group", { name: "Other model" });
    const name = within(other).getByRole("textbox", { name: /^Model name/ });
    expect(name).toHaveAttribute("placeholder", CODEX_MODEL_PLACEHOLDER);
    expect(name).toHaveValue("");
    await s.user.type(name, "gpt-5.6-luna");
    await s.user.click(within(other).getByRole("button", { name: "Save model" }));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.codex_model).toBe("gpt-5.6-luna"));
    expect(await within(other).findByText("Saved.")).toBeInTheDocument();
    expect((await s.ipc.summarySettings()).value.claude_model).toBe("");
    // A name Codex cannot take is refused with the reason; nothing is stored.
    await s.user.clear(name);
    await s.user.type(name, "-s");
    await s.user.click(within(other).getByRole("button", { name: "Save model" }));
    expect(await screen.findByText("This setting was not saved")).toBeInTheDocument();
    expect(pageText()).toContain("must start with a letter or digit");
    expect((await s.ipc.summarySettings()).value.codex_model).toBe("gpt-5.6-luna");
    // Codex's default applies at once.
    await s.user.selectOptions(model, "Codex's default");
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.codex_model).toBe(""));
    await s.user.click(within(card).getByRole("button", { name: "Check again" }));
    await waitFor(() => expect(asked.codex).toBe(2));
  });

  it("says how to install Codex or sign in to it", async () => {
    await open({ machine: "surface", summaries: { settings: { writer: "Codex" } } });
    await waitFor(() => expect(tick(CODEX)).toBeChecked());
    const card = await screen.findByRole("group", { name: "OpenAI through Codex" });
    expect(await within(card).findByText(CODEX_NOT_INSTALLED.message)).toBeInTheDocument();
    expect(within(card).getByText(CODEX_NOT_INSTALLED.fix ?? "")).toBeInTheDocument();
    cleanup();
    await open({
      summaries: {
        settings: { writer: "Codex" },
        codex: {
          state: "NotSignedIn",
          version: "codex-cli 0.155.1",
          message: "Codex 0.155.1 is installed but not signed in.",
          fix: "Sign in to Codex (in a terminal, run codex login), then check again.",
        },
      },
    });
    const out = await screen.findByRole("group", { name: "OpenAI through Codex" });
    expect(await within(out).findByText("Codex 0.155.1 is installed but not signed in.")).toBeInTheDocument();
    expect(within(out).getByText(/run codex login/)).toBeInTheDocument();
  });
});

describe("Settings, Summaries, the writers' setup sections (T12.96)", () => {
  const SERVER = SERVER_NAME;
  const CLAUDE_GROUP = "Claude through Claude Code";
  const CODEX_GROUP = "OpenAI through Codex";
  const SET_UP_SERVER = `${SET_UP} your own server`;
  const section = (name: string) => screen.getByRole("group", { name });

  it("start closed, each header naming its writer and what is stored", async () => {
    // One section at a time since T14.72: the service picked's. Each is checked in turn.
    const s = await open({
      machine: "desktop",
      summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }],
      summaries: {
        settings: { url: "http://192.168.1.20:8080/v1", model: "qwen3", claude_model: "opus", claude_effort: "Medium" },
      },
    });
    await screen.findByRole("group", { name: CLAUDE_GROUP });
    const closed = (name: string) => {
      const toggle = setupToggle(section(name));
      expect(toggle).toHaveAttribute("aria-expanded", "false");
      expect(toggle).not.toHaveAttribute("aria-controls");
      expect(within(section(name)).queryByRole("combobox")).toBeNull();
      expect(within(section(name)).queryByRole("textbox")).toBeNull();
    };
    // Signed in and set up: nothing needs the user, so each stays closed.
    await waitFor(() =>
      expect(setupToggle(section(CLAUDE_GROUP))).toHaveAccessibleName(header("Claude Code", [SETUP_COPY.signedIn, "Opus", effortStateText("Medium")])),
    );
    closed(CLAUDE_GROUP);
    // The privacy line stays in the list's row, whatever the section does.
    expect(screen.getByText(CLAUDE_CODE_PRIVACY)).toBeInTheDocument();
    // The caret opens it and closes it again.
    const toggle = setupToggle(section(CLAUDE_GROUP));
    await s.user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    const body = document.getElementById(toggle.getAttribute("aria-controls") ?? "");
    expect(body).not.toBeNull();
    expect(within(body as HTMLElement).getByRole("combobox", { name: "Model" })).toHaveDisplayValue("Opus");
    await s.user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(within(section(CLAUDE_GROUP)).queryByRole("combobox")).toBeNull();
    await pickService(s, "Codex");
    await waitFor(() => expect(setupToggle(section(CODEX_GROUP))).toHaveAccessibleName(header("Codex", [SETUP_COPY.signedIn])));
    closed(CODEX_GROUP);
    expect(screen.getByText(CODEX_PRIVACY)).toBeInTheDocument();
    await pickService(s, "OwnServer");
    await screen.findByRole("group", { name: SERVER });
    expect(setupToggle(section(SERVER))).toHaveAccessibleName(header(SERVER_NAME, ["qwen3"]));
    closed(SERVER);
  });

  it("open and close from the keyboard with Enter and Space on the caret", async () => {
    const s = await open({ onlineService: "endpoint" });
    const toggle = setupToggle(section(SERVER));
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    toggle.focus();
    await s.user.keyboard("{Enter}");
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(section(SERVER)).getByLabelText("Address")).toBeInTheDocument();
    await s.user.keyboard(" ");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(within(section(SERVER)).queryByLabelText("Address")).toBeNull();
    await s.user.keyboard(" ");
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    await s.user.keyboard("{Enter}");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(toggle).toHaveFocus();
  });

  it("keep what was typed across a close", async () => {
    const s = await open({ onlineService: "endpoint" });
    const form = await openSetup(s.user, SERVER);
    await s.user.type(within(form).getByLabelText("Address"), "http://192.168.1.20:8080/v1");
    await s.user.click(setupToggle(form));
    expect(within(form).queryByLabelText("Address")).toBeNull();
    await s.user.click(setupToggle(form));
    expect(within(form).getByLabelText("Address")).toHaveValue("http://192.168.1.20:8080/v1");
  });

  it("open the endpoint's form and focus it from Set up on its row", async () => {
    const s = await open({ onlineService: "endpoint" });
    const form = await screen.findByRole("group", { name: SERVER });
    expect(setupToggle(form)).toHaveAccessibleName(header(SERVER_NAME, [ORDER_COPY.page.not_set_up]));
    expect(setupToggle(form)).toHaveAttribute("aria-expanded", "false");
    const setUp = screen.getByRole("button", { name: SET_UP_SERVER });
    await s.user.click(setUp);
    expect(setupToggle(form)).toHaveAttribute("aria-expanded", "true");
    // The form's first control, the address (the picker is the provider: T14.72).
    await waitFor(() => expect(within(form).getByLabelText("Address")).toHaveFocus());
    // Again while it is open, from the keyboard: the focus goes back to the form.
    setUp.focus();
    await s.user.keyboard("{Enter}");
    await waitFor(() => expect(within(form).getByLabelText("Address")).toHaveFocus());
    // Once set up the row has no Set up button and the header says what is stored.
    await s.user.type(within(form).getByLabelText("Address"), "http://192.168.1.20:8080/v1");
    await s.user.type(within(form).getByLabelText("Model"), "qwen");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: SET_UP_SERVER })).toBeNull());
    expect(setupToggle(form)).toHaveAccessibleName(header(SERVER_NAME, ["qwen"]));
    // Under "No summary" there are no forms, so the button is off.
    cleanup();
    await open({ summaryOrder: [], onlineService: "endpoint" });
    await waitFor(() => expect(radio(/^No summary/)).toBeChecked());
    expect(screen.getByRole("button", { name: SET_UP_SERVER })).toBeDisabled();
  });

  it("open by themselves when their writer is ticked but cannot run", async () => {
    // An endpoint in use with no Anthropic key.
    await open({
      summaries: { settings: { writer: "OwnEndpoint", provider: "Anthropic", url: MOCK_ANTHROPIC_URL, model: "claude" } },
    });
    await waitFor(() => expect(setupToggle(section(ANTHROPIC_NAME))).toHaveAttribute("aria-expanded", "true"));
    expect(setupToggle(section(ANTHROPIC_NAME))).toHaveAccessibleName(header(ANTHROPIC_NAME, ["claude", SETUP_COPY.noApiKey]));
    expect(within(section(ANTHROPIC_NAME)).getByLabelText("API key")).toBeInTheDocument();
    cleanup();
    // Claude Code not installed.
    await open({ summaryOrder: [{ kind: "ClaudeCode" }], summaries: { claudeCode: CLAUDE_CODE_NOT_INSTALLED } });
    await waitFor(() => expect(setupToggle(section(CLAUDE_GROUP))).toHaveAttribute("aria-expanded", "true"));
    expect(setupToggle(section(CLAUDE_GROUP))).toHaveAccessibleName(header("Claude Code", [SETUP_COPY.notInstalled]));
    cleanup();
    // Codex signed out.
    await open({
      summaryOrder: [{ kind: "Codex" }],
      summaries: { codex: { ...CODEX_STATUS.desktop, state: "NotSignedIn", message: "Codex is not signed in." } },
    });
    await waitFor(() => expect(setupToggle(section(CODEX_GROUP))).toHaveAttribute("aria-expanded", "true"));
    expect(setupToggle(section(CODEX_GROUP))).toHaveAccessibleName(header("Codex", [SETUP_COPY.notSignedIn]));
    cleanup();
    // Ticked while signed out: it opens once the check answers; closed by the user, it stays so.
    const s = await open({ machine: "surface", onlineService: "claude-code" });
    await makeFirst(s, CLAUDE);
    await waitFor(() => expect(setupToggle(section(CLAUDE_GROUP))).toHaveAttribute("aria-expanded", "true"));
    await s.user.click(setupToggle(section(CLAUDE_GROUP)));
    expect(setupToggle(section(CLAUDE_GROUP))).toHaveAttribute("aria-expanded", "false");
    cleanup();
    // A check that fails opens it too, to say so.
    const t = await open({ machine: "desktop" });
    t.ipc.codexStatus = async () => {
      throw new Error("the check could not run");
    };
    // Picked after the check is set to fail: the card asks when it shows.
    await pickService(t, "Codex");
    await makeFirst(t, CODEX);
    await waitFor(() => expect(setupToggle(section(CODEX_GROUP))).toHaveAttribute("aria-expanded", "true"));
    expect(setupToggle(section(CODEX_GROUP))).toHaveAccessibleName(header("Codex", [SETUP_COPY.couldNotCheck]));
    expect(within(section(CODEX_GROUP)).getByText("Earshot could not check Codex")).toBeInTheDocument();
  });
});

describe("A writer's setup section (T12.96)", () => {
  it("speaks its name and state apart, joined by the separator", () => {
    expect(setupHeaderName("Codex", SETUP_COPY.signedIn)).toBe("Codex, Signed in");
    expect(setupState(["Anthropic (Claude)", null, "claude"])).toBe("Anthropic (Claude), claude");
    expect(effortStateText("ExtraHigh")).toBe("extra high effort");
    for (const text of Object.values(SETUP_COPY)) expect(text).not.toMatch(/—|please|!/i);
  });

  it("spends a Set up request on the open that shows nothing to focus", async () => {
    const user = userEvent.setup();
    const section = (openRequest: number, withField: boolean) => (
      <SetupSection name="Test writer" state="Not set up" group="Test writer" attention={false} openRequest={openRequest} fixed={false}>
        {withField ? <input aria-label="Field" /> : <span>Nothing to focus yet</span>}
        <span />
      </SetupSection>
    );
    const view = render(
      <>
        {section(0, false)}
        <button type="button">Elsewhere</button>
      </>,
    );
    view.rerender(
      <>
        {section(1, false)}
        <button type="button">Elsewhere</button>
      </>,
    );
    const toggle = screen.getByRole("button", { name: "Test writer, Not set up" });
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    await user.click(screen.getByRole("button", { name: "Elsewhere" }));
    // A field that appears later does not take the focus: the request was spent.
    view.rerender(
      <>
        {section(1, true)}
        <button type="button">Elsewhere</button>
      </>,
    );
    expect(screen.getByRole("textbox", { name: "Field" })).not.toHaveFocus();
    expect(screen.getByRole("button", { name: "Elsewhere" })).toHaveFocus();
    // A new request with a field does focus it (the positive case of the same check).
    view.rerender(
      <>
        {section(2, true)}
        <button type="button">Elsewhere</button>
      </>,
    );
    await waitFor(() => expect(screen.getByRole("textbox", { name: "Field" })).toHaveFocus());
  });
});
