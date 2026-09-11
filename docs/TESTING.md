# Testing Strategy

Tests should protect current behavior: runtime tools, session guards, project
and file operations, Git and shell dispatch, Runner transports, MCP, OpenAPI,
and OAuth scope policy. Test count alone proves neither useful coverage nor
unnecessary complexity. Trace a test to a current entry point, consumer, or real
boundary; a passing test of an unused configuration parser or always-empty
policy does not justify keeping that mechanism.

When retiring a concept, remove its dedicated tests and fixtures with it. Keep
coverage of the surviving public behavior and safety boundaries, rather than
replacing removed assertions with source-text checks that freeze another
implementation detail. Also review layering, global state leakage, bounded
waits, and the cost of each test lane.

## Test Lanes

| Lane | Purpose | Default resources | Typical command |
|---|---|---|---|
| fast unit | Pure parsing, validation, helpers, local state machines, small fixtures. | No network, no global env mutation, no long sleeps. | `cargo test -p webcodex --lib tool_call` |
| contract/schema | Keep metadata, registry, MCP `tools/list`, OpenAPI, and runtime tool names synchronized. | No external network; in-process services are preferred. | `cargo test -p webcodex --lib metadata`; `cargo test -p webcodex --lib mcp`; `cargo test -p webcodex --lib openapi` |
| local integration | Exercise HTTP handlers, runtime dispatch, sessions, local agent registry, temp dirs, loopback listeners, and database fixtures. | Loopback only, isolated temp dirs, bounded waits, no shared mutable state without a lock. | `cargo test -p webcodex --lib runtime_http -- --nocapture`; `cargo test -p webcodex --lib session -- --nocapture` |
| Runner real-process | Process-tree ownership, real shell timeout/stop, polling dispatch timing, Plugin startup, validation/Git `ManagedChild`, and JobManager descendant cleanup. These tests are ignored by the ordinary Runner suite and share the `runner_real_process_` name prefix. | Real local child processes only; no external network. Run serially because the assertions intentionally exercise OS scheduling and process teardown. | `cargo test --locked -p webcodex-runner runner_real_process -- --ignored --test-threads=1` |
| Process lifecycle real-process | `ManagedChild` ownership, graceful/forced termination, descendants, EOF, liveness, and reaping. Most lifecycle tests in the integration target are ignored; pure type/spawn-error smoke remains ordinary. | Real local helper processes and OS liveness probes. | `cargo test --locked -p webcodex-process --test managed_child -- --ignored --test-threads=1` |
| Persistent-shell timing | Timeout, concurrent busy-state, close-vs-exec, idle expiry, descendant teardown, and heavy adversarial PowerShell timing/status coverage. Fast state/error/exit smoke remains ordinary. | Real shell processes; serial execution only. | `cargo test --locked -p webcodex-persistent-shell -- --ignored --test-threads=1` |
| Desktop Windows real-process | Windows Desktop stdin-EOF shutdown and bounded-command process-tree reclamation. These tests are ignored by the ordinary Desktop suite and share the `desktop_real_process_windows_` name prefix. | Real local child processes only; no external network. Run serially so PowerShell startup and process teardown do not compete with the ordinary Desktop libtest pool. | `cargo test --locked --manifest-path apps/desktop/src-tauri/Cargo.toml desktop_real_process_windows_ -- --ignored --test-threads=1` |
| slow/manual ignored | Valuable coverage that is local but slow, serial, large-input, or global-state-sensitive. | Explicit operator opt-in; often `--ignored` and `--test-threads=1`. | Run the specific ignored test/filter documented by its subsystem. |
| e2e/deployment smoke | Prove that binaries, local services, GPT Actions schema, MCP, artifact transfer, and an agent can work together. | Temporary local services and loopback ports; real deployment only when explicitly requested. | `bash scripts/e2e_zero_config_ws.sh`; `bash scripts/smoke_deployment.sh`; `bash scripts/smoke_artifact_transfer.sh` |
| reconnect continuity | Runner disconnect/reconnect layer independence, stale-not-ready observations, reconciliation-aware recovering/lost transitions, server-restart durable Session plus explicit-session continuity, meaningful-activity scoping, and version-mismatch diagnostics. | In-process fixtures, no external network. | `cargo test -p webcodex --lib reconnect` |
| trusted smoke | Disposable git fixture full chain (start → edit → failing shell validation → fix → pass → git review → finish) asserting zero approval interruptions under `trusted_agent` authority, resolved failure evidence, dirty-worktree advisory-only, and bounded payloads; prints baseline counters. | Temp git fixture, no external network. | `cargo test -p webcodex --lib trusted_smoke` |
| real-process reconnect harness | Boot a real server plus reconciliation-capable runner, assert layered connection observations, crash the runner (layers degrade independently; running job enters `recovering`), restart with a new runner instance (old job is fenced to terminal `lost` with `runner_instance_replaced`, no server restart), then restart the server and verify runner auto-reconnect plus durable Session lookup and continuation by the original explicit `session_id`. It also prints post-deploy smoke facts (server version/commit, authority mode, version compatibility, runner shell dialect). | Local processes and loopback ports. | `bash scripts/e2e_reconnect_ws.sh` |
| real-process hosted-connect harness | Build the real Server, Runner, and CLI; start a shared-key-enabled loopback Server; run `webcodex connect`; verify same-key project visibility and a read, cross-key isolation, detached Runner survival, repeated-connect PID reuse, hosted `runner status`, explicit stop, secret-safe output/log/state, and an untouched Git checkout. | Local processes, isolated XDG config/state roots, a temp Git project, bounded curl and outer timeout, trap cleanup; never production. | `bash scripts/e2e_hosted_connect.sh` |
| real-process job reconciliation harness | Boot a real server plus a WebSocket runner that advertises `job_state_reconciliation`. Scenario A keeps a raw async Job running across a SERVER-only restart and asserts the SAME runner instance, original `job_id`, preserved ownership/project/session, non-regressing sequence/log cursors, `recovered_after_server_restart`, original-process stop, and one side-effect set. Scenario B lets a Job complete while the Server is offline and reconciles the terminal result without duplicate logs or execution. Scenario C forces `run_process` past its synchronous grace window, then proves the handed-off structured Job survives a Server restart and an old Server-epoch observation token refreshes immediately for the same `job_id`. Scenario D uses a delayed Cargo fixture to force a real `cargo_check` validation handoff past its sync window, then proves the same restart/token-refresh/stop contract with the validation command started exactly once. Ordinary Runner-owned Jobs keep the Runner process alive for these scenarios; `run_detached_process` restart survival is a separate supervisor-ownership contract covered by its focused Runner suites and production dogfood. | Local processes, temp dirs/ports/tokens, and a temp project; no production services or QUIC certs. Scenario D intentionally takes roughly the validation sync window plus restart time. | `bash scripts/e2e_job_reconciliation_ws.sh` |
| real-process job recovery failure/non-reconciliation harness | Cover the failure and non-reconciliation paths the happy-path reconciliation harness omits, using `WEBCODEX_JOB_RECOVERY_GRACE_SECS=10` (clamped, above the 5s floor) so the deadline is bounded without waiting the 120s default. Scenario C: kill the runner only (server stays up), let the job enter `recovering`, and assert the non-request-triggered recovery-timeout sweep transitions it to `lost` with `runner_recovery_deadline_exceeded`, `ended_at` set once, one list record, stop-on-lost stable, and the command never re-executes. Scenario D: instance B replaces instance A (same client_id, new `agent_instance_id`); A's job becomes `lost` with `runner_instance_replaced`, B starts its own new job, A's late update is rejected, first `ended_at`/reason preserved. Scenario E: a generation-2 Runner registered with `WEBCODEX_RUNNER_DISABLE_JOB_STATE_RECONCILIATION=1` (no capability, no inventory) dispatches a job and, on disconnect, deterministically fences it to `lost` with `runner_disconnected_without_reconciliation` (never `recovering`); after a server restart the lost job has no durable record and a same-client new no-reconciliation instance cannot revive it. Scenario F: a long job across three server restarts keeps the same `job_id`, runs the command once, keeps `last_update_seq`/log cursors non-regressing and markers non-duplicating, and reaches a terminal `stopped` that survives a third restart with `ended_at` unchanged by terminal inventory replay. | Local processes, temp dirs/ports/tokens, and a temp project; no production services or QUIC certs. | `bash scripts/e2e_job_recovery_failures_ws.sh` |
| security auth matrix | Cover OAuth, scope policy, shared-key behavior, token classes, read-only session guards, and denied mutations. | No external identity provider by default; use local fixtures and synthetic tokens. | `cargo test -p webcodex --lib oauth -- --nocapture`; `cargo test -p webcodex --lib scope -- --nocapture`; `cargo test -p webcodex --lib metadata -- --nocapture` |

