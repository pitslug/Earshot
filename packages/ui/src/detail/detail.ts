// Pure helpers of the meeting detail screen (docs/ui-design.md "Meeting detail").

import {
  IpcFailure,
  type MeetingDetail,
  type PersonItem,
  type RememberOutcome,
  type RememberResult,
  type SpeakerCard,
} from "../ipc";
import { clock } from "../meeting/liveMeeting";
import { clockTime } from "../shell/meetings";

/** The header's second line: "Mon 2 Nov 09:30 · 42 min · Call · Final transcript · Summary by
 * Qwen 3.5 9B · Transcribed on GPU (CUDA)". */
export function headerLine(d: MeetingDetail): string {
  const h = d.header;
  const date = new Date(h.started_at_ms).toLocaleDateString(undefined, {
    weekday: "short",
    day: "numeric",
    month: "short",
  });
  const parts = [`${date} ${clockTime(h.started_at_ms)}`];
  if (h.duration_ms !== null) parts.push(`${Math.max(1, Math.round(h.duration_ms / 60_000))} min`);
  parts.push(h.mode === "Call" ? "Call" : "In the room");
  if (h.transcript === "Final") parts.push("Final transcript");
  if (h.transcript === "Draft") parts.push("Draft transcript");
  if (h.summary_model !== null) parts.push(`Summary by ${h.summary_model}`);
  if (h.transcript_backend !== null) parts.push(`Transcribed on ${h.transcript_backend}`);
  return parts.join(" · ");
}

/** The summary tab's line on the last handover to Claude Code (T7.4ga) or Codex (T7.4gc):
 * "Handed to Claude Code: 5,324 characters at 14:02"; null when the meeting has none. */
export function handoverLine(d: MeetingDetail): string | null {
  const last = d.handovers.at(-1);
  if (last === undefined) return null;
  const to = last.cli === "codex" ? "Codex" : "Claude Code";
  return `Handed to ${to}: ${last.characters.toLocaleString("en-GB")} characters at ${clockTime(last.at_ms)}`;
}

/** Where a speaker came from, as the panel says it. */
export function sourceLabel(s: SpeakerCard): string {
  switch (s.source) {
    case "Mic":
      return "Mic";
    case "FarEnd":
      return "Computer audio";
    case "Voiceprint":
      return "Matched voiceprint";
  }
}

/** A transcript time (meeting time). */
export function rowTime(ms: number): string {
  return clock(ms);
}

/** Voices that still need a name. */
export function voicesToName(d: MeetingDetail): number {
  return d.speakers.filter((s) => s.needs_name).length;
}

/** "2 voices need names. Names flow into the summary and export." */
export function namesBanner(n: number): string {
  return `${n} ${n === 1 ? "voice needs a name" : "voices need names"}. Names flow into the summary and export.`;
}

/** The speakers "Remember these voices" sends: named (linked to a person) and not the user. */
export function rememberable(d: MeetingDetail): SpeakerCard[] {
  return d.speakers.filter((s) => s.kind === "Other" && s.person !== null);
}

/** The persons a speaker may be linked to: the user's own person only for the "Me" row, and
 * only other people for everyone else. */
export function linkablePersons(card: SpeakerCard, persons: PersonItem[]): PersonItem[] {
  return persons.filter((p) => (card.kind === "Me" ? p.is_me : !p.is_me));
}

/** The person a typed name means: an existing (non-user) person with exactly that name, else
 * none (a new person). */
export function personNamed(name: string, persons: PersonItem[]): PersonItem | null {
  const n = name.trim().toLowerCase();
  const found = persons.filter((p) => !p.is_me && p.name.trim().toLowerCase() === n);
  return found.length === 1 ? found[0] : null;
}

/** Plain copy for a failed call. */
export function failureText(e: unknown): string {
  if (e instanceof IpcFailure) {
    if (e.error.kind === "meeting_busy") {
      return "This meeting is busy: it is being recorded, processed or deleted. Try again when that has finished.";
    }
    if (e.error.kind === "busy") {
      return "This meeting is already being re-transcribed or re-summarised.";
    }
    return e.error.message;
  }
  return e instanceof Error ? e.message : String(e);
}

/** What "Save voices" did, in plain copy: how many were remembered, then why the others
 * were not, by name. */
export function rememberText(results: RememberResult[], speakers: SpeakerCard[]): string {
  const name = (id: string) => speakers.find((s) => s.id === id)?.name ?? "A voice";
  const names = (outcome: RememberOutcome) =>
    results.filter((r) => r.outcome === outcome).map((r) => name(r.speaker));
  const list = (n: string[]) =>
    n.length <= 1 ? (n[0] ?? "") : `${n.slice(0, -1).join(", ")} and ${n[n.length - 1]}`;
  const verb = (n: string[], one: string, many: string) => (n.length === 1 ? one : many);
  const saved = names("Remembered").length;
  const parts = [`${saved} ${saved === 1 ? "voice" : "voices"} remembered.`];
  const already = names("AlreadyRemembered");
  if (already.length > 0) {
    parts.push(`${list(already)} ${verb(already, "was", "were")} already remembered from this meeting.`);
  }
  const short = names("NotEnoughSpeech");
  if (short.length > 0) {
    parts.push(`${list(short)} did not speak long enough to remember.`);
  }
  const none = names("NoStoredVoice");
  if (none.length > 0) {
    parts.push(
      `There is no stored voice for ${list(none)} yet: voices are kept once the meeting has been processed.`,
    );
  }
  const unsupported = names("Unsupported");
  if (unsupported.length > 0) {
    parts.push(`The speaker model in use cannot remember voices, so ${list(unsupported)} ${verb(unsupported, "was", "were")} not saved.`);
  }
  return parts.join(" ");
}

/** The meeting's recordings were deleted (audio retention, or by hand): it was recorded and
 * no track's recording is kept. While it records or has no stored track yet, nothing is
 * gone. */
export function recordingsGone(detail: MeetingDetail): boolean {
  return !detail.audio_kept && (detail.state === "Ready" || detail.state === "Unprocessed");
}
