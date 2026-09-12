# Runner

Runner 是真正执行工作的组件。可执行文件是 `webcodex-runner`；管理它的 CLI 命名
空间是 `webcodex runner ...`。`webcodex` 与 `webcodex-runner` 是两个独立可执行
文件。运维生命周期统一使用 `runner` 命名空间；历史 `agent` 术语只保留在兼容性所需
的令牌、存储、身份、项目 id 与 wire contract 中。本页说明 Runner 做什么、如何连接、
如何注册项目、如何以服务方式运维，以及它的主要运行时概念。

安装与服务设置见[部署指南](DEPLOYMENT.zh-CN.md)；管理 Runner 的命令见
[CLI](CLI.zh-CN.md#runner-生命周期)。

## Runner 做什么

Runner 运行在持有仓库的机器上。它主动连接 WebCodex Server，注册它允许服务的
项目，并在这些项目边界内执行有界操作——文件读写、Git 检查、结构化校验、shell
命令与长任务 Job。

Runner 是最接近你仓库的信任边界。请用窄的 allowed roots 与显式 shell profile
配置它，而不是继承宽泛的交互式 shell 状态。

## 核心术语

| 术语 | 含义 |
| --- | --- |
| **Server** | 认证调用方、保存共享 runtime 状态并路由工作。 |
| **CLI** | 运维/开发者使用的 `webcodex` 命令。 |
| **Runner** | 在仓库机器上执行工作的 `webcodex-runner` 进程。 |
| **profile** | 一组命名的本地 Runner/client 配置。 |
| **client_id** | 一个 Runner/设备的稳定逻辑名称。 |
| **Project** | 由该 Runner 注册的一个仓库/工作区。 |

部分 compatibility-facing value 仍使用历史 `agent` 名称，例如 Runner token 的 `wc_agent_*` 前缀与 `agent:<client_id>:<project_id>` runtime Project address。它们不属于 WebCodex 独立的 Durable Agent domain；普通用户也不需要理解 Runner recovery 背后的进程级 lease identifier。

### Runner 配置文件名兼容

`runner.toml` 是 canonical config filename。只有旧 `agent.toml` 的历史目录仍可继续读取；同一配置目录同时存在两种文件名时 WebCodex 会 fail closed，要求 operator 先消除歧义。`WEBCODEX_RUNNER_CONFIG` 是当前 path override，旧 `WEBCODEX_AGENT_CONFIG` 只作为兼容 alias 保留。

## 连接 Server

Runner 主动向外连接 Server，使用四种传输之一，由 `runner.toml` 中的 `transport`
设置选择：

| 传输 | 配置值 | 用途 |
| --- | --- | --- |
| Auto | `auto` | 生产环境推荐（配置 `[quic]` 时）：先 QUIC，再 WebSocket，最后 polling。 |
| QUIC | `quic` | 仅 QUIC。Server 端独立 UDP listener。 |
| WebSocket | `websocket` | 无 UDP 场景的稳定 fallback。 |
| Polling | `polling` | 受限网络的最后手段。 |

Runner 使用 Runner token（兼容前缀 `wc_agent_*`）认证；hosted shared-key 模式则使用对应 shared key。这个 credential 只用于 Runner transport，不用于 MCP、REST 或 GPT Actions。

WebSocket 与 polling 都使用 `Authorization: Bearer <token>` 认证 first-party
Runner；Runner query-string credential 不再接受。QUIC 把凭据限制在
transport-specific v1 首个注册帧中，共享 Runner envelope 不再携带凭据。

### Server/Runner 兼容

旧安装跨越 0.4 边界升级时，应同步升级 first-party Server 与 Runner。`0.4.x` 内保持稳定 protocol baseline，新 optional capability 通过显式 capability 增量加入；旧但兼容的 Runner 缺少某项 capability 时，该功能 fail closed，而不是猜测或模拟支持。

精确的 protocol-generation field、baseline capability list、registration grammar 与 compatibility test matrix 属于 maintainer/wire contract，有意不放在这份运维指南中。

使用 QUIC 时保持 Server/Runner QUIC 配置一致。`[quic].keepalive_interval_secs` 默认 20 秒，允许 `1..=25`；非法值会被拒绝，不会 silent clamp。

## 注册项目

项目存放在 Runner 机器上。Runner 把允许的目录注册给 Server；Server 不扫描文件
系统，也不自行发现项目路径。

每个注册项目是 Runner `project_registry_dir`（默认 `project-registry`）下的一个一文件一项目
TOML：

```toml
id = "webcodex"
path = "/srv/webcodex/projects/webcodex"
name = "WebCodex"
kind = "repo"
allow_patch = true
```

真正重要的是 `id` 与 `path`；`kind` 只属于可选描述 metadata。Registry directory
用于保存 Project record，本身不是 workspace root。

新配置使用 `project-registry/` 与 `project_registry_dir`。历史安装如果只有
`projects.d/` / `projects_dir` 仍可读取；如果新旧 location/field 同时存在，WebCodex
会 fail closed，而不是 merge 或猜 precedence。新的 CLI 命令使用
`--project-registry-dir`。

Runtime project id 形如 `agent:<client_id>:<project_id>`，例如
`agent:workstation:my-repo`。project-bound Connector 会在内部解析它；普通用户
不需要输入。

### 允许根目录

Runner policy 中的 `allowed_roots` 控制项目可以在哪里注册或创建：

- `allowed_roots` 缺失或为空时默认 `$HOME`。
- 显式 `allowed_roots` 覆盖该默认值。
- 用显式 roots 把 Runner 收窄到某个工作区，例如：

```toml
[policy]
allow_cwd_anywhere = false
allowed_roots = ["/root/git"]
```

### 运行时注册项目

runtime 工具 `register_project` 与 `create_project` 让客户端在在线 Runner 上
注册已有目录或创建新目录，受 Runner 的 `allowed_roots` policy 约束。

## Skill 来源

`skill_list` 继续只暴露一个 catalog，但其中保留三种彼此独立的 ownership / lifecycle：

| 来源 | 位置 / owner | Trust | 版本语义 |
| --- | --- | --- | --- |
| Project Skills | `<project>/.agents/skills/<package>/SKILL.md` | `project_content` | Project live content；没有 package revision。 |
| Configured live Runner Skill roots | Runner 主机上由 operator 配置的绝对目录 | `operator_configured_guidance` | 直接读取的只读 live filesystem content；没有 install、activation、rollback 或 package revision。 |
| Managed Runner Skill Store | Runner state 下的 `runner-skills-v1` | `operator_installed_guidance` | immutable package revision，并保留 install、activation、remove 与 rollback-oriented Store 语义。 |

Configured live roots 默认不存在，需要在 Runner 的 `runner.toml` 中显式配置：

```toml
[skills]
roots = [
    "/home/alice/.codex/skills",
    "/home/alice/.agents/skills",
    "/opt/company/agent-skills",
]
```

Windows 使用等价的本机绝对路径；包含反斜杠时可以使用 TOML literal string：

```toml
[skills]
roots = [
    'C:\Users\alice\.codex\skills',
    'C:\Users\alice\.agents\skills',
]
```

每个 root 直接包含 `<root>/<package>/SKILL.md`，package 内可以有 `references/`
等 resource。WebCodex 不会把它们复制到 managed Store；`skill_install`、
`skill_activate` 与 `skill_remove_revision` 仍然只修改 managed Store。

这些路径始终属于 **Runner 主机**；Server 与 Runner 不在同一台机器时也不会改用
Server 的 filesystem。Configured roots 不会加入 `[policy].allowed_roots`，因此不会给普通
Project file/shell/process 工具扩大文件系统 authority，native root path 也不会投影到
model-facing Skill catalog。Skill read 只提交 opaque `skill_id` 与 package-relative resource
path，由 Runner 根据 trusted config 解析 root，并拒绝 traversal 与 link escape。

Skill 文件本身是 live 的：修改 `SKILL.md` 或 resource 后，下一次 discovery/read 会直接
看到新内容，不需要 reload。只有修改 `roots` 配置列表时才需要按正式流程先执行
`runner_config_check`，再携带当前 generation 执行 `runner_config_reload`；该字段支持 hot
reload，不需要重启 Runner 进程。

## 本地 MCP provider

Runner 可以直接托管供 WebCodex 内建 MCP gateway 使用的 persistent stdio MCP provider：

```toml
[mcp]
request_timeout_secs = 30

[[mcp.providers]]
id = "github"
name = "GitHub"
executable = "/absolute/path/to/github-mcp-server"
args = []
cwd = "/absolute/provider/workdir"
env_from_env = { GITHUB_TOKEN = "GITHUB_TOKEN", PATH = "PATH", HOME = "HOME" }
timeout_secs = 30
```

`executable` 与可选 `cwd` 都是 Runner host-local operator 配置并且必须为绝对路径；非法路径会 fail closed。`[mcp]` 属于 restart-required 配置，不支持 provider hot reload。

provider 不会整体继承 Runner 环境。`env_from_env` 只复制显式列出的变量，WebCodex 自己的 sensitive transport/account credential 变量不允许映射；配置的 source variable 缺失时会在 provider 启动前失败。Windows 上，Runner 在清空环境后会额外只提供非敏感的 `SYSTEMROOT` OS bootstrap（除非 operator 显式映射该 destination）；`PATH`、用户 profile 状态、代理与 credential 仍不会被整体继承。

把 credential 映射给 provider，就等于把这份 credential 委托给该 provider process。provider 可以按自身实现使用它，也可以通过正常 tool result 返回派生值甚至原始值；WebCodex 不会尝试对任意 provider output 做 secret redaction。因此应把 configured provider 视为 credential recipient，使用 least-privilege provider credential，并注意任何拥有 `mcp:local` 权限的 caller 都能行使这些 credential 为 provider 提供的能力。

provider connection 在第一次真实交互时启动，并在健康时复用。发生 fatal stdio/protocol failure 时只会退休当前 connection；WebCodex 绝不会重放刚才失败的 request。后续由 caller 明确发起的新 request 可以在同一逻辑 provider identity 下建立新 connection；effectful `tools/call` 在 dispatch 前仍会重新 `tools/list` 并核对已绑定 schema。Server 只看到逻辑 provider `id`/`name`，不会拿到 executable path、环境 value、PID、stderr 或 Runner credential。`mcp_tool(action=list)` 只表示 provider id 是否可路由；`list(server=...)` 与 `describe` 才会与 provider 交互。

### Provider-side gateway V1 compatibility

Runner 到 configured local provider 的内建 gateway 有意限制为 bounded stdio tool subset，并不是所有 MCP feature 的透明 bridge：

- provider-side tool 行为基于 MCP `2025-06-18`；
- 支持 `tools/list` 与 `tools/call`；
- 不支持 callback、list pagination、media/resource 与端到端 progress forwarding；
- 支持 text tool result 与有界 `structuredContent`。

不支持的 protocol/content shape 会 fail closed，而不是静默转换。

## Shell profile

普通 Project Shell/Process 默认使用 `[shell] environment_mode = "inherit"`，继承
启动 Runner 的 PATH、HOME/USERPROFILE 和工具链环境，继续过滤 WebCodex 内部凭据。
Shell env 覆盖继承值，profile env 再覆盖 Shell env；未配置 init_script 时不执行
启动脚本，也不会自动 source `.bashrc` / `.profile`。

可显式选择 `environment_mode = "isolated"`：Unix 仅提供 `/usr/bin:/bin` PATH，
Windows 提供 SystemRoot 与 System32 PATH，再应用配置 env/path_prepend。这不是文件系统沙箱。
MCP 的显式 credential delegation 规则不变；Native Plugin 继续使用已有的凭据过滤。

Windows structured process 支持 `.cmd`/`.bat`，由 Runner 内部转换 argv。支持空参数、
空格、`&`、`|`、括号；双引号、`%`、`!`、`^`、控制字符和尾部反斜杠在启动前拒绝，
命令上限为 8000 UTF-16 units；UNC cwd 会在启动前拒绝，避免 cmd.exe 静默切换工作目录。这些参数应改用 native runtime。进程树和 Job 契约不变。

`read_files` 的精确单条目读取可读取 node_modules/target，普通搜索仍跳过它们，structured edit 仍拒绝。
`.env*`、凭据、Runner 配置及 `.git` 控制数据继续保护。

`runner.toml` 中的 Rust/Cargo 示例：

```toml
[shell]
default_profile = "rust"

[shell.profiles.rust]
program = "sh"
args = ["-c"]

[shell.profiles.rust.env]
PATH = "/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
CARGO_HOME = "/root/.cargo"
RUSTUP_HOME = "/root/.rustup"
```

Python venv 示例：

```toml
[shell.profiles.py-venv]
program = "bash"
args = ["-lc"]
init_script = '''
source .venv/bin/activate
'''
```

`init_script` 是项目相对路径：从项目根目录解析，因此每个项目激活自己的 venv。
项目可以固定 profile：

```toml
id = "paper-exp"
path = "/root/git/paper-exp"
shell_profile = "conda-ml"
```

解析顺序：`project.shell_profile`，再 `shell.default_profile`，最后是普通 shell
配置（不生成快照）。

Profile 的安全要点：

- 不要把令牌放进 `init_script`，也不要在其中 `echo` 密钥——脚本 stdout 会被
  当作快照的一部分解析。
- status 与 runtime API 只暴露脱敏的 profile 元数据（名称、`has_init_script`、
  env 键数量、program、dialect）——绝不暴露 `init_script` 正文或环境值。
- Profile 以清空环境 + 显式白名单运行；请声明所需 env。

### `run_script` 的 typed 脚本语言

`run_script` 接受 `sh`、`bash`、`powershell`、`javascript` 和 `typescript`。
JavaScript 与 TypeScript 都由 Runner 上的外部 Node.js 执行。WebCodex 从准备好的
shell/profile PATH 解析 `node`（仅当已配置的 shell/profile program 本身是
`node`/`node.exe` 时也可直接使用）。JavaScript 正文写入 Runner-owned `.mjs`
临时文件，并以 `node <temporary.mjs> <args...>` 的 native argv 形式启动；`.mjs`
固定 ESM 语义，不受项目 `package.json` 或临时目录 metadata 影响。

TypeScript 的定位是 typed-script runtime，不是项目编译器。Runner 将正文写入
Runner-owned `.mts` 文件，因此入口始终采用 ESM，并使用 Node 原生的 erasable type
stripping。最低支持 Node.js 22.6.0。创建或启动用户脚本之前，Runner 只执行一次有界的
`node --version` capability probe：Node 22.6–22.17 与 Node 23.0–23.5 由 Runner
固定加入 `--experimental-strip-types`；Node 22.18+、23.6+ 以及后续支持版本使用默认的
native stripping，不再附加该 flag。Node 缺失、版本无法解析或低于 22.6 时返回
`not_started` / `interpreter_unavailable`，用户脚本不会启动。如果版本 probe 已通过、
但实际脚本进程之后拒绝某项 runtime semantics，则以实际已启动进程的 lifecycle 为准。

该 TypeScript contract 覆盖 Node 能直接擦除的语法，例如 type annotations、interface /
type alias、generics，以及普通 JavaScript 的 async/await、ESM、Node builtins。WebCodex
不会执行 type checking，不会调用 `tsc`，不会把 `tsconfig.json` 当作项目构建配置，
不会实现 tsconfig path alias，也不承诺 enum、parameter properties、runtime namespace、
import alias 等需要 transform 的 TypeScript 语法。本 contract 也不使用
`--experimental-transform-types`。在仍把 type stripping 标为 experimental 的旧版 Node
上，Node 自己的 `ExperimentalWarning` 可能进入 stderr；WebCodex 不做 suppress/filter，
以免同时吞掉用户脚本产生的 warning。

滚动升级时，JavaScript 独立要求 `structured_script_javascript`，TypeScript 独立要求
`structured_script_typescript`。这些 capability 只表示当前运行的 Runner binary 理解
对应 typed-script wire semantics，并不表示本机一定有兼容 Node。脚本 args 继续作为
literal native argv，stdin 继续独立传递，两种语言继续复用相同的 resolved project cwd、
timeout/cancellation、Runner policy 和 Job lifecycle。

WebCodex 不会安装或 bootstrap npm dependencies，不会注入 `node_modules` / `NODE_PATH`，
不会选择 package manager，也不会 fallback 到 Bun、Deno、`tsx`、`npx` 或其他 runtime。
由于 `.mjs` / `.mts` 入口位于 Runner-owned 临时目录，相对 ESM import 会相对于该临时
module 解析，而不是 project cwd；导入项目代码时应使用 Node builtin 或显式项目路径/file URL。

## Job 与并发

Job 是在发起调用返回后仍继续运行的长命令或校验。Job 有稳定的 `job_id`、有界的
stdout/stderr 尾部，并且可以停止。结构化执行（`run_process`、`run_script`）与
校验 Job 会在单个执行超过同步宽限期时把同一执行交给 Job 延续；同一个进程继续
运行——绝不会被重启。

Runner 同时最多执行 `max_concurrent_jobs` 个 Job（默认 4，合法范围 1..64）。超出范围
的值会作为配置错误被拒绝。这个参数用于运维调优，不是安全边界，修改需要重启 Runner：

```toml
max_concurrent_jobs = 4
```

当所有槽位都被占用时，已接受的 Job 仍是同一个可查询 Job（同一 `job_id`），并
报告 `agent_queued`。

## 传输细节

### Server 的 QUIC 要求

在 Server 上启用 QUIC listener 并打开所选 UDP 端口：

```sh
WEBCODEX_QUIC_ENABLED=true
WEBCODEX_QUIC_LISTEN=0.0.0.0:8443
WEBCODEX_QUIC_CERT=/etc/letsencrypt/live/<host>/fullchain.pem
WEBCODEX_QUIC_KEY=/etc/letsencrypt/live/<host>/privkey.pem
WEBCODEX_QUIC_ALPN=webcodex-runner/1
```

证书 SAN 必须匹配 Runner 上配置的 `server_name`。配置了 `[quic]` 时 `auto` 先试
QUIC，再 WebSocket，最后 polling。

### Runner 的出站代理

如果 Runner 主机需要出站 HTTP 代理，请把代理变量放进 Runner 的服务环境，而不仅
是交互 shell。WebSocket 遵循 `HTTPS_PROXY`/`https_proxy`、再
`HTTP_PROXY`/`http_proxy`、再 `ALL_PROXY`/`all_proxy`；`NO_PROXY`/`no_proxy`
可绕过匹配主机。当前支持的代理传输是 `http://host:port` 的 HTTP `CONNECT`。
QUIC 不使用代理设置。

## 重连与恢复

Runner 断连是 liveness 事实，不等于工作丢失。已接受的活跃 Job 会进入有界的
`recovering` 状态（默认宽限 120 秒），并在同一 Runner 实例重连后从其 inventory
恢复。替换的 Runner 实例不会继承旧实例的 Job；它们会变成 `lost`。Runner 进程重启
无法恢复其旧的子进程。

Server 会把稳定的 Runner `client_id` 与当前 live process lease 分开。stale/replacement process 不能继续使用旧 lease 提交结果，普通 child-process Job 也不会被 replacement Runner 接管。精确 lease identifier 属于内部 wire detail。

重连以短延迟自动进行。认证失败等致命错误会停止 Runner，而不是无限重试。

## 关停与重启

`webcodex-runner` 在 `SIGINT`/`SIGTERM` 时干净关停。它不会自行 daemonize。
托管部署请用 `webcodex runner install --scope user|system` 把它安装并托管为
user 或 system 服务，并把令牌放进服务环境。

机器重启后，hosted `connect` profile 通过重新运行 `webcodex connect` 或
`webcodex runner start --profile <profile>` 来恢复。hosted profile 暂不支持开机自动
启动。

## SSH 会话资源（高级）

本地 OpenSSH 客户端可用时，Runner 会声明 `ssh_shell` capability。Workflow
Session 可以选择命名的 SSH 资源，使 `run_shell` 与 `run_job` 在 Unix 和 Windows
上都通过 Runner 自己的 OpenSSH 客户端在远程主机执行。Unix 可以复用 Runner 本地
ControlMaster 传输；Windows 的每次 one-shot/background 执行都会启动一个直接的
`ssh.exe`，不使用 `ControlMaster`、`ControlPersist` 或 `-S`。独立的
`ssh_persistent_shell` capability 允许同一资源用于 `open_session_shell`：Unix
可以复用 mux，Windows 则拥有一个直接的长生命周期 `ssh.exe` channel。 这些 SSH resource 语义不意味着 PTY/ConPTY 或额外的终端控制协议。

```toml
[ssh.resources.tmp]
host = "tmp"
default_cwd = "/opt/webcodex-edge"
```

`host` 值会传给 Runner 机器的 OpenSSH 客户端，因此 `~/.ssh/config`、密钥、
`ssh-agent`、`ProxyJump` 等配置都留在该机器上。不要把凭据、私钥或完整 SSH 配置
放进 Session 数据、Server 存储或工具输入。Session 的 `execution_context.resource`
会让 `run_shell`、`run_job` 以及受支持的 `open_session_shell` 调用通过该资源执行；
文件、Git、LSP 工具仍在本地。配置 reload 后，后续命令绑定当前 resource
generation；已经启动的 SSH 命令继续自己的有界生命周期，不会被重定向、replay
或盲目重试。

有权限的模型 client 也可以通过 MCP `ssh_resource` 工具登记 Runner-local SSH
resource。`list` 只返回安全的逻辑名称、`static|managed`、active/pending-restart 状态，
以及绑定 exact Runner 与 registry revision 的 opaque binding。`register` 接受一个用户
明确提供的 OpenSSH destination argv 与可选 default cwd；`remove` 只删除 managed
desired state。工具不会返回 raw target、用户名、地址、SSH option、credential 或
identity path。静态 `[ssh.resources.*]` 名称始终 reserved，不能通过这条路径覆盖或删除。

Managed mutation 修改的是 durable desired state，不是当前进程的 live config。返回
`restart_required=true` 时，需要重启该 Runner，再次 `list` 后才能把资源绑定到
Workflow Session。已经与 frozen startup snapshot 一致的幂等操作可以返回
`restart_required=false`。访问还需要独立的可选 `ssh:local` permission；hosted OAuth
client 通过 `webcodex connect ... --oauth-local-ssh` 显式 opt in。

Managed target 最终仍由现有 SSH transport 消费。因此，成功登记 Windows OpenSSH
destination 不代表一定能建立 PersistentShell：当前 remote persistent-shell contract
仍要求远端已有 `sh`/`bash`。Remote PowerShell PersistentShell 不属于本能力。

## LSP 导航（只读）

Runner 可以通过在仓库机器上运行的语言服务器提供只读语义导航：

| 语言 | 服务器 | 标记 |
| --- | --- | --- |
| Rust | `rust-analyzer` | `Cargo.toml` |
| Go | `gopls` | `go.mod`、`go.work` |
| Python | `pyright` | `pyproject.toml`、`setup.py`、`requirements.txt`、… |
| TypeScript / JavaScript | `typescript-language-server` | `tsconfig.json`、`package.json`、… |

工具包括 `lsp_status`、`document_symbols`、`goto_definition`、
`find_references`、`document_diagnostics`、`hover` 与 `workspace_symbols`。
独立的 `call_hierarchy` 操作在 Runner 内完成 prepare 以及有界的
incoming/outgoing 广度优先遍历；canonical Connector 将其投影为 `code_impact`，
不会暴露原始协议方法或不透明 LSP item data。
它们只读、project-bound，并且被约束为启动语言服务器绝不执行仓库代码或拉取依赖。
路径是项目相对路径；外部/依赖位置会被省略。语言服务器必须安装在 Runner 机器上，
或通过 `WEBCODEX_RUST_ANALYZER`、`WEBCODEX_GOPLS` 等 env override 指定。gopls
profile 还会关闭 module/toolchain 网络访问并使用 `-mod=readonly`；WebCodex 不会为语义
导航自动安装 gopls，也不会拉取缺失的 Go 依赖。

Call hierarchy 必须由独立的 `lsp_call_hierarchy` capability 声明，且所选语言服务器
必须提供 `callHierarchyProvider`。缺少支持会显式失败，不回退到 grep、AST、shell
或 references。

## 运维 Runner

最简命令：

```bash
webcodex runner status --profile <profile>
webcodex runner logs --profile <profile> --lines 100
webcodex runner restart --profile <profile>
```

用户服务：

```bash
webcodex runner install --scope user --config <login-reported-runner-config>
webcodex runner status --scope user --config <login-reported-runner-config>
```

管理员管理的系统服务：

```bash
sudo webcodex runner install --scope system --profile <profile> \
  --user <runner-user> --working-directory /home/<runner-user>
sudo webcodex runner status --scope system --profile <profile>
```

install、status、start、stop、restart、logs、uninstall 请使用相同的 `--scope`。
User scope 使用 `systemctl --user`；system scope 使用 `/etc/systemd/system`。

对已经运行的 Runner，修改配置时使用正式的 first-class 流程，不再查 PID 或手工发信号：

1. 编辑该 Runner 启动时绑定的现有 `runner.toml`。
2. 调用 `runner_config_check(client_id=...)`。它只读取这个绑定路径，不激活 candidate，
   返回当前 generation 以及有界的 validation/restart 元数据。
3. candidate 有效后调用
   `runner_config_reload(client_id=..., expected_generation=<current_generation>)`。
   optimistic generation fence 会在激活前拒绝 stale caller。
4. reload 后调用 `runtime_status(client_id=...)`（或 `list_runners`）检查当前运行状态。

`runner_config_reload` 不写 `runner.toml`，只激活磁盘上已经存在的 candidate。policy、
shell、configured Skill roots、Native Plugin 与静态 SSH resource 中可热加载的字段可以立即生效；`restart_required_fields`
报告的字段仍保持 startup-only，重启前不会假装已在线生效。无效 candidate 保留旧 active
snapshot 与 generation。`ssh_resource` managed mutation 不同：它使用 frozen startup
snapshot，且只在工具返回 `restart_required=true` 时要求重启 Runner。

Plugin 配置通过与 `plugin_tool reload` 相同的 validated candidate admission/commit primitive
进行 live apply，因此修改 `[plugins]` 不属于 restart-only 变更。只想 reload Plugin state 时，
仍使用权限更窄、要求 `plugin:manage` 的 `plugin_tool reload`。

Unix 上 service reload/SIGHUP 仍可作为兼容 trigger，并调用同一个 authoritative reload
primitive。Windows 与 macOS 直接使用 first-class operation，不模拟信号，也不需要 PID
管理。对于可安全分类的 validation failure，config operation 只报告闭集、非 secret 的
原子信息，例如 `field=max_concurrent_jobs` 与 `reason=out_of_range`；不会投影 raw TOML、
配置值、路径、credential、parser 文本或 shell environment value。
