"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");
const { fetchToFile } = require("../install");

async function checkFailureCleanup(kind, expectedError) {
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "webcodex-download-cleanup-"));
  const destination = path.join(tempDir, "partial.bin");
  const originalCreateWriteStream = fs.createWriteStream;
  const sockets = new Set();
  let response;
  let stream;
  let streamClosed = Promise.resolve();
  let releaseClose;
  let holdClose = true;
  let settled = false;
  let outcome;
  let signalCloseAttempt;
  const closeAttempted = new Promise((resolve) => { signalCloseAttempt = resolve; });
  let timer;
  const deadline = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error("Download cleanup fixture timed out")), 5000);
  });

  const server = http.createServer((_req, res) => {
    response = res;
    // Do not send Content-Length: the byte-limit failure must happen after
    // the installer creates its output stream, not during header validation.
    res.writeHead(200, { "Transfer-Encoding": "chunked" });
    if (kind === "oversized") res.end("too large");
    else res.write("partial");
  });
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
  });

  try {
    await Promise.race([
      new Promise((resolve, reject) => {
        server.once("error", reject);
        server.listen(0, "127.0.0.1", resolve);
      }),
      deadline
    ]);

    // Keep the real WriteStream and filesystem operations. Gate only the
    // close callback so this race is deterministic on every supported OS.
    // Tests in this file run serially; restore the shared method in finally.
    fs.createWriteStream = (file, options) => {
      stream = originalCreateWriteStream(file, {
        ...options,
        fs: {
          open: fs.open,
          write: fs.write,
          writev: fs.writev,
          close(fd, callback) {
            if (!holdClose) return fs.close(fd, callback);
            releaseClose = () => {
              releaseClose = null;
              fs.close(fd, callback);
            };
            signalCloseAttempt();
          }
        }
      });
      streamClosed = new Promise((resolve) => stream.once("close", resolve));
      if (kind === "aborted") stream.once("open", () => response.destroy());
      return stream;
    };

    outcome = fetchToFile(`http://127.0.0.1:${server.address().port}/artifact`, destination, {
      label: "Artifact",
      maxBytes: kind === "oversized" ? 1 : 1024,
      firstByteTimeoutMs: 2000,
      inactivityTimeoutMs: 2000,
      totalTimeoutMs: 3000,
      environment: {}
    }).then(
      () => { settled = true; return null; },
      (error) => { settled = true; return error; }
    );

    await Promise.race([closeAttempted, deadline]);
    assert.equal(stream.closed, false);
    assert.equal(settled, false, "download must not reject while its output file is still open");
    assert.equal(fs.existsSync(destination), true, "partial output must remain until close finishes");

    releaseClose();
    const error = await Promise.race([outcome, deadline]);
    assert.ok(error instanceof Error, "the original download failure must still be reported");
    assert.match(error.message, expectedError);
    assert.equal(stream.closed, true);
    assert.equal(fs.existsSync(destination), false, "failure must remove the closed partial output");
    // Match the installer's caller: immediate synchronous directory cleanup
    // must be safe once the download promise rejects, including on Windows.
    fs.rmSync(tempDir, { recursive: true, force: true });
  } finally {
    fs.createWriteStream = originalCreateWriteStream;
    holdClose = false;
    if (releaseClose) releaseClose();
    if (stream) stream.destroy();
    await streamClosed;
    for (const socket of sockets) socket.destroy();
    await new Promise((resolve) => server.close(resolve));
    if (outcome) await Promise.race([outcome, deadline]).catch(() => {});
    clearTimeout(timer);
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
}

test("chunked byte-limit failure waits for the output file to close", { concurrency: false }, async () => {
  await checkFailureCleanup("oversized", /Artifact exceeds the 1-byte download limit/);
});

test("aborted response waits for the output file to close", { concurrency: false }, async () => {
  await checkFailureCleanup("aborted", /Artifact download ended before completion/);
});
