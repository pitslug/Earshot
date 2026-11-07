// Settings, General (docs/ui-design.md "Settings", T7.4h, T9.1n): start Earshot with Windows,
// and the theme: the system's, light or dark.

import {
  Body1,
  Button,
  Caption1,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  Radio,
  RadioGroup,
  Spinner,
  Subtitle2,
  Switch,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { type ReactNode, useEffect, useId, useState } from "react";

import { type GeneralView, type PreviewsView, useIpc } from "../ipc";
import { type ThemeChoice, useThemeChoice } from "../theme/EarshotThemeProvider";
import { CrashLogsFolder } from "./CrashLogsFolder";
import { errorText } from "./useModels";

const useStyles = makeStyles({
  action: {
    alignSelf: "flex-start",
  },
  page: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXL,
    maxWidth: "860px",
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
  },
  text: {
    maxWidth: "70ch",
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
  `Press ${shortcut} anywhere in Windows to start a meeting, and again to stop it. A meeting started this way uses the capture mode on the Audio page.`;

/** What the setup guide is, on the General page. */
export const SETUP_EXPLAINED =
  "The steps Earshot showed the first time: your Obsidian vault, the models, summaries, the audio check and your voice. Nothing changes until you choose something.";

/** What provisional words are, on the General page. */
export const PREVIEWS_EXPLAINED =
  "Shows grey words within a few seconds while someone is talking, then replaces them with the settled text. It uses more of your computer during a meeting and changes nothing that is saved. Only with a Parakeet model, and not on battery.";

/** Whether meetings show provisional words: read and changed on its own, from the next meeting. */
function PreviewsSection() {
  const styles = useStyles();
  const ipc = useIpc();
  const [view, setView] = useState<PreviewsView | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    ipc.previewsSettings().then(
      (v) => {
        if (live) setView(v);
      },
      (e: unknown) => {
        if (live) setProblem(errorText(e));
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  const set = async (on: boolean) => {
    setBusy(true);
    try {
      setView(await ipc.setPreviews(on));
      setProblem(null);
    } catch (e) {
      setProblem(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Section title="During a meeting">
      <Switch
        label="Show words while someone is talking"
        checked={view?.on === true}
        disabled={busy || view === null || !view.can_change}
        onChange={(_, d) => void set(d.checked)}
      />
      <Caption1 className={styles.muted}>{PREVIEWS_EXPLAINED}</Caption1>
      {view?.note != null && <Caption1 className={styles.muted}>{view.note}</Caption1>}
      {problem !== null && (
        <MessageBar intent="error">
          <MessageBarBody>{problem}</MessageBarBody>
        </MessageBar>
      )}
    </Section>
  );
}

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

function Section({ title, children }: { title: string; children: ReactNode }) {
  const styles = useStyles();
  const id = useId();
  return (
    <section className={styles.section} aria-labelledby={id}>
      <Subtitle2 as="h2" id={id} className={styles.heading}>
        {title}
      </Subtitle2>
      {children}
    </section>
  );
}

type Load =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; view: GeneralView };

export interface GeneralPageProps {
  /** Opens the first-run wizard again. */
  onOpenSetup?: () => void;
}

/** The General page of Settings. */
export function GeneralPage({ onOpenSetup }: GeneralPageProps) {
  const styles = useStyles();
  const ipc = useIpc();
  const [load, setLoad] = useState<Load>({ status: "loading" });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    void ipc.generalSettings().then(
      (view) => {
        if (live) setLoad({ status: "loaded", view });
      },
      (e: unknown) => {
        if (live) setLoad({ status: "failed", message: errorText(e) });
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  if (load.status === "loading") return <Spinner label="Reading the general settings" />;
  if (load.status === "failed") {
    return (
      <MessageBar intent="error">
        <MessageBarBody>
          <MessageBarTitle>The general settings could not be read</MessageBarTitle>
          {load.message}
        </MessageBarBody>
      </MessageBar>
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
    <div className={styles.page}>
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
      <PreviewsSection />
      <Section title="Keyboard shortcut">
        <Body1 className={styles.text}>{shortcutExplained(view.shortcut)}</Body1>
        {view.shortcut_error !== null && (
          <MessageBar intent="warning">
            <MessageBarBody>{view.shortcut_error}</MessageBarBody>
          </MessageBar>
        )}
      </Section>
      {onOpenSetup !== undefined && (
        <Section title="Setup guide">
          <Body1 className={styles.text}>{SETUP_EXPLAINED}</Body1>
          <Button className={styles.action} onClick={onOpenSetup}>
            Open the setup guide
          </Button>
        </Section>
      )}
      <ThemeSection />
      <Section title="About">
        <Body1>Earshot {view.version}</Body1>
      </Section>
      <Section title="Crash logs">
        <CrashLogsFolder />
      </Section>
    </div>
  );
}