The Result App's DOM/message-order regression tests run without browser or npm
dependencies: `node --test src/mcp_tests/result_app.test.mjs`. They exercise the
embedded HTML script, including results arriving before or after initialization
success, rejection, and timeout. Rust projection and capability tests use
`cargo test --locked -p webcodex --lib result_app`.

## Explicit High-Cost Local Evidence

Ordinary `cargo test` and ordinary CI intentionally skip ignored timing/real-process
coverage. Run the smallest relevant group locally when changing one of these boundaries:

```bash
cargo test --locked -p webcodex-runner runner_real_process -- --ignored --test-threads=1
cargo test --locked -p webcodex-process --test managed_child -- --ignored --test-threads=1
cargo test --locked -p webcodex-persistent-shell -- --ignored --test-threads=1
cargo test --locked -p webcodex --lib tool_runtime_real_process_ -- --ignored --test-threads=1
cargo test --locked --manifest-path apps/desktop/src-tauri/Cargo.toml desktop_real_process_windows_ -- --ignored --test-threads=1
```

These commands are evidence for the boundary they exercise, not a routine pre-commit
check. Prefer one exact ignored test while iterating, then the relevant group when the
change is ready for review.

## CI Mapping

The lanes above define test semantics; workflows decide when to run them.

- `.github/workflows/ci.yml` is the ordinary repository gate. Its cheap `changes`
  job classifies the exact PR base...head path set before native scheduling, while
  the `contract` job remains mandatory for every configured pull request and every
  push to `main`. The classifier is deterministic and local to Git: it does not use
  commit messages or PR titles, and it emits frontend, per-platform, and package-lane
  requirements. For changed Rust/Cargo files it searches only bounded platform-marker
  lines from both the base and head file versions, so body-only changes inside an
  existing platform cfg remain visible without serializing near-complete file diffs.
  If that marker scan exceeds its bound, CI fails closed to native core plus
  architecture compilation while preserving path-derived package/Desktop decisions;
  only an untrustworthy changed-path inventory falls back to the complete native
  matrix. The contract lane always owns workspace-boundary self-test/checks,
  formatting, the heuristic test-inventory self-test/report (without count thresholds),
  and focused registry/OpenAPI/MCP schema and metadata parity. Main and Desktop
  frontend dependency installation/type/test/build steps run only when the classifier
  selects their respective frontend surface; full-native invocations select both.
