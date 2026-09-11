import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const PROTOCOL_VERSION = "webcodex-plugin-v1";
const STATUS_MAX_BYTES = 32 * 1024;
const here = path.dirname(fileURLToPath(import.meta.url));
const pluginPath = path.join(here, "dist", "plugin.js");

const EXPECTED_GIT_SUMMARY_TOOL = {
  name: "git_summary",
  title: "Git summary",
  description:
    "Observe only this repo-info Plugin provider's configured cwd and return a bounded Git branch, HEAD, and porcelain status summary. This tool accepts no repository path, does not discover another project or Runner, does not fetch or use the network, and does not modify the repository.",
  inputSchema: {
    type: "object",
    properties: {},
    required: [],
    additionalProperties: false,
  },
  outputSchema: {
    type: "object",
    properties: {
      branch: { type: "string", maxLength: 512 },
      head: { type: "string", maxLength: 64 },
      detached: { type: "boolean" },
      dirty: { type: "boolean" },
      status: { type: "string", maxLength: STATUS_MAX_BYTES },
      errorCode: {
        type: "string",
        enum: [
          "",
          "not_git_repository",
          "git_not_found",
          "git_timeout",
          "git_output_too_large",
          "git_failed",
        ],
        maxLength: 64,
      },
    },
    required: ["branch", "head", "detached", "dirty", "status", "errorCode"],
    additionalProperties: false,
  },
  annotations: {
    readOnlyHint: true,
    destructiveHint: false,
    idempotentHint: true,
    openWorldHint: false,
  },
};

function tempRoot() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "webcodex-repo-info-"));
}

function git(cwd, args, options = {}) {
  const result = spawnSync("git", args, {
    cwd,
    encoding: "utf8",
    shell: false,
    windowsHide: true,
    ...options,
  });
  assert.equal(result.error, undefined, result.error?.message);
  assert.equal(result.status, 0, `git ${args.join(" ")} failed: ${result.stderr}`);
  return result.stdout;
}

function initRepository() {
  const root = tempRoot();
  const hooks = path.join(root, "empty-hooks");
  fs.mkdirSync(hooks);
  git(root, ["init", "-q"]);
  fs.writeFileSync(path.join(root, "tracked.txt"), "initial\n");
  git(root, ["add", "tracked.txt"]);
  git(root, [
    "-c",
    "user.name=WebCodex Test",
    "-c",
    "user.email=webcodex-test@example.invalid",
    "-c",
    "commit.gpgSign=false",
    "-c",
    `core.hooksPath=${hooks}`,
    "commit",
    "-qm",
    "initial",
  ]);
  return root;
}

async function runProtocol(cwd, requests) {
  const child = spawn(process.execPath, [pluginPath], {
    cwd,
    stdio: ["pipe", "pipe", "pipe"],
    windowsHide: true,
    shell: false,
  });
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  for (const request of requests) child.stdin.write(`${JSON.stringify(request)}\n`);
  child.stdin.end();

  const exitCode = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error("repo-info protocol test timed out"));
    }, 10_000);
    child.once("exit", (code) => {
      clearTimeout(timer);
      resolve(code);
    });
  });

  assert.equal(exitCode, 0, stderr);
  assert.equal(stderr, "");
  const lines = stdout.split(/\r?\n/u).filter((line) => line.length > 0);
  assert.equal(lines.length, requests.length, `unexpected stdout framing: ${stdout}`);
  return lines.map((line) => JSON.parse(line));
}

function initializeRequest(id = 1) {
  return { jsonrpc: "2.0", id, method: "initialize", params: { protocolVersion: PROTOCOL_VERSION } };
}

function callRequest(id = 2) {
  return { jsonrpc: "2.0", id, method: "tools/call", params: { name: "git_summary", arguments: {} } };
}

