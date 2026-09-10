# 部署指南

[English](DEPLOYMENT.md) | [简体中文](DEPLOYMENT.zh-CN.md)

本文档只面向**生产与高级自托管**：长期 Server、多机器/多用户、systemd/Docker、反向代理和 operator 管理的网络配置。普通 Windows / macOS 工作站**不要从这里开始**；最推荐的路径是 [WebCodex Desktop + 官方 OpenAI Secure Tunnel](desktop-install.zh-CN.md)。CLI 或已有 Server 再看[完整使用指南](PERSONAL_SETUP.zh-CN.md)；如果只想几分钟临时体验一个仓库，再看[快速试用](QUICK_START.zh-CN.md)。

## 组件

- `webcodex` —— 统一 CLI：项目工作流、Server/Runner 生命周期、接入与运维。
- `webcodex-server` —— Server 进程：暴露 REST、GPT Actions OpenAPI、MCP 与
  Runner endpoint。
- `webcodex-runner` —— 运行在持有仓库机器上的长驻 worker。

执行配置属于实际工作的 Runner。旧 Server 的 `CODEX_*` 设置不再用于选择编码代理的可执行文件、审批模式、超时或参数白名单；编码代理应通过 Runner 的 `[acp]` / `[[acp.agents]]` 配置，参见 [ACP 编码代理指南](agent/acp-coding-agent-run.md)。Server 需要可写的数据目录，不需要单独的旧 `uploads` 目录。

## 构建与安装

官方分发路径是 npm 薄安装器/包装器：

```bash
npm install -g @yyjeqhc/webcodex
```

支持 Linux x64、Linux arm64、macOS x64、macOS arm64、Windows x64 与 Windows arm64。Windows 支持 CLI + Runner、显式前台 Server，以及显式本机 `webcodex share --tunnel cloudflare|openai|none`。Windows x64 支持 managed Cloudflare 获取；固定版本 upstream 没有官方 Windows ARM64 artifact，因此 ARM64 使用 Cloudflare 时需要受信任的显式/`PATH` binary。managed OpenAI `tunnel-client` 支持 Windows x64/arm64。WebCodex 仍不支持 Windows Server/Runner service 托管生命周期；Windows 上应显式以前台方式运行。npm 包装器要求 Node.js 18 或更新。Linux x64 native artifact 以 glibc 2.17 或更新为兼容基线。

从源码构建：

```bash
cargo build --release --workspace --bins
export PATH="$PWD/target/release:$PATH"
```

这会生成 `webcodex`、`webcodex-server`、`webcodex-runner`。

## Windows 前台 Server 与 Runner

npm 包会同时安装三个 Windows 可执行文件。Windows 不需要 Linux、WSL 或 Windows Service，也可以直接以前台方式运行 Server 与 Runner；使用期间保持对应终端开启即可。

在 Windows Server 机器上，从 PowerShell 初始化显式 env 文件并启动 Server：

```powershell
$envFile = Join-Path $HOME ".config\webcodex\webcodex.env"
$dataDir = Join-Path $HOME ".local\share\webcodex"
webcodex server init --listen 127.0.0.1:8080 --data-dir $dataDir --env-file $envFile
webcodex server run --env-file $envFile
```

启动前台 Server 时继续显式传入同一个 `--env-file`，不要依赖受管 service 的默认行为。如果先做同一台机器上的本地接入，再打开一个 PowerShell 窗口创建短期 pairing code：

```powershell
webcodex pairing create --server-url http://127.0.0.1:8080 --env-file $envFile --username workstation --display-name "Windows Workstation" --ttl-secs 600
```

然后在持有仓库的 Windows 机器上只兑换该短期 code，并以前台方式运行登录流程生成的 Runner 配置：

```powershell
webcodex login http://127.0.0.1:8080 --code <wc_pair_...> --allowed-root C:\src --project C:\src\my-repo
webcodex runner run --config <login-reported-runner-config>
```

