use super::{normalize_proxy_server, SystemProxyCandidate};
use webcodex_process::SpawnOptions;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn managed_spawn_options() -> SpawnOptions {
    SpawnOptions {
        windows_creation_flags: CREATE_NO_WINDOW,
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
