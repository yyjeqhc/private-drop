// WebCodex Safe Delete Native Tool Plugin.
//
// Moves one file or directory under the configured Plugin cwd to the operating
// system Trash/Recycle Bin. It never permanently deletes the requested path as a fallback.

import { spawnSync as nodeSpawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { errorResult, textResult } from "@yyjeqhc/webcodex-plugin-sdk";

export const MAX_PATH_CHARS = 4096;
const BACKEND_TIMEOUT_MS = 10_000;
const BACKEND_MAX_BUFFER = 64 * 1024;

function safeRelativeDisplay(value) {
  if (typeof value !== "string") return "[invalid path]";
  const withoutControls = value.replace(/[\u0000-\u001f\u007f]/gu, "?");
  return withoutControls.slice(0, MAX_PATH_CHARS);
}

function structured(outcome, relativePath, backend = "none", errorCode = "") {
  return {
    outcome,
    path: safeRelativeDisplay(relativePath),
    backend,
    errorCode,
  };
}

function rejected(relativePath, code, message) {
  return errorResult(message, structured("rejected", relativePath, "none", code));
}

function failed(relativePath, backend, code, message, outcome = "failed") {
  return errorResult(message, structured(outcome, relativePath, backend, code));
}

function isWithinOrEqual(root, candidate) {
  const relative = path.relative(root, candidate);
  return (
    relative === "" ||
    (!path.isAbsolute(relative) && relative !== ".." && !relative.startsWith(`..${path.sep}`))
  );
}

function isStrictlyWithin(root, candidate) {
  return candidate !== root && isWithinOrEqual(root, candidate);
}

function hasParentTraversal(value) {
  return value.split(/[\\/]+/u).some((component) => component === "..");
}

function realpathNative(value) {
  return fs.realpathSync.native ? fs.realpathSync.native(value) : fs.realpathSync(value);
}

function nearestExistingAncestor(value) {
  let current = path.dirname(value);
  for (;;) {
    try {
      return realpathNative(current);
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
    const parent = path.dirname(current);
    if (parent === current) throw new Error("no existing ancestor");
    current = parent;
  }
}

export function resolveAuthorizedTarget(relativePath, root = process.cwd()) {
  if (typeof relativePath !== "string") {
    return { ok: false, code: "invalid_path", message: "safe_delete requires a string path" };
  }
  if (
    relativePath.length === 0 ||
    relativePath.length > MAX_PATH_CHARS ||
    /[\u0000-\u001f\u007f]/u.test(relativePath)
  ) {
    return {
      ok: false,
      code: "invalid_path",
      message: "safe_delete path is empty, too long, or contains control characters",
    };
  }
  if (path.isAbsolute(relativePath)) {
    return {
      ok: false,
      code: "absolute_path_forbidden",
      message: "safe_delete accepts only paths relative to the Plugin provider cwd",
    };
  }
  if (hasParentTraversal(relativePath)) {
    return {
      ok: false,
      code: "parent_traversal_forbidden",
      message: "safe_delete rejects parent traversal components",
    };
  }

  let canonicalRoot;
  try {
    canonicalRoot = realpathNative(root);
  } catch {
    return {
      ok: false,
      code: "root_unavailable",
      message: "safe_delete could not resolve the Plugin provider cwd",
    };
  }

  const lexicalTarget = path.resolve(canonicalRoot, relativePath);
  if (lexicalTarget === canonicalRoot) {
    return {
      ok: false,
      code: "root_delete_forbidden",
      message: "safe_delete refuses to move the Plugin provider cwd itself to Trash",
    };
  }
  if (!isStrictlyWithin(canonicalRoot, lexicalTarget)) {
    return {
      ok: false,
      code: "path_outside_root",
      message: "safe_delete path is outside the Plugin provider cwd",
    };
  }

  let metadata;
  try {
    metadata = fs.lstatSync(lexicalTarget);
  } catch (error) {
    if (error?.code !== "ENOENT") {
      return {
        ok: false,
        code: "path_inspection_failed",
        message: "safe_delete could not inspect the requested path",
      };
    }
    try {
      const ancestor = nearestExistingAncestor(lexicalTarget);
      if (!isWithinOrEqual(canonicalRoot, ancestor)) {
        return {
          ok: false,
          code: "path_outside_root",
          message: "safe_delete path resolves outside the Plugin provider cwd",
        };
      }
    } catch {
      return {
        ok: false,
        code: "path_inspection_failed",
        message: "safe_delete could not verify the requested path boundary",
      };
    }
    return {
      ok: true,
      absent: true,
      root: canonicalRoot,
      target: lexicalTarget,
      displayPath: relativePath,
    };
  }

  if (metadata.isSymbolicLink()) {
    return {
      ok: false,
      code: "symlink_unsupported",
      message:
        "safe_delete refuses symlinks and junctions because cross-platform Trash backends differ in link-following behavior",
    };
  }
  if (!metadata.isFile() && !metadata.isDirectory()) {
    return {
      ok: false,
      code: "unsupported_file_type",
      message: "safe_delete supports only ordinary files and directories",
    };
  }

  let canonicalTarget;
  try {
    canonicalTarget = realpathNative(lexicalTarget);
  } catch {
    return {
      ok: false,
      code: "path_inspection_failed",
      message: "safe_delete could not resolve the requested path",
    };
  }
  if (!isStrictlyWithin(canonicalRoot, canonicalTarget)) {
    return {
      ok: false,
      code: "path_outside_root",
      message: "safe_delete path resolves outside the Plugin provider cwd",
    };
  }

  return {
    ok: true,
    absent: false,
    root: canonicalRoot,
    target: canonicalTarget,
    displayPath: relativePath,
    kind: metadata.isDirectory() ? "directory" : "file",
  };
}

function normalizeSpawnResult(value) {
  if (value?.error?.code === "ENOENT") return { state: "missing" };
  if (value?.error?.code === "ETIMEDOUT") return { state: "timeout" };
  if (value?.error) return { state: "failed" };
  if (value?.status === 0) return { state: "success" };
  return { state: "failed" };
}

function spawnBackend(spawnSync, command, args, options = {}) {
  return normalizeSpawnResult(
    spawnSync(command, args, {
      shell: false,
      windowsHide: true,
      timeout: BACKEND_TIMEOUT_MS,
      maxBuffer: BACKEND_MAX_BUFFER,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      ...options,
    }),
  );
}

const MACOS_TRASH_JXA = String.raw`
ObjC.import('Foundation');
function run(argv) {
  const url = $.NSURL.fileURLWithPath(argv[0]);
  const resultURL = Ref();
  const error = Ref();
  const ok = $.NSFileManager.defaultManager.trashItemAtURLResultingItemURLError(
    url,
    resultURL,
    error,
  );
  if (!ok) throw new Error('Trash operation failed');
}
`.trim();

const WINDOWS_TRASH_SCRIPT = String.raw`
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName Microsoft.VisualBasic
$target = $env:WEBCODEX_SAFE_DELETE_TARGET
if ([string]::IsNullOrWhiteSpace($target)) { throw 'missing target' }
$item = Get-Item -LiteralPath $target -Force
$ui = [Microsoft.VisualBasic.FileIO.UIOption]::OnlyErrorDialogs
$recycle = [Microsoft.VisualBasic.FileIO.RecycleOption]::SendToRecycleBin
$cancel = [Microsoft.VisualBasic.FileIO.UICancelOption]::ThrowException
if ($item.PSIsContainer) {
  [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory($target, $ui, $recycle, $cancel)
} else {
  [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile($target, $ui, $recycle, $cancel)
}
`.trim();

function ensureRealDirectory(directory) {
  try {
    fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
    const metadata = fs.lstatSync(directory);
    return metadata.isDirectory() && !metadata.isSymbolicLink();
  } catch {
    return false;
  }
}

function trashDeletionDate(now = new Date()) {
  const pad = (value) => String(value).padStart(2, "0");
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}T${pad(now.getHours())}:${pad(now.getMinutes())}:${pad(now.getSeconds())}`;
}

function trashInfoPathValue(target) {
  return target.split("/").map((component) => encodeURIComponent(component)).join("/");
}

export function moveToFreedesktopTrash(
  target,
  { env = process.env, home = os.homedir(), now = new Date() } = {},
) {
  const dataHome = env.XDG_DATA_HOME?.trim() || (home ? path.join(home, ".local", "share") : "");
  if (!dataHome || !path.isAbsolute(dataHome)) return { state: "unavailable" };

  const trashRoot = path.join(dataHome, "Trash");
  const filesDir = path.join(trashRoot, "files");
  const infoDir = path.join(trashRoot, "info");
  if (!ensureRealDirectory(filesDir) || !ensureRealDirectory(infoDir)) {
    return { state: "unavailable" };
  }

  const base = path.basename(target) || "item";
  let destination;
  let infoPath;
  let tempInfo;
  for (let attempt = 0; attempt < 8; attempt += 1) {
    const suffix = randomBytes(8).toString("hex");
    const name = `${base}.${suffix}`;
    const candidateDestination = path.join(filesDir, name);
    const candidateInfo = path.join(infoDir, `${name}.trashinfo`);
    const candidateTemp = path.join(infoDir, `.${name}.trashinfo.webcodex-tmp`);
    if (
      !fs.existsSync(candidateDestination) &&
      !fs.existsSync(candidateInfo) &&
      !fs.existsSync(candidateTemp)
    ) {
      destination = candidateDestination;
      infoPath = candidateInfo;
      tempInfo = candidateTemp;
      break;
    }
  }
  if (!destination || !infoPath || !tempInfo) return { state: "unavailable" };

  const info = `[Trash Info]\nPath=${trashInfoPathValue(target)}\nDeletionDate=${trashDeletionDate(now)}\n`;
  try {
    fs.writeFileSync(tempInfo, info, { encoding: "utf8", flag: "wx", mode: 0o600 });
  } catch {
    return { state: "unavailable" };
  }

  try {
    // rename is intentionally required: EXDEV fails closed rather than copying
    // and permanently unlinking the requested path across filesystems.
    fs.renameSync(target, destination);
  } catch {
    try {
      fs.rmSync(tempInfo, { force: true });
    } catch {
      // Cleanup concerns only our own unpublished metadata temp file.
    }
    return { state: "unavailable" };
  }

  try {
    fs.renameSync(tempInfo, infoPath);
  } catch {
    // The requested object is already inside Trash/files. Do not attempt to
    // move it back or report a definite failure: recovery metadata publication
    // is incomplete, so the effect is explicitly unknown.
    return { state: "unknown" };
  }
  return { state: "success" };
}

export function runTrashBackend(
  target,
  { platform = process.platform, spawnSync = nodeSpawnSync, env = process.env } = {},
) {
  if (platform === "linux") {
    const freedesktop = moveToFreedesktopTrash(target, { env });
    if (freedesktop.state === "success" || freedesktop.state === "unknown") {
      return { backend: "freedesktop", ...freedesktop };
    }
    const gio = spawnBackend(spawnSync, "gio", ["trash", target]);
    if (gio.state !== "missing") return { backend: "gio", ...gio };
    const trashPut = spawnBackend(spawnSync, "trash-put", [target]);
    if (trashPut.state !== "missing") return { backend: "trash-put", ...trashPut };
    return { backend: "none", state: "missing" };
  }

  if (platform === "darwin") {
    const foundation = spawnBackend(spawnSync, "/usr/bin/osascript", [
      "-l",
      "JavaScript",
      "-e",
      MACOS_TRASH_JXA,
      target,
    ]);
    return {
      backend: foundation.state === "missing" ? "none" : "foundation",
      ...foundation,
    };
  }

  if (platform === "win32") {
    const childEnv = { ...env, WEBCODEX_SAFE_DELETE_TARGET: target };
    const args = ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", WINDOWS_TRASH_SCRIPT];
    const powershell = spawnBackend(spawnSync, "powershell.exe", args, { env: childEnv });
    if (powershell.state !== "missing") return { backend: "powershell", ...powershell };
    const pwsh = spawnBackend(spawnSync, "pwsh.exe", args, { env: childEnv });
    if (pwsh.state !== "missing") return { backend: "powershell", ...pwsh };
    return { backend: "none", state: "missing" };
  }

  return { backend: "none", state: "missing" };
}

function targetStillExists(target) {
  try {
    fs.lstatSync(target);
    return true;
  } catch (error) {
    if (error?.code === "ENOENT") return false;
    throw error;
  }
}

export function safeDelete(
  argumentsValue,
  {
    root = process.cwd(),
    platform = process.platform,
    spawnSync = nodeSpawnSync,
    env = process.env,
  } = {},
) {
  const requestedPath = argumentsValue?.path;
  const displayPath = safeRelativeDisplay(requestedPath);
  const target = resolveAuthorizedTarget(requestedPath, root);
  if (!target.ok) return rejected(displayPath, target.code, target.message);
  if (target.absent) {
    return textResult(
      `Nothing was deleted: ${displayPath} is already absent.`,
      structured("already_absent", displayPath),
    );
  }

  const backendResult = runTrashBackend(target.target, { platform, spawnSync, env });
  if (backendResult.state === "missing") {
    return failed(
      displayPath,
      "none",
      "trash_backend_unavailable",
      "No supported system Trash/Recycle Bin backend is available; safe_delete did not fall back to permanent deletion.",
    );
  }
  if (backendResult.state === "timeout") {
    return failed(
      displayPath,
      backendResult.backend,
      "trash_operation_timeout",
      "The system Trash operation timed out. Its effect is unknown; inspect the path or Trash before retrying.",
      "unknown",
    );
  }
  if (backendResult.state === "unknown") {
    return failed(
      displayPath,
      backendResult.backend,
      "trash_operation_unknown",
      "The item may already be inside the system Trash, but recovery metadata could not be confirmed. Inspect the path and Trash before retrying.",
      "unknown",
    );
  }
  if (backendResult.state !== "success") {
    try {
      if (!targetStillExists(target.target)) {
        return failed(
          displayPath,
          backendResult.backend,
          "trash_operation_unknown",
          "The Trash backend reported a failure but the original path is now absent. Its effect is unknown; inspect the path and Trash before retrying.",
          "unknown",
        );
      }
    } catch {
      return failed(
        displayPath,
        backendResult.backend,
        "trash_postcondition_unknown",
        "The Trash backend reported a failure and safe_delete could not verify whether the original path still exists. Inspect the path and Trash before retrying.",
        "unknown",
      );
    }
    return failed(
      displayPath,
      backendResult.backend,
      "trash_operation_failed",
      "The system Trash/Recycle Bin backend reported a failure and the original path is still present; safe_delete did not use a permanent-delete fallback.",
    );
  }

  try {
    if (targetStillExists(target.target)) {
      return failed(
        displayPath,
        backendResult.backend,
        "trash_postcondition_unknown",
        "The Trash backend returned success but the path still exists. The effect is unknown; inspect the path and Trash before retrying.",
        "unknown",
      );
    }
  } catch {
    return failed(
      displayPath,
      backendResult.backend,
      "trash_postcondition_unknown",
      "The Trash backend returned success but safe_delete could not verify the postcondition. Inspect the path and Trash before retrying.",
      "unknown",
    );
  }

  return textResult(
    `Moved ${displayPath} to the system Trash/Recycle Bin.`,
    structured("trashed", displayPath, backendResult.backend),
  );
}
