// The voice sample copy (T12.77b7): the agreed table, filled by a function replacer.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/voice_sample_copy_cases.json";
import {
  fillVoiceCopy,
  linkedText,
  markedMeText,
  mergedText,
  notThisPersonSavedHint,
  notThisPersonText,
  savedVoiceText,
} from "./voiceSampleCopy";

describe("voice sample copy", () => {
  it("fills names with a function replacer, so a name holding $& stays as written", () => {
    expect(fillVoiceCopy(table.not_taken_back, { name: "Ana $& $1 Diaz" })).toBe(
      "Unlinked. This meeting's voice is no longer part of Ana $& $1 Diaz's saved voice.",
    );
    expect(fillVoiceCopy(table.save_moved, { name: "Ana", other: "Bo $'" })).toBe(
      "Saved for Bo $'. This meeting's voice is no longer part of Ana's saved voice.",
    );
  });

  it("says what Not did: taken back, saved before samples, or just unlinked", () => {
    expect(notThisPersonText("Ana Diaz", { taken_back: 1, saved_before_samples: false })).toBe(
      "Unlinked. This meeting's voice is no longer part of Ana Diaz's saved voice.",
    );
    expect(notThisPersonText("Ana Diaz", { taken_back: 0, saved_before_samples: true })).toBe(
      "Unlinked. This voice was saved before Earshot 0.4.0, so it stays part of Ana Diaz's saved voice. To start their voice over, remove them on the People page.",
    );
    expect(notThisPersonText("Ana Diaz", { taken_back: 0, saved_before_samples: false })).toBe("Unlinked.");
    expect(table.since).toBe("0.4.0");
  });

  it("says a link, a mark or a merge took a voice out only when it did", () => {
    expect(linkedText("Ana", "Bo", { person: "p", taken_back: 1 })).toBe(
      "Linked to Bo. This meeting's voice is no longer part of Ana's saved voice.",
    );
    expect(linkedText("Ana", "Bo", { person: "p", taken_back: 0 })).toBeNull();
    expect(linkedText(null, "Bo", { person: "p", taken_back: 1 })).toBeNull();
    // Linked to the same person again (a stray sample for someone else went): no notice.
    expect(linkedText("Bo", "Bo", { person: "p", taken_back: 1 })).toBeNull();
    expect(markedMeText("Ana", { offers: [], taken_back: 1 })).toBe(
      "Marked as you. This meeting's voice is no longer part of Ana's saved voice.",
    );
    expect(markedMeText("Ana", { offers: [], taken_back: 0 })).toBeNull();
    expect(mergedText("Ana", { samples_taken_back: 2 })).toBe(
      "Merged. The merged voice is no longer part of Ana's saved voice: save the voice again to add it.",
    );
    expect(mergedText("Ana", { samples_taken_back: 0 })).toBeNull();
  });

  it("says a Save moved the voice, or that the link moved it and why nothing was saved", () => {
    const base = { person: "p", taken_back: 1, moved: true };
    expect(savedVoiceText("Ana", "Bo", { ...base, outcome: "Remembered" })).toBe(
      "Saved for Bo. This meeting's voice is no longer part of Ana's saved voice.",
    );
    // Refused: the link and the refusal's reason, without "Saved Bo." in between.
    expect(savedVoiceText("Ana", "Bo", { ...base, moved: false, outcome: "NotEnoughSpeech" })).toBe(
      "Linked to Bo. This meeting's voice is no longer part of Ana's saved voice. They did not speak long enough for their voice to be remembered, so later meetings will not recognise it yet.",
    );
    expect(savedVoiceText("Ana", "Bo", { ...base, moved: false, outcome: "Unsupported" })).toBe(
      "Linked to Bo. This meeting's voice is no longer part of Ana's saved voice. The speaker model in use cannot remember voices, so no voiceprint was kept.",
    );
    // Nothing moved: the save's own words.
    expect(savedVoiceText(null, "Bo", { ...base, taken_back: 0, moved: false, outcome: "Remembered" })).toBe(
      "Saved Bo. Later meetings will recognise this voice.",
    );
    expect(savedVoiceText("Bo", "Bo", { ...base, taken_back: 1, moved: false, outcome: "Replaced" })).toBe(
      "Saved Bo. Later meetings will recognise this voice.",
    );
  });

  it("gives the agreed tooltip for a voice saved here", () => {
    expect(notThisPersonSavedHint("Ana Diaz")).toBe(
      "Shows this voice as an unnamed speaker in this meeting, stops asking for its name and takes this meeting's voice out of Ana Diaz's saved voice.",
    );
  });
});
