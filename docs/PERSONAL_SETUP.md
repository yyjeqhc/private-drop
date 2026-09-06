# Full WebCodex Setup

[English](PERSONAL_SETUP.md) | [简体中文](PERSONAL_SETUP.zh-CN.md)

Use this path for CLI setup, an existing Server, or a setup that needs more control than Desktop provides. For a normal Windows or macOS personal installation, the recommended first path is [WebCodex Desktop + the official OpenAI Secure Tunnel](desktop-install.md). In either case, the full WebCodex model is the same: ChatGPT connects to a regular Server, and a persistent Runner uses the real projects, Git checkout, compiler, and test tools on your machine.

If you only want to try WebCodex for a few minutes, use the [Quick Trial](QUICK_START.md) and `webcodex share` instead. `share` is a temporary, single-project, more restricted experience that ends when the command exits.

## What you will have

```text
ChatGPT / another AI client
          |
          | MCP
          v
   WebCodex Server
          |
          v
       Runner
          |
          +-- your projects
          +-- Git
          +-- compilers / tests / developer tools
```

The Server may run on the repository machine or elsewhere. The Runner should run on the machine that actually owns the code and development environment. Public HTTPS, Cloudflare Tunnel, and OpenAI Secure MCP Tunnel are only ways for the AI client to reach the Server; they do not turn a regular Server into a different restricted execution mode.

## 1. Install WebCodex

On Windows or macOS, prefer the dedicated [Desktop installation guide](desktop-install.md): install Desktop from the matching GitHub Release, let it manage the local Server + Runner, and connect ChatGPT through the official OpenAI Secure Tunnel. The command-line setup below is the alternative for Linux, existing Servers, advanced configuration, and troubleshooting.

For the CLI path, install Node.js 18+ and Git, then:

```bash
npm install -g @yyjeqhc/webcodex
webcodex --version
```

## 2. Start a regular Server

For a first personal setup, foreground mode is the easiest path to understand and diagnose. You can switch to a Linux service or another long-lived process manager after the setup works.

Linux example:

```bash
webcodex server init \
  --listen 127.0.0.1:8080 \
  --data-dir "$HOME/.local/share/webcodex" \
  --env-file "$HOME/.config/webcodex/webcodex.env"

webcodex server run \
  --env-file "$HOME/.config/webcodex/webcodex.env"
```

Windows PowerShell example:

```powershell
$envFile = Join-Path $HOME ".config\webcodex\webcodex.env"
$dataDir = Join-Path $HOME ".local\share\webcodex"

webcodex server init --listen 127.0.0.1:8080 --data-dir $dataDir --env-file $envFile
webcodex server run --env-file $envFile
```

Keep that terminal running. On Linux, once the setup is verified, the [Deployment guide](DEPLOYMENT.md) covers installing the Server as a service.

## 3. Choose the connection paths

There are really two connections. Ordinary users only need to make sure each can reach the Server:

- **Runner / CLI → Server**: when Server and Runner are on the same machine, `http://127.0.0.1:8080` is enough; otherwise use a Server address reachable by the Runner machine.
- **ChatGPT → Server**: hosted ChatGPT cannot reach your computer's `127.0.0.1`, so use a stable public HTTPS endpoint, Cloudflare Tunnel, OpenAI Secure MCP Tunnel, or another trusted reachable entry.

A Tunnel solves reachability only. Do not switch to `webcodex share` merely because you use a Tunnel; the full setup continues to use the regular Server started above.

With public HTTPS or Cloudflare, both connections can often use the same HTTPS Server URL. OpenAI Secure MCP Tunnel may differ: the Runner can keep using loopback/LAN while ChatGPT reaches `/mcp` through the Tunnel.

For Windows + OpenAI Secure MCP Tunnel, see the [Windows + OpenAI Tunnel deep-dive and troubleshooting guide](WINDOWS_OPENAI_TUNNEL.md). It contains the current setup/troubleshooting path plus a separately labeled historical validation record; treat it as deeper material to read only when needed.

Below, `<server-url>` means the address used by the **CLI / Runner** to reach the Server. A same-machine setup can use `http://127.0.0.1:8080`; an existing stable HTTPS URL can be used directly as well.

## 4. Create a one-time login code

On the Server machine, open another terminal and create a short-lived pairing code:

```bash
webcodex pairing create \
  --server-url <server-url> \
  --env-file <server-init-used-env-file> \
  --username <your-name> \
  --ttl-secs 600
```

