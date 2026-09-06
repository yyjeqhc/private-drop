# WebCodex 完整使用指南

[English](PERSONAL_SETUP.md) | [简体中文](PERSONAL_SETUP.zh-CN.md)

这条路径适合 CLI、已有 Server，或需要比 Desktop 更多控制的配置。普通 Windows / macOS 个人安装最推荐先看 [WebCodex Desktop + 官方 OpenAI Secure Tunnel](desktop-install.zh-CN.md)。无论哪种入口，完整 WebCodex 的模型都一样：ChatGPT 连接普通 Server，再由长期运行的 Runner 使用你机器上的真实项目、Git、编译器和测试工具。

如果你只想先试几分钟，不想配置长期服务，请直接看[快速试用](QUICK_START.zh-CN.md)并使用 `webcodex share`。`share` 是临时、单项目的受限体验；关闭命令后连接就会结束。

## 你最终会得到什么

```text
ChatGPT / 其他 AI 客户端
          |
          | MCP
          v
   WebCodex Server
          |
          v
       Runner
          |
          +-- 你的项目
          +-- Git
          +-- 编译器 / 测试 / 开发工具
```

Server 可以在仓库机器本身运行，也可以放在另一台机器。Runner 应运行在真正持有代码和开发环境的机器上。公网 HTTPS、Cloudflare Tunnel 或 OpenAI Secure MCP Tunnel 只负责让 AI 客户端到达 Server，不会把普通 Server 变成另一套受限执行模式。

## 1. 安装 WebCodex

Windows / macOS 用户优先按 [Desktop 安装与连接指南](desktop-install.zh-CN.md)操作：从 GitHub Release 安装 Desktop，由 Desktop 管理本机 Server + Runner，再通过官方 OpenAI Secure Tunnel 连接 ChatGPT。下面的命令行路径主要用于 Linux、已有 Server、高级配置和排障。

如果走 CLI 路径，需要 Node.js 18+ 和 Git：

```bash
npm install -g @yyjeqhc/webcodex
webcodex --version
```

## 2. 启动普通 Server

第一次个人使用可以先以前台方式运行。这样最容易理解和排查，之后再按需要改成 Linux service 或其他长期托管方式。

Linux 示例：

```bash
webcodex server init \
  --listen 127.0.0.1:8080 \
  --data-dir "$HOME/.local/share/webcodex" \
  --env-file "$HOME/.config/webcodex/webcodex.env"

webcodex server run \
  --env-file "$HOME/.config/webcodex/webcodex.env"
```

Windows PowerShell 示例：

```powershell
$envFile = Join-Path $HOME ".config\webcodex\webcodex.env"
$dataDir = Join-Path $HOME ".local\share\webcodex"

webcodex server init --listen 127.0.0.1:8080 --data-dir $dataDir --env-file $envFile
webcodex server run --env-file $envFile
```

保持这个终端运行。Linux 如果希望开机长期运行，可在首次验证成功后再看[部署指南](DEPLOYMENT.zh-CN.md)中的 service 安装方式。

## 3. 选择连接方式

这里其实有两条连接，普通用户只需要保证它们各自能到达 Server：

- **Runner / CLI → Server**：如果 Server 和 Runner 在同一台机器，可以直接使用 `http://127.0.0.1:8080`；如果不在同一台机器，就使用 Runner 能访问到的 Server 地址。
- **ChatGPT → Server**：Hosted ChatGPT 不能直接访问你电脑上的 `127.0.0.1`，因此需要稳定公网 HTTPS、Cloudflare Tunnel 或 OpenAI Secure MCP Tunnel 等可达入口。

Tunnel 只解决网络可达性。不要为了使用 Tunnel 改成 `webcodex share`；完整使用路径仍然是上一步启动的普通 Server。

普通公网 HTTPS / Cloudflare 场景中，两条连接通常可以使用同一个 HTTPS Server URL。OpenAI Secure MCP Tunnel 可以不同：Runner 仍然通过 loopback/LAN 连接普通 Server，ChatGPT 单独通过 Tunnel 访问 `/mcp`。

如果你使用 Windows + OpenAI Secure MCP Tunnel，可以参考[Windows + OpenAI Tunnel 深入配置与故障排查](WINDOWS_OPENAI_TUNNEL.zh-CN.md)。其中当前设置/排障步骤与单独标注的历史验证记录已经分层，作为需要时再看的深入材料即可。

下面用 `<server-url>` 表示 **CLI / Runner 用来访问 Server 的地址**。同机部署可以直接写 `http://127.0.0.1:8080`；已有稳定 HTTPS 时也可以直接使用该 HTTPS URL。

## 4. 创建一次性登录码

在 Server 机器上打开另一个终端，创建一个短期 pairing code：

```bash
webcodex pairing create \
  --server-url <server-url> \
  --env-file <server-init-used-env-file> \
  --username <your-name> \
  --ttl-secs 600
```

