# T9.24 after-profile (the desktop, 2026-11-13)

Taken on the desktop (Windows 11 x86-64, RTX 5080 host), not the Surface, so the numbers compare with each other and with a before-run on the same desktop below, never directly with [t924-before.md](t924-before.md). Same method as the before-profile: the session's built-in browser, the Vite preview serving `apps/desktop/profile/index.html` (port 1430), React's development build under `StrictMode`, every commit's `actualDuration` from `<Profiler>`, the same data (300 meetings, the 3-hour "Weekly sync", 1,800 landed lines for the live case). Every number below was taken with the page visible (`document.visibilityState`); one run taken while the pane was hidden is discarded.

The scenarios now run in one go from the page: `__profile.scenarios()` on a freshly loaded page (about 80 s), result also in `window.__profileResult`. Its rates are over each scenario's whole window (10 s idle, 4 s open summary, 5 s open transcript, 3.6 s typing, 8 s each live case), so a window with no commit reads 0. The desktop before-run used the same steps by hand, with rates between the window's first and last commit (equal to the window for the steady scenarios; it overstates the open and typing rates, which are therefore given as commits per window below).

What changed (T9.24): the clock is held by the elapsed-time leaf (`Elapsed`) and the list's day by `useLocalDay` (checked once a minute); `Levels` events go to a store outside React state that only the meter bars subscribe to (`useLevel`); live lines (`LiveLine`) and the meeting page's transcript lines (`TranscriptLine`) are memoised; a newer search cancels the one before in the runtime (`search::Searches`, `search_meetings_unless`) and the UI treats the replaced search's `superseded` error as no answer.

| Scenario | Before (desktop): commits | median | p95 | React ms/s | After (desktop, 2 runs): commits | median | p95 | React ms/s |
|---|---|---|---|---|---|---|---|---|
| Idle, meeting list of 300 (10 s) | 10 | 92-100 ms | 101-111 ms | 84-94 | 0, 0 | - | - | 0 |
| Open the 3 h meeting, Summary tab (4 s) | 7 | 53-58 ms | 119-121 ms | 105-116 | 2, 2 | 142-156 ms | 142-156 ms | 37-42 |
| Idle on its Summary tab (10 s) | 10 | 100-104 ms | 110-120 ms | 95-98 | 0, 0 | - | - | 0 |
| Open its Transcript tab, 1,800 rows (5 s) | 8 | 296-303 ms | 328-519 ms | 373-431 | 3, 3 | 0.4-0.6 ms | 402-443 ms | 80-89 |
| Idle on the 3 h transcript (10 s) | 10 | 319-353 ms | 389-629 ms | 346-387 | 0, 0 | - | - | 0 |
| Typing a 14-character search, transcript open (3.6 s) | 24-31 | 190-464 ms | 253-870 ms | 655-754 | 17, 17 | 15-17 ms | 52-54 ms | 111-113 |
| Live call, just started (8 s) | 63 (7.9/s) | 96 ms | 180 ms | 725 | 99, 104 (12-13/s) | 0.1-0.2 ms | 109-135 ms | 206-239 |
| Live call with 3 h of transcript (8 s) | 24 (3.0/s, frames dropped) | 198 ms | 303 ms | 592 | 83, 88 (10-11/s) | 0.2 ms | 110-114 ms | 121-123 |

Before ranges are the two desktop before-runs (the live rows had one complete run). After ranges are the two after-runs.

What the numbers show:

- **Idle costs nothing now**: no commit in 10 s on the list, the summary or the 3 h transcript (before: one whole-app commit a second, 100 to 350 ms each).
- **Live meetings**: the 10 a second level updates are now commits of about 0.2 ms (the meters only). The p95 of about 110-135 ms is the commit when a line lands, which still renders `AppShell` and its 300-row meeting list; that happens once per settled stretch, not 10 times a second. With 3 hours of live transcript the page keeps all 10 level updates a second (before: 3 a second, frames dropped) and spends about 120 ms a second in React (before: about 590).
- **Typing a search** with the 3 h transcript open: commits fell from 190-460 ms to about 15 ms (median), because the transcript's lines are no longer rendered again.
- **Rows**: after memoising, the transcript's 1,800 rows cost only when the tab first mounts (one commit of about 400-440 ms in the development build); nothing measured here renders them again. Virtualisation was therefore not done: it would only shorten that first mount, and it would put citations (scroll to a line) and the browser's find-in-page at risk, which both rely on every line being in the page.
- **Search cancellation** is not measurable here (the mock's search is JavaScript); it is proved by tests (`search_with_a_cancelled_token_gives_no_answer`, `search_checks_its_token_before_every_meeting_and_answers_as_before`, `search_a_newer_search_supersedes_the_one_before`, `search_replaced_by_a_newer_one_fails_as_superseded_without_the_query`, and the UI's "search treats a search a newer one replaced as no answer, never a failure").

Reproduce: `cmd /c pnpm --filter @earshot/desktop exec vite --port 1430 --strictPort`, open `http://localhost:1430/profile/index.html` with the window in view, then in the page's console `await __profile.scenarios()`.
