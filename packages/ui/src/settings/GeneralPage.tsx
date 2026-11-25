// Settings, General (docs/ui-design.md "Settings", T7.4h, T9.1n, T12.5b): start Earshot with
// Windows, closing to the tray, the offer to record a call (T12.86) and stopping by itself when
// one ends (T14.52), the shortcut, the theme (the system's, light or dark), updates
// and the setup guide.

import {
  Body1,
  Button,
  Caption1,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  Radio,
  RadioGroup,
  Subtitle2,
  Switch,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useId, useRef, useState } from "react";

import { errorText, useIpc } from "../ipc";
import { type ThemeChoice, useThemeChoice } from "../theme/EarshotThemeProvider";
import { UpdatesSection } from "./UpdatesSection";
import { NotLoaded, Section, TEXT_WIDTH, UnknownSettings, useLoad, usePageStyles } from "./parts";

const useStyles = makeStyles({
  action: {
    alignSelf: "flex-start",
  },
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  heading: {
    margin: 0,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
    maxWidth: TEXT_WIDTH,
  },
  text: {
    maxWidth: TEXT_WIDTH,
  },
});

/** What "start with Windows" does. */
export const START_EXPLAINED =
  "Earshot starts in the tray when you sign in to Windows, so it is ready for your next meeting. It does not start a recording.";

/** What closing the window does, and how to quit. */
export const TRAY_EXPLAINED =
  "Closing the window keeps Earshot running in the tray. To quit, use Quit Earshot in the tray menu.";

/** What the global shortcut does. */
export const shortcutExplained = (shortcut: string) =>
  `Press ${shortcut} anywhere in Windows to start a meeting, and again to stop it. A meeting started this way is a call or in the room, as chosen on the Audio page.`;

/** What the setup guide is, on the General page. */
export const SETUP_EXPLAINED =
  "The steps Earshot showed the first time: summaries, the downloads, your name and voice, Obsidian and the audio check. Nothing changes until you choose something.";

/** What "System" does. */
export const THEME_SYSTEM_EXPLAINED = "System follows the light or dark setting of Windows.";

function ThemeSection() {
  const styles = useStyles();
  const { choice, setChoice } = useThemeChoice();
  const id = useId();
  return (
    <section className={styles.section} aria-labelledby={id}>
      <Subtitle2 as="h2" id={id} className={styles.heading}>
        Theme
      </Subtitle2>
      <RadioGroup
        layout="horizontal"
        aria-labelledby={id}
        value={choice}
        onChange={(_, d) => setChoice(d.value as ThemeChoice)}
      >
        <Radio value="system" label="System" />
        <Radio value="light" label="Light" />
        <Radio value="dark" label="Dark" />
      </RadioGroup>
      <Caption1 className={styles.muted}>{THEME_SYSTEM_EXPLAINED}</Caption1>
    </section>
  );
}

/** What "Offer to record when a call starts" does (T12.86). */
export const CALL_START_EXPLAINED =
  "When Teams, Zoom or another call app starts a call, Earshot asks whether to record it. Nothing records until you say so.";

/** The offer to record a call: read on its own, applied at once, shown as stored. */
function CallStartSection() {
  const styles = useStyles();
  const ipc = useIpc();
  const [load, setLoad] = useLoad((ipc) => ipc.callStartOffer());
  // Not disabled while a save is in flight, which would take focus off the switch in WebView2:
  // a press meanwhile is ignored.
  const busy = useRef(false);
  const [error, setError] = useState<string | null>(null);

  if (load.status !== "loaded") {
    return (
      <NotLoaded
        load={load}
        size="small"
        loading="Reading the call setting"
        failed="The call setting could not be read"
      />
    );
  }
  const set = async (on: boolean) => {
    if (busy.current) return;
    busy.current = true;
    try {
      setLoad({ status: "loaded", view: await ipc.setCallStartOffer(on) });
      setError(null);
    } catch (e) {
      setError(errorText(e));
    } finally {
      busy.current = false;
    }
  };
  return (
    <>
      <Switch
        label="Offer to record when a call starts"
        checked={load.view}
        onChange={(_, d) => void set(d.checked)}
      />
      <Caption1 className={styles.muted}>{CALL_START_EXPLAINED}</Caption1>
      {error !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>This setting was not changed</MessageBarTitle>
            {error}
          </MessageBarBody>
        </MessageBar>
      )}
    </>
  );
}

