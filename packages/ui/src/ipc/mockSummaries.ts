// The Summaries page part of the mock `Ipc` (T7.4g): the settings with the Rust rules
// (`earshot_runtime::summary_settings`) and their messages, and an API key kept in memory
// that, as in the app, is never returned. The same on both sample machines (the model in use
// comes from the Models mock) except Claude Code's status (T7.4ga): signed in on the desktop,
// installed but not signed in on the Surface; and Codex's (T7.4gc): installed and signed in
// with ChatGPT on the desktop, not installed on the Surface.

import {
  type ClaudeCodeStatusView,
  type CodexStatusView,
  IpcFailure,
  type SummarySettingsValue,
  type SummarySettingsView,
} from "./ipc";
import type { SampleMachine } from "./mockModels";
import type { SummaryStyleView } from "./summaryStyle";
import type { SummaryVocabularyView } from "./summaryVocabulary";

/** The Anthropic API address the app offers. */
export const MOCK_ANTHROPIC_URL = "https://api.anthropic.com";

/** The reason the app gives when summarising during the meeting cannot be used. */
const LIVE_NEEDS_LOCAL = "Summarising during the meeting needs a local summary model.";

/** What the Summaries mock starts with. */
export interface SummariesMockData {
  /** The stored settings (default: on this computer, no endpoint, live off). */
  settings?: Partial<SummarySettingsValue>;
  /** Whether an API key is stored at the start. */
  apiKeySaved?: boolean;
  /** Every key call fails with this Credential Manager error. */
  keyStoreError?: string;
  /** What `claudeCodeStatus` answers (default: the sample machine's, `CLAUDE_CODE_STATUS`). */
  claudeCode?: ClaudeCodeStatusView;
  /** What `codexStatus` answers (default: the sample machine's, `CODEX_STATUS`). */
  codex?: CodexStatusView;
  /** The stored summary style (T7.4b; default: none). */
  style?: string;
  /** The stored Dictionary words (T12.15a; default: none). */
  vocabulary?: string[];
}

/** The longest summary style the app accepts (`earshot_core::prompt::MAX_STYLE_CHARS`). */
export const MAX_STYLE_CHARS = 1000;

/** The most Dictionary words the app accepts (`earshot_core::prompt::MAX_VOCABULARY_WORDS`). */
export const MAX_VOCABULARY_WORDS = 100;
/** The longest Dictionary word (`earshot_core::prompt::MAX_VOCABULARY_WORD_CHARS`). */
export const MAX_VOCABULARY_WORD_CHARS = 40;
/** The longest Dictionary in all (`earshot_core::prompt::MAX_VOCABULARY_CHARS`). */
export const MAX_VOCABULARY_CHARS = 1000;

/** What the page says when the installed Claude Code has no `--effort` (the app's copy,
 * `summaries::EFFORT_NEEDS_NEWER`, T12.18). */
export const CLAUDE_CODE_EFFORT_NEEDS_NEWER =
  "Choosing the effort needs a newer Claude Code. This one runs at its own default effort; update Claude Code to choose it.";

/** Claude Code's status on each sample machine (the app's copy, `summaries::claude_code_status`). */
export const CLAUDE_CODE_STATUS: Record<SampleMachine, ClaudeCodeStatusView> = {
  desktop: {
    state: "SignedIn",
    version: "2.1.290 (Claude Code)",
    message: "Claude Code 2.1.290 is installed and signed in.",
    fix: null,
    effort_note: null,
  },
  surface: {
    state: "NotSignedIn",
    version: "2.1.290 (Claude Code)",
    message: "Claude Code 2.1.290 is installed but not signed in.",
    fix: "Open Claude Code and sign in (in a terminal, run claude, then /login), then check again.",
    effort_note: null,
  },
};

/** Claude Code's status when it is not installed. */
export const CLAUDE_CODE_NOT_INSTALLED: ClaudeCodeStatusView = {
  state: "NotInstalled",
  version: null,
  message: "Claude Code is not installed on this computer.",
  fix: "Install Claude Code and sign in to it, then check again.",
  effort_note: null,
};

/** Codex's status when it is not installed (the app's copy, `summaries::codex_status`). */
export const CODEX_NOT_INSTALLED: CodexStatusView = {
  state: "NotInstalled",
  version: null,
  message: "Codex is not installed on this computer.",
  fix: "Install the Codex CLI (in a terminal, run npm install -g @openai/codex) and sign in to it, then check again.",
};

/** Codex's status on each sample machine. */
export const CODEX_STATUS: Record<SampleMachine, CodexStatusView> = {
  desktop: {
    state: "SignedIn",
    version: "codex-cli 0.155.1",
    message: "Codex 0.155.1 is installed and signed in with ChatGPT.",
    fix: null,
  },
  surface: CODEX_NOT_INSTALLED,
};

