import {
  definePlugin,
  defineTool,
  errorResult,
  runPlugin,
  schema,
  textResult,
} from "@yyjeqhc/webcodex-plugin-sdk";

import {
  AFFECTED_PACKAGE_LIMIT,
  CHANGED_PATH_LIMIT,
  PACKAGE_NAME_MAX_CHARS,
  PATH_MAX_CHARS,
  WARNING_LIMIT,
  WARNING_MAX_CHARS,
  WORKSPACE_MEMBER_LIMIT,
  observeRepoContext,
} from "./context.js";

const repoContextTool = defineTool({
  name: "repo_context",
  title: "Repository context",
  description:
    "Observe only this Plugin provider's configured cwd and return compact bounded Git status plus advisory Cargo workspace/package context. The tool accepts no path or repository input, performs local read-only Git/Cargo process calls without a shell or network access, and does not return raw cargo metadata, Git diffs, validation verdicts, or absolute provider paths.",
  inputSchema: schema.object({}),
  outputSchema: schema.object({
    branch: schema.string({ maxLength: 512 }),
    head: schema.string({ maxLength: 64 }),
    detached: schema.boolean(),
    dirty: schema.boolean(),
    stagedCount: schema.integer(),
    unstagedCount: schema.integer(),
    untrackedCount: schema.integer(),
    changedPaths: schema.array(schema.string({ maxLength: PATH_MAX_CHARS }), {
      maxItems: CHANGED_PATH_LIMIT,
    }),
    totalChangedPaths: schema.integer(),
    changedPathsTruncated: schema.boolean(),
    cargoAvailable: schema.boolean(),
    workspaceMemberCount: schema.integer(),
    workspaceMembers: schema.array(schema.string({ maxLength: PACKAGE_NAME_MAX_CHARS }), {
      maxItems: WORKSPACE_MEMBER_LIMIT,
    }),
    workspaceMembersTruncated: schema.boolean(),
    affectedPackages: schema.array(schema.string({ maxLength: PACKAGE_NAME_MAX_CHARS }), {
      maxItems: AFFECTED_PACKAGE_LIMIT,
    }),
    affectedPackagesTruncated: schema.boolean(),
    warnings: schema.array(schema.string({ maxLength: WARNING_MAX_CHARS }), {
      maxItems: WARNING_LIMIT,
    }),
    elapsedMs: schema.integer(),
  }),
  annotations: {
    readOnlyHint: true,
    destructiveHint: false,
    idempotentHint: true,
    openWorldHint: false,
  },
  async execute() {
    const observation = await observeRepoContext(process.cwd());
    const value = observation.structured;
    if (!observation.gitAvailable) {
      return errorResult("Git context is unavailable for the configured provider cwd.", value);
    }
    const location = value.detached ? "detached HEAD" : `branch ${value.branch}`;
    const affected = value.affectedPackages.length === 0 ? "none" : value.affectedPackages.join(", ");
    return textResult(
      `Repository ${location} at ${value.head.slice(0, 12)}; ${value.totalChangedPaths} changed path(s), ${value.workspaceMemberCount} Cargo workspace member(s), advisory affected packages: ${affected}.`,
      value,
    );
  },
});

runPlugin(definePlugin({ tools: [repoContextTool] }));
