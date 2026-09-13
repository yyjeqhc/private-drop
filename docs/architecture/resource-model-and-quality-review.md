# WebCodex 资源模型与架构质量探索

状态：探索性评审，不是新的运行时契约。除文末列出的启动目录投影重构外，本文提议均未实现，也不改变现有权限、协议、执行或部署行为。

评审日期：2026-09-12。精确源码基线：`3d8332190c336fc7becaa0ae65700e0fd08cb43d`。分析在 special 主项目的独立 managed worktree 中进行，分支为 `explore/architecture-resource-boundaries`。下文源码行号指这一基线；符号名用于后续定位。

## 1. 总体判断与范围

WebCodex 不缺架构：Server/Runner 分离、分层 workspace、统一 ToolDefinition、RunnerOperation、事务式 Memory、精确 Plugin binding、独立 Workflow Session/Job/AgentTask 都已经存在。继续增加一层通用框架，未必比收拢现有概念更好。

当前最值得投入的是：**统一观察与描述，收敛规则所有权，保留业务状态机，降低模型决策成本。** 目标应是以后新增一种资源或一种工具时，更少触碰不相关模块，而不是让所有对象实现同一个 CRUD 接口。

本轮按边界抽样追踪了 workspace 策略、工具契约/Kernel/MCP、Memory/Skill/Plugin、启动与上下文投影、执行预算、存储、卡片投影及相关测试。没有逐行审计所有代码，没有验证所有前端页面、Windows/macOS、真实断网/重启或部署场景，也没有做全仓性能画像。未发现并复现可在此直接定级为安全漏洞的问题；下面的维护风险、性能假设与功能提议分别标注。

## 2. 应保留的结构，而不是推倒重写

| 已有结构 | 源码证据 | 判断 |
|---|---|---|
| 17 个 workspace package 的依赖策略 | [workspace-boundaries.toml](../../workspace-boundaries.toml)、[检查器](../../scripts/workspace_boundary_check.py)，本轮实际检查通过 | 分层已有机器约束，不需要再发明一套目录约定替代它 |
| 统一工具定义 | [ToolDefinition](../../crates/webcodex-tool-contracts/src/tool_definition.rs)，698–712 | 审计、策略、模型暴露、Session evidence 已有合理归属，应沿此继续收敛 |
| 线协议与内部语义分离 | [RunnerOperation](../../crates/webcodex-core/src/runner_operation.rs)，1–6、30–44 | 在边界解码旧 DTO，内部携带类型化操作；比重写全部 wire protocol 更稳健 |
| 描述与执行权限分离 | [PluginBinding/PluginOperation](../../src/plugin_gateway.rs)，27–34、135–187 | describe 不是执行授权，metadata hint 不是权限；必须保留 |
| Memory 事务与条件更新 | [set_project_memory_attributed](../../crates/webcodex-store/src/memory.rs)，962–1054 | 内容相同的幂等重试、CAS、实例/代际身份不能简化为一个内容哈希 |
| 语义不同的任务对象独立 | [Durable Agent 设计](durable-agent-runtime.md)、[Session 模型](../agent/session-model.md) | AgentTask、Workflow Session、Job、ClientWindow 不是四种同名 Task |
| 有界观察而非完整转储 | [context_projection](../../src/tool_runtime/context_projection.rs)、[startup_brief](../../src/tool_runtime/startup_brief.rs) | 模型上下文是受预算约束的投影，不能当完整数据库或权限凭证 |

建议继续采用模块化单体与明确 Runner 执行边界。当前没有测量证据支持为了代码量而拆微服务；那会把本地调用复杂度换成部署、网络失败、分布式事务和版本协调复杂度。

## 3. Memory / Skill / Plugin 可以统一成什么

### 3.1 不要把五个维度揉成一条继承链

`runner 级 / project 级 / 用户自定义级` 表达了真实需求，但包含不同维度：

| 维度 | 回答的问题 | 例子 |
|---|---|---|
| Kind | 这是什么？ | Memory、Skill、Plugin；以后可能是 WorkflowTemplate |
| Owner / authority domain | 谁持有它并决定访问、变更？ | Control 存储域、一个 Runner、一个受权主体 |
| Applicability / visibility | 在哪个上下文适用、可见？ | 某 Project、某 Runner 的项目、显式选择的个人空间 |
| Origin / provenance | 从哪里来，谁维护？ | 仓库文件、operator 配置目录、安装包、用户编写 |
| Lifecycle / operations | 它怎样变化、能做什么？ | 读文本、CAS 更新、安装并激活、describe 后调用进程 |

