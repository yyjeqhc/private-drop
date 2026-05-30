use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    pub data_dir: PathBuf,
    pub token: Option<String>,
    pub max_text_size: usize,
    pub max_file_size: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct EnvFileLoad {
    pub(crate) path: PathBuf,
    pub(crate) loaded_count: usize,
}

pub(crate) fn parse_env_file_line(line: &str) -> Option<Result<(String, String), String>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim();
    let Some((key, value)) = line.split_once('=') else {
        return Some(Err("missing '='".to_string()));
    };
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Some(Err(format!("invalid env key '{}'", key)));
    }
    let value = value.trim();
    let value = if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    };
    Some(Ok((key.to_string(), value)))
}

fn load_env_file(path: &Path) -> Result<EnvFileLoad, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read env file {}: {}", path.display(), e))?;
    let mut loaded_count = 0;
    for (idx, line) in content.lines().enumerate() {
        let Some(parsed) = parse_env_file_line(line) else {
            continue;
        };
        let (key, value) = parsed.map_err(|e| {
            format!(
                "failed to parse env file {} line {}: {}",
                path.display(),
                idx + 1,
                e
            )
        })?;
        if std::env::var_os(&key).is_none() {
            std::env::set_var(&key, value);
            loaded_count += 1;
        }
    }
    Ok(EnvFileLoad {
        path: path.to_path_buf(),
        loaded_count,
    })
}

pub(crate) fn load_startup_env_files() -> Result<Vec<EnvFileLoad>, String> {
    if let Ok(path) = std::env::var("DROP_ENV_FILE") {
        return Ok(vec![load_env_file(Path::new(&path))?]);
    }
    let candidates = [
        PathBuf::from("./private-drop.env"),
        PathBuf::from("/opt/private-drop/private-drop.env"),
        PathBuf::from("/etc/private-drop/private-drop.env"),
    ];
    let mut loaded = Vec::new();
    for path in candidates {
        if path.exists() {
            loaded.push(load_env_file(&path)?);
        }
    }
    Ok(loaded)
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            addr: std::env::var("DROP_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            data_dir: std::env::var("DROP_DATA")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./data")),
            token: std::env::var("DROP_TOKEN").ok(),
            max_text_size: 2 * 1024 * 1024,
            max_file_size: 100 * 1024 * 1024,
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("drop.db")
    }

    pub fn uploads_dir(&self) -> PathBuf {
        self.data_dir.join("uploads")
    }

    pub fn is_auth_enabled(&self) -> bool {
        self.token.is_some()
    }

    pub fn validate_token(&self, token: &str) -> bool {
        self.token.as_ref().map(|t| t == token).unwrap_or(false)
    }
}
