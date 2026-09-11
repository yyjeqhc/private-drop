# MCP

[English](MCP.md) | [简体中文](MCP.zh-CN.md)

WebCodex 通过 MCP endpoint，让 ChatGPT、Claude 与其他 MCP client 使用持有仓库机器上的 Runner。普通用户先选择**完整使用**还是**临时试用**即可；protocol surface、scope、credential taxonomy 都属于 reference，不是 onboarding 前置知识。

## ChatGPT：推荐的完整使用方式

日常使用推荐普通 Server + Runner。按照[完整使用指南](PERSONAL_SETUP.zh-CN.md)完成一次性登录和项目注册，并在同一次 `webcodex login` 中使用 `--print-mcp-config` 获取普通 HTTPS MCP 连接信息。公网 HTTPS、Cloudflare Tunnel 或 OpenAI Secure MCP Tunnel 只负责到达 Server，不改变这条完整开发路径本身的能力。

如果只是临时试用一个仓库，再使用下面的 `share` 路径。

## ChatGPT：临时 `share`

显式 `share` 支持 Linux、macOS 与 Windows，并由当前前台进程持有临时单项目环境。Windows x64 可直接使用 managed 默认 Cloudflare Quick Tunnel；固定版本 Cloudflare 没有官方 Windows ARM64 artifact，因此 ARM64 需要受信任的显式/`PATH` `cloudflared`。managed OpenAI `tunnel-client` 支持 Windows x64/arm64。

默认临时公网路径会复用显式指定/`PATH` 中的 `cloudflared`，否则由 WebCodex 自动下载并校验固定的 managed 副本，然后执行：

```bash
npm install -g @yyjeqhc/webcodex
cd /path/to/your/repository
webcodex share
```

CLI 显示 **WebCodex ready** 后：

1. 在 ChatGPT Developer Mode 创建基于 MCP 的 custom app。
2. 填入输出的 **MCP URL**。
3. 默认 share 选择 **Access token / API key**（Bearer token）。
4. 填入输出的临时 **Credential**。
5. 点击 **Scan Tools**。
6. 第一条可先说：`检查这个仓库并总结它的结构。先不要做任何修改。`

`share` 自己会完成 project setup。Hosted ChatGPT 无法访问 loopback-only 的
`webcodex run`，因此 `setup`、`doctor`、`run` 都不是 `share` 之前的必经步骤。ChatGPT
UI 文案可能随 rollout 变化；URL 与认证以 CLI 输出为准。Developer Mode、custom MCP app
和 write/modify action 是否可用，还分别受 ChatGPT 套餐、workspace 与管理员设置控制；
WebCodex scope 不会扩大这些客户端侧权限。

## Claude 与其他 MCP client

使用同一份输出的 `/mcp` URL 与认证值。Claude 中添加 custom connector 并粘贴 MCP URL；
其他 MCP client 同样使用 CLI 报告的 endpoint 与认证方式。仅本地 client 可用
`webcodex share --tunnel none`，不需要 `cloudflared`。

如果 client 无法设置 Bearer header，可显式使用 `webcodex share --auth query-token`：
粘贴输出的敏感 `/mcp?token=...` URL，并选择 No authentication。这个 query 只接受当前
share 的 Project Credential，并不是 PAT/OAuth/shared-key 的通用 query auth。整条 URL
都必须当作 secret，因为 URL query 可能进入日志。

如果只需要 OpenAI 产品的私有 transport，创建/选择 Secure MCP Tunnel，导出
`CONTROL_PLANE_TUNNEL_ID` 与只授予 Tunnels Read + Use 的 Restricted
`CONTROL_PLANE_API_KEY`，然后运行 `webcodex share --tunnel openai`。ChatGPT 使用
Connection: Tunnel + No authentication；临时 WebCodex Bearer 留在本机，由固定且经过校验的
OpenAI `tunnel-client` 注入。

如果在 Windows 上使用普通独立 Server + Runner 并通过 OpenAI Tunnel 接入，或排查“本地 `/readyz` 正常但 ChatGPT Connector 创建失败”的情况，见 [Windows + OpenAI Secure MCP Tunnel 深入实操](WINDOWS_OPENAI_TUNNEL.zh-CN.md)。它是深入配置/排障文档，不是普通用户第一次必须阅读的教程。

## 已有 Server

对于已经明确配置为 shared-key client 接入的 hosted Server，使用 operator 提供的 credential
走长期 shared-key 路径：

