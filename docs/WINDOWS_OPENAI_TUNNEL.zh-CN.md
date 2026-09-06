# Windows + OpenAI Secure MCP Tunnel 实操指南

[English](WINDOWS_OPENAI_TUNNEL.md) | [简体中文](WINDOWS_OPENAI_TUNNEL.zh-CN.md)

本文介绍如何在 Windows 上运行独立的前台 WebCodex Server 和 Runner，并通过 OpenAI Secure MCP Tunnel 接入，包括配置、验证和故障排查步骤。

本文来自作者的手动实操。[实操案例](#手动实操案例--2026-08-30)保留具体环境、操作经过和排障过程；创建 Connector 的步骤配有原始截图，便于对照操作。

## 适用场景

如果你的目标只是**正常长期使用 WebCodex**，先看[完整使用指南](PERSONAL_SETUP.zh-CN.md)。本文是 Windows + OpenAI Tunnel 的深入配置与故障排查指南，只有在你明确选择这条私有网络入口、需要复现实操拓扑或排查 Tunnel 问题时才需要继续阅读。

你希望 Windows 机器本身承担完整的 WebCodex runtime：

- WebCodex Server 运行在 Windows 前台；
- WebCodex Runner 作为独立 Runner 连接这个 Server；
- Server 不暴露公网端口；
- ChatGPT 通过 OpenAI Secure MCP Tunnel 访问 Server 的 `/mcp`；
- 后续可以像使用普通公网 Server + 正式 Runner 一样注册多个本机项目并进行读写。

如果只是临时分享一个仓库，优先使用更简单的：

```powershell
webcodex share --tunnel openai
```

本文重点是更底层的“独立 Server + 独立 Runner”拓扑，适合需要显式控制 runtime 生命周期或深入排查 Tunnel 的 Windows 环境。

## 整体结构

```mermaid
flowchart LR
    C[ChatGPT custom MCP app] -->|OpenAI Tunnel| CP[OpenAI control plane]
    CP --> TC[tunnel-client on Windows]
    TC -->|HTTP streamable MCP\nBearer stays local| S[WebCodex Server\n127.0.0.1:18080]
    R[WebCodex Runner] -->|WebSocket| S
    R --> P[C:\\src\\your-repository]
```

这里有两个重要边界：

1. ChatGPT 只选择 **Connection: Tunnel**，不需要拿到 WebCodex 本地 Bearer。
2. Runner 仍然是标准 WebCodex Runner；Tunnel 只解决 ChatGPT 到 MCP Server 的私有传输，不改变 Runner 连接 WebCodex 和操作 Project 的方式。

## 1. 前置条件

### WebCodex

安装当前 WebCodex，确保三个 Windows binary 来自同一版本/commit：

```powershell
webcodex --version
webcodex-server --version
webcodex-runner --version
```

不要把不同版本的 CLI、Server 和 Runner 混在一起排查网络问题；应先统一三个 binary baseline。

### OpenAI Secure MCP Tunnel

需要已经创建好的 OpenAI Secure MCP Tunnel，并在 Windows 用户环境中配置：

```text
CONTROL_PLANE_TUNNEL_ID
CONTROL_PLANE_API_KEY
```

`CONTROL_PLANE_API_KEY` 建议使用只授予 Tunnels **Read + Use** 的 Restricted key。

不要打印或提交 Tunnel ID、API key、WebCodex Bearer、bootstrap token 或 authorization-file 内容，也不要把它们放进文档、issue 或截图。

WebCodex 会使用固定并校验的 OpenAI `tunnel-client`；当前实现也可以从 `WEBCODEX_TUNNEL_CLIENT_BIN` 或 `PATH` 解析它。

## 2. 初始化 Windows 前台 Server

选择独立 env/data 路径，避免影响机器上已有的 WebCodex runtime：

```powershell
$envFile = Join-Path $HOME ".config\webcodex\openai-tunnel\webcodex.env"
$dataDir = Join-Path $HOME ".local\share\webcodex-openai-tunnel"

webcodex server init `
  --listen 127.0.0.1:18080 `
  --data-dir $dataDir `
  --env-file $envFile `
  --json
```

`server init` 会创建 Server bootstrap/admin credential，并写入私有 env 文件。不要把该 token 当成 MCP 或 Runner credential。

然后在一个保持开启的 PowerShell 窗口运行：

```powershell
webcodex server run --env-file $envFile
```

确认 Server 正在 loopback 监听并提供本地 MCP endpoint：

```text
Listening on: 127.0.0.1:18080
MCP endpoint: http://localhost:18080/mcp
```

Windows 当前支持的是**前台 Server runtime**。关闭这个终端或按 Ctrl-C 会结束 Server。

## 3. Pairing、login，并启动独立 Runner

打开第二个 PowerShell 窗口，在 Server 侧创建短期 pairing code：

```powershell
webcodex pairing create `
  --server-url http://127.0.0.1:18080 `
  --env-file $envFile `
  --username windows-user `
  --display-name "Windows Runner" `
  --ttl-secs 600
```

在 Runner 侧兑换 code。此时就注册后续要通过 Connector 使用的仓库，并建议把允许范围限制到它的真实父目录，例如：

```powershell
webcodex login http://127.0.0.1:18080 `
  --code <wc_pair_...> `
  --allowed-root C:\src `
  --project C:\src\your-repository `
  --json
```

`login --json` 会返回 `runner_config` 路径，但不会要求把 credential 粘贴给 ChatGPT。用返回的配置启动 Runner：

```powershell
webcodex runner run --config <login-reported-runner-config>
```

使用 `webcodex runner status --config <login-reported-runner-config>` 检查状态。此时 Server 应看到一个普通独立 Runner，并且 `C:\src\your-repository` 已经注册。以后需要增加更多 Project 时，再使用普通的 `webcodex project register --config ...` 流程。

## 4. 先验证本地 MCP，再启动 OpenAI Tunnel

OpenAI `tunnel-client` 的关键目标是本机：

```text
http://127.0.0.1:18080/mcp
```

WebCodex Bearer 应保留在本机，通过 file-backed `Authorization` header 注入给 `tunnel-client`；不要把这个 Bearer 复制到 ChatGPT。

启动 daemon 前先运行 `doctor`，要求它同时验证：

- Tunnel ID；
- Restricted control-plane API key；
- 本地 WebCodex MCP；
- 本地 Bearer 注入。

成功后运行 Tunnel daemon，并为它配置健康检查：

```text
--mcp.server-url url=http://127.0.0.1:18080/mcp,channel=main
--mcp.extra-headers Authorization: file:<private-authorization-file>
--health.listen-addr 127.0.0.1:0
--health.url-file <private-health-url-file>
```

WebCodex 的 `share --tunnel openai` 会自动完成相同类别的准备、`doctor` 和 `/readyz` 等待。手工拓扑只建议用于本文这种独立 Server/Runner 场景或排障。

## 5. `/readyz` 正常但 ChatGPT 创建 Connector 失败

`/readyz = 200` 只说明本地 Tunnel/MCP 一侧能够通过健康检查，并不能证明 `tunnel-client` 已经能够获取 OpenAI Tunnel metadata 并持续完成 control-plane poll。

如果 ChatGPT 仍然创建失败，应检查 `tunnel-client` 日志中的 metadata/poll failure。浏览器能够联网也不能证明该进程使用相同 proxy、DNS 或 IPv6 路径。

排障时按两条链路分别判断：

```mermaid
flowchart TD
    A[tunnel-client /readyz = 200] --> B[MCP session initialized]
    B --> C{OpenAI control-plane poll 可达?}
    C -->|否| D[ChatGPT 能看到 Tunnel 元数据\n但创建 Connector 失败]
    D --> E[检查 tunnel-client 日志]
    E --> F[检查进程 proxy / DNS / IPv6 路径]
    F --> G[需要时为 control-plane 单独设置 HTTP proxy]
    G --> H[metadata fetched + poll 正常]
    H --> I[Connector 创建成功]
```

如果该主机需要 HTTP proxy，先确认 proxy 本身能够访问 `api.openai.com`，再只把 **control-plane** 流量交给它：

```text
--control-plane.http-proxy http://127.0.0.1:<proxy-port>
```

修改 route 后，应同时确认本地 readiness 与 control-plane metadata/poll 都正常，再重试创建 Connector。

## 6. 在 ChatGPT Developer Mode 创建 Connector

在 ChatGPT 的新插件/custom MCP app 页面：

1. 名称：任意，例如 `WebCodex Windows`；
2. 连接选择 **隧道 / Tunnel**；
3. 从可用 Tunnel 中选择对应的 WebCodex Tunnel；
4. 身份验证选择 **无身份验证 / No authentication**；
5. 确认自定义 MCP 风险提示；
6. 点击创建。

为什么是 **No authentication**？

因为 OpenAI Tunnel 到本机 WebCodex MCP 的 Bearer 已由 `tunnel-client` 在本机注入。ChatGPT 不应该拿到或保存该 Bearer。

### 创建与连接的截图示例

以下是作者在 2026-08-30 手动操作时的截图。图中的 `tunnel-test` 是示例名称，界面文案和工具列表可能随版本变化。

创建 app：选择隧道与无身份验证，确认后点击创建。

![创建 MCP app 时的隧道和身份验证选项](WINDOWS_OPENAI_TUNNEL.zh-CN/0eeb07fe3df699b66c7fc2d592d9d4cf.png)

在添加到 ChatGPT 的确认窗口中点击“连接”。

![将 tunnel-test 连接到 ChatGPT 的确认窗口](WINDOWS_OPENAI_TUNNEL.zh-CN/fdeaad5c8f257517d1f556f090c9a37c.png)

打开 app 详情检查工具是否已加载；图中展示的是当时的 `apply_text_edits` 工具说明，实际工具以当前 Server 为准。

![连接后的 app 详情和工具列表](WINDOWS_OPENAI_TUNNEL.zh-CN/06653330b9a154a1ed60a481afa6efb7.png)

## 7. 通过 Connector 做端到端验收

Connector 创建成功后，如果当前对话还没出现新工具，先刷新 ChatGPT 窗口。然后验证真正的数据路径，而不只是 UI 创建成功：

1. 确认 `webcodex login --project ...` 阶段已经注册的 Project 可以看到；
2. 选择这个已注册 Project，并通过 Connector 读取一个已知文件；
3. 如果明确启用了写权限，用专用分支/安全小改动验证写入，并检查 Git diff。

WebCodex 返回的 Project handle 是结果，不是设置时需要用户自己编造的输入；不要让用户手工拼 Runner/project runtime id。

## 8. 推荐的验收清单

不要把“进程启动了”当成最终成功。至少验证以下层级：

| 层级 | 应看到的证据 |
| --- | --- |
| Windows Server | loopback listener 正常，`/mcp` 可达 |
| Runner | `actual_transport=websocket`，Runner online |
| Local MCP | `tunnel-client` 显示 `mcp session initialized` |
| Tunnel health | `/readyz = 200` |
| OpenAI control plane | metadata fetch 成功，poll 无持续失败 |
| ChatGPT | Connector 可以创建并 Scan/加载 tools |
| Project | login 阶段注册的 Project 可以通过 Connector 看到 |
| Read | 可以通过新 Connector 读取仓库文件 |
| Write | 可以在专用分支创建/修改文件，并用 Git diff 验证 |

只有最后几项也通过，才说明它真的具有和普通 WebCodex Server + Runner 相同的开发使用体验。

## 9. 常见误区

### `/readyz = 200` 就代表 Tunnel 完全可用

不是。还必须确认 OpenAI control-plane metadata/poll 正常。

### ChatGPT 需要填写 WebCodex Bearer

OpenAI Secure MCP Tunnel 模式不需要。选择 **No authentication**，Bearer 保留在本机。

### Windows 一定需要 WSL 或 Windows Service

不需要。当前支持 Windows 前台 Server 和前台 Runner。缺点是终端关闭后进程也会结束；Windows managed service lifecycle 仍未提供。

### `share --tunnel openai` 和本文手工拓扑完全不同

底层核心链路相同：本地 WebCodex MCP + 本地 Bearer 注入 + OpenAI `tunnel-client`。区别是 `share` 自己管理临时 Server/Runner/session，而本文显式拆开 Server 和 Runner，便于验证长期拓扑和排障。

## 手动实操案例 — 2026-08-30

下面保留作者亲自完成的一次 Windows 接入过程，包括操作环境、项目读写验证和网络故障的解决经过。版本、机器路径、app 名和分支用于交代案例背景；复现时应使用自己的配置，并按前文准备当前版本。

### 操作环境

当时的环境是：

```text
WebCodex 0.3.9
commit 1fa862829122
Runner client_id=tutorial-msi-runner
preferred/actual transport=websocket
ChatGPT app=tunnel-test
```

### 注册项目并验证读写

独立 Runner 最初没有注册 Project，之后通过 Tunnel Connector 真实注册并访问了：

```text
E:\git\petal-meadow-3d
E:\git\webcodex
```

同一个 Connector 还用于创建本文最初的文档分支：

```text
docs/windows-openai-tunnel-guide
```

这次真实运行因此验证了：ChatGPT 可以经过 OpenAI Tunnel 到达 Windows 前台 Server，再到独立 WebSocket Runner，并完成真实 Project 注册、仓库读取和仓库写入。

### 从创建失败到连接成功

第一次创建 Connector 时还出现了一个重要网络故障：本地 MCP 已初始化、`/readyz = 200`，但 OpenAI control-plane poll 仍失败，因为该 Windows 进程没有继承浏览器使用的 Clash 路由，`api.openai.com` 的直连 DNS/IPv6 路径不可用。当时验证可用的 Clash HTTP proxy 是：

```text
http://127.0.0.1:7890
```

只把 control plane 路由到该 proxy：

```text
--control-plane.http-proxy http://127.0.0.1:7890
```

之后 Tunnel log 显示 healthy proxy route 与正常 metadata/poll，Connector 创建成功。这是当前“浏览器联网和 `/readyz` 都不能单独证明 `tunnel-client` control-plane 可达”这一排障规则的历史证据。

## 相关文档

- [部署指南](DEPLOYMENT.zh-CN.md)
- [MCP](MCP.zh-CN.md)
- [CLI](CLI.zh-CN.md)
- [故障排查](TROUBLESHOOTING.zh-CN.md)
