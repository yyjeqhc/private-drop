# repo-info Native Tool Plugin

`repo-info` 是一个很小的只读 first-party Native Tool Plugin，用于 dogfood WebCodex 的 TypeScript Plugin SDK 和 authoring workflow。它只提供一个 `git_summary` 工具，观察对象由 provider 配置中的 `cwd` 精确决定。

仓库中的这份 Plugin 最初由 `webcodex plugin init plugins/repo-info --id repo-info` 真实创建，随后把 scaffold 默认的 published SDK dependency 改为 `file:../../npm/plugin-sdk`，使 repository CI 始终验证当前 checkout，而不依赖 npm registry 可用性。普通外部项目通过 `plugin init` 生成时仍精确依赖公开的 `@yyjeqhc/webcodex-plugin-sdk@0.1.0`。

## 安装、构建和测试

```bash
npm ci
npm run typecheck
npm run build
npm test
```

需要 Node.js 18 或更新版本。实际运行的是编译后的 ESM `dist/plugin.js`。

## 配置一个 Runner

构建 Plugin 后，在目标 Runner 启动时读取的 `runner.toml` 中加入：

```toml
[[plugins.providers]]
id = "repo-info"
name = "Repo Info"
command = "node"
args = ["/absolute/path/to/webcodex/plugins/repo-info/dist/plugin.js"]
cwd = "/absolute/path/to/repository"
timeout_secs = 30
```

`cwd` 就是要观察的 repository，也是 `git_summary` 的 authority boundary；除非你正好要观察 Plugin 源码所在仓库，否则它不应被理解为 Plugin source directory。工具没有 path 参数，也不会自行发现 project、Runner 或其它路径。

`git_summary` 只执行 bounded 的本地 `git --no-optional-locks` observation；不会 fetch、pull、checkout、reset、add、commit、修改 Git config 或访问 remote，也不会返回 absolute cwd、环境变量、credential、remote URL 或 Git config。Git status 超出 Plugin 自身上限时会显式报错，不会静默截断后伪装为完整状态。

## Author loop

```text
webcodex plugin check --runner <runner> --plugin repo-info
webcodex plugin reload --runner <runner>
webcodex plugin list --runner <runner> --plugin repo-info
webcodex plugin describe --runner <runner> --plugin repo-info --tool git_summary
```

实际调用继续使用现有 canonical `plugin_tool describe -> call`；这里刻意没有 `webcodex plugin call`。这个 Plugin 首先是 authoring/dogfood 示例，不是 WebCodex 内建 Git 工具的替代品。