Only carry the resulting `wc_pair_...` code to the Runner machine. Do not copy the Server administrator token or the whole Server env file.

Even when Server and Runner are on the same computer, this login flow is useful because everyday development access stays separate from Server administration.

## 5. Log in and choose a project

On the machine that owns the code:

The example below assumes **regular HTTPS MCP**, so it includes `--print-mcp-config` in the same one-time login. If ChatGPT will connect through **OpenAI Secure MCP Tunnel**, remove `--print-mcp-config` and follow the Tunnel guide for the ChatGPT-side connection; never redeem the same pairing code again just to print configuration.

```bash
webcodex login <server-url> \
  --code <wc_pair_...> \
  --allowed-root /path/to/your/projects \
  --project /path/to/your/projects/my-repo \
  --print-mcp-config
```

Windows PowerShell example:

```powershell
webcodex login <server-url> `
  --code <wc_pair_...> `
  --allowed-root E:\git `
  --project E:\git\my-repo `
  --print-mcp-config
```

For ordinary use, remember only two things:

- `--project` is the project you want the AI to use now;
- `--allowed-root` is a parent directory from which you may add more projects later.

You do not need to edit the Runner's internal configuration files manually.

To add another project later, use the Runner config printed by login:

```bash
webcodex project register --config <runner-config> /path/to/another-repo
```

## 6. Start the Runner

`login` prints the Runner configuration path. On Windows, run it in the foreground:

```bash
webcodex runner run --config <login-reported-runner-config>
```

On Linux, foreground mode is also a good first check:

```bash
webcodex runner run --config <login-reported-runner-config>
```

Or install it as a user service:

```bash
webcodex runner install --scope user --config <login-reported-runner-config>
```

Once the Runner is online, WebCodex has an execution path to the machine's files, Git checkout, compiler, tests, and other development tools.

## 7. Add WebCodex to ChatGPT

If you chose regular HTTPS MCP, the `login --print-mcp-config` command above prints the connection values after that **same one-time login** succeeds. Do not redeem the pairing code first and then repeat `login` just to obtain MCP values; pairing codes are single-use.

That output contains a user credential. Only you should enter it into ChatGPT; do not paste it into issues, logs, or chat messages. Create an App / MCP connection in ChatGPT using the MCP URL and authentication method printed by the CLI, then run **Scan Tools**.

If you chose OpenAI Secure MCP Tunnel, login does not need `--print-mcp-config` for ChatGPT. ChatGPT normally uses Tunnel + No authentication while the local tunnel client injects the WebCodex credential. Follow the Tunnel guide instead of copying the local Bearer into ChatGPT.

## 8. Verify the full experience

Start with a read-only request:

```text
Inspect this project and summarize its structure. Do not change files yet.
```

Then try a small, reviewable change:

```text
Fix one small issue, run the relevant tests, and tell me what actually changed.
```

With the full setup, the AI should be able to explore and edit projects, use Git, run commands and tests, handle long-running work, and use code navigation. Exact availability still depends on the local directories/features you allow this Runner to use and the permissions actually granted by the AI client.

## Quick share vs. full setup

| Goal | Recommended entry | What it is |
| --- | --- | --- |
| Try one repository for a few minutes | `webcodex share` | One command, temporary, single-project, ends on exit, more restricted |
| Everyday use on Windows/macOS | [Desktop + official OpenAI Secure Tunnel](desktop-install.md) | Local Server + Runner, multiple projects, simplest recommended personal path |
| CLI, Linux, or an existing Server | regular Server + Runner (this guide) | Durable identity, multiple projects, full development tools, independent network choice |
| Operate a team/production deployment | [Deployment](DEPLOYMENT.md) | systemd, Docker, OAuth, multi-user and operator reference |

A Tunnel is a network path, not a permission mode. Choosing a public hostname, Cloudflare, or OpenAI Tunnel should not determine whether you use the temporary-share or full-setup product experience.

## Troubleshooting

Start with three simple checks:

```bash
webcodex server status --env-file <server-env-file>
webcodex runner status --config <runner-config>
webcodex ops status --server-url <server-url> --token-file <login-reported-webcodex-user-token>
```

See [Troubleshooting](TROUBLESHOOTING.md) for detailed failures. The [CLI reference](CLI.md) documents every command and internal configuration field, but you do not need to understand those internals before successfully using WebCodex.
