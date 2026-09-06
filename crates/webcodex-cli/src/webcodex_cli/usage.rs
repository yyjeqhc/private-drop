pub(crate) fn usage() -> &'static str {
    "Usage: webcodex [COMMAND]\n\n\
Unified command-line interface for WebCodex.\n\n\
Quick trial:\n\
  share                         Temporarily share one project; ends when the command exits\n\
  (no command)                  Interactive Git repo shortcut for `share` on Linux/macOS\n\n\
Daily self-hosted setup:\n\
  server                        Configure and operate the Server\n\
  pairing create                Create a one-time login code\n\
  login                         Log the project machine in with that code\n\
  project register              Add an existing project\n\
  runner                        Configure and operate the Runner\n\
  See `webcodex server --help` and `webcodex runner --help` for full lifecycle commands.\n\n\
Existing Server:\n\
  connect                       Connect the current project to an existing Server\n\n\
Project / diagnostics:\n\
  status                        Show concise project coding readiness\n\
  doctor                        Diagnose project readiness\n\
  setup                         Configure the current Git project without starting it\n\
  run                           Run the project-bound Server and Runner locally\n\
  disconnect                    Disconnect a local project from its hosted Server\n\
  task                          Review tasks and make host-local decisions\n\n\
Account:\n\
  auth status                   Show login status\n\
  logout                        Remove this device's credentials\n\n\
Advanced / operator:\n\
  ops                           Read-only operator workflow checks\n\
  users                         Manage users\n\
  tokens                        Manage personal API credentials\n\
  runner-tokens                 Manage Runner transport credentials\n\n\
Options:\n\
  -h, --help                    Print help and exit\n\
  -V, --version                 Print version and exit\n"
}

pub(crate) fn connect_usage() -> &'static str {
    "Usage: webcodex connect <SERVER_URL> [OPTIONS]\n\n\
Connect a local project to a hosted WebCodex Server. Shared-key auth is the default;\n\
`--auth oauth` bridges that same shared-key identity through browser OAuth for ChatGPT.\n\
`--auth managed-oauth` retains the advanced managed-user OAuth flow.\n\
The command writes a reusable local profile, starts one background Runner,\n\
and waits until the Runner and project are visible through the Server.\n\n\
Options:\n\
  --proxy http://HOST:PORT   Override standard proxy environment for CLI Server requests\n\
  --no-system-proxy          Ignore proxy environment and connect directly\n\
  --auth bearer|oauth|managed-oauth\n\
                             MCP authentication mode [default: bearer]\n\
  --oauth-redirect-uri URL   Exact OAuth callback URL; required with OAuth modes\n\
  --oauth-computer-permissions\n\
                             Allow ordinary OAuth browser consent to offer optional Computer permissions\n\
  --oauth-local-mcp           Explicitly allow this OAuth client to request mcp:local authority\n\
  --oauth-local-plugins       Explicitly allow this OAuth client to request plugin:inspect + plugin:invoke authority\n\
  --oauth-local-ssh           Explicitly allow this OAuth client to request ssh:local authority\n\
  --oauth-coding-agent        Explicitly allow this OAuth client to request coding_agent:run authority\n\
  --user USER                Select a logged-in managed user; managed-oauth only\n\
  --key KEY                  Shared key (use --key-file to avoid shell history)\n\
  --key-file PATH            Read the shared key from a file\n\
  --project PATH             Local project directory [default: .]\n\
  --profile NAME             Override the derived local profile name\n\
  --client-id ID             Override the persistent Runner client id\n\
  --project-id ID            Override the derived project id\n\
  -h, --help                 Print help and exit\n\n\
In bearer mode, omitting --key/--key-file generates a strong hosted shared key.\n\
OAuth mode uses that same key for Runner transport and provisions a bridge client only\n\
after the matching Runner group is connected. Enter the shared key only on WebCodex's\n\
browser authorize page; ChatGPT receives OAuth client credentials/tokens, never the key.\n\
Without explicit opt-ins the bridge keeps the direct shared-key model-facing baseline.\n\
--oauth-computer-permissions adds only the fixed launch/display/pointer/clipboard Computer\n\
ceiling; browser checkboxes decide the actual grant. --oauth-local-mcp adds class-level\n\
mcp:local authority for Runner-owned MCP providers in this shared-key group.\n\
--oauth-local-plugins independently adds plugin:inspect + plugin:invoke authority for Runner-owned native Tool Plugins; it never grants plugin:manage.\n\
--oauth-local-ssh independently adds ssh:local authority for Runner-local managed SSH resources.\n\
--oauth-coding-agent adds only coding_agent:run delegated coding-agent authority. Existing\n\
clients are never widened implicitly. managed-oauth remains a separate managed-user flow.\n"
}

