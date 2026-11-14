// The parts every Settings page and the first-run wizard share (T12.54): the read on opening,
// a section with its heading, the page column, what shows until a read is done, the stored
// settings this version does not know and a dismissible notice. Each caller passes its own
// wording: the pages say these things slightly differently, and their tests pin the words.

import {
  Button,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  Spinner,
  Subtitle2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { type Dispatch, type ReactNode, type SetStateAction, useEffect, useId, useRef, useState } from "react";

import { type Ipc, errorText, useIpc } from "../ipc";

/** A read that is under way, failed, or done. */
export type Load<T> = { status: "loading" } | { status: "failed"; message: string } | { status: "loaded"; view: T };

/** Reads `fetch(ipc)` when the component mounts (and again for another `Ipc`); a result that
 * arrives after the component is gone is dropped. `onLoaded` runs with the view as it arrives.
 * The setter lets a save show what the app stored. */
export function useLoad<T>(
  fetch: (ipc: Ipc) => Promise<T>,
  onLoaded?: (view: T) => void,
): [Load<T>, Dispatch<SetStateAction<Load<T>>>] {
  const ipc = useIpc();
  const [load, setLoad] = useState<Load<T>>({ status: "loading" });
  const latest = useRef({ fetch, onLoaded });
  latest.current = { fetch, onLoaded };

  useEffect(() => {
    let live = true;
    void latest.current.fetch(ipc).then(
      (view) => {
        if (!live) return;
        setLoad({ status: "loaded", view });
        latest.current.onLoaded?.(view);
      },
      (e: unknown) => {
        if (live) setLoad({ status: "failed", message: errorText(e) });
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  return [load, setLoad];
}

const useStyles = makeStyles({
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  heading: {
    margin: 0,
  },
});

/** The column every Settings page (and the first-run wizard) lays its sections out in. */
export const usePageStyles = makeStyles({
  page: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXL,
    maxWidth: "860px",
  },
});

/** A section of a page, named by its heading. */
export function Section({ title, children }: { title: string; children: ReactNode }) {
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

/** What shows until a read is done: the spinner labelled `loading`, or the failure titled
 * `failed` with its message. */
export function NotLoaded({
  load,
  loading,
  failed,
  size,
}: {
  load: { status: "loading" } | { status: "failed"; message: string };
  loading: string;
  failed: string;
  size?: "small";
}) {
  if (load.status === "loading") return <Spinner size={size} label={loading} />;
  return (
    <MessageBar intent="error">
      <MessageBarBody>
        <MessageBarTitle>{failed}</MessageBarTitle>
        {load.message}
      </MessageBarBody>
    </MessageBar>
  );
}

/** Stored settings this version does not know (nothing when there are none). */
export function UnknownSettings({
  unknown,
  title = "Some stored settings are not known to this version",
}: {
  unknown: string[];
  title?: string;
}) {
  if (unknown.length === 0) return null;
  return (
    <MessageBar intent="warning">
      <MessageBarBody>
        <MessageBarTitle>{title}</MessageBarTitle>
        {unknown.join(" ")}
      </MessageBarBody>
    </MessageBar>
  );
}

/** A refusal or failure (or any notice), shown until dismissed. */
export function NoticeBar({
  intent = "error",
  title,
  message,
  onDismiss,
}: {
  intent?: "error" | "warning" | "info" | "success";
  title?: string;
  message: ReactNode;
  onDismiss: () => void;
}) {
  return (
    <MessageBar intent={intent}>
      <MessageBarBody>
        {title !== undefined && <MessageBarTitle>{title}</MessageBarTitle>}
        {message}
      </MessageBarBody>
      <MessageBarActions
        containerAction={
          <Button size="small" appearance="transparent" onClick={onDismiss}>
            Dismiss
          </Button>
        }
      />
    </MessageBar>
  );
}
