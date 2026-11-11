import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { VOICE_SAMPLE_MS, VOICE_STEP_MS, voiceMessage } from "../ipc/mockSettings";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import { FORGET_PERSON_EXPLAINED, REMOVE_PERSON_EXPLAINED, PEOPLE_EXPLAINED } from "./PeoplePage";
import { FORGET_EXPLAINED, NAME_EXPLAINED, READ_ALOUD, RECORD_CONSENT, VOICE_EXPLAINED } from "./PeoplePage";
import { SpeakersNoticeBar, VOICE_NOTICES } from "./PeoplePage";

/** The UI's sources (tests left out), for the notice-bar scan. */
const SOURCES = import.meta.glob(["../**/*.tsx", "!../**/*.test.tsx", "!../test/**"], {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;
import type { SpeakersNotice, SpeakersPageState } from "./useSpeakers";

interface Setup {
  ipc: Ipc;
  timer: ManualTimer;
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
  await user.click(within(nav).getByRole("button", { name: "People" }));
  await screen.findByRole("heading", { level: 1, name: "People" });
  await screen.findByRole("textbox", { name: /Your name/ });
  return { ipc, timer, user };
}

function advance(timer: ManualTimer, ms: number) {
  act(() => timer.advance(ms));
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, People, people", () => {
  const people = [
    { id: "me", name: "The User", has_voiceprint: true, is_me: true },
    { id: "a", name: "Alex Sample", has_voiceprint: true, is_me: false },
    { id: "b", name: "Bo Example", has_voiceprint: false, is_me: false },
  ];

  it("lists everyone but the user, saying whose voice is saved", async () => {
    await open({ persons: people });
    await screen.findByText("Alex Sample");
    expect(screen.getByText(PEOPLE_EXPLAINED)).toBeTruthy();
    expect(screen.getByText("Bo Example")).toBeTruthy();
    expect(screen.queryByText("The User")).toBeNull();
    expect(screen.getByText("Voice saved")).toBeTruthy();
    expect(screen.getByText("No saved voice")).toBeTruthy();
    // Nothing to forget without a voice.
    expect(screen.getByRole("button", { name: "Forget the voice of Alex Sample" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Forget the voice of Bo Example" })).toBeNull();
  });

  it("says so when nobody is known yet", async () => {
    await open({ persons: [people[0]] });
    await screen.findByText(/Nobody yet/);
  });

  it("renames someone in the stored list", async () => {
    const s = await open({ persons: people });
    await s.user.click(await screen.findByRole("button", { name: "Rename Bo Example" }));
    const field = screen.getByRole("textbox", { name: "New name for Bo Example" });
    await s.user.clear(field);
    expect((within(screen.getByRole("region", { name: "People" })).getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(true);
    await s.user.type(field, "Bo Renamed");
    await s.user.click(within(screen.getByRole("region", { name: "People" })).getByRole("button", { name: "Save" }));
    await screen.findByText("Bo Renamed");
    expect(screen.queryByText("Bo Example")).toBeNull();
    expect((await s.ipc.listPersons()).find((p) => p.id === "b")?.name).toBe("Bo Renamed");
  });

  it("keeps the name being typed when a rename is refused", async () => {
    const s = await open({ persons: people });
    s.ipc.renamePerson = async () => {
      throw new Error("refused");
    };
    await s.user.click(await screen.findByRole("button", { name: "Rename Bo Example" }));
    await s.user.type(screen.getByRole("textbox", { name: "New name for Bo Example" }), "!");
    await s.user.click(within(screen.getByRole("region", { name: "People" })).getByRole("button", { name: "Save" }));
    await screen.findByText("The name was not saved. Try another name.");
    expect((screen.getByRole("textbox", { name: "New name for Bo Example" }) as HTMLInputElement).value).toBe(
      "Bo Example!",
    );
  });

  it("asks before forgetting a voice, and Cancel changes nothing", async () => {
    const s = await open({ persons: people });
    await s.user.click(await screen.findByRole("button", { name: "Forget the voice of Alex Sample" }));
    expect(await screen.findByText(FORGET_PERSON_EXPLAINED)).toBeTruthy();
    await s.user.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByText(FORGET_PERSON_EXPLAINED)).toBeNull());
    expect((await s.ipc.listPersons()).find((p) => p.id === "a")?.has_voiceprint).toBe(true);

    await s.user.click(await screen.findByRole("button", { name: "Forget the voice of Alex Sample" }));
    const dialog = await screen.findByRole("alertdialog");
    await s.user.click(within(dialog).getByRole("button", { name: "Forget voice" }));
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "Forget the voice of Alex Sample" })).toBeNull(),
    );
    expect((await s.ipc.listPersons()).find((p) => p.id === "a")?.has_voiceprint).toBe(false);
  });

  it("asks before removing a person, and Cancel changes nothing", async () => {
    const s = await open({ persons: people });
    await s.user.click(await screen.findByRole("button", { name: "Remove Bo Example" }));
    expect(await screen.findByText(REMOVE_PERSON_EXPLAINED)).toBeTruthy();
    await s.user.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByText(REMOVE_PERSON_EXPLAINED)).toBeNull());
    expect((await s.ipc.listPersons()).some((p) => p.id === "b")).toBe(true);

    await s.user.click(await screen.findByRole("button", { name: "Remove Bo Example" }));
    const dialog = await screen.findByRole("alertdialog");
    await s.user.click(within(dialog).getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Remove Bo Example" })).toBeNull());
    expect((await s.ipc.listPersons()).some((p) => p.id === "b")).toBe(false);
    expect(screen.getByRole("button", { name: "Remove Alex Sample" })).toBeTruthy();
  });

  it("offers no way to remove you", async () => {
    await open({ persons: [...people, { id: "me", name: "Casey", has_voiceprint: true, is_me: true }] });
    await screen.findByRole("button", { name: "Remove Bo Example" });
    expect(screen.queryByRole("button", { name: "Remove Casey" })).toBeNull();
  });
});