async function callGitSummary(cwd) {
  const responses = await runProtocol(cwd, [initializeRequest(), callRequest()]);
  assert.equal(responses[0].result.protocolVersion, PROTOCOL_VERSION);
  return responses[1].result;
}

test("compiled Plugin preserves initialize/list/call framing for a clean repository", async () => {
  const root = initRepository();
  try {
    const branch = git(root, ["branch", "--show-current"]).trim();
    const head = git(root, ["rev-parse", "HEAD"]).trim();
    const responses = await runProtocol(root, [
      initializeRequest(1),
      { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
      callRequest(3),
    ]);

    assert.equal(responses[0].result.protocolVersion, PROTOCOL_VERSION);
    assert.deepEqual(responses[1].result.tools, [EXPECTED_GIT_SUMMARY_TOOL]);
    assert.equal(responses[2].result.isError, false);
    assert.deepEqual(responses[2].result.structuredContent, {
      branch,
      head,
      detached: false,
      dirty: false,
      status: `## ${branch}`,
      errorCode: "",
    });
    assert.equal(JSON.stringify(responses).includes(root), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("dirty repository status is bounded and observation does not modify worktree or index", async () => {
  const root = initRepository();
  try {
    const trackedPath = path.join(root, "tracked.txt");
    const untrackedPath = path.join(root, "untracked.txt");
    fs.writeFileSync(trackedPath, "changed\n");
    fs.writeFileSync(untrackedPath, "untracked\n");
    const trackedBefore = fs.readFileSync(trackedPath);
    const untrackedBefore = fs.readFileSync(untrackedPath);
    const indexPath = path.join(root, ".git", "index");
    const indexBefore = fs.readFileSync(indexPath);

    const result = await callGitSummary(root);

    assert.equal(result.isError, false);
    assert.equal(result.structuredContent.dirty, true);
    assert.match(result.structuredContent.status, / M tracked\.txt/mu);
    assert.match(result.structuredContent.status, /\?\? untracked\.txt/mu);
    assert.ok(Buffer.byteLength(result.structuredContent.status, "utf8") <= STATUS_MAX_BYTES);
    assert.deepEqual(fs.readFileSync(trackedPath), trackedBefore);
    assert.deepEqual(fs.readFileSync(untrackedPath), untrackedBefore);
    assert.deepEqual(fs.readFileSync(indexPath), indexBefore);
    assert.equal(JSON.stringify(result).includes(root), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("detached HEAD is reported without inventing a branch", async () => {
  const root = initRepository();
  try {
    git(root, ["checkout", "--detach", "-q"]);
    const head = git(root, ["rev-parse", "HEAD"]).trim();
    const result = await callGitSummary(root);
    assert.equal(result.isError, false);
    assert.equal(result.structuredContent.detached, true);
    assert.equal(result.structuredContent.branch, "");
    assert.equal(result.structuredContent.head, head);
    assert.equal(result.structuredContent.dirty, false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("non-Git cwd returns a known application error result", async () => {
  const root = tempRoot();
  try {
    const result = await callGitSummary(root);
    assert.equal(result.isError, true);
    assert.deepEqual(result.structuredContent, {
      branch: "",
      head: "",
      detached: false,
      dirty: false,
      status: "",
      errorCode: "not_git_repository",
    });
    assert.match(result.content[0].text, /configured repo-info provider cwd/u);
    assert.equal(JSON.stringify(result).includes(root), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("oversized porcelain status fails explicitly instead of returning a partial repository state", async () => {
  const root = initRepository();
  try {
    for (let index = 0; index < 1_500; index += 1) {
      fs.writeFileSync(path.join(root, `untracked-${String(index).padStart(4, "0")}-status-bound.txt`), "x");
    }
    const result = await callGitSummary(root);
    assert.equal(result.isError, true);
    assert.equal(result.structuredContent.errorCode, "git_output_too_large");
    assert.equal(result.structuredContent.status, "");
    assert.match(result.content[0].text, /no partial repository state/u);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});