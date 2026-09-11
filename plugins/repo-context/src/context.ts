import { execFile } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

export const CHANGED_PATH_LIMIT = 128;
export const WORKSPACE_MEMBER_LIMIT = 128;
export const AFFECTED_PACKAGE_LIMIT = 128;
export const WARNING_LIMIT = 16;
export const PATH_MAX_CHARS = 512;
export const PACKAGE_NAME_MAX_CHARS = 256;
export const WARNING_MAX_CHARS = 512;

const GIT_TIMEOUT_MS = 5_000;
const CARGO_TIMEOUT_MS = 10_000;
const GIT_MAX_BUFFER = 256 * 1024;
const CARGO_MAX_BUFFER = 8 * 1024 * 1024;
const HEAD_MAX_CHARS = 64;
const BRANCH_MAX_CHARS = 512;

type CommandFailureKind = "not_found" | "timeout" | "too_large" | "failed";

export interface CommandOptions {
  readonly cwd: string;
  readonly timeoutMs: number;
  readonly maxBuffer: number;
}

export type CommandResult =
  | { readonly ok: true; readonly stdout: string }
  | { readonly ok: false; readonly kind: CommandFailureKind };

export type CommandRunner = (
  executable: string,
  args: readonly string[],
  options: CommandOptions,
) => Promise<CommandResult>;

export interface RepoContextStructured {
  readonly branch: string;
  readonly head: string;
  readonly detached: boolean;
  readonly dirty: boolean;
  readonly stagedCount: number;
  readonly unstagedCount: number;
  readonly untrackedCount: number;
  readonly changedPaths: string[];
  readonly totalChangedPaths: number;
  readonly changedPathsTruncated: boolean;
  readonly cargoAvailable: boolean;
  readonly workspaceMemberCount: number;
  readonly workspaceMembers: string[];
  readonly workspaceMembersTruncated: boolean;
  readonly affectedPackages: string[];
  readonly affectedPackagesTruncated: boolean;
  readonly globalChange: boolean;
  readonly warnings: string[];
  readonly elapsedMs: number;
}

export interface RepoContextObservation {
  readonly structured: RepoContextStructured;
  readonly gitAvailable: boolean;
}

interface GitObservation {
  readonly available: boolean;
  readonly branch: string;
  readonly head: string;
  readonly detached: boolean;
  readonly dirty: boolean;
  readonly stagedCount: number;
  readonly unstagedCount: number;
  readonly untrackedCount: number;
  readonly changedPaths: readonly string[];
  readonly mappingPaths: readonly string[];
  readonly totalChangedPaths: number;
  readonly changedPathsTruncated: boolean;
}

interface WorkspaceMember {
  readonly name: string;
  readonly root: string;
}

interface CargoObservation {
  readonly available: boolean;
  readonly members: readonly WorkspaceMember[];
}

interface CargoMetadataPackage {
  readonly id?: unknown;
  readonly name?: unknown;
  readonly manifest_path?: unknown;
}

interface CargoMetadata {
  readonly packages?: unknown;
  readonly workspace_members?: unknown;
}

export function nativeCommandRunner(
  executable: string,
  args: readonly string[],
  options: CommandOptions,
): Promise<CommandResult> {
  return new Promise((resolve) => {
    execFile(
      executable,
      [...args],
      {
        cwd: options.cwd,
        encoding: "utf8",
        maxBuffer: options.maxBuffer,
        shell: false,
        timeout: options.timeoutMs,
        windowsHide: true,
      },
      (error, stdout) => {
        if (error === null) {
          resolve({ ok: true, stdout: String(stdout) });
          return;
        }
        const details = error as {
          readonly code?: unknown;
          readonly killed?: unknown;
          readonly message?: unknown;
        };
        let kind: CommandFailureKind = "failed";
        if (details.code === "ENOENT") kind = "not_found";
        else if (details.killed === true || details.code === "ETIMEDOUT") kind = "timeout";
        else if (
          details.code === "ERR_CHILD_PROCESS_STDIO_MAXBUFFER" ||
          String(details.message ?? "").includes("maxBuffer")
        ) {
          kind = "too_large";
        }
        resolve({ ok: false, kind });
      },
    );
  });
}

function boundedString(value: string, maxChars: number): string | undefined {
  const text = value.trim();
  if (text.length === 0 || text.length > maxChars || Buffer.byteLength(text, "utf8") > maxChars) {
    return undefined;
  }
  return text;
}

function pushWarning(warnings: string[], warning: string): void {
  if (warnings.length >= WARNING_LIMIT) return;
  const flattened = warning.replace(/\s+/gu, " ").trim();
  warnings.push(flattened.slice(0, WARNING_MAX_CHARS));
}

function commandWarning(scope: "git" | "cargo", kind: CommandFailureKind): string {
  switch (kind) {
    case "not_found":
      return `${scope} executable is unavailable`;
    case "timeout":
      return `${scope} observation timed out`;
    case "too_large":
      return `${scope} observation exceeded the local process output bound`;
    case "failed":
      return `${scope} observation failed`;
  }
}

