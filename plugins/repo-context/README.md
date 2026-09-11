# repo-context Native Tool Plugin

`repo-context` is a small read-only first-party experiment for Native Plugin composition. It was created with the real `webcodex plugin init plugins/repo-context --id repo-context` authoring flow, then its repository dependency was switched to `file:../../npm/plugin-sdk` so CI validates the current checkout without changing the public `@yyjeqhc/webcodex-plugin-sdk@0.1.0` contract.

It exposes exactly one tool, `repo_context`, with an empty input object. The provider's configured `cwd` is the only repository authority boundary; callers cannot supply a path, project, repository, Runner, command, or environment override.

## Output

The bounded structured result contains Git branch/HEAD/detached/dirty state, staged/unstaged/untracked counts, up to 128 changed paths, Cargo availability, up to 128 workspace member names, up to 128 advisory affected package names, at most 16 warnings, and `elapsedMs`.

`affectedPackages` is deliberately advisory, not dependency-impact or validation evidence. Files under a concrete workspace package root map to the deepest package root. Root/shared files such as `Cargo.toml` conservatively mark all observed workspace packages affected. Paths outside Cargo package roots remain unmapped rather than being attributed to the root crate merely because its manifest lives at `.`.

The Plugin never returns a Git diff, complete manifests, raw `cargo metadata`, absolute provider paths, remote URLs, credentials, validation verdicts, or merge/safety claims.

## Local execution boundary

Git uses `execFile` with literal argv, `shell: false`, `windowsHide: true`, timeouts, and bounded buffers. It uses only local read-only observations (`rev-parse` and porcelain `status`) and never fetches or mutates the repository.

Cargo uses `cargo metadata --offline --no-deps --format-version 1`. `--offline` makes the no-network experiment boundary explicit. If Cargo is missing, the root is not a Cargo workspace, metadata is malformed, or the command times out/fails, Git context is still returned and the Cargo portion becomes unavailable with a bounded warning.

## Install, build, and test

```bash
npm ci
npm run typecheck
npm run build
npm test
```

Node.js 18 or newer is required. Production execution uses the compiled ESM `dist/plugin.js`.

## Configure one Runner

```toml
[[plugins.providers]]
id = "repo-context"
name = "Repo Context"
command = "node"
args = ["/absolute/path/to/webcodex/plugins/repo-context/dist/plugin.js"]
cwd = "/absolute/path/to/repository"
timeout_secs = 30
```

`cwd` is the repository being observed and is intentionally distinct from the Plugin source directory when necessary.

## Author loop

```text
webcodex plugin check --runner <runner> --plugin repo-context
webcodex plugin reload --runner <runner>
webcodex plugin list --runner <runner> --plugin repo-context
webcodex plugin describe --runner <runner> --plugin repo-context --tool repo_context
```

Invocation remains on the canonical `plugin_tool describe -> call` binding path. There is intentionally no `webcodex plugin call` command and no outer MCP tool is added for `repo_context`.
