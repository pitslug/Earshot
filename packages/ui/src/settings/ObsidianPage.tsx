// Settings, Obsidian (docs/ui-design.md "Settings", PLAN 3.8, T7.4h): the vault folder and
// the subfolder exported notes go to. The app checks them when they are saved (the folder
// exists, is a folder and can be written to; the subfolder stays inside the vault). "Choose
// folder..." fills the vault field from the Windows folder dialog (T7.4l); Save still stores.

import {
  Body1,
  Button,
  Field,
  Input,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  Spinner,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useEffect, useState } from "react";

import { type ObsidianSettingsView, useIpc } from "../ipc";
import { type SaveOnNext, useSaveOnNext } from "./saveOnNext";
import { errorText } from "./useModels";

const useStyles = makeStyles({
  page: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXL,
    maxWidth: "860px",
  },
  heading: {
    margin: 0,
  },
  text: {
    maxWidth: "70ch",
  },
  fields: {
    display: "grid",
    gap: tokens.spacingVerticalM,
    maxWidth: "520px",
  },
  save: {
    justifySelf: "start",
  },
});

/** What the page says about export. */
export const EXPORT_EXPLAINED =
  "Earshot writes a meeting into your Obsidian vault only when you choose Export on that meeting. Nothing is written on its own.";

/** Where notes go, in words. */
export function exportPlace(vault: string, folder: string): string {
  return folder === "" ? `Notes go to the top folder of ${vault}.` : `Notes go to ${folder} in ${vault}.`;
}

type Load =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; view: ObsidianSettingsView };

export interface ObsidianPageProps {
  /** Leaves out the page's own heading (the first-run wizard has its own). */
  noHeading?: boolean;
  /** The first-run wizard saves with its Next, so no Save button shows. */
  saveOnNext?: SaveOnNext;
}

/** The Obsidian page of Settings. */
export function ObsidianPage({ noHeading = false, saveOnNext }: ObsidianPageProps) {
  const styles = useStyles();
  const ipc = useIpc();
  const [load, setLoad] = useState<Load>({ status: "loading" });
  const [vault, setVault] = useState("");
  const [folder, setFolder] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [picking, setPicking] = useState(false);
  const [pickError, setPickError] = useState<string | null>(null);

  const show = (view: ObsidianSettingsView) => {
    setLoad({ status: "loaded", view });
    setVault(view.value.vault);
    setFolder(view.value.folder);
  };

  useEffect(() => {
    let live = true;
    void ipc.obsidianSettings().then(
      (view) => {
        if (live) show(view);
      },
      (e: unknown) => {
        if (live) setLoad({ status: "failed", message: errorText(e) });
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  const unsaved = load.status === "loaded" && (vault !== load.view.value.vault || folder !== load.view.value.folder);

  const save = async () => {
    setBusy(true);
    setSaved(false);
    try {
      show(await ipc.setObsidianSettings({ vault, folder }));
      setError(null);
      setSaved(true);
      return true;
    } catch (e) {
      setError(errorText(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  useSaveOnNext(saveOnNext, async () => !unsaved || save());

  if (load.status === "loading") return <Spinner label="Reading the Obsidian settings" />;
  if (load.status === "failed") {
    return (
      <MessageBar intent="error">
        <MessageBarBody>
          <MessageBarTitle>The Obsidian settings could not be read</MessageBarTitle>
          {load.message}
        </MessageBarBody>
      </MessageBar>
    );
  }
  const stored = load.view.value;
  const changed = unsaved;

  const choose = async () => {
    setPicking(true);
    try {
      const picked = await ipc.pickObsidianVault(vault.trim());
      if (picked !== null) {
        setVault(picked);
        setSaved(false);
      }
      setPickError(null);
    } catch (e) {
      setPickError(errorText(e));
    } finally {
      setPicking(false);
    }
  };

  return (
    <div className={styles.page}>
      {!noHeading && (
        <Title2 as="h1" className={styles.heading}>
          Obsidian
        </Title2>
      )}
      <Body1 className={styles.text}>{EXPORT_EXPLAINED}</Body1>
      {load.view.unknown.length > 0 && (
        <MessageBar intent="warning">
          <MessageBarBody>
            <MessageBarTitle>A stored setting could not be used</MessageBarTitle>
            {load.view.unknown.join(" ")}
          </MessageBarBody>
        </MessageBar>
      )}
      {pickError !== null && (
        <MessageBar intent="warning">
          <MessageBarBody>{pickError}</MessageBarBody>
        </MessageBar>
      )}
      {error !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>This setting was not saved</MessageBarTitle>
            {error}
          </MessageBarBody>
        </MessageBar>
      )}
      <form
        className={styles.fields}
        onSubmit={(e) => {
          e.preventDefault();
          void save();
        }}
      >
        <Field
          label="Vault folder"
          hint="The folder of your Obsidian vault, for example C:\Users\you\Documents\Notes. Leave it empty to turn export off."
        >
          <Input
            value={vault}
            onChange={(_, d) => setVault(d.value)}
            spellCheck={false}
            contentAfter={
              <Button
                appearance="transparent"
                size="small"
                disabled={busy || picking}
                onClick={() => void choose()}
              >
                Choose folder...
              </Button>
            }
          />
        </Field>
        <Field
          label="Subfolder"
          hint={`A folder inside the vault, for example ${load.view.default_folder} or Work/Meetings. Leave it empty for the vault's top folder.`}
        >
          <Input value={folder} onChange={(_, d) => setFolder(d.value)} spellCheck={false} />
        </Field>
        {saveOnNext === undefined && (
          <Button type="submit" appearance="primary" className={styles.save} disabled={busy || !changed}>
            Save
          </Button>
        )}
      </form>
      {saved && !changed && (
        <MessageBar intent="success">
          <MessageBarBody>
            {stored.vault === ""
              ? "Saved. Export is off until a vault is set."
              : `Saved. ${exportPlace(stored.vault, stored.folder)}`}
          </MessageBarBody>
        </MessageBar>
      )}
      {!saved && stored.vault !== "" && <Body1 className={styles.text}>{exportPlace(stored.vault, stored.folder)}</Body1>}
    </div>
  );
}
