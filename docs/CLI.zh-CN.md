# WebCodex CLI

`webcodex` 是统一的操作与开发命令行。它覆盖项目设置、Server 与 Runner 生命周期、
设备接入、令牌管理和只读运维检查。

远程操作（用户、令牌、pairing、运维检查）走 Server HTTP API，CLI 是它们的便捷
客户端；本地操作（项目设置、服务管理、task 审查决策）直接运行在主机上，无法通过
Server API 完成。

从源码构建会产生三个二进制：

- `webcodex` —— 本文档介绍的统一命令。
- `webcodex-server` —— Server 进程（用 `webcodex server ...` 启动与托管）。
- `webcodex-runner` —— 实际执行项目工作的 Runner 进程（用
  `webcodex runner ...` 启动与托管）。

`webcodex --help` 会列出顶层命名空间。下面按命名空间说明各自的用途。本文是完整 CLI reference，不要求普通用户在第一次成功使用前理解所有命令、凭据或内部配置字段。

日常使用推荐按[完整使用指南](PERSONAL_SETUP.zh-CN.md)建立普通 Server + Runner。`webcodex share` 则是 Linux、macOS、Windows 上用于**临时试用/分享一个项目**的显式入口；它会为本次前台运行准备临时项目环境、Server、Runner 和可选 Tunnel，退出即结束。Linux/macOS 在交互式 Git checkout 中运行裸 `webcodex` 自动进入 `share`，也只是这个临时试用的 convenience shortcut。

## 命令总览

### 项目 / 本地工作流

以下命令作用于当前 Git 项目。

| 命令 | 用途 | 说明 |
| --- | --- | --- |
| `webcodex`（无子命令） | 临时分享的交互式快捷方式 | 仅 Linux/macOS + stdin/stdout 为终端 + 当前目录位于 Git checkout 时自动进入 `share`；否则照常显示 help。 |
| `webcodex share` | 临时把当前项目接入 ChatGPT/MCP | Linux/macOS/Windows 的快速试用/短期分享路径；包含临时 setup、本地 Server + Runner、`cloudflare|openai|none` 和有界前台 cleanup。日常完整使用见 `PERSONAL_SETUP`。 |
| `webcodex connect <server>` | 把当前项目接入已有的 Server | 已经拥有 Server URL 时的长期路径；默认使用 hosted shared-key。 |
| `webcodex status` | 简洁的项目 coding 就绪状态 | 简短状态；`doctor` 提供完整诊断。 |
| `webcodex doctor` | 当前项目的只读就绪检查 | 诊断/手动工作流；输出稳定的 `next action`。 |
| `webcodex setup` | 只配置当前 Git 项目，不启动 runtime | local-only/手动工作流；创建私有状态与 Project Credential。 |
| `webcodex run` | 启动 project-bound loopback Server 与本地 Runner | local-only/手动工作流；前台运行，Ctrl-C 同时停止两者。 |
| `webcodex disconnect [--project PATH] [--profile NAME]` | 移除一个 hosted 项目注册 | 是该仓库 `connect` 的精确逆操作；绝不删除仓库或 `.git`。 |

`webcodex share --auth query-token` 是给无法配置 Bearer header 的 MCP client 使用的显式临时 share 兼容模式。它只在 `/mcp?token=...` 接受当前 share 的精确 Project Credential，输出经过 URL 编码的敏感 MCP URL，并让 client 选择 No authentication。普通 Server/runtime 请求不会因此启用 query auth，PAT/OAuth/shared-key/Runner credential 也不能通过这个 query 路径回退认证；`--tunnel openai` 会拒绝该模式。整条 URL 都必须当作 credential，因为 query 可能被 client、proxy、剪贴板或 access log 留存。默认仍是 `--auth bearer`。

`webcodex share --auth oauth --oauth-redirect-uri <精确回调地址>` 使用 OAuth 2.0 Authorization Code + PKCE S256。OAuth client ID/secret 会按“项目 + 回调地址”保存在受保护的 project state 中；临时 OAuth grant 只在当前 `share` 运行期间有效。重启 `share` 会让旧 OAuth grant 失效，但不会改变项目。OAuth access token 永远不能用于 Runner transport。

Cloudflare Quick Tunnel 的公网 origin 仍然是临时的。如需稳定 HTTPS origin，可使用 `--tunnel none --public-url https://share.example`，并由 operator 自己把该 origin 反向代理/隧道到 loopback WebCodex Server；`--public-url` 只声明外部 origin/issuer，不会创建代理或 tunnel。