只需要把输出的 `wc_pair_...` 一次性登录码带到 Runner 机器。不要复制 Server 的管理员 token 或整个 env 文件。

如果 Server 和 Runner 是同一台机器，也仍然建议走这条登录流程：日常开发权限与 Server 管理权限会保持分离。

## 5. 登录并选择项目

在持有代码的机器上运行：

下面的示例按**普通 HTTPS MCP** 展示，因此在同一次一次性登录中带上 `--print-mcp-config`。如果 ChatGPT 将通过 **OpenAI Secure MCP Tunnel** 连接，请删掉 `--print-mcp-config`，并按 Tunnel 指南完成 ChatGPT 侧连接；不要为了打印配置再次兑换同一个 pairing code。

```bash
webcodex login <server-url> \
  --code <wc_pair_...> \
  --allowed-root /path/to/your/projects \
  --project /path/to/your/projects/my-repo \
  --print-mcp-config
```

Windows PowerShell 示例：

```powershell
webcodex login <server-url> `
  --code <wc_pair_...> `
  --allowed-root E:\git `
  --project E:\git\my-repo `
  --print-mcp-config
```

这里普通用户只需要记住两件事：

- `--project` 是你现在希望 AI 使用的项目；
- `--allowed-root` 是以后允许你继续添加项目的父目录。

不需要手工编辑 Runner 的内部配置文件。

如果以后再添加一个项目，使用 login 输出的 Runner 配置：

```bash
webcodex project register --config <runner-config> /path/to/another-repo
```

## 6. 启动 Runner

`login` 会打印 Runner 配置路径。Windows 可以直接以前台方式运行：

```bash
webcodex runner run --config <login-reported-runner-config>
```

Linux 普通用户可以先以前台验证，也可以安装 user service：

```bash
webcodex runner run --config <login-reported-runner-config>
```

或：

```bash
webcodex runner install --scope user --config <login-reported-runner-config>
```

Runner 运行后，WebCodex 才真正拥有调用本机文件、Git、编译器和测试工具的执行入口。

## 7. 把 WebCodex 加到 ChatGPT

如果你选择普通 HTTPS MCP，上面的 `login --print-mcp-config` 会在**同一次一次性登录**成功后打印连接信息。不要先兑换 pairing code，再为了拿连接信息重复执行 `login`；pairing code 是一次性的。

这段输出包含用户凭据，只应由你本人填入 ChatGPT，不要粘贴到 issue、日志或聊天消息中。在 ChatGPT 的 App / MCP 设置中创建连接，使用 CLI 打印的 MCP URL 与认证方式，然后 **Scan Tools**。

如果你选择 OpenAI Secure MCP Tunnel，登录时不需要 `--print-mcp-config` 给 ChatGPT 使用；ChatGPT 侧通常选择 Tunnel + No authentication，由本机 tunnel client 注入 WebCodex 凭据。按对应 Tunnel 指南操作，不要把本地 Bearer 复制给 ChatGPT。

## 8. 验证完整体验

先让 AI 读取项目：

```text
检查这个项目并总结结构。先不要修改文件。
```

然后再做一个小而可审查的修改：

```text
修复一个小问题，运行相关测试，并告诉我实际修改了什么。
```

完整使用路径下，AI 应能够探索和编辑项目、使用 Git、运行命令和测试、处理长时间任务，并使用代码导航能力。最终可用范围仍由你给这台 Runner 开放的本机目录/功能，以及 ChatGPT 客户端实际授予的权限共同决定。

## 临时试用和完整使用有什么区别？

| 场景 | 推荐入口 | 特点 |
| --- | --- | --- |
| 我只想几分钟体验一下一个仓库 | `webcodex share` | 一条命令、临时、单项目、关闭即失效、能力更受限 |
| Windows / macOS 日常使用 | [Desktop + 官方 OpenAI Secure Tunnel](desktop-install.zh-CN.md) | 本机 Server + Runner、多个项目、最简单的推荐个人路径 |
| CLI、Linux 或已有 Server | 普通 Server + Runner（本文） | 长期身份、多个项目、完整开发工具、网络入口可独立选择 |
| 我在维护团队/生产部署 | [部署指南](DEPLOYMENT.zh-CN.md) | systemd、Docker、OAuth、多用户和运维参考 |

Tunnel 是网络入口，不是权限模式。是否使用公网域名、Cloudflare 或 OpenAI Tunnel，不应该改变你选择“临时分享”还是“完整使用”。

## 遇到问题

优先检查三个简单状态：

```bash
webcodex server status --env-file <server-env-file>
webcodex runner status --config <runner-config>
webcodex ops status --server-url <server-url> --token-file <login-reported-webcodex-user-token>
```

更详细的错误处理见[故障排查](TROUBLESHOOTING.zh-CN.md)。命令和配置字段的完整参考见 [CLI](CLI.zh-CN.md)，但第一次成功使用 WebCodex 不需要先理解那些内部术语。