如果 Server 与 Runner 位于不同机器，把 loopback URL 替换为 Server 可访问的 HTTPS URL，并按下文配置 Server listener/public URL 与受信任的反向代理或 tunnel。不要把 Server bootstrap token 或 env 文件复制到 Runner 机器。Windows 仍不支持 `webcodex server install/start/stop/restart/logs/uninstall` 与 `webcodex runner install`；Ctrl-C 或 Ctrl-Break 会结束前台 runtime。

如果希望 Windows Server 保持 loopback-only，同时让 ChatGPT 通过 OpenAI Secure MCP Tunnel 访问，并把独立 Runner 当作普通长期 Runner 使用，见 [Windows + OpenAI Secure MCP Tunnel 深入实操](WINDOWS_OPENAI_TUNNEL.zh-CN.md)。该文档用于高级配置和排障；普通用户不需要先读它才能理解完整使用流程。

## 把仓库接入已有的 shared-key Server

hosted shared-key 路径不需要本地 Server、数据库、反向代理或 systemd unit，但需要一把
该 Server 已接受的 shared key（通常由 operator 提供，或从本机已有受保护 profile 恢复）：

```bash
cd /path/to/your/repository
webcodex connect https://your-server.example --key-file /private/path/shared-key
```

`connect` 把当前目录作为项目，写入 owner-only profile，启动 detached Runner，并等待
Server 同时看到 Runner 与项目。把输出的 `/mcp` URL 与 credential 填入 MCP client。
机器重启后，重新运行同一条 `connect` 或使用 `webcodex runner start --profile <profile>`。

这不是刚完成 bootstrap 的自托管 Server 的首次 enrollment。那种情况应把 bootstrap
administrator token 留在 Server，并按下文 pairing / `webcodex login` 流程接入仓库机器。
shared-key 自动化场景优先使用 `--key-file <path>`，不要与 `--key` 同时传入。

## 首次生产部署

初次运维者不需要 OAuth、QUIC 或 account credential。最小生产路径：

1. 使用带 systemd、`sudo` 与公网 HTTPS 域名（或受信 tunnel）的 Linux x64 主机。
2. 安装 `@yyjeqhc/webcodex`，运行 `webcodex server init`，安装
   `webcodex-server` 服务。
3. 配置反向代理并把 `WEBCODEX_PUBLIC_URL` 设为精确的公网 HTTPS origin。
4. 在 server 上创建短期 pairing code，并在持有仓库的机器上运行
   `webcodex login <server-url> --code <code>`。
5. 在该仓库机器上安装 `webcodex-runner` 服务。
6. 运行 `webcodex ops status --strict`；之后才导入 GPT Actions schema 或添加
   MCP connector。

### Server 设置

初始化 Server env 文件（创建 bootstrap `WEBCODEX_TOKEN` 与监听/数据设置）：

```bash
sudo webcodex server init \
  --listen 127.0.0.1:8080 \
  --data-dir /var/lib/webcodex \
  --env-file /etc/webcodex/webcodex.env \
  --public-url https://your-domain.example
```

`server init` 会创建所选 data directory 与 server 侧 bootstrap/admin token；它不创建 user API token 或 Runner token。后续 guidance 会保留上面实际传入的 env/data 路径。

安装并启动受管 systemd socket/service pair：

```bash
sudo webcodex server install \
  --env-file /etc/webcodex/webcodex.env \
  --working-directory /var/lib/webcodex \
  --bin /usr/local/bin/webcodex-server
webcodex server status --env-file /etc/webcodex/webcodex.env
```

省略 `--working-directory` 时，`server install` 会优先读取所选 env 文件中的 `WEBCODEX_DATA`，再回退到平台默认值。安装前会在任何 systemd mutation 之前拒绝缺失/不可执行 binary、缺失 WorkingDirectory、不可读 env file 或不存在的显式 User/Group。`server status` 也会默认从同一 env 的 `WEBCODEX_ADDR` 派生本地 HTTP probe；显式 `--url` 始终优先。

受管 Linux 部署会明确拆分职责：`webcodex.socket` 持有固定的
`WEBCODEX_ADDR` listener，`webcodex.service` 只持有 Server process。该结构建立后，
普通二进制替换应先放置新 binary，然后执行：

