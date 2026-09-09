//! Shared project-path sensitivity policy.
//!
//! Four separate predicates used to encode this, one per surface (search,
//! search globs, artifacts, edits). They disagreed in ways that mattered: the
//! edit path did not stop `*.pem`/`*.key`, the artifact path did not stop
//! Runner config files, and the search paths were case-sensitive, so a file
//! literally named `.ENV` was excluded from one surface but not another.
//!
//! The policy is split along the two distinct jobs those predicates were doing:
//!
//! - [`is_secret_path`] — content that must not be read or written through the
//!   tool surface at all (credentials, keys, agent configuration, Git control data).
//! - [`is_bulk_excluded_path`] — high-volume, low-signal trees that search and
//!   listing skip for noise and cost reasons. These are *not* secret;
//!   `read_file` of a specific path inside them stays allowed.
//!
//! Both match on whole path components and are case-insensitive, so `.ENV` and
//! `ID_RSA.PEM` cannot slip past on a case-preserving filesystem.

/// Credential trees and Git integrity-sensitive control data.
const SECRET_COMPONENTS: &[&str] = &[
    ".git",
    "secrets",
    "tokens",
    "project-registry",
    "projects.d",
];

/// Component prefixes that mark a credential or Runner-config file.
///
/// `.env` as a prefix also covers `.env.local`, `.env.production`, and the
/// like. Runner config names and `webcodex.env` are prefixes so editor and
/// backup suffixes (`runner.toml.swp`, `agent.toml.bak`) remain protected.
const SECRET_PREFIXES: &[&str] = &[".env", "runner.toml", "agent.toml", "webcodex.env"];

/// Component suffixes that mark key material or a credential backup.
const SECRET_SUFFIXES: &[&str] = &[".pem", ".key", ".env", ".toml.bak"];

/// High-volume trees that search and listing skip. Not secret.
const BULK_COMPONENTS: &[&str] = &["target", "node_modules"];

/// True when any component names credentials, key material, Runner configuration
/// or Git integrity-sensitive control data. Deny both reads and writes for these.
pub fn is_secret_path(path: &str) -> bool {
    path_components(path).any(|component| {
        SECRET_COMPONENTS.contains(&component.as_str())
            || SECRET_PREFIXES
                .iter()
                .any(|prefix| component.starts_with(prefix))
            || SECRET_SUFFIXES
                .iter()
                .any(|suffix| component.ends_with(suffix))
    })
}

/// True when any component of `path` names a high-volume tree that bulk
/// operations (search, recursive listing) skip. These are not secret — a
/// direct `read_file` of a path inside them is still allowed.
pub fn is_bulk_excluded_path(path: &str) -> bool {
    path_components(path).any(|component| BULK_COMPONENTS.contains(&component.as_str()))
}

/// True when a bulk operation should skip `path`, for either reason.
pub fn is_bulk_skipped_path(path: &str) -> bool {
    is_secret_path(path) || is_bulk_excluded_path(path)
}

/// True when a literal (non-wildcard) component of an include glob names a
/// protected path, i.e. the caller is explicitly reaching for something the
/// bulk surfaces would otherwise skip.
///
/// Components containing glob metacharacters are ignored: `*.rs` must not be
/// read as an attempt to target `*.key`. A whole-glob match is checked first so
/// that patterns like `**/.env` are caught even though `**` is a wildcard.
pub fn glob_targets_protected_path(glob: &str) -> bool {
    let normalized = glob.strip_prefix("./").unwrap_or(glob);
    let lowered = normalized.to_lowercase();
    let last = lowered.rsplit('/').next().unwrap_or(&lowered);
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| last.starts_with(prefix))
        || SECRET_SUFFIXES.iter().any(|suffix| last.ends_with(suffix))
    {
        return true;
    }
    lowered
        .split('/')
        .filter(|component| !component.contains(['*', '?', '[', ']']))
        .any(|component| {
            SECRET_COMPONENTS.contains(&component)
                || BULK_COMPONENTS.contains(&component)
                || SECRET_PREFIXES
                    .iter()
                    .any(|prefix| component.starts_with(prefix))
                || SECRET_SUFFIXES
                    .iter()
                    .any(|suffix| component.ends_with(suffix))
        })
}

fn path_components(path: &str) -> impl Iterator<Item = String> + '_ {
    path.split(['/', '\\'])
        .filter(|component| !component.is_empty() && *component != "." && *component != "..")
        .map(str::to_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_paths_cover_every_rule_the_four_predicates_had_between_them() {
        for path in [
            ".git/config",
            ".git/HEAD",
            // exact credential directories
            "secrets/key.txt",
            "tokens/agent",
            "project-registry/demo.toml",
            "projects.d/demo.toml",
            // Runner configuration, canonical and legacy
            "runner.toml",
            "config/runner.toml",
            "agent.toml",
            "config/agent.toml",
            "webcodex.env",
            // dotenv family
            ".env",
            ".env.local",
            "app/.env.production",
            // key material — the edit path used to miss these entirely
            "certs/server.pem",
            "certs/server.key",
            // credential backups
            "runner.toml.bak",
            "agent.toml.bak",
            "deploy.env",
        ] {
            assert!(is_secret_path(path), "expected secret: {path}");
        }
    }

    #[test]
    fn secret_matching_is_case_insensitive() {
        // The search predicates used to be case-sensitive, so a file named
        // `.ENV` was protected on some surfaces and exposed on others.
        for path in [
            ".ENV",
            "Certs/Server.PEM",
            "SECRETS/token",
            "Runner.TOML",
            "Agent.TOML",
        ] {
            assert!(is_secret_path(path), "expected secret: {path}");
        }
    }

    #[test]
    fn ordinary_project_files_are_not_secret() {
        for path in [
            "src/main.rs",
            "README.md",
            "Cargo.toml",
            "docs/environment.md",
            "keyboard.rs",
            "src/envelope.rs",
        ] {
            assert!(!is_secret_path(path), "unexpected secret: {path}");
        }
    }

    #[test]
    fn bulk_trees_are_skipped_but_not_secret() {
        // Reading a specific file inside these stays allowed; only bulk
        // operations skip them.
        for path in ["target/debug/app", "node_modules/pkg/i.js"] {
            assert!(is_bulk_excluded_path(path), "expected bulk: {path}");
            assert!(!is_secret_path(path), "must not be secret: {path}");
            assert!(is_bulk_skipped_path(path));
        }
    }

    #[test]
    fn traversal_components_do_not_mask_a_secret_tail() {
        assert!(is_secret_path("../.env"));
        assert!(is_secret_path("./secrets/key"));
        assert!(is_secret_path("a/../.env"));
    }

    #[test]
    fn globs_targeting_protected_paths_are_detected() {
        for glob in [
            ".env",
            "**/.env",
            ".env.*",
            "**/.env.*",
            "agent.toml",
            "**/agent.toml",
            "runner.toml",
            "**/runner.toml",
            "*.pem",
            "**/*.pem",
            "*.key",
            "**/*.KEY",
            "secrets/**",
            "project-registry/**",
            "projects.d/**",
            "./**/.env",
        ] {
            assert!(
                glob_targets_protected_path(glob),
                "expected protected: {glob}"
            );
        }
    }

    #[test]
    fn ordinary_globs_are_not_treated_as_protected() {
        for glob in ["**/*.rs", "src/**", "*.md", "docs/**/*.txt", "**/mod.rs"] {
            assert!(
                !glob_targets_protected_path(glob),
                "unexpected protected: {glob}"
            );
        }
    }
}