“用户自定义”首先可能是来源，也可能是未来独立的用户私有命名空间，不能不加区分地排在 Project 或 Runner 之上。用户写的 Skill 可以存在项目目录，也可以由 operator 安装到 Runner；位置与作者不等价。审计记录中的 principal 归因也不自动等于个人隐私隔离。

权限是这些条件与现有授权策略的交集，不是“更具体的 scope 覆盖上一级”。默认行为应是保留候选、标记冲突、让模型或用户选择精确身份，而不是隐式 last-wins。

### 3.2 当前支持情况，不把提议误写成现状

| 资源 | 当前存放与作用域 | 读取/使用方式 | 生命周期 |
|---|---|---|---|
| Project Memory | Control 数据库；scope 由 Project runtime id、Runner client id、注册根目录共同派生 | memory_search / memory_read；读取还受 project 与 Memory scope 权限约束 | 行记录、定义哈希、实例身份、generation/CAS；非 Runner 文件 |
| Project Skill | 项目 `.agents/skills` | skill_list / skill_read_file；正文需显式读取 | 仓库活文件，definition revision |
| Runner configured Skill | Runner 的配置 roots | 同一 Skill 发现入口，Runner 解析 opaque id | 活目录；不等于已安装不可变包 |
| Runner managed Skill | Runner 管理的 Skill store | 同一发现入口；安装/版本/激活/删除由管理操作负责 | 包 revision、active pointer、state revision、幂等记录 |
| Native Plugin | Runner 持有进程与配置；项目启动目录只收录 cwd 精确匹配该项目根目录的 provider | plugin_tool list / describe / call；binding 固定实例与 schema | 进程实例、冻结 catalog、显式 reload、结果不确定性 |
| 用户私有资源空间 | 本轮没有确认一个覆盖上述三种资源的统一实现 | 需要独立设计 principal namespace、分享和撤销 | 不是给 source_scope 增加一个字符串就完成 |

证据：[Memory scope](../../src/tool_runtime/memory.rs)，38–56；[Skill descriptor/locator](../../src/tool_runtime/skills.rs)，61–94、562–638；[Skill store](../../crates/webcodex-core/src/skill_store.rs)，35–103；[Plugin project catalog](../../crates/webcodex-runner/src/webcodex_runner/plugin.rs)，392–490。

### 3.3 推荐的共同边界是只读目录，不是万能 ResourceManager

概念结构如下，仅表示设计方向，不是新公开 API：

```text
Memory provider    Skill providers    Plugin provider
       \                |                 /
        existing access checks + domain observations
                          |
           bounded metadata projection / catalog
                          |
         id + kind + source + applicability + revision
         completeness + diagnostics + next read/describe
                          |
           domain-specific read / describe / mutation
```

共同描述层可以负责：身份与来源说明、可见性原因、描述长度与预算、空/不可用/不完整状态、分页、稳定顺序、下一步入口。它不能负责启动 Plugin、修改 Memory、自动执行 Skill、生成执行授权，或为所有领域定义相同的 revision。

现有 [ContextMaterialSpec](../../src/tool_runtime/context_projection.rs)，19–80，已经是一个合适的起点：材料 key、project required、scope policy、surface。不要在旁边另建一套功能重叠的总注册表。先让现有启动目录和 context sidecar 共用小型投影原语；在确有第三个消费者时再决定是否提炼 provider 接口。

### 3.4 三阶段模型

1. **Discover**：拿到小目录与可选操作，尚未读取正文或获得执行能力。
2. **Materialize / bind**：Memory/Skill 读取正文；Plugin describe 固定 schema 与实例。二者不能伪装成相同效果。
3. **Act**：使用现有具体工具更新 Memory、安装 Skill 或调用 Plugin；每次仍经原有权限与执行链路。

这也解释了为什么 Skill 不是 Plugin 的子类：Skill 是交给模型的指导，Plugin 是可执行 provider。统一“它们可发现”不意味着统一“它们可运行”。

## 4. 优先改进项与源码证据

### A. 收敛扩展家族分类的所有权（后续 consolidation 已落地）

最初探索时，Skill/Memory 的 runtime/management 分类分别由实现模块中的 `is_*_tool_name` helpers 持有，Kernel、发现 surface、MCP adapter 与 registry 又各自消费或重复维护这些名字集合。这不是已证明的权限漏洞，而是“新增工具时容易改漏一处”的结构风险。