function emptyGitObservation(): GitObservation {
  return {
    available: false,
    branch: "",
    head: "",
    detached: false,
    dirty: false,
    stagedCount: 0,
    unstagedCount: 0,
    untrackedCount: 0,
    changedPaths: [],
    mappingPaths: [],
    totalChangedPaths: 0,
    changedPathsTruncated: false,
  };
}

function parsePorcelainStatus(
  stdout: string,
  warnings: string[],
): Omit<GitObservation, "available" | "branch" | "head" | "detached"> {
  const fields = stdout.split("\0");
  const visiblePaths: string[] = [];
  const mappingPaths: string[] = [];
  let stagedCount = 0;
  let unstagedCount = 0;
  let untrackedCount = 0;
  let totalChangedPaths = 0;

  for (let index = 0; index < fields.length; index += 1) {
    const field = fields[index];
    if (field === undefined || field.length === 0) continue;
    if (field.length < 4 || field[2] !== " ") {
      pushWarning(warnings, "git status returned a malformed porcelain record");
      continue;
    }
    const x = field[0] ?? " ";
    const y = field[1] ?? " ";
    const normalized = field.slice(3).replaceAll("\\", "/");
    const renameOrCopy = x === "R" || x === "C" || y === "R" || y === "C";
    if (renameOrCopy && index + 1 < fields.length) index += 1;

    if (x === "?" && y === "?") {
      untrackedCount += 1;
    } else {
      if (x !== " " && x !== "?") stagedCount += 1;
      if (y !== " " && y !== "?") unstagedCount += 1;
    }

    totalChangedPaths += 1;
    mappingPaths.push(normalized);
    if (visiblePaths.length >= CHANGED_PATH_LIMIT) continue;
    if (normalized.length <= PATH_MAX_CHARS && Buffer.byteLength(normalized, "utf8") <= PATH_MAX_CHARS) {
      visiblePaths.push(normalized);
    } else {
      pushWarning(warnings, "a changed path exceeded the per-string output bound and was omitted");
    }
  }

  return {
    dirty: totalChangedPaths > 0,
    stagedCount,
    unstagedCount,
    untrackedCount,
    changedPaths: visiblePaths,
    mappingPaths,
    totalChangedPaths,
    changedPathsTruncated: totalChangedPaths > visiblePaths.length,
  };
}

async function observeGit(cwd: string, run: CommandRunner, warnings: string[]): Promise<GitObservation> {
  const headResult = await run("git", ["--no-optional-locks", "rev-parse", "--verify", "HEAD"], {
    cwd,
    timeoutMs: GIT_TIMEOUT_MS,
    maxBuffer: GIT_MAX_BUFFER,
  });
  if (!headResult.ok) {
    pushWarning(warnings, commandWarning("git", headResult.kind));
    return emptyGitObservation();
  }
  const head = boundedString(headResult.stdout, HEAD_MAX_CHARS);
  if (head === undefined || !/^[0-9a-f]{40,64}$/u.test(head)) {
    pushWarning(warnings, "git returned a malformed HEAD object id");
    return emptyGitObservation();
  }

  const branchResult = await run("git", ["--no-optional-locks", "rev-parse", "--abbrev-ref", "HEAD"], {
    cwd,
    timeoutMs: GIT_TIMEOUT_MS,
    maxBuffer: GIT_MAX_BUFFER,
  });
  if (!branchResult.ok) {
    pushWarning(warnings, commandWarning("git", branchResult.kind));
    return emptyGitObservation();
  }
  const branchValue = boundedString(branchResult.stdout, BRANCH_MAX_CHARS);
  if (branchValue === undefined) {
    pushWarning(warnings, "git returned a malformed branch name");
    return emptyGitObservation();
  }
  const detached = branchValue === "HEAD";

  const statusResult = await run(
    "git",
    ["--no-optional-locks", "status", "--porcelain=v1", "-z", "--untracked-files=all"],
    { cwd, timeoutMs: GIT_TIMEOUT_MS, maxBuffer: GIT_MAX_BUFFER },
  );
  if (!statusResult.ok) {
    pushWarning(warnings, commandWarning("git", statusResult.kind));
    return emptyGitObservation();
  }

  return {
    available: true,
    branch: detached ? "" : branchValue,
    head,
    detached,
    ...parsePorcelainStatus(statusResult.stdout, warnings),
  };
}

