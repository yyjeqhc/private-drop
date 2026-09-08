import test from "node:test";
import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const frontendRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(frontendRoot, "..");
const buildScript = resolve(frontendRoot, "scripts/build.mjs");
const requiredAssets = [
  "console.html",
  "app.js",
  "workflow_session_state.js",
  "runtime_console_state.js",
  "runtime.html",
  "runtime.js",
  "runtime.css",
  "styles.css",
  "admin.html",
  "admin.js",
  "admin.css",
  "admin_controller.js",
  "admin_mutation_controller.js",
  "admin_mutation_view.js",
  "admin_view.js",
];

function exec(command, args, options = {}) {
  return new Promise((resolvePromise, reject) => {
    execFile(command, args, options, (error, stdout, stderr) => {
      if (error) {
        reject(
          new Error(
            `${command} ${args.join(" ")} failed: ${stderr || error.message}`
          )
        );
      } else {
        resolvePromise({ stdout, stderr });
      }
    });
  });
}

async function assertRequiredAssets(outputDirectory) {
  for (const asset of requiredAssets) {
    assert.equal((await stat(resolve(outputDirectory, asset))).isFile(), true);
  }
  const app = await readFile(resolve(outputDirectory, "app.js"), "utf8");
  assert.equal(app.includes("interface Review"), false);
  assert.equal(/\.innerHTML\b|\binnerHTML\s*=/.test(app), false);
  assert.match(app, /workflow-session/);
  assert.match(app, /textContent/);
  assert.match(app, /workflow-session-overview-validation/);
  assert.match(app, /workflow-session-overview-progress/);
  const consoleHtml = await readFile(resolve(outputDirectory, "console.html"), "utf8");
  assert.match(consoleHtml, /workflow-session-overview-work/);
  assert.match(consoleHtml, /Reported progress/);
  assert.match(consoleHtml, /Model-reported; informational only\./);
  assert.match(consoleHtml, /WebCodex — Project Review Console/);
  assert.match(consoleHtml, /Project review console/);
  const runtimeHtml = await readFile(resolve(outputDirectory, "runtime.html"), "utf8");
  assert.match(runtimeHtml, /WebCodex Runtime Console/);
  assert.match(runtimeHtml, /runtime-device-select/);
  assert.match(runtimeHtml, /runtime-project-list/);
  assert.equal(runtimeHtml.includes("runtime-project-" + "select"), false);
  assert.match(runtimeHtml, /runtime-project-search/);
  assert.match(runtimeHtml, /runtime-project-search[^>]*maxlength="200"/);
  assert.match(runtimeHtml, /runtime-collaboration-form/);
  assert.match(runtimeHtml, /id="runtime-chat-scroll" class="chat-scroll"/);
  assert.match(runtimeHtml, /id="runtime-message-body" rows="1" maxlength="4000" enterkeyhint="send"/);
  assert.match(runtimeHtml, /id="runtime-message-options"/);
  assert.match(runtimeHtml, /class="composer-options-popover"/);
  assert.match(runtimeHtml, /runtime-refresh-status/);
  assert.match(runtimeHtml, /runtime-token-form/);
  assert.match(runtimeHtml, /runtime-token-remember/);
  assert.match(runtimeHtml, /content="light dark"/);
  assert.match(runtimeHtml, /viewport-fit=cover/);
  assert.match(runtimeHtml, /data-theme-option="system"/);
  assert.match(runtimeHtml, /data-theme-option="light"/);
  assert.match(runtimeHtml, /data-theme-option="dark"/);
  assert.match(runtimeHtml, /data-language-toggle/);
  assert.match(runtimeHtml, /data-language-toggle-label/);
  assert.match(runtimeHtml, /webcodex\.runtime\.language\.v1/);
  assert.match(runtimeHtml, /document\.documentElement\.lang = language/);
  assert.match(runtimeHtml, /runtime-mobile-nav-toggle/);
  assert.match(runtimeHtml, /runtime-mobile-nav-close/);
  assert.match(runtimeHtml, /runtime-mobile-nav-backdrop/);
  assert.match(runtimeHtml, /runtime-inspector-panel/);
  assert.match(runtimeHtml, /data-runtime-view="sessions"/);
  assert.match(runtimeHtml, /data-runtime-view="operations"/);
  assert.match(runtimeHtml, /runtime-operations-stage/);
  assert.match(runtimeHtml, /runtime-operations-overview/);
  assert.match(runtimeHtml, /runtime-operations-runners/);
  assert.match(runtimeHtml, /runtime-operations-agents/);
  assert.match(runtimeHtml, /runtime-collaboration-board[^>]*role="log"/);
  assert.match(runtimeHtml, /runtime-message-announcer[^>]*aria-live="polite"/);
  assert.match(runtimeHtml, /runtime-new-messages/);
  assert.match(runtimeHtml, /runtime-topbar-more/);
  assert.equal(runtimeHtml.includes("runtime-communication-panel"), false);
  assert.match(runtimeHtml, /runtime-agent-create-form/);
  assert.match(runtimeHtml, /runtime-agent-update-form/);
  assert.match(runtimeHtml, /Continue as this Agent/);
  assert.match(runtimeHtml, /runtime-conversation-transcript/);
  assert.match(runtimeHtml, /runtime-conversation-send-as-agent/);
  assert.match(runtimeHtml, /runtime-inbox-list/);
  assert.match(runtimeHtml, /no production model-resume adapter/);
  assert.match(runtimeHtml, /pending Wake Intents remain durable/);
  assert.match(runtimeHtml, /Jump to latest/);
  assert.match(runtimeHtml, /Reported progress/);
  assert.match(runtimeHtml, /Model-reported; informational only\./);
  const runtime = await readFile(resolve(outputDirectory, "runtime.js"), "utf8");
  assert.match(runtime, /\/api\/runtime-console\//);
  assert.match(runtime, /runtimeDeviceIds/);
  assert.match(runtime, /runtimeProjectsForDevice/);
  assert.match(runtime, /filterAndSortRuntimeProjects/);
  assert.match(runtime, /workflow-session-post-message/);
  assert.match(runtime, /syncCollaborationComposerLayout/);
  assert.match(runtime, /scrollCollaborationToLatest/);
  assert.match(runtime, /firstRetainedRender \|\| \(hasNewMessages && shouldFollowNewMessages\)/);
  assert.match(runtime, /collaborationFollowLatest \|\| chatIsNearLatest\(\)/);
  assert.match(runtime, /collaborationPendingMessages \+= newMessageIds\.length/);
  assert.match(runtime, /appendRichMessage/);
  assert.match(runtime, /DRAFT_STORAGE_PREFIX/);
  assert.match(runtime, /WORKSPACE_VIEW_STORAGE_KEY/);
  assert.match(runtime, /closeTopbarMore/);
  assert.match(runtime, /form\.requestSubmit\(\)/);
  assert.match(runtime, /message-entering/);
  assert.match(runtime, /Refresh failed · showing previous data/);
  assert.match(runtime, /preferredRuntimeProjectSelection/);
  assert.match(runtime, /workflowSessionListOverviewFacts/);
  assert.match(runtime, /isCurrentRuntimeWorkflowSessionRequest/);
  const sessionListStart = runtime.indexOf("function renderSessionList");
  const sessionListEnd = runtime.indexOf("function selectSession", sessionListStart);
  const sessionListRender = runtime.slice(sessionListStart, sessionListEnd);
  assert.doesNotMatch(sessionListRender, /summary-facts|appendPreview|workflowSessionListOverviewFacts/);
  assert.match(runtime, /workflowSessionScrollTopAfterRender/);
  assert.match(runtime, /jumpWorkflowSessionToLatest/);
  assert.match(runtime, /communication\/agent\/create/);
  assert.match(runtime, /communication\/conversation\/create/);
  assert.match(runtime, /communication\/message\/post/);
  assert.match(runtime, /communication\/inbox\/consume/);
  assert.match(runtime, /pendingConversationMessage/);
  assert.match(runtime, /detachCommunicationEndpointsBestEffort/);
  assert.match(runtime, /textContent/);
  assert.equal(/document\.cookie/.test(runtime), false);
  assert.match(runtime, /localStorage/);
  assert.match(runtime, /APPEARANCE_STORAGE_KEY/);
  assert.match(runtime, /LANGUAGE_STORAGE_KEY/);
  assert.match(runtime, /applyLanguage/);
  assert.match(runtime, /WebCodex 运行控制台/);
  assert.match(runtime, /document\.documentElement\.lang = runtimeLanguage/);
  assert.doesNotMatch(runtime, /localStorage\.(?:getItem|setItem)\(RUNTIME_CREDENTIAL_SESSION_KEY/);
  assert.match(runtime, /sessionStorage/);
  assert.match(runtime, /RUNTIME_CREDENTIAL_SESSION_KEY/);
  assert.match(runtime, /sessionStorage\.setItem\(RUNTIME_CREDENTIAL_SESSION_KEY/);
  assert.doesNotMatch(runtime, /localStorage\.setItem\(RUNTIME_CREDENTIAL_SESSION_KEY/);
  assert.match(runtime, /lock\("", false\)/);
  assert.match(runtime, /clearRememberedRuntimeCredential/);
  assert.match(runtime, /sessionsPanel\?\.remove\(\);\s*clearNode\(projectList\);\s*if \(projectList && sessionsPanel\)/);
  assert.match(runtime, /MOBILE_NAVIGATION_MEDIA/);
  assert.match(runtime, /setMobileNavigationOpen/);
  assert.match(runtime, /syncResponsiveNavigation/);
  assert.equal(/\.innerHTML\b|\binnerHTML\s*=/.test(runtime), false);
  await exec(process.execPath, ["--check", resolve(outputDirectory, "runtime.js")]);
  const styles = await readFile(resolve(outputDirectory, "styles.css"), "utf8");
  assert.match(styles, /workflow-session-summary-runtime/);
  const runtimeStyles = await readFile(resolve(outputDirectory, "runtime.css"), "utf8");
  assert.match(runtimeStyles, /max-width:\s*900px/);
  assert.match(runtimeStyles, /min-width:\s*1280px/);
  assert.match(runtimeStyles, /safe-area-inset-bottom/);
  assert.match(runtimeStyles, /safe-area-inset-top/);
  assert.match(runtimeStyles, /prefers-reduced-motion/);
  assert.match(runtimeStyles, /data-resolved-theme="light"/);
  assert.match(runtimeStyles, /data-language="zh-CN"/);
  assert.match(runtimeStyles, /PingFang SC/);
  assert.match(runtimeStyles, /backdrop-filter/);
  assert.match(runtimeStyles, /--ambient-three/);
  assert.match(runtimeStyles, /workspace-topbar\{position:relative;z-index:40/);
  assert.match(runtimeStyles, /runtime-shell\{[^}]*gap:0[^}]*padding:0/);
  assert.match(runtimeStyles, /workspace-main\{[^}]*background:var\(--page-surface\)/);
  assert.match(runtimeStyles, /--layout-major:61\.8%/);
  assert.match(runtimeStyles, /--layout-minor:38\.2%/);
  assert.match(runtimeStyles, /--sidebar-width:clamp\(280px,21vw,320px\)/);
  assert.match(runtimeStyles, /--content-width:760px/);
  assert.match(runtimeStyles, /--context-rail-width:clamp\(320px,26vw,420px\)/);
  assert.match(runtimeStyles, /runtime-shell\.context-docked\{--content-width:760px;grid-template-columns:var\(--sidebar-width\) minmax\(0,1fr\) var\(--context-rail-width\)/);
  assert.match(runtimeStyles, /message-card\.message-incoming\{[^}]*width:fit-content[^}]*max-width:min\(82%,880px\)/);
  assert.match(runtimeStyles, /message-card\.message-outgoing\{[^}]*max-width:min\(68%,680px\)[^}]*align-self:flex-end/);
  assert.match(runtimeStyles, /--message-bubble-radius:22px/);
  assert.doesNotMatch(runtimeStyles, /message-avatar/);
  assert.doesNotMatch(runtimeStyles, /--message-bubble-anchor-radius/);
  assert.match(runtimeStyles, /message-card\.message-incoming \.message-bubble\{[^}]*border:0[^}]*border-radius:var\(--message-bubble-radius\)/);
  assert.match(runtimeStyles, /project-row\.selected\{background:var\(--sidebar-selected\)/);
  assert.match(runtimeStyles, /device-project-list\{[^}]*border-left:0/);
  assert.match(runtimeStyles, /project-row-state/);
  assert.doesNotMatch(runtimeStyles, /project-row-path/);
  assert.match(runtimeStyles, /--message-incoming-bg:rgba\(255,255,255,\.055\)/);
  assert.match(runtimeStyles, /--message-outgoing-bg:#285487/);
  assert.match(runtimeStyles, /message-card\.message-incoming \.message-bubble\{[^}]*background:var\(--message-incoming-bg\)/);
  assert.match(runtimeStyles, /message-card\.message-outgoing \.message-bubble\{[^}]*background:var\(--message-outgoing-bg\)/);
  assert.match(runtimeStyles, /message-footer\{[^}]*display:flex/);
  assert.match(runtimeStyles, /message-card\s*\+\s*\.message-card/);
  assert.match(runtimeStyles, /project-row-icon/);
  assert.match(runtimeStyles, /session-card-icon/);
  assert.match(runtimeStyles, /message-action/);
  assert.match(runtimeStyles, /message-code-copy/);
  assert.match(runtimeStyles, /message-date-separator/);
  assert.match(runtimeStyles, /new-messages-button/);
  assert.match(runtimeStyles, /operations-stage/);
  assert.match(runtimeStyles, /topbar-more-popover/);
  assert.match(runtimeStyles, /composer-options-popover/);
  assert.match(runtimeStyles, /@keyframes composer-enter/);
  assert.match(runtimeStyles, /@keyframes message-enter/);
  assert.match(runtimeStyles, /send-btn\.is-ready/);
  assert.match(runtimeStyles, /session-state-chips \.chip:not\(:last-child\)\{display:none/);
  assert.match(runtimeStyles, /message-card\.message-group-continuation \.message-author\{display:none/);
  assert.match(runtimeStyles, /context-navigation/);
  await exec(process.execPath, ["--check", resolve(outputDirectory, "app.js")]);
  const admin = await readFile(resolve(outputDirectory, "admin.js"), "utf8");
  await exec(process.execPath, ["--check", resolve(outputDirectory, "admin.js")]);
  assert.equal(/localStorage|sessionStorage|document\.cookie/.test(admin), false);
  assert.equal(/innerHTML/.test(admin), false);
  assert.match(admin, /textContent/);
}

async function copySources(sourceDirectory) {
  await mkdir(sourceDirectory, { recursive: true });
  for (const source of [
    "app.ts",
    "review_state.ts",
    "workflow_session_state.ts",
    "styles.css",
    "console.html",
    "runtime.ts",
    "runtime_console_state.ts",
    "runtime.css",
    "runtime.html",
    "admin.ts",
    "admin_controller.ts",
    "admin_mutation_controller.ts",
    "admin_mutation_view.ts",
    "admin_view.ts",
    "admin.css",
    "admin.html",
  ]) {
    await copyFile(
      resolve(frontendRoot, "src", source),
      resolve(sourceDirectory, source)
    );
  }
}

async function waitFor(predicate, diagnostic, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 50));
  }
  throw new Error(diagnostic());
}

test("custom development build creates parseable fixed console assets", async () => {
  const outputDirectory = await mkdtemp(resolve(tmpdir(), "webcodex-assets-"));
  try {
    const result = await exec(process.execPath, [
      buildScript,
      "--out-dir",
      outputDirectory,
    ]);
    assert.match(result.stdout, /\[console\] built/);
    await assertRequiredAssets(outputDirectory);
  } finally {
    await rm(outputDirectory, { recursive: true, force: true });
  }
});

test(
  "watch mode rebuilds changed sources and preserves the last good bundle",
  { timeout: 20_000 },
  async () => {
    const workspace = await mkdtemp(resolve(tmpdir(), "webcodex-watch-"));
    const sourceDirectory = resolve(workspace, "src");
    const outputDirectory = resolve(workspace, "out");
    await copySources(sourceDirectory);
    const child = spawn(
      process.execPath,
      [
        buildScript,
        "--source-dir",
        sourceDirectory,
        "--out-dir",
        outputDirectory,
        "--watch",
      ],
      { stdio: ["ignore", "pipe", "pipe"] }
    );
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    try {
      await waitFor(
        () => stdout.includes("[console] watching"),
        () => `watcher did not start: ${stdout}\n${stderr}`
      );
      await assertRequiredAssets(outputDirectory);

      const changedCss = "body { color: rgb(1, 2, 3); }\n";
      await writeFile(resolve(sourceDirectory, "styles.css"), changedCss);
      await waitFor(
        async () =>
          (await readFile(resolve(outputDirectory, "styles.css"), "utf8")).includes(
            "rgb(1,2,3)"
          ),
        () => `watcher did not rebuild CSS: ${stdout}\n${stderr}`
      );

      const lastGoodApp = await readFile(resolve(outputDirectory, "app.js"), "utf8");
      await writeFile(resolve(sourceDirectory, "app.ts"), "const broken: = 1;\n");
      await waitFor(
        () => stderr.includes("[console] build failed:"),
        () => `watcher did not report the failed build: ${stdout}\n${stderr}`
      );
      assert.equal(
        await readFile(resolve(outputDirectory, "app.js"), "utf8"),
        lastGoodApp
      );
      await exec(process.execPath, ["--check", resolve(outputDirectory, "app.js")]);
    } finally {
      child.kill("SIGTERM");
      await new Promise((resolvePromise) => child.once("exit", resolvePromise));
      await rm(workspace, { recursive: true, force: true });
    }
  }
);

test("development output is ignored by Git", async () => {
  await exec(
    "git",
    ["check-ignore", "--quiet", "frontend/.dev-dist/app.js"],
    { cwd: repositoryRoot }
  );
});
