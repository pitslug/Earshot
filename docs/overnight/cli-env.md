# Overnight report: lane/cli-env (2026-11-12)

## T9.25 What a Claude Code or Codex child inherits

Result: done (the owner's decision: clear them; no notice option built).

### What changed
- `earshot_llm::claude_code::CLEARED_ENV` (now `pub`, 61 names): the T7.4ga markers (`CLAUDECODE`, `CLAUDE_CODE_SIMPLE`), every documented API address (`ANTHROPIC_BASE_URL` and the Bedrock, Mantle, Claude Platform on AWS, Foundry and Vertex base URLs, `ANTHROPIC_FOUNDRY_RESOURCE`), provider switches (`CLAUDE_CODE_USE_*`) and their skipped sign-ins (`CLAUDE_CODE_SKIP_*_AUTH`), credentials other than the stored sign-in (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_CUSTOM_HEADERS`, `CLAUDE_CODE_OAUTH_TOKEN` and its refresh token and scopes, profile, federation and workspace ids, the AWS and Foundry keys, `AWS_BEARER_TOKEN_BEDROCK`), OpenTelemetry export (`CLAUDE_CODE_ENABLE_TELEMETRY`, the beta tracing switches, `OTEL_*_EXPORTER`, OTLP endpoints and headers, `OTEL_LOG_*` content flags) and proxies (`HTTP(S)_PROXY`, `NO_PROXY`, upper and lower case).
- `earshot_llm::codex::CLEARED_ENV` (16 names): `OPENAI_BASE_URL`, `OPENAI_API_BASE`, `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, `OPENAI_FEDERATION_RULE_ID`, `OPENAI_IDENTITY_TOKEN_FILE`, `OPENAI_WORKLOAD_IDENTITY_CONTEXT`, `HTTP(S)_PROXY`, `ALL_PROXY`, `NO_PROXY` (upper and lower case). Codex has no environment provider switch (`model_provider` is a config key and `--ignore-user-config` is passed); its OTel export is config-only.
- Kept on purpose: `CLAUDE_CONFIG_DIR` and `CODEX_HOME` (which of the user's own sign-ins), CA and client-certificate variables (they name no address).
- Search order: known install folders before `PATH`. Claude Code: `%USERPROFILE%\.local\bin`, `%APPDATA%\npm`, `%LOCALAPPDATA%\npm`, then `PATH`. Codex: `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin` (new: the standalone installer's default), then the npm folders, then `PATH`.
- PRIVACY.md: the Claude Code / Codex bullet says what is cleared and what that means.

### Where each name came from (read 2026-11-12)
- Claude Code (2.1.292 installed): https://code.claude.com/docs/en/env-vars (all address, provider, credential, header and proxy names), https://code.claude.com/docs/en/network-config (lower-case proxy forms), https://code.claude.com/docs/en/monitoring-usage (telemetry export names). `claude --help` lists no environment variables beyond `CLAUDE_CODE_SIMPLE`.
- Codex (0.166.0 installed): https://learn.chatgpt.com/docs/config-file/environment-variables (credential and federation names; formerly developers.openai.com/codex/environment-variables), the config reference (`openai_base_url`, upstream proxy "from the environment"), T7.4gc's measurement for `OPENAI_BASE_URL`/`OPENAI_API_BASE`/`OPENAI_API_KEY`; proxy names from reqwest's docs (crate Proxies section, `NoProxy::from_env`). Every Codex name except `OPENAI_API_BASE` was also found as text in the installed `codex.exe`. Standalone folder: `$defaultVisibleBinDir` in openai/codex `scripts/install/install.ps1`.

### Accept
- `cargo test -p earshot-llm claude_code` -> pass (unit 6 passed; tests/claude_code.rs 15 passed, 1 ignored: the real-CLI run)
- `cargo test -p earshot-llm codex` -> pass (unit 9 passed; tests/codex.rs 15 passed, 1 ignored: the real-CLI run)
- `cmd /c pnpm --filter @earshot/ui test` -> 468 passed, 2 failed ("Test Files 1 failed | 35 passed (36)"), both in `src/detail/MeetingDetail.test.tsx` "suggested words (T12.15e)" (`keeps the word with Keep...`, `shows a refusal as a line edit's refusal is shown`: "Unable to find an accessible element with the role table"), with other lanes building. That file alone: "Tests 84 passed (84)". This lane changes no UI code; the coordinator's batch ci on main is the check that counts. Looks like the tabster hide-pass timing under load (AGENTS.md), not proven.
- Also: `cargo test -p earshot-llm` all pass; `cargo clippy -p earshot-llm --all-targets -- -D warnings` clean; `cargo fmt --all -- --check` clean.
- Negative control: the new tests run against the old `src/` failed 3 cases in each binary (environment, search order, sign-in check environment).

### Review
code-agents:code-reviewer (privacy). First pass BLOCKED: Claude Code's OpenTelemetry export (`CLAUDE_CODE_ENABLE_TELEMETRY`, `OTEL_*`, `OTEL_LOG_USER_PROMPTS`) can send the prompt to a user's collector and was not cleared. Fixed, plus four advisories (OAuth refresh token and scopes, `CLAUDE_CONFIG_DIR` named as an exception, `OPENAI_WORKLOAD_IDENTITY_CONTEXT`, PRIVACY wording).

### For the owner
- Design question (reviewer, not built): a deny list follows a vendor table that grows each release. The stronger form is an allowlist: `env_clear()` in the child and pass only what the CLI needs (`PATH`, `SystemRoot`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `TEMP`, ... plus `CLAUDE_CONFIG_DIR`/`CODEX_HOME`). Needs an `env_clear` flag in earshot-proc and a real sign-in check on both CLIs.
- A user signed in only through `CLAUDE_CODE_OAUTH_TOKEN` or `CODEX_ACCESS_TOKEN` now reads as not signed in (summary skipped with a banner). Neither is set on this desktop.
- Not cleared, undocumented: `CODEX_REFRESH_TOKEN_URL_OVERRIDE` is present in `codex.exe` 0.166.0 (would send the refresh token elsewhere); the brief required documented names only.
- Not run: the two ignored real-CLI cases (they now run with every cleared variable set to a dead loopback address, so a pass would prove the clearing on the real CLIs).

### Proposed AGENTS.md bullets
- The agent-CLI child environment (T9.25) exists once per CLI, `claude_code::CLEARED_ENV` and `codex::CLEARED_ENV`, each name cited from the vendor's docs; the tests' `MUST_CLEAR` lists are written out, never read from the crate, and the fakes record the environment of the run and of the sign-in check. Re-read the vendor's environment variable reference (Claude Code: env-vars and monitoring-usage, which holds the OTel names) on every CLI version change. Claude Code's OpenTelemetry export is switched on from the environment and can carry the prompt; `DISABLE_TELEMETRY` does not turn it off.
- std `Command::env_remove` on Windows is case-insensitive (measured by the reviewer 2026-11-12): lower-case proxy names in a clear list serve other systems only.
- CLI search: known install folders before `PATH` (`CliSearch::dirs`, `CodexCli::search_dirs`); Codex's standalone folder is `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin`.

### TODO.md
- None added.
