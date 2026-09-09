# Release Readiness Checklist

This checklist is for final release readiness before tagging, publishing artifacts, updating client schemas, or deploying a new WebCodex server/Runner/runtime build.

It governs release/publish rollouts and deployment of published releases. An explicitly requested development/dogfood deployment of a reviewed commit is governed by [`AGENTS.md`](../AGENTS.md) and [Runner Release Process Notes](agent/release-process.md): it does not require the version/tag/publication/artifact steps below, but it still uses the focused post-deployment smoke in section 10 where applicable.

Do not create tags, push commits, publish npm packages, create GitHub Releases, rewrite history, deploy, or touch secrets while running this checklist unless the operator explicitly requests that action.

## 1. Source Validation

After the release-prep PR is squash-merged, select the exact `origin/main` commit that would be tagged. Final pre-tag validation is a reviewed GitHub Actions workflow, not a sequence of hand-run release-host commands:

```bash
python3 scripts/release_operator.py readiness-start \
  --source-sha <MAIN_COMMIT> \
  --state-file <STATE_FILE>

python3 scripts/release_operator.py readiness-status \
  --state-file <STATE_FILE> \
  --wait-secs 3600
```

`readiness-start` first requires GitHub `main` to equal `<MAIN_COMMIT>` and requires exactly one successful `push` CI run for that exact source. It records that CI run id and attempt in the mode-0600 operator-local state before dispatch, then passes the proof into `.github/workflows/release-readiness.yml`. The workflow re-fetches that exact immutable CI attempt with read-only Actions authority and fails closed unless it is the successful `.github/workflows/ci.yml` main-push run for the requested source. Readiness never tags, publishes, deploys, uploads release candidates, rebuilds the six native release archives, or builds the three formal Desktop distribution candidates.

If dispatch delivery becomes uncertain or the run is not resolved before the short start timeout, **do not create a second state file and do not redispatch**. Continue with `readiness-status` on the original state; its unique request id recovers the exact workflow run when present. Only terminal `success` (operator exit 0) satisfies the pre-tag gate. A nonterminal/unresolved status is not release approval.

The evidence graph deliberately avoids repeating work that exact-main CI already completed. Main-push CI is the cross-platform correctness authority: it runs formatting/frontend/workspace boundaries, complete Linux Rust package shards, `cargo check --workspace --all-targets`, the deterministic release static/tooling contract, native macOS Intel/Apple-Silicon Desktop lanes, Windows x64 core/Runner/package/Desktop-installer lanes, and lightweight native Linux arm64 plus Windows arm64 production-target compile gates. Release readiness first proves that exact CI attempt, then runs only release-specific WebSocket/polling zero-config E2E and coding-loop compare eval in parallel. After both succeed, it builds the disposable native `linux/amd64` and `linux/arm64` Server images, verifies runtime/health/non-root/bootstrap generation, and discards them. The six native release-profile/ABI/PE/Mach-O/package surfaces and three formal Desktop distribution artifacts are intentionally **not** rebuilt before tagging; `.github/workflows/release-build.yml` remains their single authoritative builder after the immutable tag exists. The eval job explicitly builds debug Server/Runner fixtures before running the harness so cold-runner compilation is not misclassified as a service-start timeout.

For focused diagnosis outside the final workflow, the underlying commands remain available, but a local pass does not replace the exact-source readiness run.

## 2. Focused Runtime Tests

During implementation/review, run focused lanes when touching runtime metadata, schemas, OpenAPI, MCP, session, handoff, validation, or coding-task behavior. Exact-main CI owns the full Linux package suites plus deterministic metadata/schema/OpenAPI/MCP and release-tooling evidence; release-readiness binds that successful CI attempt instead of repeating it. Re-run an individual lane only to diagnose a failure or when a review explicitly requires separate evidence.

## 3. Product Documentation Check

Confirm the user-facing docs tell one story:

- README states the product position in the first screen and clearly separates full daily use from temporary `share`.
- Full Setup has one recommended regular Server + Runner path for ordinary daily use; Quick Trial stays focused on temporary single-project `share`.
- Concepts explains server, Runner, Runner-registered projects, runtime project ids, ToolRuntime, MCP, GPT Actions, session, handoff, validation, review/hygiene, and `run_shell` as a bounded shell primitive for real shell semantics or short related command chains.
- Architecture starts with client/server/Runner/codebase, security-boundary, and runtime-module diagrams before Rust module notes.
- MCP and GPT Actions both say they call the same WebCodex ToolRuntime.
- Security explains what the model can and cannot do, project access, Runner trust boundary, shell/job risk, token handling, session/audit evidence, and revocation.
- The release PR / GitHub Release notes read like external release notes and include highlights, compatibility or breaking changes, known limitations, upgrade notes, and validation. Do not restore per-version release-note files as a second documentation source.
- Roadmap stays short and does not promise a full IDE replacement, autonomous ops, arbitrary computer use, or universal client compatibility.

