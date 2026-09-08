# Coding 工作流

[English](CODING_WORKFLOW.md) | [简体中文](CODING_WORKFLOW.zh-CN.md)

本文面向普通 WebCodex coding/review 工作，只说明模型真正需要遵循的流程，不展开内部 continuity、audit 或 transport 协议。

## 普通循环

日常 WebCodex coding loop 应该保持很小：

```text
work_on_project
→ inspect / search / read
→ edit
→ focused validation
→ review changes
→ finish_coding_task
```

`work_on_project` 是普通 coding/review 的 canonical bootstrap。把当前任务 instruction 交给它，然后遵循连接到的 Server 返回的 project instructions 与 tool surface。

## 开始或继续任务

新任务和显式 continuation 都使用 `work_on_project`。WebCodex 会保留有界 Workflow Session evidence，让 validation、review 与 handoff 可以指向同一轮工作，但 Workflow Session 不是认证凭据，也不会扩大 project authority。

普通使用不需要理解 WebCodex 内部的 continuity/audit field；这些属于 implementation/maintainer contract。

内置工作流为所有任务提供默认 guidance，不要求先指定角色：核对目标和适用规则、保留已有工作、完成已授权的实现、按改动范围验证、观察已有 Job 而不重复执行，以及如实报告证据。只使用当前暴露的 schema 支持的工具与协议字段。

Behavioral role 在默认原则上增加侧重点，写在 task instruction 里即可，例如实现任务：

```text
使用 implementation_owner guidance。实现 <任务>，运行聚焦 validation，
并审查最终 diff。
```

独立评审：

```text
使用 independent_review guidance。独立评审 <改动或 commit>，
报告有文件/行号证据和影响说明的具体发现，不修改文件。
```

如果也希望修复，明确补充“修复具体发现，并运行聚焦回归验证”。单独指定评审角色不代表授权修改。

Guidance 通过工具结果交给客户端，不是客户端的 system prompt，也不会授予执行权限。Host 指令、用户任务、适用项目规则、认证和运行时安全策略仍然有效。返回 guidance 不等于模型已经读取、记住或遵守；只有当前模型上下文仍保留内容时才应关闭其返回。

## 编辑前先检查

能够表达任务时，优先使用 structured project search/read，而不是 shell。只读取理解当前改动所需的文件和范围，并保留 workspace 中已经存在的无关工作。

Bootstrap 只读取固定的几个指令入口，不会扫描所有子目录规则。修改某个路径前，需要检查适用的子目录指令，并补读相关缺失或被截断的规则内容。

做 branch/PR review 时，先使用当前 Server 提供的有界 review/change-summary 工具，再按需要缩小到具体文件或 diff hunks。

## 编辑

模型生成的普通编辑，在 `read_file`/`read_files` 已经拿到当前文件内容和 SHA 时，canonical/default 路径是 `apply_text_edits`。现有文件把读取结果中的 SHA 作为 `expected_sha256`；exact selector 默认要求唯一，需要显式消歧时再使用 `line_scope`/`occurrence`。即使一次修改很多行，默认路径仍然不变；“改动行数多”本身不是选择 `apply_patch` 的理由。只有当 contextual patch 明显更自然、large/multi-hunk rewrite 用 guarded exact edit 表达明显笨重，或 patch-style context 本身更清楚地表达修改关系时，才使用 `apply_patch`。对于 repetitive code，每个 patch chunk 都必须带稳定且唯一的 surrounding context，优先使用 containing function / impl / type / test / module；不要只拿重复出现的单行或短片段作为 mutation anchor。默认 `matching_mode=unique` 仍要求唯一 mutation target。只有明确需要 stale-context/concurrency fence 时才使用 `matching_mode=exact_unique`。输入本身已经是标准 unified diff 时才使用 `apply_unified_diff`。

Guard failure 是 **zero-write conflict**，不是削弱 guard 的理由。重新读取当前源码，并基于最新状态重新生成原本的编辑。