```bash
sudo systemctl restart webcodex.service
```

正常替换路径不要 restart `webcodex.socket`。旧 Server drain 到新 Server 继承 listener
期间，socket 仍持续绑定，因此在正常有界 backlog 条件下消除 listener ownership gap /
`ECONNREFUSED` 窗口。收到 SIGTERM（受管 restart/stop）或 Ctrl-C/SIGINT（前台运行）后，
WebCodex 会先让 process-local drain fence 生效，再要求 Salvo 停止 accept 新 connection
并 graceful close 已有 HTTP connection。fence 生效前已经 admit 的有限生命周期 request
最多可继续 315 秒完成并写回 response；fence 生效后若 request 仍到达旧进程，则会得到
可重试的 HTTP 503，且不会进入业务 handler。315 秒由普通 HTTP hard timeout 300 秒加
15 秒 response/teardown margin 派生。生成的 systemd service 使用 `TimeoutStopSec=330s`，
再留 15 秒余量，避免 systemd 在应用自己的 bounded shutdown 之前先发 SIGKILL。

这是 availability-preserving graceful restart，不是 overlapping generations。若已有合法
request 接近最大 deadline，新 TCP connection 可能在 systemd socket backlog 中等待，直到
旧进程退出并由新 Server 继承 listener，因此 restart latency 也可能接近该有限 request
上限。已有 WebSocket、HTTP keep-alive 与 streaming connection 仍可能断开并重连；不保证
WebSocket continuity，也不宣称 literal zero interruption。

只有替换已有受管 pair 时才在 `server install` 上使用 `--overwrite`。如果当前仍是
正在运行的 legacy direct-bind `webcodex.service`，第一次迁移属于单独的一次 migration
boundary：installer 会 fail closed，避免与旧进程争抢地址。先停止 legacy Server，再用
`--overwrite` 重新安装；这次首次迁移本身不保证无 gap。

### Tool invocation trace

Tool-request trace 是 **operator diagnostic**，默认关闭。轻量排障使用 metadata 模式；只有明确需要 request/response payload capture 时才使用 `full`：

```text
WEBCODEX_TOOL_REQUEST_TRACE=full
WEBCODEX_TOOL_REQUEST_TRACE_DIR=/var/lib/webcodex/tool-request-traces
WEBCODEX_TOOL_REQUEST_TRACE_RETENTION_HOURS=168
WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES=2147483648
```

`full` 可能包含源码、patch、command/script 输入输出、user message，以及本身就出现在 tool payload 中的 secret。应像保护其它敏感诊断数据一样保护 trace 目录，并设置有界 retention/budget。Trace capture 只用于诊断；trace 写入失败不会改变 tool execution。

精确的 trace 文件布局、correlation id、model-facing forensic reader、queue/compression 与 payload validation 规则属于 implementation/maintainer detail，有意不放在这份部署指南中。

### 公网 HTTPS

Hosted MCP 客户端与 GPT Actions 需要公网 HTTPS URL。在 Server env 文件中设置
`WEBCODEX_PUBLIC_URL`，并在 `127.0.0.1:8080` 前面配置反向代理。支持 Nginx；
named Cloudflare Tunnel 也是有效入口。同一 hostname 必须承载普通 HTTPS 请求
与 `/api/agents/ws`（Cloudflare 支持 WebSocket upgrade）。WebCodex CLI 不会自动
配置反向代理或 tunnel。

### 接入仓库机器

在持有仓库的机器上，以将运行项目命令的普通用户身份执行（不要用 `sudo`）：

```bash
webcodex login https://your-domain.example --code <wc_pair_...> \
  --allowed-root "$HOME/git" \
  --project "$HOME/git/my-repo"
webcodex runner install --scope user \
  --config <login-reported-runner-config>
webcodex runner status --scope user \
  --config <login-reported-runner-config>
webcodex ops status --server-url https://your-domain.example \
  --token-file <login-reported-webcodex-user-token> --strict
```