pub(crate) fn disconnect_usage() -> &'static str {
    "Usage: webcodex disconnect [OPTIONS]\n\n\
Disconnect exactly one local repository registered by hosted `webcodex connect`.\n\
The source repository and .git are never removed or modified. A live Runner is\n\
unregistered through the Server first; an offline hosted Runner is updated locally.\n\n\
Options:\n\
  --project PATH             Local project directory [default: .]\n\
  --profile NAME             Select an exact hosted profile when more than one matches\n\
  -h, --help                 Print help and exit\n"
}

pub(crate) fn project_register_usage() -> &'static str {
    "Usage: webcodex project register --config PATH <PROJECT> [OPTIONS]\n\n\
Add one existing project to a Runner configuration.\n\
A newly added project is loaded after that Runner restarts; adding the same project again is idempotent.\n\
Advanced: project_registry_dir is the Runner project registry directory, not a workspace root; allowed_roots remains the filesystem authority boundary.\n\n\
Options:\n\
  --config PATH              Runner configuration created by login/init\n\
  --json                     Print machine-readable output\n\
  -h, --help                 Print help and exit\n"
}

pub(crate) fn pairing_usage() -> &'static str {
    "Usage: webcodex pairing <COMMAND>\n\n\
     Commands:\n\
       create       Create a short-lived one-time login code\n"
}

pub(crate) fn pairing_create_usage() -> &'static str {
    "Usage: webcodex pairing create --server-url URL --username USER [--client-id CLIENT_ID] [OPTIONS]\n\n\
     Create a one-time login code on the Server, then use it once on the machine that holds the project.\n\n\
     Options:\n\
       --server-url URL          WebCodex server URL\n\
       --proxy http://HOST:PORT Explicit proxy override for this CLI request\n\
       --no-system-proxy        Ignore proxy environment and connect directly\n\
       --env-file PATH           Read WEBCODEX_TOKEN from env file\n\
       --token-file PATH         Read bootstrap/admin bearer token from file\n\
       --token TOKEN             Bootstrap/admin bearer token (discouraged in shell history)\n\
       --username USER           User to ensure/create for enrollment\n\
       --client-id CLIENT_ID     Optional device binding; omit to let the login device claim it.\n\
                                 When set, login must use the same --device value.\n\
       --display-name NAME       Optional display name for a newly created user\n\
       --ttl-secs SECS           Pairing code lifetime [default: 600; range: 60..3600]\n\
       --user-token-name NAME    Name for the user API token created during enroll\n\
       --runner-token-name NAME  Name for the Runner transport token created during enroll\n\
       --json                    Print machine-readable output\n\
       -h, --help                Print help and exit\n\n\
     Server/admin-side command:\n\
       pairing create needs server bootstrap/admin auth. The default server\n\
       bootstrap env file lives on the server, not the client.\n\
       On the client, redeem the code with: webcodex login <server-url> --code <code>\n\
       If --client-id was specified, append the matching\n\
       --device <client-id> to that login command.\n\
     Copy only the short-lived wc_pair_* code to the client. Do not copy\n\
     WEBCODEX_TOKEN, wc_pat_*, or wc_agent_* values from server to client.\n\
     This command does not create wc_pat_* or wc_agent_* token files on the\n\
     server.\n"
}

pub(crate) fn ops_usage() -> &'static str {
    "Usage: webcodex ops <COMMAND>\n\n\
     Read-only operator workflow checks for WebCodex.\n\n\
     Commands:\n\
       status                  Summarize runtime, tools, jobs, Runners, and projects\n\
       runners                 Show compact Runner fleet status\n\
       runner                  Show one exact Runner registration/build identity\n\
       projects                Show compact project inventory and smoke suitability\n\
       smoke-preflight         Check a project before deploy smoke validation\n\n\
     Common flags:\n\
       --server-url URL        WebCodex server URL [default: http://127.0.0.1:8080]\n\
       --proxy http://HOST:PORT Explicit proxy override for Server requests\n\
       --no-system-proxy       Ignore proxy environment and connect directly\n\
       --env-file PATH         Read WEBCODEX_TOKEN from env file\n\
       --token-file PATH       Read bearer token from file\n\
       --token TOKEN           Bearer token input; never printed\n\
       --json                  Print machine-readable output\n\
       -h, --help              Print help and exit\n\n\
     These commands are read-only. They do not run jobs, start shell commands,\n\
     create sessions, write files, or print token/env values.\n"
}