/** What "Stop recording by itself when a call ends" does (T14.52). */
export const CALL_END_STOP_EXPLAINED =
  "When a call seems to have ended, Earshot asks whether to stop recording. With this on, Earshot stops the recording after 3 minutes with no answer. With it off, Earshot keeps recording until you stop it. Turning this off applies at once. Turning it on applies from the next meeting.";

/** Stopping by itself when a call ends: read on its own, saved at once and shown as stored. Off by
 * default; off reaches a recording meeting at once, on applies from the next meeting. */
function CallEndStopSection() {
  const styles = useStyles();
  const ipc = useIpc();
  const [load, setLoad] = useLoad((ipc) => ipc.callEndStop());
  // Not disabled while a save is in flight, which would take focus off the switch in WebView2:
  // a press meanwhile is ignored.
  const busy = useRef(false);
  const [error, setError] = useState<string | null>(null);

  if (load.status !== "loaded") {
    return (
      <NotLoaded
        load={load}
        size="small"
        loading="Reading the setting for when a call ends"
        failed="The setting for when a call ends could not be read"
      />
    );
  }
  const set = async (on: boolean) => {
    if (busy.current) return;
    busy.current = true;
    try {
      setLoad({ status: "loaded", view: await ipc.setCallEndStop(on) });
      setError(null);
    } catch (e) {
      setError(errorText(e));
    } finally {
      busy.current = false;
    }
  };
  return (
    <>
      <UnknownSettings unknown={load.view.unknown} title="A stored setting is not known to this version" />
      <Switch
        label="Stop recording by itself when a call ends"
        checked={load.view.on}
        onChange={(_, d) => void set(d.checked)}
      />
      <Caption1 className={styles.muted}>{CALL_END_STOP_EXPLAINED}</Caption1>
      {error !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>This setting was not changed</MessageBarTitle>
            {error}
          </MessageBarBody>
        </MessageBar>
      )}
    </>
  );
}

export interface GeneralPageProps {
  /** Opens the first-run wizard again. */
  onOpenSetup?: () => void;
}

/** The General page of Settings. */
export function GeneralPage({ onOpenSetup }: GeneralPageProps) {
  const styles = useStyles();
  const page = usePageStyles();
  const ipc = useIpc();
  const [load, setLoad] = useLoad((ipc) => ipc.generalSettings());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (load.status !== "loaded") {
    return (
      <NotLoaded
        load={load}
        loading="Reading the general settings"
        failed="The general settings could not be read"
      />
    );
  }
  const { view } = load;

  const setStart = async (on: boolean) => {
    setBusy(true);
    try {
      setLoad({ status: "loaded", view: await ipc.setStartWithWindows(on) });
      setError(null);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className={page.page}>
      <Title2 as="h1" className={styles.heading}>
        General
      </Title2>
      {error !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>This setting was not changed</MessageBarTitle>
            {error}
          </MessageBarBody>
        </MessageBar>
      )}
      <Section title="Start">
        <Switch
          label="Start Earshot with Windows"
          checked={view.start_with_windows === true}
          disabled={busy || view.start_with_windows === null}
          onChange={(_, d) => void setStart(d.checked)}
        />
        <Caption1 className={styles.muted}>{START_EXPLAINED}</Caption1>
        {view.start_error !== null && (
          <Caption1 className={styles.muted}>This setting could not be read: {view.start_error}</Caption1>
        )}
        <Caption1 className={styles.muted}>{TRAY_EXPLAINED}</Caption1>
      </Section>
      <Section title="Calls">
        <CallStartSection />
        <CallEndStopSection />
      </Section>
      <Section title="Keyboard shortcut">
        <Body1 className={styles.text}>{shortcutExplained(view.shortcut)}</Body1>
        {view.shortcut_error !== null && (
          <MessageBar intent="warning">
            <MessageBarBody>{view.shortcut_error}</MessageBarBody>
          </MessageBar>
        )}
      </Section>
      <ThemeSection />
      <Section title="Updates">
        <UpdatesSection />
      </Section>
      {onOpenSetup !== undefined && (
        <Section title="Setup guide">
          <Body1 className={styles.text}>{SETUP_EXPLAINED}</Body1>
          <Button className={styles.action} onClick={onOpenSetup}>
            Open the setup guide
          </Button>
        </Section>
      )}
    </div>
  );
}
