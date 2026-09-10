import { copyFileSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const cache = join(root, "node_modules", ".cache");
mkdirSync(cache, { recursive: true });
const output = mkdtempSync(join(cache, "desktop-icons-"));
try {
  execFileSync(process.execPath, [
    join(root, "node_modules", "@tauri-apps", "cli", "tauri.js"),
    "icon", join(root, "src", "assets", "app-icon.png"), "--output", output,
  ], { cwd: root, stdio: "inherit" });
  for (const name of ["32x32.png", "128x128.png", "128x128@2x.png", "icon.png", "icon.ico"]) {
    copyFileSync(join(output, name), join(root, "src-tauri", "icons", name));
  }

  const icnsDestination = join(root, "src-tauri", "icons", "icon.icns");
  if (process.platform === "darwin") {
    // Tauri's ICNS encoder currently produces byte-different containers for
    // identical pixels. Repacking through iconutil gives us a stable macOS
    // artifact while keeping Tauri responsible for all source resizing.
    const iconset = join(output, "canonical.iconset");
    const canonicalIcns = join(output, "canonical.icns");
    execFileSync("iconutil", ["-c", "iconset", join(output, "icon.icns"), "-o", iconset]);
    execFileSync("iconutil", ["-c", "icns", iconset, "-o", canonicalIcns]);
    copyFileSync(canonicalIcns, icnsDestination);
  } else {
    console.warn("Preserving the tracked macOS icon.icns; run npm run icons on macOS to refresh its canonical form.");
  }

  copyFileSync(join(output, "128x128.png"), join(root, "src", "assets", "brand.png"));
} finally {
  rmSync(output, { recursive: true, force: true });
}
