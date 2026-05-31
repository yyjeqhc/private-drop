#!/usr/bin/env node
import { readFileSync, writeFileSync, mkdirSync, existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const checkOnly = process.argv.includes("--check");

function read(relPath) {
  return readFileSync(resolve(root, relPath), "utf8");
}

function write(relPath, content) {
  const fullPath = resolve(root, relPath);
  mkdirSync(dirname(fullPath), { recursive: true });
  writeFileSync(fullPath, content);
}

function normalizeNewline(content) {
  return content.replace(/\r\n/g, "\n").trim() + "\n";
}

function stripTypeScript(source) {
  let js = source;
  js = js.replace(/^type\s+RequestOptions\s*=.*?;\n\n/s, "");
  js = js.replace(/^declare\s+global\s*\{[\s\S]*?^\}\n\n/m, "");
  js = js.replace(/^export\s*\{\};\s*\n?/gm, "");
  js = js.replace(/: RequestOptions(?=\s*[=,)])/g, "");
  js = js.replace(/: string(?=\s*[=,)])/g, "");
  js = js.replace(/: number(?=\s*[=,)])/g, "");
  js = js.replace(/: unknown(?=\s*[=,)])/g, "");
  js = js.replace(/: Promise<Response \| null>(?=\s*\{)/g, "");
  js = js.replace(/: Promise<void>(?=\s*\{)/g, "");
  js = js.replace(/: boolean(?=\s*\{)/g, "");
  js = js.replace(/: string(?=\s*\{)/g, "");
  js = js.replace(/: void(?=\s*\{)/g, "");
  return js;
}

function buildJs(source) {
  // Keep generated JS readable and avoid whitespace-sensitive rewrites inside
  // template literals. CSS is safe to minify below; JS only needs deterministic
  // TypeScript stripping for the current no-bundler frontend.
  return normalizeNewline(source);
}

function minifyCss(source) {
  return normalizeNewline(source)
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\s+/g, " ")
    .replace(/\s*([{}:;,>])\s*/g, "$1")
    .replace(/;}/g, "}")
    .replace(/0\.([0-9]+)/g, ".$1")
    .trim() + "\n";
}

const outputs = new Map([
  ["dist/app.js", buildJs(stripTypeScript(read("src/app.ts")))],
  ["dist/styles.css", minifyCss(read("src/styles.css"))],
]);

let drift = false;
for (const [relPath, expected] of outputs) {
  const fullPath = resolve(root, relPath);
  if (checkOnly) {
    const actual = existsSync(fullPath) ? readFileSync(fullPath, "utf8") : "";
    if (actual !== expected) {
      console.error(`${relPath} is out of date. Run: npm --prefix frontend run build`);
      drift = true;
    }
  } else {
    write(relPath, expected);
    console.log(`wrote ${relPath}`);
  }
}

if (drift) process.exit(1);
