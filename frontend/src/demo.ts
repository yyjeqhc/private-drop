// An isolated, fictional feature tour. This page has no API client, credentials,
// browser storage, or connection to the production Runtime Console state.
type DemoReview = "pending" | "approved" | "changes_requested";
type DemoView = "workspace" | "runners" | "jobs" | "review";
type DemoTab = "conversation" | "logs" | "diff";
type DemoFile = { path: string; diff: string };
type DemoTask = {
  id: string;
  project: string;
  title: string;
  branch: string;
  duration: string;
  prompt: string;
  response: string;
  result: string;
  command: string;
  tests: number;
  review: DemoReview;
  steps: string[];
  files: DemoFile[];
};

const demoProjects = [
  {
    id: "webcodex",
    name: "WebCodex",
    mark: "W",
    stack: "Rust · TypeScript",
    path: "/workspace/webcodex",
    runner: "dev-linux",
    description: "AI 开发环境连接与控制台",
  },
  {
    id: "atlas",
    name: "Atlas API",
    mark: "A",
    stack: "Python · FastAPI",
    path: "/workspace/atlas-api",
    runner: "build-linux",
    description: "业务 API 与接口测试",
  },
  {
    id: "studio",
    name: "Studio UI",
    mark: "S",
    stack: "TypeScript · React",
    path: "/workspace/studio-ui",
    runner: "design-mac",
    description: "设计系统与组件库",
  },
];
const demoRunners = [
  {
    id: "dev-linux",
    name: "开发工作站",
    platform: "Ubuntu 24.04 · x86_64",
    online: true,
    memory: "6.2 / 32 GB",
    cpu: 18,
    slots: "0 / 4",
    seen: "刚刚 · 示例快照",
  },
  {
    id: "build-linux",
    name: "构建服务器",
    platform: "Debian 12 · x86_64",
    online: true,
    memory: "3.1 / 16 GB",
    cpu: 8,
    slots: "0 / 2",
    seen: "刚刚 · 示例快照",
  },
  {
    id: "design-mac",
    name: "MacBook Pro",
    platform: "macOS · arm64",
    online: false,
    memory: "—",
    cpu: 0,
    slots: "—",
    seen: "2 小时前 · 示例快照",
  },
];
const demoTasks: DemoTask[] = [
  {
    id: "heartbeat",
    project: "webcodex",
    title: "为 Runner 心跳补齐边界测试",
    branch: "feature/heartbeat-tests",
    duration: "3m 42s",
    prompt:
      "检查 Runner 心跳的超时处理，补充断线与重连的边界测试。完成后运行相关测试，把代码差异留给我审查。",
    response:
      "已定位心跳状态转换与超时判断。接下来复用现有测试结构，验证超时、重连和重复心跳三个场景。",
    result:
      "相关测试全部通过，2 个文件的变更已保留。请查看代码差异，再决定是否接受。",
    command: "cargo test -p runner heartbeat",
    tests: 24,
    review: "pending",
    steps: [
      "读取项目结构，确认当前分支",
      "补齐超时、重连与重复心跳测试",
      "运行相关测试，保留输出与代码差异",
    ],
    files: [
      {
        path: "src/runner/heartbeat.rs",
        diff: "@@ 心跳状态判断（示例） @@\n impl Heartbeat {\n-    fn expired(&self, now: Instant) -> bool {\n-        now > self.deadline\n+    fn expired(&self, now: Instant) -> bool {\n+        now >= self.deadline\n     }\n }",
      },
      {
        path: "tests/heartbeat.rs",
        diff: "@@ 边界回归测试（示例） @@\n+#[test]\n+fn heartbeat_expires_at_deadline() {\n+    let clock = TestClock::new();\n+    let heartbeat = Heartbeat::new(clock.now());\n+    clock.advance(HEARTBEAT_TIMEOUT);\n+    assert!(heartbeat.expired(clock.now()));\n+}",
      },
    ],
  },
  {
    id: "runner-docs",
    project: "webcodex",
    title: "补充 Runner 本地接入说明",
    branch: "docs/runner-setup",
    duration: "1m 18s",
    prompt:
      "整理本地 Runner 的接入步骤，补充连接检查命令，并检查文档里的链接。",
    response:
      "已整理 Server 与 Runner 的连接顺序，并给每一步补充可验证的检查点。",
    result: "说明与链接检查已完成。示例审查已接受，文档变更保留在工作区。",
    command: "python scripts/check_doc_links.py",
    tests: 12,
    review: "approved",
    steps: ["读取已有接入文档", "补充连接检查与排查步骤", "检查文档链接"],
    files: [
      {
        path: "docs/runner-setup.md",
        diff: "@@ 连接检查（示例） @@\n ## 验证连接\n+1. 确认 Runner 使用预期的 Server 地址。\n+2. 在控制台查看设备连接状态。\n+3. 选择已注册项目并执行一次只读检查。",
      },
    ],
  },
  {
    id: "pagination",
    project: "atlas",
    title: "修复分页接口的空结果边界",
    branch: "fix/pagination-empty",
    duration: "2m 16s",
    prompt:
      "当查询没有结果时，分页接口应该返回空数组和正确的总数。请修复并补充回归测试。",
    response:
      "已复现空结果时返回 null 的问题。将保持响应结构一致，补充空集合与越界页码的测试。",
    result: "18 项相关测试通过。响应结构保持兼容，等待你审查这次修复。",
    command: "pytest tests/api/test_pagination.py -q",
    tests: 18,
    review: "pending",
    steps: [
      "复现空结果响应",
      "修正返回值并新增边界测试",
      "运行分页接口回归测试",
    ],
    files: [
      {
        path: "app/api/pagination.py",
        diff: '@@ 空结果分页（示例） @@\n def paginate(items, total):\n-    return {"items": items or None, "total": total}\n+    return {"items": items or [], "total": total}',
      },
      {
        path: "tests/api/test_pagination.py",
        diff: '@@ 回归测试（示例） @@\n+def test_empty_page_keeps_response_shape():\n+    page = paginate([], total=0)\n+    assert page == {"items": [], "total": 0}',
      },
    ],
  },
  {
    id: "api-health",
    project: "atlas",
    title: "验证健康检查接口",
    branch: "test/health-check",
    duration: "48s",
    prompt: "检查健康检查接口的返回内容，确保外部状态页能稳定读取服务状态。",
    response: "已确认状态字段约定，并补充响应类型与状态码的测试。",
    result: "9 项接口测试通过，示例审查已接受。",
    command: "pytest tests/api/test_health.py -q",
    tests: 9,
    review: "approved",
    steps: ["读取健康检查路由", "检查返回字段及状态码", "验证接口契约"],
    files: [
      {
        path: "tests/api/test_health.py",
        diff: '@@ 健康检查测试（示例） @@\n+def test_health(client):\n+    response = client.get("/health")\n+    assert response.status_code == 200\n+    assert response.json()["status"] == "ok"',
      },
    ],
  },
  {
    id: "theme",
    project: "studio",
    title: "完善深浅主题下的按钮状态",
    branch: "fix/button-theme",
    duration: "1m 54s",
    prompt: "检查按钮组件在深色主题下的焦点与禁用状态，补充可访问性验证。",
    response:
      "已找到硬编码的焦点色。改为使用现有主题变量，并检查键盘导航与禁用态对比度。",
    result: "16 项组件检查通过。结果已保留，设备离线后仍可回看本次记录。",
    command: "npm test -- Button.test.tsx",
    tests: 16,
    review: "approved",
    steps: ["检查按钮主题变量", "统一焦点与禁用状态", "验证键盘操作与组件行为"],
    files: [
      {
        path: "src/components/Button.css",
        diff: "@@ 主题焦点状态（示例） @@\n .button:focus-visible {\n-  outline: 2px solid #000;\n+  outline: 2px solid var(--focus-ring);\n+  outline-offset: 3px;\n }",
      },
    ],
  },
];

