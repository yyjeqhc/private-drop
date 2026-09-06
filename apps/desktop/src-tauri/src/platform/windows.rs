use super::{normalize_proxy_server, SystemProxyCandidate};
use webcodex_process::SpawnOptions;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn managed_spawn_options(silent_child_breakaway: bool) -> SpawnOptions {
    SpawnOptions {
        windows_creation_flags: CREATE_NO_WINDOW,
        windows_silent_child_breakaway: silent_child_breakaway,
    }
}

pub fn system_http_proxy_candidate() -> Option<SystemProxyCandidate> {
    let internet_settings = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let value: String = internet_settings.get_value("ProxyServer").ok()?;
    let url = normalize_proxy_server(&value)?;
    let enabled = internet_settings
        .get_value::<u32, _>("ProxyEnable")
        .ok()
        .is_some_and(|value| value != 0);
    Some(SystemProxyCandidate { url, enabled })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_trusted_supervisor_spawn_enables_silent_child_breakaway() {
        let ordinary = managed_spawn_options(false);
        assert_eq!(ordinary.windows_creation_flags, CREATE_NO_WINDOW);
        assert!(!ordinary.windows_silent_child_breakaway);

        let supervisor = managed_spawn_options(true);
        assert_eq!(supervisor.windows_creation_flags, CREATE_NO_WINDOW);
        assert!(supervisor.windows_silent_child_breakaway);
    }
}
