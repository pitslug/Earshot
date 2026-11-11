// Settings, General, Updates (T9.1b, D24): nothing is asked of the network until the button is
// pressed or automatic checks are on; nothing downloads until Download; nothing restarts until the
// user says so.

import {
  Body1,
  Button,
  Caption1,
  Dialog,
  DialogActions,
  DialogBody,
  DialogContent,
  DialogSurface,
  DialogTitle,
  Field,
  MessageBar,
  MessageBarBody,
  ProgressBar,
  Select,
  Switch,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useEffect, useState } from "react";

import { IpcFailure, type ScheduleChoice, type UpdatePhase, type UpdateView, useIpc } from "../ipc";
import { formatSize } from "./copy";
import { errorText } from "./useModels";

const useStyles = makeStyles({
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  row: {
    display: "flex",
    gap: tokens.spacingHorizontalS,
  },
  action: {
    alignSelf: "flex-start",
  },
  field: {
    maxWidth: "320px",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
});

/** What the section says about a check. */
export const UPDATES_EXPLAINED =
  "A check asks GitHub for one small file. GitHub sees your IP address and the request, and nothing about your meetings. Every update is verified with a key built into Earshot before it runs.";

/** The automatic switch: the one consent for checks nobody pressed a button for. */
export const AUTO_CHECK_LABEL = "Check for updates automatically";

/** How often automatic checks run, in the order the list shows them (T12.22). */
export const SCHEDULE_OPTIONS: readonly { value: ScheduleChoice; label: string }[] = [
  { value: "hourly", label: "Every hour" },
  { value: "four_hours", label: "Every 4 hours" },
  { value: "twelve_hours", label: "Every 12 hours" },
  { value: "daily", label: "Every day" },
  { value: "weekly", label: "Every week" },
];

/** Under the list with automatic checks on. */
export const SCHEDULE_EXPLAINED =
  "Earshot checks when it starts, then this often while it runs, counted from the last check.";

/** Under the list with automatic checks off: the list still applies at once, for later. */
export const SCHEDULE_OFF_EXPLAINED =
  "Automatic checks are off, so Earshot asks nothing until you press Check for updates.";

/** A stored schedule this version does not know. */
export function scheduleUnknownText(stored: string): string {
  return `The stored schedule (${JSON.stringify(stored)}) is not known to this version; every day is used.`;
}

/** What the section says before it restarts the app. */
export const RESTART_EXPLAINED =
  "Earshot closes, the update installs, and Earshot opens again. Your meetings and settings stay as they are.";

/** Shows where the update stands, and the one step that can come next. */
export function UpdatesSection() {
  const styles = useStyles();
  const ipc = useIpc();
  const [view, setView] = useState<UpdateView | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    ipc.updateStatus().then(
      (v) => {
        if (live) setView(v);
      },
      (e: unknown) => {
        if (live) setProblem(errorText(e));
      },
    );
    const stop = ipc.onUpdateEvent((e) => {
      if (live) setView((v) => (v === null ? v : { ...v, phase: e.phase }));
    });
    return () => {
      live = false;
      void stop.then((f) => f());
    };
  }, [ipc]);

  const run = async (work: () => Promise<unknown>) => {
    setBusy(true);
    setProblem(null);
    try {
      await work();
    } catch (e) {
      setProblem(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const setAuto = (on: boolean) =>
    run(async () => {
      setView(await ipc.setUpdateAutoCheck(on));
    });

  // A choice applies at once (the save rule): the schedule reads it at its next wake.
  const setSchedule = (schedule: ScheduleChoice) =>
    run(async () => {
      setView(await ipc.setUpdateSchedule(schedule));
    });

  const phase: UpdatePhase = view?.phase ?? { state: "Idle" };
  const check = (
    <Button className={styles.action} disabled={busy || view === null} onClick={() => void run(() => ipc.checkForUpdate())}>
      Check for updates
    </Button>
  );

  return (
    <div className={styles.section}>
      <Body1>Earshot {view?.current ?? ""}</Body1>
      {phase.state === "Idle" && check}
      {phase.state === "Checking" && <Caption1 className={styles.muted}>Checking for updates</Caption1>}
      {phase.state === "UpToDate" && (
        <>
          <Body1>Earshot is up to date.</Body1>
          {check}
        </>
      )}
      {phase.state === "Available" && (
        <>
          <Body1>
            Earshot {phase.version} is available ({formatSize(phase.size)}).
          </Body1>
          <Button
            className={styles.action}
            appearance="primary"
            disabled={busy}
            onClick={() => void run(() => ipc.downloadUpdate())}
          >
            Download
          </Button>
        </>
      )}
      {phase.state === "Downloading" && (
        <>
          <Body1>Downloading Earshot {phase.version}</Body1>
          <ProgressBar
            aria-label={`Downloading Earshot ${phase.version}`}
            value={phase.total > 0 ? phase.done / phase.total : 0}
          />
          <Button className={styles.action} onClick={() => void run(() => ipc.cancelUpdate())}>
            Cancel
          </Button>
        </>
      )}
      {phase.state === "Ready" && (
        <>
          <Body1>Earshot {phase.version} is ready to install.</Body1>
          <Button
            className={styles.action}
            appearance="primary"
            disabled={busy}
            onClick={() => setConfirming(true)}
          >
            Restart and install
          </Button>
        </>
      )}
      {phase.state === "Failed" && (
        <>
          <MessageBar intent="error">
            <MessageBarBody>{phase.message}</MessageBarBody>
          </MessageBar>
          {check}
        </>
      )}
      {problem !== null && (
        <MessageBar intent="error">
          <MessageBarBody>{problem}</MessageBarBody>
        </MessageBar>
      )}
      <Switch
        label={AUTO_CHECK_LABEL}
        checked={view?.auto_check === true}
        disabled={busy || view === null}
        onChange={(_, d) => void setAuto(d.checked)}
      />
      <Field label="How often" className={styles.field}>
        <Select
          value={view?.schedule ?? "daily"}
          disabled={busy || view === null}
          onChange={(_, d) => void setSchedule(d.value as ScheduleChoice)}
        >
          {SCHEDULE_OPTIONS.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </Select>
      </Field>
      <Caption1 className={styles.muted}>
        {view?.auto_check === true ? SCHEDULE_EXPLAINED : SCHEDULE_OFF_EXPLAINED}
      </Caption1>
      {view?.schedule_unknown != null && (
        <MessageBar intent="warning">
          <MessageBarBody>{scheduleUnknownText(view.schedule_unknown)}</MessageBarBody>
        </MessageBar>
      )}
      <Caption1 className={styles.muted}>{UPDATES_EXPLAINED}</Caption1>
      <Dialog modalType="alert" open={confirming} onOpenChange={(_, d) => setConfirming(d.open)}>
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Restart and install?</DialogTitle>
            <DialogContent>{RESTART_EXPLAINED}</DialogContent>
            <DialogActions>
              <Button
                appearance="primary"
                onClick={() => {
                  setConfirming(false);
                  void run(async () => {
                    try {
                      await ipc.installUpdate();
                    } catch (e) {
                      // The app reports this one as a phase too: one message is enough.
                      if (!(e instanceof IpcFailure && e.error.kind === "update_failed")) throw e;
                    }
                  });
                }}
              >
                Restart and install
              </Button>
              <Button onClick={() => setConfirming(false)}>Not now</Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
    </div>
  );
}
