import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  moveToFreedesktopTrash,
  resolveAuthorizedTarget,
  runTrashBackend,
  safeDelete,
} from "./dist/domain.js";

const PROTOCOL_VERSION = "webcodex-plugin-v1";
const here = path.dirname(fileURLToPath(import.meta.url));
const pluginPath = path.join(here, "dist", "plugin.js");

const EXPECTED_SAFE_DELETE_TOOL = {
  name: "safe_delete",
  title: "Safe delete",
  description:
    "Move exactly one ordinary file or directory under this Plugin provider's configured cwd to the operating system Trash/Recycle Bin. Use this instead of permanent deletion when recovery may be needed. The path must be relative to the provider cwd. The provider cwd itself, paths that escape it, symlinks/junctions, and unsupported file types are rejected. This tool never permanently deletes the requested path through rm, unlink, Remove-Item, or another fallback.",
  inputSchema: {
    type: "object",
    properties: {
      path: {
        type: "string",
        minLength: 1,
        maxLength: 4096,
        description:
          "One file or directory path relative to the Plugin provider cwd. Absolute paths and parent traversal are rejected.",
      },
    },
    required: ["path"],
    additionalProperties: false,
  },
  outputSchema: {
    type: "object",
    properties: {
      outcome: {
        type: "string",
        enum: ["trashed", "already_absent", "rejected", "failed", "unknown"],
      },
      path: { type: "string", maxLength: 4096 },
      backend: {
        type: "string",
        enum: ["none", "freedesktop", "gio", "trash-put", "foundation", "powershell"],
      },
      errorCode: { type: "string", maxLength: 128 },
    },
    required: ["outcome", "path", "backend", "errorCode"],
    additionalProperties: false,
  },
  annotations: {
    readOnlyHint: false,
    destructiveHint: true,
    idempotentHint: false,
    openWorldHint: true,
  },
};

function tempRoot() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "webcodex-safe-delete-"));
}