```bash
webcodex connect https://webcodex.example --key-file /private/path/shared-key
```

`connect` 会启动/复用本地 Runner，并在连接验证完成后输出 MCP URL 与 credential source。
这与刚完成 Docker bootstrap 的自托管 Server enrollment 不同：bootstrap administrator token
保留在 Server 机器上，由 Server 创建短期 pairing code，再在仓库机器上执行 `webcodex login`。
自托管见[部署指南](DEPLOYMENT.zh-CN.md)。

Bearer/shared-key 是最简单的路径。客户端要求 OAuth 时，使用 `share --auth oauth` 或
`connect --auth oauth` 并传入该客户端的精确 callback URL，然后按 CLI 输出配置。managed-user
OAuth 仍是独立的高级身份路径。

## Advanced / reference

### Runtime surface selection

Server 启动时会选择 model-facing MCP surface。普通用户不需要选择或理解内部 routing 名称，直接使用当前 Server 展示的工具即可。需要调整该 surface 的 maintainer 应查看内部 architecture/configuration contract；routing 不会改变目标工具原有的 authentication、project 或 safety checks。

### Tool result framing

MCP tool 的 machine-readable 结果位于 `structuredContent`；`content` 只保留简短的人类可读或 protocol-native fallback。需要结构化字段的 client 应读取 `structuredContent`，不要解析文本。

Result 中的 recovery 字段只描述下一次**显式**调用的安全建议，不授予 authority，也不会触发 hidden retry。尤其是 uncertain outcome，必须先 reconcile，再决定是否重复 effect。

### 内建 local MCP gateway

hosted Server 可以通过同一个 `/mcp` 暴露 Runner-owned 本地 stdio MCP provider。有权限的 caller 使用单一 `mcp_tool` 来 list、describe、call 已配置 provider；provider process/instance identity 与 schema-revision state 都留在内部。

