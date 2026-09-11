//! Shared workspace path policy used by always-on inspection and optional snapshots.

/// Return true for project-relative paths that must be excluded from broad
/// workspace inspection/snapshot surfaces. This intentionally preserves the
/// historical workspace-checkpoint predicate, which is broader than the core
/// secret-path classifier because it also excludes bulk/private components.
pub fn sensitive_path(path: &str) -> bool {
    let parts = path
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>();
    for part in parts {
        if matches!(
            part.as_str(),
            ".git"
                | "target"
                | "node_modules"
                | "project-registry"
                | "projects.d"
                | "runner.toml"
                | "agent.toml"
                | "webcodex.env"
                | ".env"
                | ".npmrc"
                | ".netrc"
                | "secrets"
                | "secret"
                | "tokens"
                | "token"
                | "credentials"
                | "credential"
                | "passwords"
                | "password"
        ) {
            return true;
        }
        if part.starts_with(".env")
            || part.starts_with("runner.toml")
            || part.starts_with("agent.toml")
            || part.starts_with("webcodex.env")
        {
            return true;
        }
        if part == "id_rsa"
            || part == "id_ed25519"
            || part.ends_with(".pem")
            || part.ends_with(".key")
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::sensitive_path;

    #[test]
    fn preserves_workspace_private_and_bulk_path_policy() {
        for path in [
            ".git/config",
            "target/debug/app",
            "node_modules/pkg/index.js",
            "project-registry/demo.toml",
            "projects.d/demo.toml",
            "runner.toml",
            "nested/.env.local",
            "keys/id_ed25519",
            "certs/client.pem",
            "secrets/token.txt",
        ] {
            assert!(sensitive_path(path), "{path}");
            assert!(
                crate::project_overview::normalize_project_overview_path(path).is_err(),
                "{path}"
            );
        }
        assert!(!sensitive_path("src/lib.rs"));
        assert!(!sensitive_path("docs/architecture.md"));
    }
}
