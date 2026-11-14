// The folder part of the mock `Ipc` (T9.5c): folders kept in memory beside the mock's meeting
// rows, with the refusals of `earshot_runtime::folders` in its fixed copy. The rules
// themselves live in Rust and are tested there.

import type { FolderCommands, FolderDeleted, FolderListItem } from "./folders";
import { IpcFailure, type MeetingListItem } from "./ipc";

/** The sample folders of the preview (invented names): `sampleMeetings` files a meeting in
 * each. */
export const SAMPLE_FOLDERS: FolderListItem[] = [
  { id: "0190f1a2-0000-7000-8000-f00000000001", name: "Planning" },
  { id: "0190f1a2-0000-7000-8000-f00000000002", name: "Supplier calls" },
];

/** What the folder mock starts with. */
export interface FoldersMockData {
  /** The folders (default: `SAMPLE_FOLDERS` with the sample meetings, none with given ones). */
  folders?: FolderListItem[];
}

/** What the folder mock needs of the rest of the mock. */
export interface FoldersMockHooks {
  /** Called with the arguments of every command. */
  call: (...args: unknown[]) => void;
  /** Whether a meeting is recording (it cannot be moved). */
  recording: (meeting: string) => boolean;
  /** Whether a meeting is busy (it cannot be deleted). */
  busy: (meeting: string) => boolean;
  /** Deletes a meeting that is not busy. */
  remove: (meeting: string) => void;
}

const fail = (kind: string, message: string): never => {
  throw new IpcFailure({ kind, message, retryable: false });
};

const fold = (name: string) => name.toLocaleLowerCase();

/** `earshot_core::metrics::normalize::fold_diacritic`: a lowercase Latin letter with a
 * diacritic and its base letters. */
const BASE_LETTERS: [string, string][] = [
  ["àáâãäåāăą", "a"],
  ["çćĉċč", "c"],
  ["ďđð", "d"],
  ["èéêëēĕėęě", "e"],
  ["ĝğġģ", "g"],
  ["ĥ", "h"],
  ["ìíîïĩīĭį", "i"],
  ["ĵ", "j"],
  ["ķ", "k"],
  ["ĺļľł", "l"],
  ["ñńņň", "n"],
  ["òóôõöøōŏő", "o"],
  ["ŕŗř", "r"],
  ["śŝşš", "s"],
  ["ţť", "t"],
  ["ùúûüũūŭůűų", "u"],
  ["ŵ", "w"],
  ["ýÿŷ", "y"],
  ["źżž", "z"],
  ["æ", "ae"],
  ["œ", "oe"],
  ["ß", "ss"],
  ["þ", "th"],
];
/** Each letter of `BASE_LETTERS` and its base letters; pinned to core's whole table by
 * `fixtures/ui/base_letter_cases.json`, generated from it by the runtime's tests. */
export const BASE_OF = new Map(BASE_LETTERS.flatMap(([letters, base]) => [...letters].map((l) => [l, base] as const)));

/** Rust's `char::to_lowercase` per character (`search::fold_case`), not a locale's. */
const foldCase = (name: string) => [...name].map((c) => c.toLowerCase()).join("");

/** Compares by code point, as Rust compares strings (`<` on JS strings compares UTF-16 units). */
function compareCodePoints(a: string, b: string): number {
  const x = [...a].map((c) => c.codePointAt(0)!);
  const y = [...b].map((c) => c.codePointAt(0)!);
  for (let i = 0; i < Math.min(x.length, y.length); i += 1) {
    if (x[i] !== y[i]) return x[i] - y[i];
  }
  return x.length - y.length;
}

/** The folder list's order (T12.45), `folders::name_order`: the case-folded name with Latin
 * diacritics folded to their base letters, then the case-folded name; by code point.
 * `fixtures/ui/folder_sort_cases.json` (both suites run it) keeps the two the same. */
