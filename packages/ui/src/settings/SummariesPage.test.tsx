import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import {
  CLAUDE_CODE_EFFORT_NEEDS_NEWER,
  CLAUDE_CODE_NOT_INSTALLED,
  CLAUDE_CODE_STATUS,
  CODEX_NOT_INSTALLED,
  CODEX_STATUS,
  MOCK_ANTHROPIC_URL,
} from "../ipc/mockSummaries";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import {
  CLAUDE_CODE_PRIVACY,
  CLAUDE_MODEL_PLACEHOLDER,
  CODEX_MODEL_PLACEHOLDER,
  CODEX_PRIVACY,
  LIVE_DESCRIPTION,
  MODEL_PLACEHOLDER,
  remoteWarning,
} from "./SummariesPage";
import { partName, titleOf } from "./copy";

const SECRET = "sk-ant-api03-ui-test-secret-0042";

interface Setup {
  ipc: Ipc;
  user: ReturnType<typeof userEvent.setup>;
}

async function open(data: MockIpcData = {}): Promise<Setup> {
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
  await user.click(within(nav).getByRole("button", { name: "Summaries" }));
  await screen.findByRole("heading", { level: 1, name: "Summaries" });
  await screen.findByRole("radiogroup", { name: "Who writes the summary" });
  return { ipc, user };
}

const radio = (name: RegExp) => screen.getByRole("radio", { name });
const optionTexts = (select: HTMLElement) => [...select.querySelectorAll("option")].map((o) => o.textContent);
const live = () => screen.getByRole("switch", { name: "Summarise during the meeting" });

