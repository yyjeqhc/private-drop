# Desktop artwork

`app-icon.png` is the original user-provided desktop artwork. Keep it as the source
for all sizes; `brand.png` is its generated 128px in-app version.

From `apps/desktop`, run `npm run icons` after replacing the source. This uses the
installed Tauri CLI to regenerate the Windows ICO, PNG bundle sizes, and in-app
brand image. On macOS it also repacks Tauri's ICNS output through `iconutil` so the
tracked `icon.icns` is reproducible; other platforms preserve that canonical file
and should use a macOS run when the source artwork changes. Only desktop outputs
are retained. The explicit icon list in `src-tauri/tauri.conf.json` is shared by
packaging and the native default window icon used by the tray. Do not edit generated
sizes independently.