- The heavy Linux Rust matrix `test-linux-rust` and Linux tooling lane
  `test-linux-tooling` run for every pull request as well as every push to `main`,
  including owner-authored PRs. They start in parallel with `contract` rather than
  waiting for unrelated frontend/static work. Native child lanes likewise wait only
  for the cheap `changes` classifier, while the stable macOS/Windows/native aggregates
  retain the mandatory `contract` gate. Pushes to `main`, external-contributor PRs,
  and owner PRs carrying `run-ci` force the complete deterministic native matrix.
  Real-process and timing-sensitive ignored tests are deliberately outside ordinary
  CI, including full-native overrides: run them explicitly when changing their
  lifecycle boundary or investigating platform behavior. Computer, platform-specific,
  Desktop, npm, packaging, signing, and release surfaces retain their existing
  independent deterministic lanes. The stable `test-macos`,
  `test-windows`, and `test-native` aggregates always resolve and verify each child
  lane is `success` when required or `skipped` when not required, avoiding a skipped
  required-check context that could leave branch protection pending.
- Linux Rust execution remains package-sharded: the server package `webcodex`, the
  Runner package `webcodex-runner`, and the remaining workspace crates run in
  parallel. Ordinary libtest compiles ignored real-process coverage but does not
  execute it. The remainder shard uses
  `--workspace --exclude webcodex --exclude webcodex-runner`, so newly added
  workspace members enter CI automatically rather than depending on a hand-maintained
  package list. The split changes scheduling, not process-ownership coverage.
- Linux tooling runs in parallel with the Rust shards and retains
  release-verification tooling, Markdown-link validation, and npm package-smoke
  tooling on every PR. The complete `cargo check --workspace --all-targets` pass is
  reserved for pushes to `main`, external-contributor PRs, and explicit `run-ci` PRs;
  ordinary owner PRs already pay for the package-sharded Rust test compilation and do
  not repeat that broad compile-only pass. macOS and Windows native jobs keep deterministic Runner/Computer/Desktop
  coverage, but they do not execute ignored real-process groups. Process-tree,
  detached-supervisor, shell timeout/stop, PowerShell stdin EOF, fake-SSH lifecycle,
  selected Plugin shutdown, and similar OS-scheduling-sensitive coverage is retained
  as explicit local evidence. The local-`sshd` SSH integration fixture remains
  Linux-only and manual because it depends on Linux daemon account/auth configuration.
- Exact-source release acceptance is a separate trust boundary from ordinary CI.
  Release readiness first binds the exact source to a successful `main`-push CI run;
  main pushes force the complete deterministic native classification. Readiness then
  runs its release-specific E2E/eval and disposable Server-image checks. Manual
  real-process evidence remains separate and must never be inferred from a passing CI
  run. Follow [`RELEASE_CHECKLIST.md`](RELEASE_CHECKLIST.md) and
  `.github/workflows/release-readiness.yml`.