后续 consolidation 已将这一个静态事实收敛到 canonical `ToolDefinition`：`ToolOperatorExtensionFamily` 只描述 `SkillRuntime / SkillManagement / MemoryRuntime / MemoryManagement / TraceDiagnostics` 的 Stateless Operator protocol admission family，各定义在 Skill、Memory、diagnostic 的 ToolDefinition 旁显式声明。Registry、Kernel capability gate、tool manifest surface 和 MCP Full Operator projection 统一通过 `runtime_tool_operator_extension_family` 消费；旧 Skill/Memory classifier 和 registry 中对应的硬编码 family name sets 已从 live code 删除。

证据：[tool_definition.rs](../../crates/webcodex-tool-contracts/src/tool_definition.rs)、[tool_policy.rs](../../crates/webcodex-tool-contracts/src/tool_policy.rs)、[tool_specs.rs](../../crates/webcodex-tool-contracts/src/registry/tool_specs.rs)、[kernel.rs](../../src/tool_runtime/kernel.rs)、[surface.rs](../../src/tool_runtime/surface.rs)、[mcp/tools.rs](../../src/mcp/tools.rs)。

这个收敛没有把 authorization 混入 family：Skill Management 的 admin 要求、Memory conjunctive scopes、Project/Runner authority、permission 与 Session evidence 仍由原 canonical policy 所有；Goal Plan、Work Result、Agent Continuation 的 MCP App/Host capability 也保持独立。Definition invariant 还要求 operator-extension family 必须保持 `ModelHidden`，避免普通 model-visible 工具因误标 family 同时进入通用 registry 与 extension projection。

### B. Skill 发现与精确读取耦合过紧（P1 设计风险；延迟尚未量化）

`discover_project_skills` 逐个读取包定义；`discover_skills` 依次观察项目、configured、managed 来源；`skill_read_file` 也先重建全部目录再按 id 定位。某可选来源观察失败会让整体目录不可用。证据：[skills.rs](../../src/tool_runtime/skills.rs)，562–575、788–803、1376–1416。

注意：启动层的 Skill 和 Plugin 已经并行等待，见 [coding_task.rs](../../src/tool_runtime/coding_task.rs)，1232–1269；不能把它描述为整个启动串行。

可行顺序：先按来源记录扫描次数、条目数、耗时、字节量和不可用原因；再分离目录观察与已知引用解析；最后才考虑有界并发或缓存。缓存键至少绑定 Project authority、Runner 实例、source/配置版本及适用 revision。缓存 descriptor 不是缓存授权，已移除资源不能被缓存复活；不通过猜 opaque id 格式路由。

将来源失败降级为 partial catalog 会改变外部语义，应单独评审并显式报告 incomplete；本轮没有把失败偷偷转换成成功空目录。

### C. 区分仓库知识身份与执行 worktree 身份（P1 产品设计）

当前隔离有理由：新 managed worktree 是普通新 Project，Memory scope 不等于主工作区；Plugin 的 cwd 也不会自动指向新 worktree。因此同仓库分析中看到空目录，不一定是 provider 不工作。

可以增加显式、只读的知识关联，例如“本 worktree 可引用 source repository 的指定 Memory/Skill 快照”。它应包含来源、引用版本、适用范围、撤销/失效及审计；读取源知识仍须授权。不应仅凭相同 Git remote URL 判定同一仓库，也不能顺带继承主工作区写权限、Session guard 或 Plugin 执行 cwd。

这属于可选新能力，不能冒充纯重构。首先改善解释性诊断：资源属于哪里，为什么在当前 worktree 不出现，以及应在何处配置；不枚举用户无权得知的资源。

### D. ToolRuntime 是逐渐膨胀的组合对象（P2，维护风险）

[ToolRuntime](../../src/tool_runtime/runtime.rs)，106–177，持有多个 gateway、Session、执行预算、reconciliation 锁、观察组件和数个数据库注入字段。它作为 composition root 合理，但所有领域方法都依赖整个对象，会扩大可触达状态和测试装配面。

优先让一个具体用例依赖小而明确的服务集合，例如 SkillCatalogService 只借用所需 Runner/catalog 端口，而不是立刻拆成很多新 crate 或制造动态 ServiceLocator。先抽纯投影与解析，再处理有状态服务；明确锁、缓存、超时由谁持有。

### E. 存储访问需要测量隔离，不宜先换数据库（P2，性能假设）

