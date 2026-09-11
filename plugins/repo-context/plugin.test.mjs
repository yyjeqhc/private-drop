import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  AFFECTED_PACKAGE_LIMIT,
  CHANGED_PATH_LIMIT,
  WARNING_LIMIT,
  WORKSPACE_MEMBER_LIMIT,
  observeRepoContext,
} from "./dist/context.js";

const PROTOCOL_VERSION = "webcodex-plugin-v1";
const here = path.dirname(fileURLToPath(import.meta.url));
const pluginPath = path.join(here, "dist", "plugin.js");
const HEAD = "a".repeat(40);

function tempRoot() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "webcodex-repo-context-"));
}

function git(cwd, args) {
  const result = spawnSync("git", args, {
    cwd,
    encoding: "utf8",
    shell: false,
    windowsHide: true,
  });
  assert.equal(result.error, undefined, result.error?.message);
  assert.equal(result.status, 0, `git ${args.join(" ")} failed: ${result.stderr}`);
  return result.stdout;
}

function writePackage(root, relative, name) {
  const packageRoot = path.join(root, relative);
  fs.mkdirSync(path.join(packageRoot, "src"), { recursive: true });
  fs.writeFileSync(
    path.join(packageRoot, "Cargo.toml"),
    `[package]\nname = "${name}"\nversion = "0.1.0"\nedition = "2021"\n`,
  );
  fs.writeFileSync(path.join(packageRoot, "src", "lib.rs"), `pub const NAME: &str = "${name}";\n`);
}

function initWorkspace() {
  const root = tempRoot();
  const hooks = path.join(root, "empty-hooks");
  fs.mkdirSync(hooks);
  git(root, ["init", "-q"]);
  fs.writeFileSync(
    path.join(root, "Cargo.toml"),
    '[workspace]\nmembers = ["crates/a", "crates/b"]\nresolver = "2"\n',
  );
  writePackage(root, "crates/a", "pkg-a");
  writePackage(root, "crates/b", "pkg-b");
  git(root, ["add", "."]);
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
      reject(new Error("repo-context protocol test timed out"));
    }, 15_000);
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
  return { jsonrpc: "2.0", id, method: "tools/call", params: { name: "repo_context", arguments: {} } };
}

async function callRepoContext(cwd) {
  const responses = await runProtocol(cwd, [initializeRequest(), callRequest()]);
  assert.equal(responses[0].result.protocolVersion, PROTOCOL_VERSION);
  return responses[1].result;
}

function fakeGitAndCargo(cwd, cargoResult, status = "") {
  return async (executable, args) => {
    if (executable === "git") {
      if (args.includes("--verify")) return { ok: true, stdout: `${HEAD}\n` };
      if (args.includes("--abbrev-ref")) return { ok: true, stdout: "main\n" };
      if (args.includes("status")) return { ok: true, stdout: status };
      return { ok: false, kind: "failed" };
    }
    if (executable === "cargo") return cargoResult;
    throw new Error(`unexpected executable ${executable} in ${cwd}`);
  };
}

function fakeMetadata(cwd, count) {
  const packages = [];
  const workspaceMembers = [];
  for (let index = 0; index < count; index += 1) {
    const suffix = String(index).padStart(3, "0");
    const id = `workspace-pkg-${suffix}`;
    workspaceMembers.push(id);
    packages.push({
      id,
      name: `pkg-${suffix}`,
      manifest_path: path.join(cwd, "crates", `pkg-${suffix}`, "Cargo.toml"),
    });
  }
  return JSON.stringify({ packages, workspace_members: workspaceMembers });
}

