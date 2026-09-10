import {
  definePlugin,
  defineTool,
  runPlugin,
  schema,
} from "@yyjeqhc/webcodex-plugin-sdk";

import { MAX_PATH_CHARS, safeDelete } from "./domain.js";

const inputSchema = schema.object({
  path: schema.string({
    minLength: 1,
    maxLength: MAX_PATH_CHARS,
    description:
      "One file or directory path relative to the Plugin provider cwd. Absolute paths and parent traversal are rejected.",
  }),
});

const outputSchema = schema.object({
  outcome: schema.string({
    enum: ["trashed", "already_absent", "rejected", "failed", "unknown"],
  }),
  path: schema.string({ maxLength: MAX_PATH_CHARS }),
  backend: schema.string({
    enum: ["none", "freedesktop", "gio", "trash-put", "foundation", "powershell"],
  }),
  errorCode: schema.string({ maxLength: 128 }),
});

const safeDeleteTool = defineTool({
  name: "safe_delete",
  title: "Safe delete",
  description:
    "Move exactly one ordinary file or directory under this Plugin provider's configured cwd to the operating system Trash/Recycle Bin. Use this instead of permanent deletion when recovery may be needed. The path must be relative to the provider cwd. The provider cwd itself, paths that escape it, symlinks/junctions, and unsupported file types are rejected. This tool never permanently deletes the requested path through rm, unlink, Remove-Item, or another fallback.",
  inputSchema,
  outputSchema,
  annotations: {
    readOnlyHint: false,
    destructiveHint: true,
    idempotentHint: false,
    openWorldHint: true,
  },
  execute(args) {
    return safeDelete(args);
  },
});

runPlugin(definePlugin({ tools: [safeDeleteTool] }));