[Database](../../crates/webcodex-store/src/lib.rs)，94–100，当前使用 `Mutex<Connection>`；[Memory store](../../crates/webcodex-store/src/memory.rs)，935–990，有同步加锁查询/事务。共享连接可能造成不同领域操作排队，但本轮未测得实际锁竞争、阻塞时间或吞吐问题。

先量化 DB lock wait、statement/transaction duration 和各域请求占比。若确有运行时线程阻塞，可评估有界 blocking worker 或数据库 actor；队列、取消和事务完成后的未知结果必须有明确定义。拆线程或连接不等于提升所有负载，更不能破坏当前 CAS/重放事务边界。

### F. 表达规则和投影预算有小规模重复（P2，本轮验证一部分）

基线 `StartupSkillsCatalog` 和 `StartupPluginsCatalog` 重复了相同的状态、revision、count、truncated、entries、hint 外壳，以及逐条试装入 JSON 字节预算的算法；只有条目类型、上游完整性和文案不同。[startup_brief.rs](../../src/tool_runtime/startup_brief.rs)，139–293。

适合提炼的是启动专用的泛型目录投影，不是引入所有资源共用的存储、动态 provider trait 或统一执行方法。这是一个有两个真实消费者、可用差分测试约束的抽象。

### G. 字符串错误分类应停留在边界（后续 Runner Skill provider 小切片已落地）

最初探索时，[runner_skill_store_request](../../src/tool_runtime/skills.rs) 通过 `error.contains(...)` 区分 capability、exact Runner 变化与一般不可用，内部文本变化可能改变模型侧 error kind。后续小切片已经为 Runner Registry 的 managed Skill-store 与 configured Skill roots enqueue 分别增加 `EnqueueSkillStoreError` / `EnqueueConfiguredSkillRootsError`，Tool Runtime 通过 typed variants 映射回各自既有稳定 error kind，不再解析这两个 provider 的 presentation text。

原有 `enqueue_skill_store -> Result<_, String>` 与 `enqueue_configured_skill_roots -> Result<_, String>` public entry points 仍作为文本兼容边界保留，内部需要分类的调用走对应 typed entry point；因此这不是全仓错误框架重写，也没有借机修正或重新命名既有 wire/model-facing 错误语义。

同理，内容 revision、catalog revision、状态 CAS、Job observation token、Session context ACK 虽然都长得像字符串，不应共用“版本号”的业务语义。只在误传风险高、已有具体消费者的接口引入 typed boundary 或 newtype，不要求每个字符串都包装。

## 5. 可以类推到其他领域的设计

### 5.1 观察外壳统一，业务状态机分离

Memory record、Plugin binding、Job observation、Artifact、Workflow template 都可以有共同的“从哪里来、是否新鲜、结果是否完整、下一步怎么读取”的描述。但它们不是相同资源生命周期。

Job 的 running/recovering、Memory 的 CAS changed、Plugin 的 stale binding、Artifact 的 expiry 不宜合并成一个巨大的 ResourceStatus 枚举。可以共享小型原语，例如边界明确的 completeness、bounded page、来源引用，而状态机仍归具体领域。

### 5.2 把“是否执行过”与“是否成功”分开

Plugin 已有 `NotStarted / OutcomeUnknown / Completed`，见 [plugin.rs](../../crates/webcodex-core/src/plugin.rs)，83–89。结构化执行也区分排队、运行和 unknown，见 [structured_execution.rs](../../src/tool_runtime/structured_execution.rs)，162–182。

建议建立跨工具一致的恢复语义：未执行可调整输入；明确业务失败按失败处理；结果未知先观察原执行。共享的是这套知识模型，不是把所有操作转成 Job，也不是引入通用自动 retry。执行能力与可安全重试性不能从 readOnly/idempotent hint 推导。

### 5.3 明确四种预算

执行总超时、同步等待窗口、单次观察等待、模型输出预算是不同东西。`StructuredExecutionBudget` 已把前两者分开，见 [structured_execution.rs](../../src/tool_runtime/structured_execution.rs)，17–52。

后续工具组合应保留子执行的总预算与身份，外层只增加组合预算。截止时间一经创建，不应因分页/恢复反复重置。不要为了让卡片持续刷新而延长任务寿命，也不要让模型输出截断改变实际执行结果。

### 5.4 将模型上下文视为按需查询的投影

当前 context sidecar 明确 `post_tool` 与 `applies_to_current_effect=false`，见 [context_projection.rs](../../src/tool_runtime/context_projection.rs)，120–126。这是好的读模型边界。