`webcodex share --tunnel openai` 是显式 opt-in 的 OpenAI Secure MCP Tunnel provider。它要求 `CONTROL_PLANE_TUNNEL_ID` 与只授予 Tunnels Read + Use 的 Restricted `CONTROL_PLANE_API_KEY`，当前只支持 `--auth bearer`。WebCodex 会从 `WEBCODEX_TUNNEL_CLIENT_BIN`、`PATH` 或经过校验的 managed 下载解析固定 OpenAI `tunnel-client` v0.0.12；启动 daemon 前运行 `doctor`，并等待 `/readyz`。临时 WebCodex Bearer 只写入私有 share 目录，通过 file-backed MCP `Authorization` header 交给 `tunnel-client`，因此 ChatGPT 使用 Connection: Tunnel + No authentication。长驻 daemon 环境会显式移除 `OPENAI_ADMIN_KEY` 与 `OPENAI_API_KEY`；Runtime API key 仍只承担 control-plane authority。

公网 `share` 会 best-effort 复制 MCP URL；默认 Bearer/OAuth 模式仍不会自动把临时 credential 复制进剪贴板。显式 `--auth query-token` 模式则按设计复制含临时 credential 的敏感 URL，并在状态输出中明确提示。Linux/macOS 交互式终端还会提供按 Enter 打开 ChatGPT App 设置的快捷入口。剪贴板/浏览器集成都只是 convenience，失败不会影响已经 ready 的 runtime。使用 `--no-copy-url` 可关闭剪贴板访问。

面向受监督的 machine integration，可使用 `webcodex share --json --stop-on-stdin-eof`。它仍保持原有前台生命周期，但会把 supervising parent 关闭 stdin 视为停止请求，使 Desktop 或其他 structured process owner 可以让 `share` 自己清理临时 Server、Runner 与 Tunnel，而不需要拼 shell signal 命令。该 flag 在非 `--json` 模式下会被拒绝。