`webcodex login` 是 canonical 客户端入口：它自动派生唯一设备名、兑换 pairing code，并写入客户端侧 `webcodex-user-token` 与 `runner.toml`。`--allowed-root` 只授予 Project 注册 authority，`--project` 才表示要注册的实际 existing workspace；生成的 `project_registry_dir` 是 registry 而不是 workspace root。如果 login 时没有传 `--project`，应在 project-bound 工作前执行 `webcodex project register --config <login-reported-runner-config> /path/to/repo`。需要显式设备 identity 或不同的本地 base directory 时，使用 `login` 已文档化的 `--device` 与 `--dir`；不再有单独的 compatibility enrollment 命令。

pairing code 由 server/admin 侧创建：

```bash
webcodex pairing create \
  --server-url https://your-domain.example \
  --env-file /etc/webcodex/webcodex.env \
  --username friendname \
  --display-name "Friend Name" \
  --ttl-secs 600
```

只把短期 `wc_pair_*` code 传给客户端。不要跨机器复制 `WEBCODEX_TOKEN`、user API
token、Runner token、env 文件或完整 `runner.toml`。每个用户使用唯一 `username`。

## Runner 服务 scope

`webcodex runner install` 支持 user 或 system scope。非 root 用户默认 user scope；
root 默认 system scope。

**User scope** 使用 `systemctl --user`，unit 写入
`$XDG_CONFIG_HOME/systemd/user`，配置放在 `$XDG_CONFIG_HOME/webcodex`，无需
`sudo`：

```bash
webcodex runner install --scope user --profile workstation
webcodex runner status --scope user --profile workstation
webcodex runner logs --scope user --profile workstation --lines 100
```

已启用的 user unit 跟随该账号的 user manager。如需无人值守的开机持久化，管理员
可显式运行 `sudo loginctl enable-linger <runner-user>`；WebCodex 不会自动修改
lingering。

**System scope** 使用 `/etc/systemd/system`，需要指定非 root `--user`：

```bash
sudo webcodex runner install \
  --scope system \
  --profile workstation \
  --user <runner-user> \
  --working-directory /home/<runner-user> \
  --config /etc/webcodex/clients/workstation/runner.toml
sudo webcodex runner status --scope system --profile workstation
```

除非显式 `--allow-root-runner`，否则拒绝以 root 运行 Runner（不推荐）。所有
生命周期命令使用相同 `--scope`。示例文件在 `deploy/`（`webcodex.env.example`、
`webcodex.service.example`、`webcodex-runner.toml.example`、
`webcodex-runner.service.example`、`nginx.webcodex.example.conf`）。

## Docker（仅 Server）

仓库提供 server-only Dockerfile 与 Compose 部署，运行 `webcodex-server` 与管理
CLI；有意不包含 Runner、项目仓库与工具链。正式 Release 会向
`ghcr.io/yyjeqhc/webcodex-server` 发布同时支持 `linux/amd64` 与 `linux/arm64`
的多架构镜像。

普通 Server 部署不再需要 clone 仓库，也不需要 Rust toolchain。直接从最新公开 Release
下载一个 self-contained bootstrap 再运行：

```bash
mkdir -p webcodex-server && cd webcodex-server
curl -fLO https://github.com/yyjeqhc/webcodex/releases/latest/download/webcodex-server-bootstrap.sh
sh webcodex-server-bootstrap.sh https://webcodex.example.com
```

Release 生成的 bootstrap 内嵌固定到该 Release 不可变 multi-arch image digest 的 Compose
定义，并在本地 materialize 为 `webcodex-server-compose.yaml`。因此即使使用
`latest/download` 这个便捷入口，脚本下载完成后部署目标也已经固定，同时不存在两个 Release
资产分别下载时发生版本切换的竞态。需要明确选择某个版本时，只需把 URL 中的
`releases/latest/download` 替换为 `releases/download/v<VERSION>`。bootstrap 会把实际
`COMPOSE_FILE` 与精确 image reference 写入私有 `.env`，之后在该目录执行普通
`docker compose` 命令仍会复用同一份固定部署。