test("compiled Plugin lists the bounded read-only repo_context contract", async () => {
  const root = initWorkspace();
  try {
    const responses = await runProtocol(root, [
      initializeRequest(1),
      { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
    ]);
    const tools = responses[1].result.tools;
    assert.equal(tools.length, 1);
    assert.equal(tools[0].name, "repo_context");
    assert.deepEqual(tools[0].inputSchema, {
      type: "object",
      properties: {},
      required: [],
      additionalProperties: false,
    });
    assert.deepEqual(tools[0].annotations, {
      readOnlyHint: true,
      destructiveHint: false,
      idempotentHint: true,
      openWorldHint: false,
    });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("clean Git repository returns HEAD, branch, and Cargo workspace members", async () => {
  const root = initWorkspace();
  try {
    const branch = git(root, ["branch", "--show-current"]).trim();
    const head = git(root, ["rev-parse", "HEAD"]).trim();
    const result = await callRepoContext(root);
    assert.equal(result.isError, false);
    assert.equal(result.structuredContent.branch, branch);
    assert.equal(result.structuredContent.head, head);
    assert.equal(result.structuredContent.detached, false);
    assert.equal(result.structuredContent.dirty, false);
    assert.equal(result.structuredContent.cargoAvailable, true);
    assert.equal(result.structuredContent.workspaceMemberCount, 2);
    assert.deepEqual(result.structuredContent.workspaceMembers, ["pkg-a", "pkg-b"]);
    assert.deepEqual(result.structuredContent.affectedPackages, []);
    assert.equal(JSON.stringify(result).includes(root), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("dirty tracked file maps to its deepest workspace package", async () => {
  const root = initWorkspace();
  try {
    fs.writeFileSync(path.join(root, "crates", "a", "src", "lib.rs"), "pub const CHANGED: bool = true;\n");
    const result = await callRepoContext(root);
    assert.equal(result.isError, false);
    assert.equal(result.structuredContent.dirty, true);
    assert.equal(result.structuredContent.stagedCount, 0);
    assert.equal(result.structuredContent.unstagedCount, 1);
    assert.equal(result.structuredContent.untrackedCount, 0);
    assert.deepEqual(result.structuredContent.changedPaths, ["crates/a/src/lib.rs"]);
    assert.deepEqual(result.structuredContent.affectedPackages, ["pkg-a"]);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("untracked file is counted and maps to its workspace package", async () => {
  const root = initWorkspace();
  try {
    fs.writeFileSync(path.join(root, "crates", "b", "new.txt"), "new\n");
    const result = await callRepoContext(root);
    assert.equal(result.structuredContent.untrackedCount, 1);
    assert.deepEqual(result.structuredContent.changedPaths, ["crates/b/new.txt"]);
    assert.deepEqual(result.structuredContent.affectedPackages, ["pkg-b"]);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("detached HEAD is reported without inventing a branch", async () => {
  const root = initWorkspace();
  try {
    git(root, ["checkout", "--detach", "-q"]);
    const result = await callRepoContext(root);
    assert.equal(result.isError, false);
    assert.equal(result.structuredContent.detached, true);
    assert.equal(result.structuredContent.branch, "");
    assert.equal(result.structuredContent.head, git(root, ["rev-parse", "HEAD"]).trim());
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("root shared change conservatively affects all observed workspace packages", async () => {
  const root = initWorkspace();
  try {
    fs.appendFileSync(path.join(root, "Cargo.toml"), "\n# shared workspace change\n");
    const result = await callRepoContext(root);
    assert.deepEqual(result.structuredContent.changedPaths, ["Cargo.toml"]);
    assert.deepEqual(result.structuredContent.affectedPackages, ["pkg-a", "pkg-b"]);
    assert.match(result.structuredContent.warnings.join("\n"), /conservatively mark all/u);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Git command failure produces a bounded partial error without leaking cwd", async () => {
  const root = tempRoot();
  try {
    const observation = await observeRepoContext(root, async (executable) => {
      if (executable === "git") return { ok: false, kind: "failed" };
      throw new Error("cargo should not run without a root Cargo.toml");
    });
    assert.equal(observation.gitAvailable, false);
    assert.equal(observation.structured.head, "");
    assert.match(observation.structured.warnings.join("\n"), /git observation failed/u);
    assert.equal(JSON.stringify(observation).includes(root), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Cargo unavailable, malformed metadata, and timeout preserve Git context", async (t) => {
  for (const [name, cargoResult, warning] of [
    ["unavailable", { ok: false, kind: "not_found" }, /cargo executable is unavailable/u],
    ["malformed", { ok: true, stdout: "{not-json" }, /malformed JSON/u],
    ["timeout", { ok: false, kind: "timeout" }, /cargo observation timed out/u],
  ]) {
    await t.test(name, async () => {
      const root = tempRoot();
      try {
        fs.writeFileSync(path.join(root, "Cargo.toml"), "[workspace]\nmembers = []\n");
        const observation = await observeRepoContext(root, fakeGitAndCargo(root, cargoResult));
        assert.equal(observation.gitAvailable, true);
        assert.equal(observation.structured.head, HEAD);
        assert.equal(observation.structured.cargoAvailable, false);
        assert.match(observation.structured.warnings.join("\n"), warning);
      } finally {
        fs.rmSync(root, { recursive: true, force: true });
      }
    });
  }
});

test("bounded projections truncate changed paths, members, affected packages, and warnings", async () => {
  const root = tempRoot();
  try {
    fs.writeFileSync(path.join(root, "Cargo.toml"), "[workspace]\nmembers = []\n");
    const status = Array.from({ length: 150 }, (_, index) => {
      const suffix = String(index).padStart(3, "0");
      return `?? ${index < 140 ? `crates/pkg-${suffix}/src/new.rs` : `misc-${suffix}.txt`}\0`;
    }).join("");
    const metadata = fakeMetadata(root, 140);
    const observation = await observeRepoContext(
      root,
      fakeGitAndCargo(root, { ok: true, stdout: metadata }, status),
    );
    const value = observation.structured;
    assert.equal(value.totalChangedPaths, 150);
    assert.equal(value.changedPaths.length, CHANGED_PATH_LIMIT);
    assert.equal(value.changedPathsTruncated, true);
    assert.equal(value.workspaceMemberCount, 140);
    assert.equal(value.workspaceMembers.length, WORKSPACE_MEMBER_LIMIT);
    assert.equal(value.workspaceMembersTruncated, true);
    assert.equal(value.affectedPackages.length, AFFECTED_PACKAGE_LIMIT);
    assert.equal(value.affectedPackagesTruncated, true);
    assert.ok(value.warnings.length <= WARNING_LIMIT);
    assert.equal(JSON.stringify(value).includes(root), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
