// An error bar of the app window that can go away while it holds keyboard focus: its Dismiss
// was pressed, or its Try again worked (T14.15, T14.16). A button that is removed while focused
// leaves focus on nothing, and the next Tab starts from the top of the window; so the bar says,
// just before it goes, that focus was in it, and the window puts focus back on the screen.

import {
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
} from "@fluentui/react-components";
import { type ReactNode, useLayoutEffect, useRef } from "react";

export interface ErrorBarProps {
  title: string;
  /** What went wrong, in the app's words. */
  children: ReactNode;
  /** Its buttons (Dismiss, Try again). */
  actions: ReactNode;
  /** Called as the bar is removed while focus is inside it, before it leaves the page: the
   * caller moves focus. Not called when focus is elsewhere. */
  onGoneWithFocus(): void;
}

/** An error bar with a title, the reason and buttons, announced as the app's other error bars
 * that appear after something the user did are (`role="alert"`, as the meeting page's "could not
 * start the job" bar). */
export function ErrorBar({ title, children, actions, onGoneWithFocus }: ErrorBarProps) {
  const bar = useRef<HTMLDivElement | null>(null);
  const gone = useRef(onGoneWithFocus);
  gone.current = onGoneWithFocus;
  // A layout effect's cleanup runs while the bar is still on the page, so where focus is can
  // still be read; the element is kept here because the ref is cleared on the way out.
  useLayoutEffect(() => {
    const el = bar.current;
    return () => {
      if (el !== null && el.contains(el.ownerDocument.activeElement)) gone.current();
    };
  }, []);
  return (
    <MessageBar ref={bar} intent="error" role="alert">
      <MessageBarBody>
        <MessageBarTitle>{title}</MessageBarTitle>
        {children}
      </MessageBarBody>
      <MessageBarActions>{actions}</MessageBarActions>
    </MessageBar>
  );
}
