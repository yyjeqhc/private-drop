#[cfg(target_os = "windows")]
mod windows;

use webcodex_process::SpawnOptions;

pub fn managed_spawn_options() -> SpawnOptions {
    #[cfg(target_os = "windows")]
    {
        return windows::managed_spawn_options();
    }
    #[cfg(not(target_os = "windows"))]
    {
        SpawnOptions::new()
    }
}

pub fn current_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "desktop".to_string())
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
