use crate::{project_entry, task_cli};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegularServerTunnelOptions {
    pub local_server_url: String,
    pub bootstrap_token: String,
    pub runtime_parent: PathBuf,
}

pub async fn run_regular_server_tunnel(options: RegularServerTunnelOptions) -> Result<(), String> {
    project_entry::run_regular_server_tunnel(&project_entry::RegularServerTunnelOptions {
        local_server_url: options.local_server_url,
        bootstrap_token: options.bootstrap_token,
        runtime_parent: options.runtime_parent,
    })
    .await
    .map_err(|error| project_entry::render_error(&error, true))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProjectCliAction {
    Setup(project_entry::ProjectCommandOptions),
    Doctor(project_entry::ProjectCommandOptions),
    Status(project_entry::ProjectCommandOptions),
    Run(project_entry::ProjectCommandOptions),
    Share(project_entry::ShareCommandOptions),
    Task(task_cli::TaskCliCommand),
    Exit {
        code: i32,
        stdout: String,
        stderr: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliCommandOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CliCommandOutput {
    fn success(stdout: String) -> Self {
        Self {
            code: 0,
            stdout,
            stderr: String::new(),
        }
    }

    fn failure(code: i32, stderr: String) -> Self {
        Self {
            code,
            stdout: String::new(),
            stderr,
        }
    }
}

pub fn is_project_command(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("setup" | "doctor" | "status" | "run" | "share" | "task")
    )
}

pub(crate) fn project_cli_action<I, S>(args: I) -> ProjectCliAction
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_string())
        .collect();
    let Some(command) = args.first().map(String::as_str) else {
        return ProjectCliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: "missing project command\n".to_string(),
        };
    };

    if matches!(command, "setup" | "doctor" | "status" | "run" | "share") {
        if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
            return ProjectCliAction::Exit {
                code: 0,
                stdout: project_entry::usage().to_string(),
                stderr: String::new(),
            };
        }
        let parsed = if command == "share" {
            project_entry::parse_share_options(&args[1..]).map(ProjectCliAction::Share)
        } else {
            project_entry::parse_options(&args[1..], command).map(|options| match command {
                "setup" => ProjectCliAction::Setup(options),
                "doctor" => ProjectCliAction::Doctor(options),
                "status" => ProjectCliAction::Status(options),
                "run" => ProjectCliAction::Run(options),
                _ => unreachable!(),
            })
        };
        return match parsed {
            Ok(action) => action,
            Err(error) => ProjectCliAction::Exit {
                code: 2,
                stdout: String::new(),
                stderr: format!("{error}\n\n{}", project_entry::usage()),
            },
        };
    }

    if command == "task" {
        if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
            return ProjectCliAction::Exit {
                code: 0,
                stdout: task_cli::usage().to_string(),
                stderr: String::new(),
            };
        }
        return match task_cli::parse(&args[1..]) {
            Ok(command) => ProjectCliAction::Task(command),
            Err(error) if error == "help requested" => ProjectCliAction::Exit {
                code: 0,
                stdout: task_cli::usage().to_string(),
                stderr: String::new(),
            },
            Err(error) => ProjectCliAction::Exit {
                code: 2,
                stdout: String::new(),
                stderr: format!("{}\n\n{}", error, task_cli::usage()),
            },
        };
    }

    ProjectCliAction::Exit {
        code: 2,
        stdout: String::new(),
        stderr: format!("unknown project command: {}\n", args.join(" ")),
    }
}

