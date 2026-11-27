// The Sync page part of the mock `Ipc` (T11.3a): a sync setting kept in memory, the server's
// answers made up (a fixed setup secret and a fixed pairing code) and the app's fixed copy read
// from `fixtures/ui/sync_copy_cases.json`, the table the desktop's own tests pin to
// `sync_settings::problem_copy` and `command_copy`. `problems` words the status line (the view
// carries it as copy); `command_problems` words a refused command, which nothing retries. The
// address and pairing code rules are only as much of the Rust rules as the table and the page
// tests need (`mockSync.test.ts` runs every `address_cases` row).

import offload from "../../../../fixtures/ui/offload_copy_cases.json";
import table from "../../../../fixtures/ui/sync_copy_cases.json";
import { IpcFailure } from "./ipc";
import type {
  SummaryHelperView,
  SummaryHelpersView,
  SyncCodeView,
  SyncCommands,
  SyncDeviceView,
  SyncView,
} from "./sync";

/** The offload copy the app sends (`fixtures/ui/offload_copy_cases.json`, pinned to the
 * desktop's `offload` copy by its tests). */
export const OFFLOAD_COPY = offload;

/** `template` with `{device}` filled: the name, else the table's unnamed words. */
export function offloadCopy(template: string, name: string | null, inline = false): string {
  const fill = name !== null && name.trim() !== "" ? name : inline ? offload.unnamed_inline : offload.unnamed;
  return template.replace("{device}", fill);
}

/** The longest name the app keeps for this computer (characters). */
export const MAX_DEVICE_NAME = 64;

/** What the app says about a longer name (`sync_settings::DEVICE_NAME_TOO_LONG`, the table's
 * `device_name_too_long`). */
export const DEVICE_NAME_TOO_LONG = table.device_name_too_long;

/** The full id the mock gives a device: its short id padded to a UUID. */
export function mockDeviceId(shortId: string): string {
  return `${shortId}-0000-7000-8000-000000000000`;
}

/** A UUID as the app reads a device id (hyphenated or not, either case). */
const UUID = /^(?:[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12})$/i;

/** A problem word the app sends (`sync_settings::problem_copy`). */
export type SyncProblemWord = keyof typeof table.problems;

/** The status line's copy of each problem word. */
export const SYNC_PROBLEMS: Record<SyncProblemWord, string> = table.problems;

/** A refused command's copy of each problem word (`sync_settings::command_copy`). */
export const SYNC_COMMAND_PROBLEMS: Record<SyncProblemWord, string> = table.command_problems;

/** What the app says when the setup secret is left empty. */
export const SECRET_MISSING = table.secret_missing;

/** What the app says when the last sync skipped damaged queued items (T12.77b8). */
export const DAMAGED_NOTE = table.damaged_note;

/** What the app says when the last sync left `count` queued items unsent because they cannot be
 * sealed (too large, or holding an audio file's signature; T14.43, `sync_settings::refused_note`):
 * null for none. */
