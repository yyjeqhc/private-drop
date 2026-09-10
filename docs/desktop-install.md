# WebCodex Desktop: quick install and ChatGPT connection

[English](desktop-install.md) | [简体中文](desktop-install.zh-CN.md)

For normal Windows or macOS personal use, **WebCodex Desktop + the official OpenAI Secure Tunnel is the recommended path**. It keeps the Server and Runner local, gives ChatGPT a private Tunnel connection, and avoids making first-time users configure reverse proxies, OAuth, system services, or a public WebCodex endpoint.

The normal path is intentionally product-level; you do not need to understand the Runner registry, `runtime_project_id`, the internal MCP authorization file, or launchd internals:

```text
install Desktop
→ prepare and confirm the Tunnel configuration
→ choose the real project ChatGPT should use
→ wait for Service / Runner / Project to become ready
→ start the official OpenAI Secure Tunnel
→ connect ChatGPT with the Tunnel ID
→ Desktop shows “Tunnel ready; waiting for ChatGPT”
→ perform one real project read from ChatGPT as final acceptance
```

**Important:** “OpenAI Secure Tunnel ready” proves only that the local Tunnel is ready **for** ChatGPT. It does **not** prove that ChatGPT has connected or that a project tool can already execute. The real project read in step 8 is the final proof. For CLI, an existing remote Server, production hosting, or advanced networking, use the [Full Setup](PERSONAL_SETUP.md) or [Deployment](DEPLOYMENT.md) guides instead.

For everyday use after installation, see [Using Desktop](desktop-guide.md). Home now centers the current project and three usage steps; component details live under “View runtime diagnostics”. OpenAI Platform screenshots below are configuration references; follow the current Desktop control names in the text.

## 1. Install WebCodex Desktop

