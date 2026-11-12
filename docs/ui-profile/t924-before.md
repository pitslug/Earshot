# T9.24 before-profile (the Surface, 2026-11-13)

Numbers only, no fixes. Measured on the Surface (Snapdragon X2 Elite, ARM64, mains) in the session's built-in browser (Chromium WebView), with the Vite preview serving `apps/desktop/profile/index.html`: the app (`AppShell`) on mock data, wrapped in React's `<Profiler>`, which records every commit's `actualDuration`. The page is evaluation only: Vite's build reads `index.html` alone and the desktop `tsconfig.json` does not include `profile/`, so nothing here ships.

Data: a 300-meeting store (the four sample meetings plus 296 generated finished ones), the sample "Weekly sync" meeting stretched to 3 hours (1,800 transcript rows, one every 6 s, summary and speakers as the sample), and for the live case a call started from New meeting, then 1,800 `Landed` lines (3 hours, both tracks in turn) delivered at once through the meeting-event listeners (`window.__profile.landLive(3)`); the mock keeps sending its own `Levels` 10 times a second and the other live events.

Caveats: React's development build (the Profiler does not run in a production build) and `StrictMode`, so absolute times are larger than the shipped app's; compare scenarios with each other. The pane was in view for every number below (`document.visibilityState` visible; a first pass taken while the pane was hidden is discarded). The mock's download progress runs at the mock's own step rate, not one event per network chunk, so the download row says nothing about the real event rate (T9.24a's throttle is measured by its own test). The mock's search is JavaScript over sample details, not the runtime's `search_meetings`, so the search row measures rendering only.

| Scenario | Commits / s | Median commit | p95 commit | Main thread in React per second |
|---|---|---|---|---|
| Idle, meeting list of 300 | 1.1 | 108 ms | 117 ms | 103 ms (10%) |
| Open the 3 h meeting (Summary tab), first 4 s | 2.1 | 75 ms | 132 ms | 143 ms |
| Idle on the 3 h meeting's Summary tab | 1.1 | 100 ms | 114 ms | 95 ms (10%) |
| Open its Transcript tab (1,800 rows), first 5 s | 1.9 | 259 ms | 314 ms | 388 ms |
| Idle on the 3 h transcript | 1.1 | 284 ms | 304 ms | 304 ms (30%) |
| Typing a 14-character search with the 3 h transcript open | 1.6 | 248 ms | 310 ms | 339 ms |
| Live call, just started (list of 300 behind it) | 10.7 | 54 ms | 102 ms | 629 ms (63%) |
| Live call with 3 h of transcript | 7.2 (frames dropped) | 98 ms | 143 ms | 749 ms (75%) |
| Settings, Summaries page, idle | 1.2 | 7.6 ms | 8.5 ms | 9 ms |
| Settings, Summaries page, mock download running | 2.2 | 7.3 ms | 9.1 ms | 15 ms |

What the numbers show (for the fixes T9.24 lists, in its order):

- **Something commits the whole app about once a second while nothing happens** (idle list, idle summary, idle transcript: 1.1 commits/s each). Its cost follows what is on screen: about 100 ms with the list of 300, about 285 ms with the 3 h transcript. This matches the 1 s clock re-rendering all of `AppShell`; the profiler here records the whole tree, so the source is inferred, not proven.
- **Live meetings are dominated by the 10 Hz `Levels` events**: 10.7 commits/s at the start, each re-rendering beyond the meter (54 ms median with only the list behind). With 3 hours of live transcript each commit costs about 98 ms, the rate falls to 7 a second because the main thread is three-quarters busy, so the window would feel slow. Holding the levels and the clock in leaf components is the largest fix by these numbers.
- **The 1,800-row transcript costs about 260-310 ms per commit** whatever caused it (opening, typing a search, the clock). Memoised rows should come before virtualisation; whether virtualisation is needed should be judged from the after-profile of the first two fixes.
- **Download progress**: not measurable with the mock (see caveats); the Summaries page itself is cheap (under 10 ms a commit).

Reproduce: `cmd /c pnpm --filter @earshot/desktop exec vite --port 1431 --strictPort`, open `http://localhost:1431/profile/index.html`, then in the page's console: `__profile.reset()`, do the scenario, wait, `__profile.stats()`. For the live case start a call from New meeting, then `__profile.landLive(3)`. Keep the window in view.