pub(crate) fn ops_status_usage() -> &'static str {
    "Usage: webcodex ops status [OPTIONS]\n\n\
     Summarize runtime, tools, jobs, Runners, and project health.\n\n\
     Options:\n\
       --server-url URL        WebCodex server URL [default: http://127.0.0.1:8080]\n\
       --proxy http://HOST:PORT Explicit proxy override for Server requests\n\
       --no-system-proxy       Ignore proxy environment and connect directly\n\
       --env-file PATH         Read WEBCODEX_TOKEN from env file\n\
       --token-file PATH       Read bearer token from file\n\
       --token TOKEN           Bearer token input; never printed\n\
       --json                  Print machine-readable output\n\
       --strict                Exit 2 when the ops report status is FAIL\n\
       -h, --help              Print help and exit\n"
}

pub(crate) fn ops_runners_usage() -> &'static str {
    "Usage: webcodex ops runners [OPTIONS]\n\n\
     Show compact read-only Runner fleet status.\n\n\
     Options:\n\
       --server-url URL        WebCodex server URL [default: http://127.0.0.1:8080]\n\
       --proxy http://HOST:PORT Explicit proxy override for Server requests\n\
       --no-system-proxy       Ignore proxy environment and connect directly\n\
       --env-file PATH         Read WEBCODEX_TOKEN from env file\n\
       --token-file PATH       Read bearer token from file\n\
       --token TOKEN           Bearer token input; never printed\n\
       --json                  Print machine-readable output\n\
       --strict                Exit 2 when the ops report status is FAIL\n\
       -h, --help              Print help and exit\n"
}

pub(crate) fn ops_runner_usage() -> &'static str {
    "Usage: webcodex ops runner --client-id CLIENT_ID [OPTIONS]\n\n\
     Show one exact caller-visible Runner registration and build identity.\n\n\
     Options:\n\
       --client-id CLIENT_ID   Exact Runner client_id (required)\n\
       --request-timeout-ms MS Bound one Server observation [default: 5000; max: 30000]\n\
       --server-url URL        WebCodex server URL [default: http://127.0.0.1:8080]\n\
       --proxy http://HOST:PORT Explicit proxy override for Server requests\n\
       --no-system-proxy       Ignore proxy environment and connect directly\n\
       --env-file PATH         Read WEBCODEX_TOKEN from env file\n\
       --token-file PATH       Read bearer token from file\n\
       --token TOKEN           Bearer token input; never printed\n\
       --json                  Print machine-readable output\n\
       --strict                Exit 2 when the ops report status is FAIL\n\
       -h, --help              Print help and exit\n\n\
     This command is read-only and projects only bounded runtime identity fields.\n"
}

pub(crate) fn ops_projects_usage() -> &'static str {
    "Usage: webcodex ops projects [OPTIONS]\n\n\
     Show compact read-only project inventory and smoke suitability.\n\n\
     Options:\n\
       --server-url URL        WebCodex server URL [default: http://127.0.0.1:8080]\n\
       --proxy http://HOST:PORT Explicit proxy override for Server requests\n\
       --no-system-proxy       Ignore proxy environment and connect directly\n\
       --env-file PATH         Read WEBCODEX_TOKEN from env file\n\
       --token-file PATH       Read bearer token from file\n\
       --token TOKEN           Bearer token input; never printed\n\
       --json                  Print machine-readable output\n\
       --strict                Exit 2 when the ops report status is FAIL\n\
       -h, --help              Print help and exit\n"
}

pub(crate) fn ops_smoke_preflight_usage() -> &'static str {
    "Usage: webcodex ops smoke-preflight --project PROJECT_ID [OPTIONS]\n\n\
     Read-only deploy smoke preflight for one project.\n\n\
     Options:\n\
       --project PROJECT_ID    Runtime project id to check\n\
       --server-url URL        WebCodex server URL [default: http://127.0.0.1:8080]\n\
       --proxy http://HOST:PORT Explicit proxy override for Server requests\n\
       --no-system-proxy       Ignore proxy environment and connect directly\n\
       --env-file PATH         Read WEBCODEX_TOKEN from env file\n\
       --token-file PATH       Read bearer token from file\n\
       --token TOKEN           Bearer token input; never printed\n\
       --json                  Print machine-readable output\n\
       --strict                Exit 2 when the ops report status is FAIL\n\
       -h, --help              Print help and exit\n\n\
     This command calls only read-only status/project/workspace inspection APIs.\n"
}

