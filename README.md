# WebCodex

[English](README.md) | [简体中文](README.zh-CN.md)

**WebCodex lets ChatGPT, Claude, and other AI agents work directly with code and developer tools on your own machines.**

Ask your assistant to inspect a repository, modify code, run tests, use Git, or investigate a failure. Your repository stays on the machine where it already lives; you do not need to move the project into a hosted workspace just to use an AI coding agent.

## Start using WebCodex

### Everyday development: full WebCodex (recommended)

If you want ChatGPT to keep using your real development environment, start with a **regular Server + Runner**. This is the full development experience: durable access to multiple projects plus project exploration, editing, Git, commands, tests, long-running work, and code navigation. Public HTTPS, Cloudflare Tunnel, and OpenAI Secure MCP Tunnel are only ways for ChatGPT to reach the Server; they do not switch you into a different restricted experience.

For Windows or macOS, the recommended first path is **WebCodex Desktop + the official OpenAI Secure Tunnel**. Follow the screenshot-based [Desktop installation guide](docs/desktop-install.md). For CLI, an existing Server, self-hosting, or advanced setup, use the [Full Setup guide](docs/PERSONAL_SETUP.md).

### Just trying it for a few minutes: temporary share

To quickly see whether WebCodex fits your workflow, run this inside one repository:

```bash
cd /path/to/your/repository
npx --yes @yyjeqhc/webcodex share
```

`share` starts a temporary, single-project, restricted WebCodex environment and prints the ChatGPT connection values. The endpoint and temporary credential stop working when the command exits. It is intended for trials and short-lived sharing, not as the default full daily setup. See the [Quick Trial](docs/QUICK_START.md) for the exact steps.

## What can it do?

- **Understand and edit code** — read, search, inspect, and make guarded changes inside configured projects.
- **Use the real toolchain** — run commands, tests, formatters, compilers, and project-specific tooling on the machine that owns the repository.
- **Work with Git** — inspect status and diffs while keeping repository operations visible and reviewable.
- **Handle long-running work** — keep jobs observable instead of requiring one model turn to stay open indefinitely.
- **Support human review** — use the [Runtime Console](docs/runtime-console.md) and task workflow to guide, cancel, accept, or reject work where those actions are available.

## Why WebCodex?

- **Your code stays on your machine.** The repository does not need to be copied into the chat service.
- **The agent gets a real development environment.** It can use the same files, Git checkout, compilers, tests, and tools you already use.
- **Work survives beyond a single request.** Long-running execution and evidence remain observable through WebCodex.
- **Start temporary or run it long-term.** Use one-command sharing for a quick session, or connect machines to a self-hosted Server for a durable setup.

## How it works

```text
AI client
   |
   | MCP / HTTPS
   v
WebCodex
   |
   v
your machine
   |
   +-- repository
   +-- Git
   +-- compilers / tests / developer tools
```

For the internal Server/Runner architecture, protocol surfaces, and authority boundaries, see [Architecture](docs/ARCHITECTURE.md), [MCP](docs/MCP.md), and [Authentication](docs/AUTH_MODEL.md).

## Platforms

- **Linux x64/arm64** — local `share`, Server, and Runner workflows.
- **macOS x64/arm64** — Desktop local Server + Runner, OpenAI Secure Tunnel, local `share`, and standalone Runner workflows.
- **Windows x64** — Desktop local Server + Runner with the official OpenAI Secure Tunnel, plus CLI + Runner, local foreground Server, and explicit `webcodex share --tunnel cloudflare|openai|none`.
- **Windows arm64** — CLI + Runner, local foreground Server, and `share`; managed OpenAI `tunnel-client` is supported. The pinned Cloudflare release has no official Windows ARM64 artifact, so Cloudflare requires a trusted explicit/PATH `cloudflared`. The 0.4.0 Desktop installer is Windows x64 only. WebCodex-managed Windows Server services remain unsupported outside Desktop's owned foreground runtime.

Windows and long-lived deployments are covered in [Deployment](docs/DEPLOYMENT.md) and [MCP](docs/MCP.md).

## Existing Servers and advanced setup

If someone already provides the WebCodex Server and connection credential, use that existing Server and follow the [Full Setup guide](docs/PERSONAL_SETUP.md). For a normal Windows/macOS personal installation, use the [Desktop guide](docs/desktop-install.md). Use [Deployment](docs/DEPLOYMENT.md) only for production hosting, multiple users, systemd/Docker, OAuth, proxies, and private CAs.

Those are follow-up operating concerns, not concepts a first-time user should have to learn before WebCodex works.

## Documentation

- [Desktop installation](docs/desktop-install.md) — recommended Windows/macOS path: Desktop + official OpenAI Secure Tunnel
- [Full Setup](docs/PERSONAL_SETUP.md) — CLI, existing Server, Linux, and advanced regular Server + Runner setup
- [Quick Trial](docs/QUICK_START.md) — temporarily try one repository with `share`
- [MCP](docs/MCP.md) — ChatGPT, Claude, authentication choices, and MCP reference
- [Deployment](docs/DEPLOYMENT.md) — production, self-hosting, and advanced operations
- [Troubleshooting](docs/TROUBLESHOOTING.md) — connection and runtime problems
- [CLI](docs/CLI.md) — command and credential reference
- [AI-assisted setup](docs/AI_ONBOARDING.md) — have an AI agent help configure WebCodex
- [Security](SECURITY.md) — security model and operational guidance
- [Documentation index](docs/INDEX.md) — all user and contributor documentation

## Security

WebCodex can read and modify files and execute commands inside configured project boundaries. Use version control, keep credentials out of prompts/logs/Git, and register only project roots the assistant should access. Read [SECURITY.md](SECURITY.md) for the complete model.

## Build from source

```bash
cargo build --release --workspace --bins
export PATH="$PWD/target/release:$PATH"
```

## Contributing

Contributions are welcome, including contributions created with WebCodex itself or other coding agents. For bug reports, development workflow, and pull request guidance, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Acknowledgements

Thanks to the [LINUX DO](https://linux.do/) community for its welcoming space for technical discussion and support for open-source sharing.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