在 Runner 的 `[mcp]` 中配置 provider。访问需要显式 `mcp:local` permission；hosted OAuth client 通过 `webcodex connect ... --oauth-local-mcp` opt in。Provider compatibility 细节见 [Runner](RUNNER.zh-CN.md#provider-side-gateway-v1-compatibility)。

### Managed SSH resource 接入

`ssh_resource` 工具提供一条窄化的 Runner-local 命名 SSH resource 接入路径。`list`
观察安全逻辑名称并返回 opaque exact-Runner/revision binding；`register` 与 `remove`
消费该 binding，只修改 durable desired state。它们不会把旧 mutation 静默重定向到
replacement Runner，也不会在 uncertain outcome 后自动 replay。Raw SSH target 不会出现在
工具结果或 normal/full trace body 中。返回 `restart_required=true` 时，应先重启 Runner，
再重新 `list`，之后才能在 Workflow Session 中选择该资源。

该 surface 需要可选 `ssh:local` permission，不属于普通 hosted OAuth baseline；通过
`webcodex connect ... --oauth-local-ssh` 显式 opt in。Static/managed 语义与 PersistentShell
边界见 [Runner](RUNNER.zh-CN.md#ssh-会话资源高级)。

### OAuth2

启用 OAuth 后，MCP client 可以使用 authorization-code flow，而不是静态 token。注册 client 实际要求的精确 callback URL；host 要求 refresh-token support 时保留 `offline_access`；连接参数以 `share --auth oauth` 或 `connect --auth oauth` 的输出为准。Server 配置见[部署指南](DEPLOYMENT.zh-CN.md#oauth2)。

普通 hosted `connect --auth oauth` 中，Runner 保持原 hosted credential，MCP client 获得独立 OAuth credential。只有真正需要额外能力时才增加 `--oauth-computer-permissions`、`--oauth-local-mcp` 或 `--oauth-local-ssh`。已有 client 不会被静默扩权；真实权限变化要求重新授权。

Project-first `share --auth oauth` 仍绑定本次临时 share 环境。Managed-user OAuth 是另一条高级流程（`connect --auth managed-oauth`）。OAuth credential 永远不能用于 Runner transport。

Credential 与 scope 模型见[认证](AUTH_MODEL.zh-CN.md#oauth2)。

### Grok Custom Connector（OAuth）

Grok 支持自定义 MCP Connector，并可完成 MCP Server 要求的 OAuth 流程。对于
自托管 WebCodex Server，先通过公网 HTTPS 暴露
`https://your-domain.example/mcp`，并启用 OAuth：

```text
WEBCODEX_OAUTH2_ENABLED=true
WEBCODEX_OAUTH2_ISSUER=https://your-domain.example
WEBCODEX_PUBLIC_URL=https://your-domain.example
```

当前 Grok Web Connector 流程（2026 年 8 月已验证）使用以下 redirect URI，注册时
必须精确匹配：

```text
https://grok.com/connectors-oauth-exchange-code/
```

如果后续 Grok 展示或实际使用了不同 callback，应改为注册 Grok 当时提供的精确值。
为 Grok 单独创建 OAuth client；`client_secret` 只会返回一次：

```bash
curl -fsS -X POST https://your-domain.example/api/oauth/clients/create \
  -H "Authorization: Bearer $WEBCODEX_PAT" \
  -H "Content-Type: application/json" \
  -d '{"name":"Grok MCP","redirect_uris":["https://grok.com/connectors-oauth-exchange-code/"],"allowed_scopes":["runtime:read","project:read","project:write","job:run"]}'
```

在 Grok 的 **Custom Connector** 表单中填写：

| 字段 | 值 |
| --- | --- |
| MCP server URL | `https://your-domain.example/mcp` |
| Client ID | 创建 client 时返回的 `wc_client_*` |
| Client Secret | 只返回一次的 `wc_csec_*` |
| Authorization Endpoint | `https://your-domain.example/oauth/authorize` |
| Token Endpoint | `https://your-domain.example/oauth/token` |
| Scopes | `runtime:read`、`project:read`、`project:write`、`job:run`、`offline_access` |
| Token Auth Method | `client_secret_post` |

WebCodex 会公布 PKCE `S256`；Grok 可以同时使用 PKCE 与
`client_secret_post`。对于已经注册 client secret 的 WebCodex OAuth client，不要选
`none (PKCE only)`。`offline_access` 是用于 refresh token 的协议级 scope，不会写进
OAuth client 的 `allowed_scopes` 权限列表。MCP Protected Resource Metadata 会省略
`scopes_supported`，因为不同的预注册 client 可能有不同的 scope 上限。通用 MCP 客户端
因此可以省略 `scope`，由 WebCodex 把授权请求默认到该 client 已注册的
`allowed_scopes`。

打开 WebCodex Authorization 页面后，用希望 Grok 代表的用户当前有效 PAT
（`wc_pat_*`）登录。Runner token（`wc_agent_*`）不是用户登录 token。最终签发的
OAuth access token 会绑定到该用户，同时继续受 client 注册权限和本次请求 scopes
约束。

常见错误：

- **Save & Connect 为灰色：** Grok 在启动 OAuth 前要求 Client ID 已填写。
- **`invalid token`：** PAT 必须能在当前这台 WebCodex Server 的数据库中通过认证；
  不要使用 Runner token，也不要使用旧 Server/旧数据库遗留的 stale PAT。
- **`invalid scope`：** 每个 WebCodex permission scope 都必须包含在该 OAuth client
  的 `allowed_scopes` 中。普通 Grok MCP 接入不需要 `account:manage`；
  `offline_access` 作为协议级 scope 单独接受。
- **redirect mismatch：** redirect URI 必须与注册值逐字一致，包括路径和末尾 `/`。

Grok Custom MCP UI 与可用范围以 xAI 的
[Connector 文档](https://docs.x.ai/grok/connectors)为准。

## Project-bound Connector workflow

`webcodex run` 与 `webcodex share` 会绑定一个已经配置好的仓库，并暴露一组较小的 task-oriented MCP 工具：

```text
task_start
task_list
task_resume
files_list
files_read
files_search
code_navigate
edits_apply
checks_run
commands_run
task_review
task_cancel
task_finish
code_impact
```

从 `task_start` 开始。Connector 已经知道当前 Project，因此 prompt 不需要 runtime project id 或 project discovery。返回的 `task_id` 是该 Connector task 的 durable handle；明确继续旧工作时使用 `task_resume(task_id)`。不要假设同一个 chat、HTTP/MCP connection 或 credential 会自动 resume 之前的任务。

精确的 MCP Tasks-extension materialization/polling 协议属于 implementation compatibility detail，有意不放在这份 user-facing guide 中。

## 黄金 coding 循环

```text
task_start
→ files_list
→ files_read / files_search / code_navigate / code_impact
→ edits_apply
→ checks_run
→ task_finish
→ task_review
```

`task_start` 只有两种 execution mode：

- `normal`（默认）用于可写 coding。WebCodex 在 target checkout 外准备受管理的隔离
  Git worktree，所有 edit/command/check 都在那里执行，`task_finish` 捕获稳定结果。
  只有项目 owner 在本地接受结果后 target checkout 才会变化。隔离 worktree 无法安全
  创建或验证时，`normal` 会 fail closed，绝不会回退成直接写 target checkout。
- `read_only` 只用于分析。read/search/LSP/impact analysis 仍可使用；structured write、
  command 与 check 均会被拒绝。

同一 task 可以保持当前 mode，也可以在写权限和隔离 workspace 准备成功后从
`read_only` 升级到 `normal`。已经进入 writable `normal` 的 task 不能降级为
`read_only`；应先 finish 或 reject 当前 writable task，再新建 `read_only` task。
任何 isolated writable result 在 `task_finish` 前都必须有 structured checks，不能依赖
持久化 mode 字符串绕过。

- `files_list` 从 Git index 回答"项目里有什么"，因此被忽略的目录不会出现。猜测
  路径前先调用它。
- `code_navigate` 提供只读的语言服务器状态、document/workspace symbols、
  definition、references、diagnostics 与 hover。它只接受项目相对路径和从 1 开始的
  Unicode scalar 位置；Connector 负责选择已绑定的 executor project。参数按
  operation 严格区分：`status` 不带额外字段；document symbols 与 diagnostics
  使用 `path`；workspace symbols 使用 `query`；definition、references 与 hover
  使用 `path` + `line` + `column`。无意义的字段会被拒绝。normal 与 read-only
  task 均可调用。
- `code_impact` 从项目相对源码位置执行一次有界 call hierarchy 操作。它支持
  `incoming`、`outgoing`、`both`，广度优先深度为 1 或 2，全局 edge 上限为
  1..100；只返回规范化的项目内 root、edge 和有界 call-site range。语言服务器
  不支持时会显式失败，不回退到 grep 或 AST。normal 与 read-only task 均可调用。
- `edits_apply` 是受保护的编辑工具；`commands_run` 是需要 shell 的命令的有界
  逃生口。
- `checks_run` 做 structured validation。按照它返回的 retry/status guidance 继续，不需要手工重建内部 operation identity。
- `task_finish` 生成稳定结果；由人工在本地用 `webcodex task accept <id>` /
  `webcodex task reject <id>` 接受或拒绝。模型永远不能接受自己的工作。

### 校验 recipe

`checks_run` 支持 `format`、`check`、`test` 与可选 `recipe` 枚举（`rust`、
`node`、`python`、`go`）。省略 `recipe` 时，从任务 `cwd` 相对位置最近的
`Cargo.toml`、`package.json`、`pyproject.toml` 或 `go.mod` 自动解析。Recipe 不
安装依赖、不修改 lockfile、不使用网络。缺少工具是 executor 失败；已启动的
validator 返回非零是断言失败。

| Recipe | 标记 | `format` | `check` | `test` |
| --- | --- | --- | --- | --- |
| Rust | `Cargo.toml` | `cargo fmt -- --check` | `cargo check --all-targets` | `cargo test` |
| Node | `package.json` | `format:check`/`format-check`/`check:format` 第一个 | `check`/`typecheck`/`lint` 第一个 | 精确 `test` |
| Python | `pyproject.toml` | 配置的 Ruff/Black | 配置的 Ruff/Mypy | 配置的 pytest |
| Go | `go.mod` | 不可用 | `go vet ./...` | `go test -json ./...` |

### 长校验会持久继续

`checks_run` 与 `commands_run` 使用 durable execution；工作仍在继续时，调用大约
8 秒后可能 quick-yield。在十四工具 Connector surface 上，用 `task_review` 的
`after_cursor` / `wait_ms`（需要输出时再加 `include_output_tail=true`）观察进度，
直到 execution 进入 terminal；需要停止时调用 `task_cancel`。不要为了轮询而重新
执行同一个操作。

在普通 runtime surface 上，长时间工作也可能作为 WebCodex Job 暴露。使用当前 Server 返回的 Job observation/recovery guidance，不要再启动一个副本。Opaque observation token 原样回传即可；它只是 read cursor，不是 credential 或 execution authority。

## 第一个安全 prompt

```text
Use the configured WebCodex project. Start a read-only task, read README.md,
summarize the project, review the result, and finish. Do not edit files.
```

这个 prompt 里不需要项目发现或 runtime 标识符。

## 读取与搜索边界

- `read_files` 是 canonical 有界范围读取工具，一次支持 1 到 8 个文件；单条目 batch
  就是单范围读取路径。每个成功条目返回完整文件 SHA-256 与有界行元数据；partial
  条目返回可直接执行的单条目 `read_files` continuation，且读取并非 snapshot-stable。
- `search_project_texts` 是 canonical 有界搜索面，一次支持 1 到 8 个独立查询（优先
  ripgrep，并保留现有有界 fallback）；单查询直接使用 one-query batch。

只有已识别的 backend 明确报告搜索正常完成且无匹配时，空搜索结果才是肯定的
“无匹配”证据。backend 标识缺失或畸形、完成状态缺失、状态与输出不一致、
backend 失败、Runner 失败、超时、请求丢失及 provider 失败都会返回失败。
搜索失败保留兼容的 `code`，并增加有界的 `failure_stage` 与具体
`reason_code`。批量失败条目保留宽泛的 `reason_code`，同时通过
`failure_stage` 和 `detail_code` 保留单项搜索 provenance；成功条目仍保持
稀疏投影。

失败返回只含项目相对路径的小型结构化错误——绝不包含绝对路径、命令或 Runner
stderr、provider stderr 或任意 provider prose。

## 常见错误

| 错误码 | 含义 | 处理 |
| --- | --- | --- |
| `project_not_configured` | 没有 canonical setup | 运行 `webcodex setup` |
| `project_credential_invalid` | 私有 Project Credential 缺失或不匹配 | 恢复两个匹配的私有文件或重建 profile |
| `project_credential_rejected` | 可达 server 拒绝了该凭据 | 恢复与 server 匹配的凭据 |
| `workspace_unavailable` | 配置的 Git 工作区不可用 | 恢复工作区，再运行 doctor |
| `server_unreachable` / `agent_offline` | 项目 Runner/runtime 不可用 | 运行 `webcodex run` / `webcodex doctor` |
| `required_capability_unavailable` | 当前 Runner/runtime 缺少所需 coding capability | 升级所有二进制 |
| `task_not_active` | 任务无法再变更或执行 | 开始新任务 |
| `execution_not_terminal` | Finish 被活跃/未知工作阻塞 | 审查/等待/取消 |
| `checks_required` | 普通任务尚未运行检查 | 调用 `checks_run` |
| `checks_stale` | 上次检查后工作区已变化 | 运行一次新检查 |

## 高级 runtime surface

在 project-bound Connector 之外，WebCodex 还可以作为多项目管理 ToolRuntime 运行，
提供 discovery、session、LSP、raw job 与 artifact 工具。那是面向运维者的高级
surface，不是 canonical project Connector，也不是普通 coding 的前提。

### ChatGPT 文件桥接

在暴露 artifact 工具的更宽 MCP operator surface 上，WebCodex 支持双向的
host-native 文件传输，不需要把完整二进制经由模型文本搬运：

- `import_conversation_files_to_project` 通过 ChatGPT host 的
  `openai/fileParams` 导入 1..10 个文件。它既适用于用户选择的当前会话附件，也
  适用于 host 能绑定为 file parameter 的本轮新生成文件。Control 端负责下载原始
  bytes，并通过现有有界 artifact write 路径提交；调用方不应自行构造下载 URL，
  也不应手工 Base64 转运这些文件。
- `export_project_artifact` 为一个有界 project artifact 创建短期、受认证的 MCP
  `ResourceLink` 并返回 metadata。`tools/call` 不包含完整二进制；host 通过
  `resources/read` 取得 binary resource。读取时会再次检查认证与当前
  project-read authority，并在返回 bytes 前重新验证 artifact metadata。
- resource URI 本身不是独立 bearer authority。Export handle 只是短期、
  process-local 的 presentation state；现有 project artifact 的大小、MIME、路径与
  authorization 边界继续生效。

`read_project_artifact` 仍然只是有界 chunk inspection API，不承担大文件下载。
DOCX/PPTX/XLSX 等 Office artifact 与 PDF 复用同一 artifact transport，因此在
支持这些 host 能力的 ChatGPT 中，可以在 project 与 host 之间直接传递，而不需要
模型手工搬运 Base64。

更宽的 model coding surface 暴露 `work_on_project` 时，请阅读
[Coding 工作流](CODING_WORKFLOW.zh-CN.md)，使用 canonical bootstrap / behavioral role
心智模型，并遵循其中的 validation/closeout guidance。运维工具见
[架构](ARCHITECTURE.md)与 `webcodex` CLI。
