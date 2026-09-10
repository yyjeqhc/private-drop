# Using WebCodex Desktop

[English](desktop-guide.md) | [简体中文](desktop-guide.zh-CN.md)

Desktop prepares local projects and manages connections. You ask for work in ChatGPT or another AI client. For installation, Tunnel configuration, and system permissions, see the [installation and connection guide](desktop-install.md).

## First use

1. Choose **Use WebCodex on this computer** on the welcome page, the recommended personal setup.
2. Click **Choose folder** and select the actual project you want AI to use. Review the project path at the top, then apply the setup.
3. Tunnel presence checks live under **Optional: check ChatGPT tunnel configuration**. Enter the Tunnel ID and API key and click Save configuration to prefer the local file without restarting. You can also prepare the local project before configuring a Tunnel. If configuration is present, you can opt into connecting ChatGPT after setup.
4. On Home, confirm the current project path. The three steps are **Prepare your project → Connect your AI client → Start working**.
5. Click **Open connection settings**, select **OpenAI Secure Tunnel**, then click **Start secure tunnel**. Selecting a connection method alone does not start a process.
6. When the tunnel is ready, follow the connection page instructions to enter the Tunnel ID in ChatGPT and perform one real project read.

**Local tunnel readiness does not prove ChatGPT is connected.** Home keeps the verification step pending until Desktop observes a real call against the current project.

If you already have a remote Server, choose the existing Server option on the welcome page, enter its address and pairing code, and select a local project. Connection shows the remote Server; its operator manages external connectivity.

For temporary use of one project, choose Quick Share and a connection provider. Quick Share has its own temporary lifecycle, with handoff information and a stop control on Home.

## Start each day on Home

Home prioritizes overall readiness, the next action, and your current project. Restore the runtime when stopped; start a secure tunnel once the local runtime and Tunnel configuration are ready.

- **Current project** shows its name and full path. In a configured local Full Runtime, **Choose another project** or **Add project** opens the folder picker and applies that exact project immediately; there is no second setup confirmation.
- **Three steps** distinguish project readiness, connection preparation, and verified use.
- **View runtime diagnostics** expands the detailed Service, Runner, project, and connection states.
- **View projects / Manage connection / View activity** open the corresponding pages.

You do not need to stop the runtime or OpenAI Secure Tunnel before changing projects. Desktop adds the selected exact project root to the Runner policy, hot-activates it on a compatible Runner, persists the current selection, and keeps the existing Service and Tunnel. Only a legacy or incompatible Runner may need its Desktop-owned Runner process refreshed. Do not broaden allowed directories to work around a project loading failure.

## Connections and recovery

Connection shows current status and available controls first, followed by ChatGPT handoff instructions. Missing Tunnel configuration automatically expands presence checks. The section also saves configuration and shows its active source. API keys use a password field; saved values are never returned to the UI.

| Situation | Next action |
| --- | --- |
| Local runtime is not ready | Restore it on Home; reconfigure the same project if necessary |
| Tunnel ID or API key is missing | Enter and save the Tunnel ID and API key in Desktop; quitting and reopening is needed only for environment-variable changes |
| Start failed with no active tunnel | Fix configuration or networking, then click Start again with the same selection |
| An existing tunnel reports an error | Stop it, then start it again; stop failures remain visible and can be retried |
| Tunnel ready but clipboard handoff failed | Restore clipboard access, then stop and start the tunnel |
| Tunnel ready, waiting for ChatGPT | Configure the Tunnel in ChatGPT and read the selected project's README |

**Settings → OpenAI Tunnel network** controls automatic, direct, and custom HTTP proxy modes. Stop a running tunnel before changing its proxy, save, then start it again.

## Activity, settings, and background operation

**Activity** shows newest entries first. Search content or sources, or select **Warnings and errors only**. Filtering never deletes records.

**Settings** contains language, launch at login, Tunnel networking, and diagnostics. You can also switch language at the bottom of the sidebar. Supported languages are 简体中文, English, 日本語, 한국어, Deutsch, and Français. The selection is remembered across restarts, and activity times follow the selected locale. System tray menus and raw backend diagnostics remain in English; the operating system controls native file-picker language.

Closing the window hides it in the menu bar or system tray; the runtime continues in the background. **Quit WebCodex** in the tray ends Desktop and its owned processes. Stopping the Desktop-managed runtime on Home also updates the saved runtime startup preference. These controls do not terminate independently started WebCodex processes.

Use **⌘ + 1–5** on macOS or **Ctrl + 1–5** on Windows to switch between the five pages. Shortcuts do not intercept form-field typing. Use Tab to focus controls and Enter to activate them; diagnostic disclosure controls also support the keyboard.