function parseCargoMetadata(cwd: string, stdout: string, warnings: string[]): CargoObservation {
  let parsed: CargoMetadata;
  try {
    parsed = JSON.parse(stdout) as CargoMetadata;
  } catch {
    pushWarning(warnings, "cargo metadata returned malformed JSON");
    return { available: false, members: [] };
  }
  if (!Array.isArray(parsed.packages) || !Array.isArray(parsed.workspace_members)) {
    pushWarning(warnings, "cargo metadata omitted packages or workspace_members");
    return { available: false, members: [] };
  }

  const workspaceIds = new Set(
    parsed.workspace_members.filter((value): value is string => typeof value === "string"),
  );
  const members: WorkspaceMember[] = [];
  for (const value of parsed.packages) {
    if (value === null || typeof value !== "object") continue;
    const pkg = value as CargoMetadataPackage;
    if (typeof pkg.id !== "string" || !workspaceIds.has(pkg.id)) continue;
    if (typeof pkg.name !== "string" || typeof pkg.manifest_path !== "string") {
      pushWarning(warnings, "cargo metadata contained a malformed workspace package");
      continue;
    }
    const name = boundedString(pkg.name, PACKAGE_NAME_MAX_CHARS);
    if (name === undefined) {
      pushWarning(warnings, "a Cargo package name exceeded the per-string bound and was omitted");
      continue;
    }
    const relative = path.relative(cwd, path.dirname(pkg.manifest_path));
    if (relative.startsWith("..") || path.isAbsolute(relative)) {
      pushWarning(warnings, `workspace package ${name} is outside provider cwd and was omitted`);
      continue;
    }
    members.push({
      name,
      root: relative === "" ? "." : relative.replaceAll(path.sep, "/"),
    });
  }
  members.sort((left, right) => left.name.localeCompare(right.name) || left.root.localeCompare(right.root));
  if (members.length !== workspaceIds.size) {
    pushWarning(warnings, "some Cargo workspace members could not be projected inside provider cwd");
  }
  return { available: true, members };
}

async function observeCargo(cwd: string, run: CommandRunner, warnings: string[]): Promise<CargoObservation> {
  if (!fs.existsSync(path.join(cwd, "Cargo.toml"))) {
    pushWarning(warnings, "provider cwd has no root Cargo.toml; Cargo workspace context is unavailable");
    return { available: false, members: [] };
  }
  const result = await run(
    "cargo",
    ["metadata", "--offline", "--no-deps", "--format-version", "1"],
    { cwd, timeoutMs: CARGO_TIMEOUT_MS, maxBuffer: CARGO_MAX_BUFFER },
  );
  if (!result.ok) {
    pushWarning(warnings, commandWarning("cargo", result.kind));
    return { available: false, members: [] };
  }
  return parseCargoMetadata(cwd, result.stdout, warnings);
}

function isSharedWorkspacePath(changedPath: string): boolean {
  return !changedPath.includes("/") || changedPath.startsWith(".cargo/");
}

function mapAffectedPackages(
  changedPaths: readonly string[],
  members: readonly WorkspaceMember[],
  warnings: string[],
): { packages: string[]; globalChange: boolean } {
  if (members.length === 0 || changedPaths.length === 0) {
    return { packages: [], globalChange: false };
  }
  const affected = new Set<string>();
  const rootMember = members.find((member) => member.root === ".");
  const byDepth = members
    .filter((member) => member.root !== ".")
    .sort((left, right) => right.root.split("/").length - left.root.split("/").length);
  let sharedWarningAdded = false;
  let globalChange = false;

  for (const changedPath of changedPaths) {
    if (isSharedWorkspacePath(changedPath)) {
      globalChange = true;
      for (const member of members) affected.add(member.name);
      if (!sharedWarningAdded) {
        pushWarning(
          warnings,
          "root/shared changes conservatively mark all observed workspace packages affected",
        );
        sharedWarningAdded = true;
      }
      continue;
    }
    const member = byDepth.find(
      (candidate) =>
        changedPath === candidate.root || changedPath.startsWith(`${candidate.root}/`),
    );
    if (member !== undefined) {
      affected.add(member.name);
      continue;
    }
    if (
      rootMember !== undefined &&
      ["src/", "tests/", "benches/", "examples/"].some((prefix) => changedPath.startsWith(prefix))
    ) {
      affected.add(rootMember.name);
    }
  }
  return { packages: [...affected].sort(), globalChange };
}

export async function observeRepoContext(
  cwd: string,
  run: CommandRunner = nativeCommandRunner,
): Promise<RepoContextObservation> {
  const started = Date.now();
  const warnings: string[] = [];
  const git = await observeGit(cwd, run, warnings);
  const cargo = await observeCargo(cwd, run, warnings);
  const affected = mapAffectedPackages(git.mappingPaths, cargo.members, warnings);
  const visibleMembers = cargo.members.slice(0, WORKSPACE_MEMBER_LIMIT);
  const visibleAffected = affected.packages.slice(0, AFFECTED_PACKAGE_LIMIT);

  return {
    gitAvailable: git.available,
    structured: {
      branch: git.branch,
      head: git.head,
      detached: git.detached,
      dirty: git.dirty,
      stagedCount: git.stagedCount,
      unstagedCount: git.unstagedCount,
      untrackedCount: git.untrackedCount,
      changedPaths: [...git.changedPaths],
      totalChangedPaths: git.totalChangedPaths,
      changedPathsTruncated: git.changedPathsTruncated,
      cargoAvailable: cargo.available,
      workspaceMemberCount: cargo.members.length,
      workspaceMembers: visibleMembers.map((member) => member.name),
      workspaceMembersTruncated: cargo.members.length > visibleMembers.length,
      affectedPackages: [...visibleAffected],
      affectedPackagesTruncated: affected.packages.length > visibleAffected.length,
      globalChange: affected.globalChange,
      warnings: [...warnings],
      elapsedMs: Math.max(0, Date.now() - started),
    },
  };
}
