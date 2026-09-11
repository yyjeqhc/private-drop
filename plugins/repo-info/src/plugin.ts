import { execFile } from "node:child_process";

import {
  definePlugin,
  defineTool,
  errorResult,
  runPlugin,
  schema,
  textResult,
} from "@yyjeqhc/webcodex-plugin-sdk";
import type { ToolResult } from "@yyjeqhc/webcodex-plugin-sdk";

const GIT_TIMEOUT_MS = 5_000;
const STATUS_MAX_BYTES = 32 * 1024;
const GIT_MAX_BUFFER = STATUS_MAX_BYTES + 4 * 1024;
const BRANCH_MAX_CHARS = 512;
const HEAD_MAX_CHARS = 64;

const ERROR_CODES = [
  "",
  "not_git_repository",
  "git_not_found",
  "git_timeout",
  "git_output_too_large",
  "git_failed",
] as const;

type GitErrorCode = (typeof ERROR_CODES)[number];

interface GitSummaryStructured {
  readonly branch: string;
  readonly head: string;
  readonly detached: boolean;
  readonly dirty: boolean;
  readonly status: string;
  readonly errorCode: GitErrorCode;
}

type GitRunResult =
  | { readonly ok: true; readonly stdout: string }
  | { readonly ok: false; readonly errorCode: Exclude<GitErrorCode, ""> };

function emptySummary(errorCode: Exclude<GitErrorCode, "">): GitSummaryStructured {
  return {
    branch: "",
    head: "",
    detached: false,
    dirty: false,
    status: "",
    errorCode,
  };
}

function classifyGitError(error: unknown, stderr: string): Exclude<GitErrorCode, ""> {
  const details = error as {
    readonly code?: unknown;
    readonly killed?: unknown;
    readonly message?: unknown;
  };
  if (details.code === "ENOENT") return "git_not_found";
  if (details.killed === true || details.code === "ETIMEDOUT") return "git_timeout";
  if (
    details.code === "ERR_CHILD_PROCESS_STDIO_MAXBUFFER" ||
    String(details.message ?? "").includes("maxBuffer")
  ) {
    return "git_output_too_large";
  }
  if (/not a git repository/iu.test(stderr)) return "not_git_repository";
  return "git_failed";
}

function runGit(args: readonly string[]): Promise<GitRunResult> {
  return new Promise((resolve) => {
    execFile(
      "git",
      ["--no-optional-locks", ...args],
      {
        cwd: process.cwd(),
        encoding: "utf8",
        maxBuffer: GIT_MAX_BUFFER,
        shell: false,
        timeout: GIT_TIMEOUT_MS,
        windowsHide: true,
      },
      (error, stdout, stderr) => {
        if (error !== null) {
          resolve({ ok: false, errorCode: classifyGitError(error, String(stderr)) });
          return;
        }
        resolve({ ok: true, stdout: String(stdout) });
      },
    );
  });
}

function failureResult(
  errorCode: Exclude<GitErrorCode, "">,
): ToolResult<GitSummaryStructured, true> {
  const messages: Record<Exclude<GitErrorCode, "">, string> = {
    not_git_repository: "The configured repo-info provider cwd is not a Git repository.",
    git_not_found: "Git is not available to the repo-info Plugin.",
    git_timeout: "Git observation timed out before a complete repository summary was available.",
    git_output_too_large:
      "Git observation exceeded the repo-info Plugin output bound; no partial repository state was returned.",
    git_failed: "Git could not produce a complete repository summary.",
  };
  return errorResult(messages[errorCode], emptySummary(errorCode));
}

function boundedText(value: string, maxChars: number): string | undefined {
  const text = value.trim();
  if (text.length > maxChars || Buffer.byteLength(text, "utf8") > maxChars) return undefined;
  return text;
}

async function gitSummary(): Promise<ToolResult<GitSummaryStructured>> {
  const headResult = await runGit(["rev-parse", "--verify", "HEAD"]);
  if (!headResult.ok) return failureResult(headResult.errorCode);
  const head = boundedText(headResult.stdout, HEAD_MAX_CHARS);
  if (head === undefined || !/^[0-9a-f]{40,64}$/u.test(head)) {
    return failureResult("git_failed");
  }

  const branchResult = await runGit(["rev-parse", "--abbrev-ref", "HEAD"]);
  if (!branchResult.ok) return failureResult(branchResult.errorCode);
  const branchValue = boundedText(branchResult.stdout, BRANCH_MAX_CHARS);
  if (branchValue === undefined) return failureResult("git_output_too_large");
  const detached = branchValue === "HEAD";
  const branch = detached ? "" : branchValue;

  const statusResult = await runGit(["status", "--porcelain=v1", "--branch"]);
  if (!statusResult.ok) return failureResult(statusResult.errorCode);
  const status = statusResult.stdout.replace(/\r\n/gu, "\n").replace(/\n$/u, "");
  if (status.length > STATUS_MAX_BYTES || Buffer.byteLength(status, "utf8") > STATUS_MAX_BYTES) {
    return failureResult("git_output_too_large");
  }
  const dirty = status
    .split("\n")
    .some((line) => line.length > 0 && !line.startsWith("## "));

  const structured: GitSummaryStructured = {
    branch,
    head,
    detached,
    dirty,
    status,
    errorCode: "",
  };
  const location = detached ? "detached HEAD" : `branch ${branch}`;
  const state = dirty ? "dirty" : "clean";
  return textResult(`Git repository at ${location}, ${head.slice(0, 12)}; working tree ${state}.`, structured);
}

const gitSummaryTool = defineTool({
  name: "git_summary",
  title: "Git summary",
  description:
    "Observe only this repo-info Plugin provider's configured cwd and return a bounded Git branch, HEAD, and porcelain status summary. This tool accepts no repository path, does not discover another project or Runner, does not fetch or use the network, and does not modify the repository.",
  inputSchema: schema.object({}),
  outputSchema: schema.object({
    branch: schema.string({ maxLength: BRANCH_MAX_CHARS }),
    head: schema.string({ maxLength: HEAD_MAX_CHARS }),
    detached: schema.boolean(),
    dirty: schema.boolean(),
    status: schema.string({ maxLength: STATUS_MAX_BYTES }),
    errorCode: schema.string({ enum: ERROR_CODES, maxLength: 64 }),
  }),
  annotations: {
    readOnlyHint: true,
    destructiveHint: false,
    idempotentHint: true,
    openWorldHint: false,
  },
  execute() {
    return gitSummary();
  },
});

runPlugin(definePlugin({ tools: [gitSummaryTool] }));