可逐步增加按需刷新与来源版本说明，避免每次重复大段目录；但要保留请求级 ACK 对模型上下文保留状态的意义，不能用 session id、窗口身份或服务器缓存命中替代。summary、read body、execute schema 应分别预算。

### 5.5 UI 与观测层只消费事实

[MCP presentation](../../src/mcp/presentation.rs)，9–33，将 App descriptor eligibility 与兼容投影分开；162–176 从 canonical lifecycle 派生展示状态。这个方向应保留。

更好的卡片应回答“现在是否还有工作、是否可安全重试、下一步观察哪个身份”，而不是另建 UI 状态机。展示失败不应取消执行；显示完成不应替代测试/审查证据。若新增 Goal，应在现有执行之上做关联与推进；没有 Goal 时原 read/edit/run/test/observe 路径照常工作，不能强制普通工具先走 Goal admission。

### 5.6 组合层是调用优化，不是第二执行内核

已有 [tool-composition-research.md](tool-composition-research.md) 讨论了在 canonical tools 之上组合。本轮不重新引入另一套 DAG/Workflow engine。

资源目录能帮助选择工具，组合层能减少往返，执行内核继续拥有授权/效果/Job。这三层必须分开；外层批次不自动成为可回滚事务。先支持独立只读观察的已知组合，再考虑有证明的资源级并发。

## 6. 不建议本轮采用的方案

- 一个 `ResourceManager` 同时管理 Memory 数据、Skill 文件、Plugin 进程、Job 和 AgentTask：表面统一，实则塞入大量不适用字段与例外。
- scope 直接定义成 User > Project > Runner 并隐式覆盖：混淆来源、所有权和授权，隐藏同名冲突。
- 全部对象共用一张 JSON/EAV 表：放弃已有事务/索引/约束，并没有消除领域差异。
- 所有失败统一 retry，或把 outcome_unknown 转成普通业务失败：可能重复外部副作用。
- 因文件行数多就拆 crate/微服务：文件长度包含测试和契约，不能独立证明职责错误。
- 提前搭建动态注册框架：当前闭合集合适合 enum/静态声明；扩展点越靠近权限与执行，越不应无约束开放。

## 7. 渐进落地顺序与验收

| 阶段 | 交付 | 保持不变的边界 | 验收方式 |
|---|---|---|---|
| 0，本轮 | 启动目录投影的小型复用、基线表征测试、本文 | JSON 形状、顺序、hint、预算、来源发现和权限均不改 | 新旧 JSON oracle 对照，空/不可用/上游截断、Unicode/转义、超大条目；既有 startup 测试 |
| 1，后续已完成 | 扩展家族 admission 声明归 ToolDefinition；Runner Skill provider enqueue typed error 小切片 | 外部错误、scope、surface、direct/gateway 语义不改 | family/registry invariant、ModelHidden invariant、surface/principal focused tests、typed-to-legacy error-kind 对照 |
| 2 | Skill observer/resolver 分离；增加来源级诊断与测量 | opaque identity 和请求时授权不改 | 读取触发扫描次数、cold/warm latency、删除/重配/断线/换实例测试 |
| 3 | 实测需要的缓存、有界并发、DB worker 隔离 | 未知结果/事务/重放语义不改 | 与基线比较 p50/p95、锁等待、内存/字节上限、故障注入 |
| 4，独立功能设计 | 用户私有命名空间、显式仓库知识复用、统一资源浏览界面 | 不隐式继承权限，不改变执行 cwd | principal 隔离、分享撤销、worktree 来源、冲突展示与迁移方案 |

区分纯重构和新增功能很重要：共同描述结构可以先不改变任何用户功能；跨 worktree 共享或 user namespace 一定要另行定义产品行为。阶段 4 不应成为阶段 1/2 的前提。

后续复查也进一步收紧了阶段 2 的前置条件：managed Skill 已能按 opaque `skill_id` 精确读取，但 configured Skill 的 Runner read 当前仍会在来源内部执行完整 `discover(config)`；Project Skill 的 opaque id 又不能反推出 package name，只能从有界包名集合计算匹配；完整 catalog 还承担跨来源 duplicate-id fail-closed 检查。因此不要简单把 `skill_read_file` 的 `discover_skills` 删除后按来源盲试。更安全的顺序仍是先增加来源级测量/诊断并定义 exact resolution 的失败与重复身份语义，再做 observer/resolver 分离。

建议持续关注的指标不是抽象数量，而是：新增一个工具需要改多少个独立分类点；精确 Skill 读取触发多少次 Runner 请求；为了做一个简单选择要给模型多少 schema 字节；失败是否直接给出可执行的下一步；核心改动需要编译和运行哪些无关测试。