bootstrap 现在是可恢复事务，而不是一次性脚本。它会在创建 administrator secret 之前完成
严格 HTTPS origin、Compose/source asset、主机端口、Docker/Compose 与公开镜像 preflight；
随后通过私有 `.webcodex-bootstrap.receipt` 依次记录 `AssetsPrepared`、
`SecretCommitted`、`ContainerStarted`、`ServerHealthy`、`PairingReady`。receipt 只保存
hash 与阶段，不保存 administrator token。`.env` 通过 0600 临时文件写入、sync 后原子 rename。
只有 Compose healthcheck 与 `/openapi.json` 都验证通过后才打印成功，并在这个 readiness
barrier 之后创建第一枚短期 pairing code。

安装被中断，或 startup/health check 失败时，不要删除 `.env`；在同一目录继续使用同一份
bootstrap：

```bash
sh webcodex-server-bootstrap.sh status
sh webcodex-server-bootstrap.sh resume
# 清除运行时 effect，但保留已经提交的 administrator token 与 named data volume：
sh webcodex-server-bootstrap.sh rollback
```

`resume` 会复用已经提交的 administrator token，不会仅因为 `docker compose up` 或 health
check 失败就重新生成 token。`rollback` 在 secret 已提交后会回到 `SecretCommitted`，而不是
删除 `.env`，因为 Server 可能已经用该 token 初始化过 durable data。没有对应 receipt 的旧版或
非受管 `.env` 不会被自动覆盖或删除。

开发场景，或首个公开 GHCR 镜像尚未完成启用之前，再 clone 源码并显式选择 source build：

```bash
git clone https://github.com/yyjeqhc/webcodex.git
cd webcodex
./deploy/docker/bootstrap.sh https://webcodex.example.com --build-from-source
# 后续源码重建继续显式带上 override：
docker compose -f compose.yaml -f compose.build.yaml up -d --build
```

默认绑定 `127.0.0.1:8080`。在前面配置 HTTPS 反向代理；bootstrap 已经验证本地 Server
并创建第一枚短期 pairing code，使用该 code 接入持有仓库的机器。GitHub 首次创建 GHCR
package 时默认将其设为 private；维护者
需要一次性把 package visibility 改成 Public，之后 workflow 的匿名拉取 gate 才会
通过，普通用户无需配置 registry 凭据。

## Runner 配置

客户端接入会生成 Runner 配置。`runner.toml` 中的重要设置：

| 设置 | 说明 |
| --- | --- |
| `server_url` | 公网 WebCodex URL。 |
| `token` | Runner credential。不要提交或打印。 |
| `client_id` | 用于 `agent:<client_id>:<project_id>` 的稳定 id。 |
| `owner` | 该 Runner 的 owner principal。 |
| `transport` | 配置 `[quic]` 时优先用 `auto`。 |
| `project_registry_dir` | 项目注册文件目录。 |
| `[policy]` | 本地执行边界（`allowed_roots` 等）。 |
| `[skills].roots` | 可选的 Runner 本机绝对只读 Skill roots；直接 live discovery，不复制进 managed Skill Store。 |
| `[shell]` | 可选 shell profile 定义与有界 persistent-shell 限制。 |
| `[ssh.resources.<name>]` | 可选命名 SSH 目标，用于 Session 绑定的 `run_shell` / `run_job`。 |

Policy 默认：`allowed_roots` 缺失或为空时默认 `$HOME`；显式 `allowed_roots`
覆盖默认值。用显式 roots 收窄 Runner，例如只允许一个工作区：

```toml
[policy]
allow_raw_shell = true
allow_cwd_anywhere = false
allowed_roots = ["/root/git"]
max_timeout_secs = 3600
max_output_bytes = 262144
```

编辑已经运行的 Runner 启动时绑定的 `runner.toml` 后，先调用
`runner_config_check(client_id=...)`，再把返回的 `current_generation` 作为
`runner_config_reload(client_id=..., expected_generation=...)` 的 fence，最后调用
`runtime_status(client_id=...)` 检查状态。check 不激活 candidate；reload 也不写配置文件。
无效 candidate 保留旧 active snapshot/generation，`restart_required_fields` 明确列出仍需
重启、且不会假装已经在线生效的 startup-only 变更。Unix service reload/SIGHUP 仍保留为
调用同一 reload primitive 的兼容 trigger，但 first-class config control 不依赖它。身份、
server/auth、项目来源、并发、能力与传输等字段在被报告为 restart-only 时仍需要重启。

