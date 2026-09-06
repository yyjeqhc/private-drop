"use strict";

const assert = require("assert");
const childProcess = require("child_process");
const fs = require("fs");
const http = require("http");
const os = require("os");
const path = require("path");
const { pathToFileURL } = require("url");
const zlib = require("zlib");
const install = require("../install");
const wrapper = require("../bin/wrapper");
const packageJson = require("../package.json");
const releaseManifestPath = path.join(__dirname, "..", "manifest.json");
const releaseManifest = fs.existsSync(releaseManifestPath)
  ? JSON.parse(fs.readFileSync(releaseManifestPath, "utf8"))
  : null;
const exampleManifest = require("../manifest.example.json");

// The whole suite runs against the native platform so `npm test` passes
// unchanged on windows-latest and on Linux: no sh, no bash, no Git Bash, no
// Unix tar, no WSL, and no Unix chmod semantics.
const PLATFORM = process.platform;
const ARCH = process.arch;
const KEY = install.platformKey(PLATFORM, ARCH);
const EXE = (name) => install.exeName(name, PLATFORM);

function defaultIdentity() {
  return `${packageJson.version} test-revision dirty=false`;
}

// A test fixture "binary".
//
// - Unix: a small shell script that answers `--version` and echoes its
//   arguments — a real spawned executable, so the installer's actual
//   spawn-based version check runs.
// - Windows: CI has no C toolchain to build fake PE fixtures, so the file
//   carries its identity as content and the narrow test-only seam
//   (`versionIdentity` override, see install.js) reads it. The real
//   spawn-based validation against actual build output is covered by the
//   Windows real-binary smoke (scripts/npm_install_windows_smoke.ps1).
function makeBinary(dir, name, identity = defaultIdentity()) {
  const file = path.join(dir, name);
  if (PLATFORM === "win32") {
    // The identity content uses the runtime binary name (no `.exe`),
    // matching the `<name> <version> <identity>` parsing in install.js.
    const runtimeName = name.endsWith(".exe") ? name.slice(0, -4) : name;
    fs.writeFileSync(file, `${runtimeName} ${identity}\n`);
    return file;
  }
  fs.writeFileSync(
    file,
    `#!/bin/sh\nif [ "\${WEBCODEX_REQUIRE_WRAPPER_MARKER-}" = "1" ] && [ "\${WEBCODEX_NPM_WRAPPER-}" != "1" ]; then exit 24; fi\nif [ "\${1-}" = "--version" ]; then echo "${name} ${identity}"; exit 0; fi\nprintf '%s\\n' "$@"\nexit "\${WEBCODEX_TEST_EXIT:-0}"\n`,
    { mode: 0o755 }
  );
  return file;
}

function makeBinarySet(dir, identity) {
  fs.mkdirSync(dir, { recursive: true });
  for (const name of install.runtimeBinaryFiles(PLATFORM)) makeBinary(dir, name, identity);
}

// Pure-Node tar.gz writer: no external `tar` binary on any platform.
function tarEntry(name, content) {
  const size = content.length;
  const header = Buffer.alloc(512);
  header.write(name, 0, 100, "utf8");
  header.write("0000644\0", 100, 8, "ascii");
  header.write("0000000\0", 108, 8, "ascii");
  header.write("0000000\0", 116, 8, "ascii");
  header.write(`${size.toString(8).padStart(11, "0")}\0`, 124, 12, "ascii");
  header.write("00000000000\0", 136, 12, "ascii");
  header.fill(0x20, 148, 156);
  header[156] = "0".charCodeAt(0);
  header.write("ustar\0", 257, 6, "ascii");
  let checksum = 0;
  for (const byte of header) checksum += byte;
  header.write(`${checksum.toString(8).padStart(6, "0")}\0 `, 148, 8, "ascii");
  const padded = Buffer.alloc(Math.ceil(size / 512) * 512);
  content.copy(padded);
  return Buffer.concat([header, padded]);
}

function archiveDirectory(sourceDir, archive) {
  const entries = [];
  for (const entry of fs.readdirSync(sourceDir)) {
    const content = fs.readFileSync(path.join(sourceDir, entry));
    entries.push(tarEntry(entry, content));
  }
  entries.push(Buffer.alloc(1024)); // end-of-archive zero blocks
  fs.writeFileSync(archive, zlib.gzipSync(Buffer.concat(entries)));
}

// The narrow test-only seam implementation for Windows: mirrors the
// production `versionIdentity` parsing exactly (first line must be
// "<name> <identity>", identity must match the package version), reading the
// identity from the fixture file content instead of spawning it.
function fakeVersionIdentity(binary, name, expectedVersion) {
  const content = fs.readFileSync(binary, "utf8").trim().split(/\r?\n/, 1)[0];
  const prefix = `${name} `;
  if (!content.startsWith(prefix)) {
    throw new Error(`${name} returned an unexpected version string`);
  }
  const identity = content.slice(prefix.length);
  if (identity !== expectedVersion && !identity.startsWith(`${expectedVersion} `)) {
    throw new Error(`${name} version does not match package ${expectedVersion}`);
  }
  return identity;
}

