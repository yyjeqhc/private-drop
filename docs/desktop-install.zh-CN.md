# WebCodex Desktop 快速安装与 ChatGPT 连接

[English](desktop-install.md) | [简体中文](desktop-install.zh-CN.md)

对于普通 Windows / macOS 个人用户，**最推荐的路径是 WebCodex Desktop + 官方 OpenAI Secure Tunnel**。Server 和 Runner 都留在本机，ChatGPT 通过私有 Tunnel 连接；第一次使用不需要先配置反向代理、OAuth、系统 service 或公开的 WebCodex 地址。

普通用户只需要按下面这条主链走，不需要理解 Runner registry、`runtime_project_id`、内部 MCP authorization 文件或 launchd 的实现细节：

```text
安装 Desktop
→ 准备并确认 Tunnel 配置
→ 选择真正要让 ChatGPT 使用的项目
→ 等待 Service / Runner / Project 全部就绪
→ 启动 OpenAI Secure Tunnel
→ 在 ChatGPT 填 Tunnel ID
→ Desktop 显示“Tunnel 已就绪，等待 ChatGPT”
→ 用 ChatGPT 做一次真实项目读取作为最终验收
```

**重要：**“OpenAI Secure Tunnel 已就绪”只证明本机 Tunnel 已经可以接受 ChatGPT 连接，**不等于 ChatGPT 已连接，也不等于已经可以执行项目工具**。最终是否打通，以第 8 步的真实项目读取为准。CLI、已有远程 Server、生产部署或高级网络配置再看[完整使用指南](PERSONAL_SETUP.zh-CN.md)和[部署指南](DEPLOYMENT.zh-CN.md)。

## 1. 安装 WebCodex Desktop