Download the matching Desktop artifact from the [GitHub Releases](https://github.com/yyjeqhc/webcodex/releases) page:

- **Windows:** use the Windows x64 installer.
- **macOS:** use the DMG matching your Mac architecture, Intel or Apple Silicon.

Current macOS builds are ad-hoc signed and are not notarized. If Gatekeeper blocks the first launch of a newly downloaded build, open **System Settings → Privacy & Security → Open Anyway**, then confirm **Open**. Do not disable Gatekeeper globally.

Launch WebCodex Desktop after installation.

**Success looks like:** the main WebCodex window opens without a missing package/runtime error.

**If it fails:** use **Open Anyway** for the Gatekeeper case above; if the packaged runtime is missing, reinstall the same Desktop version instead of manually assembling internal binaries.

**Next:** confirm the Tunnel configuration before starting a Tunnel.

### Background lifecycle and Launch at Login

WebCodex Desktop is a long-running local runtime controller. Closing the main window does **not** quit the application:

- On **macOS**, use the WebCodex menu-bar item to reopen the window.
- On **Windows**, use the WebCodex system-tray icon to reopen the window.
- The Desktop-owned local Server, Runner, regular OpenAI Secure Tunnel, and an active Quick Share session continue running while the window is hidden. Quick Share remains a temporary session; it is not converted into a permanent service by background residency.
- **Stop local runtime** is a desired-state action: it stops the local runtime and changes the saved runtime preference. It is different from hiding the window.
- **Quit WebCodex** is the application-exit action. Quit stops the Desktop-owned process tree before Desktop exits. It does not broadly terminate unrelated WebCodex processes that Desktop does not own.

In **Settings → Background & startup**, **Launch WebCodex at login** registers Desktop with the operating system and starts it in the background without opening the main window. This setting is separate from the saved runtime preferences: `runtime_autostart` still decides whether the saved local runtime is restored, and the saved connection preference still decides whether the regular ChatGPT Tunnel is restored when appropriate.

### Everyday controls

- Use the sidebar to open Home, Projects, Connection, Activity, and Settings. Use **⌘ + 1–5** on macOS or **Ctrl + 1–5** on Windows. These shortcuts do not intercept typing in form fields.
- Home shows the current project, next action, and three usage steps. Expand **View runtime diagnostics** to inspect all four components.
- In **Activity**, search messages or sources, or select **Warnings and errors only**. Results are newest first. Filtering changes the view without deleting records.
- Use Tab to focus main buttons and Enter to activate them. Navigation moves focus into page content.

## 2. Prepare an OpenAI Tunnel

Create a Tunnel in the OpenAI Platform and prepare an API key that can use that Tunnel:

- [Tunnels - OpenAI API](https://platform.openai.com/settings/organization/tunnels)
- [API keys - OpenAI API](https://platform.openai.com/settings/organization/api-keys)

The Tunnel name is up to you. Record the Tunnel ID. A Restricted API key with only the Tunnel permissions you need is recommended.

![OpenAI Tunnels page](desktop-install/image-20260906171606559.png)

![OpenAI API Keys page](desktop-install/image-20260906171633208.png)

Do not commit or share real API keys, WebCodex tokens, or authorization values.

## 3. Give Desktop the Tunnel settings

The normal Desktop Tunnel path needs only:

```text
CONTROL_PLANE_TUNNEL_ID
CONTROL_PLANE_API_KEY
```

You do not need `OPENAI_ADMIN_KEY` or `OPENAI_API_KEY` for this path. The Desktop package does not bundle `tunnel-client`; when the official OpenAI Secure Tunnel is first needed, WebCodex downloads and verifies the pinned client automatically. Most users therefore do not install it manually. If managed download fails, check the network/proxy first; `WEBCODEX_TUNNEL_CLIENT_BIN` is an advanced override.

### Windows

Set the values as persistent variables for the current user, then **fully quit WebCodex and start it again**:

```powershell
[Environment]::SetEnvironmentVariable("CONTROL_PLANE_TUNNEL_ID", "tunnel_...", "User")
[Environment]::SetEnvironmentVariable("CONTROL_PLANE_API_KEY", "<restricted-tunnel-key>", "User")
```

### macOS

Apps launched from Finder or the Dock do **not** read `~/.zshrc`. If you keep the values in your shell setup, Terminal may see them while Desktop does not.

For a temporary test, launch Desktop from a Terminal that already has the variables:

```bash
source ~/.zshrc
"/Applications/WebCodex Desktop.app/Contents/MacOS/WebCodex"
```

To keep launching from Finder or the Dock, copy the current values into the login session's launchd environment, then reopen Desktop:

```bash
source ~/.zshrc
launchctl setenv CONTROL_PLANE_TUNNEL_ID "$CONTROL_PLANE_TUNNEL_ID"
launchctl setenv CONTROL_PLANE_API_KEY "$CONTROL_PLANE_API_KEY"
```

If you want Computer Use features such as screenshots, window observation, keyboard, or pointer control, grant the permissions required by macOS to the process actually running WebCodex Runner/Desktop. At minimum this may include **Screen & System Audio Recording**; UI control also requires **Accessibility**. Restart the affected process after changing these permissions when macOS requires it.

Back in Desktop, open **Connection → Optional: check ChatGPT tunnel configuration** and check **OpenAI Tunnel configuration detection**. Missing configuration expands this section automatically. The same checks are also available in **Settings → OpenAI Tunnel network**:

- `Tunnel ID` should say **Detected**;
- `Tunnel API key` should say **Detected**;
- the UI reports presence only and never displays the API-key value.

If you just changed environment variables, use **Recheck configuration**. Recheck only observes the environment visible to the **current Desktop process**. It does not execute `~/.zshrc` or secretly load credentials.

If Recheck still says **Not detected**, remember that clicking the window close button only hides WebCodex in the menu bar/system tray. It does not create a new process. Use the tray/menu-bar **Quit WebCodex** action, make sure Desktop has actually exited, then start it again. On macOS, Finder/Dock still will not read `~/.zshrc`; use the Terminal-launch or login-session environment approach above.

**Success looks like:** both fields say **Detected** and the OpenAI Secure Tunnel action is available.

**If it fails:** use Recheck first; if the current process still cannot see the settings, fully quit WebCodex and relaunch it. Closing and reopening the window is not a restart.

**Next:** choose the real project ChatGPT should use.

## 4. Start the local runtime and add your project

On first use, choose **Local Full Runtime / Use WebCodex on this computer**, then select the **real repository directory you want ChatGPT to use**. Desktop prepares the local Service + Runner and makes the Runner load that exact project. Desktop's own default workspace is not a substitute for selecting your actual project.

Setup remains disabled until you select a project. If project setup or the optional Tunnel start fails, the setup page keeps your selection and shows the error so you can retry.

This is an intentional authority boundary: the default Desktop project does not grant access to unrelated directories or the whole disk.

For local pairing, Desktop derives a Server-compatible username from your OS username: names the Server already accepts are kept as-is; otherwise ASCII letters are lowercased, each run of unsupported characters becomes a single `-`, the name's own `-` characters are preserved, leading and trailing generated separators are removed, and the result is limited to 64 characters. Names with nothing left use `desktop`. This local pairing name is not an OS login identity; existing saved enrollment is reused on restart.

Home puts overall readiness and the next action first, with direct shortcuts to Projects, Connection, and Activity. In Projects, use **Choose another project** when a default project already exists, or **Add project** when none is configured. Choose a runtime mode and workspace in setup before applying the change. **Back to overview** exits setup; entering or leaving setup alone does not change the runtime.

After setup, expand **View runtime diagnostics** on Home and confirm all three items:

- Service: Running / Ready;
- Runner: Connected / Ready;
- Project: Ready, with the path you actually selected.

If you see **Project not ready** / `project_not_loaded`, normal Desktop users do **not** need to inspect a project registry. Use the **Reload project** action in the error card. Desktop retries the same project and, when required, restarts only the Runner process it owns within a bounded readiness window.

When the runtime is already running and you choose another project, do not manually stop the whole runtime first. Select the new project and apply the change. Desktop keeps the local Service running, replaces its own old Runner when required, and waits for the new project to become ready. A failed transition must not leave the old project falsely displayed as ready.

**Success looks like:** Service, Runner, and Project are all Ready, and the displayed project path is exact.

**If it fails:** use **Reload project**, then inspect Activity/error details if it still fails. Do not broaden allowed roots or substitute a different Runner to bypass project authority.

**Next:** start the OpenAI Secure Tunnel only after these three are ready.

## 5. Configure Tunnel networking if needed

Open **Settings → OpenAI Tunnel network**:

- **Automatic (recommended):** use the proxy inherited by Desktop; Windows can also detect the system proxy.
- **Direct:** do not use a proxy.
- **Custom HTTP proxy:** for example `http://127.0.0.1:7890`.

If the Tunnel is already running, stop it, save the new network setting, and start it again. You do **not** need to restart Desktop; each Tunnel start reads the current setting.

**Success looks like:** the network mode saves successfully. If you do not need special proxy handling, leave **Automatic (recommended)** selected.

**If it fails:** fix Tunnel networking here; do not change project authority or Runner configuration to work around a network error.

**Next:** start the OpenAI Secure Tunnel.

## 6. Start the official OpenAI Secure Tunnel

Open **Connection**, select **OpenAI Secure Tunnel**, then click **Start secure tunnel**. Selection alone does not start or stop processes. If an existing tunnel reports an error, stop it before starting again; failures remain visible with a retry path. When local handoff is ready, Desktop should show wording such as:

> OpenAI Secure Tunnel ready; waiting for ChatGPT

Desktop copies the Tunnel ID to the clipboard when the operating system allows it.

Desktop must **not** promote daemon readiness or a successful clipboard copy to “ChatGPT connected” or “Ready to use”. Those facts prove only that the Tunnel is **ready for ChatGPT**.

**Success looks like:** the Tunnel itself is locally ready while the overall presentation still makes clear that ChatGPT connection/final verification is pending.

**If it fails:** first confirm that both configuration-presence fields from step 3 are Detected, then check the proxy mode in step 5 and use the on-screen Tunnel recovery action.

**Next:** enter the Tunnel ID in ChatGPT.

## 7. Add WebCodex to ChatGPT

When creating the custom connection/app in ChatGPT:

1. Choose **Tunnel** as the connection method.
2. Enter the Tunnel ID from Desktop/OpenAI.
3. Set **Authentication** to **None / No authentication**.

You do not configure OAuth in ChatGPT for this path. WebCodex keeps the MCP authorization credential locally and the Tunnel client injects it; ChatGPT does not need the local credential.

After saving the ChatGPT connection, return to Desktop. Saving a connection in ChatGPT does not magically upgrade local Tunnel evidence into an authoritative “connected” signal. If this Desktop version has no stable external-MCP-client observation signal, it intentionally continues to say that it is waiting for ChatGPT.

**Success looks like:** the ChatGPT connection is saved and the Desktop Tunnel remains running.

**If it fails:** verify that you entered the Tunnel ID, not the API key; Authentication should be None / No authentication. Do not paste the Tunnel API key into ChatGPT.

**Next:** immediately perform one real project read.

![Create ChatGPT connection](desktop-install/image-20260906174157920.png)

![Tunnel configuration](desktop-install/image-20260906174207352.png)

![Connected](desktop-install/image-20260906174215647.png)

## 8. Minimal acceptance check

After connecting, start with **one minimal real project read**, for example: “List the WebCodex projects, then read README from the project I just selected.” Only this proves the full path is actually working:

- list the WebCodex projects;
- read a file from the project you explicitly added;
- create, read back, and remove one temporary file inside that project;
- run a read-only command such as `git status`, `uname -a`, or `ver`;
- if you enabled Computer Use, list windows or capture a browser window.

If these work, the Tunnel, Server, Runner, project authority, and ordinary tool path are connected correctly.

If Desktop shows a healthy Service / Runner / Project / Tunnel but ChatGPT still cannot list projects or read a file, do **not** treat local green status as proof of an external connection. Recheck the Tunnel ID and ChatGPT connection configuration, then inspect Desktop Connection / Activity for the latest state.

## Troubleshooting

**OpenAI Secure Tunnel is disabled:** use **OpenAI Tunnel configuration detection**. It shows exactly which presence check is missing. **Recheck configuration** observes only the current process. If you just set the variables, fully quit WebCodex and launch a new process; closing the window is not a quit.

**macOS Terminal sees the values but Desktop does not:** Finder/Dock apps do not load `~/.zshrc`; use the Terminal-launch or `launchctl setenv` path above. Desktop Recheck does not execute shell startup scripts.

**Tunnel startup or ChatGPT connection times out:** check **Settings → OpenAI Tunnel network** first, then stop and restart the Tunnel after changing the proxy mode.

**A repository is not accessible:** add that directory explicitly on the **Projects** page instead of broadening the default Desktop project's filesystem authority.

**You see `project_not_loaded` / Project not ready:** use **Reload project**. Desktop retries the same project and manages only its own Runner; normal users do not need to edit or understand the internal project registry.

**I closed the window and reopened it, but new environment variables are still missing:** since the background-lifecycle change, closing the window hides Desktop in the tray/menu bar. Use **Quit WebCodex** there, then start a new process.
