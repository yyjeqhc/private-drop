#!/usr/bin/env node
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { runBuild } from "./build.mjs";

const outputDirectory = fileURLToPath(new URL("../.dev-dist", import.meta.url));
runBuild({ outputDirectory });
// Serve only these two public artifacts. Never expose the repository or APIs.
const assets = new Map([
  ["/", ["demo.html", "text/html; charset=utf-8"]],
  ["/demo", ["demo.html", "text/html; charset=utf-8"]],
  ["/webcodex-logo.png", ["webcodex-logo.png", "image/png"]],
]);
const port = Number(process.env.WEBCODEX_DEMO_PORT || 4173);
if (!Number.isInteger(port) || port < 1 || port > 65535)
  throw new Error("Invalid WEBCODEX_DEMO_PORT");
const server = createServer((request, response) => {
  response.setHeader("Cache-Control", "no-store");
  if (request.method !== "GET" && request.method !== "HEAD") {
    response.writeHead(405, { Allow: "GET, HEAD" }).end();
    return;
  }
  let pathname;
  try {
    pathname = new URL(request.url, "http://localhost").pathname;
  } catch {
    response.writeHead(400).end("Invalid URL");
    return;
  }
  const asset = assets.get(pathname);
  if (!asset) {
    response.writeHead(404).end("Not found");
    return;
  }
  let body;
  try {
    body = readFileSync(`${outputDirectory}/${asset[0]}`);
  } catch {
    response.writeHead(503).end("Demo asset unavailable");
    return;
  }
  response.writeHead(200, {
    "Content-Type": asset[1],
    "Content-Length": body.length,
    "X-Content-Type-Options": "nosniff",
  });
  response.end(request.method === "HEAD" ? undefined : body);
});
server.requestTimeout = 10_000;
server.headersTimeout = 5_000;
server.keepAliveTimeout = 1_000;
server.on("error", (error) => {
  console.error(error.message);
  process.exitCode = 1;
});
server.listen(port, "127.0.0.1", () =>
  console.log(
    `WebCodex demo: http://127.0.0.1:${port}/demo (fictional data only)`,
  ),
);
for (const signal of ["SIGINT", "SIGTERM"])
  process.once(signal, () => server.close());
