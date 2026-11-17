// The Speakers panel of the meeting detail (T12.19, T9.12, T12.44), moved out of
// MeetingDetail.test.tsx unchanged (T12.55).

import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { POST_STEP_MS, RECOGNISED_ME_COPY, SAMPLE_SPEAKERS, sampleDetail, samplePersons } from "../ipc";
import { ITEM, setup, panel, speakerRow, openNamed } from "../test/meetingDetail";
import { notThisPersonHint } from "./SpeakersPanel";

describe("MeetingDetailView", () => {
  it("offers This is me only in the room, and it and its undo follow through the transcript", async () => {
    const { user, calls } = await setup({ inRoom: true });
    await screen.findByRole("combobox", { name: "Name of Speaker 1" });
    expect(within(panel()).getAllByRole("button", { name: "This is me" })).toHaveLength(4);
    expect(within(panel()).queryByRole("button", { name: "This isn't me" })).toBeNull();

    await user.click(within(speakerRow("Speaker 2")).getByRole("button", { name: "This is me" }));
    expect(calls).toContainEqual(["markMe", ITEM.id, SAMPLE_SPEAKERS.priya]);
    // One Me row: the offer leaves every voice, and the user's name shows among the named.
    await waitFor(() => expect(within(panel()).queryByRole("listitem", { name: "Speaker 2" })).toBeNull());
    await openNamed(user);
    const mine = await within(panel()).findByRole("listitem", { name: "Casey" });
    expect(within(mine).getByRole("button", { name: "This isn't me" })).toBeInTheDocument();
    expect(within(panel()).queryByRole("button", { name: "This is me" })).toBeNull();
    expect(within(mine).queryByRole("textbox")).toBeNull();
    // Nothing saves a voice and no voiceprint call is made.
    expect(calls.some((c) => c[0] === "saveVoice")).toBe(false);
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    const table = screen.getByRole("table", { name: "Transcript" });
    expect(within(table).getAllByText("Casey").length).toBeGreaterThan(0);

    await user.click(within(mine).getByRole("button", { name: "This isn't me" }));
    expect(calls).toContainEqual(["unmarkMe", ITEM.id, SAMPLE_SPEAKERS.priya]);
    const back = await within(panel()).findByRole("listitem", { name: "Speaker 2" });
    expect(within(back).getByRole("combobox", { name: "Name of Speaker 2" })).toBeInTheDocument();
    expect(within(panel()).getAllByRole("button", { name: "This is me" })).toHaveLength(4);
    expect(within(table).queryByText("Casey")).toBeNull();
  });

  it("offers neither on a call, and the mock refuses like the runtime", async () => {
    const { ipc } = await setup();
    await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    expect(within(panel()).queryByRole("button", { name: "This is me" })).toBeNull();
    expect(within(panel()).queryByRole("button", { name: "This isn't me" })).toBeNull();
    await expect(ipc.markMe(ITEM.id, SAMPLE_SPEAKERS.second)).rejects.toMatchObject({
      error: { kind: "not_in_room" },
    });
    await expect(ipc.unmarkMe(ITEM.id, SAMPLE_SPEAKERS.me)).rejects.toMatchObject({
      error: { kind: "not_in_room" },
    });
  });

  it("refuses a second voice in the mock with the runtime's copy", async () => {
    const { ipc } = await setup({ inRoom: true });
    await ipc.markMe(ITEM.id, SAMPLE_SPEAKERS.me);
    await expect(ipc.markMe(ITEM.id, SAMPLE_SPEAKERS.second)).rejects.toMatchObject({
      error: {
        kind: "meeting_has_me",
        message: 'One voice in this meeting is already you. Choose "This isn\'t me" on it first.',
      },
    });
    await expect(ipc.unmarkMe(ITEM.id, SAMPLE_SPEAKERS.second)).rejects.toMatchObject({
      error: { kind: "not_me" },
    });
  });

  it("says which person a typed name links to before Save", async () => {
    const { user } = await setup();
    const field = await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await user.type(field, "  priya SHAH ");
    const hint = within(speakerRow("Speaker 2")).getByText("Links to Priya Shah (4 meetings)");
    expect(field).toHaveAttribute("aria-describedby", hint.id);
    // A new name links to nobody yet: it makes a new person.
    await user.clear(field);
    await user.type(field, "Ana Diaz");
    expect(within(speakerRow("Speaker 2")).queryByText(/^Links to/)).toBeNull();
    const fresh = within(speakerRow("Speaker 2")).getByText("New person: Ana Diaz");
    expect(field).toHaveAttribute("aria-describedby", fresh.id);
    // The user's own name links to nobody either, and says why (only the user's row is them).
    await user.clear(field);
    await user.type(field, "casey");
    expect(within(speakerRow("Speaker 2")).queryByText(/^Links to/)).toBeNull();
    const own = within(speakerRow("Speaker 2")).getByText(
      "This is your name. Save makes a separate person called Casey, because only your own row can be you.",
    );
    expect(field).toHaveAttribute("aria-describedby", own.id);
  });

  it("points to This is me when the user's own name is typed in the room", async () => {
    const { user } = await setup({ inRoom: true });
    const field = await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await user.type(field, "Casey");
    expect(
      within(speakerRow("Speaker 2")).getByText(
        'This is your name. Save makes a separate person called Casey. To show this voice as you, choose "This is me".',
      ),
    ).toBeInTheDocument();
  });

  const TWIN = { id: "9e000000-0000-7000-8000-000000000009", name: "casey", has_voiceprint: true, is_me: false, meetings: 1 };

  it("offers to merge a person named after the user after This is me", async () => {
    const { user, calls } = await setup({ inRoom: true, persons: [...samplePersons(), TWIN] });
    await screen.findByRole("combobox", { name: "Name of Speaker 1" });
    expect(screen.queryByRole("button", { name: "Merge into me" })).toBeNull();
    await user.click(within(speakerRow("Speaker 2")).getByRole("button", { name: "This is me" }));
    const merge = await within(panel()).findByRole("button", { name: "Merge into me" });
    expect(
      within(panel()).getByText(
        "There is also a person called casey. If that is you, merge them into you. Earshot will then recognise you by the voice saved for them.",
      ),
    ).toBeInTheDocument();
    await user.click(merge);
    expect(calls).toContainEqual(["mergeIntoMe", TWIN.id]);
    await screen.findByText("casey is now merged into you.");
    expect(within(panel()).queryByRole("button", { name: "Merge into me" })).toBeNull();
  });

  it("keeps a person named after the user separate when asked", async () => {
    const { user, calls, ipc } = await setup({ inRoom: true, persons: [...samplePersons(), TWIN] });
    await screen.findByRole("combobox", { name: "Name of Speaker 1" });
    await user.click(within(speakerRow("Speaker 2")).getByRole("button", { name: "This is me" }));
    await user.click(await within(panel()).findByRole("button", { name: "Keep separate" }));
    expect(within(panel()).queryByRole("button", { name: "Merge into me" })).toBeNull();
    expect(calls.some((c) => c[0] === "mergeIntoMe")).toBe(false);
    expect((await ipc.listPersons()).some((p) => p.id === TWIN.id)).toBe(true);
    // The mock refuses anyone else, as the runtime does.
    await expect(ipc.mergeIntoMe(samplePersons()[1].id)).rejects.toMatchObject({
      error: { kind: "not_named_after_user" },
    });
  });

  it("refuses This isn't me on a voice the user's voiceprint named, and says how to change it", async () => {
    const { user, calls, ipc } = await setup({ inRoom: "recognised" });
    await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await openNamed(user);
    const mine = within(panel()).getByRole("listitem", { name: "Casey" });
    const button = within(mine).getByRole("button", { name: "This isn't me" });
    expect(button).toHaveAttribute("aria-disabled", "true");
    await user.hover(button);
    expect(await screen.findByText(RECOGNISED_ME_COPY)).toBeInTheDocument();
    expect(RECOGNISED_ME_COPY).toContain("Settings, People");
    await user.click(button);
    expect(calls.some((c) => c[0] === "unmarkMe")).toBe(false);
    await expect(ipc.unmarkMe(ITEM.id, SAMPLE_SPEAKERS.me)).rejects.toMatchObject({
      error: { kind: "recognised_me", message: RECOGNISED_ME_COPY },
    });
  });

  it("saves a voice from its name field: named, linked and remembered in one step, and the transcript follows", async () => {
    const { user, calls } = await setup();
    const field = await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await user.type(field, "Sam Lee{Enter}");
    await waitFor(() => expect(within(panel()).queryByRole("combobox", { name: "Name of Speaker 2" })).toBeNull());
    expect(calls).toContainEqual(["saveVoice", ITEM.id, SAMPLE_SPEAKERS.second, { kind: "New", name: "Sam Lee" }]);
    expect(calls.filter((c) => c[0] === "saveVoice")).toHaveLength(1);
    expect(calls.some((c) => c[0] === "linkSpeaker")).toBe(false);
    expect(await screen.findByText("Saved Sam Lee. Later meetings will recognise this voice.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Speakers, 1 needs a name" })).toBeInTheDocument();
    // It moved to the named voices, where its voice can be forgotten.
    await openNamed(user);
    expect(within(speakerRow("Sam Lee")).getByRole("button", { name: "Forget voice" })).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    const rows = within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row");
    expect(within(rows[0]).getAllByRole("cell")[1].textContent).toBe("Sam Lee");
  });

  it("renames a named speaker on Enter", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    await user.click(within(speakerRow("Priya Shah")).getByRole("button", { name: "Rename here" }));
    expect(within(panel()).getByText("Renames this speaker in this meeting only.")).toBeInTheDocument();
    const field = within(panel()).getByRole("textbox", { name: "Name of Priya Shah in this meeting" });
    await user.clear(field);
    await user.type(field, "Priya S{Enter}");
    expect(await within(panel()).findByText("Priya S")).toBeInTheDocument();
    expect(calls).toContainEqual(["renameSpeaker", ITEM.id, SAMPLE_SPEAKERS.priya, "Priya S"]);
  });

  it("merges a speaker away from the panel", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(within(speakerRow("Speaker 3")).getByRole("button", { name: "Merge" }));
    await user.click(await screen.findByRole("menuitem", { name: "Into Priya Shah" }));
    await waitFor(() => expect(within(panel()).queryByRole("listitem", { name: "Speaker 3" })).toBeNull());
    expect(calls).toContainEqual(["mergeSpeakers", ITEM.id, SAMPLE_SPEAKERS.priya, SAMPLE_SPEAKERS.third]);
    await openNamed(user);
    expect(within(speakerRow("Priya Shah")).getByText(/29% of talk time/)).toBeInTheDocument();
    // The user's own row cannot be merged away.
    expect(within(speakerRow("Me")).getByRole("button", { name: "Merge" })).toHaveAttribute("aria-disabled", "true");
  });

  it("links a wrong match to a new person from the named voices", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    await user.click(within(speakerRow("Priya Shah")).getByRole("button", { name: "Link to person" }));
    const dialog = await screen.findByRole("dialog", { name: "Link Priya Shah to a person" });
    await user.type(within(dialog).getByRole("combobox", { name: "Person" }), "Ana Diaz");
    await user.click(await screen.findByRole("option", { name: "New person: Ana Diaz" }));
    await user.click(within(dialog).getByRole("button", { name: "Link" }));
    await waitFor(() => expect(within(panel()).getByRole("listitem", { name: "Ana Diaz" })).toBeInTheDocument());
    expect(calls).toContainEqual(["linkSpeaker", ITEM.id, SAMPLE_SPEAKERS.priya, { kind: "New", name: "Ana Diaz" }]);
    // Linking is not saving: no voice is remembered by it.
    expect(calls.some((c) => c[0] === "saveVoice")).toBe(false);
  });

  it("saves with the Save button, to an existing person by exact name, and never an empty name", async () => {
    const { user, calls } = await setup();
    const save = await screen.findByRole("button", { name: "Save Speaker 3" });
    expect(save).toBeDisabled();
    await user.type(within(panel()).getByRole("combobox", { name: "Name of Speaker 3" }), "  ");
    expect(save).toBeDisabled();
    await user.type(within(panel()).getByRole("combobox", { name: "Name of Speaker 3" }), "tom reid");
    await user.click(save);
    await waitFor(() =>
      expect(calls).toContainEqual([
        "saveVoice",
        ITEM.id,
        SAMPLE_SPEAKERS.third,
        { kind: "Existing", person: "9e000000-0000-7000-8000-000000000003" },
      ]),
    );
    expect(await screen.findByText("Saved Tom Reid. Later meetings will recognise this voice.")).toBeInTheDocument();
  });

  it("ignores a voice: it stops needing a name, keeps its label and can be named after all", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(within(speakerRow("Speaker 3")).getByRole("button", { name: "Ignore" }));
    expect(calls).toContainEqual(["ignoreSpeaker", ITEM.id, SAMPLE_SPEAKERS.third]);
    expect(await screen.findByRole("button", { name: "Speakers, 1 needs a name" })).toBeInTheDocument();
    const needing = within(panel()).getByRole("region", { name: "Need a name" });
    expect(within(needing).queryByRole("listitem", { name: "Speaker 3" })).toBeNull();
    // Nothing is saved or linked by ignoring.
    expect(calls.some((c) => c[0] === "saveVoice" || c[0] === "linkSpeaker")).toBe(false);
    // The label stays in the summary and the transcript.
    expect(within(screen.getByRole("table", { name: "Actions" })).getByText("Speaker 3")).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    const table = screen.getByRole("table", { name: "Transcript" });
    expect(within(table).getAllByText("Speaker 3").length).toBeGreaterThan(0);

    await user.click(within(panel()).getByRole("button", { name: "Ignored (1)" }));
    await user.click(within(speakerRow("Speaker 3")).getByRole("button", { name: "Stop ignoring" }));
    expect(calls).toContainEqual(["unignoreSpeaker", ITEM.id, SAMPLE_SPEAKERS.third]);
    expect(await screen.findByRole("button", { name: "Speakers, 2 need names" })).toBeInTheDocument();
    expect(within(panel()).queryByRole("button", { name: /^Ignored/ })).toBeNull();
    expect(within(panel()).getByRole("combobox", { name: "Name of Speaker 3" })).toBeInTheDocument();
  });

  it("puts a wrongly named voice back as an ignored speaker with Not <name>, and never on the user", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    expect(within(speakerRow("Me")).queryByRole("button", { name: /^Not / })).toBeNull();
    const not = within(speakerRow("Priya Shah")).getByRole("button", { name: "Not Priya Shah" });
    expect(not).toHaveAccessibleDescription(
      "Shows this voice as an unnamed speaker in this meeting and stops asking for its name. Later meetings may still name it Priya Shah, because Priya Shah's saved voice may hold it. Forget voice deletes Priya Shah's saved voice.",
    );
    // Forget voice is named only where the row shows it (a saved voice).
    expect(within(speakerRow("Priya Shah")).getByRole("button", { name: "Forget voice" })).toBeInTheDocument();
    expect(notThisPersonHint("Ana Diaz", false)).toBe(
      "Shows this voice as an unnamed speaker in this meeting and stops asking for its name. Later meetings may still name it Ana Diaz, because Ana Diaz's saved voice may hold it.",
    );
    await user.click(not);
    expect(calls).toContainEqual(["notThisPerson", ITEM.id, SAMPLE_SPEAKERS.priya]);
    // The row leaves the named voices for the ignored ones, under the first free label.
    await waitFor(() => expect(within(panel()).queryByRole("listitem", { name: "Priya Shah" })).toBeNull());
    await user.click(within(panel()).getByRole("button", { name: "Ignored (1)" }));
    const back = within(panel()).getByRole("list", { name: "Ignored" });
    expect(within(back).getByRole("listitem", { name: "Speaker 1" })).toBeInTheDocument();
    // It does not ask for a name, and nothing is linked, saved or forgotten.
    expect(within(panel()).queryByRole("combobox", { name: "Name of Speaker 1" })).toBeNull();
    expect(
      calls.some((c) => ["saveVoice", "linkSpeaker", "forgetPersonVoices", "removePerson"].includes(c[0] as string)),
    ).toBe(false);
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    const table = screen.getByRole("table", { name: "Transcript" });
    expect(within(table).queryByText("Priya Shah")).toBeNull();
    expect(within(table).getAllByText("Speaker 1").length).toBeGreaterThan(0);
  });

  it("the mock refuses Not <name> like the runtime", async () => {
    const { ipc } = await setup();
    await screen.findByText(/TL;DR/);
    await expect(ipc.notThisPerson(ITEM.id, SAMPLE_SPEAKERS.me)).rejects.toMatchObject({
      error: { kind: "user_row" },
    });
    await expect(ipc.notThisPerson(ITEM.id, SAMPLE_SPEAKERS.second)).rejects.toMatchObject({
      error: { kind: "no_person" },
    });
  });

  it("offers no Not <name> on a voice linked to a person merged into the user, and the mock refuses it", async () => {
    const { ipc } = await setup({ persons: [...samplePersons(), TWIN] });
    await screen.findByText(/TL;DR/);
    await ipc.linkSpeaker(ITEM.id, SAMPLE_SPEAKERS.second, { kind: "Existing", person: TWIN.id });
    const card = async () =>
      (await ipc.getMeeting(ITEM.id)).speakers.find((s) => s.id === SAMPLE_SPEAKERS.second);
    expect((await card())?.can_not_this_person).toBe(true);
    await ipc.mergeIntoMe(TWIN.id);
    expect((await card())?.can_not_this_person).toBe(false);
    await expect(ipc.notThisPerson(ITEM.id, SAMPLE_SPEAKERS.second)).rejects.toMatchObject({
      error: {
        kind: "user_row",
        message: "This voice is you. Only a voice named as someone else can be put back as an unnamed speaker.",
      },
    });
  });

  it("offers no Ignore for a voice with nothing stored, and the mock refuses like the runtime", async () => {
    const base = sampleDetail(ITEM);
    const detail = {
      ...base,
      speakers: base.speakers.map((s) => (s.id === SAMPLE_SPEAKERS.third ? { ...s, can_ignore: false } : s)),
    };
    const { ipc } = await setup({ details: { [ITEM.id]: detail } });
    await screen.findByText(/TL;DR/);
    expect(within(speakerRow("Speaker 3")).queryByRole("button", { name: "Ignore" })).toBeNull();
    expect(within(speakerRow("Speaker 2")).getByRole("button", { name: "Ignore" })).toBeInTheDocument();
    await expect(ipc.ignoreSpeaker(ITEM.id, SAMPLE_SPEAKERS.third)).rejects.toMatchObject({
      error: { kind: "no_stored_voice" },
    });
    await expect(ipc.ignoreSpeaker(ITEM.id, SAMPLE_SPEAKERS.priya)).rejects.toMatchObject({
      error: { kind: "not_unnamed", message: "Only a voice that needs a name can be ignored." },
    });
    await expect(ipc.saveVoice(ITEM.id, SAMPLE_SPEAKERS.me, { kind: "New", name: "Sky Moss" })).rejects.toMatchObject({
      error: { kind: "me_is_the_user" },
    });
    expect((await ipc.listPersons()).some((p) => p.name === "Sky Moss")).toBe(false);
  });

  it("stays closed when no voice needs a name, with recognised voices marked, and the header button toggles it", async () => {
    const base = sampleDetail(ITEM);
    const detail = {
      ...base,
      speakers: base.speakers.map((s) =>
        s.needs_name
          ? {
              ...s,
              name: "Tom Reid",
              person: { id: "9e000000-0000-7000-8000-000000000003", name: "Tom Reid", has_voiceprint: true },
              source: "Voiceprint" as const,
              needs_name: false,
              can_ignore: false,
            }
          : s,
      ),
    };
    const { user } = await setup({ details: { [ITEM.id]: detail } });
    await screen.findByText(/TL;DR/);
    const toggle = screen.getByRole("button", { name: "Speakers" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("complementary", { name: "Speakers" })).toBeNull();

    await user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(panel()).getByText("Every voice in this meeting has a name.")).toBeInTheDocument();
    await openNamed(user);
    const recognisedRows = within(panel())
      .getAllByRole("listitem")
      .filter((r) => within(r).queryByText("Recognised") !== null);
    expect(recognisedRows.map((r) => r.getAttribute("aria-label"))).toEqual(["Priya Shah", "Tom Reid", "Tom Reid"]);
    expect(within(speakerRow("Me")).queryByText("Recognised")).toBeNull();

    await user.click(within(panel()).getByRole("button", { name: "Close Speakers" }));
    expect(screen.queryByRole("complementary", { name: "Speakers" })).toBeNull();
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    await user.click(toggle);
    expect(panel()).toBeInTheDocument();
    await user.click(toggle);
    expect(screen.queryByRole("complementary", { name: "Speakers" })).toBeNull();
  });

  it("slides the panel out while its column closes, hidden and inert, then removes it", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    const toggle = screen.getByRole("button", { name: /^Speakers/ });
    if (toggle.getAttribute("aria-expanded") !== "true") await user.click(toggle);
    const open = panel();
    await user.click(toggle);
    // Still drawn for the slide out, but no longer reachable.
    expect(open).toBeInTheDocument();
    expect(screen.queryByRole("complementary", { name: "Speakers" })).toBeNull();
    const leaving = open.closest('[aria-hidden="true"]');
    expect(leaving).not.toBeNull();
    expect(leaving).toHaveAttribute("inert");
    // The column's own transition end removes it; a child's transition does not.
    const column = leaving?.parentElement as HTMLElement;
    fireEvent.transitionEnd(open);
    expect(open).toBeInTheDocument();
    fireEvent.transitionEnd(column);
    expect(open).not.toBeInTheDocument();
    // Opening again draws a reachable panel.
    await user.click(toggle);
    expect(panel()).toBeInTheDocument();
  });

  it("opens by itself when a reload brings more voices that need a name, and never closes by itself", async () => {
    const base = sampleDetail(ITEM);
    const allNamed = {
      ...base,
      speakers: base.speakers.map((s) =>
        s.needs_name
          ? {
              ...s,
              name: "Tom Reid",
              person: { id: "9e000000-0000-7000-8000-000000000003", name: "Tom Reid", has_voiceprint: true },
              source: "Voiceprint" as const,
              needs_name: false,
              can_ignore: false,
            }
          : s,
      ),
    };
    const { user, ipc, timer } = await setup({ details: { [ITEM.id]: allNamed } });
    await screen.findByText(/TL;DR/);
    const closed = () => screen.queryByRole("complementary", { name: "Speakers" });
    expect(closed()).toBeNull();
    // A reload that brings no new voice to name leaves it closed.
    await user.click(screen.getByRole("button", { name: "Rename" }));
    await user.type(screen.getByRole("textbox", { name: "Meeting title" }), " again{Enter}");
    await screen.findByRole("heading", { level: 1, name: `${ITEM.title} again` });
    expect(closed()).toBeNull();

    // A re-transcription ends with two voices that need names: the panel opens by itself.
    ipc.getMeeting = async () => base;
    await user.click(screen.getByRole("button", { name: "Re-transcribe" }));
    for (let i = 0; i < 12; i++) act(() => timer.advance(POST_STEP_MS));
    expect(await screen.findByRole("complementary", { name: "Speakers" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Speakers, 2 need names" })).toHaveAttribute("aria-expanded", "true");

    // Closed by the user, it stays closed on a reload with no more voices to name than before.
    await user.click(within(panel()).getByRole("button", { name: "Close Speakers" }));
    await user.click(screen.getByRole("button", { name: "Rename" }));
    await user.type(screen.getByRole("textbox", { name: "Meeting title" }), "{Enter}");
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "Meeting title" })).toBeNull());
    expect(closed()).toBeNull();
  });

  it("returns focus to the Speakers button when the panel closes, and points at the panel only while it exists", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    const toggle = screen.getByRole("button", { name: "Speakers, 2 need names" });
    expect(toggle.getAttribute("aria-controls")).toBe(panel().id);
    within(panel()).getByRole("button", { name: "Close Speakers" }).focus();
    await user.keyboard("{Enter}");
    expect(screen.queryByRole("complementary", { name: "Speakers" })).toBeNull();
    expect(toggle).toHaveFocus();
    expect(toggle).not.toHaveAttribute("aria-controls");
    await user.keyboard("{Enter}");
    expect(toggle.getAttribute("aria-controls")).toBe(panel().id);
  });

  it("puts the panel beside the summary and transcript, pushing them aside rather than covering them", async () => {
    await setup();
    await screen.findByText(/TL;DR/);
    const aside = panel();
    const main = screen.getByText(/TL;DR/).closest("div");
    if (main === null) throw new Error("no summary");
    // Both live in one grid row: the panel's column is beside the content's, in normal flow.
    let column: HTMLElement | null = aside;
    while (column !== null && column.parentElement !== null && !column.parentElement.contains(main)) {
      column = column.parentElement;
    }
    if (column === null || column.parentElement === null) throw new Error("no shared container");
    expect(getComputedStyle(column.parentElement).display).toBe("grid");
    for (let el: HTMLElement | null = aside; el !== null && el !== column.parentElement; el = el.parentElement) {
      expect(["absolute", "fixed"]).not.toContain(getComputedStyle(el).position);
    }
  });
});