test("absolute paths, root deletion, and parent traversal are rejected", () => {
  const root = tempRoot();
  try {
    assert.equal(resolveAuthorizedTarget(path.resolve(root, "victim"), root).code, "absolute_path_forbidden");
    assert.equal(resolveAuthorizedTarget(".", root).code, "root_delete_forbidden");
    assert.equal(resolveAuthorizedTarget("../victim", root).code, "parent_traversal_forbidden");
    assert.equal(resolveAuthorizedTarget(`nested${path.sep}..${path.sep}victim`, root).code, "parent_traversal_forbidden");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("missing target is a safe no-op and never invokes a backend", () => {
  const root = tempRoot();
  let calls = 0;
  try {
    const value = safeDelete(
      { path: "missing.txt" },
      {
        root,
        platform: "linux",
        spawnSync: () => {
          calls += 1;
          return { status: 0 };
        },
      },
    );
    assert.equal(value.isError, false);
    assert.equal(value.structuredContent.outcome, "already_absent");
    assert.equal(calls, 0);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("symlink target is rejected without invoking Trash", { skip: process.platform === "win32" }, () => {
  const root = tempRoot();
  const outside = tempRoot();
  let calls = 0;
  try {
    fs.writeFileSync(path.join(outside, "target.txt"), "outside");
    fs.symlinkSync(path.join(outside, "target.txt"), path.join(root, "link.txt"));
    const value = safeDelete(
      { path: "link.txt" },
      {
        root,
        platform: "linux",
        spawnSync: () => {
          calls += 1;
          return { status: 0 };
        },
      },
    );
    assert.equal(value.isError, true);
    assert.equal(value.structuredContent.errorCode, "symlink_unsupported");
    assert.equal(calls, 0);
    assert.equal(fs.readFileSync(path.join(outside, "target.txt"), "utf8"), "outside");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(outside, { recursive: true, force: true });
  }
});

test("symlinked parent escape is rejected", { skip: process.platform === "win32" }, () => {
  const root = tempRoot();
  const outside = tempRoot();
  try {
    fs.writeFileSync(path.join(outside, "victim.txt"), "outside");
    fs.symlinkSync(outside, path.join(root, "escape"));
    const resolved = resolveAuthorizedTarget("escape/victim.txt", root);
    assert.equal(resolved.ok, false);
    assert.equal(resolved.code, "path_outside_root");
    assert.equal(fs.readFileSync(path.join(outside, "victim.txt"), "utf8"), "outside");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(outside, { recursive: true, force: true });
  }
});

test("freedesktop backend creates Trash metadata and moves the requested file", () => {
  const root = tempRoot();
  const dataHome = tempRoot();
  const target = path.join(root, "victim #1.txt");
  fs.writeFileSync(target, "recoverable");
  try {
    const value = moveToFreedesktopTrash(target, {
      env: { XDG_DATA_HOME: dataHome },
      home: root,
      now: new Date(2026, 8, 5, 12, 34, 56),
    });
    assert.equal(value.state, "success");
    assert.equal(fs.existsSync(target), false);
    const files = fs.readdirSync(path.join(dataHome, "Trash", "files"));
    const infos = fs.readdirSync(path.join(dataHome, "Trash", "info"));
    assert.equal(files.length, 1);
    assert.equal(infos.length, 1);
    assert.equal(
      fs.readFileSync(path.join(dataHome, "Trash", "files", files[0]), "utf8"),
      "recoverable",
    );
    const info = fs.readFileSync(path.join(dataHome, "Trash", "info", infos[0]), "utf8");
    assert.match(info, /^\[Trash Info\]/m);
    assert.match(info, /victim%20%231\.txt/);
    assert.match(info, /DeletionDate=2026-09-05T12:34:56/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(dataHome, { recursive: true, force: true });
  }
});

test("Linux falls back to gio when freedesktop Trash cannot start", () => {
  const calls = [];
  const root = tempRoot();
  const target = path.join(root, "victim");
  fs.writeFileSync(target, "x");
  try {
    const value = runTrashBackend(target, {
      platform: "linux",
      spawnSync: (command, args) => {
        calls.push([command, args]);
        return { status: 0 };
      },
      env: { XDG_DATA_HOME: "relative-not-usable" },
    });
    assert.equal(value.backend, "gio");
    assert.equal(value.state, "success");
    assert.deepEqual(calls, [["gio", ["trash", target]]]);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Linux does not try another backend after a real gio failure", () => {
  const calls = [];
  const root = tempRoot();
  const target = path.join(root, "victim");
  fs.writeFileSync(target, "x");
  try {
    const value = runTrashBackend(target, {
      platform: "linux",
      env: { XDG_DATA_HOME: "relative-not-usable" },
      spawnSync: (command, args) => {
        calls.push([command, args]);
        return { status: 7 };
      },
    });
    assert.equal(value.backend, "gio");
    assert.equal(value.state, "failed");
    assert.equal(calls.length, 1);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("Linux fails closed when no Trash backend is installed", () => {
  const missing = () => ({ error: { code: "ENOENT" } });
  const root = tempRoot();
  const target = path.join(root, "victim");
  fs.writeFileSync(target, "x");
  try {
    assert.deepEqual(
      runTrashBackend(target, {
        platform: "linux",
        spawnSync: missing,
        env: { XDG_DATA_HOME: "relative-not-usable" },
      }),
      { backend: "none", state: "missing" },
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("macOS uses Foundation Trash and passes the target as argv instead of interpolating it", () => {
  const calls = [];
  const target = `/tmp/quote-'-$HOME-victim`;
  runTrashBackend(target, {
    platform: "darwin",
    spawnSync: (command, args) => {
      calls.push({ command, args });
      return { status: 0 };
    },
  });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, "/usr/bin/osascript");
  assert.equal(calls[0].args.at(-1), target);
  assert.equal(calls[0].args.at(-2).includes(target), false);
  assert.deepEqual(calls[0].args.slice(0, 3), ["-l", "JavaScript", "-e"]);
  assert.match(calls[0].args.at(-2), /NSFileManager/);
  assert.equal(calls[0].args.at(-2).includes("Finder"), false);
});

test("Windows passes the target through environment and only falls back when powershell is missing", () => {
  const calls = [];
  const target = String.raw`C:\repo\odd ' victim.txt`;
  const value = runTrashBackend(target, {
    platform: "win32",
    env: { PATH: "test" },
    spawnSync: (command, args, options) => {
      calls.push({ command, args, options });
      if (command === "powershell.exe") return { error: { code: "ENOENT" } };
      return { status: 0 };
    },
  });
  assert.equal(value.backend, "powershell");
  assert.equal(value.state, "success");
  assert.deepEqual(calls.map((call) => call.command), ["powershell.exe", "pwsh.exe"]);
  for (const call of calls) {
    assert.equal(call.options.env.WEBCODEX_SAFE_DELETE_TARGET, target);
    assert.equal(call.args.join(" ").includes(target), false);
    assert.equal(call.args.join(" ").includes("Remove-Item"), false);
  }
});

test("backend failure never removes the target or exposes raw backend stderr", () => {
  const root = tempRoot();
  const target = path.join(root, "victim.txt");
  fs.writeFileSync(target, "keep");
  try {
    const value = safeDelete(
      { path: "victim.txt" },
      {
        root,
        platform: "linux",
        env: { XDG_DATA_HOME: "relative-not-usable" },
        spawnSync: () => ({ status: 9, stderr: `secret:${root}` }),
      },
    );
    assert.equal(value.isError, true);
    assert.equal(value.structuredContent.outcome, "failed");
    assert.equal(value.structuredContent.errorCode, "trash_operation_failed");
    assert.equal(fs.readFileSync(target, "utf8"), "keep");
    assert.equal(JSON.stringify(value).includes(root), false);
    assert.equal(JSON.stringify(value).includes("secret:"), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("backend failure after the target disappears is outcome unknown", () => {
  const root = tempRoot();
  const target = path.join(root, "victim.txt");
  fs.writeFileSync(target, "maybe trashed");
  try {
    const value = safeDelete(
      { path: "victim.txt" },
      {
        root,
        platform: "linux",
        env: { XDG_DATA_HOME: "relative-not-usable" },
        spawnSync: () => {
          fs.unlinkSync(target);
          return { status: 9 };
        },
      },
    );
    assert.equal(value.isError, true);
    assert.equal(value.structuredContent.outcome, "unknown");
    assert.equal(value.structuredContent.errorCode, "trash_operation_unknown");
    assert.match(value.content[0].text, /inspect/i);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("timeout is reported as unknown and does not invite automatic retry", () => {
  const root = tempRoot();
  const target = path.join(root, "victim.txt");
  fs.writeFileSync(target, "keep");
  try {
    const value = safeDelete(
      { path: "victim.txt" },
      {
        root,
        platform: "linux",
        env: { XDG_DATA_HOME: "relative-not-usable" },
        spawnSync: () => ({ error: { code: "ETIMEDOUT" } }),
      },
    );
    assert.equal(value.isError, true);
    assert.equal(value.structuredContent.outcome, "unknown");
    assert.equal(value.structuredContent.errorCode, "trash_operation_timeout");
    assert.match(value.content[0].text, /inspect/i);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("successful backend must make the target absent before reporting trashed", () => {
  const root = tempRoot();
  const target = path.join(root, "victim.txt");
  fs.writeFileSync(target, "trash me");
  try {
    const value = safeDelete(
      { path: "victim.txt" },
      {
        root,
        platform: "linux",
        env: { XDG_DATA_HOME: "relative-not-usable" },
        spawnSync: () => {
          fs.unlinkSync(target);
          return { status: 0 };
        },
      },
    );
    assert.equal(value.isError, false);
    assert.equal(value.structuredContent.outcome, "trashed");
    assert.equal(value.structuredContent.backend, "gio");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("TypeScript SDK preserves initialize/list/call wire behavior for an absent path", async () => {
  const root = tempRoot();
  const child = spawn(process.execPath, [pluginPath], {
    cwd: root,
    stdio: ["pipe", "pipe", "pipe"],
  });
  const lines = readline.createInterface({ input: child.stdout, crlfDelay: Infinity });
  const responses = [];
  lines.on("line", (line) => responses.push(JSON.parse(line)));

  child.stdin.write(
    `${JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: PROTOCOL_VERSION } })}\n`,
  );
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} })}\n`);
  child.stdin.write(
    `${JSON.stringify({ jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "safe_delete", arguments: { path: "already-gone.txt" } } })}\n`,
  );
  child.stdin.end();

  const exitCode = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error("plugin protocol smoke timed out"));
    }, 5000);
    child.once("exit", (code) => {
      clearTimeout(timer);
      resolve(code);
    });
  });
  try {
    assert.equal(exitCode, 0);
    assert.equal(responses.length, 3);
    assert.equal(responses[0].result.protocolVersion, PROTOCOL_VERSION);
    assert.deepEqual(responses[1].result.tools, [EXPECTED_SAFE_DELETE_TOOL]);
    assert.deepEqual(responses[2].result, {
      content: [{ type: "text", text: "Nothing was deleted: already-gone.txt is already absent." }],
      structuredContent: {
        outcome: "already_absent",
        path: "already-gone.txt",
        backend: "none",
        errorCode: "",
      },
      isError: false,
    });
  } finally {
    lines.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});
