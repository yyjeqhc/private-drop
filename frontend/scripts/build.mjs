#!/usr/bin/env node
import {
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  watch,
  writeFileSync,
} from "node:fs";
import { basename, dirname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Script } from "node:vm";
import ts from "typescript";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const watchedSources = new Set([
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
  "demo.ts",
  "demo.css",
  "demo.html",
  "webcodex-logo.png",
]);

function readSource(sourceDirectory, fileName) {
  return readFileSync(resolve(sourceDirectory, fileName), "utf8");
}

function normalizeNewline(content) {
  return content.replace(/\r\n/g, "\n").trim() + "\n";
}

const diagnosticHost = {
  getCanonicalFileName: (fileName) => fileName,
  getCurrentDirectory: () => root,
  getNewLine: () => "\n",
};

function transpileTypeScript(sourceDirectory, fileName) {
  const sourcePath = resolve(sourceDirectory, fileName);
  const result = ts.transpileModule(readSource(sourceDirectory, fileName), {
    compilerOptions: {
      target: ts.ScriptTarget.ES2020,
      module: ts.ModuleKind.ES2020,
      newLine: ts.NewLineKind.LineFeed,
      removeComments: false,
      sourceMap: false,
      inlineSourceMap: false,
    },
    fileName: sourcePath,
    reportDiagnostics: true,
  });
  const errors = (result.diagnostics || []).filter(
    (diagnostic) => diagnostic.category === ts.DiagnosticCategory.Error
  );
  if (errors.length) {
    throw new Error(ts.formatDiagnostics(errors, diagnosticHost).trim());
  }
  return normalizeNewline(result.outputText);
}

function buildJs(source) {
  // Keep generated JS readable and avoid whitespace-sensitive rewrites inside
  // template literals. TypeScript owns syntax erasure.
  return normalizeNewline(source);
}

function assertClassicScript(label, source) {
  try {
    new Script(source, { filename: label });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`${label} is not valid browser JavaScript: ${message}`);
  }
}

function minifyCss(source) {
  return (
    normalizeNewline(source)
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .replace(/\s+/g, " ")
      .replace(/\s*([{}:;,>])\s*/g, "$1")
      .replace(/;}/g, "}")
      .replace(/0\.([0-9]+)/g, ".$1")
      .trim() + "\n"
  );
}

// Turn an ESM module into classic-script statements for inlining: drop the
// `export {}` marker and the `export` keyword on top-level declarations.
function stripModuleExports(js) {
  return js
    .replace(/^export\s*\{\};\s*\n?/gm, "")
    .replace(/^export\s+(function|const|let|class)\b/gm, "$1");
}

