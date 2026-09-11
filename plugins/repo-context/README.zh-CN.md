# repo-context Native Tool Plugin

`repo-context` 是一个很小的只读 first-party Native Plugin composition 实验。它先通过真实的 `webcodex plugin init plugins/repo-context --id repo-context` authoring flow 创建，再把仓库内 dependency 调整为 `file:../../npm/plugin-sdk`，从而让 CI 验证当前 checkout，同时不改变公开的 `@yyjeqhc/webcodex-plugin-sdk@0.1.0` contract。

它只暴露一个 `repo_context` 工具，input 是空对象。provider 配置中的 `cwd` 是唯一 repository authority boundary；调用者不能传入 path、project、repository、Runner、command 或 environment override。

## 输出

bounded structured result 包含 Git branch/HEAD/detached/dirty 状态、staged/unstaged/untracked 计数、最多 128 个 changed paths、Cargo availability、最多 128 个 workspace member 名称、最多 128 个 advisory affected package 名称、`globalChange`、最多 16 条 warning，以及 `elapsedMs`。

`affectedPackages` 明确只是 advisory context，不是 dependency impact analysis，也不是 validation evidence。位于明确 workspace package root 下的文件映射到最深 package root；`Cargo.toml` 等 root/shared change 会保守地标记全部已观察 workspace packages，并令 `globalChange=true`；位于 Cargo package roots 之外的路径不会因为 root crate 的 manifest 位于 `.` 就被误归到 root crate。

Plugin 不返回 Git diff、完整 manifest、raw `cargo metadata`、provider 绝对路径、remote URL、credential、validation verdict，也不会给出 verified/safe-to-merge 之类结论。

## 本地执行边界

Git 通过 `execFile` 和 literal argv 执行，固定 `shell: false`、`windowsHide: true`，并设置 timeout 与 bounded buffer。它只做本地只读的 `rev-parse` 和 porcelain `status`，不会 fetch，也不会修改 repository。

Cargo 使用 `cargo metadata --frozen --no-deps --format-version 1`，其中 `--frozen` 同时明确保证本实验不访问网络、也不生成或更新 lockfile。如果 Cargo 不存在、provider cwd 不是 Cargo workspace、metadata malformed、timeout 或失败，Git context 仍会返回，Cargo 部分则通过 bounded warning 表示 unavailable。

## 安装、构建与测试

```bash
npm ci
npm run typecheck
npm run build
npm test
```

需要 Node.js 18 或更新版本；实际 provider 运行编译后的 ESM `dist/plugin.js`。

## 配置一个 Runner

```toml
[[plugins.providers]]
id = "repo-context"
name = "Repo Context"
command = "node"
args = ["/absolute/path/to/webcodex/plugins/repo-context/dist/plugin.js"]
cwd = "/absolute/path/to/repository"
timeout_secs = 30
```

`cwd` 就是被观察的 repository；必要时它与 Plugin source directory 完全不同。

## Author loop

```text
webcodex plugin check --runner <runner> --plugin repo-context
webcodex plugin reload --runner <runner>
webcodex plugin list --runner <runner> --plugin repo-context
webcodex plugin describe --runner <runner> --plugin repo-context --tool repo_context
```

实际 invocation 继续使用 canonical `plugin_tool describe -> call` opaque binding 路径；这里不会新增 `webcodex plugin call`，也不会把 `repo_context` 暴露成 outer MCP root tool。