## 8. 本轮工具使用反馈

### 实际观察

- `work_on_project(mode=worktree)` 完成精确基线解析、隔离、项目注册和指导材料加载，降低了手工 git worktree 与授权注册串联的成本。
- `read_files` 的编号、SHA 与显式 continuation 支撑可追溯分析和受保护修改；`search_project_texts` 一个子查询失败不影响其他查询。
- 本轮 `show_changes(max_hunk_lines=200)` 顶层正确报告 `diff_hunk_line_limit` 并给出恢复调用，但对应 hunk 同时带有 `truncated=false`，文本又在函数中途结束。按提示使用 `git_diff_hunks`、更窄路径与 400 行上限后取得完整 diff。建议对顶层/条目级完整性元数据增加一致性测试；本轮只记录，不修改该工具。
- 对不存在的 `src/tool_runtime/context_material.rs` 发起搜索，得到 `search_execution_failed / backend_process_failed / exit_code=2`。重新定位后实际文件是 `context_projection.rs`。相比之下，read_files 对不存在的文件明确返回 `not_found`。建议搜索入口也区分缺失路径与真正后端故障。
- 一个拟进行只读源码计数的 Python 命令被宿主以“无法确定请求的安全状态”拦截；没有 Runner 执行结果。没有改换通道重试。此事不能归因于 WebCodex 权限策略，报告也不使用该计数结果。产品诊断应区分 host pre-dispatch rejection 与 Runner business error；Runner 未收到请求时不能声称观察到执行状态。
- `cargo_test` 能把同一执行交给 Job，再与独立阅读重叠，保留了执行身份。其当前结构化 schema 有 package/filter 等参数，却没有 `--lib` 选择项。可评估增加常见 target selector，减少为了精准验证退回 raw Cargo 的需要；不要求默认扩大运行范围。

### 可用性建议

以“模型每次需要额外判断什么”为优化对象，而不是只数工具数量。优先改善空目录的归属/完整性说明、错误恢复提示、相同操作在 direct/gateway 的发现一致性。不要为少一次工具调用牺牲每个子操作独立结果，也不要用更长、更严厉的工具说明弥补本可由类型和默认值解决的问题。

标准启动结果有不泄露绝对根路径的测试约束，因此即使路径展示能节省一次 `pwd`，也不能未经评审直接加入。诊断模式可以按既有可见性政策提供精确位置；普通模型结果继续保持 path-safe。

## 9. 本轮代码验证切片

本轮选择的实现是启动专用 `StartupCatalog<Entry>`：Skill/Plugin 的公共投影外壳与贪心字节预算算法共用，保留具名别名、各自构造入口、hint 和上游完整性来源。

这不会新增资源管理工具、provider 加载器、Memory 层级或 Plugin cwd 重定向，不会改变 Token/Session/Job 授权，也不会对 live services 做任何 reload。它是“共享描述，不共享行为”的小型实例，不代表本文其余提议已经完成。

新增独立测试模块 `src/tool_runtime/tests/startup_catalog.rs`，使用独立 JSON oracle 描述原有 greedy-prefix 协议。测试覆盖多种数量/长度、Unicode 与 JSON 转义、已观察条目少于 provider total、空目录、不可用目录、超大条目停止、总预算及来源截断。验证先在原实现运行，再在重构后运行；具体实际结果见下节。

## 10. 验证记录

- workspace boundary：本轮已通过，17 packages；没有修改依赖策略。
- 新增表征测试的基线运行：`cargo test -p webcodex startup_catalog`，4 passed / 0 failed；两组参数矩阵合计 100 个 JSON 对照场景，另有不可用与合并预算测试。
- 最终源码：`cargo fmt -p webcodex -- --check` 通过；`cargo test -p webcodex startup` 完成编译并执行 41 tests，41 passed / 0 failed，包含新增的 4 tests。
- 扩展目录集成：`cargo test -p webcodex work_on_project_extension_catalog`，2 passed / 0 failed；覆盖 configured roots 进入目录及关闭目录时跳过发现。
- 文档链接：`python3 scripts/check_markdown_links.py`，75 个 Markdown 文件、512 个本地链接、missing=0。
- 变更检查：`git diff --cached --check` 通过；已审查完整 Rust diff 和新增测试。性能收益没有计时验证；编译成功不等于全仓测试通过。
- 未运行：全 workspace 测试、跨平台测试、外网/生产重启、性能基准。