`[plugins]` 支持 live reload：generic Runner config reload 与 `plugin_tool reload` 共用同一个
Plugin candidate admission/atomic-commit primitive。Plugin provider Tool 始终是 Runner-local
capability，只通过 `plugin_tool` 暴露，不会 promotion 到外层 MCP `tools/list`，也不需要为了
Plugin discovery 重启 Runner。

前台测试可运行 `webcodex-runner --profile workstation`。高级手动生成配置用
`webcodex runner init`。

## OAuth2

Server 没有公网 origin 时 OAuth2 仍默认关闭。使用 `webcodex server init --public-url https://your-domain.example` 时，初始化会写入 public URL、以该 URL 作为 issuer 启用 OAuth，并为普通 hosted connect 启用 shared-key OAuth bridge。手工维护 env 时等价配置为：

```text
WEBCODEX_PUBLIC_URL=https://your-domain.example
WEBCODEX_OAUTH2_ENABLED=true
WEBCODEX_OAUTH2_ISSUER=https://your-domain.example
WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE=true
```

普通仓库机器不需要 managed login，直接使用 MCP 客户端要求的精确 callback：

```bash
cd /path/to/your/repository
webcodex connect https://your-domain.example --auth oauth \
  --oauth-redirect-uri https://client.example/callback --project .
```

如需让这个 ordinary shared-key OAuth client 提供固定的 optional Computer consent 集合，必须显式 opt in：

```bash
webcodex connect https://your-domain.example --auth oauth \
  --oauth-redirect-uri https://client.example/callback \
  --oauth-computer-permissions --project .
```

Runner 继续使用原有 hosted credential，MCP client 获得独立 OAuth credential。`--oauth-computer-permissions` 与 `--oauth-local-mcp` 都是 optional capability 的显式 opt-in；普通 reconnect 不会静默增加这些权限。真实 OAuth 权限变化需要 client 重新授权。ChatGPT 不会得到 Runner/shared-key credential，OAuth token 也不能用于 Runner transport。

只有明确需要 managed-user OAuth identity 时，才使用高级 `webcodex login` 流程，再执行 `webcodex connect ... --auth managed-oauth --oauth-redirect-uri ...`；`--user` 仅用于该模式。

创建 OAuth client（`client_secret` 只返回一次；服务端只存其 hash）：

```bash
curl -fsS -X POST https://your-domain.example/api/oauth/clients/create \
  -H "Authorization: Bearer $WEBCODEX_PAT" \
  -H "Content-Type: application/json" \
  -d '{"name":"ChatGPT MCP","redirect_uris":["https://chatgpt.com/connector/oauth/<callback-id>"],"allowed_scopes":["runtime:read","project:read","project:write","job:run"]}'
```