describe("Settings, People", () => {

  it("saves your name with Save, not when you leave the field, and only if it changed", async () => {
    const s = await open();
    const field = screen.getByRole("textbox", { name: /Your name/ });
    const save = () => within(screen.getByRole("region", { name: "Your name" })).getByRole("button", { name: "Save" });
    expect(field).toHaveValue("");
    expect(save()).toBeDisabled();
    await s.user.type(field, "Casey Parker");
    expect((await s.ipc.speakers()).voice.name).toBe("");
    // Typed text is saved only by Save or Enter (the save rule): leaving the field saves nothing.
    await s.user.tab();
    expect((await s.ipc.speakers()).voice.name).toBe("");
    await s.user.click(save());
    await waitFor(async () => expect((await s.ipc.speakers()).voice.name).toBe("Casey Parker"));
    await waitFor(() => expect(save()).toBeDisabled());
    expect(screen.getByRole("textbox", { name: /Your name/ })).toHaveValue("Casey Parker");
    expect(screen.getByText(NAME_EXPLAINED)).toBeInTheDocument();
    // Enter saves too, and clearing it goes back to no name.
    await s.user.clear(field);
    await s.user.keyboard("{Enter}");
    await waitFor(async () => expect((await s.ipc.speakers()).voice.name).toBe(""));
  });

  it("saves the name once when Enter is pressed again while the save is on its way", async () => {
    const s = await open();
    const calls: string[] = [];
    let release = () => {};
    const gate = new Promise<void>((r) => {
      release = r;
    });
    const real = s.ipc.setMyName.bind(s.ipc);
    s.ipc.setMyName = async (name) => {
      calls.push(name);
      await gate;
      return real(name);
    };
    const field = screen.getByRole("textbox", { name: /Your name/ });
    await s.user.type(field, "Casey Parker");
    await s.user.keyboard("{Enter}");
    await s.user.keyboard("{Enter}");
    await s.user.click(within(screen.getByRole("region", { name: "Your name" })).getByRole("button", { name: "Save" }));
    expect(calls).toEqual(["Casey Parker"]);
    release();
    await waitFor(async () => expect((await s.ipc.speakers()).voice.name).toBe("Casey Parker"));
    expect(calls).toEqual(["Casey Parker"]);
  });

  it("shows only a speaker timing notice under the speaker timing", () => {
    const state = (kind: SpeakersNotice["kind"]) =>
      ({
        notice: { kind, title: `Invented ${kind} notice`, message: "Invented reason." },
        dismiss: () => {},
      }) as unknown as SpeakersPageState;
    const voice = render(
      <EarshotThemeProvider>
        <SpeakersNoticeBar state={state("voice")} kinds={["timing"]} />
      </EarshotThemeProvider>,
    );
    expect(within(voice.container).queryByText("Invented voice notice")).toBeNull();
    voice.unmount();
    const timing = render(
      <EarshotThemeProvider>
        <SpeakersNoticeBar state={state("timing")} kinds={["timing"]} />
      </EarshotThemeProvider>,
    );
    expect(within(timing.container).getByText("Invented timing notice")).toBeInTheDocument();
    timing.unmount();
    // The voice bar shows a voice notice and never a timing one, so a notice shows once.
    const underVoice = render(
      <EarshotThemeProvider>
        <SpeakersNoticeBar state={state("timing")} kinds={VOICE_NOTICES} />
      </EarshotThemeProvider>,
    );
    expect(within(underVoice.container).queryByText("Invented timing notice")).toBeNull();
    underVoice.unmount();
    const voiceUnderVoice = render(
      <EarshotThemeProvider>
        <SpeakersNoticeBar state={state("voice")} kinds={VOICE_NOTICES} />
      </EarshotThemeProvider>,
    );
    expect(within(voiceUnderVoice.container).getByText("Invented voice notice")).toBeInTheDocument();
    voiceUnderVoice.unmount();
  });

  it("names the notices every speaker notice bar shows, the voice ones apart from the timing", () => {
    // Each bar sits under one control: a bar that took every kind would show a notice twice.
    const uses = Object.entries(SOURCES).flatMap(([file, text]) =>
      [...text.matchAll(/<SpeakersNoticeBar\b[^>]*>/g)].map((m) => `${file}: ${m[0]}`),
    );
    expect(uses.length).toBeGreaterThanOrEqual(3);
    const voice = uses.filter((u) => u.includes("kinds={VOICE_NOTICES}"));
    const timing = uses.filter((u) => u.includes('kinds={["timing"]}'));
    expect(voice.length + timing.length, uses.join("\n")).toBe(uses.length);
    expect(timing).toHaveLength(1);
    expect(voice.some((u) => u.includes("FirstRunWizard.tsx"))).toBe(true);
    expect(voice.some((u) => u.includes("PeoplePage.tsx"))).toBe(true);
    expect(VOICE_NOTICES).toEqual(["voice"]);
  });

  it("shows the name already given and says when one is refused", async () => {
    const s = await open({ settings: { myName: "Casey" } });
    const field = screen.getByRole("textbox", { name: /Your name/ });
    expect(field).toHaveValue("Casey");
    await s.user.clear(field);
    await s.user.paste("x".repeat(101));
    await s.user.click(within(screen.getByRole("region", { name: "Your name" })).getByRole("button", { name: "Save" }));
    expect(await screen.findByText("Your name was not saved")).toBeInTheDocument();
    expect((await s.ipc.speakers()).voice.name).toBe("Casey");
  });


  it("explains the voiceprint and asks before recording; cancelling the question records nothing", async () => {
    const s = await open();
    expect(screen.getByText(VOICE_EXPLAINED)).toBeInTheDocument();
    expect(VOICE_EXPLAINED).toContain("never leaves it");
    expect(VOICE_EXPLAINED).toContain("The recording itself is never saved");
    expect(screen.getByText("No voice is saved.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Forget my voice" })).toBeNull();
    await s.user.click(screen.getByRole("button", { name: "Record my voice" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Save a voiceprint of your voice?" });
    expect(within(dialog).getByText(RECORD_CONSENT)).toBeInTheDocument();
    await s.user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(screen.queryByRole("group", { name: "Recording your voice" })).toBeNull();
    advance(s.timer, VOICE_SAMPLE_MS * 2);
    expect((await s.ipc.speakers()).voice.saved).toBe(false);
  });

  it("records with progress and the text to read, saves the voice, and forgets it after asking", async () => {
    const s = await open();
    await s.user.click(screen.getByRole("button", { name: "Record my voice" }));
    const dialog = await screen.findByRole("alertdialog");
    await s.user.click(within(dialog).getByRole("button", { name: "Record and save" }));
    const group = await screen.findByRole("group", { name: "Recording your voice" });
    expect(within(group).getByText(READ_ALOUD)).toBeInTheDocument();
    expect(within(group).getByText("Recording: 0 of 30 seconds")).toBeInTheDocument();
    advance(s.timer, 10 * VOICE_STEP_MS);
    expect(within(group).getByText("Recording: 5 of 30 seconds")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Record my voice" })).toBeNull();
    advance(s.timer, VOICE_SAMPLE_MS);
    expect(await screen.findByText(voiceMessage("Saved", 26_000))).toBeInTheDocument();
    expect(screen.getByText("Your voice is saved, from 26 seconds of speech.")).toBeInTheDocument();
    expect((await s.ipc.speakers()).voice.saved).toBe(true);
    expect(screen.getByRole("button", { name: "Add another sample" })).toBeInTheDocument();

    await s.user.click(screen.getByRole("button", { name: "Forget my voice" }));
    const ask = await screen.findByRole("alertdialog", { name: "Forget your voice?" });
    expect(within(ask).getByText(FORGET_EXPLAINED)).toBeInTheDocument();
    await s.user.click(within(ask).getByRole("button", { name: "Forget my voice" }));
    expect(await screen.findByText(/^Your voice was forgotten\./)).toBeInTheDocument();
    expect(screen.getByText("No voice is saved.")).toBeInTheDocument();
    expect((await s.ipc.speakers()).voice.saved).toBe(false);
  });

  it("cancels a recording and saves nothing", async () => {
    const s = await open();
    await s.user.click(screen.getByRole("button", { name: "Record my voice" }));
    await s.user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Record and save" }));
    const group = await screen.findByRole("group", { name: "Recording your voice" });
    advance(s.timer, 4 * VOICE_STEP_MS);
    await s.user.click(within(group).getByRole("button", { name: "Cancel" }));
    expect(await screen.findByText("The recording was cancelled. Nothing was saved.")).toBeInTheDocument();
    advance(s.timer, VOICE_SAMPLE_MS * 2);
    expect((await s.ipc.speakers()).voice.saved).toBe(false);
    expect(screen.getByRole("button", { name: "Record my voice" })).toBeInTheDocument();
  });

  it("shows why a sample was refused, and a start the app refused", async () => {
    const s = await open({ settings: { voiceResult: "SeveralVoices" } });
    await s.user.click(screen.getByRole("button", { name: "Record my voice" }));
    await s.user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Record and save" }));
    await screen.findByRole("group", { name: "Recording your voice" });
    advance(s.timer, VOICE_SAMPLE_MS + VOICE_STEP_MS);
    expect(await screen.findByText(voiceMessage("SeveralVoices", 0))).toBeInTheDocument();
    expect((await s.ipc.speakers()).voice.saved).toBe(false);
    cleanup();

    const busy = "A meeting is running; record your voice when it has finished.";
    const t = await open({ settings: { voiceStartError: { kind: "busy", message: busy, retryable: true } } });
    await t.user.click(screen.getByRole("button", { name: "Record my voice" }));
    await t.user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Record and save" }));
    expect(await screen.findByText("The recording did not start")).toBeInTheDocument();
    expect(screen.getByText(busy)).toBeInTheDocument();
    // The closed dialog's aria-hidden on the page lifts on tabster's deferred pass.
    expect(await screen.findByRole("button", { name: "Record my voice" })).toBeInTheDocument();
  });

  it("starts with a saved voice and offers to add a sample or forget it", async () => {
    await open({ settings: { voiceSpeechMs: 41_500 } });
    expect(screen.getByText("Your voice is saved, from 41 seconds of speech.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add another sample" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Forget my voice" })).toBeInTheDocument();
  });
});
