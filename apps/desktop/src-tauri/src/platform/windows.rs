use super::{normalize_proxy_server, SystemProxyCandidate};
use crate::models::PowerShellRuntimeSnapshot;
use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::process::Command;
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

pub fn powershell_runtime_snapshot() -> PowerShellRuntimeSnapshot {
    let path = std::env::var_os("PATH");
    PowerShellRuntimeSnapshot {
        pwsh_available: program_on_path("pwsh.exe", path.as_deref()),
        windows_powershell_available: program_on_path("powershell.exe", path.as_deref())
            || std::env::var_os("SystemRoot").is_some_and(|system_root| {
                PathBuf::from(system_root)
                    .join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe")
                    .is_file()
            }),
    }
}

fn program_on_path(program: &str, path: Option<&OsStr>) -> bool {
    path.into_iter()
        .flat_map(std::env::split_paths)
        .any(|directory| directory.join(program).is_file())
}

pub fn open_external_url(url: &str) -> io::Result<()> {
    Command::new("explorer.exe").arg(url).spawn().map(|_| ())
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
    fn program_on_path_detects_pwsh_without_process_execution() {
        let root = std::env::temp_dir().join(format!(
            "webcodex-pwsh-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = std::env::join_paths([root.as_path()]).unwrap();
        assert!(!program_on_path("pwsh.exe", Some(path.as_os_str())));
        std::fs::write(root.join("pwsh.exe"), b"").unwrap();
        assert!(program_on_path("pwsh.exe", Some(path.as_os_str())));
        std::fs::remove_dir_all(root).unwrap();
    }

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