Run `python3 scripts/check_markdown_links.py` (also included in `release_check.sh`) and require zero missing repository-local links. Product narrative and allowed legacy-term matches remain review judgments in the release-prep PR; the readiness workflow automates only deterministic checks.

## 4. Legacy Surface Guard

Scan docs and scripts for stale onboarding guidance:

```bash
rg "run_codex|Codex delegation|retained runner|future explicit opt-in|WEBCODEX_ENABLE_LEGACY_CODEX_RUN|PROJECTS_CONFIG|server_static|/api/codex|api/codex|projects.toml" README.md README.zh-CN.md docs deploy scripts SECURITY.md
```

Allowed matches are negative statements, release-note breaking changes, guard tests, and deployment comments that explicitly say the legacy path is removed or not required.

Do not allow docs that ask users to configure server-side project onboarding, imply legacy routes exist, imply `run_codex` exists, or describe retained runner / future opt-in behavior as the current plan.

## 5. E2E Smoke

After exact-main CI proof succeeds, the release-readiness workflow builds dedicated debug `webcodex-server` / `webcodex-runner` fixtures once for both supported zero-config transports while the compare eval runs independently. Disposable Server-image validation waits for both release-specific gates. The debug fixtures and images are never uploaded or promoted; `.github/workflows/release-build.yml` remains the only native release-candidate producer after the immutable tag exists.

The underlying smoke commands remain:

```bash
bash scripts/e2e_zero_config_ws.sh
E2E_TRANSPORT=polling bash scripts/e2e_zero_config_ws.sh
```

These commands remain useful for diagnosis, but do not rerun them manually after a successful readiness workflow merely to duplicate evidence. They must never target a production repository; any write checks stay within disposable probe files or a temporary project.

## 6. Eval Harness

The exact-source release-readiness workflow runs `EVAL_MODE=compare bash scripts/eval_coding_loop.sh`. Run it separately only for focused diagnosis. The eval harness measures scripted WebCodex tool-call mechanics; it is not a full model-behavior evaluation.

## 7. Security And Leakage Checks

Confirm:

- No secrets, `.env`, credentials, token files, generated deployment env files, or Authorization headers were touched or printed.
- `finish_coding_task` and `session_handoff_summary` compact outputs do not expose raw stdout/stderr bodies, command text, tails, excerpts, env values, tokens, or secrets.
- `run_shell` is documented as a bounded shell primitive, while structured validators remain the default validation source.
- Model-facing runtime docs keep admin, account, pairing, token-management, and Runner-token management outside MCP and GPT Actions.

## 8. Packaging And Artifact Checks

For every new binary and npm release, choose one candidate `<VERSION>` first and treat its tag and uploaded bytes as immutable once published:

