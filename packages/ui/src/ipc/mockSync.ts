// The Sync page part of the mock `Ipc` (T11.3a): a sync setting kept in memory, the server's
// answers made up (a fixed setup secret and a fixed pairing code) and the app's fixed copy read
// from `fixtures/ui/sync_copy_cases.json`, the table the desktop's own tests pin to
// `sync_settings::problem_copy` and `command_copy`. `problems` words the status line (the view
// carries it as copy); `command_problems` words a refused command, which nothing retries. The
// address and pairing code rules are only as much of the Rust rules as the table and the page
// tests need (`mockSync.test.ts` runs every `address_cases` row).

import table from "../../../../fixtures/ui/sync_copy_cases.json";
import { IpcFailure } from "./ipc";
import type { SyncCodeView, SyncCommands, SyncDeviceView, SyncView } from "./sync";

/** A problem word the app sends (`sync_settings::problem_copy`). */
export type SyncProblemWord = keyof typeof table.problems;

/** The status line's copy of each problem word. */
export const SYNC_PROBLEMS: Record<SyncProblemWord, string> = table.problems;

/** A refused command's copy of each problem word (`sync_settings::command_copy`). */
export const SYNC_COMMAND_PROBLEMS: Record<SyncProblemWord, string> = table.command_problems;

/** What the app says when the setup secret is left empty. */
export const SECRET_MISSING = table.secret_missing;

/** What the app says about an address over `MAX_ADDRESS_CHARS` (Rust's copy, not in the table). */
export const ADDRESS_TOO_LONG = "The address is too long.";

/** The longest address the app accepts (Rust's limit). */
const MAX_ADDRESS_CHARS = 2048;

/** The setup secret the mock's server holds. */
export const MOCK_SYNC_SECRET = "mock-setup-secret";

/** The pairing code the mock's other device shows, and the one this computer offers. */
export const MOCK_SYNC_CODE = "ABCDE-FGHJK-MNPQR-STVWX-YZ0123";

/** How long an offered pairing code works (s). */
const CODE_LIFE_S = 600;

/** The words that stop sync until something is done (the rest are tried again): the table's
 * `stops`, pinned to `SyncProblem::stops` by the desktop's tests. */
const STOPPING: readonly string[] = table.stops;

/** A command the page can call. */
export type SyncCommandName = keyof SyncCommands;

/** What the Sync mock starts with. */
export interface SyncMockData {
  /** This computer is set up (default false). */
  setUp?: boolean;
  /** Sync is on (default: as `setUp`). */
  on?: boolean;
  /** The sync thread is running (default: set up and on). */
  running?: boolean;
  /** Voiceprints sync (default false). */
  voiceprints?: boolean;
  /** The server's address (default `https://sync.example.com` when set up). */
  address?: string | null;
  /** When sync last finished without a problem (Unix ms, default none). */
  lastSuccessUnixMs?: number | null;
  /** The last problem's word (default none). */
  problem?: SyncProblemWord | null;
  /** Whether that problem stopped sync (default: by its word). */
  stopped?: boolean;
  /** Rows changed here and not yet sent (default 0). */
  waiting?: number;
  /** Rows from other devices held for later (default 0). */
  held?: number;
  /** Rows from other devices that could not be read (default 0). */
  unreadable?: number;
  /** The vault's devices (default: this computer and one other). */
  devices?: SyncDeviceView[];
  /** Commands refused with a problem word (their `command_problems` copy). */
  refuse?: Partial<Record<SyncCommandName, SyncProblemWord>>;
  /** The clock a sync now stamps (default 2026-11-02 15:00 UTC). */
  now?: number;
  /** Every call fails with this message. */
  error?: string;
  /** Called with the name of every command. */
  onCall?: (name: string) => void;
}

const DEFAULT_DEVICES: SyncDeviceView[] = [
  { this_device: true, short_id: "3f2a9c1e", joined_unix_s: 1_793_000_000, seen_s_ago: 0 },
  { this_device: false, short_id: "b71d04a8", joined_unix_s: 1_793_100_000, seen_s_ago: 7_200 },
];

function commandFailure(word: SyncProblemWord): never {
  throw new IpcFailure({ kind: "sync", message: SYNC_COMMAND_PROBLEMS[word], retryable: false });
}

function invalid(message: string): never {
  throw new IpcFailure({ kind: "invalid_setting", message, retryable: false });
}

/** The address as the mock stores it, or the copy that refuses it. Rust's order: empty, too
 * long, not a web address, credentials, path, query or fragment, then plain http to another
 * computer. */