`allowed_scopes` 限制 OAuth client 最多可以请求哪些权限。WebCodex 新增 permission 时不会静默扩大已有 client。要修改现有 client，请把期望保留的完整、非空 allow-list 提交到 `POST /api/oauth/clients/update_scopes`。真实变化会让旧 OAuth grant 失效并要求重新授权；提交相同 canonical list 是 no-op。安全模型见[认证](AUTH_MODEL.zh-CN.md#oauth2)。

如果启用 ChatGPT MCP host-file import，请把精确的 server-generated OAuth client id 配入 `WEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS`。重新创建 client 会生成新 id，因此应把更新这个设置作为一次显式 trust rotation。Client display name 与 redirect URI 不能替代该精确 client id。

用 `POST /api/oauth/clients/list` 与 `POST /api/oauth/clients/revoke` 列出与
撤销 client。OAuth 使用 authorization-code 流程；动态 client 注册、OIDC 与
device-code 流程未实现。宿主提供 `offline_access` 时保持勾选——它是协议级
refresh-token scope，不授予额外 WebCodex 权限。

## GPT Actions 与 MCP

- **MCP：** 用 user API token（`wc_pat_*`）把客户端连接到
  `https://your-domain.example/mcp`；启用 OAuth 时用 OAuth 流程。
- **GPT Actions：** 把 `https://your-domain.example/openapi.json` 以 HTTP Bearer
  认证导入 Custom GPT。

两者使用同一个 user API token 与同一个 ToolRuntime。OpenAPI schema 有意排除
users、token、pairing/enrollment、setup、doctor、npm、server 管理与 audit
endpoint。这些请用 `webcodex` 完成。

MCP 与 GPT Actions 见 [MCP.md](MCP.zh-CN.md) 与客户端特定设置
[AI 接入指南](AI_ONBOARDING.zh-CN.md)。

## 运维

### Authority mode

`WEBCODEX_AUTHORITY_MODE` 控制有后果的 runtime 工具是自动执行还是需要人工审批：

| 值 | 行为 |
| --- | --- |
| 未设置 / 空 | `trusted_agent`（自托管单运维者部署的默认值）。 |
| `trusted_agent` | 项目工作、shell、jobs、git、校验在硬安全检查后自动执行，无审批中断。Push/tag/publish/release/deploy 仍要求用户任务显式包含该动作。 |
| `restricted` | 有后果的工具在人工批准前被拒绝（`webcodex task approve/deny`）。 |

`trusted_agent` 永不放松硬安全边界（项目根、只读会话、路径策略、凭据脱敏、
job 取消语义）。`WEBCODEX_PERMISSION_MODE` 支持明确映射：`dev_auto_approve` → `trusted_agent`，
`require_approval` → `restricted`；未知值及新旧配置冲突仍拒绝。

### 运维检查

```bash
webcodex ops status --server-url "$SERVER_URL" --token-file "$USER_TOKEN_FILE" --strict
webcodex ops runners --server-url "$SERVER_URL" --token-file "$USER_TOKEN_FILE"
webcodex ops projects --server-url "$SERVER_URL" --token-file "$USER_TOKEN_FILE"
webcodex ops smoke-preflight --server-url "$SERVER_URL" \
  --token-file "$USER_TOKEN_FILE" --project agent:workstation:my-repo
```

`ops` 命令只读，绝不打印 token 或 env 值。`--strict` 会让 FAIL 报告以状态码 2
退出。`WARN` 表示值得关注但不是 deploy blocker。

### Smoke 检查

推荐的生产 smoke 序列：

1. `webcodex ops status ... --strict` 通过。
2. `POST /api/runtime/status` 返回 `service=webcodex` 与预期公网 URL。
3. `list_runners` 显示至少一个在线 Runner。
4. `listProjects` 显示 `agent:<client_id>:<project_id>` id。
5. 已知项目上的只读项目工具可用。
6. 写入/替换/校验测试只针对一次性 smoke 项目。

### Runtime console

Server 在 `/console` 提供 host-local 浏览器 console。它展示项目就绪状态、工作队列、
Workflow Session 活动、当前可见 Runner 与近期变更性活动。对于 Connector task，同机
人类可以发送 task guidance、处理待审批操作、取消工作，并对稳定结果执行 Accept 或
Reject；这些动作与 CLI 使用相同的权限边界，在线模型仍然不能接受自己的工作。Console
还会展示不含 secret 的客户端连接目标，并把 ChatGPT Developer Mode MCP custom app
作为 ChatGPT 主路径。Credential 不会由 console API 返回。

### Runtime job API 信任模型

`job_status`、`job_log`、`list_jobs` 与 `job_tail` 面向受信的单运维者部署。它们
不是互不信任用户之间的租户边界。不要把单个 runtime 暴露给多个不受信用户，除非
为无项目 job API 增加 job-owner 隔离；否则请使用独立的 server/runtime 实例。

## 故障排查

运维检查清单与常见修复（已有 systemd 服务、`HTTP reachable: no`、客户端 PATH
缺少 CLI、server 侧 pairing 与客户端 enrollment 的区别、`client online: no`）
见[故障排查](TROUBLESHOOTING.zh-CN.md)。