export function createOutputs(
  outputDirectory,
  sourceDirectory = resolve(root, "src")
) {
  const reviewStateModule = buildJs(
    transpileTypeScript(sourceDirectory, "review_state.ts")
  );
  const reviewStateClassic = stripModuleExports(reviewStateModule);
  const workflowSessionStateModule = buildJs(
    transpileTypeScript(sourceDirectory, "workflow_session_state.ts")
  );
  const workflowSessionStateClassic = stripModuleExports(workflowSessionStateModule);
  const runtimeConsoleStateModule = buildJs(
    transpileTypeScript(sourceDirectory, "runtime_console_state.ts")
  );
  const runtimeConsoleStateClassic = stripModuleExports(
    runtimeConsoleStateModule.replace(
      /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/workflow_session_state(?:\.js)?["'];?\s*\n/m,
      ""
    )
  );
  const appModule = transpileTypeScript(sourceDirectory, "app.ts");
  const appScript = stripModuleExports(
    appModule
      .replace(
        /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/review_state(?:\.js)?["'];?\s*\n/m,
        ""
      )
      .replace(
        /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/workflow_session_state(?:\.js)?["'];?\s*\n/m,
        ""
      )
  );
  const appInlined = buildJs(
    reviewStateClassic + "\n" + workflowSessionStateClassic + "\n" + appScript
  );
  assertClassicScript(resolve(outputDirectory, "app.js"), appInlined);
  const runtimeModule = transpileTypeScript(sourceDirectory, "runtime.ts");
  const runtimeScript = stripModuleExports(
    runtimeModule
      .replace(
        /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/workflow_session_state(?:\.js)?["'];?\s*\n/m,
        ""
      )
      .replace(
        /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/runtime_console_state(?:\.js)?["'];?\s*\n/m,
        ""
      )
  );
  const runtimeInlined = buildJs(
    workflowSessionStateClassic + "\n" + runtimeConsoleStateClassic + "\n" + runtimeScript
  );
  assertClassicScript(resolve(outputDirectory, "runtime.js"), runtimeInlined);
  const adminControllerModule = buildJs(
    transpileTypeScript(sourceDirectory, "admin_controller.ts")
  );
  const adminControllerClassic = stripModuleExports(adminControllerModule);
  const adminMutationControllerModule = buildJs(
    transpileTypeScript(sourceDirectory, "admin_mutation_controller.ts")
  );
  const adminMutationControllerClassic = stripModuleExports(adminMutationControllerModule);
  const adminMutationViewModule = buildJs(
    transpileTypeScript(sourceDirectory, "admin_mutation_view.ts")
  );
  const adminMutationViewClassic = stripModuleExports(adminMutationViewModule);
  const adminViewModule = buildJs(
    transpileTypeScript(sourceDirectory, "admin_view.ts")
  );
  const adminViewClassic = stripModuleExports(adminViewModule);
  const adminModule = transpileTypeScript(sourceDirectory, "admin.ts");
  const adminScript = buildJs(
    adminControllerClassic +
      "\n" +
      adminMutationControllerClassic +
      "\n" +
      adminMutationViewClassic +
      "\n" +
      adminViewClassic +
      "\n" +
      stripModuleExports(
        adminModule
          .replace(
            /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/admin_controller(?:\.js)?["'];?\s*\n/m,
            ""
          )
          .replace(
            /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/admin_mutation_controller(?:\.js)?["'];?\s*\n/m,
            ""
          )
          .replace(
            /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/admin_mutation_view(?:\.js)?["'];?\s*\n/m,
            ""
          )
          .replace(
            /^import\s*\{[\s\S]*?\}\s*from\s*["']\.\/admin_view(?:\.js)?["'];?\s*\n/m,
            ""
          )
      )
  );
  assertClassicScript(resolve(outputDirectory, "admin.js"), adminScript);
  const demoScript = stripModuleExports(transpileTypeScript(sourceDirectory, "demo.ts"));
  assertClassicScript(resolve(outputDirectory, "demo.js"), demoScript);
  const demoHtml = readSource(sourceDirectory, "demo.html")
    .replace("/* WEBCODEX_DEMO_STYLES */", () => minifyCss(readSource(sourceDirectory, "demo.css")))
    .replace("/* WEBCODEX_DEMO_SCRIPT */", () => demoScript);

  return new Map([
    ["demo.html", normalizeNewline(demoHtml)],
    ["webcodex-logo.png", readFileSync(resolve(sourceDirectory, "webcodex-logo.png"))],
    ["review_state.js", reviewStateModule],
    ["workflow_session_state.js", workflowSessionStateModule],
    ["runtime_console_state.js", runtimeConsoleStateModule],
    ["admin_controller.js", adminControllerModule],
    ["admin_mutation_controller.js", adminMutationControllerModule],
    ["admin_mutation_view.js", adminMutationViewModule],
    ["admin_view.js", adminViewModule],
    ["app.js", appInlined],
    ["styles.css", minifyCss(readSource(sourceDirectory, "styles.css"))],
    ["runtime.js", runtimeInlined],
    ["runtime.css", minifyCss(readSource(sourceDirectory, "runtime.css"))],
    ["admin.js", adminScript],
    ["admin.css", minifyCss(readSource(sourceDirectory, "admin.css"))],
    ["console.html", normalizeNewline(readSource(sourceDirectory, "console.html"))],
    ["runtime.html", normalizeNewline(readSource(sourceDirectory, "runtime.html"))],
    ["admin.html", normalizeNewline(readSource(sourceDirectory, "admin.html"))],
  ]);
}

function atomicWriteOutputs(outputDirectory, outputs) {
  mkdirSync(outputDirectory, { recursive: true });
  const nonce = `${process.pid}-${Date.now()}`;
  const staged = [];
  try {
    let index = 0;
    for (const [name, content] of outputs) {
      const finalPath = resolve(outputDirectory, name);
      const temporaryPath = resolve(
        outputDirectory,
        `.${basename(name)}.${nonce}-${index}.tmp`
      );
      index += 1;
      writeFileSync(temporaryPath, content);
      staged.push({ finalPath, temporaryPath });
    }
    for (const entry of staged) {
      renameSync(entry.temporaryPath, entry.finalPath);
    }
  } finally {
    for (const entry of staged) {
      rmSync(entry.temporaryPath, { force: true });
    }
  }
}

function checkOutputs(outputDirectory, outputs) {
  const drift = [];
  for (const [name, expected] of outputs) {
    const fullPath = resolve(outputDirectory, name);
    const actual = existsSync(fullPath) ? readFileSync(fullPath) : Buffer.alloc(0);
    if (!actual.equals(Buffer.from(expected))) drift.push(name);
  }
  if (drift.length) {
    throw new Error(
      `${drift.join(", ")} out of date; run: npm --prefix frontend run build`
    );
  }
}

export function runBuild({
  outputDirectory,
  sourceDirectory = resolve(root, "src"),
  checkOnly = false,
}) {
  const startedAt = Date.now();
  // Generate and validate every output before touching any final file. A
  // TypeScript or JS parse failure therefore preserves the previous build.
  const outputs = createOutputs(outputDirectory, sourceDirectory);
  if (checkOnly) {
    checkOutputs(outputDirectory, outputs);
  } else {
    atomicWriteOutputs(outputDirectory, outputs);
  }
  const displayDirectory =
    relative(root, outputDirectory) || basename(outputDirectory);
  console.log(
    `[console] ${checkOnly ? "checked" : "built"} ${displayDirectory} (${
      outputs.size
    } files, ${Date.now() - startedAt}ms)`
  );
}

function parseArguments(argv) {
  let outputDirectory = resolve(root, "dist");
  let sourceDirectory = resolve(root, "src");
  let checkOnly = false;
  let watchMode = false;
  for (let index = 0; index < argv.length; index += 1) {
    switch (argv[index]) {
      case "--check":
        checkOnly = true;
        break;
      case "--watch":
        watchMode = true;
        break;
      case "--out-dir": {
        const value = argv[index + 1];
        if (!value || value.startsWith("--")) {
          throw new Error("--out-dir requires a path");
        }
        outputDirectory = resolve(root, value);
        index += 1;
        break;
      }
      case "--source-dir": {
        const value = argv[index + 1];
        if (!value || value.startsWith("--")) {
          throw new Error("--source-dir requires a path");
        }
        sourceDirectory = resolve(root, value);
        index += 1;
        break;
      }
      default:
        throw new Error(`unknown option: ${argv[index]}`);
    }
  }
  if (checkOnly && watchMode) {
    throw new Error("--check and --watch cannot be used together");
  }
  return { outputDirectory, sourceDirectory, checkOnly, watchMode };
}

function startWatcher(outputDirectory, sourceDirectory) {
  let debounceTimer;
  let closed = false;

  const rebuild = () => {
    debounceTimer = undefined;
    try {
      runBuild({ outputDirectory, sourceDirectory });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      console.error(`[console] build failed: ${message}`);
    }
  };
  const schedule = () => {
    if (closed) return;
    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(rebuild, 100);
  };
  const sourceWatcher = watch(sourceDirectory, (_event, fileName) => {
    const name = fileName === null ? null : fileName.toString();
    if (name === null || watchedSources.has(name)) schedule();
  });
  const displaySourceDirectory =
    relative(root, sourceDirectory) || basename(sourceDirectory);
  const watched = [...watchedSources]
    .sort()
    .map((name) => `${displaySourceDirectory}/${name}`)
    .join(", ");
  console.log(`[console] watching ${watched}`);

  const close = () => {
    if (closed) return;
    closed = true;
    if (debounceTimer) clearTimeout(debounceTimer);
    sourceWatcher.close();
  };
  process.once("SIGINT", close);
  process.once("SIGTERM", close);
}

function main() {
  const options = parseArguments(process.argv.slice(2));
  try {
    runBuild(options);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (!options.watchMode) throw error;
    console.error(`[console] initial build failed: ${message}`);
  }
  if (options.watchMode) {
    startWatcher(options.outputDirectory, options.sourceDirectory);
  }
}

const invokedPath = process.argv[1] ? resolve(process.argv[1]) : "";
if (invokedPath === fileURLToPath(import.meta.url)) {
  try {
    main();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    console.error(`[console] build failed: ${message}`);
    process.exitCode = 1;
  }
}