/** The Summaries page methods of the mock. */
export interface SummariesMock {
  summarySettings(): Promise<SummarySettingsView>;
  setSummarySettings(settings: SummarySettingsValue): Promise<SummarySettingsView>;
  setApiKey(key: string): Promise<void>;
  hasApiKey(): Promise<boolean>;
  removeApiKey(): Promise<void>;
  claudeCodeStatus(): Promise<ClaudeCodeStatusView>;
  codexStatus(): Promise<CodexStatusView>;
  summaryStyle(): Promise<SummaryStyleView>;
  setSummaryStyle(style: string): Promise<SummaryStyleView>;
  summaryVocabulary(): Promise<SummaryVocabularyView>;
  setSummaryVocabulary(words: string[]): Promise<SummaryVocabularyView>;
}

/** The words as stored, with the app's rules and messages (`Vocabulary::new`): a control
 * character refuses the list; each word is trimmed with inner white space joined, empty ones
 * and repeats in another case are dropped (the first spelling wins), then the limits apply. */
function checkVocabulary(words: string[]): string[] {
  const kept: string[] = [];
  const folded = new Set<string>();
  let total = 0;
  for (const raw of words) {
    if (/\p{Cc}/u.test(raw)) fail("invalid_setting", "A word can hold only text, with no line breaks or tabs.");
    const word = raw.split(/\s+/).filter((p) => p !== "").join(" ");
    if (word === "") continue;
    const n = [...word].length;
    if (n > MAX_VOCABULARY_WORD_CHARS) {
      fail("invalid_setting", `A word can have at most ${MAX_VOCABULARY_WORD_CHARS} characters; this one has ${n}.`);
    }
    const key = word.toLowerCase();
    if (folded.has(key)) continue;
    folded.add(key);
    total += n;
    kept.push(word);
  }
  if (kept.length > MAX_VOCABULARY_WORDS) {
    fail("invalid_setting", `The list can hold at most ${MAX_VOCABULARY_WORDS} words; this one has ${kept.length}.`);
  }
  if (total > MAX_VOCABULARY_CHARS) {
    fail("invalid_setting", `The list can have at most ${MAX_VOCABULARY_CHARS} characters in all; this one has ${total}.`);
  }
  return kept;
}

/** The style as stored, with the app's rules and messages (`SummaryStyle::new`): control
 * characters other than tabs and line breaks are refused, CR LF becomes LF, the text is
 * trimmed and may hold at most `MAX_STYLE_CHARS` characters. */
function checkStyle(text: string): string {
  if (/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(text)) {
    fail("invalid_setting", "The summary style can hold only text, line breaks and tabs.");
  }
  const style = text.replace(/\r\n?/g, "\n").trim();
  const n = [...style].length;
  if (n > MAX_STYLE_CHARS) {
    fail("invalid_setting", `The summary style can be at most ${MAX_STYLE_CHARS} characters; this one has ${n}.`);
  }
  return style;
}

const fail = (kind: string, message: string): never => {
  throw new IpcFailure({ kind, message, retryable: false });
};

/** Whether `host` is this computer or a private LAN address (earshot-net's rule). */
function isLocalHost(host: string): boolean {
  const h = host.replace(/^\[|\]$/g, "").toLowerCase();
  if (h === "localhost" || h === "::1") return true;
  if (/^f[cd][0-9a-f]{0,2}:/.test(h)) return true;
  const v4 = h.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
  if (!v4) return false;
  const [a, b] = [Number(v4[1]), Number(v4[2])];
  return a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168);
}

/** The parsed URL, or null when it is not one. */
function parse(url: string): URL | null {
  try {
    return new URL(url);
  } catch {
    return null;
  }
}

function checkUrl(url: string, provider: SummarySettingsValue["provider"]): void {
  const bad = (why: string): never => fail("invalid_setting", `This address cannot be used: ${why}.`);
  const u = parse(url);
  if (u === null) bad("not a valid URL: relative URL without a base");
  const parsed = u as URL;
  const scheme = parsed.protocol.replace(/:$/, "");
  if (scheme !== "http" && scheme !== "https") bad(`scheme \`${scheme}\` is not allowed (use http or https)`);
  if (parsed.username !== "" || parsed.password !== "") bad("the URL must not contain a user name or password");
  if (parsed.search !== "" || parsed.hash !== "" || url.includes("?") || url.includes("#")) {
    bad("it must not contain a query (?) or a fragment (#)");
  }
  const path = parsed.pathname.replace(/\/+$/, "");
  if (provider === "Anthropic" && path !== "") {
    bad(`use the address without a path, for example ${MOCK_ANTHROPIC_URL}`);
  }
  if (provider === "OpenAiCompatible" && path.endsWith("/chat/completions")) {
    bad("use the address without /chat/completions, for example http://127.0.0.1:8080/v1");
  }
  if (scheme === "http" && !isLocalHost(parsed.hostname)) {
    bad("plain http is only allowed to this computer or the local network; use https");
  }
}

