// Settings, Notes and storage: where the crash logs are (T9.1e). The path is read-only, for copying; the
// app has no opener plugin.

import { Caption1, Input, makeStyles, tokens } from "@fluentui/react-components";
import { useEffect, useState } from "react";

import { useIpc } from "../ipc";

const useStyles = makeStyles({
  muted: {
    color: tokens.colorNeutralForeground3,
    maxWidth: "70ch",
  },
  path: {
    maxWidth: "70ch",
  },
});

/** What a crash log holds and what it never does. */
export const CRASH_LOGS_EXPLAINED =
  "If Earshot's own code stops on an error, it saves a short report on this computer: the app version, the time and where in the program it stopped. A crash inside a speech or summary engine leaves no report. A report never holds anything said in a meeting, and nothing is sent anywhere. The newest 20 are kept.";

/** What the page says when the app has no data folder. */
export const CRASH_LOGS_NO_FOLDER = "Earshot has no data folder here, so it cannot save crash logs.";

/** The folder the crash logs are in, with what they hold. */
export function CrashLogsFolder() {
  const styles = useStyles();
  const ipc = useIpc();
  // undefined until read; null when there is no folder or it cannot be read.
  const [folder, setFolder] = useState<string | null | undefined>(undefined);

  useEffect(() => {
    let live = true;
    void ipc.crashLogsFolder().then(
      (path) => {
        if (live) setFolder(path);
      },
      () => {
        if (live) setFolder(null);
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  return (
    <>
      <Caption1 className={styles.muted}>{CRASH_LOGS_EXPLAINED}</Caption1>
      {folder === null && <Caption1 className={styles.muted}>{CRASH_LOGS_NO_FOLDER}</Caption1>}
      {typeof folder === "string" && (
        <Input className={styles.path} readOnly aria-label="Crash logs folder" value={folder} />
      )}
    </>
  );
}
