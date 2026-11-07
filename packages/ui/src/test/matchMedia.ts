// A controllable `window.matchMedia` for tests (jsdom has none).

type Listener = (event: MediaQueryListEvent) => void;

export interface FakeMedia {
  /** Changes whether the colour scheme is dark and notifies listeners. */
  setDark(dark: boolean): void;
}

export function installMatchMedia(initialDark: boolean): FakeMedia {
  let dark = initialDark;
  const lists: { query: string; listeners: Set<Listener>; list: MediaQueryList }[] = [];
  const matches = (query: string) =>
    query.includes("prefers-color-scheme: dark") ? dark : false;
  window.matchMedia = (query: string): MediaQueryList => {
    const listeners = new Set<Listener>();
    const list = {
      get matches() {
        return matches(query);
      },
      media: query,
      onchange: null,
      addEventListener: (_type: string, l: Listener) => listeners.add(l),
      removeEventListener: (_type: string, l: Listener) => listeners.delete(l),
      addListener: (l: Listener) => listeners.add(l),
      removeListener: (l: Listener) => listeners.delete(l),
      dispatchEvent: () => true,
    } as unknown as MediaQueryList;
    lists.push({ query, listeners, list });
    return list;
  };
  return {
    setDark(next: boolean) {
      dark = next;
      for (const { query, listeners } of lists) {
        const event = { matches: matches(query), media: query } as MediaQueryListEvent;
        for (const l of listeners) l(event);
      }
    },
  };
}