const demoReviewLabels: Record<DemoReview, string> = {
  pending: "等待审查",
  approved: "已接受",
  changes_requested: "需要修改",
};
let demoReviews = new Map(demoTasks.map((task) => [task.id, task.review]));
let demoProjectId = "webcodex";
let demoTaskId = "heartbeat";
let demoActiveTab: DemoTab = "conversation";
let demoReplayTimer: number | undefined;
let demoReplayStep = 3;

function demoEl<T extends HTMLElement = HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`Missing demo element: ${id}`);
  return node as T;
}
function demoNode<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  text = "",
  className = "",
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  node.textContent = text;
  if (className) node.className = className;
  return node;
}
function demoIcon(name: string): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#icon-${name}`);
  svg.setAttribute("aria-hidden", "true");
  svg.append(use);
  return svg;
}
function demoText(id: string, value: string | number): void {
  demoEl(id).textContent = String(value);
}
function demoCurrentTask(): DemoTask {
  return demoTasks.find((task) => task.id === demoTaskId)!;
}
function demoCurrentProject() {
  return demoProjects.find((project) => project.id === demoProjectId)!;
}
function demoReview(task: DemoTask): DemoReview {
  return demoReviews.get(task.id)!;
}
function demoPill(review: DemoReview): HTMLSpanElement {
  return demoNode("span", demoReviewLabels[review], `pill ${review}`);
}
function demoDiffCount(files: DemoFile[]): { added: number; removed: number } {
  const lines = files.flatMap((file) => file.diff.split("\n"));
  return {
    added: lines.filter((line) => line.startsWith("+")).length,
    removed: lines.filter((line) => line.startsWith("-")).length,
  };
}
function demoCloseNavigation(): void {
  document.querySelector(".demo-shell")?.classList.remove("nav-open");
  demoEl("demo-menu").setAttribute("aria-expanded", "false");
}
function demoStopReplay(): void {
  if (demoReplayTimer !== undefined) window.clearTimeout(demoReplayTimer);
  demoReplayTimer = undefined;
}
function demoSelectProject(id: string): void {
  demoProjectId = id;
  demoSelectTask(demoTasks.find((task) => task.project === id)!.id);
}
function demoSelectTask(id: string): void {
  demoStopReplay();
  const task = demoTasks.find((item) => item.id === id)!;
  demoTaskId = task.id;
  demoProjectId = task.project;
  demoReplayStep = task.steps.length;
  demoRenderProjects();
  demoRenderTask();
  demoCloseNavigation();
}
function demoRenderProjects(): void {
  const list = demoEl("demo-projects");
  const query = demoEl<HTMLInputElement>("demo-search")
    .value.trim()
    .toLowerCase();
  list.replaceChildren();
  for (const project of demoProjects.filter((item) =>
    `${item.name} ${item.stack} ${item.description}`
      .toLowerCase()
      .includes(query),
  )) {
    const button = demoNode("button", "", "project-button");
    button.type = "button";
    button.setAttribute("aria-pressed", String(project.id === demoProjectId));
    const copy = demoNode("span", "", "project-copy");
    copy.append(
      demoNode("strong", project.name),
      demoNode("small", project.stack),
    );
    const runner = demoRunners.find((item) => item.id === project.runner)!;
    const dot = demoNode(
      "span",
      "",
      `status-dot${runner.online ? "" : " offline"}`,
    );
    dot.setAttribute(
      "aria-label",
      runner.online ? "设备在线" : "设备离线，记录可回看",
    );
    button.append(
      demoNode("span", project.mark, "project-monogram"),
      copy,
      dot,
    );
    button.addEventListener("click", () => {
      demoSetView("workspace");
      demoSelectProject(project.id);
    });
    list.append(button);
  }
  demoEl("demo-search-empty").hidden = list.childElementCount !== 0;
}
function demoSetTab(tab: DemoTab, focus = false): void {
  demoActiveTab = tab;
  document
    .querySelectorAll<HTMLButtonElement>("[data-tab]")
    .forEach((button) => {
      const selected = button.dataset.tab === tab;
      button.setAttribute("aria-selected", String(selected));
      button.tabIndex = selected ? 0 : -1;
      if (selected && focus) button.focus();
    });
  for (const name of ["conversation", "logs", "diff"])
    demoEl(`panel-${name}`).hidden = name !== tab;
}
function demoRenderSteps(): void {
  const task = demoCurrentTask();
  demoEl("demo-steps").replaceChildren(
    ...task.steps.map((step, index) => {
      const row = demoNode("li");
      const finished = index < demoReplayStep;
      row.append(
        demoNode(
          "span",
          finished ? "✓" : index === demoReplayStep ? "·" : "○",
          `step-indicator ${finished ? "" : index === demoReplayStep ? "active" : "waiting"}`,
        ),
        demoNode("span", step),
      );
      return row;
    }),
  );
  const finished = demoReplayStep >= task.steps.length;
  demoText(
    "demo-test-summary",
    finished
      ? `${task.tests} passed · 0 failed · exit code 0`
      : "示例过程回放中，尚未展示测试结果…",
  );
  demoText(
    "demo-result",
    finished ? task.result : "正在回放预设步骤；完成后展示保留结果。",
  );
  demoText(
    "demo-replay",
    demoReplayTimer === undefined ? "▷ 重播执行过程" : "Ⅱ 停止回放",
  );
  const logLines = [
    "[14:32:00] job started · example data",
    `[14:32:01] workspace: ${demoCurrentProject().path}`,
  ];
  task.steps
    .slice(0, demoReplayStep)
    .forEach((step, index) => logLines.push(`[14:3${index + 2}:12] ✓ ${step}`));
  if (finished)
    logLines.push(
      "",
      `$ ${task.command}`,
      "",
      `test result: ok. ${task.tests} passed; 0 failed`,
      `finished in ${task.duration} · exit code: 0`,
      "",
      "[14:36:00] output and workspace diff retained",
      "[demo] No command was executed by this page.",
    );
  demoText("demo-logs", logLines.join("\n"));
}
function demoRenderTask(): void {
  const project = demoCurrentProject();
  const task = demoCurrentTask();
  demoText("demo-project-mark", project.mark);
  demoText("demo-project-name", project.name);
  const select = demoEl<HTMLSelectElement>("demo-task");
  select.replaceChildren(
    ...demoTasks
      .filter((item) => item.project === project.id)
      .map((item) => {
        const option = demoNode("option", item.title);
        option.value = item.id;
        return option;
      }),
  );
  select.value = task.id;
  demoText("demo-branch", task.branch);
  demoText("demo-session-time", `09.08 14:32 · 耗时 ${task.duration}`);
  demoText("demo-prompt", task.prompt);
  demoText("demo-response", task.response);
  demoText("demo-command", task.command);
  demoText(
    "demo-runner",
    demoRunners.find((runner) => runner.id === project.runner)!.name,
  );
  demoText("demo-stack", project.stack);
  demoText("demo-path", project.path);
  demoText("demo-test-count", task.tests);
  demoText("demo-validation-note", `0 项失败 · 示例任务耗时 ${task.duration}`);
  demoText("demo-file-count", task.files.length);
  demoText("demo-changed-count", task.files.length);
  const totals = demoDiffCount(task.files);
  demoText("demo-diff-stat", `+${totals.added} −${totals.removed}`);
  demoEl("demo-files").replaceChildren(
    ...task.files.map((file) => {
      const item = demoNode("li");
      const count = demoDiffCount([file]);
      item.append(
        demoNode("span", file.path),
        demoNode("span", `+${count.added} −${count.removed}`, "file-stat"),
      );
      return item;
    }),
  );
  demoEl("demo-diff").replaceChildren(
    ...task.files.map((file) => {
      const article = demoNode("article", "", "diff-file");
      const pre = demoNode("pre");
      for (const line of file.diff.split("\n"))
        pre.append(
          demoNode(
            "span",
            line,
            `diff-line ${line.startsWith("+") ? "add" : line.startsWith("-") ? "remove" : line.startsWith("@@") ? "meta" : ""}`,
          ),
        );
      article.append(demoNode("h3", file.path), pre);
      return article;
    }),
  );
  demoText("demo-replay-status", "示例会话 · 过程与结果均可回看");
  demoRenderSteps();
  demoRenderReview();
  demoSetTab(demoActiveTab);
}
function demoRenderReview(): void {
  const task = demoCurrentTask();
  const status = demoReview(task);
  demoText("demo-session-status", demoReviewLabels[status]);
  demoEl("demo-session-status").className = `pill ${status}`;
  const pending = demoTasks.filter((item) => demoReview(item) === "pending");
  demoText("demo-review-count", pending.length);
  demoText("demo-pending-metric", pending.length);
  demoText(
    "demo-review-title",
    status === "pending"
      ? "最后一步，由你决定。"
      : status === "approved"
        ? "示例变更已接受。"
        : "已要求继续修改。",
  );
  demoText(
    "demo-review-description",
    status === "pending"
      ? "测试结果和代码差异已就绪。查看后接受变更，或要求继续修改。"
      : status === "approved"
        ? "审查决定已记录在本次演示中。实际仓库和 Git 分支没有变化。"
        : "示例任务已标记为需要修改。重置演示可恢复初始状态。",
  );
  demoEl<HTMLButtonElement>("demo-approve").disabled = status !== "pending";
  demoEl<HTMLButtonElement>("demo-request-changes").disabled =
    status !== "pending";
  const queue = demoEl("demo-review-queue");
  queue.replaceChildren();
  for (const item of pending) {
    const button = demoNode("button", item.title);
    button.type = "button";
    button.setAttribute("aria-pressed", String(item.id === task.id));
    button.append(
      demoNode(
        "small",
        `${demoProjects.find((project) => project.id === item.project)!.name} · 等待审查`,
      ),
    );
    button.addEventListener("click", () => {
      demoSelectTask(item.id);
      demoSetTab("diff");
    });
    queue.append(button);
  }
  if (!pending.length)
    queue.append(
      demoNode("p", "所有示例变更均已审查。可重置演示，再体验一次。", "empty"),
    );
}
function demoSetView(view: DemoView): void {
  demoStopReplay();
  demoReplayStep = demoCurrentTask().steps.length;
  demoRenderSteps();
  demoText("demo-replay-status", "示例会话 · 过程与结果均可回看");
  const headings: Record<DemoView, [string, string, string]> = {
    workspace: [
      "工作空间",
      "让 AI 的每一步，都清晰可见。",
      "连接自己的开发环境，观察任务、检查结果，让代码始终掌握在你手中。",
    ],
    runners: [
      "Runner 设备",
      "一个工作空间，连接你的机器。",
      "每个 Runner 在自己的机器上工作，项目、命令与工具链都留在原处。",
    ],
    jobs: [
      "执行记录",
      "任务结束了，证据仍然在。",
      "回看命令、验证结果与代码差异，了解每一次工作的来龙去脉。",
    ],
    review: [
      "代码审查",
      "AI 负责执行，你来确认结果。",
      "并排查看代码变化与测试证据，模拟接受变更或提出修改要求。",
    ],
  };
  demoText("demo-breadcrumb", headings[view][0]);
  demoText("demo-title", headings[view][1]);
  demoText("demo-description", headings[view][2]);
  document
    .querySelectorAll<HTMLButtonElement>("[data-view]")
    .forEach((button) => {
      if (button.dataset.view === view)
        button.setAttribute("aria-current", "page");
      else button.removeAttribute("aria-current");
    });
  demoEl("demo-workbench").hidden = view === "jobs" || view === "runners";
  demoEl("demo-runners-view").hidden = view !== "runners";
  demoEl("demo-jobs-view").hidden = view !== "jobs";
  demoEl("demo-review-queue").hidden = view !== "review";
  if (view === "review") {
    const pending = demoTasks.find((task) => demoReview(task) === "pending");
    if (pending && demoReview(demoCurrentTask()) !== "pending")
      demoSelectTask(pending.id);
    demoSetTab("diff");
  }
  if (view === "jobs") demoRenderJobs();
  demoCloseNavigation();
}
function demoRenderRunners(): void {
  demoEl("demo-runners").replaceChildren(
    ...demoRunners.map((runner) => {
      const project = demoProjects.find((item) => item.runner === runner.id)!;
      const card = demoNode("article", "", "runner-card");
      card.append(
        demoIcon("device"),
        demoNode("h3", runner.name),
        demoNode("p", runner.platform, "runner-platform"),
        demoNode(
          "span",
          runner.online ? "● 在线" : "○ 离线",
          `pill ${runner.online ? "approved" : ""}`,
        ),
      );
      const details = demoNode("div", "", "runner-detail");
      const dl = demoNode("dl");
      for (const [label, value] of [
        ["项目", project.name],
        ["执行槽位", runner.slots],
        ["内存占用", runner.memory],
        ["CPU 占用", runner.online ? `${runner.cpu}%` : "—"],
      ]) {
        const row = demoNode("div");
        row.append(demoNode("dt", label), demoNode("dd", value));
        dl.append(row);
      }
      const meter = demoNode("div", "", "runner-meter");
      const fill = demoNode("span");
      fill.style.width = `${runner.cpu}%`;
      meter.append(fill);
      details.append(dl, meter, demoNode("p", `最近连接：${runner.seen}`));
      const button = demoNode(
        "button",
        runner.online ? "查看项目与任务 →" : "回看保留记录 →",
        "button",
      );
      button.type = "button";
      button.addEventListener("click", () => {
        demoSetView("workspace");
        demoSelectProject(project.id);
      });
      card.append(details, button);
      return card;
    }),
  );
}
function demoRenderJobs(): void {
  const filter = demoEl<HTMLSelectElement>("demo-job-filter").value;
  const tasks = demoTasks.filter(
    (task) => filter === "all" || demoReview(task) === filter,
  );
  demoEl("demo-jobs").replaceChildren(
    ...tasks.map((task) => {
      const project = demoProjects.find((item) => item.id === task.project)!;
      const runner = demoRunners.find((item) => item.id === project.runner)!;
      const tr = demoNode("tr");
      const title = demoNode("td");
      title.append(
        demoNode("strong", task.title),
        demoNode("small", task.branch, "mono"),
      );
      const where = demoNode("td", project.name);
      where.append(demoNode("small", runner.name));
      const status = demoNode("td");
      status.append(demoPill(demoReview(task)));
      const action = demoNode("td");
      const button = demoNode("button", "查看 →", "text-button");
      button.type = "button";
      button.setAttribute("aria-label", `查看：${task.title}`);
      button.addEventListener("click", () => {
        demoSetView("workspace");
        demoSelectTask(task.id);
        demoSetTab("logs");
      });
      action.append(button);
      tr.append(
        title,
        where,
        status,
        demoNode("td", `${task.tests} passed`, "success-text"),
        demoNode("td", task.duration, "mono"),
        action,
      );
      return tr;
    }),
  );
  demoEl("demo-jobs-empty").hidden = tasks.length !== 0;
}
function demoStartReplay(): void {
  if (demoReplayTimer !== undefined) {
    demoStopReplay();
    demoRenderSteps();
    demoText("demo-replay-status", "回放已停止 · 点击可从头重播");
    return;
  }
  demoReplayStep = 0;
  demoSetTab("conversation");
  const advance = () => {
    demoReplayStep += 1;
    if (demoReplayStep < demoCurrentTask().steps.length)
      demoReplayTimer = window.setTimeout(advance, 1000);
    else demoReplayTimer = undefined;
    demoRenderSteps();
    demoText(
      "demo-replay-status",
      demoReplayTimer === undefined
        ? "示例回放完成 · 未执行实际命令"
        : `示例回放 ${demoReplayStep + 1} / 3`,
    );
  };
  demoReplayTimer = window.setTimeout(advance, 1000);
  demoRenderSteps();
  demoText("demo-replay-status", "示例回放 1 / 3");
}
function demoReset(): void {
  demoStopReplay();
  demoReviews = new Map(demoTasks.map((task) => [task.id, task.review]));
  demoEl<HTMLInputElement>("demo-search").value = "";
  demoEl<HTMLSelectElement>("demo-job-filter").value = "all";
  demoSelectTask("heartbeat");
  demoSetView("workspace");
  demoSetTab("conversation");
  demoText("demo-replay-status", "演示已重置");
}