pub(crate) fn server_usage() -> &'static str {
    "Usage: webcodex server <COMMAND>\n\n\
The Server is the first part of the full daily WebCodex setup.\n\n\
Commands:\n\
  init        Initialize or update Server configuration\n\
  install     Install, enable, and start the Linux systemd socket/service pair\n\
  run         Run webcodex-server directly in the foreground\n\
  tunnel      Run a regular local Server OpenAI Secure Tunnel in the foreground\n\
  start       Start the Linux listener socket, then the Server service\n\
  stop        Stop Linux socket activation and the Server service\n\
  restart     Restart only the Linux Server service while the socket stays active\n\
  status      Check socket/service state, HTTP reachability, and build revisions\n\
  logs        Read bounded Linux Server service journal logs or explicitly follow them\n\
  uninstall   Remove only the Linux systemd socket/service pair; requires --confirm\n\n\
Windows supports `server init`, foreground `server run`, and `server tunnel`; WebCodex-managed Windows Server services are not supported yet.\n\
For start/stop/restart/logs/uninstall, --service-file PATH targets a custom managed service unit and derives its sibling .socket.\n"
}

pub(crate) fn server_tunnel_usage() -> &'static str {
    "Usage: webcodex server tunnel --provider openai --env-file PATH --json --stop-on-stdin-eof\n\n\
Run the canonical OpenAI Secure Tunnel for an already-running local WebCodex Server.\n\n\
Options:\n\
  --provider openai          Required provider; regular Cloudflare remains a separate future contract\n\
  --env-file PATH            Local Server env file used for loopback address and bootstrap authority\n\
  --json                     Emit the safe machine readiness event\n\
  --stop-on-stdin-eof        Stop when the owning integration closes stdin\n\
  -h, --help                 Print help and exit\n\n\
The Tunnel exposes only the local Server MCP endpoint and authenticates it with the effective Server bootstrap credential (process environment overrides the env file). The ready event contains only provider/readiness/clipboard metadata; credentials are never printed.\n"
}

pub(crate) fn server_init_usage() -> &'static str {
    "Usage: webcodex server init [OPTIONS]\n\n\
Options:\n\
  --listen ADDR          Listen address [default: 127.0.0.1:8080]\n\
  --data-dir PATH        Data directory\n\
  --env-file PATH        Server env file\n\
  --public-url URL       Optional public URL\n\
  --open                 Allow anonymous access for trusted demos\n\
  --overwrite            Update an existing env file while preserving its token\n\
  --json                 Print a summary without the full token\n\
  -h, --help             Print help and exit\n\n\
Shared-key mode is enabled. The full bootstrap token is saved only in the env file.\n"
}

pub(crate) fn server_install_service_usage() -> &'static str {
    "Usage: webcodex server install [OPTIONS]\n\n\
Options:\n\
  --env-file PATH             EnvironmentFile= path\n\
  --bin PATH                  webcodex-server path; sibling then absolute PATH by default\n\
  --service-file PATH         Service unit path; sibling .socket is derived [default: /etc/systemd/system/webcodex.service]\n\
  --user USER                 Optional systemd User=\n\
  --group GROUP               Optional systemd Group=\n\
  --working-directory PATH    WorkingDirectory=\n\
  --overwrite                 Replace an existing managed socket/service pair\n\
  --no-start                  Enable both units without starting immediately\n\
  --dry-run                   Render only; never call systemctl\n\
  --output -                  Render only; never call systemctl\n\
  --json                      Print machine-readable output\n\
  -h, --help                  Print help and exit\n\n\
Normal execution installs both units coherently, runs daemon-reload, enables both, then starts the socket before the service unless --no-start is used. Tokens are never inlined.\n"
}

pub(crate) fn server_status_usage() -> &'static str {
    "Usage: webcodex server status [OPTIONS]\n\n\
     Options:\n\
       --url URL              Runtime URL [default: http://127.0.0.1:8080]\n\
       --proxy http://HOST:PORT Explicit proxy override for Server requests\n\
       --no-system-proxy      Ignore proxy environment and connect directly\n\
       --env-file PATH        Read WEBCODEX_TOKEN from env file [default: root /etc/webcodex/webcodex.env; user ~/.config/webcodex/webcodex.env]\n\
       --token-file PATH      Read bearer token from file\n\
       --service-file PATH    Managed service unit path; sibling .socket is derived [default: /etc/systemd/system/webcodex.service]\n\
       --json                 Print a machine-readable summary\n\
       -h, --help             Print help and exit\n\n\
     Token priority: --token-file, WEBCODEX_TOKEN from --env-file, process\n\
     WEBCODEX_TOKEN, then no token for auth-disabled servers.\n"
}

