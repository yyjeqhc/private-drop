//! Desktop-owned Tunnel credentials. Never project the key into UI state or logs.
use crate::error::{DesktopError, DesktopResult};
use crate::models::{OpenAiTunnelConfigSnapshot, TunnelConfigSource};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::process::Command;

const MAX_CONFIG_BYTES: u64 = 16 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    tunnel_id: String,
    api_key: String,
}

#[derive(Clone, Default)]
pub(crate) struct TunnelConfig {
    saved: Option<Credentials>,
    invalid: bool,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TunnelConfigRequest {
    Save {
        #[serde(rename = "tunnelId")]
        tunnel_id: String,
        #[serde(rename = "apiKey")]
        api_key: Option<String>,
    },
    UseEnvironment,
}

impl TunnelConfig {
    pub fn load(path: &Path) -> Self {
        match Self::read(path) {
            Ok(saved) => Self {
                saved,
                invalid: false,
            },
            Err(_) => Self {
                saved: None,
                invalid: true,
            },
        }
    }

    fn read(path: &Path) -> DesktopResult<Option<Credentials>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(config_error()),
        };
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return Err(config_error());
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|_| config_error())?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| config_error())?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(config_error());
        }
        let saved: Option<Credentials> =
            serde_json::from_slice(&bytes).map_err(|_| config_error())?;
        if let Some(value) = &saved {
            validate(&value.tunnel_id, &value.api_key)?;
        }
        Ok(saved)
    }

    pub fn update(&mut self, path: &Path, request: TunnelConfigRequest) -> DesktopResult<()> {
        let next = match request {
            TunnelConfigRequest::UseEnvironment => None,
            TunnelConfigRequest::Save { tunnel_id, api_key } => {
                let tunnel_id = tunnel_id.trim().to_string();
                let api_key = api_key
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| value.trim().to_string())
                    .or_else(|| self.saved.as_ref().map(|value| value.api_key.clone()))
                    .ok_or_else(config_error)?;
                validate(&tunnel_id, &api_key)?;
                Some(Credentials { tunnel_id, api_key })
            }
        };
        let bytes = serde_json::to_vec_pretty(&next).map_err(|_| config_error())?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(config_error());
        }
        // No backup containing retired credentials. `null` explicitly restores environment fallback.
        crate::state::write_atomic_file(path, &bytes).map_err(|_| {
            DesktopError::new(
                "tunnel_config_save_failed",
                "Could not save the Tunnel configuration",
                "Check access to the Desktop application data directory and retry.",
            )
        })?;
        self.saved = next;
        self.invalid = false;
        Ok(())
    }

    pub fn snapshot(&self) -> OpenAiTunnelConfigSnapshot {
        if self.invalid {
            return OpenAiTunnelConfigSnapshot {
                source: TunnelConfigSource::Invalid,
                ..Default::default()
            };
        }
        if let Some(value) = &self.saved {
            return OpenAiTunnelConfigSnapshot {
                tunnel_id_present: true,
                api_key_present: true,
                source: TunnelConfigSource::File,
                saved_tunnel_id: Some(value.tunnel_id.clone()),
                effective_tunnel_id: Some(value.tunnel_id.clone()),
            };
        }
        environment_snapshot()
    }

    pub fn apply_to_command(&self, command: &mut Command) -> DesktopResult<()> {
        if self.invalid {
            return Err(config_error());
        }
        if let Some(value) = &self.saved {
            command
                .env("CONTROL_PLANE_TUNNEL_ID", &value.tunnel_id)
                .env("CONTROL_PLANE_API_KEY", &value.api_key);
        }
        Ok(())
    }
}

pub(crate) fn environment_snapshot() -> OpenAiTunnelConfigSnapshot {
    OpenAiTunnelConfigSnapshot {
        tunnel_id_present: std::env::var_os("CONTROL_PLANE_TUNNEL_ID")
            .is_some_and(|value| !value.is_empty()),
        api_key_present: std::env::var_os("CONTROL_PLANE_API_KEY")
            .is_some_and(|value| !value.is_empty()),
        source: TunnelConfigSource::Environment,
        saved_tunnel_id: None,
        effective_tunnel_id: std::env::var("CONTROL_PLANE_TUNNEL_ID").ok().filter(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        }),
    }
}

fn validate(tunnel_id: &str, api_key: &str) -> DesktopResult<()> {
    if tunnel_id.is_empty()
        || tunnel_id.len() > 256
        || !tunnel_id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || value == b'_' || value == b'-')
        || api_key.is_empty()
        || api_key.len() > 8192
        || !api_key.bytes().all(|value| value.is_ascii_graphic())
    {
        return Err(config_error());
    }
    Ok(())
}

fn config_error() -> DesktopError {
    DesktopError::new("tunnel_config_invalid", "Tunnel configuration is missing or invalid",
        "Enter a valid Tunnel ID and API key, then save again. Invalid saved configuration never falls back to environment credentials.")
}

#[cfg(test)]
mod tests;
