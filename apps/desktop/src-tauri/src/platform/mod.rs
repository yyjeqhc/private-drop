#[cfg(target_os = "windows")]
mod windows;

use webcodex_process::SpawnOptions;

pub fn managed_spawn_options(silent_child_breakaway: bool) -> SpawnOptions {
    #[cfg(target_os = "windows")]
    {
        return windows::managed_spawn_options(silent_child_breakaway);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = silent_child_breakaway;
        SpawnOptions::new()
    }
}

pub fn current_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .map(|value| normalize_local_username(&value))
        .unwrap_or_else(|| "desktop".to_string())
}

fn normalize_local_username(value: &str) -> String {
    // This is a name for Desktop's locally authorized pairing, not an OS
    // authentication identity. Names the Server already accepts keep their
    // original characters, including every '-' the user already has.
    let trimmed = value.trim();
    if is_server_valid_username(trimmed) {
        return trimmed.to_string();
    }
    // Runs of unsupported characters collapse into one generated separator.
    // The user's own '-' characters are never folded as separators and are
    // not stripped at the edges; the whole result is still capped at 64
    // characters, so an original '-' can still be lost to that truncation.
    #[derive(PartialEq)]
    enum Part {
        Kept(char),
        GeneratedSeparator,
    }
    let mut parts: Vec<Part> = Vec::with_capacity(trimmed.len().min(64));
    for character in trimmed.chars() {
        let lowered = character.to_ascii_lowercase();
        if is_username_character(lowered) {
            parts.push(Part::Kept(lowered));
        } else if parts.last() != Some(&Part::GeneratedSeparator) {
            parts.push(Part::GeneratedSeparator);
        }
    }
    // Only separators generated above may be trimmed at the edges.
    while parts.first() == Some(&Part::GeneratedSeparator) {
        parts.remove(0);
    }
    while parts.last() == Some(&Part::GeneratedSeparator) {
        parts.pop();
    }
    let mut normalized: Vec<Part> = parts.into_iter().take(64).collect();
    while normalized.last() == Some(&Part::GeneratedSeparator) {
        normalized.pop();
    }
    let normalized: String = normalized
        .into_iter()
        .map(|part| match part {
            Part::Kept(character) => character,
            Part::GeneratedSeparator => '-',
        })
        .collect();
    if normalized.is_empty() {
        "desktop".to_string()
    } else {
        normalized
    }
}

fn is_server_valid_username(value: &str) -> bool {
    // Accepts the same usernames as the Server's validate_username:
    // non-empty, at most 64 characters, and only lowercase ASCII letters,
    // digits, '_' and '-'. The Server also rejects '..' explicitly, which
    // this character set cannot produce.
    !value.is_empty() && value.chars().count() <= 64 && value.chars().all(is_username_character)
}

fn is_username_character(character: char) -> bool {
    character.is_ascii_lowercase() || character.is_ascii_digit() || matches!(character, '_' | '-')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemProxyCandidate {
    pub url: String,
    pub enabled: bool,
}

pub fn system_http_proxy_candidate() -> Option<SystemProxyCandidate> {
    #[cfg(target_os = "windows")]
    {
        return windows::system_http_proxy_candidate();
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

pub(crate) fn normalize_proxy_server(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let selected = if value.contains('=') {
        let entries = value
            .split(';')
            .filter_map(|entry| entry.split_once('='))
            .map(|(scheme, target)| (scheme.trim().to_ascii_lowercase(), target.trim()))
            .collect::<Vec<_>>();
        entries
            .iter()
            .find(|(scheme, _)| scheme == "https")
            .or_else(|| entries.iter().find(|(scheme, _)| scheme == "http"))?
            .1
    } else {
        value
    };
    let candidate = if selected.contains("://") {
        selected.to_string()
    } else {
        format!("http://{selected}")
    };
    let parsed = url::Url::parse(&candidate).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.host_str().is_none()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return None;
    }
    Some(candidate.trim_end_matches('/').to_string())
}

pub(crate) fn proxy_is_loopback(url: &str) -> bool {
    url::Url::parse(url).ok().and_then(|parsed| {
        parsed.host_str().map(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host == "127.0.0.1"
                || host == "::1"
                || host == "[::1]"
        })
    }) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_username_normalization_matches_server_username_rules() {
        // Already-valid names pass through unchanged.
        assert_eq!(normalize_local_username("alice"), "alice");
        assert_eq!(normalize_local_username("alice--dev"), "alice--dev");
        assert_eq!(normalize_local_username("alice_dev"), "alice_dev");
        assert_eq!(normalize_local_username("alice-"), "alice-");
        assert_eq!(normalize_local_username("-alice"), "-alice");
        assert_eq!(normalize_local_username("valid_name-2"), "valid_name-2");
        // Case is folded to lowercase; surrounding whitespace is trimmed.
        assert_eq!(normalize_local_username("Alice"), "alice");
        assert_eq!(normalize_local_username("  alice\t"), "alice");
        // Unsupported characters collapse into one generated '-' per run
        // while the user's own '-' characters stay untouched.
        assert_eq!(normalize_local_username("Alice Smith"), "alice-smith");
        assert_eq!(normalize_local_username("Alice  Smith"), "alice-smith");
        assert_eq!(normalize_local_username("alice.smith"), "alice-smith");
        assert_eq!(
            normalize_local_username("DOMAIN\\Jane Doe"),
            "domain-jane-doe"
        );
        assert_eq!(normalize_local_username("alice..dev"), "alice-dev");
        assert_eq!(normalize_local_username("alice.-dev"), "alice--dev");
        assert_eq!(normalize_local_username("alice-."), "alice-");
        // The result always fits the Server's 64-character limit.
        assert_eq!(normalize_local_username(&"A".repeat(65)), "a".repeat(64));
        // Names with nothing supported left fall back explicitly.
        assert_eq!(normalize_local_username("用户"), "desktop");
        assert_eq!(normalize_local_username(""), "desktop");
        assert_eq!(normalize_local_username(" \t "), "desktop");
    }

    #[test]
    fn local_username_normalization_preserves_existing_valid_names() {
        for name in [
            "alice",
            "alice--dev",
            "alice-",
            "-alice",
            "_",
            "--",
            "user_123",
        ] {
            assert_eq!(normalize_local_username(name), name);
        }
        let maximum_length = "a".repeat(64);
        assert_eq!(normalize_local_username(&maximum_length), maximum_length);
    }

    #[test]
    fn proxy_server_normalization_accepts_common_windows_shapes() {
        assert_eq!(
            normalize_proxy_server("127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            normalize_proxy_server("http=127.0.0.1:7890;https=127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            normalize_proxy_server("http=127.0.0.1:7890;https=127.0.0.1:7891").as_deref(),
            Some("http://127.0.0.1:7891")
        );
        assert!(proxy_is_loopback("http://127.0.0.1:7890"));
        assert!(!proxy_is_loopback("http://proxy.example.test:8080"));
    }

    #[test]
    fn proxy_server_normalization_rejects_credentials_and_non_http_schemes() {
        assert!(normalize_proxy_server("http://user:secret@127.0.0.1:7890").is_none());
        assert!(normalize_proxy_server("socks5://127.0.0.1:7890").is_none());
    }
}
