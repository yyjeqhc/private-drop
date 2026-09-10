# Desktop artwork

`app-icon.png` is the original user-provided desktop artwork. Keep it as the source
for all sizes; `brand.png` is its generated 128px in-app version.

From `apps/desktop`, run `npm run icons` after replacing the source. This uses the
installed Tauri CLI to regenerate the Windows ICO, macOS ICNS, PNG bundle sizes,
and in-app brand image. Only desktop outputs are retained. The explicit icon list
in `src-tauri/tauri.conf.json` is shared by packaging and the native default window
icon used by the tray. Do not edit generated sizes independently.
