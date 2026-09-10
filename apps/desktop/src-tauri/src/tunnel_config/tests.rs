use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "webcodex-tunnel-settings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("secrets")).unwrap();
        Self(path)
    }
    fn path(&self) -> std::path::PathBuf {
        self.0.join("secrets").join("tunnel-config.json")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn save(id: &str, key: Option<&str>) -> TunnelConfigRequest {
    TunnelConfigRequest::Save {
        tunnel_id: id.into(),
        api_key: key.map(str::to_owned),
    }
}

#[test]
fn saved_pair_roundtrips_overrides_environment_and_never_projects_key() {
    let fixture = Fixture::new();
    let mut config = TunnelConfig::default();
    config
        .update(
            &fixture.path(),
            save("tunnel_saved", Some("test-only-api-key")),
        )
        .unwrap();
    let mut config = TunnelConfig::load(&fixture.path());
    assert_eq!(config.snapshot().source, TunnelConfigSource::File);
    assert_eq!(
        config.snapshot().effective_tunnel_id.as_deref(),
        Some("tunnel_saved")
    );
    let projected = serde_json::to_string(&config.snapshot()).unwrap();
    assert!(!projected.contains("test-only-api-key"));
    let mut command = Command::new("unused");
    command
        .env("CONTROL_PLANE_TUNNEL_ID", "tunnel_environment")
        .env("CONTROL_PLANE_API_KEY", "environment-key");
    config.apply_to_command(&mut command).unwrap();
    let env: std::collections::HashMap<_, _> = command.get_envs().collect();
    assert_eq!(
        env[std::ffi::OsStr::new("CONTROL_PLANE_TUNNEL_ID")].unwrap(),
        "tunnel_saved"
    );
    assert_eq!(
        env[std::ffi::OsStr::new("CONTROL_PLANE_API_KEY")].unwrap(),
        "test-only-api-key"
    );
    config
        .update(&fixture.path(), save("tunnel_changed", None))
        .unwrap();
    assert_eq!(
        TunnelConfig::load(&fixture.path()).saved.unwrap().api_key,
        "test-only-api-key"
    );
    config
        .update(&fixture.path(), TunnelConfigRequest::UseEnvironment)
        .unwrap();
    assert_eq!(
        TunnelConfig::load(&fixture.path()).snapshot().source,
        TunnelConfigSource::Environment
    );
    assert_eq!(std::fs::read_to_string(fixture.path()).unwrap(), "null");
    assert_eq!(
        std::fs::read_dir(fixture.0.join("secrets"))
            .unwrap()
            .count(),
        1,
        "no secret backup"
    );
}

#[test]
fn invalid_or_unreadable_saved_config_fails_closed_and_can_be_repaired() {
    let fixture = Fixture::new();
    std::fs::write(fixture.path(), b"not-json-with-test-secret").unwrap();
    let mut config = TunnelConfig::load(&fixture.path());
    assert_eq!(config.snapshot().source, TunnelConfigSource::Invalid);
    assert!(!config.snapshot().is_configured());
    let error = config
        .apply_to_command(&mut Command::new("unused"))
        .unwrap_err();
    assert!(!error.to_string().contains("test-secret"));
    config
        .update(&fixture.path(), save("tunnel_fixed", Some("test-key")))
        .unwrap();
    assert!(TunnelConfig::load(&fixture.path())
        .snapshot()
        .is_configured());
    std::fs::write(fixture.path(), vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
    assert_eq!(
        TunnelConfig::load(&fixture.path()).snapshot().source,
        TunnelConfigSource::Invalid
    );
}

#[test]
fn validation_and_failed_writes_keep_previous_saved_pair() {
    let fixture = Fixture::new();
    let mut config = TunnelConfig::default();
    assert!(config
        .update(&fixture.path(), save("tunnel_one", None))
        .is_err());
    config
        .update(&fixture.path(), save("tunnel_one", Some("old-test-key")))
        .unwrap();
    for (id, key) in [("invalid/id", "key"), ("tunnel_one", "invalid\nkey")] {
        let error = config
            .update(&fixture.path(), save(id, Some(key)))
            .unwrap_err();
        assert_eq!(error.code, "tunnel_config_invalid");
    }
    let escaped_key = "\\".repeat(8192);
    assert!(config
        .update(&fixture.path(), save("tunnel_one", Some(&escaped_key)))
        .is_err());
    let blocked = fixture.0.join("directory");
    std::fs::create_dir(&blocked).unwrap();
    assert!(config
        .update(&blocked, save("tunnel_two", Some("new-test-key")))
        .is_err());
    assert_eq!(
        config.snapshot().saved_tunnel_id.as_deref(),
        Some("tunnel_one")
    );
    assert_eq!(
        TunnelConfig::load(&fixture.path()).saved.unwrap().api_key,
        "old-test-key"
    );
}

#[cfg(unix)]
#[test]
fn credential_file_is_private_and_symlinks_are_not_loaded() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let fixture = Fixture::new();
    let mut config = TunnelConfig::default();
    config
        .update(&fixture.path(), save("tunnel_one", Some("test-key")))
        .unwrap();
    assert_eq!(
        std::fs::metadata(fixture.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let link = fixture.0.join("link.json");
    symlink(fixture.path(), &link).unwrap();
    assert_eq!(
        TunnelConfig::load(&link).snapshot().source,
        TunnelConfigSource::Invalid
    );
}

#[tokio::test]
async fn saved_settings_survive_restart_and_published_state_never_uses_environment_over_them() {
    let fixture = Fixture::new();
    let app = crate::state::AppState::new(fixture.0.clone(), fixture.0.join("resources")).unwrap();
    let snapshot = app
        .update_tunnel_config(save("tunnel_persisted", Some("test-only-secret")))
        .await
        .unwrap();
    assert_eq!(
        snapshot.openai_tunnel_config.source,
        TunnelConfigSource::File
    );
    assert!(app.get_state().openai_tunnel_configured);
    assert_eq!(
        app.get_state()
            .openai_tunnel_config
            .saved_tunnel_id
            .as_deref(),
        Some("tunnel_persisted")
    );
    let serialized = serde_json::to_string(&app.get_state()).unwrap();
    assert!(!serialized.contains("test-only-secret"));
    assert!(!serde_json::to_string(&app.activity())
        .unwrap()
        .contains("test-only-secret"));
    let restarted =
        crate::state::AppState::new(fixture.0.clone(), fixture.0.join("resources")).unwrap();
    assert_eq!(
        restarted.get_state().openai_tunnel_config.source,
        TunnelConfigSource::File
    );
    assert!(restarted.get_state().openai_tunnel_configured);
    restarted
        .update_tunnel_config(TunnelConfigRequest::UseEnvironment)
        .await
        .unwrap();
    assert_eq!(
        restarted.get_state().openai_tunnel_config.source,
        TunnelConfigSource::Environment
    );
}