// Only Windows uses the seam: on Unix the fixture scripts are real spawned
// executables and the production validation path runs untouched.
function testOptions(extra = {}) {
  const options = { platform: PLATFORM, arch: ARCH, ...extra };
  if (PLATFORM === "win32") options.versionIdentity = fakeVersionIdentity;
  return options;
}

function writeTarHeader(name, size) {
  const header = Buffer.alloc(512);
  header.write(name, 0, 100, "utf8");
  header.write("0000755\0", 100, 8, "ascii");
  header.write("0000000\0", 108, 8, "ascii");
  header.write("0000000\0", 116, 8, "ascii");
  header.write(`${size.toString(8).padStart(11, "0")}\0`, 124, 12, "ascii");
  header.write("00000000000\0", 136, 12, "ascii");
  header.fill(0x20, 148, 156);
  header[156] = "0".charCodeAt(0);
  header.write("ustar\0", 257, 6, "ascii");
  let checksum = 0;
  for (const byte of header) checksum += byte;
  header.write(`${checksum.toString(8).padStart(6, "0")}\0 `, 148, 8, "ascii");
  return header;
}

function makeDeclaredEntryArchive(file, name, size) {
  const tar = Buffer.concat([writeTarHeader(name, size), Buffer.alloc(1024)]);
  fs.writeFileSync(file, zlib.gzipSync(tar));
}

async function withServer(handler, fn) {
  const server = http.createServer(handler);
  const sockets = new Set();
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    await fn(`http://127.0.0.1:${server.address().port}`);
  } finally {
    for (const socket of sockets) socket.destroy();
    await new Promise((resolve) => server.close(resolve));
  }
}

async function waitFor(predicate, timeoutMs = 500) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("Timed out waiting for test condition");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

function manifestFor(url, sha256, key = KEY) {
  return {
    version: packageJson.version,
    binaries: install.RUNTIME_BINARIES,
    artifacts: { [key]: { url, sha256 } }
  };
}

function writeManifest(file, manifest) {
  fs.writeFileSync(file, JSON.stringify(manifest));
}

function installedIdentity(destination) {
  const file = path.join(destination, EXE("webcodex"));
  if (PLATFORM === "win32") {
    // Fixture binaries are not real PE images; the identity lives in the
    // file content (see `makeBinary` / the version-identity seam).
    const line = fs.readFileSync(file, "utf8").trim().split(/\r?\n/, 1)[0];
    return `${line}\n`;
  }
  return childProcess.execFileSync(file, ["--version"], { encoding: "utf8" });
}

function assertCompleteInstall(destination) {
  assert.deepStrictEqual(
    fs.readdirSync(destination).sort(),
    install.runtimeBinaryFiles(PLATFORM).slice().sort()
  );
}

function assertNoInstallerTemps(tempRoot) {
  const leftovers = fs.readdirSync(tempRoot).filter((name) =>
    name.startsWith("webcodex-manifest-") || name.startsWith("webcodex-artifact-") || name.startsWith(".bin-staging-")
  );
  assert.deepStrictEqual(leftovers, []);
}

async function expectInstallFailure(action, destination, tempRoot, pattern) {
  const identityBefore = installedIdentity(destination);
  await assert.rejects(action, pattern);
  assert.strictEqual(installedIdentity(destination), identityBefore);
  assertCompleteInstall(destination);
  assertNoInstallerTemps(tempRoot);
}