document
  .querySelectorAll<HTMLButtonElement>("[data-view]")
  .forEach((button) =>
    button.addEventListener("click", () =>
      demoSetView(button.dataset.view as DemoView),
    ),
  );
document.querySelectorAll<HTMLButtonElement>("[data-tab]").forEach((button) => {
  button.addEventListener("click", () =>
    demoSetTab(button.dataset.tab as DemoTab),
  );
  button.addEventListener("keydown", (event) => {
    const tabs: DemoTab[] = ["conversation", "logs", "diff"];
    const index = tabs.indexOf(demoActiveTab);
    const next =
      event.key === "ArrowRight"
        ? (index + 1) % 3
        : event.key === "ArrowLeft"
          ? (index + 2) % 3
          : event.key === "Home"
            ? 0
            : event.key === "End"
              ? 2
              : null;
    if (next !== null) {
      event.preventDefault();
      demoSetTab(tabs[next], true);
    }
  });
});
demoEl("demo-search").addEventListener("input", demoRenderProjects);
demoEl<HTMLSelectElement>("demo-task").addEventListener("change", (event) =>
  demoSelectTask((event.target as HTMLSelectElement).value),
);
demoEl("demo-open-diff").addEventListener("click", () =>
  demoSetTab("diff", true),
);
demoEl("demo-job-filter").addEventListener("change", demoRenderJobs);
demoEl("demo-replay").addEventListener("click", demoStartReplay);
demoEl("demo-reset").addEventListener("click", demoReset);
for (const [id, decision] of [
  ["demo-approve", "approved"],
  ["demo-request-changes", "changes_requested"],
] as const) {
  demoEl(id).addEventListener("click", () => {
    if (demoReview(demoCurrentTask()) !== "pending") return;
    demoReviews.set(demoTaskId, decision);
    demoRenderReview();
    demoText(
      "demo-replay-status",
      `模拟审查：${demoReviewLabels[decision]} · 未修改实际仓库`,
    );
    demoEl("demo-open-diff").focus();
  });
}
demoEl("demo-theme").addEventListener("click", () => {
  const light = document.documentElement.dataset.theme !== "light";
  document.documentElement.dataset.theme = light ? "light" : "dark";
  const label = light ? "切换到深色主题" : "切换到浅色主题";
  demoEl("demo-theme").setAttribute("aria-label", label);
  demoEl("demo-theme").title = label;
});
demoEl("demo-menu").addEventListener("click", () => {
  const open = document
    .querySelector(".demo-shell")!
    .classList.toggle("nav-open");
  demoEl("demo-menu").setAttribute("aria-expanded", String(open));
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") demoCloseNavigation();
});
window.addEventListener("pagehide", demoStopReplay);
demoRenderProjects();
demoRenderTask();
demoRenderRunners();

export {};