从 [GitHub Releases](https://github.com/yyjeqhc/webcodex/releases) 下载对应安装包：

- **Windows：**使用 Windows x64 installer。
- **macOS：**按 Mac 架构选择 Intel 或 Apple Silicon DMG。

当前 macOS 构建使用 ad-hoc 签名且没有 notarization。如果 Gatekeeper 拦截新下载构建的首次启动，进入**系统设置 → 隐私与安全 → 仍要打开**，再确认**打开**；不要全局关闭 Gatekeeper。

安装完成后启动 WebCodex Desktop。

**成功时你应该看到：**WebCodex 主窗口能够打开，首页没有安装包/运行时缺失错误。

**失败时：**macOS 被 Gatekeeper 拦截就按上面的“仍要打开”处理；安装包或 bundled runtime 缺失则重新安装同一版本，不要手工拼装内部二进制。

**下一步：**先确认 Tunnel 配置，再选择项目。不要先启动 Tunnel。

### 后台驻留与登录时启动

WebCodex Desktop 是长期运行的本机 Runtime 控制器。关闭主窗口**不会**退出应用：

- **macOS：**通过菜单栏中的 WebCodex 图标重新打开窗口。
- **Windows：**通过系统托盘中的 WebCodex 图标重新打开窗口。
- 窗口隐藏后，Desktop 自己管理的本机 Server、Runner、Regular OpenAI Secure Tunnel，以及当时正在运行的 Quick Share 都会继续运行。Quick Share 仍然只是临时会话；后台驻留不会把它变成永久服务。
- **停止本机运行环境**是用户明确选择的 desired-state 操作：它会停止本机 Runtime，并改变已保存的 Runtime 偏好；这和隐藏窗口不是同一件事。
- **退出 WebCodex**才是真正退出应用。退出时 Desktop 会回收自己拥有的进程树，不会按进程名称广泛终止不属于 Desktop 的其他 WebCodex 进程。

在**设置 → 后台与启动**中开启**登录时启动 WebCodex**后，Desktop 会向操作系统注册登录启动，并以后台方式启动，不主动显示主窗口。这个设置与 Runtime 的保存偏好相互独立：`runtime_autostart` 仍决定是否恢复已保存的本机 Runtime，已保存的连接偏好仍决定条件合适时是否恢复 Regular ChatGPT Tunnel。

## 2. 准备 OpenAI Tunnel

在 OpenAI 平台创建一个 Tunnel，并准备一个可用于该 Tunnel 的 API key：

- [Tunnels - OpenAI API](https://platform.openai.com/settings/organization/tunnels)
- [API keys - OpenAI API](https://platform.openai.com/settings/organization/api-keys)

Tunnel 名称可以自定义；记录自己的 Tunnel ID。API key 建议使用 Restricted key，只授予 Tunnels 所需的 **Read + Use** 权限。

![OpenAI Tunnels 页面](desktop-install/image-20260906171606559.png)

![OpenAI API Keys 页面](desktop-install/image-20260906171633208.png)

不要把真实 API key、WebCodex token 或 authorization 内容提交到 Git、issue、截图或聊天记录中。

## 3. 配置 Desktop 所需环境变量

Desktop 普通 Tunnel 只需要：

```text
CONTROL_PLANE_TUNNEL_ID
CONTROL_PLANE_API_KEY
```

不需要额外配置 `OPENAI_ADMIN_KEY` 或 `OPENAI_API_KEY`。Desktop 安装包当前不把 `tunnel-client` 直接塞进安装目录；首次需要 OpenAI Secure Tunnel 时，WebCodex 会自动下载并校验固定版本，所以普通用户仍然不需要手动安装。若自动下载失败，再检查网络 / 代理，或高级用户显式设置 `WEBCODEX_TUNNEL_CLIENT_BIN`。

### Windows

建议设置为当前用户的持久环境变量，然后**完全退出 WebCodex，再重新启动**：

```powershell
[Environment]::SetEnvironmentVariable("CONTROL_PLANE_TUNNEL_ID", "tunnel_...", "User")
[Environment]::SetEnvironmentVariable("CONTROL_PLANE_API_KEY", "<restricted-tunnel-key>", "User")
```

![Windows Desktop 示例](desktop-install/image-20260906171812348.png)

### macOS

下面以默认 shell 使用 zsh、变量已写入 `~/.zshrc` 为例；如果你使用其他 shell，请按实际配置调整。

从 Finder / Dock 启动的 App **不会执行 `~/.zshrc`**。仅把变量写进 `.zshrc`，Terminal 能看到，但 Desktop 不一定能看到。

临时测试可以从已经加载变量的 Terminal 启动：

```bash
source ~/.zshrc
"/Applications/WebCodex Desktop.app/Contents/MacOS/WebCodex"
```

如果希望仍从 Finder / Dock 打开，可先把当前 shell 中的值写入当前登录会话的 launchd 环境，再重新打开 Desktop：

```bash
source ~/.zshrc
launchctl setenv CONTROL_PLANE_TUNNEL_ID "$CONTROL_PLANE_TUNNEL_ID"
launchctl setenv CONTROL_PLANE_API_KEY "$CONTROL_PLANE_API_KEY"
```

如果要使用截图、窗口观察、键盘鼠标等 Computer Use 能力，还需要在 **系统设置 → 隐私与安全性** 中为实际运行 WebCodex Runner / Desktop 的进程授予 macOS 要求的权限：至少包括 **屏幕与系统音频录制（Screen Recording）**，涉及界面控制时还需要 **辅助功能（Accessibility）**。授权后通常需要重新启动相关进程才能生效；WebCodex 不会绕过或替代系统权限确认。

回到 Desktop 后查看 **OpenAI Tunnel 配置检测**：

- `Tunnel ID` 应显示**已检测**；
- `Tunnel API key` 应显示**已检测**；
- 页面只显示是否检测到，不会显示 API key value。

如果刚修改环境变量，可以点击**重新检测配置**。这个动作只重新观察**当前 Desktop 进程**能看到的环境，不会执行 `~/.zshrc`，也不会偷偷加载 secret。

如果重新检测仍显示“未检测”，请注意：**点击窗口 X 只是隐藏到菜单栏/系统托盘，不是重启。**需要通过菜单栏/托盘选择**退出 WebCodex**，确认进程真正退出，再重新启动。macOS 从 Finder / Dock 启动时仍不会读取 `~/.zshrc`；此时使用上面的 Terminal 启动或 launchd 会话环境方案。

**成功时你应该看到：**两项配置都为“已检测”，OpenAI Secure Tunnel 操作可用。

**失败时：**先点“重新检测配置”；仍缺失就完全退出 WebCodex 后重新启动。不要反复关闭/打开窗口假装重启。

**下一步：**选择真正要给 ChatGPT 使用的项目。

## 4. 启动本机运行环境并添加项目

首次启动后，选择 **Local Full Runtime / 在此电脑使用 WebCodex**，并直接选择**真正要让 ChatGPT 使用的代码仓库目录**。Desktop 会准备本机 Service + Runner，并让 Runner 加载这个精确项目。默认 workspace 只用于 Desktop 自身，不应该替代你的真实项目选择。

选择项目后才能提交配置。如果项目配置或可选的 Tunnel 启动失败，配置页会保留你的项目选择并显示错误，方便重试。

本地配对时，Desktop 会将操作系统用户名转换为 Server 接受的名称：原本合法的名称保持不变；否则 ASCII 字母转为小写，连续的不支持字符合并为一个 `-`，名称中原有的 `-` 保持原样，去掉首尾生成的 `-`，结果限制为 64 个字符；转换后为空时使用 `desktop`。这个本地配对名称不是操作系统登录身份；重启时会复用已保存的注册身份。

这是有意的安全边界：默认项目不会自动获得其他目录或整块磁盘的访问权限。

首页优先显示整体状态与下一步操作，下方的“查看项目”“管理连接”和“查看活动”可直接进入对应页面。已有默认项目时，在项目页点击“选择其他项目”进入配置，选择运行方式和工作目录后再应用；“返回运行概览”可退出配置。没有项目时，“添加项目”进入同一流程。进入或退出配置页面本身不会修改运行环境。

配置完成后，至少确认三项：

- Service：运行中 / Ready；
- Runner：已连接 / Ready；
- Project：Ready，而且显示路径就是你刚选择的目录。

如果出现“项目尚未就绪”或 `project_not_loaded`，普通用户**不需要检查 project registry**。先点击错误卡片里的**重新加载项目**；Desktop 会在有需要时只重启自己管理的 Runner，并在有界时间内重新验证同一个项目。

如果 Runtime 正在运行时选择另一个项目，也不需要先手工“停止本机运行环境”。直接选择新项目并配置；Desktop 会保留本机 Service，在需要时替换自己拥有的旧 Runner，再等待新项目 Ready。失败时页面不会继续把旧项目显示成 fake ready。

**成功时你应该看到：**Service、Runner、Project 同时 Ready，Project 路径与实际目录一致。

**失败时：**使用“重新加载项目”；仍失败再查看 Activity/错误详情。不要扩大 allowed root，也不要切换到其他 Runner 来绕过项目权限。

**下一步：**只有这三项都 Ready 后才启动 OpenAI Secure Tunnel。

![本机运行环境示例](desktop-install/image-20260906171904811.png)

## 5. 配置 Tunnel 网络

进入 **设置 → OpenAI Tunnel 网络**：

- **自动（推荐）**：优先使用 Desktop 进程继承的代理；Windows 还会检测系统代理。
- **直接连接**：不使用代理。
- **自定义 HTTP 代理**：例如 `http://127.0.0.1:7890`。

如果 Tunnel 已在运行，先停止 Tunnel，修改并保存代理设置，再重新启动 Tunnel。**不需要重启 Desktop**；每次启动 Tunnel 都会重新读取最新代理设置。

**成功时你应该看到：**网络模式保存成功；如果不需要代理，保持“自动（推荐）”即可。

**失败时：**Tunnel 网络错误优先在这里切换代理模式并重试，不要修改项目权限或 Runner 配置。

**下一步：**启动 OpenAI Secure Tunnel。

![连接页面示例](desktop-install/image-20260906172102174.png)

![Tunnel 网络设置示例](desktop-install/image-20260906173905826.png)

## 6. 启动官方 OpenAI Secure Tunnel

进入 **连接 → OpenAI Secure Tunnel**。运行成功后，Desktop 会显示类似：

> OpenAI Secure Tunnel 已就绪，等待 ChatGPT 连接

系统允许时，Desktop 会把 Tunnel ID 复制到剪贴板。

这里**不应该**因为 daemon ready 或 Tunnel ID 已复制就显示“ChatGPT 已连接”或“可以使用”。这些证据只证明 Tunnel **ready for ChatGPT**。

**成功时你应该看到：**Tunnel 本地就绪，同时整体状态仍明确提示等待 ChatGPT/需要最终验证。

**失败时：**先确认第 3 步两项配置都“已检测”，再检查第 5 步代理；按页面动作重新启动安全隧道。

**下一步：**把 Tunnel ID 填到 ChatGPT。

![Tunnel 正常运行示例](desktop-install/image-20260906174123335.png)

## 7. 在 ChatGPT 创建连接

在 ChatGPT 中创建自定义连接/应用时：

1. 选择 **Tunnel** 连接方式。
2. 填入刚才的 Tunnel ID。
3. **Authentication 选择 None / No authentication**。

这里不需要 OAuth。WebCodex 会在本机保存 MCP authorization credential，并由 Tunnel client 注入；ChatGPT 侧不需要看到这份本机凭据。

保存 ChatGPT 连接后，回到 Desktop。仅仅保存 ChatGPT 配置并不会自动把 Desktop 的本地 Tunnel 证据升级成“已连接”；如果当前版本没有稳定的外部 MCP 客户端观测信号，Desktop 会继续保守显示“等待 ChatGPT”。

**成功时你应该看到：**ChatGPT 侧连接保存成功；Desktop Tunnel 继续运行。

**失败时：**确认填入的是 Tunnel ID，而不是 API key；Authentication 使用 None / No authentication；API key 不应复制到 ChatGPT。

**下一步：**立即做一次真实项目读取。

![ChatGPT 创建连接示例](desktop-install/image-20260906174157920.png)

![Tunnel 配置示例](desktop-install/image-20260906174207352.png)

![连接完成示例](desktop-install/image-20260906174215647.png)

## 8. 最小验收

连接后先做**一个最小、真实的项目读取**，例如：“列出 WebCodex 项目，然后读取我刚选择项目中的 README”。只有这一步成功，才证明完整链路真的打通：

- 列出 WebCodex 项目。
- 读取一个文件。
- 在明确注册的项目中创建并再读取一个临时文件，然后删除。
- 执行 `git status`、`uname -a` / `ver` 等只读命令。
- 需要 Computer Use 时，尝试列出窗口或截取浏览器窗口。

如果这些都正常，说明 Tunnel、Server、Runner、项目权限和普通工具调用链已经打通。

如果 Desktop 看起来 Service / Runner / Project / Tunnel 都正常，但 ChatGPT 仍无法列项目或读取文件，**不要把本地全绿当作外部连接成功证据**。先检查 ChatGPT 中的 Tunnel ID 和连接配置，再回到 Desktop 的 Connection / Activity 查看最新状态。

## 常见问题

**OpenAI Secure Tunnel 按钮不可用**：查看 Desktop 的“OpenAI Tunnel 配置检测”。缺哪一项会明确显示；点“重新检测配置”只观察当前进程。如果变量刚设置，必须**完全退出 WebCodex 后重新启动**，关闭窗口不算退出。

**macOS `.zshrc` 已配置但 Desktop 仍检测不到**：这是正常的进程环境语义。Finder / Dock 不 source `~/.zshrc`；按上面的 Terminal 或 `launchctl setenv` 方式处理。Desktop 的“重新检测配置”不会执行 shell startup script。

**Tunnel 启动失败或连接 ChatGPT 超时**：优先检查“设置 → OpenAI Tunnel 网络”的代理；修改后停止并重新启动 Tunnel 即可。

**项目目录无法访问**：到“项目”页面显式添加对应目录，不要通过扩大默认安装目录权限来绕过项目边界。

**出现 `project_not_loaded` / 项目尚未就绪**：点击“重新加载项目”。Desktop 会重试同一个项目并只管理自己拥有的 Runner；普通用户不需要理解或手工修改内部 project registry。

**我关了窗口再打开，为什么新环境变量还是识别不到**：因为 #346 之后关闭窗口默认是隐藏到菜单栏/托盘，进程一直没退出。使用菜单栏/托盘中的**退出 WebCodex**，再重新启动新进程。