export function refusedNote(count: number): string | null {
  if (count <= 0) return null;
  if (count === 1) return table.refused_note.one;
  return table.refused_note.many.replace("{count}", () => String(count));
}

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
  /** Dictionary words sync (default true, T11.3e). */
  dictionary?: boolean;
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
  /** Whether the last sync skipped damaged queued rows (default false). */
  unsendable?: boolean;
  /** The last sync left damaged rows here as they are (T14.57): the page shows `damaged_note`. */
  damaged?: boolean;
  /** Queued rows the last sync could not seal and left unsent (default 0, T14.43). */
  refused?: number;
  /** The vault's devices (default: this computer and three others). */
  devices?: SyncDeviceView[];
  /** "Write summaries for my other computers" (default false). */
  offersSummaries?: boolean;
  /** This computer's name (default "Desk"). */
  deviceName?: string;
  /** The chosen helper's short id (default none). */
  helper?: string | null;
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
  {
    this_device: true,
    name: "Desk",
    online: true,
    recording: false,
    offers_summaries: false,
    writer: null,
    id: mockDeviceId("3f2a9c1e"),
    short_id: "3f2a9c1e",
    joined_unix_s: 1_793_000_000,
    seen_s_ago: 0,
  },
  {
    this_device: false,
    name: null,
    online: false,
    recording: false,
    offers_summaries: false,
    writer: null,
    id: mockDeviceId("b71d04a8"),
    short_id: "b71d04a8",
    joined_unix_s: 1_793_100_000,
    seen_s_ago: 7_200,
  },
  {
    this_device: false,
    name: "Laptop",
    online: true,
    recording: true,
    offers_summaries: true,
    writer: offload.writers.claude_code,
    id: mockDeviceId("c5e09b72"),
    short_id: "c5e09b72",
    joined_unix_s: 1_793_200_000,
    seen_s_ago: 20,
  },
  {
    this_device: false,
    name: "Studio",
    online: true,
    recording: false,
    offers_summaries: true,
    writer: offload.writers.local,
    id: mockDeviceId("e4a17d30"),
    short_id: "e4a17d30",
    joined_unix_s: 1_793_300_000,
    seen_s_ago: 30,
  },
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
  let dictionary = data.dictionary ?? true;
  let address = data.address ?? (setUp ? "https://sync.example.com" : null);
  let lastSuccess = data.lastSuccessUnixMs ?? null;
  let problem: SyncProblemWord | null = data.problem ?? null;
  let stopped = data.stopped ?? (problem !== null && STOPPING.includes(problem));
  let waiting = data.waiting ?? 0;
  const held = data.held ?? 0;
  const unreadable = data.unreadable ?? 0;
  let devices = (data.devices ?? DEFAULT_DEVICES).map((d) => ({ ...d }));
  const now = data.now ?? Date.UTC(2026, 10, 2, 15, 0);
  let offersSummaries = data.offersSummaries ?? false;
  let deviceName = data.deviceName ?? "Desk";
  let helper: string | null = data.helper ?? null;
  // The computer each meeting was asked of (`offload::ask` keeps one open request per meeting).
  const asked = new Map<string, string>();
  const available = (d: SyncDeviceView) => d.online && d.offers_summaries && !d.recording && d.writer !== null;
  const helpers = (): SummaryHelpersView => {
    const ready = setUp && on;
    const listed = ready
      ? devices.filter((d) => !d.this_device && (d.offers_summaries || d.short_id === helper))
      : devices.filter((d) => d.short_id === helper);
    return {
      sync_ready: ready,
      chosen: helper === null ? null : mockDeviceId(helper),
      helpers: listed.map(
        (d): SummaryHelperView => ({
          id: mockDeviceId(d.short_id),
          name: ready ? d.name : null,
          short_id: d.short_id,
          writer: ready && d.offers_summaries ? d.writer : null,
          offers: ready && d.offers_summaries,
          online: ready && d.online,
          recording: ready && d.recording,
          available: ready && available(d),
          chosen: d.short_id === helper,
        }),
      ),
      problem: null,
    };
  };

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
    dictionary,
    address,
    address_problem: null,
    last_success_unix_ms: lastSuccess,
    problem: problem === null ? null : SYNC_PROBLEMS[problem],
    stopped,
    unreadable,
    waiting,
    damaged_note: data.unsendable === true || data.damaged === true ? DAMAGED_NOTE : null,
    refused_note: refusedNote(data.refused ?? 0),
    held,
    offers_summaries: offersSummaries,
    device_name: deviceName,
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
    setSyncDictionary: async (value) => {
      call("setSyncDictionary");
      dictionary = value;
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
    setSyncOfferSummaries: async (value) => {
      call("setSyncOfferSummaries");
      offersSummaries = value;
      return view();
    },
    setSyncDeviceName: async (name) => {
      call("setSyncDeviceName");
      const t = name.trim();
      if ([...t].length > MAX_DEVICE_NAME) invalid(DEVICE_NAME_TOO_LONG);
      deviceName = t === "" ? "Desk" : t;
      return view();
    },
    summaryHelpers: async () => {
      call("summaryHelpers");
      return helpers();
    },
    setSummaryHelper: async (device) => {
      call("setSummaryHelper");
      const found = device === null ? null : devices.find((d) => mockDeviceId(d.short_id) === device);
      if (device !== null && (found === undefined || found?.this_device)) {
        invalid("This computer cannot write summaries for itself.");
      }
      helper = found?.short_id ?? null;
      return helpers();
    },
    askSummaryHelper: async (_meeting, device) => {
      call("askSummaryHelper");
      const d = devices.find((x) => mockDeviceId(x.short_id) === device);
      const notAsked = (template: string) => {
        throw new IpcFailure({
          kind: "offload",
          message: offloadCopy(template, d?.name ?? null),
          retryable: false,
        });
      };
      if (!(setUp && on)) notAsked(offload.not_asked.sync_off);
      if (d === undefined || !d.online) notAsked(offload.not_asked.offline);
      else if (!d.offers_summaries) notAsked(offload.not_asked.not_offering);
      else if (d.recording) notAsked(offload.not_asked.recording);
      else if (d.writer === null) notAsked(offload.not_asked.no_writer);
      // One computer at a time per meeting.
      const open = asked.get(_meeting);
      if (open !== undefined && open !== device) {
        throw new IpcFailure({ kind: "offload", message: offload.already_waiting, retryable: false });
      }
      asked.set(_meeting, device);
      return offloadCopy(offload.waiting, d?.name ?? null, true);
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
    removeOtherSyncDevice: async (id) => {
      call("removeOtherSyncDevice");
      // Rust's order: text that is not an id, or this computer's, is refused before anything
      // else; then a computer that is not set up has no key (`vault`); an id the server does not
      // list changes nothing.
      const me = devices.find((d) => d.this_device);
      if (!UUID.test(id) || id.toLowerCase() === me?.id) invalid(table.remove_other.not_another);
      if (!setUp) commandFailure("vault");
      devices = devices.filter((d) => d.id !== id.toLowerCase());
      return { devices: devices.map((d) => ({ ...d })) };
    },
  };
}