`webcodex connect <server> --auth oauth --oauth-redirect-uri <精确回调地址>` 是普通 hosted OAuth 路径。Runner 保持原有 hosted credential，MCP client 使用 OAuth。只有真正需要额外能力时才增加 `--oauth-computer-permissions`、`--oauth-local-mcp` 或 `--oauth-local-ssh`；它们属于显式权限变更，可能要求重新授权。`--oauth-local-ssh` 会授予 MCP client 使用模型侧 `ssh_resource` 接入工具所需的可选 `ssh:local` authority；它不会暴露 SSH credential，也不会绕过工具返回的 Runner restart requirement 让新资源立即生效。Client 设置见 [MCP](MCP.zh-CN.md#oauth2)，安全模型见[认证](AUTH_MODEL.zh-CN.md#oauth2)。

高级 managed identity 流程仍保留为 `--auth managed-oauth --oauth-redirect-uri <精确回调地址>`，它才要求先 `webcodex login`；`--user` 也只用于该模式。

`disconnect` 按 canonical 仓库路径匹配，不根据 basename 或 project id 猜测。如果同一仓库
注册在多个 hosted profile 中，必须显式指定 `--profile`。managed Runner 在线时，它先执行
structured unregister，再删除本地 registration；Runner 已停止时，只删除精确
匹配的本地项目 registration。其他项目、profile credential 和 `runner.toml` 都会保留。

接入 MCP coding client 后，可阅读 [Coding 工作流](CODING_WORKFLOW.zh-CN.md)，了解 canonical
`work_on_project` model bootstrap、behavioral guidance、validation 与 closeout evidence。

### 设备接入

| 命令 | 用途 | 说明 |
| --- | --- | --- |
| `webcodex login <server-url> --code <wc_pair_...> [--project PATH]` | 用一次性登录码把本机接入 Server | 普通 managed 接入入口；`--project` 选择实际项目，`--allowed-root` 指定以后允许添加项目的父目录；`--print-mcp-config` 可显式打印敏感的 ChatGPT MCP 连接信息。 |
| `webcodex project register --config PATH <PROJECT>` | 给现有 Runner 再添加一个项目 | 写入该 Runner 的项目配置；不要求 Server 在线即可持久化，运行中的 Runner 是否需 reload 以命令输出为准。 |
| `webcodex pairing create` | Server/admin 侧：创建短期 pairing code | 需要 server bootstrap/admin 认证。 |
| `webcodex logout <server-url> [--user USER|--all]` | 移除本机对某 Server 的凭据 | 只有一个 saved user 时自动选择；多个 saved user 时必须用 `--user USER` 选择一个，或显式用 `--all` 选择全部；真正删除仍遵守现有 confirmation/`--yes` 流程。 |

本地 `login --project`、`project register` 与 Desktop picker 显式选择现有项目时，
会自动持久化 exact canonical root 授权，不扩大到父目录或整个共享目录。
需要新授权的 symlink 路径须显式选择 canonical 目标；危险 Windows namespace 与
`..` traversal 仍拒绝。模型注册入口仍受既有 Runner policy 约束，UNC 仍先检查网络授权。
已运行 Runner 若需要 reload，按命令输出处理。

root login 会给出可直接运行的 foreground 命令，并提示项目命令拥有 root 权限；
Linux system service 安装命令明确包含 `--allow-root-runner`，无需重新 login。

### Runner 生命周期

Runner 可执行文件是 `webcodex-runner`。其规范 CLI 生命周期命名空间是 `runner`：
`webcodex runner ...` 管理 `webcodex-runner` 进程与服务。`webcodex` 与
`webcodex-runner` 是两个独立可执行文件。

| 命令 | 用途 |
| --- | --- |
| `webcodex runner init` | 手动生成 `runner.toml` 配置 |
| `webcodex runner install` | 安装、启用并启动 Runner 服务 |
| `webcodex runner run` | 前台运行 `webcodex-runner` |
| `webcodex runner start` | 启动 hosted 后台 Runner 或已安装的 profile 服务 |
| `webcodex runner stop` | 停止 |
| `webcodex runner restart` | 重启 |
| `webcodex runner status` | 检查 Runner 生命周期、配置与连通性 |
| `webcodex runner logs` | 读取 Runner 日志（有界） |
| `webcodex runner uninstall` | 移除服务单元（需要 `--confirm`） |

服务命令接受 `--scope user|system`。非 root 用户默认 user scope；root 默认
system scope。`webcodex connect` 创建的 profile 在不传 `--scope` 时保持其
detached-process 行为。

### Server

| 命令 | 用途 |
| --- | --- |
| `webcodex server init` | 初始化/更新 Server env 文件与所选 data directory（创建 bootstrap token） |
| `webcodex server install` | 安装 Linux systemd `webcodex.socket` + `webcodex.service` pair；默认 WorkingDirectory 跟随所选 env 的 `WEBCODEX_DATA` |
| `webcodex server run [--env-file PATH]` | 前台运行 `webcodex-server`（direct bind）；`--env-file` 通过 `WEBCODEX_ENV_FILE` 精确传递路径 |
| `webcodex server start` / `stop` | 一致地启动或停止 socket activation 与 Server process |
| `webcodex server restart` | 只 restart Server process，保持受管 listener socket active |
| `webcodex server status` | 检查 authoritative socket/service 状态、HTTP 可达性与构建版本 |
| `webcodex server logs` | 读取 Server service journal |
| `webcodex server uninstall` | stop/disable/remove 受管 socket/service pair |

Windows 支持 `server init`、前台 `server run` 与显式 `share`。受管 service 生命周期（`install`、`start`、`stop`、`restart`、`logs`、`uninstall`）仍只支持 Linux。

使用 `webcodex server install --service-file /path/name.service` 时，会派生同目录的
`/path/name.socket`。后续 `start`、`stop`、`restart`、`status`、`logs` 和 `uninstall`
应传入相同的 `--service-file` 来管理或检查该自定义 pair；省略时仍操作默认的
`webcodex.service` / `webcodex.socket` pair。

Runner 配置术语中，`project_registry_dir` 是 Project registry TOML 文件目录，不是 workspace root；`[policy].allowed_roots` 只限制哪些文件系统路径可以注册，Project record 才指向实际 workspace。

### 运维（只读操作检查）

| 命令 | 用途 |
| --- | --- |
| `webcodex ops status` | 汇总 runtime、工具、任务、Runner 与项目 |
| `webcodex ops runners` | 简洁的 Runner fleet 状态 |
| `webcodex ops runner --client-id <id>` | 精确读取单个 Runner 的注册与构建状态 |
| `webcodex ops projects` | 项目清单与 smoke 适用性 |
| `webcodex ops smoke-preflight --project <id>` | 为某项目做 deploy smoke 预检 |

`ops` 命令只读。支持 `--server-url`、`--token-file`、`--env-file`、`--token`、
`--json` 与 `--strict`。优先使用 `--token-file`；`--token` 容易泄露到 shell
历史或进程列表。`--strict` 会让 FAIL 报告以状态码 2 退出。

### 审查（本地决策）

| 命令 | 用途 |
| --- | --- |
| `webcodex task list` | 列出当前项目的近期任务 |
| `webcodex task show <id>` | 显示任务的结果、审批与时间线 |
| `webcodex task accept <id>` | 把已审查的结果应用到 checkout |
| `webcodex task reject <id> [reason]` | 拒绝稳定结果；reason 会到达模型 |
| `webcodex task resume <id>` | runtime 重启后恢复保留的运行 |
| `webcodex task guide <id> <message>` | 向运行中的任务发送纠偏指引 |
| `webcodex task approve <id> <approval> [reason]` | 批准某条原始命令使用一次 |
| `webcodex task deny <id> <approval> [reason]` | 拒绝；reason 会显示给模型 |
| `webcodex task activity` | 显示近期变更性工具执行（workspace ledger） |

`task` 命令默认作用于当前项目；可用 `--root PATH`、`--profile NAME` 或
`--state-dir PATH` 指向其他项目。Accept 与 Reject 是人工在本地应用或丢弃 coding
结果的两种方式——在线模型永远不能接受自己的工作。

### 凭据与账号

Admin 用户/令牌操作由 Server API 支撑；`auth status` 读取本机连接状态，而
`create-local` 命令在本地生成凭据，只向 Server 注册其 hash。

| 命令 | 用途 | 说明 |
| --- | --- | --- |
| `webcodex auth status` | 显示本机已登录哪些 Server | 只读；支持 `--dir` 与 `--json`。 |
| `webcodex users create` | 创建用户；`--issue-credential` 返回一次性 account credential | Server/admin 侧；使用 `--server-url`。 |
| `webcodex users list` | 列出用户 | |
| `webcodex tokens create-local` | 本地生成 `wc_pat_*` 个人 API 令牌并注册其 hash | 使用 `--server-url`、`--username` 与 account credential。 |
| `webcodex tokens create` | Admin：在服务端创建 PAT | 使用 `--server-url`。 |
| `webcodex tokens generate` | 离线生成令牌素材 | **不会**在 Server 注册。 |
| `webcodex tokens list` / `revoke` / `register-hash` | 列出或撤销 PAT；注册外部计算的 hash | Admin 侧；使用 `--server-url`。 |
| `webcodex runner-tokens create-local` | 本地生成 `wc_agent_*` Runner 令牌并注册其 hash | 使用 `--server-url` 并绑定 `--client-id`。 |
| `webcodex runner-tokens create` / `list` / `revoke` / `register-hash` | Admin 变体 | |

所有面向 Server 的 credential 命令统一使用 canonical `--server-url`。
本地 `tokens create-local` / `runner-tokens create-local` 使用 `--username` 与 account
credential；admin token management 也使用相同的 plural namespace。

### 高级与兼容命令

以下命令覆盖不常见场景；上面的推荐路径才是常规入口。

| 命令 | 用途 | 说明 |
| --- | --- | --- |
| `webcodex pairing create` | Server/admin 侧：创建短期 pairing code | 需要 server bootstrap/admin 认证。 |
| `webcodex tokens generate` | 离线生成令牌素材 | 不注册任何东西；若需要服务端注册 hash，把输出配 `tokens register-hash` 使用。 |
| `webcodex tokens register-hash` | Admin：注册外部计算的 PAT hash | 使用 `--server-url`；用于离线生成的素材。 |
| `webcodex runner-tokens register-hash` | Admin：注册外部计算的 Runner 令牌 hash | 使用 `--server-url`；用于离线生成的素材。 |

## 术语

- **Server** —— 认证调用方、保存共享 runtime 状态并路由工作。
- **Runner** —— 在持有代码的机器上执行仓库工作。
- **Project** —— 由 Runner 注册的一个仓库/工作区。
- **Task** —— 一个可审查的有界 project-first 工作单元。
- **Job** —— 发起调用返回后仍继续运行的命令或 validation。
- **Workflow Session** —— runtime 用于 coding evidence/continuity 的有界状态。普通用户通常不需要管理其内部协议字段。

部分兼容名称仍保留 `agent`，主要是 `wc_agent_*` 与 `agent:<client_id>:<project_id>`。它们属于 Runner 时代的兼容名称，不是独立 Durable Agent domain；其它 process/protocol identifier 继续留在内部。新文档除引用这些公开名称外应统一写 **Runner**。

## 凭据：我到底需要哪个令牌？

WebCodex 把 bootstrap 管理、账号接入、runtime API 访问与 Runner 连接分开。
不要跨 surface 复用同一凭据。完整模型见
[AUTH_MODEL.md](AUTH_MODEL.zh-CN.md)；下表是快速答案。

| 凭据 | 前缀 | 由谁创建 | 用途 | 不要用于 |
| --- | --- | --- | --- | --- |
| Server bootstrap token | （env `WEBCODEX_TOKEN`） | `webcodex server init` | server/admin 设置、建用户、pairing | GPT Actions、MCP、Runner、日常使用 |
| 共享 key | `wck_...` | `webcodex connect`（一次性生成） | hosted shared-key 的 MCP + Runner | 生产 IAM |
| Project Credential | （私有文件） | `webcodex setup` | 单个项目的 Connector + Runner | 其他项目、admin |
| Account credential | `wc_acct_...` | `webcodex users create --issue-credential` | 本地创建令牌 | GPT Actions、MCP、Runner |
| 个人 API 令牌（PAT） | `wc_pat_...` | `webcodex tokens create-local` | GPT Actions、MCP、REST API | Runner 连接 |
| Runner 令牌 | `wc_agent_...` | `webcodex runner-tokens create-local` | 仅 `webcodex-runner` 传输 | MCP、REST、GPT Actions |
| OAuth 访问令牌 | `wc_oat_...` | OAuth2 授权流程 | 启用 OAuth 时的 GPT Actions / MCP | — |

### 实际使用规则

- 普通 managed setup：`webcodex login` 会创建本地 user/API 与 Runner 凭据；直接使用它报告的路径和 MCP 连接值。
- 已有 shared-key Server：使用 operator 提供的 `wck_...` 配合 `webcodex connect`。
- Project-first/manual setup：Project Credential 只留在受保护的项目私有状态里，不要当作通用 user/admin token。
- `WEBCODEX_TOKEN` 只留在 Server；它不是 MCP 或 Runner 凭据。
- `wc_agent_*` 只用于 Runner transport；`wc_pat_*` 才是普通 managed user API token。
- 优先使用 `--token-file`，不要把完整配置文件粘贴进聊天。
- OAuth client 应走 OAuth flow，而不是人工复制 access token。见[认证](AUTH_MODEL.zh-CN.md#oauth2)与 [MCP](MCP.zh-CN.md#oauth2)。

## 常用示例

日常完整使用：先按[完整使用指南](PERSONAL_SETUP.zh-CN.md)启动普通 Server，再在项目机器上完成一次性登录并启动 Runner：

```bash
webcodex login https://your-server.example --code <wc_pair_...> \
  --allowed-root "$HOME/git" \
  --project "$HOME/git/my-repo" \
  --print-mcp-config
webcodex runner run --config <login-reported-runner-config>
```

只想临时试用一个仓库：

```bash
cd /path/to/your/repository
webcodex share
```

Local/manual project-bound 工作流（高级/诊断）：

```bash
webcodex setup
webcodex doctor
webcodex run          # 保持该终端打开
webcodex status       # 在另一个终端
webcodex task list
webcodex task show <task-id>
webcodex task accept <task-id>
```

已有 hosted Server：

```bash
webcodex connect https://your-server.example
webcodex runner status --profile <profile>
webcodex runner logs --profile <profile> --lines 100
```

Linux 上把已经验证过的 Runner 改为 user service：

```bash
webcodex runner install --scope user --config <login-reported-runner-config>
webcodex runner status --scope user --config <login-reported-runner-config>
webcodex ops status --server-url https://your-server.example \
  --token-file <login-reported-webcodex-user-token> --strict
```

## 代理与网络

CLI 请求默认遵循标准代理环境变量（`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、
`NO_PROXY`）。用 `--proxy http://HOST:PORT` 为单次调用覆盖，或用
`--no-system-proxy` 忽略代理环境直连。这些 flag 只影响 CLI 自身的 HTTP 请求；
`webcodex connect` 不会把它们持久化或注入 Runner 配置。
