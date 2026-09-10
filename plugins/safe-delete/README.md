# Safe Delete Native Plugin

`safe_delete` is a TypeScript-authored WebCodex Native Tool Plugin built with
`@yyjeqhc/webcodex-plugin-sdk`. It moves one ordinary file or directory to the
operating system Trash/Recycle Bin instead of permanently deleting it.

It is intentionally a Plugin rather than a built-in WebCodex filesystem tool.
Install it only on Runners where you want the model to have this additional
local capability.

## Safety contract

- The Plugin provider `cwd` is the deletion authority root. `path` is always
  relative to that directory.
- Absolute paths, `..` traversal, the authority root itself, paths that resolve
  outside the root, symlinks/junctions, and non-file/non-directory entries fail
  closed.
- One call handles exactly one path. There is no batch mode and no `force`
  option.
- An already-absent path is a successful no-op, but the tool is deliberately
  **not** declared idempotent: after an uncertain result, a new object may have
  been created at the same relative path. Never retry an unknown result blindly.
- Backend timeout, a backend failure after the original path disappears, or an
  unverifiable postcondition returns `outcome=unknown`; inspect the path and
  Trash before retrying.
- There is **no permanent-delete fallback for the requested path**. The Plugin never
  uses `rm`, `unlink`, `Remove-Item`, or an equivalent operation to dispose of
  the requested file or directory.

This protects against ordinary model/operator path mistakes. It is not a
sandbox against a hostile process racing filesystem names while the Trash API
is running.

## Platform backends

| Platform | Backend |
| --- | --- |
| Linux | freedesktop.org Home Trash (`$XDG_DATA_HOME/Trash`) using same-filesystem atomic rename; then `gio trash`; then `trash-put` only when `gio` is not installed |
| macOS | Foundation `NSFileManager` Trash API through built-in JXA (`/usr/bin/osascript -l JavaScript`), with the path passed as argv |
| Windows | PowerShell + `Microsoft.VisualBasic.FileIO` with `SendToRecycleBin` |

If no supported backend is available, the operation fails without deleting the
path. The built-in Linux backend deliberately refuses cross-filesystem copy +
unlink; it falls through to another Trash backend instead.

## Runner configuration

Set `cwd` to the exact project/root you want this Plugin to be allowed to trash
from. Build the TypeScript Plugin before check/reload, then configure the Runner
to execute `dist/plugin.js`. Use an absolute path when the Plugin code lives
outside the authority root:

```toml
[plugins]
request_timeout_secs = 30

[[plugins.providers]]
id = "safe-delete"
name = "Safe Delete"
command = "node"
args = ["/absolute/path/to/webcodex/plugins/safe-delete/dist/plugin.js"]
cwd = "/absolute/path/to/project"
timeout_secs = 30
```

During development, use the normal Plugin loop:

```text
plugin_tool(action="check", runner="my-runner", plugin="safe-delete")
-> plugin_tool(action="reload", runner="my-runner")
-> plugin_tool(action="list", runner="my-runner", plugin="safe-delete")
-> plugin_tool(action="describe", runner="my-runner", plugin="safe-delete", tool="safe_delete")
-> plugin_tool(action="call", binding="wc_pbind_...", arguments={"path":"build/old-output.bin"})
```

No Runner restart is required for Plugin discovery after a successful reload.
`safe_delete` remains provider-local and never becomes an outer MCP tool name.

## Tool

Call it only through the opaque binding returned by `plugin_tool describe`; do
not call outer MCP `tools/call(name="safe_delete")`.

Possible structured outcomes are `trashed`, `already_absent`, `rejected`,
`failed`, and `unknown`.

## Development

The SDK is consumed from this repository through a local `file:` dependency; no
workspace/package-manager layer is required. From the repository root:

```bash
npm ci --prefix npm/plugin-sdk
npm --prefix npm/plugin-sdk run build
npm ci --prefix plugins/safe-delete
npm --prefix plugins/safe-delete run typecheck
npm --prefix plugins/safe-delete test
```

The Plugin implementation is TypeScript-only. Its domain module owns path authority,
Trash backend selection, and result classification; the SDK entrypoint owns only
authoring/runtime boilerplate. The Rust Runner remains authoritative for Plugin
admission, schema/runtime validation, timeout, process lifecycle, and
`OutcomeUnknown` handling.
