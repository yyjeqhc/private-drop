# WebCodex Desktop: quick install and ChatGPT connection

[English](desktop-install.md) | [简体中文](desktop-install.zh-CN.md)

For normal Windows or macOS personal use, **WebCodex Desktop + the official OpenAI Secure Tunnel is the recommended path**. It keeps the Server and Runner local, gives ChatGPT a private Tunnel connection, and avoids making first-time users configure reverse proxies, OAuth, system services, or a public WebCodex endpoint.

The normal path is:

```text
install Desktop
→ start the local Server + Runner
→ start the official OpenAI Secure Tunnel
→ connect ChatGPT with the Tunnel ID
→ add the real project you want the AI to use
```

For CLI, an existing remote Server, production hosting, or advanced networking, use the [Full Setup](PERSONAL_SETUP.md) or [Deployment](DEPLOYMENT.md) guides instead.

## 1. Install WebCodex Desktop

Download the matching Desktop artifact from the [GitHub Releases](https://github.com/yyjeqhc/webcodex/releases) page:

- **Windows:** use the Windows x64 installer.
- **macOS:** use the DMG matching your Mac architecture, Intel or Apple Silicon.

Current macOS builds are ad-hoc signed and are not notarized. If Gatekeeper blocks the first launch of a newly downloaded build, open **System Settings → Privacy & Security → Open Anyway**, then confirm **Open**. Do not disable Gatekeeper globally.

Launch WebCodex Desktop after installation.

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

Set the values as persistent variables for the current user, then fully quit and reopen WebCodex Desktop:

```powershell
[Environment]::SetEnvironmentVariable("CONTROL_PLANE_TUNNEL_ID", "tunnel_...", "User")
[Environment]::SetEnvironmentVariable("CONTROL_PLANE_API_KEY", "<restricted-tunnel-key>", "User")
```

![Windows Desktop example](desktop-install/image-20260906171812348.png)

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

## 4. Start the local runtime and add your project

On first use, Desktop prepares its local Server + Runner. Windows initially has a Desktop-owned default project; macOS uses Desktop's own workspace. For real development, open **Projects** and explicitly add the repository directory you want the AI to use.

This is an intentional authority boundary: the default Desktop project does not grant access to unrelated directories or the whole disk.

For local pairing, Desktop derives a Server-compatible username from your OS username: names the Server already accepts are kept as-is; otherwise ASCII letters are lowercased, each run of unsupported characters becomes a single `-`, the name's own `-` characters are preserved, leading and trailing generated separators are removed, and the result is limited to 64 characters. Names with nothing left use `desktop`. This local pairing name is not an OS login identity; existing saved enrollment is reused on restart.

![Local runtime example](desktop-install/image-20260906171904811.png)

## 5. Configure Tunnel networking if needed

Open **Settings → OpenAI Tunnel network**:

- **Automatic (recommended):** use the proxy inherited by Desktop; Windows can also detect the system proxy.
- **Direct:** do not use a proxy.
- **Custom HTTP proxy:** for example `http://127.0.0.1:7890`.

If the Tunnel is already running, stop it, save the new network setting, and start it again. You do **not** need to restart Desktop; each Tunnel start reads the current setting.

![Connection page](desktop-install/image-20260906172102174.png)

![Tunnel network settings](desktop-install/image-20260906173905826.png)

## 6. Start the official OpenAI Secure Tunnel

Open **Connection → OpenAI Secure Tunnel** and start it. When the connection is ready, Desktop shows the established Tunnel and copies the Tunnel ID to the clipboard when the operating system allows it.

![OpenAI Secure Tunnel running](desktop-install/image-20260906174123335.png)

## 7. Add WebCodex to ChatGPT

When creating the custom connection/app in ChatGPT:

1. Choose **Tunnel** as the connection method.
2. Enter the Tunnel ID from Desktop/OpenAI.
3. Set **Authentication** to **None / No authentication**.

You do not configure OAuth in ChatGPT for this path. WebCodex keeps the MCP authorization credential locally and the Tunnel client injects it; ChatGPT does not need the local credential.

![Create ChatGPT connection](desktop-install/image-20260906174157920.png)

![Tunnel configuration](desktop-install/image-20260906174207352.png)

![Connected](desktop-install/image-20260906174215647.png)

## 8. Minimal acceptance check

After connecting, ask ChatGPT to do a few small checks:

- list the WebCodex projects;
- read a file from the project you explicitly added;
- create, read back, and remove one temporary file inside that project;
- run a read-only command such as `git status`, `uname -a`, or `ver`;
- if you enabled Computer Use, list windows or capture a browser window.

If these work, the Tunnel, Server, Runner, project authority, and ordinary tool path are connected correctly.

## Troubleshooting

**OpenAI Secure Tunnel is disabled:** make sure the newly launched Desktop process can see `CONTROL_PLANE_TUNNEL_ID` and `CONTROL_PLANE_API_KEY`.

**macOS Terminal sees the values but Desktop does not:** Finder/Dock apps do not load `~/.zshrc`; use the Terminal-launch or `launchctl setenv` path above.

**Tunnel startup or ChatGPT connection times out:** check **Settings → OpenAI Tunnel network** first, then stop and restart the Tunnel after changing the proxy mode.

**A repository is not accessible:** add that directory explicitly on the **Projects** page instead of broadening the default Desktop project's filesystem authority.