export function folderOrder(a: string, b: string): number {
  const fa = foldCase(a);
  const fb = foldCase(b);
  const base = (s: string) => [...s].map((c) => BASE_OF.get(c) ?? c).join("");
  return compareCodePoints(base(fa), base(fb)) || compareCodePoints(fa, fb);
}

/** The folder commands of the mock `Ipc`, over `meetings` (shared with the mock, changed in
 * place). */
export function createFoldersMock(
  meetings: MeetingListItem[],
  start: FolderListItem[],
  hooks: FoldersMockHooks,
): FolderCommands {
  let folders = start.map((f) => ({ ...f }));
  let seq = 0;
  const sorted = () => [...folders].sort((a, b) => folderOrder(a.name, b.name) || a.id.localeCompare(b.id));
  const cleanName = (name: string) => {
    const n = name.trim();
    if (n === "") fail("invalid_name", "Enter a name.");
    if ([...n].length > 100) fail("invalid_name", "Names can be up to 100 characters long.");
    // eslint-disable-next-line no-control-regex
    if (/[\u0000-\u001f\u007f]/.test(n)) fail("invalid_name", "Names cannot contain line breaks or tabs.");
    return n;
  };
  const checkFree = (name: string, except: string | null) => {
    if (folders.some((f) => f.id !== except && fold(f.name) === fold(name))) {
      fail("folder_name_taken", "Another folder already has this name.");
    }
  };
  const folderOf = (id: string) =>
    folders.find((f) => f.id === id) ?? fail("folder_not_found", "This folder no longer exists.");
  const members = (id: string) => meetings.filter((m) => m.folder_id === id);
  const close = (id: string) => {
    folders = folders.filter((f) => f.id !== id);
    let unfiled = 0;
    for (const m of members(id)) {
      m.folder_id = null;
      unfiled += 1;
    }
    return unfiled;
  };
  return {
    listFolders: async () => sorted().map((f) => ({ ...f })),
    createFolder: async (name) => {
      hooks.call("createFolder", name);
      const n = cleanName(name);
      checkFree(n, null);
      seq += 1;
      const id = `0190f1a2-0000-7000-8000-f100000000${String(seq).padStart(2, "0")}`;
      folders.push({ id, name: n });
      return id;
    },
    renameFolder: async (folder, name) => {
      hooks.call("renameFolder", folder, name);
      const n = cleanName(name);
      const f = folderOf(folder);
      checkFree(n, folder);
      f.name = n;
    },
    moveToFolder: async (meeting, folder) => {
      hooks.call("moveToFolder", meeting, folder);
      const m = meetings.find((x) => x.id === meeting) ?? fail("meeting_not_found", "This meeting no longer exists.");
      if (hooks.recording(meeting)) {
        fail("meeting_recording", "A meeting can be moved to a folder once it has finished recording.");
      }
      if (folder !== null) folderOf(folder);
      m.folder_id = folder;
    },
    deleteFolderKeepingMeetings: async (folder): Promise<FolderDeleted> => {
      hooks.call("deleteFolderKeepingMeetings", folder);
      folderOf(folder);
      const unfiled = close(folder);
      return { folder_deleted: true, meetings_deleted: 0, meetings_left: 0, meetings_unfiled: unfiled };
    },
    deleteFolderWithMeetings: async (folder): Promise<FolderDeleted> => {
      hooks.call("deleteFolderWithMeetings", folder);
      folderOf(folder);
      let deleted = 0;
      let failed = 0;
      for (const m of members(folder)) {
        if (hooks.busy(m.id)) {
          failed += 1;
        } else {
          hooks.remove(m.id);
          deleted += 1;
        }
      }
      if (failed > 0) {
        return { folder_deleted: false, meetings_deleted: deleted, meetings_left: members(folder).length, meetings_unfiled: 0 };
      }
      return { folder_deleted: true, meetings_deleted: deleted, meetings_left: 0, meetings_unfiled: close(folder) };
    },
  };
}