function validate(s: SummarySettingsValue): SummarySettingsValue {
  const url = s.url.trim();
  const model = s.model.trim();
  if (model.length > 200 || /[\u0000-\u001f\u007f]/.test(model)) {
    fail("invalid_setting", "The model name must be at most 200 characters, without line breaks.");
  }
  const claude_model = s.claude_model.trim();
  if (claude_model.length > 200) {
    fail("invalid_setting", "This Claude Code model name cannot be used: it is longer than 200 characters.");
  }
  if (claude_model.startsWith("-")) {
    fail("invalid_setting", "This Claude Code model name cannot be used: it must not start with a hyphen.");
  }
  if (!/^[A-Za-z0-9._:\[\]-]*$/.test(claude_model)) {
    fail(
      "invalid_setting",
      "This Claude Code model name cannot be used: use only letters, digits, hyphens, dots, colons and square brackets.",
    );
  }
  // Codex (`earshot_llm::codex::check_model`): a letter or digit, then up to 63 of
  // letters, digits and `._-`.
  const codex_model = s.codex_model.trim();
  if (codex_model.length > 64) {
    fail("invalid_setting", "This Codex model name cannot be used: it is longer than 64 characters.");
  }
  if (codex_model !== "" && !/^[A-Za-z0-9]/.test(codex_model)) {
    fail("invalid_setting", "This Codex model name cannot be used: it must start with a letter or digit.");
  }
  if (!/^[A-Za-z0-9._-]*$/.test(codex_model)) {
    fail(
      "invalid_setting",
      "This Codex model name cannot be used: use only letters, digits, hyphens, dots and underscores.",
    );
  }
  if (url !== "") checkUrl(url, s.provider);
  if (s.writer === "OwnEndpoint") {
    if (url === "") fail("invalid_setting", "Enter the endpoint's address.");
    if (model === "") fail("invalid_setting", "Enter the model name the endpoint expects.");
  }
  return { ...s, url, model, claude_model, codex_model };
}

function view(s: SummarySettingsValue): SummarySettingsView {
  const u = s.url === "" ? null : parse(s.url);
  const host = u === null ? null : u.hostname.replace(/^\[|\]$/g, "");
  return {
    value: { ...s },
    unknown: [],
    endpoint_host: host,
    endpoint_local: host !== null && isLocalHost(host),
    live_unavailable: s.writer === "OnThisComputer" ? null : LIVE_NEEDS_LOCAL,
    anthropic_url: MOCK_ANTHROPIC_URL,
  };
}

/** The Summaries page of the mock `Ipc`. */
export function createSummariesMock(
  data: SummariesMockData = {},
  machine: SampleMachine = "surface",
): SummariesMock {
  let settings: SummarySettingsValue = {
    writer: "OnThisComputer",
    provider: "OpenAiCompatible",
    url: "",
    model: "",
    live: false,
    claude_model: "",
    claude_effort: null,
    codex_model: "",
    ...data.settings,
  };
  let keySaved = data.apiKeySaved ?? false;
  let style = data.style ?? "";
  const styleView = (): SummaryStyleView => ({ style, unknown: [], max_chars: MAX_STYLE_CHARS });
  let vocabulary = checkVocabulary(data.vocabulary ?? []);
  const vocabularyView = (): SummaryVocabularyView => ({
    words: [...vocabulary],
    unknown: [],
    max_words: MAX_VOCABULARY_WORDS,
    max_word_chars: MAX_VOCABULARY_WORD_CHARS,
    max_chars: MAX_VOCABULARY_CHARS,
  });
  const store = () => {
    if (data.keyStoreError !== undefined) {
      fail("api_key", `Windows Credential Manager failed: ${data.keyStoreError}.`);
    }
  };
  return {
    summarySettings: async () => view(settings),
    setSummarySettings: async (s) => {
      settings = validate(s);
      return view(settings);
    },
    setApiKey: async (key) => {
      const k = key.trim();
      if (k === "") fail("invalid_setting", "This API key cannot be saved: it is empty.");
      if (/[\s\u0000-\u001f\u007f]/.test(k)) {
        fail("invalid_setting", "This API key cannot be saved: it holds spaces or line breaks; paste only the key.");
      }
      store();
      keySaved = true;
    },
    hasApiKey: async () => {
      store();
      return keySaved;
    },
    removeApiKey: async () => {
      store();
      keySaved = false;
    },
    claudeCodeStatus: async () => ({ ...(data.claudeCode ?? CLAUDE_CODE_STATUS[machine]) }),
    codexStatus: async () => ({ ...(data.codex ?? CODEX_STATUS[machine]) }),
    summaryStyle: async () => styleView(),
    setSummaryStyle: async (text) => {
      style = checkStyle(text);
      return styleView();
    },
    summaryVocabulary: async () => vocabularyView(),
    setSummaryVocabulary: async (words) => {
      vocabulary = checkVocabulary(words);
      return vocabularyView();
    },
  };
}