## Default Test Principles

- No external network by default. Tests that need HTTP should use in-process
  clients or loopback mock servers. Real internet, real cloud services, and real
  deployment targets belong in explicitly named manual smoke workflows.
- Local mock server tests must be isolated. Bind to `127.0.0.1:0`, avoid fixed
  ports, scope URL rewrites to the test fixture, reset global overrides even on
  failure paths, and stop spawned tasks when the fixture drops.
- Tests that mutate process environment must acquire `TEST_ENV_LOCK` or an
  equivalent shared guard, save the previous value, and restore or remove it at
  the end. Do not print token values while diagnosing these tests.
- Tests that touch HTTP/auth behavior must use `AuthEnvGuard` or an equivalent
  `TEST_ENV_LOCK` guard for auth mode env, especially
  `WEBCODEX_SHARED_KEY_ENABLED`, `WEBCODEX_ALLOW_ANONYMOUS`,
  and `WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE`. Managed-token rejection tests should
  explicitly disable direct shared-key fallback and open anonymous mode before
  asserting that an unknown or wrong bearer returns 401.
- Keep the auth-mode semantics separate in tests:
  `WEBCODEX_SHARED_KEY_ENABLED` is direct Bearer shared-key fallback, while
  `WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE` is only the OAuth authorize bridge.
  Quick-start shared-key mode intentionally accepts an unknown non-`wc_` Bearer
  as a lightweight shared-key principal, but invalid `wc_` managed-token
  prefixes and empty or whitespace Bearer values must still be rejected.
- Sleep, timeout, and polling tests must be bounded. Positive readiness uses one
  absolute deadline created once for the whole wait and never reset after partial
  progress; prefer channels, notifications, or direct state inspection. A test whose
  correctness materially depends on real process startup, scheduler timing, a long
  sleep, or a wall-clock timeout belongs in ignored explicit evidence rather than the
  default suite. Short deterministic negative probes, semantic grace windows, and
  exact count/protocol iterations may remain when they are the contract.
  Async Runner shutdown waits are notification-driven by `ShutdownCoordinator`;
  tests should signal that state directly rather than sleep for a presumed polling interval.
- Ignored tests are not dead tests. Each ignored test should have a reason and a
  documented command for running it intentionally. Ordinary CI never opts into
  `--ignored`: `runner_real_process_`, `desktop_real_process_windows_`, slow timing
  regressions, and real Codex/LSP dogfood remain explicit local/operator evidence.

## `import_http` Coverage

Conversation-import tests use bounded loopback fixtures in the ordinary local-integration
surface. Legacy ignored HTTP fixtures for redirect and download-size limits were retired
after equivalent boundary coverage moved to the current MCP import path; source-name
fallback is covered directly at the import-name helper. Keep new coverage on the current
transport/runtime path instead of preserving duplicate historical fixtures.

Run the current heuristic inventory with:

```bash
bash scripts/test_inventory.sh
```

The script is intentionally heuristic. It scans all Git-tracked Rust files across
the workspace, so crate-local tests (including Runner tests) are included. Using
the Git index as the source set excludes ordinary untracked `target/` output and
scratch files without maintaining a second ignore list. It does not access the
network or modify the workspace. The output includes a
stable tab-separated area summary for the root `webcodex` package and each
`crates/*` member, plus sanitized risk clues. Use
`bash scripts/test_inventory.sh --details` for a full sanitized file/line list,
and `bash scripts/test_inventory.sh --self-test` to exercise the inventory
contract against a temporary Git fixture.

## Current Test Layout Notes

Recent structure work moved large test groups out of production roots:

- OAuth HTTP endpoint tests are rooted at `src/oauth_http/tests.rs` and grouped
  by endpoint/domain under `src/oauth_http/tests/*`.
- CLI tests are grouped under
  `crates/webcodex-cli/src/webcodex_cli/tests/*`.
- CLI help smoke coverage lives with the CLI test modules and covers common
  help entry points, so new command help should extend that smoke coverage.
- Runtime HTTP tests live under `src/runtime_http/tests/*`; historical ignored
  import fixtures should not be retained once equivalent current-path coverage exists.
- Tool runtime tests live under `src/tool_runtime/tests/*` by domain.

Do not add large ordinary test blocks to production facade files when one of
these `tests/` module trees already exists. Exact full-suite pass counts should
come from a fresh `cargo test -p webcodex --lib` run; this document should not be
treated as the source of truth for exact counts.