- If `<VERSION>` already has a tag only because an earlier pre-publication attempt failed, reclaim it **before preflight** only through `python3 scripts/release_operator.py reclaim-tag --version <VERSION> --confirm v<VERSION> --root <EXACT_MAIN_WORKTREE>`. The command requires clean exact remote `main`, matching Cargo/npm versions, an annotated tag, no GitHub Release, no npm version, no active matching release-build, and no successful authoritative release-build. Keep failed historical runs as evidence. The normal Release check is authenticated; `--allow-public-release-check` is permitted only after the human operator separately confirms there is no draft Release.
- Before pre-tag readiness, run `python3 scripts/release_operator.py preflight --version <VERSION> --source-sha <MAIN_COMMIT> --root <EXACT_MAIN_WORKTREE>`. It requires a clean exact source; matching workspace Cargo, npm, Desktop package, Desktop Cargo, and Tauri app versions; GitHub `main` at that source; an unused Git tag/GitHub Release/npm version; and usable GitHub/npm publication identities without printing credentials.
- Prefer `python3 scripts/release_operator.py doctor --version <VERSION> --source-sha <MAIN_COMMIT> --root <EXACT_MAIN_WORKTREE>` before release day. `doctor` is read-only: it combines publication preflight, the five-way release-version fence, exact-main CI proof, the six-platform npm runtime plus Windows x64/macOS Intel/macOS Apple-Silicon Desktop workflow contract, draft-capable authenticated Release-list access, verifier parsing, required tool availability, and `actionlint` when installed. It validates that the current macOS distribution path remains ad-hoc signed and does not depend on paid Apple signing credentials. It reports all failed checks together and performs no tag, dispatch, draft, publish, or deployment mutation.
- For normal operator flow, initialize one durable high-level plan with `release-init --version <VERSION> --source-sha <MAIN_COMMIT> --root <EXACT_MAIN_WORKTREE> --state-file <PLAN_STATE> --work-dir <PLAN_WORK_DIR>`, then use `release-resume --state-file <PLAN_STATE> --wait-secs <N>` and `release-status --state-file <PLAN_STATE>`. The plan automatically advances only safe-to-initiate/recoverable phases (readiness observation, bound build observation, same-run bundle collection, npm staging, draft verification) and stops with `needs_authorization` before tag creation, draft creation, and public publication. Existing low-level commands remain the recovery/debug primitives and the plan never substitutes a second dispatch after uncertainty.
- `Cargo.toml`, every local WebCodex workspace entry in `Cargo.lock`, `npm/webcodex/package.json`, `apps/desktop/package.json`, `apps/desktop/src-tauri/Cargo.toml`, `apps/desktop/src-tauri/tauri.conf.json`, `manifest.example.json`, and the relevant package self-tests must agree on `<VERSION>` before tagging.
- `npm/webcodex/manifest.json` is generated release metadata and is intentionally not tracked. Do not commit real checksums or create a post-tag checksum-only PR.
- Build the six published runtime platforms (`linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`, `win32-x64`, `win32-arm64`) natively from the exact `v<VERSION>` tag through the reviewed release-build workflow. The `win32-x64`, `darwin-x64`, and `darwin-arm64` jobs each stage their already-built and verified runtime binaries into the matching Desktop distribution without recompiling a second runtime. `darwin-x64` uses GitHub's native Intel runner and `darwin-arm64` uses native Apple Silicon. The unsigned macOS runtime bytes are the same release-build inputs used by the runtime archive; platform code signing may change the Mach-O bytes copied into the `.app`, so native evidence records both the unsigned input digest and the bundled signed digest rather than claiming byte-for-byte equality after signing.
- Dispatch `release-build.yml` through `release_operator.py build-start` with a fresh mode-0600 state file, then observe that same state with `build-status`. The operator requires GitHub `main` and the remote annotated tag to resolve to the exact release source, injects one durable `rb_<24 hex>` request id into the workflow run name, and never blind-redispatches after an uncertain POST outcome. Only terminal success for the bound run/source satisfies the build gate.
- For both `linux-x64` and `linux-arm64`, build in the native-architecture manylinux2014 userspace used by the release-build workflow. Before packaging, inspect all three ELF binaries with `readelf` and fail the release if any required `GLIBC_*` symbol version exceeds 2.17 or an unexpected host-specific `DT_NEEDED` dependency appears. The published Linux x64 and arm64 artifacts therefore share the glibc 2.17 floor.
- Pin one `WEBCODEX_BUILT_AT` value for the release. Every final `webcodex`, `webcodex-server`, and `webcodex-runner` binary must report `<VERSION>`, the same concrete tag commit, `dirty=false`, and the shared `built_at`.
- Windows packaging must use `scripts/package_release_artifact.ps1` in its default provenance-checked mode. `-AllowDevelopmentBuild` is for local/CI smoke only and its output must never be uploaded.
- macOS verification and formal GitHub Release candidates use ad-hoc signing and are explicitly not notarized. This keeps Desktop distribution free of Apple Developer Program credentials. Native CI still verifies the app code signature, runtime identity, architecture, DMG contents, and exact release provenance. Gatekeeper may require the user to approve a newly downloaded build through **System Settings → Privacy & Security → Open Anyway**; do not instruct users to disable Gatekeeper globally. Developer ID signing and notarization can be introduced later as a separate distribution upgrade if the project chooses to pay for that trust path.
- After all six native targets succeed, the release-build workflow must aggregate only artifacts from that same workflow run, verify every per-target SHA sidecar plus all three Desktop sidecars and native Mac signing evidence, and upload one `<ARCHIVE_STEM>-bundle`. For a real `v<VERSION>` release the retained bundle contains the six unchanged native archives, `webcodex-desktop-v<VERSION>-win32-x64-setup.exe`, `webcodex-desktop-v<VERSION>-darwin-x64.dmg`, `webcodex-desktop-v<VERSION>-darwin-arm64.dmg`, `manifest.json`, one `SHA256SUMS` covering all nine downloadable product bytes, both Linux ELF reports, and `release-build.json`. `release-build.json` keeps the six runtime entries under `artifacts` and the three Desktop entries under `desktop_artifacts`; the npm manifest remains six-platform only.
- The release control host collects that single assembled bundle instead of fetching six target artifacts independently. Use `python3 scripts/release_operator.py collect --run-id <RUN_ID> --source-sha <TAG_COMMIT> --tag v<VERSION> --output-dir <BUNDLE_DIR>` as the canonical path. `collect` resolves exactly one `*-bundle` artifact from the locked successful run, validates its run/source/expiry/SHA-256 metadata, downloads it by artifact id through the GitHub REST API (not `gh run download`), verifies the artifact zip digest, extracts it with bounded path/type/size checks, and validates `release-build.json`, `SHA256SUMS`, archive membership, and release manifest consistency before atomically exposing `<BUNDLE_DIR>`. It reuses `GH_TOKEN`/`GITHUB_TOKEN` or the current `gh auth` login without printing credentials. Never substitute an archive from another workflow run.
- Create and validate the npm publication tree with `python3 scripts/release_operator.py stage-npm --bundle-dir <BUNDLE_DIR> --source-root <EXACT_TAG_WORKTREE> --output-dir <STAGE_DIR>`. It revalidates the retained bundle, requires a clean worktree at the immutable tag, extracts only the exact verified `linux-x64` binaries into a bounded temporary directory, then runs the existing staging helper and `npm_package_smoke.sh --binary-dir`. The release path never invokes Cargo and compares installed native files byte-for-byte with the retained CI candidates.
- Create a draft GitHub Release and upload exactly the six native archives, the Windows x64 Desktop installer, both macOS DMGs, and `SHA256SUMS` (ten initial assets). Before making it public, run `python3 scripts/release_operator.py verify-draft --bundle-dir <BUNDLE_DIR>`; require the draft asset set, sizes, states, and GitHub-provided `sha256:` digests to match the retained bundle. Do not re-download the same retained draft bytes on the control host. The post-public `verify_public_release.py` run is the single full public-byte download/verification pass; Linux verification hashes the DMGs but does not claim to prove code signing/notarization.
- Server container publication is owned by `.github/workflows/release-image.yml`, separate from the read-only `release-build.yml` candidate workflow. It runs only after a GitHub Release is public (or by guarded manual backfill of that already-public immutable tag), builds native `linux/amd64` and `linux/arm64` images from the exact annotated tag with the release build identity, verifies the non-root runtime, exact Server/CLI identity, health check, and absence of the Runner, then publishes immutable `v<VERSION>` / `<VERSION>` image tags (with SemVer `+` build metadata encoded as `_`, since Docker tags do not allow `+`). From the final immutable multi-arch digest it also generates one self-contained `webcodex-server-bootstrap.sh` from the reviewed publication-workflow source; that script embeds and materializes a Compose definition pinned to the exact image digest, which keeps guarded backfill usable for Releases whose application tags predate the deployment tooling. `webcodex-server-image.json` records the application source SHA, deployment-tooling source SHA, manifest/child digests, and bootstrap SHA-256. Existing Release assets are reconciled byte-for-byte on rerun; only the current latest stable GitHub Release may move the mutable `latest` tag.
- GitHub creates the first GHCR package as private by default. On the first WebCodex Server image publication only, the repository owner must change `webcodex-server` package visibility to **Public** in GitHub Packages, then rerun the failed anonymous-availability gate. After activation, require anonymous inspection/pull availability for every release; do not distribute registry credentials to end users.