async function main() {
  assert.strictEqual(packageJson.version, "0.4.0");
  assert.deepStrictEqual(packageJson.bin, { webcodex: "bin/webcodex.js" });
  assert.deepStrictEqual(install.RUNTIME_BINARIES, ["webcodex", "webcodex-server", "webcodex-runner"]);
  assert.deepStrictEqual(install.SUPPORTED_PLATFORM_KEYS, ["linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64", "win32-x64", "win32-arm64"]);
  assert.strictEqual(install.MAX_MANIFEST_BYTES, 1024 * 1024);
  assert.strictEqual(install.MAX_ARTIFACT_BYTES, 128 * 1024 * 1024);
  assert.strictEqual(install.MAX_UNCOMPRESSED_BYTES, 256 * 1024 * 1024);
  assert.strictEqual(install.MAX_TAR_ENTRY_BYTES, 96 * 1024 * 1024);
  assert.strictEqual(install.MAX_REDIRECTS, 5);
  assert.strictEqual(install.MAX_CA_FILE_BYTES, 4 * 1024 * 1024);
  const httpsNetwork = install.resolveNpmNetworkOptions("https://github.com/example", {
    npm_config_https_proxy: "http://proxy.example:8443",
    npm_config_noproxy: "localhost",
    npm_config_strict_ssl: "false"
  });
  assert.strictEqual(httpsNetwork.proxy.href, "http://proxy.example:8443/");
  assert.strictEqual(httpsNetwork.noProxy, "localhost");
  assert.strictEqual(httpsNetwork.ca, undefined);
  assert.strictEqual(httpsNetwork.rejectUnauthorized, false);
  const httpNetwork = install.resolveNpmNetworkOptions("http://example.test", {
    npm_config_proxy: "http://proxy.example:8080",
    npm_config_strict_ssl: "true"
  });
  assert.strictEqual(httpNetwork.proxy.href, "http://proxy.example:8080/");
  assert.strictEqual(httpNetwork.noProxy, undefined);
  assert.strictEqual(httpNetwork.ca, undefined);
  assert.strictEqual(httpNetwork.rejectUnauthorized, true);
  assert.match(
    install.sanitizeNetworkErrorMessage("connect through http://user:secret@proxy.example:8080/path?token=hidden failed"),
    /^connect through http:\/\/proxy\.example:8080\/\.\.\. failed$/
  );
  assert.doesNotMatch(
    install.sanitizeNetworkErrorMessage("failed https://github.com/release?token=hidden"),
    /token|hidden/
  );
  assert.strictEqual(install.resolveRedirectUrl("http://example.test/a", "https://example.test/b").protocol, "https:");
  assert.strictEqual(install.resolveRedirectUrl("http://example.test/a", "/b").protocol, "http:");
  assert.throws(() => install.resolveRedirectUrl("https://example.test/a", "http://example.test/b?token=secret"), /HTTPS downgrade/);
  assert.throws(() => install.resolveRedirectUrl("http://example.test/a", "file:\/\/\/tmp\/secret?credential=hidden"), /unsupported protocol/);
  const manifests = releaseManifest ? [exampleManifest, releaseManifest] : [exampleManifest];
  for (const manifest of manifests) {
    assert.strictEqual(manifest.version, packageJson.version);
    assert.deepStrictEqual(manifest.binaries, install.RUNTIME_BINARIES);
    install.validateManifest(manifest);
  }

  assert.strictEqual(install.platformKey("linux", "x64"), "linux-x64");
  assert.strictEqual(install.platformKey("darwin", "arm64"), "darwin-arm64");
  assert.strictEqual(install.platformKey("win32", "arm64"), "win32-arm64");
  assert.throws(() => install.platformKey("sunos", "x64"), /Unsupported/);
  assert.strictEqual(
    wrapper.nativePath({ packageRoot: "/tmp/package", platform: "linux" }),
    path.posix.normalize("/tmp/package/vendor/bin/webcodex")
  );
  assert.strictEqual(
    wrapper.nativePath({ packageRoot: "C:\\package", platform: "win32" }),
    path.win32.normalize("C:\\package\\vendor\\bin\\webcodex.exe")
  );

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "webcodex-npm-test-"));
  try {
    const source = path.join(tmp, "source");
    const destination = path.join(tmp, "destination");
    makeBinarySet(source);
    const identity = install.copyLocalBinaryDir(source, testOptions({ destinationDir: destination }));
    assert.strictEqual(identity, defaultIdentity());
    for (const name of install.runtimeBinaryFiles(PLATFORM)) {
      const file = path.join(destination, name);
      assert.ok(fs.statSync(file).isFile());
      if (PLATFORM !== "win32") {
        assert.ok((fs.statSync(file).mode & 0o111) !== 0, `${file} is not executable`);
      }
    }
    assert.ok(!fs.existsSync(path.join(destination, "webcodex-cli")));

    const oldDestination = path.join(tmp, "old-destination");
    makeBinarySet(oldDestination, `${packageJson.version} old-revision dirty=false`);
    const incomplete = path.join(tmp, "incomplete");
    fs.mkdirSync(incomplete);
    makeBinary(incomplete, EXE("webcodex"));
    makeBinary(incomplete, EXE("webcodex-server"));
    assert.throws(
      () => install.copyLocalBinaryDir(incomplete, testOptions({ destinationDir: oldDestination })),
      /missing webcodex-runner/
    );
    assert.match(installedIdentity(oldDestination), /old-revision/);
    assertCompleteInstall(oldDestination);

    const mixed = path.join(tmp, "mixed");
    makeBinarySet(mixed);
    makeBinary(mixed, EXE("webcodex-runner"), `${packageJson.version} other-revision dirty=false`);
    assert.throws(
      () => install.copyLocalBinaryDir(mixed, testOptions({ destinationDir: oldDestination })),
      /not from the same build/
    );
    assert.match(installedIdentity(oldDestination), /old-revision/);
    assertCompleteInstall(oldDestination);

    const archiveSource = path.join(tmp, "archive-source");
    makeBinarySet(archiveSource);
    fs.writeFileSync(path.join(archiveSource, "unrelated.txt"), "ignored");
    fs.writeFileSync(path.join(archiveSource, "webcodex-cli"), "legacy");
    const archive = path.join(tmp, "artifact.tar.gz");
    archiveDirectory(archiveSource, archive);
    const manifestPath = path.join(tmp, "manifest.json");
    const caFile = path.join(tmp, "corporate-ca.pem");
    fs.writeFileSync(caFile, "-----BEGIN CERTIFICATE-----\ntest-ca\n-----END CERTIFICATE-----\n");
    assert.strictEqual(
      install.resolveNpmNetworkOptions("https://github.com/example", { npm_config_cafile: caFile }).ca,
      fs.readFileSync(caFile, "utf8")
    );
    assert.throws(
      () => install.resolveNpmNetworkOptions("https://github.com/example", { npm_config_cafile: path.join(tmp, "missing-ca.pem") }),
      /npm cafile could not be read \(ENOENT\)/
    );
    writeManifest(manifestPath, manifestFor(pathToFileURL(archive).toString(), install.sha256File(archive)));
    const downloaded = path.join(tmp, "downloaded");
    await install.installFromManifest(manifestPath, testOptions({ destinationDir: downloaded, tempDir: tmp }));
    assert.deepStrictEqual(
      fs.readdirSync(downloaded).sort(),
      install.runtimeBinaryFiles(PLATFORM).slice().sort()
    );

    writeManifest(manifestPath, manifestFor(pathToFileURL(archive).toString(), "0".repeat(64)));
    await expectInstallFailure(
      () => install.installFromManifest(manifestPath, testOptions({ destinationDir: downloaded, tempDir: tmp })),
      downloaded, tmp, /checksum mismatch/
    );

    const corrupt = path.join(tmp, "corrupt.tar.gz");
    fs.writeFileSync(corrupt, "not gzip");
    writeManifest(manifestPath, manifestFor(pathToFileURL(corrupt).toString(), install.sha256File(corrupt)));
    await expectInstallFailure(
      () => install.installFromManifest(manifestPath, testOptions({ destinationDir: downloaded, tempDir: tmp })),
      downloaded, tmp, /valid bounded gzip archive/
    );

    await withServer((_req, res) => { res.statusCode = 503; res.end("unavailable"); }, async (base) => {
      writeManifest(manifestPath, manifestFor(`${base}/artifact.tar.gz?token=secret`, install.sha256File(archive)));
      await expectInstallFailure(
        () => install.installFromManifest(manifestPath, testOptions({ destinationDir: downloaded, tempDir: tmp })),
        downloaded, tmp, /HTTP 503/
      );
    });

    {
      let observedProxyTarget = null;
      await withServer((req, res) => {
        observedProxyTarget = req.url;
        res.end("proxy-delivered");
      }, async (proxyBase) => {
        const proxiedDest = path.join(tmp, "proxied-download.bin");
        await install.fetchToFile("http://origin.invalid/artifact.tar.gz?token=visible#fragment-secret", proxiedDest, {
          label: "Proxy",
          totalTimeoutMs: 500,
          maxBytes: 1024,
          environment: { npm_config_proxy: proxyBase }
        });
        assert.strictEqual(fs.readFileSync(proxiedDest, "utf8"), "proxy-delivered");
        assert.match(observedProxyTarget, /token=visible/);
        assert.doesNotMatch(observedProxyTarget, /fragment-secret/);
        fs.rmSync(proxiedDest, { force: true });
      });
    }

    await withServer((_req, res) => { res.end("direct-delivered"); }, async (targetBase) => {
      await withServer((_req, res) => { res.statusCode = 502; res.end("proxy must be bypassed"); }, async (proxyBase) => {
        const directDest = path.join(tmp, "no-proxy-download.bin");
        await install.fetchToFile(`${targetBase}/artifact.tar.gz`, directDest, {
          label: "No proxy",
          totalTimeoutMs: 500,
          maxBytes: 1024,
          environment: { npm_config_proxy: proxyBase, npm_config_noproxy: "127.0.0.1" }
        });
        assert.strictEqual(fs.readFileSync(directDest, "utf8"), "direct-delivered");
        fs.rmSync(directDest, { force: true });
      });
    });

    {
      const errorDest = path.join(tmp, "network-error.bin");
      await assert.rejects(
        () => install.fetchToFile("http://127.0.0.1:9/artifact.tar.gz?token=hidden", errorDest, {
          label: "Artifact",
          firstByteTimeoutMs: 500,
          totalTimeoutMs: 1000,
          maxBytes: 1024,
          environment: {}
        }),
        (err) => {
          assert.strictEqual(err.code, "ECONNREFUSED");
          assert.match(err.message, /Artifact download request failed \(ECONNREFUSED\)/);
          assert.match(err.message, /ECONNREFUSED/);
          assert.doesNotMatch(err.message, /token|hidden|artifact\.tar\.gz/);
          return true;
        }
      );
      assert.ok(!fs.existsSync(errorDest));
    }

    await withServer((_req, _res) => {}, async (base) => {
      await expectInstallFailure(
        () => install.installFromManifest(`${base}/manifest.json?credential=secret`, testOptions({
          destinationDir: downloaded,
          tempDir: tmp,
          manifestDownload: { firstByteTimeoutMs: 40, inactivityTimeoutMs: 40, totalTimeoutMs: 100 }
        })),
        downloaded, tmp, /Manifest download timed out waiting for a response/
      );
    });

    await withServer((_req, res) => {
      res.writeHead(200, { "Content-Type": "application/octet-stream" });
      res.write(Buffer.from([0x1f]));
    }, async (base) => {
      writeManifest(manifestPath, manifestFor(`${base}/artifact.tar.gz?credential=secret`, install.sha256File(archive)));
      await expectInstallFailure(
        () => install.installFromManifest(manifestPath, testOptions({
          destinationDir: downloaded,
          tempDir: tmp,
          // This case exercises the inactivity (stall) path specifically: the
          // server sends one byte and then never finishes. The first-byte
          // budget must be comfortably larger than the loopback response
          // latency, otherwise a loaded CI can trip the first-byte timer
          // ("timed out waiting for a response") before the stall is observed.
          artifactDownload: { firstByteTimeoutMs: 5000, inactivityTimeoutMs: 40, totalTimeoutMs: 10000 }
        })),
        downloaded, tmp, /Artifact download stalled before completion/
      );
    });

    await withServer((_req, res) => {
      res.writeHead(200, { "Content-Length": "4096" });
      res.end("small");
    }, async (base) => {
      writeManifest(manifestPath, manifestFor(`${base}/artifact.tar.gz`, install.sha256File(archive)));
      await expectInstallFailure(
        () => install.installFromManifest(manifestPath, testOptions({
          destinationDir: downloaded,
          tempDir: tmp,
          limits: { maxArtifactBytes: 1024 }
        })),
        downloaded, tmp, /1024-byte download limit/
      );
    });

    await withServer((_req, res) => {
      res.writeHead(200, { "Transfer-Encoding": "chunked" });
      res.write(Buffer.alloc(700));
      res.end(Buffer.alloc(700));
    }, async (base) => {
      writeManifest(manifestPath, manifestFor(`${base}/artifact.tar.gz`, install.sha256File(archive)));
      await expectInstallFailure(
        () => install.installFromManifest(manifestPath, testOptions({
          destinationDir: downloaded,
          tempDir: tmp,
          limits: { maxArtifactBytes: 1024 }
        })),
        downloaded, tmp, /1024-byte download limit/
      );
    });

    {
      const redirectSockets = new Set();
      let redirectSocket = null;
      let redirectSocketClosed = false;
      const server = http.createServer((req, res) => {
        if (req.url === "/redirect") {
          redirectSocket = req.socket;
          redirectSocket.on("close", () => { redirectSocketClosed = true; });
          res.writeHead(302, { Location: "/target" });
          res.write("redirect body never ends");
          const timer = setInterval(() => res.write("."), 10);
          res.on("close", () => clearInterval(timer));
          return;
        }
        res.end("redirect target");
      });
      server.on("connection", (socket) => {
        redirectSockets.add(socket);
        socket.on("close", () => redirectSockets.delete(socket));
      });
      await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
      const redirectDest = path.join(tmp, "redirect-target.bin");
      try {
        await install.fetchToFile(`http://127.0.0.1:${server.address().port}/redirect`, redirectDest, {
          label: "Redirect lifecycle",
          firstByteTimeoutMs: 100,
          inactivityTimeoutMs: 100,
          totalTimeoutMs: 500,
          maxBytes: 1024
        });
        assert.strictEqual(fs.readFileSync(redirectDest, "utf8"), "redirect target");
        await waitFor(() => redirectSocketClosed, 300);
        assert.ok(redirectSocket);
        assert.strictEqual(redirectSocketClosed, true);
        assert.strictEqual(redirectSocket.destroyed, true);
      } finally {
        for (const socket of redirectSockets) socket.destroy();
        await new Promise((resolve) => server.close(resolve));
        fs.rmSync(redirectDest, { force: true });
      }
    }

    await withServer((req, res) => {
      const match = /^\/deadline\/(\d+)$/.exec(req.url);
      if (!match) return res.end("done");
      const step = Number(match[1]);
      setTimeout(() => {
        res.writeHead(302, { Location: step < 3 ? `/deadline/${step + 1}` : "/target" });
        res.end();
      }, 35);
    }, async (base) => {
      const deadlineDest = path.join(tmp, "redirect-deadline.bin");
      await assert.rejects(
        () => install.fetchToFile(`${base}/deadline/0`, deadlineDest, {
          label: "Redirect deadline",
          firstByteTimeoutMs: 80,
          inactivityTimeoutMs: 80,
          totalTimeoutMs: 90,
          maxBytes: 1024
        }),
        /total timeout/
      );
      assert.ok(!fs.existsSync(deadlineDest));
    });

    await withServer((req, res) => {
      const match = /^\/count\/(\d+)$/.exec(req.url);
      const count = match ? Number(match[1]) : 0;
      if (count === 5) return res.end("five redirects succeeded");
      res.writeHead(302, { Location: `/count/${count + 1}` });
      res.end();
    }, async (base) => {
      const fiveDest = path.join(tmp, "five-redirects.bin");
      await install.fetchToFile(`${base}/count/0`, fiveDest, { label: "Redirect count", totalTimeoutMs: 500, maxBytes: 1024 });
      assert.strictEqual(fs.readFileSync(fiveDest, "utf8"), "five redirects succeeded");
      fs.rmSync(fiveDest, { force: true });
    });

    await withServer((req, res) => {
      const match = /^\/limit\/(\d+)$/.exec(req.url);
      const count = match ? Number(match[1]) : 0;
      res.writeHead(302, { Location: `/limit/${count + 1}` });
      res.end();
    }, async (base) => {
      const limitDest = path.join(tmp, "redirect-limit.bin");
      await assert.rejects(
        () => install.fetchToFile(`${base}/limit/0`, limitDest, { label: "Redirect count", totalTimeoutMs: 500, maxBytes: 1024 }),
        /redirect limit/
      );
      assert.ok(!fs.existsSync(limitDest));
    });

    await withServer((_req, res) => {
      res.writeHead(302, { Location: "file:///tmp/private?credential=hidden" });
      res.end();
    }, async (base) => {
      const protocolDest = path.join(tmp, "redirect-protocol.bin");
      await assert.rejects(
        () => install.fetchToFile(`${base}/redirect?token=secret`, protocolDest, { label: "Redirect protocol", totalTimeoutMs: 500, maxBytes: 1024 }),
        (err) => {
          assert.match(err.message, /unsupported protocol/);
          assert.doesNotMatch(err.message, /secret|hidden|credential|token/);
          return true;
        }
      );
      assert.ok(!fs.existsSync(protocolDest));
    });

    const expansion = path.join(tmp, "expansion.tar.gz");
    fs.writeFileSync(expansion, zlib.gzipSync(Buffer.alloc(4096)));
    writeManifest(manifestPath, manifestFor(pathToFileURL(expansion).toString(), install.sha256File(expansion)));
    await expectInstallFailure(
      () => install.installFromManifest(manifestPath, testOptions({
        destinationDir: downloaded,
        tempDir: tmp,
        limits: { maxUncompressedBytes: 1024 }
      })),
      downloaded, tmp, /1024-byte uncompressed size limit/
    );

    const oversizedEntry = path.join(tmp, "oversized-entry.tar.gz");
    makeDeclaredEntryArchive(oversizedEntry, EXE("webcodex"), 2048);
    writeManifest(manifestPath, manifestFor(pathToFileURL(oversizedEntry).toString(), install.sha256File(oversizedEntry)));
    await expectInstallFailure(
      () => install.installFromManifest(manifestPath, testOptions({
        destinationDir: downloaded,
        tempDir: tmp,
        limits: { maxTarEntryBytes: 1024 }
      })),
      downloaded, tmp, /1024-byte limit/
    );

    const oversizedManifest = path.join(tmp, "oversized-manifest.json");
    fs.writeFileSync(oversizedManifest, " ".repeat(2048));
    await expectInstallFailure(
      () => install.installFromManifest(oversizedManifest, testOptions({
        destinationDir: downloaded,
        tempDir: tmp,
        limits: { maxManifestBytes: 1024 }
      })),
      downloaded, tmp, /1024-byte size limit/
    );

    // Wrapper behavior: the wrapper must find the native binary, forward
    // arguments, and propagate the exit code. On Windows the fixture is
    // node.exe — a real PE image — with a `-e` script that echoes the
    // forwarded arguments and exits 23; on Unix it is the sh fixture.
    const bootstrapRoot = path.join(tmp, "wrapper-bootstrap");
    fs.mkdirSync(bootstrapRoot, { recursive: true });
    const bootstrapMarker = path.join(bootstrapRoot, "bootstrap-marker");
    fs.writeFileSync(
      path.join(bootstrapRoot, "install.js"),
      `require("fs").writeFileSync(${JSON.stringify(bootstrapMarker)}, "ok");\n`
    );
    assert.strictEqual(wrapper.bootstrapNative({ packageRoot: bootstrapRoot }), true);
    assert.strictEqual(fs.readFileSync(bootstrapMarker, "utf8"), "ok");

    assert.strictEqual(wrapper.needsNpmNetworkContext([], false), true);
    assert.strictEqual(wrapper.needsNpmNetworkContext(["share"], false), true);
    assert.strictEqual(wrapper.needsNpmNetworkContext(["status"], false), false);
    assert.strictEqual(wrapper.needsNpmNetworkContext(["status"], true), true);

    if (process.env.npm_execpath && /npm-cli\.js$/i.test(process.env.npm_execpath)) {
      const windowsInvocation = wrapper.npmInvocation(
        { npm_execpath: process.env.npm_execpath },
        { platform: "win32" }
      );
      assert.strictEqual(windowsInvocation.program, process.execPath);
      assert.deepStrictEqual(windowsInvocation.prefixArgs, [process.env.npm_execpath]);

      const windowsConfigRoot = path.join(tmp, "wrapper-windows-config");
      fs.mkdirSync(windowsConfigRoot, { recursive: true });
      const windowsUserConfig = path.join(windowsConfigRoot, ".npmrc");
      fs.writeFileSync(
        windowsUserConfig,
        "https-proxy=http://windows-user:windows-secret@proxy.example:8443/\nproxy=http://windows-user:windows-secret@proxy.example:8080/\nnoproxy=localhost\nstrict-ssl=false\n"
      );
      const windowsHydrated = wrapper.rehydrateNpmNetworkEnvironment(
        {
          PATH: process.env.PATH || "",
          HOME: windowsConfigRoot,
          USERPROFILE: windowsConfigRoot,
          npm_config_userconfig: windowsUserConfig
        },
        {
          platform: "win32",
          npmCliPath: process.env.npm_execpath,
          networkHelper: path.join(__dirname, "..", "bin", "npm-network-env.js")
        }
      );
      assert.match(windowsHydrated.npm_config_https_proxy, /windows-user:windows-secret@/);
      assert.match(windowsHydrated.npm_config_proxy, /windows-user:windows-secret@/);
      assert.strictEqual(windowsHydrated.npm_config_noproxy, "localhost");
      assert.strictEqual(windowsHydrated.npm_config_strict_ssl, "false");
    }

    const npmInvocation = wrapper.npmInvocation(process.env, { platform: PLATFORM });
    assert.ok(npmInvocation, "npm invocation could not be resolved");
    if (PLATFORM === "win32") {
      assert.strictEqual(npmInvocation.program, process.execPath);
      assert.strictEqual(npmInvocation.prefixArgs.length, 1);
      assert.match(npmInvocation.prefixArgs[0], /npm-cli\.js$/i);
    } else {
      assert.strictEqual(npmInvocation.program, "npm");
      assert.deepStrictEqual(npmInvocation.prefixArgs, []);
    }

    if (PLATFORM !== "win32") {
      const networkRoot = path.join(tmp, "wrapper-network");
      const networkBin = path.join(networkRoot, "bin");
      fs.mkdirSync(networkBin, { recursive: true });
      const networkHelper = path.join(__dirname, "..", "bin", "npm-network-env.js");
      const fakeNpm = path.join(networkRoot, "fake-npm");
      const protectedProxy = "http://wrapper-user:wrapper-secret@proxy.example:8443/";
      fs.writeFileSync(
        fakeNpm,
        [
          "#!/bin/sh",
          "if [ \"$1\" = exec ] && [ \"$2\" = --yes=false ] && [ \"$3\" = -- ]; then",
          `  printf '%s' '${JSON.stringify({ npm_config_https_proxy: protectedProxy, npm_config_proxy: protectedProxy, npm_config_noproxy: "localhost", npm_config_cafile: "/private/ca.pem", ignored_secret: "must-not-propagate" }).replace(/'/g, "'\\''")}'`,
          "  exit 0",
          "fi",
          "if [ \"$1\" = config ] && [ \"$2\" = get ]; then",
          "  case \"$3\" in",
          "    ca) printf '%s\\n' '-----BEGIN CERTIFICATE-----\\nwrapper-ca\\n-----END CERTIFICATE-----' ; exit 0 ;;",
          "    strict-ssl) printf '%s\\n' false ; exit 0 ;;",
          "  esac",
          "fi",
          "printf '%s\\n' 'http://leak-user:leak-secret@should-not-appear.invalid/' >&2",
          "exit 9",
          ""
        ].join("\n"),
        { mode: 0o700 }
      );
      const hydrated = wrapper.rehydrateNpmNetworkEnvironment(
        { PATH: process.env.PATH || "" },
        { packageRoot: networkRoot, npmProgram: fakeNpm, networkHelper }
      );
      assert.strictEqual(hydrated.npm_config_https_proxy, protectedProxy);
      assert.strictEqual(hydrated.npm_config_proxy, protectedProxy);
      assert.strictEqual(hydrated.npm_config_noproxy, "localhost");
      assert.strictEqual(hydrated.npm_config_cafile, "/private/ca.pem");
      assert.match(hydrated.npm_config_ca, /wrapper-ca/);
      assert.strictEqual(hydrated.npm_config_strict_ssl.trim(), "false");
      assert.strictEqual(hydrated.ignored_secret, undefined);

      const inheritedProxy = "http://existing-user:existing-secret@existing.example:8080/";
      const inherited = wrapper.rehydrateNpmNetworkEnvironment(
        { PATH: process.env.PATH || "", npm_config_https_proxy: inheritedProxy },
        { packageRoot: networkRoot, npmProgram: fakeNpm, networkHelper }
      );
      assert.strictEqual(inherited.npm_config_https_proxy, inheritedProxy);

      // A lazy bootstrap needs npm's protected proxy long enough to download the
      // native binaries, but a non-share native command must not inherit a proxy
      // credential that was absent from the caller's original environment.
      const scopedLazyRoot = path.join(tmp, "wrapper-lazy-network-scope");
      fs.mkdirSync(scopedLazyRoot, { recursive: true });
      const scopedLazyTarget = wrapper.nativePath({ packageRoot: scopedLazyRoot, platform: PLATFORM });
      const bootstrapEnvMarker = path.join(scopedLazyRoot, "bootstrap-env");
      const nativeEnvMarker = path.join(scopedLazyRoot, "native-env");
      const escapedNativeMarker = nativeEnvMarker.replace(/'/g, "'\\''");
      const scopedTargetScript = [
        "#!/bin/sh",
        'if [ -n "${npm_config_https_proxy-}" ]; then',
        `  printf leaked > '${escapedNativeMarker}'`,
        "else",
        `  printf clean > '${escapedNativeMarker}'`,
        "fi",
        "exit 23",
        ""
      ].join("\n");
      const scopedTargetBase64 = Buffer.from(scopedTargetScript, "utf8").toString("base64");
      fs.writeFileSync(
        path.join(scopedLazyRoot, "install.js"),
        [
          'const fs = require("fs");',
          'const path = require("path");',
          `const target = ${JSON.stringify(scopedLazyTarget)};`,
          `fs.writeFileSync(${JSON.stringify(bootstrapEnvMarker)}, process.env.npm_config_https_proxy ? "hydrated" : "missing");`,
          'fs.mkdirSync(path.dirname(target), { recursive: true });',
          `fs.writeFileSync(target, Buffer.from(${JSON.stringify(scopedTargetBase64)}, "base64"), { mode: 0o755 });`,
          ""
        ].join("\n")
      );
      const priorExitCode = process.exitCode;
      const scopedChild = wrapper.runNative({
        packageRoot: scopedLazyRoot,
        platform: PLATFORM,
        argv: ["status"],
        npmProgram: fakeNpm,
        networkHelper
      });
      assert.ok(scopedChild, "lazy scoped native child was not started");
      const scopedExitCode = await new Promise((resolve, reject) => {
        scopedChild.once("error", reject);
        scopedChild.once("exit", (code, signal) => signal ? reject(new Error(`scoped lazy child exited by ${signal}`)) : resolve(code));
      });
      process.exitCode = priorExitCode;
      assert.strictEqual(scopedExitCode, 23);
      assert.strictEqual(fs.readFileSync(bootstrapEnvMarker, "utf8"), "hydrated");
      assert.strictEqual(fs.readFileSync(nativeEnvMarker, "utf8"), "clean");
    }

    if (PLATFORM !== "win32") {
      const lazyRoot = path.join(tmp, "wrapper-lazy");
      fs.mkdirSync(lazyRoot, { recursive: true });
      const lazyTarget = wrapper.nativePath({ packageRoot: lazyRoot, platform: PLATFORM });
      fs.writeFileSync(
        path.join(lazyRoot, "install.js"),
        [
          'const fs = require("fs");',
          'const path = require("path");',
          `const target = ${JSON.stringify(lazyTarget)};`,
          'fs.mkdirSync(path.dirname(target), { recursive: true });',
          'fs.writeFileSync(target, "#!/bin/sh\\nprintf \'%s\\n\' \\\"$1\\\"\\nexit 23\\n", { mode: 0o755 });',
          ''
        ].join("\n")
      );
      const lazyProbe = childProcess.spawnSync(
        process.execPath,
        [path.join(__dirname, "wrapper-lazy-probe.js"), lazyRoot, "lazy-argument"],
        { encoding: "utf8" }
      );
      assert.strictEqual(lazyProbe.status, 23, lazyProbe.stderr);
      assert.strictEqual(lazyProbe.stdout.trim(), "lazy-argument");
      assert.ok(fs.existsSync(lazyTarget));
    }

    let wrapperTarget;
    let wrapperArgs;
    if (PLATFORM === "win32") {
      wrapperTarget = process.execPath;
      wrapperArgs = ["-e", "if (process.env.WEBCODEX_NPM_WRAPPER !== '1') process.exit(24); console.log(process.argv[1]); console.log(process.argv[2]); process.exitCode = 23;", "alpha", "two words"];
    } else {
      wrapperTarget = makeBinary(tmp, "wrapper-target");
      wrapperArgs = ["alpha", "two words"];
    }
    const probe = childProcess.spawnSync(
      process.execPath,
      [path.join(__dirname, "wrapper-probe.js"), wrapperTarget, ...wrapperArgs],
      { encoding: "utf8", env: { ...process.env, WEBCODEX_REQUIRE_WRAPPER_MARKER: "1" } }
    );
    assert.strictEqual(probe.status, 23, probe.stderr);
    assert.deepStrictEqual(probe.stdout.trim().split(/\r?\n/), ["alpha", "two words"]);

    const missingProbe = childProcess.spawnSync(process.execPath, [path.join(__dirname, "wrapper-missing-probe.js")], { encoding: "utf8" });
    assert.strictEqual(missingProbe.status, 127);
    assert.match(missingProbe.stderr, /installation is incomplete/);
    assert.doesNotMatch(missingProbe.stderr, /vendor\/bin/);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  console.log("npm wrapper, bounded download, and atomic installer self-test passed");
}

main().catch((err) => {
  console.error(err.stack || err.message);
  process.exit(1);
});
