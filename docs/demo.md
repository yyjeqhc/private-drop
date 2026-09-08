# WebCodex 功能演示 / Feature demo

演示页是一份可以交互的功能导览，展示项目、Runner、会话、命令输出、测试证据与代码审查。它独立于真实运行控制台，所有数据均为虚构示例。

## 打开演示

已使用本源码重新构建 Server 时，访问 `/demo`，或点击 `/runtime` 登录页的“查看演示工作空间”。现有已安装的 Server 需要重新构建才会包含新页面。

只预览前端时，在仓库根目录运行：

```bash
npm --prefix frontend ci
npm --prefix frontend run demo
```

打开 <http://127.0.0.1:4173/demo>。预览只监听本机回环地址；终端按 Ctrl+C 结束。端口被占用时可用 `WEBCODEX_DEMO_PORT=4174 npm --prefix frontend run demo` 指定端口。

也可以运行 `npm --prefix frontend run build`，直接用浏览器打开 `frontend/dist/demo.html`。请保留同目录的 `webcodex-logo.png`。

## 可以体验什么

| 入口 | 示例与操作 |
| --- | --- |
| 工作空间 | 切换 WebCodex、Atlas API、Studio UI，选择各自保留的会话。 |
| 执行过程 | 回放 3 个预设步骤；可停止回放，切换会话会取消当前回放。 |
| 执行日志 | 查看示例命令、输出、测试数量、退出码与耗时。 |
| 代码差异 | 按文件查看新增与删除行；增删统计直接来自示例 diff。 |
| 代码审查 | 接受示例变更或要求修改；待审查数量与执行记录筛选随之更新。 |
| Runner 设备 | 查看两台在线 Linux 设备、一台离线 Mac 及其保留记录。 |
| 外观与导航 | 切换深浅主题；手机可展开导航；详情标签支持方向键、Home 与 End。 |

点“重置演示”或刷新页面可恢复初始数据。主题与审查选择不会写入浏览器存储。

## 数据与实际功能的关系

这些界面用于说明项目能力，并非真实控制台的数据快照。示例会话中的 Agent 留言是固定文案，回放只展示预设步骤，不调用模型、执行命令或建立 Runner 连接。审查只更新页面内存，不提交或合并 Git 变更。

真实项目与 Workflow Session 仍通过 `/runtime` 使用已有凭证访问，项目审查台仍位于 `/console`。演示页不读取这些凭证、会话草稿或真实状态，也不请求任何 API。用户选择的机器人终端 Logo 原图位于 `frontend/src/webcodex-logo.png`，构建会原样复制到发布资源。

## English

Open `/demo` on a Server rebuilt from this source, or run `npm --prefix frontend ci` followed by `npm --prefix frontend run demo` and visit <http://127.0.0.1:4173/demo>. The preview binds only to loopback and needs no Server, credential, or Runner. You can also open the built `frontend/dist/demo.html` alongside its logo file.

Explore three fictional projects, three Runners, and five retained tasks. Switch Sessions, replay preset execution steps, inspect command output and file diffs, filter task records, and accept or request changes in a simulated review. The page supports light/dark appearance and mobile navigation.

All state lives in page memory and resets on refresh. No API calls, model inference, command execution, browser credential access, or Git operations occur. The feature tour is separate from the authenticated Runtime Console and Project Review Console.