## 9. Release Sequence

For the normal path, the sequence below may be driven by one `release-init` + repeated `release-resume` calls. Each `needs_authorization` result is a hard human boundary, not a request for the operator to infer consent. Low-level commands listed in the individual steps remain authoritative recovery/debug operations.

1. Put version bumps, release notes, platform docs, packaging changes, and release tests in **one release-prep PR** and squash-merge it into `main`. Resolve the exact merged commit. If an explicitly approved failed pre-publication tag for `<VERSION>` exists, run the guarded `release_operator.py reclaim-tag` now and require successful remote-ref reconciliation. Run `release_operator.py doctor` before the release window, then create the durable plan with `release-init`; require all three publication namespaces (Git tag, GitHub Release, npm version) to be unused, both publication identities to be available, and the exact merged source's main-push CI to be successful.
2. `release-resume` dispatches one readiness run and keeps its nested durable state; repeated resumes observe that same run until terminal. The low-level equivalent is `readiness-start` plus `readiness-status --wait-secs 3600`. Readiness binds the exact successful CI run id/attempt, independently revalidates that proof, runs release-specific E2E/eval, and only then validates the two disposable Server-image architectures. No six-platform native release-profile candidate is built in readiness. If dispatch/observation is uncertain, recover from the same state instead of blind redispatch.
3. After readiness success, the plan stops at `awaiting_tag_authorization`. Only after explicit human authorization create and push the immutable annotated `v<VERSION>` tag at that exact source; never move it. On the next resume the plan reconciles the remote annotated tag and dispatches/observes one bound authoritative build. The low-level equivalent is `build-start --source-sha <TAG_COMMIT> --tag v<VERSION> --state-file <BUILD_STATE>` once, then `build-status --state-file <BUILD_STATE> --wait-secs 7200`. Require all six native targets, all three native Desktop candidates/smokes, native macOS ad-hoc code-signing evidence, and `assemble` to succeed in that one bound workflow run.
4. After build success, `release-resume` collects only that run's `<ARCHIVE_STEM>-bundle` and records it in the plan. The low-level `collect` command remains available for recovery. Existing bundle bytes are reconciled against exact source/tag rather than silently replaced.
5. The same resume stages npm from the retained bundle and exact tag worktree without recompilation. If the stage path already exists but the successful state transition was not durably recorded, the plan stops with `needs_reconciliation` rather than deleting/re-running it. `stage-npm` remains the low-level primitive.
6. The plan then stops at `awaiting_draft_authorization`. After explicit authorization, create a **draft** GitHub Release and upload the six retained archives, all three retained Desktop distribution artifacts, and `SHA256SUMS`; the next resume runs the same `verify-draft` digest gate and requires all ten initial GitHub asset digests/sizes/states to match retained bytes before advancing to the publication authorization boundary.
7. After explicit human authorization, make the verified GitHub Release public. From the release control host only, publish npm from `<STAGE_DIR>/npm-package` (`npm publish --access public --registry https://registry.npmjs.org/`) and verify the requested version/dist-tag; `package.json` also pins the same public registry through `publishConfig`. If publish success is followed by delayed registry visibility, poll rather than publishing twice.
8. Require `.github/workflows/release-image.yml` to finish successfully for the same public `v<VERSION>` Release. It must publish/reconcile `ghcr.io/yyjeqhc/webcodex-server:v<VERSION>` for both Linux architectures, attach the digest metadata plus the self-contained clone-free bootstrap, verify its public SHA-256 and embedded pinned-digest contract, and pass its anonymous GHCR availability gate. For the first-ever package only, make the package Public once and rerun that failed gate; do not rebuild or retag the immutable image merely to change visibility.
9. From one trusted, well-connected Linux verification host, run `python3 scripts/verify_public_release.py <VERSION>`. This remains the single full public-byte acceptance pass for npm plus the six native GitHub archives and, for `0.4.0+`, all three Desktop distribution artifacts; pre-0.4 historical releases remain valid without Desktop. The verifier cross-checks npm manifest / `SHA256SUMS` / GitHub asset digests, validates archive membership and foreign binary architecture without executing foreign binaries, hashes the bounded public Desktop bytes, and rechecks Linux GLIBC/`DT_NEEDED` metadata. It does not attempt to re-run macOS code-signature checks from Linux; native Mac CI/release-build owns the ad-hoc `codesign` and DMG execution evidence. The container workflow independently proves the public GHCR digest/platform contract.

## 10. Post-Deployment Acceptance Smoke

After deploying a new server, Runner, or runtime build:

1. Refresh the GPT Action or MCP schema if runtime tool schemas changed.
2. Run compact `runtime_status`.
3. Run focused tool discovery.
4. Run `list_projects` and pick a Runner-registered project marked appropriate for smoke when available.
5. Run a read-only coding task: `work_on_project`, `read_file` or `search_project_text`, `show_changes(include_diff=false)`, `workspace_hygiene_check`, and `finish_coding_task(summary_only=true)`.
6. Run one small reversible edit task on a safe project and review the diff before accepting it.

Do not run production mutations as acceptance smoke.