遇到 `matching_mode_rejected` 时，保持 matching guard，不要切换到 `first_match`。先重新读取当前源码；如果已经有 current source + SHA，而且原本修改很容易表达成 exact edit，优先转为 `apply_text_edits`。如果 patch 形式仍明显更合适，则消费返回的有界 `recovery.action=read_files` / `recovery.items`，并保留原请求的 patch guard：原来是 `matching_mode=unique` 时，补充稳定且唯一的上下文后仍以 `unique` 重试；原来是 `exact_unique` 时，基于 exact current source 重新生成并继续使用 `exact_unique`。不要降级明确的 stale-context/concurrency fence。

对于确定性的 `context_mismatch`，同样消费有界 `read_files` recovery，并基于 current source 重新生成 patch；不要盲目重复相同 patch。若结果是 `outcome_unknown`，先检查 workspace，再决定是否允许任何写入重试。

具体 matching metadata 与 transactional protocol 属于维护 WebCodex 本身时才需要的细节，应以 tool contract/tests 为准。

## Validation

能使用 `cargo_test`、`cargo_check`、`go_test` 等 structured validation 时优先使用它们。先运行能够发现当前回归的最小检查，只有实际受影响的边界需要时才扩大范围。

如果某次 test invocation 必须证明“测试确实执行了”，使用 `require_tests: true` 或 `min_tests: N`。它们是本次调用的 evidence assertion，不会自动变成 Workflow Session 的持久要求。如果 validator execution 成功，但请求的 test 数量未满足或无法证明，closeout 会把这次调用保留为 evidence gap，而不是代码/测试 correctness failure。否则，exit-zero 但合法运行零个 test 只是 execution result，并不能证明 test coverage。

只有 structured validation 无法表达检查时，才使用 shell/process escape hatch。

## Review 与 closeout

编辑和 validation 之后，必须检查真实 workspace/diff。Tests 通过不能替代 diff review；反过来，diff 看起来合理也不能替代行为变化所需的 focused validation。

`finish_coding_task` 返回有界 closeout evidence。把它当成 advisory summary，不要把它当成“任务已经正确完成”的 authority。最终工程判断仍由模型完成并向用户报告。

## 长时间运行的工作

命令或 validation 超过同步等待窗口时，会作为同一条 WebCodex Job 继续执行。观察该 Job，不要再启动一个副本。Tool 返回的 recovery/continuation hint 只是下一次显式调用的 guidance；WebCodex 不会对不确定 effect 做隐藏 retry。

## 手动多窗口协作

多窗口协作属于高级 maintainer workflow，不是普通 coding loop。独立 writer 应使用不同 worktree/Project，并保持各自 Workflow Session 分离；使用当前 Server 返回的 assignment/completion 工具，不要复制另一个窗口的 execution history。

精确的 concurrency、retry、provenance 与 cross-Session authorization 规则见 [Manual Multi-Window Collaboration](agent/manual-window-collaboration.md)。对应 protocol field 有意不放在普通工作流里。

## 如何判断是否有效

运行时测试可以证明 guidance 的返回一致、有界、符合 schema，且不会变成执行权限。`scripts/eval_coding_loop.sh` 检查的是脚本化工具循环，没有运行模型，不能衡量模型是否遵守提示词。

要衡量行为收益，应固定模型、工具、参数与任务样本，对比有无 guidance 的多次运行。样本至少包括小型修复、只读评审、已有无关改动、子目录规则，以及结果不确定的长时间操作。先比较正确性与任务范围保持，再比较不必要的询问、重复执行、验证质量、工具调用和 token 成本。不能从 schema 测试通过推断模型成功率提升。

## 内部协议细节

开发 WebCodex 本身时，直接阅读 maintainer contract，而不是继续扩充这份普通用户指南：

- [Session model](agent/session-model.md) —— Workflow Session continuity、message 与 evidence 语义。
- [Authority model](agent/permission-model.md) —— execution authority 与 hard-safety layering。
- [Job reliability and concurrency](agent/job-reliability-and-concurrency.md) —— Job recovery/observation contract。
- [Architecture decisions](agent/architecture-decisions.md) —— 当前长期有效的实现决策。

普通 coding client 不应为了完成日常仓库任务而必须理解这些内部文档。
