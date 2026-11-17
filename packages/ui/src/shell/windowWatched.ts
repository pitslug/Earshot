// Whether the person can see this window now (T12.76).

import { useEffect, useState } from "react";

/** Whether the window is watched: its page is visible (not hidden to the tray or minimised) and it
 * has the focus. The webview's view of the rule `shell::window_watched` applies to notifications
 * (visible, not minimised, focused). */
export function windowWatched(): boolean {
  return document.visibilityState === "visible" && document.hasFocus();
}

/** `windowWatched`, read again whenever the page's visibility or the window's focus changes. */
export function useWindowWatched(): boolean {
  const [watched, setWatched] = useState(windowWatched);
  useEffect(() => {
    const update = () => setWatched(windowWatched());
    document.addEventListener("visibilitychange", update);
    window.addEventListener("focus", update);
    window.addEventListener("blur", update);
    update();
    return () => {
      document.removeEventListener("visibilitychange", update);
      window.removeEventListener("focus", update);
      window.removeEventListener("blur", update);
    };
  }, []);
  return watched;
}