pub(crate) fn runner_usage() -> &'static str {
    "Usage: webcodex runner <COMMAND>\n\n\
The Runner is the machine that executes project work for the full daily setup.\n\n\
Commands:\n\
  init        Generate a Runner config (`runner.toml`)\n\
  install     Install, enable, and start the Linux systemd Runner service\n\
  run         Run webcodex-runner directly in the foreground (all supported platforms)\n\
  start       Start a hosted background Runner or installed Linux service\n\
  stop        Stop a hosted background Runner or installed Linux service\n\
  restart     Restart a hosted background Runner or installed Linux service\n\
  status      Check Runner lifecycle, safe config metadata, and connectivity\n\
  logs        Read hosted Runner logs or the installed Linux service journal\n\
  uninstall   Remove only the Linux systemd unit; requires --confirm\n\n\
Linux systemd service commands accept --scope user|system. Non-root users default to user; root defaults to system.\n\
Profiles created by `connect` keep their detached-process behavior when --scope is omitted.\n\n\
`webcodex run` is the current-project runtime coordinator. `webcodex runner run` directly executes the standalone Runner.\n"
}
pub(crate) fn runner_init_usage() -> &'static str {
    "Usage: webcodex runner init --server-url URL [--token TOKEN|--token-file PATH] --client-id ID --owner USER [OPTIONS]\n\n\
     Options:\n\
       --server-url URL           WebCodex server URL\n\
       --token TOKEN              Runner transport token for generated config\n\
       --token-file PATH          Read Runner transport token from file\n\
       --client-id ID             Stable Runner client id\n\
       --profile NAME             Client config profile [default: client-id when deriving defaults]\n\
       --owner USER               Owner username\n\
       --display-name NAME        Human-readable Runner name\n\
       --transport NAME           websocket (default), polling, quic, or auto\n\
       --poll-interval-ms N       Minimum idle polling interval; default 1000, max 30000 for polling/auto\n\
       --project-registry-dir PATH  Runner project registry directory [default: profile project-registry]\n\
       --projects-dir PATH        Deprecated legacy alias for --project-registry-dir\n\
       --allowed-root PATH        Allowed project/root path; repeatable\n\
       --allow-cwd-anywhere BOOL  Allow cwd outside allowed_roots; default false\n\
       --output PATH|-            Output config path, or '-' for stdout [default: profile runner.toml]\n\
       --overwrite                Replace an existing output file\n\
       -h, --help                 Print help and exit\n\n\
     With --profile, missing output/project-registry paths are derived under\n\
     /etc/webcodex/clients/<profile> for root or\n\
     ~/.config/webcodex/clients/<profile> for non-root users. Explicit path\n\
     flags override profile-derived defaults.\n"
}