describe("Speakers panel person field (T12.80)", () => {
  // Two saved people with one name: the cause of T12.80's missing "Links to" (the hint needed
  // exactly one person with the typed name and said nothing for two).
  const SECOND_PRIYA = {
    id: "9e000000-0000-7000-8000-000000000010",
    name: "Priya  shah",
    has_voiceprint: false,
    is_me: false,
    meetings: 1,
  };
  const ZOE = { id: "9e000000-0000-7000-8000-000000000011", name: "Zoë Brontë", has_voiceprint: true, is_me: false, meetings: 2 };

  it("tells two people with one whole name apart instead of saying nothing", async () => {
    const { user, calls } = await setup({ persons: [...samplePersons(), SECOND_PRIYA] });
    const field = await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await user.type(field, "Priya Shah");
    const hint = within(speakerRow("Speaker 2")).getByText("2 people are called Priya Shah. Choose one from the list.");
    expect(field).toHaveAttribute("aria-describedby", hint.id);
    // Save waits for a choice: it would otherwise guess, or make a third Priya Shah.
    expect(within(speakerRow("Speaker 2")).getByRole("button", { name: "Save Speaker 2" })).toBeDisabled();
    await user.keyboard("{Enter}");
    expect(calls.some((c) => c[0] === "saveVoice")).toBe(false);
    // Both are listed, each with how many meetings they are in.
    await user.click(field);
    expect(screen.getByRole("option", { name: "Priya Shah (4 meetings)" })).toBeInTheDocument();
    await user.click(screen.getByRole("option", { name: "Priya shah (1 meeting)" }));
    expect(within(speakerRow("Speaker 2")).getByText("Links to Priya shah (1 meeting)")).toBeInTheDocument();
    await user.click(within(speakerRow("Speaker 2")).getByRole("button", { name: "Save Speaker 2" }));
    await waitFor(() =>
      expect(calls).toContainEqual([
        "saveVoice",
        ITEM.id,
        SAMPLE_SPEAKERS.second,
        { kind: "Existing", person: SECOND_PRIYA.id },
      ]),
    );
  });

  it("filters saved people by any part of the name, case and accents ignored", async () => {
    const { user } = await setup({ persons: [...samplePersons(), ZOE] });
    const field = await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await user.click(field);
    // Everyone before typing, the user disabled.
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual([
      "Casey (you)",
      "Priya Shah (4 meetings)",
      "Tom Reid (2 meetings)",
      "Zoë Brontë (2 meetings)",
    ]);
    expect(screen.getByRole("option", { name: "Casey (you)" })).toHaveAttribute("aria-disabled", "true");
    await user.type(field, "BRONTE");
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual([
      "New person: BRONTE",
      "Zoë Brontë (2 meetings)",
    ]);
    await user.clear(field);
    await user.type(field, "eid");
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual([
      "New person: eid",
      "Tom Reid (2 meetings)",
    ]);
    // An accented whole name links without typing the accents.
    await user.clear(field);
    await user.type(field, "zoe bronte");
    expect(within(speakerRow("Speaker 2")).getByText("Links to Zoë Brontë (2 meetings)")).toBeInTheDocument();
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual([
      "Zoë Brontë (2 meetings)",
      "New person: zoe bronte",
    ]);
  });

  it("chooses a person from the list with the keyboard and saves on Enter", async () => {
    const { user, calls } = await setup();
    const field = await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await user.type(field, "r");
    // "New person: r" first, then Priya Shah and Tom Reid.
    await user.keyboard("{ArrowDown}{ArrowDown}{Enter}");
    await waitFor(() =>
      expect(calls).toContainEqual([
        "saveVoice",
        ITEM.id,
        SAMPLE_SPEAKERS.second,
        { kind: "Existing", person: "9e000000-0000-7000-8000-000000000003" },
      ]),
    );
    expect(await screen.findByText("Saved Tom Reid. Later meetings will recognise this voice.")).toBeInTheDocument();
  });

  it("makes a new person from a name no one has on Enter, and from its option", async () => {
    const { user, calls } = await setup();
    const field = await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await user.type(field, "Ana");
    expect(screen.getByRole("option", { name: "New person: Ana" })).toBeInTheDocument();
    await user.keyboard("{Enter}");
    await waitFor(() =>
      expect(calls).toContainEqual(["saveVoice", ITEM.id, SAMPLE_SPEAKERS.second, { kind: "New", name: "Ana" }]),
    );
    // A new person with a name someone has, chosen on purpose.
    const third = await screen.findByRole("combobox", { name: "Name of Speaker 3" });
    await user.type(third, "Tom Reid");
    await user.click(screen.getByRole("option", { name: "New person: Tom Reid" }));
    expect(within(speakerRow("Speaker 3")).getByText("New person: Tom Reid")).toBeInTheDocument();
    await user.click(within(speakerRow("Speaker 3")).getByRole("button", { name: "Save Speaker 3" }));
    await waitFor(() =>
      expect(calls).toContainEqual(["saveVoice", ITEM.id, SAMPLE_SPEAKERS.third, { kind: "New", name: "Tom Reid" }]),
    );
  });

  it("never links a voice to the user from the list", async () => {
    const { user, calls } = await setup();
    const field = await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await user.type(field, "case");
    await user.click(screen.getByRole("option", { name: "Casey (you)" }));
    expect(field).toHaveValue("case");
    await user.clear(field);
    await user.type(field, "Casey");
    expect(
      within(speakerRow("Speaker 2")).getByText(
        "This is your name. Save makes a separate person called Casey, because only your own row can be you.",
      ),
    ).toBeInTheDocument();
    await user.click(within(speakerRow("Speaker 2")).getByRole("button", { name: "Save Speaker 2" }));
    await waitFor(() =>
      expect(calls).toContainEqual(["saveVoice", ITEM.id, SAMPLE_SPEAKERS.second, { kind: "New", name: "Casey" }]),
    );
  });

  it("links a named voice to a saved person through the same field", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    await user.click(within(speakerRow("Priya Shah")).getByRole("button", { name: "Link to person" }));
    const dialog = await screen.findByRole("dialog", { name: "Link Priya Shah to a person" });
    await user.type(within(dialog).getByRole("combobox", { name: "Person" }), "tom");
    await user.click(screen.getByRole("option", { name: "Tom Reid (2 meetings)" }));
    await user.click(within(dialog).getByRole("button", { name: "Link" }));
    await waitFor(() =>
      expect(calls).toContainEqual([
        "linkSpeaker",
        ITEM.id,
        SAMPLE_SPEAKERS.priya,
        { kind: "Existing", person: "9e000000-0000-7000-8000-000000000003" },
      ]),
    );
    expect(calls.some((c) => c[0] === "saveVoice")).toBe(false);
  });
});