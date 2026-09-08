import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { Script } from "node:vm";
import { createOutputs } from "../scripts/build.mjs";

test("demo is self-contained and excludes production clients and browser credentials", () => {
  const html = createOutputs("unused").get("demo.html");
  const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)];
  assert.equal(scripts.length, 1);
  const code = scripts[0][1];
  assert.doesNotThrow(() => new Script(code));
  assert.doesNotMatch(
    code,
    /\b(?:fetch|XMLHttpRequest|WebSocket|EventSource|sessionStorage|localStorage)\b|document\.cookie|\.innerHTML\b/,
  );
  assert.doesNotMatch(
    html,
    /<script[^>]+src=|WEBCODEX_DEMO_(?:SCRIPT|STYLES)|\/api\/runtime-console\//,
  );
  assert.match(html, /所有项目、设备、任务与日志均为虚构示例/);
});

test("build copies the selected logo byte for byte and all consoles reference it", async () => {
  const outputs = createOutputs("unused");
  const source = await readFile(
    new URL("../src/webcodex-logo.png", import.meta.url),
  );
  assert.deepEqual(outputs.get("webcodex-logo.png"), source);
  for (const name of [
    "runtime.html",
    "console.html",
    "admin.html",
    "demo.html",
  ]) {
    const html = outputs.get(name);
    assert.match(
      html,
      /<link\s+rel="icon"\s+type="image\/png"\s+href="\/?webcodex-logo\.png"/,
    );
    assert.match(html, /<img[^>]+src="\/?webcodex-logo\.png"/);
  }
});