pub async fn run_project_command(args: Vec<String>) -> CliCommandOutput {
    match project_cli_action(args) {
        ProjectCliAction::Setup(options) => match project_entry::setup(&options) {
            Ok(report) if options.json => match serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "command": "setup",
                "project": report.project,
                "connection_url": report.connection_url,
                "status": report.status,
                "changed": report.changed,
                "next_action": report.next_action
            })) {
                Ok(output) => CliCommandOutput::success(format!("{output}\n")),
                Err(error) => CliCommandOutput::failure(1, format!("{error}\n")),
            },
            Ok(report) => CliCommandOutput::success(project_entry::render_setup_text(&report)),
            Err(error) => CliCommandOutput::failure(
                1,
                format!("{}\n", project_entry::render_error(&error, options.json)),
            ),
        },
        ProjectCliAction::Doctor(options) => {
            let readiness = project_entry::collect_readiness(&options).await;
            let code = if readiness.ready { 0 } else { 1 };
            let output = if options.json {
                serde_json::to_string_pretty(&readiness)
                    .map(|value| format!("{value}\n"))
                    .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}\n"))
            } else {
                project_entry::render_doctor_text(&readiness)
            };
            CliCommandOutput {
                code,
                stdout: output,
                stderr: String::new(),
            }
        }
        ProjectCliAction::Status(options) => {
            let readiness = project_entry::collect_readiness(&options).await;
            let code = if readiness.ready { 0 } else { 1 };
            let output = if options.json {
                serde_json::to_string_pretty(&serde_json::json!({
                    "project": readiness.project,
                    "connection": readiness.connection,
                    "agent": readiness.agent,
                    "capabilities": readiness.capabilities,
                    "ready": readiness.ready,
                    "next_action": readiness.next_action
                }))
                .map(|value| format!("{value}\n"))
                .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}\n"))
            } else {
                project_entry::render_status_text(&readiness)
            };
            CliCommandOutput {
                code,
                stdout: output,
                stderr: String::new(),
            }
        }
        ProjectCliAction::Run(options) => match project_entry::start_runner(&options).await {
            Ok(()) => CliCommandOutput::success(String::new()),
            Err(error) => CliCommandOutput::failure(
                1,
                format!("{}\n", project_entry::render_error(&error, false)),
            ),
        },
        ProjectCliAction::Share(options) => match project_entry::share(&options).await {
            Ok(()) => CliCommandOutput::success(String::new()),
            Err(error) => CliCommandOutput::failure(
                1,
                format!(
                    "{}\n",
                    project_entry::render_error(&error, options.project.json)
                ),
            ),
        },
        ProjectCliAction::Task(command) => match task_cli::run(command) {
            Ok(stdout) => CliCommandOutput::success(format!("{stdout}\n")),
            Err(stderr) => CliCommandOutput::failure(1, format!("{stderr}\n")),
        },
        ProjectCliAction::Exit {
            code,
            stdout,
            stderr,
        } => CliCommandOutput {
            code,
            stdout,
            stderr,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_commands_have_one_canonical_dispatch() {
        assert!(matches!(
            project_cli_action(["setup", "--root", "."]),
            ProjectCliAction::Setup(_)
        ));
        assert!(matches!(
            project_cli_action(["doctor", "--root", "."]),
            ProjectCliAction::Doctor(_)
        ));
        assert!(matches!(
            project_cli_action(["status", "--root", "."]),
            ProjectCliAction::Status(_)
        ));
        assert!(matches!(
            project_cli_action(["run", "--root", "."]),
            ProjectCliAction::Run(_)
        ));
        assert!(matches!(
            project_cli_action(["share", "--root", "."]),
            ProjectCliAction::Share(_)
        ));
        assert!(matches!(
            project_cli_action(["share", "--tunnel", "none"]),
            ProjectCliAction::Share(project_entry::ShareCommandOptions {
                tunnel: project_entry::TunnelProvider::None,
                ..
            })
        ));
        assert!(matches!(
            project_cli_action(["share", "--tunnel", "openai"]),
            ProjectCliAction::Share(project_entry::ShareCommandOptions {
                tunnel: project_entry::TunnelProvider::OpenAiSecure,
                ..
            })
        ));
        assert!(matches!(
            project_cli_action(["share", "--tunnel", "none", "--json"]),
            ProjectCliAction::Share(project_entry::ShareCommandOptions {
                project: project_entry::ProjectCommandOptions { json: true, .. },
                tunnel: project_entry::TunnelProvider::None,
                ..
            })
        ));
    }

    #[test]
    fn project_help_is_available_without_server_startup() {
        let ProjectCliAction::Exit {
            code,
            stdout,
            stderr,
        } = project_cli_action(["status", "--help"])
        else {
            panic!("expected help exit");
        };
        assert_eq!(code, 0);
        assert!(stdout.contains("webcodex status"));
        assert!(stderr.is_empty());
        assert!(project_entry::usage().contains("--no-copy-url"));
        let share_help = project_entry::usage();
        assert!(
            share_help.contains("`share` is the Quick Trial path"),
            "{share_help}"
        );
        assert!(
            share_help.contains("ends when the command exits"),
            "{share_help}"
        );
        assert!(share_help.contains("full daily use"), "{share_help}");
        assert!(
            !share_help.contains("`share` is the first-run path"),
            "{share_help}"
        );
    }
}
