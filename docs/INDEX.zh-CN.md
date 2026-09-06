# 文档索引

[English](INDEX.md) | [简体中文](INDEX.zh-CN.md)

按你现在想完成的事情选择文档即可。

## 我想正常使用 WebCodex

- [README](../README.zh-CN.md) —— WebCodex 能做什么，以及完整使用和临时试用的区别
- [完整使用指南](PERSONAL_SETUP.zh-CN.md) —— **推荐入口**：普通 Server + Runner + 项目 + ChatGPT
- [AI 辅助接入](AI_ONBOARDING.zh-CN.md) —— 让 AI 帮你按普通用户语言完成配置
- [MCP](MCP.zh-CN.md) —— ChatGPT、Claude 与其他 MCP 客户端

## 我只想先试几分钟

- [快速试用](QUICK_START.zh-CN.md) —— 一条 `webcodex share` 临时体验一个仓库

## 我需要生产部署或深入排障

- [部署指南](DEPLOYMENT.zh-CN.md) —— systemd/Docker、多用户、自托管和长期运维
- [Windows + OpenAI Secure MCP Tunnel 深入实操](WINDOWS_OPENAI_TUNNEL.zh-CN.md) —— 独立 Windows Server/Runner、Tunnel 与真实故障排查
- [Runner](RUNNER.zh-CN.md) —— Runner 运维参考
- [CLI](CLI.zh-CN.md) —— 完整命令、配置和凭据参考

## 我需要认证或网络配置

- [MCP](MCP.zh-CN.md) —— Bearer、query-token 兼容方式、OAuth、私有隧道和 MCP 参考
- [认证模型](AUTH_MODEL.zh-CN.md) —— 详细凭据和权限边界
- [部署指南](DEPLOYMENT.zh-CN.md) —— 稳定 HTTPS、自托管和生产网络配置
- [GPT Actions](GPT_ACTIONS.zh-CN.md) —— 可选的 OpenAPI Custom GPT 集成

## 我遇到了问题

- [故障排查](TROUBLESHOOTING.zh-CN.md) —— 安装、连接、运行和 Runner 问题
- [安全说明](../SECURITY.md) —— 安全模型和使用建议

## 我想理解或扩展 WebCodex

- [架构](ARCHITECTURE.md) —— 主要组件如何协同
- [Coding 工作流](CODING_WORKFLOW.zh-CN.md) —— 任务启动、指导、验证和收尾
- [Native Tool Plugins](PLUGINS.zh-CN.md) —— 用任意可执行语言为 Runner 增加本地工具，不需要 MCP SDK
- [Computer Use 架构](COMPUTER_USE.md) —— 桌面自动化能力、设计边界与扩展原则

## 我想参与开发或发布

下面 `docs/agent/` 中的页面属于 maintainer/internal contract。它们会有意保留
protocol field、兼容名称和实现 invariant；普通用户不需要为了使用 WebCodex 而学习
这些内容。

- [AGENTS.md](../AGENTS.md) —— 面向 coding/AI agent 的仓库开发指引
- [仓库维护](MAINTENANCE.md) —— 文档保留标准、维护队列、依赖更新与双语文档规则
- [测试策略](TESTING.md)
- [发布清单](RELEASE_CHECKLIST.md)
- [架构决策](agent/architecture-decisions.md)
- [Runtime host context](agent/runtime-host-context.md) —— Runner 配置的 planning context 与 runtime diagnostics
- [Job 可靠性与 Runner 并发](agent/job-reliability-and-concurrency.md)
- [Tool request tracing](agent/tool-request-tracing.md) —— maintainer forensic payload/correlation contract
- [权限模型](agent/permission-model.md)
- [会话模型](agent/session-model.md)
- [手动多窗口协作](agent/manual-window-collaboration.md)
- [OpenAPI 指南](agent/openapi-guidelines.md)
- [发布流程](agent/release-process.md)
