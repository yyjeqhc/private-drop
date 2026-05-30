use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Executor {
    #[default]
    Local,
    Ssh,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProjectChecks {
    pub fmt: Option<String>,
    pub test: Option<String>,
    pub build: Option<String>,
    pub e2e: Option<String>,
    pub full: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProjectConfig {
    pub path: String,
    #[serde(default)]
    pub executor: Executor,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    pub allow_patch: bool,
    #[serde(default)]
    pub allow_command_requests: bool,
    pub allowed_checks: Vec<String>,
    pub checks: Option<ProjectChecks>,
    #[serde(default)]
    pub commands: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SshConfig {
    #[serde(default)]
    pub batch_mode: bool,
    #[serde(default)]
    pub connect_timeout_secs: Option<u64>,
    #[serde(default)]
    pub control_master: bool,
    #[serde(default)]
    pub control_persist: Option<String>,
    #[serde(default)]
    pub control_path: Option<String>,
    #[serde(default)]
    pub server_alive_interval: Option<u64>,
    #[serde(default)]
    pub server_alive_count_max: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ProjectsConfig {
    #[serde(default)]
    pub ssh: Option<SshConfig>,
    pub projects: HashMap<String, ProjectConfig>,
}

impl ProjectsConfig {
    pub fn load() -> Result<Self, String> {
        let config_path =
            std::env::var("PROJECTS_CONFIG").unwrap_or_else(|_| "./projects.toml".to_string());
        let path = Path::new(&config_path);
        if !path.exists() {
            return Err(format!(
                "Projects config not found at '{}'. Set PROJECTS_CONFIG env var or create projects.toml",
                config_path
            ));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read projects config: {}", e))?;
        toml::from_str(&content).map_err(|e| format!("Failed to parse projects config: {}", e))
    }

    pub fn get_project(&self, name: &str) -> Result<&ProjectConfig, String> {
        self.projects.get(name).ok_or_else(|| {
            format!(
                "Unknown project '{}'. Available: {}",
                name,
                self.projects.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }
}

impl ProjectConfig {
    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    pub fn allow_patch(&self) -> bool {
        self.allow_patch
    }

    pub fn is_check_allowed(&self, suite: &str) -> bool {
        self.allowed_checks.iter().any(|c| c == suite)
    }

    pub fn get_check_command(&self, suite: &str) -> Result<String, String> {
        let checks = self
            .checks
            .as_ref()
            .ok_or("No checks configured for this project")?;
        match suite {
            "fmt" => checks.fmt.clone(),
            "test" => checks.test.clone(),
            "build" => checks.build.clone(),
            "e2e" => checks.e2e.clone(),
            "full" => checks.full.clone(),
            _ => None,
        }
        .ok_or_else(|| format!("Check '{}' has no command configured", suite))
    }

    pub fn is_ssh(&self) -> bool {
        self.executor == Executor::Ssh
    }

    /// Build SSH target: [user@]host
    pub fn ssh_target(&self) -> Result<String, String> {
        let host = self
            .host
            .as_ref()
            .ok_or("SSH executor requires 'host' in projects.toml")?;
        match &self.user {
            Some(user) => Ok(format!("{}@{}", user, host)),
            None => Ok(host.clone()),
        }
    }
}

pub fn canonicalize_and_verify(path: &Path, project_root: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("Path does not exist: {}", e))?;
    let canonical_root = project_root
        .canonicalize()
        .map_err(|e| format!("Project root does not exist: {}", e))?;
    if !canonical.starts_with(&canonical_root) {
        return Err("Path is outside project directory".to_string());
    }
    Ok(canonical)
}
