# Safe Delete Native Plugin

`safe_delete` 是一个使用 `@yyjeqhc/webcodex-plugin-sdk` 编写的 TypeScript WebCodex
Native Tool Plugin。它不会永久删除文件，而是把一个普通文件或目录移动到操作系统的
Trash / Recycle Bin。

它有意作为 Plugin 提供，而不是加入 WebCodex 内建文件系统工具。只有在你确实希望
模型获得这项额外本机能力的 Runner 上才安装它。

## 安全契约

- Plugin provider 的 `cwd` 就是删除权限根；`path` 永远相对于这个目录解析。
- 绝对路径、`..` traversal、权限根本身、解析后逃出权限根的路径、symlink/junction，
  以及普通文件/目录以外的对象全部 fail closed。
- 每次调用只处理一个路径；没有 batch，也没有 `force`。
- 目标已经不存在时返回成功 no-op，但整个工具会明确标记为**非幂等**：未知结果之后，
  同一个相对路径可能已经出现了新的对象，因此不能盲目重试。
- backend timeout、backend 报错但原路径已经消失，或无法验证最终状态时返回
  `outcome=unknown`；再次调用前应先检查路径和回收站。
- **绝不对请求目标 fallback 到永久删除**。Plugin 不会用 `rm`、`unlink`、
  `Remove-Item` 或等价操作处置用户要求删除的文件或目录。

这个边界用于降低模型或操作者拼错路径造成的事故风险；它不是对抗恶意进程并发修改
文件系统名称的 sandbox。

## 平台 backend

| 平台 | Backend |
| --- | --- |
| Linux | 先按 freedesktop.org Home Trash（`$XDG_DATA_HOME/Trash`）做同文件系统 atomic rename；再尝试 `gio trash`；只有系统未安装 `gio` 时才尝试 `trash-put` |
| macOS | 通过系统内置 JXA（`/usr/bin/osascript -l JavaScript`）直接调用 Foundation `NSFileManager` Trash API，目标路径作为独立 argv 传入 |
| Windows | PowerShell + `Microsoft.VisualBasic.FileIO` 的 `SendToRecycleBin` |

没有可用的安全回收站 backend 时，操作会失败，不会改成永久删除。内建 Linux backend
明确拒绝用跨文件系统 copy + unlink 模拟回收站，而是继续尝试其他 Trash backend。

## Runner 配置

把 `cwd` 设置为你希望这个 Plugin **唯一有权移动到回收站**的项目/目录。在
check/reload 前先构建 TypeScript Plugin，然后让 Runner 直接执行 `dist/plugin.js`。
如果 Plugin 代码不在这个权限根下，使用构建产物的绝对路径：

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

开发阶段使用标准 Plugin 闭环：

```text
plugin_tool(action="check", runner="my-runner", plugin="safe-delete")
-> plugin_tool(action="reload", runner="my-runner")
-> plugin_tool(action="list", runner="my-runner", plugin="safe-delete")
-> plugin_tool(action="describe", runner="my-runner", plugin="safe-delete", tool="safe_delete")
-> plugin_tool(action="call", binding="wc_pbind_...", arguments={"path":"build/old-output.bin"})
```

成功 reload 后不需要重启 Runner 才能发现/调用 Plugin。`safe_delete` 始终是
provider-local Tool，不会成为外层 MCP tool name。

## Tool

只能使用 `plugin_tool describe` 返回的 opaque binding 调用它；不要直接调用外层 MCP
`tools/call(name="safe_delete")`。

结构化结果可能是 `trashed`、`already_absent`、`rejected`、`failed` 或 `unknown`。

## 开发

SDK 通过仓库内 `file:` dependency 使用，不需要引入新的 workspace/package-manager
架构。从仓库根目录执行：

```bash
npm ci --prefix npm/plugin-sdk
npm --prefix npm/plugin-sdk run build
npm ci --prefix plugins/safe-delete
npm --prefix plugins/safe-delete run typecheck
npm --prefix plugins/safe-delete test
```

Plugin 实现现在完全使用 TypeScript。domain module 负责路径权限、Trash backend
选择与结果分类；SDK entrypoint 只承载 authoring/runtime boilerplate。Rust Runner 继续
权威负责 Plugin admission、schema/runtime validation、timeout、process lifecycle 与
`OutcomeUnknown` 处理。