function pageText(): string {
  return document.body.textContent ?? "";
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, Summaries", () => {
  it("shows on this computer with the model in use on both sample machines", async () => {
    for (const machine of ["surface", "desktop"] as const) {
      const s = await open({ machine });
      expect(radio(/^On this computer/)).toBeChecked();
      const models = await s.ipc.models();
      const detail = `${titleOf(models, models.in_use.summary)} on the ${partName(models.in_use.summary_runs_on)}`;
      expect(await screen.findByText(detail)).toBeInTheDocument();
      expect(screen.getByText("Nothing leaves this computer.")).toBeInTheDocument();
      expect(screen.queryByRole("group", { name: "Your own server" })).toBeNull();
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
    await s.user.click(radio(/^On this computer/));
    await waitFor(() => expect(live()).toBeEnabled());
    await s.user.click(live());
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.live).toBe(true));
    expect(live()).toBeChecked();
  });

  it("uses a local network endpoint only once saved, without the remote warning", async () => {
    const s = await open();
    await s.user.click(radio(/^Your own server/));
    const form = screen.getByRole("group", { name: "Your own server" });
    // Nothing is stored until the endpoint is saved.
    expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer");
    await s.user.type(within(form).getByLabelText("Address"), "http://192.168.1.20:8080/v1");
    await s.user.type(within(form).getByLabelText("Model"), "qwen");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("OwnEndpoint"));
    const stored = (await s.ipc.summarySettings()).value;
    expect(stored).toMatchObject({ provider: "OpenAiCompatible", url: "http://192.168.1.20:8080/v1", model: "qwen" });
    expect(await within(form).findByText("Saved.")).toBeInTheDocument();
    expect(screen.queryByText(/^The meeting's transcript, title, speaker names and notes are sent to /)).toBeNull();
    expect(within(form).getByLabelText("Model")).toHaveAttribute("placeholder", MODEL_PLACEHOLDER.OpenAiCompatible);
    expect(within(form).queryByLabelText("API key")).toBeNull();
    expect(live()).toBeDisabled();
    // Back to this computer and to the endpoint again: the stored one is used at once.
    await s.user.click(radio(/^On this computer/));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer"));
    await s.user.click(radio(/^Your own server/));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("OwnEndpoint"));
  });

  it("shows a refused endpoint as a message and stores nothing", async () => {
    const s = await open();
    await s.user.click(radio(/^Your own server/));
    const form = screen.getByRole("group", { name: "Your own server" });
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
    await s.user.click(radio(/^Your own server/));
    const form = screen.getByRole("group", { name: "Your own server" });
    await s.user.selectOptions(within(form).getByRole("combobox", { name: "Provider" }), "Anthropic");
    await s.user.type(within(form).getByLabelText("Address"), "/v1");
    await s.user.type(within(form).getByLabelText("Model"), "claude-opus-5-5");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    expect(
      await screen.findByText(
        "This address cannot be used: use the address without a path, for example https://api.anthropic.com.",
      ),
    ).toBeInTheDocument();
    expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer");
    await s.user.selectOptions(within(form).getByRole("combobox", { name: "Provider" }), "OpenAiCompatible");
    await s.user.clear(within(form).getByLabelText("Address"));
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
    await s.user.click(radio(/^Your own server/));
    const form = screen.getByRole("group", { name: "Your own server" });
    await s.user.selectOptions(within(form).getByRole("combobox", { name: "Provider" }), "Anthropic");
    expect(within(form).getByLabelText("Address")).toHaveValue(MOCK_ANTHROPIC_URL);
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
    const form = screen.getByRole("group", { name: "Your own server" });
    await s.user.type(within(form).getByLabelText("API key"), "two words");
    await s.user.click(within(form).getByRole("button", { name: "Save key" }));
    expect(await screen.findByText("The API key was not saved")).toBeInTheDocument();
    expect(
      screen.getByText("This API key cannot be saved: it holds spaces or line breaks; paste only the key."),
    ).toBeInTheDocument();
    cleanup();
    await open({
      summaries: {
        settings: { writer: "OwnEndpoint", provider: "Anthropic", url: MOCK_ANTHROPIC_URL, model: "claude" },
        keyStoreError: "access denied",
      },
    });
    expect(await screen.findByText("Earshot cannot tell whether a key is saved")).toBeInTheDocument();
    expect(screen.getByText("Windows Credential Manager failed: access denied.")).toBeInTheDocument();
  });

  it("starts with a stored key as Key saved and shows the remote warning for the stored endpoint", async () => {
    await open({
      summaries: {
        settings: { writer: "OwnEndpoint", provider: "Anthropic", url: MOCK_ANTHROPIC_URL, model: "claude" },
        apiKeySaved: true,
      },
    });
    expect(radio(/^Your own server/)).toBeChecked();
    expect(await screen.findByText("Key saved")).toBeInTheDocument();
    expect(screen.getByText(remoteWarning("api.anthropic.com"))).toBeInTheDocument();
    expect(live()).toBeDisabled();
  });

  it("has no em dash, please or exclamation mark in any state", async () => {
    const texts: string[] = [];
    const s = await open();
    texts.push(pageText());
    await s.user.click(radio(/^Your own server/));
    const form = screen.getByRole("group", { name: "Your own server" });
    texts.push(pageText());
    await s.user.selectOptions(within(form).getByRole("combobox", { name: "Provider" }), "Anthropic");
    await s.user.type(within(form).getByLabelText("Model"), "claude");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    await screen.findByText(remoteWarning("api.anthropic.com"));
    texts.push(pageText());
    await s.user.click(radio(/^No summary/));
    await waitFor(() => expect(live()).toBeDisabled());
    texts.push(pageText());
    await s.user.click(radio(/^Claude \(through Claude Code\)/));
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
    expect(screen.getByText(CLAUDE_CODE_PRIVACY)).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: "Claude through Claude Code" })).toBeNull();
    expect(asked.n).toBe(0);
    await s.user.click(radio(/^Claude \(through Claude Code\)/));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("ClaudeCode"));
    const card = await screen.findByRole("group", { name: "Claude through Claude Code" });
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

  it("chooses Claude Code's effort from a list, applied at once", async () => {
    const s = await open({ machine: "desktop", summaries: { settings: { writer: "ClaudeCode", claude_model: "opus" } } });
    const card = await screen.findByRole("group", { name: "Claude through Claude Code" });
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
    await open({
      summaries: {
        settings: { writer: "ClaudeCode", claude_effort: "High" },
        claudeCode: { ...CLAUDE_CODE_STATUS.desktop, effort_note: CLAUDE_CODE_EFFORT_NEEDS_NEWER },
      },
    });
    const card = await screen.findByRole("group", { name: "Claude through Claude Code" });
    expect(await within(card).findByText(CLAUDE_CODE_EFFORT_NEEDS_NEWER)).toBeInTheDocument();
    // The choice is kept for when Claude Code is updated.
    expect(within(card).getByRole("combobox", { name: "Effort" })).toHaveDisplayValue("High");
  });

  it("shows a stored model that is not in the list under Other", async () => {
    await open({ summaries: { settings: { writer: "ClaudeCode", claude_model: "claude-opus-4-1-20250805" } } });
    const card = await screen.findByRole("group", { name: "Claude through Claude Code" });
    expect(within(card).getByRole("combobox", { name: "Model" })).toHaveDisplayValue("Other");
    const other = within(card).getByRole("group", { name: "Other model" });
    expect(within(other).getByRole("textbox", { name: /^Model name/ })).toHaveValue("claude-opus-4-1-20250805");
    expect(within(other).getByText("Saved.")).toBeInTheDocument();
    cleanup();
    await open({ summaries: { settings: { writer: "Codex", codex_model: "gpt-5.6-luna" } } });
    const codex = await screen.findByRole("group", { name: "OpenAI through Codex" });
    expect(within(codex).getByRole("combobox", { name: "Model" })).toHaveDisplayValue("Other");
    expect(
      within(within(codex).getByRole("group", { name: "Other model" })).getByRole("textbox", { name: /^Model name/ }),
    ).toHaveValue("gpt-5.6-luna");
  });

  it("says how to sign in to Claude Code or install it", async () => {
    await open({ machine: "surface", summaries: { settings: { writer: "ClaudeCode" } } });
    expect(radio(/^Claude \(through Claude Code\)/)).toBeChecked();
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
    expect(screen.getByText(CODEX_PRIVACY)).toBeInTheDocument();
    expect(CODEX_PRIVACY).toContain("handed to Codex, which sends them to OpenAI on your ChatGPT sign-in");
    expect(screen.queryByRole("group", { name: "OpenAI through Codex" })).toBeNull();
    expect(asked.codex).toBe(0);
    await s.user.click(radio(/^OpenAI \(through Codex\)/));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("Codex"));
    const card = await screen.findByRole("group", { name: "OpenAI through Codex" });
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
    expect(radio(/^OpenAI \(through Codex\)/)).toBeChecked();
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