export function checkSyncAddress(input: string): { address: string } | { refused: string } {
  const copy = (input: string) => table.address_cases.find((c) => c.input === input)?.copy ?? "";
  const t = input.trim();
  if (t === "") return { refused: copy("") };
  if ([...t].length > MAX_ADDRESS_CHARS) return { refused: ADDRESS_TOO_LONG };
  if (!t.includes("://")) return { refused: copy("sync.example.com") };
  let url: URL;
  try {
    url = new URL(t);
  } catch {
    return { refused: copy("sync.example.com") };
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") return { refused: copy("sync.example.com") };
  if (url.username !== "" || url.password !== "") return { refused: copy("https://user:pw@sync.example.com") };
  if (url.pathname !== "/" || url.search !== "" || url.hash !== "" || /[?#]$/.test(t)) {
    return { refused: copy("https://sync.example.com/earshot") };
  }
  const local = url.hostname === "localhost" || url.hostname === "[::1]" || /^127\./.test(url.hostname);
  if (url.protocol === "http:" && !local) return { refused: copy("http://sync.example.com") };
  return { address: t.replace(/\/$/, "") };
}

/** A pairing code in its canonical form (Crockford base32: case-insensitive, I and L read as 1,
 * O as 0, dashes and spaces ignored), or null when it is not 26 such characters. */
export function canonicalCode(input: string): string | null {
  const c = input
    .replace(/[-\s]/g, "")
    .toUpperCase()
    .replace(/[IL]/g, "1")
    .replace(/O/g, "0");
  return /^[0-9A-HJKMNP-TV-Z]{26}$/.test(c) ? c : null;
}

/** The Sync page of the mock `Ipc`. */
export function createSyncMock(data: SyncMockData = {}): SyncCommands {
  let setUp = data.setUp ?? false;
  let on = data.on ?? setUp;
  let running = data.running ?? (setUp && on);
  let voiceprints = data.voiceprints ?? false;
  let address = data.address ?? (setUp ? "https://sync.example.com" : null);
  let lastSuccess = data.lastSuccessUnixMs ?? null;
  let problem: SyncProblemWord | null = data.problem ?? null;
  let stopped = data.stopped ?? (problem !== null && STOPPING.includes(problem));
  let waiting = data.waiting ?? 0;
  const held = data.held ?? 0;
  const unreadable = data.unreadable ?? 0;
  let devices = (data.devices ?? DEFAULT_DEVICES).map((d) => ({ ...d }));
  const now = data.now ?? Date.UTC(2026, 10, 2, 15, 0);

  const call = (name: SyncCommandName) => {
    data.onCall?.(name);
    if (data.error !== undefined) throw new IpcFailure({ kind: "sync", message: data.error, retryable: false });
    const word = data.refuse?.[name];
    if (word !== undefined) commandFailure(word);
  };
  const view = (): SyncView => ({
    set_up: setUp,
    on,
    running: setUp && on && running,
    voiceprints,
    address,
    address_problem: null,
    last_success_unix_ms: lastSuccess,
    problem: problem === null ? null : SYNC_PROBLEMS[problem],
    stopped,
    unreadable,
    waiting,
    held,
  });
  const storedAddress = (input: string) => {
    const checked = checkSyncAddress(input);
    if ("refused" in checked) invalid(checked.refused);
    return checked.address;
  };
  const begin = (at: string) => {
    setUp = true;
    on = true;
    running = true;
    address = at;
    problem = null;
    stopped = false;
    devices = (data.devices ?? DEFAULT_DEVICES).map((d) => ({ ...d }));
  };

  return {
    syncView: async () => {
      call("syncView");
      return view();
    },
    syncDevices: async () => {
      call("syncDevices");
      return setUp ? devices.map((d) => ({ ...d })) : [];
    },
    setUpSync: async (input, secret) => {
      call("setUpSync");
      const at = storedAddress(input);
      if (secret.trim() === "") invalid(SECRET_MISSING);
      if (secret !== MOCK_SYNC_SECRET) commandFailure("bad_secret");
      begin(at);
      devices = devices.filter((d) => d.this_device);
      return view();
    },
    offerSyncCode: async (): Promise<SyncCodeView> => {
      call("offerSyncCode");
      return { code: MOCK_SYNC_CODE, expires_in_s: CODE_LIFE_S };
    },
    joinSync: async (input, code) => {
      call("joinSync");
      const at = storedAddress(input);
      const given = canonicalCode(code);
      if (given === null) commandFailure("bad_code");
      if (given !== canonicalCode(MOCK_SYNC_CODE)) commandFailure("no_slot");
      begin(at);
      return view();
    },
    setSyncOn: async (value) => {
      call("setSyncOn");
      on = value;
      running = value;
      return view();
    },
    setSyncVoiceprints: async (value) => {
      call("setSyncVoiceprints");
      voiceprints = value;
      return view();
    },
    syncNow: async () => {
      call("syncNow");
      if (setUp && on) {
        running = true;
        if (problem === null) {
          waiting = 0;
          lastSuccess = now;
        }
      }
      return view();
    },
    removeSyncDevice: async () => {
      call("removeSyncDevice");
      setUp = false;
      on = false;
      running = false;
      voiceprints = false;
      address = null;
      lastSuccess = null;
      problem = null;
      stopped = false;
      waiting = 0;
      return view();
    },
  };
}
