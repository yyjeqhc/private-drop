use super::get_projects;
use super::types::{InstanceInfo, ProjectCapabilities, ProjectCapabilityInfo, ProjectsResponse};
use crate::auth::get_config;
use crate::projects::{Executor, ProjectChecks, ProjectConfig};
use salvo::prelude::*;

fn configured_checks(checks: &Option<ProjectChecks>) -> Vec<String> {
    let Some(checks) = checks else {
        return Vec::new();
    };
    let mut names = Vec::new();
    if checks.fmt.is_some() {
        names.push("fmt".to_string());
    }
    if checks.test.is_some() {
        names.push("test".to_string());
    }
    if checks.build.is_some() {
        names.push("build".to_string());
    }
    if checks.e2e.is_some() {
        names.push("e2e".to_string());
    }
    if checks.full.is_some() {
        names.push("full".to_string());
    }
    names
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

fn instance_info(depot: &Depot) -> InstanceInfo {
    let data_dir = get_config(depot)
        .map(|cfg| cfg.data_dir.display().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    InstanceInfo {
        service: "private-drop".to_string(),
        api: "codex".to_string(),
        schema: "codex-openapi-compact".to_string(),
        package_version: env!("CARGO_PKG_VERSION").to_string(),
        server_time: chrono::Utc::now().timestamp(),
        pid: std::process::id(),
        hostname: hostname(),
        data_dir,
        projects_config_path: std::env::var("PROJECTS_CONFIG")
            .unwrap_or_else(|_| "./projects.toml".to_string()),
        public_url: std::env::var("PUBLIC_URL")
            .ok()
            .filter(|s| !s.trim().is_empty()),
    }
}

fn project_info(name: &str, project: &ProjectConfig) -> ProjectCapabilityInfo {
    let mut commands = project.commands.keys().cloned().collect::<Vec<_>>();
    commands.sort();
    let mut allowed_checks = project.allowed_checks.clone();
    allowed_checks.sort();
    let configured_checks = configured_checks(&project.checks);
    let ssh_target = if project.executor == Executor::Ssh {
        project.ssh_target().ok()
    } else {
        None
    };
    ProjectCapabilityInfo {
        name: name.to_string(),
        executor: match project.executor {
            Executor::Local => "local".to_string(),
            Executor::Ssh => "ssh".to_string(),
        },
        root: project.path.clone(),
        ssh_target,
        allowed_checks,
        configured_checks: configured_checks.clone(),
        commands: commands.clone(),
        default_apply_patch_backend: project
            .default_apply_patch_backend
            .clone()
            .unwrap_or_else(|| "builtin".to_string()),
        capabilities: ProjectCapabilities {
            edit: true,
            patch: project.allow_patch(),
            artifact: true,
            git: true,
            checks: !configured_checks.is_empty() && !project.allowed_checks.is_empty(),
            jobs: true,
            command_requests: project.allow_command_requests,
            raw_command_requests: project.allow_raw_command_requests,
            configured_commands: !commands.is_empty(),
            reports: true,
        },
    }
}

#[handler]
pub async fn codex_projects(depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(ProjectsResponse {
            success: false,
            projects: Vec::new(),
            project_names: Vec::new(),
            instance: Some(instance_info(depot)),
            error: Some("Projects config not loaded".to_string()),
        }));
        return;
    };
    let mut project_names = projects.available_project_names();
    project_names.sort();
    let mut infos = project_names
        .iter()
        .filter_map(|name| projects.projects.get(name).map(|p| project_info(name, p)))
        .collect::<Vec<_>>();
    infos.sort_by(|a, b| a.name.cmp(&b.name));
    res.render(Json(ProjectsResponse {
        success: true,
        projects: infos,
        project_names,
        instance: Some(instance_info(depot)),
        error: None,
    }));
}
