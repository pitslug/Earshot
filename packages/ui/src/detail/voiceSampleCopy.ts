// What a speaker edit did to a saved voice (T12.77b7, docs/voice-samples.md section 10): the
// copy the owner agreed, from `fixtures/ui/voice_sample_copy_cases.json` (the desktop pins it
// to the design's table and pins `since`). Results carry counts and flags; the words are here.

import table from "../../../../fixtures/ui/voice_sample_copy_cases.json";
import type {
  LinkResult,
  MarkedMeResult,
  MergeResult,
  NotThisPersonResult,
  SavedVoiceResult,
} from "../ipc/ipc";
import { savedText } from "./detail";

/** The copy table (`fixtures/ui/voice_sample_copy_cases.json`). */
export const VOICE_SAMPLE_COPY = table;

/** `template` with `{name}`, `{other}` and `{since}` filled. A function replacer, so a name
 * holding `$&` or `$1` stays as written. */
export function fillVoiceCopy(template: string, values: { name?: string; other?: string }): string {
  return template.replace(/\{(name|other|since)\}/g, (_whole, key: string) => {
    if (key === "since") return table.since;
    return (key === "name" ? values.name : values.other) ?? "";
  });
}

/** What "Not <name>" did, for the voice that was `name`'s. */
export function notThisPersonText(name: string, result: NotThisPersonResult): string {
  if (result.saved_before_samples) return fillVoiceCopy(table.not_saved_before_samples, { name });
  if (result.taken_back > 0) return fillVoiceCopy(table.not_taken_back, { name });
  return table.not_otherwise;
}

/** What linking a voice that was `name`'s to `other` did, or `null` when no saved voice
 * changed (nothing to say beyond the new name in the transcript). */
export function linkedText(name: string | null, other: string, result: LinkResult): string | null {
  // The same person as before: what was taken back was a stray sample saved for someone else,
  // and naming the same person twice would say nothing true.
  if (result.taken_back === 0 || name === null || name === other) return null;
  return fillVoiceCopy(table.linked_away, { name, other });
}

/** What "Save" did on a voice that was `name`'s (`null`: no one), saved as `other`. A moved
 * sample says so; a refused save that still took the voice from `name` says the link and why
 * nothing was saved; otherwise the save's own words. */
export function savedVoiceText(name: string | null, other: string, result: SavedVoiceResult): string {
  const saved = result.outcome === "Remembered" || result.outcome === "Replaced";
  if (name !== null && result.moved && saved) return fillVoiceCopy(table.save_moved, { name, other });
  if (name !== null && name !== other && result.taken_back > 0 && !saved && result.outcome !== "AlreadyRemembered") {
    // The refusal's reason alone: its "Saved {other}." lead would contradict the link.
    const full = savedText(other, result.outcome);
    const lead = `Saved ${other}. `;
    const reason = full.startsWith(lead) ? full.slice(lead.length) : full;
    return `${fillVoiceCopy(table.linked_away, { name, other })} ${reason}`;
  }
  return savedText(other, result.outcome);
}

/** What "This is me" did on a voice that was `name`'s, or `null` when no saved voice changed. */
export function markedMeText(name: string | null, result: MarkedMeResult): string | null {
  if (result.taken_back === 0 || name === null) return null;
  return fillVoiceCopy(table.marked_me, { name });
}

/** What merging away a voice that was `name`'s did, or `null` when no saved voice changed. */
export function mergedText(name: string | null, result: MergeResult): string | null {
  if (result.samples_taken_back === 0 || name === null) return null;
  return fillVoiceCopy(table.merged, { name });
}

/** The "Not <name>" tooltip when this meeting's voice is saved as a sample of `name`'s voice. */
export function notThisPersonSavedHint(name: string): string {
  return fillVoiceCopy(table.not_hint_saved, { name });
}