pub(crate) fn runner_install_service_usage() -> &'static str {
    "Usage: webcodex runner install [--profile NAME] [--config PATH] [OPTIONS]\n\n\
Options:\n\
  --profile NAME             Profile for config and unit defaults\n\
  --scope user|system        Service manager scope [default: user for non-root; system for root]\n\
  --config PATH              Runner config path\n\
  --bin PATH                 webcodex-runner path; sibling then absolute PATH by default\n\
  --service-file PATH        Unit path [default: webcodex-runner[-<profile>].service]\n\
  --working-directory PATH   WorkingDirectory= [default: selected user's home]\n\
  --user USER                Required non-root system service user\n\
  --group GROUP              Optional system service Group=\n\
  --allow-root-runner        Explicitly allow project commands to run as root\n\
  --overwrite                Replace an existing unit\n\
  --no-start                 Enable without starting immediately\n\
  --dry-run                  Render only; never call systemctl\n\
  --output -                 Render only; never call systemctl\n\
  --json                     Print machine-readable output\n\
  -h, --help                 Print help and exit\n\n\
User scope uses systemctl --user, the XDG user unit directory, default.target,\n\
and never writes User= or Group=. System scope uses /etc/systemd/system,\n\
multi-user.target, and requires --user unless --allow-root-runner is explicit.\n\
The unit runs webcodex-runner --config <config>. Tokens are never inlined.\n"
}
pub(crate) fn runner_status_usage() -> &'static str {
    "Usage: webcodex runner status [OPTIONS]\n\n\
     Options:\n\
       --profile NAME             Client config profile for config/token defaults\n\
       --scope user|system        Service manager scope [default: user for non-root; system for root]\n\
       --config PATH              Runner config path [default: scope-specific runner.toml]\n\
       --service-file PATH        Override the scope-specific systemd unit path\n\
       --server-url URL           Override server URL for runtime checks\n\
       --proxy http://HOST:PORT  Explicit proxy override for Server checks\n\
       --no-system-proxy         Ignore proxy environment and connect directly\n\
       --user-token-file PATH     Read user API token for /api/runtime/status\n\
       --runner-token-file PATH   Read Runner transport token for boundary check\n\
       --json                     Print a machine-readable summary\n\
       -h, --help                 Print help and exit\n\n\
     User scope derives config under $XDG_CONFIG_HOME/webcodex (or\n\
     $HOME/.config/webcodex) and units under $XDG_CONFIG_HOME/systemd/user\n\
     (or $HOME/.config/systemd/user). System scope uses /etc/webcodex and\n\
     /etc/systemd/system. Explicit path flags override profile-derived defaults.\n\
     Profiles created by `connect` report their detached process when --scope\n\
     is omitted; an explicit scope checks systemd instead. Status prints safe metadata only:\n\
     no tokens, Authorization headers, full Runner config, env files, or secrets.\n"
}

pub(crate) fn login_usage() -> &'static str {
    "Usage: webcodex login <SERVER-URL> (--code <PAIRING-CODE>|--code-stdin) [OPTIONS]\n\n\
     Use a one-time login code (`webcodex pairing create`) to connect this project machine.\n\
     Add --project to add an existing project during the same login.\n\n\
     Options:\n\
     \x20\x20--code CODE          Pairing code from the server\n\
     \x20\x20--code-stdin         Read the pairing code from bounded UTF-8 stdin; avoids argv exposure\n\
     \x20\x20--proxy http://HOST:PORT Explicit proxy override for this CLI request\n\
     \x20\x20--no-system-proxy   Ignore proxy environment and connect directly\n\
     \x20\x20--device NAME        Name for this device [default: hostname + local suffix]\n\
     \x20\x20--allowed-root PATH  Repeatable location under which projects may be added later\n\
     \x20\x20--project PATH       Existing workspace to register with this login\n\
     \x20\x20--transport NAME     websocket|polling|quic|auto [default: websocket]\n\
     \x20\x20--dir PATH           Where connections are stored [default: root /etc/webcodex;\n\
     \x20\x20                       non-root ~/.config/webcodex]\n\
     \x20\x20--overwrite          Replace an existing login for this server and user\n\
     \x20\x20--json               Print machine-readable output (no credentials)\n\
     \x20\x20--print-mcp-config   Print a Bearer MCP connection block (includes a\n\
     \x20\x20                       credential); mutually exclusive with --json\n\
     \x20\x20-h, --help           Print help and exit\n\n\
     Credentials are written to <dir>/<server>/<user>/ with restrictive\n\
     permissions (0600 on Unix).\n\
     The same user can be logged in on several servers, and several users can\n\
     be logged in on one server; each is a separate directory.\n"
}

pub(crate) fn logout_usage() -> &'static str {
    "Usage: webcodex logout <SERVER-URL> [OPTIONS]\n\n\
     Remove this device's stored credentials for a server.\n\n\
     Options:\n\
     \x20\x20--user NAME    Log out one saved user\n\
     \x20\x20--all          Log out every saved user on this server (mutually exclusive with --user)\n\
     \x20\x20--dir PATH     Where connections are stored [default: ~/.config/webcodex]\n\
     \x20\x20-y, --yes      Confirm removal\n\
     \x20\x20--json         Print machine-readable output\n\
     \x20\x20-h, --help     Print help and exit\n\n\
     With one saved user, no selector is needed. With several saved users, choose\n\
     --user NAME or --all. Without --yes this only reports what would be removed.\n"
}

pub(crate) fn status_usage() -> &'static str {
    "Usage: webcodex auth status [OPTIONS]\n\n\
     Show which servers this device is logged in to.\n\n\
     Options:\n\
     \x20\x20--dir PATH     Where connections are stored [default: ~/.config/webcodex]\n\
     \x20\x20--json         Print machine-readable output\n\
     \x20\x20-h, --help     Print help and exit\n"
}
