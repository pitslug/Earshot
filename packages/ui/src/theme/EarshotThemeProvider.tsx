import { FluentProvider, webDarkTheme, webLightTheme } from "@fluentui/react-components";
import {
  type ReactNode,
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useState,
} from "react";

const DARK_QUERY = "(prefers-color-scheme: dark)";

/** The theme a person can choose in Settings, General (T9.1n). */
export type ThemeChoice = "system" | "light" | "dark";

/** Where the choice is kept. A device preference with no meeting content, read before the first
 * paint, so it lives in `localStorage` and not behind IPC. `apps/desktop/public/theme-boot.js`
 * reads the same key (pinned by `EarshotThemeProvider.test.tsx`). */
export const THEME_KEY = "earshot-theme";

/** The stored choice; anything else, or storage that cannot be read, is "system". */
export function readThemeChoice(): ThemeChoice {
  try {
    const stored = window.localStorage.getItem(THEME_KEY);
    if (stored === "light" || stored === "dark" || stored === "system") return stored;
  } catch {
    // A preference that cannot be read never stops the app.
  }
  return "system";
}

function darkQuery(): MediaQueryList | null {
  return typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia(DARK_QUERY)
    : null;
}

/** Whether the system colour scheme is dark, following changes live. */
export function usePrefersDark(): boolean {
  const [dark, setDark] = useState(() => darkQuery()?.matches ?? false);
  useEffect(() => {
    const query = darkQuery();
    if (!query) return;
    const onChange = (event: MediaQueryListEvent) => setDark(event.matches);
    setDark(query.matches);
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  }, []);
  return dark;
}

/** Puts the page itself (native scrollbars, the canvas behind the app) on the theme, with every
 * colour jumping to its new value. Fluent's controls animate `color` and `background` for 100 ms,
 * and on a page producing no frames (a hidden window or preview pane) those animations stay at
 * their first frame: buttons, list rows and links then show the old theme's colours until
 * something repaints them. `FluentProvider` has already written the new tokens when this runs,
 * and nothing has read a style since, so a style pass here takes the new colours with no
 * animation to start. */
function applyToPage(dark: boolean) {
  const root = document.documentElement;
  const off = document.createElement("style");
  off.textContent = "*,*::before,*::after{transition:none!important}";
  document.head.append(off);
  root.dataset.theme = dark ? "dark" : "light";
  root.style.colorScheme = dark ? "dark" : "light";
  void root.offsetHeight;
  off.remove();
}

interface ThemeChoiceValue {
  choice: ThemeChoice;
  setChoice(choice: ThemeChoice): void;
}

const ThemeChoiceContext = createContext<ThemeChoiceValue | null>(null);

/** The theme chosen in Settings. Only under `EarshotThemeProvider`. */
export function useThemeChoice(): ThemeChoiceValue {
  const value = useContext(ThemeChoiceContext);
  if (value === null) throw new Error("useThemeChoice needs an EarshotThemeProvider");
  return value;
}

/** Fluent's web light or dark theme: the one chosen in Settings, or the system's. */
export function EarshotThemeProvider({ children }: { children: ReactNode }) {
  const prefersDark = usePrefersDark();
  const [choice, setStored] = useState<ThemeChoice>(readThemeChoice);
  const dark = choice === "system" ? prefersDark : choice === "dark";

  const setChoice = useCallback((next: ThemeChoice) => {
    setStored(next);
    try {
      window.localStorage.setItem(THEME_KEY, next);
    } catch {
      // The choice still applies until the app closes.
    }
  }, []);
  const value = useMemo(() => ({ choice, setChoice }), [choice, setChoice]);

  useLayoutEffect(() => applyToPage(dark), [dark]);

  return (
    <ThemeChoiceContext.Provider value={value}>
      <FluentProvider
        theme={dark ? webDarkTheme : webLightTheme}
        data-theme={dark ? "dark" : "light"}
        style={{ height: "100%" }}
      >
        {children}
      </FluentProvider>
    </ThemeChoiceContext.Provider>
  );
}
