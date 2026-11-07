// Runs before the first paint (a blocking script in <head>; the CSP allows no inline script), so
// the page starts on the chosen theme with no flash of the other one. React's
// EarshotThemeProvider takes over from here; the key and the rule are pinned to it by
// packages/ui/src/theme/EarshotThemeProvider.test.tsx.
(function () {
  var choice = null;
  try {
    choice = window.localStorage.getItem("earshot-theme");
  } catch (e) {
    // A preference that cannot be read means the system's theme.
  }
  var dark =
    choice === "dark" ||
    (choice !== "light" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  var root = document.documentElement;
  root.dataset.theme = dark ? "dark" : "light";
  root.style.colorScheme = dark ? "dark" : "light";
})();
