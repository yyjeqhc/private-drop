use super::env::read_env_file_value;
use std::path::{Path, PathBuf};

pub(crate) fn write_text_file(
    path: &Path,
    content: &str,
    overwrite: bool,
    secret: bool,
) -> Result<(), String> {
    if path.exists() && !overwrite {
        return Err(format!(
            "{} already exists; pass --overwrite to replace it",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        if overwrite {
            options.create(true).truncate(true);
        } else {
            options.create_new(true);
        }
        if secret {
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        use std::io::Write;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        if secret {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("failed to set permissions on {}: {}", path.display(), e))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = secret;
        if overwrite {
            std::fs::write(path, content)
                .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        } else {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
            use std::io::Write;
            file.write_all(content.as_bytes())
                .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        }
    }
    Ok(())
}

pub(crate) fn write_server_secret_text_file(
    path: &Path,
    content: &str,
    overwrite: bool,
) -> Result<(), String> {
    #[cfg(windows)]
    {
        return write_windows_secret_text_file(path, content, overwrite);
    }
    #[cfg(not(windows))]
    {
        write_text_file(path, content, overwrite, true)
    }
}

#[cfg(windows)]
fn write_windows_secret_text_file(
    path: &Path,
    content: &str,
    overwrite: bool,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            super::connections::ensure_real_directory_tree(parent).map_err(|error| {
                format!(
                    "failed to prepare secret file directory {}: {error}",
                    parent.display()
                )
            })?;
        }
    }
    use std::io::{Seek, SeekFrom, Write};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT,
    };

    // WRITE_DAC is deliberately requested on the same handle that will receive
    // the secret. The DACL is hardened before truncation/write, so an ACL
    // failure cannot leave newly written token material under inherited access.
    const WRITE_DAC: u32 = 0x0004_0000;
    let existed = path.exists();
    let mut options = std::fs::OpenOptions::new();
    options
        .write(true)
        .access_mode(GENERIC_WRITE | WRITE_DAC)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    if overwrite {
        options.create(true);
    } else {
        options.create_new(true);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("failed to open secret file {}: {}", path.display(), e))?;
    let result = (|| {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(format!(
                "failed to inspect secret file {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "refusing to write secret file through a reparse point: {}",
                path.display()
            ));
        }
        protect_windows_secret_handle(file.as_raw_handle() as _, path)?;
        file.set_len(0)
            .map_err(|e| format!("failed to truncate {}: {}", path.display(), e))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| format!("failed to seek {}: {}", path.display(), e))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("failed to write {}: {}", path.display(), e))?;
        file.sync_all()
            .map_err(|e| format!("failed to sync {}: {}", path.display(), e))?;
        Ok(())
    })();
    drop(file);
    if result.is_err() && !existed {
        let _ = std::fs::remove_file(path);
    }
    result
}

#[cfg(windows)]
fn protect_windows_secret_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
    path: &Path,
) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(format!(
            "failed to open current Windows token while protecting {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    let result = (|| {
        let mut required = 0u32;
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut required) };
        if required == 0 {
            return Err(format!(
                "failed to size current Windows user identity while protecting {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let words = (required as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; words];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(format!(
                "failed to read current Windows user identity while protecting {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
        let mut sid_text_ptr = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text_ptr) } == 0 {
            return Err(format!(
                "failed to encode current Windows user SID while protecting {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let sid = unsafe {
            let mut len = 0usize;
            while *sid_text_ptr.add(len) != 0 {
                len += 1;
            }
            let value = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text_ptr, len));
            LocalFree(sid_text_ptr as _);
            value
        };
        let sddl = format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)");
        let sddl_wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(format!(
                "failed to construct protected Windows ACL for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let descriptor_result = (|| {
            let mut present = 0;
            let mut defaulted = 0;
            let mut dacl = std::ptr::null_mut();
            if unsafe {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            } == 0
                || present == 0
                || dacl.is_null()
            {
                return Err(format!(
                    "failed to inspect protected Windows ACL for {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            let status = unsafe {
                SetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    dacl,
                    std::ptr::null_mut(),
                )
            };
            if status != 0 {
                return Err(format!(
                    "failed to protect Windows secret file {}: OS error {}",
                    path.display(),
                    status
                ));
            }
            Ok(())
        })();
        unsafe { LocalFree(descriptor as _) };
        descriptor_result
    })();
    unsafe { CloseHandle(token) };
    result
}

#[cfg(all(test, windows))]
pub(crate) fn windows_dacl_sddl(path: &Path) -> Result<String, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    const READ_CONTROL: u32 = 0x0002_0000;
    let file = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|e| {
            format!(
                "failed to open {} for ACL inspection: {}",
                path.display(),
                e
            )
        })?;
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as _,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(format!(
            "failed to read Windows ACL for {}: OS error {}",
            path.display(),
            status
        ));
    }
    let result = (|| {
        let mut text = std::ptr::null_mut();
        let mut text_len = 0u32;
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut text_len,
            )
        } == 0
        {
            return Err(format!(
                "failed to render Windows ACL for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let value = unsafe {
            let value =
                String::from_utf16_lossy(std::slice::from_raw_parts(text, text_len as usize));
            LocalFree(text as _);
            value
        };
        Ok(value)
    })();
    unsafe { LocalFree(descriptor as _) };
    result
}

pub(crate) fn discover_internal_binary(name: &str) -> Option<PathBuf> {
    discover_sibling_binary(name).or_else(|| discover_named_binary_absolute(name))
}

fn discover_sibling_binary(name: &str) -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let directory = current.parent()?;
    let candidate = directory.join(name);
    if candidate.is_file() {
        return Some(candidate);
    }
    #[cfg(windows)]
    {
        let candidate = directory.join(format!("{name}.exe"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn discover_named_binary_absolute(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = dir.join(format!("{}.exe", name));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
pub(crate) fn system_user_home(user: &str) -> Option<PathBuf> {
    use std::ffi::{CStr, CString, OsString};
    use std::os::unix::ffi::OsStringExt;

    let user = CString::new(user).ok()?;
    let initial_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = if initial_size > 0 {
        usize::try_from(initial_size).ok()?.clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    };
    let mut buffer = vec![0_u8; size];
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwnam_r(
            user.as_ptr(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let record = unsafe { record.assume_init() };
    if record.pw_dir.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes().to_vec();
    let path = PathBuf::from(OsString::from_vec(bytes));
    path.is_absolute().then_some(path)
}

#[cfg(unix)]
pub(crate) fn system_user_is_root(user: &str) -> bool {
    use std::ffi::CString;

    if user == "root" || user.parse::<u32>().is_ok_and(|uid| uid == 0) {
        return true;
    }
    let Ok(user) = CString::new(user) else {
        return false;
    };
    let initial_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = if initial_size > 0 {
        usize::try_from(initial_size)
            .unwrap_or(16 * 1024)
            .clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    };
    let mut buffer = vec![0_u8; size];
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwnam_r(
            user.as_ptr(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    status == 0 && !result.is_null() && unsafe { record.assume_init().pw_uid == 0 }
}

#[cfg(unix)]
pub(crate) fn system_user_exists(user: &str) -> bool {
    use std::ffi::CString;

    let initial_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = if initial_size > 0 {
        usize::try_from(initial_size)
            .unwrap_or(16 * 1024)
            .clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    };
    let mut buffer = vec![0_u8; size];
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    if let Ok(uid) = user.parse::<libc::uid_t>() {
        return unsafe {
            libc::getpwuid_r(
                uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            ) == 0
                && !result.is_null()
        };
    }
    let Ok(user) = CString::new(user) else {
        return false;
    };
    unsafe {
        libc::getpwnam_r(
            user.as_ptr(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        ) == 0
            && !result.is_null()
    }
}

#[cfg(unix)]
pub(crate) fn system_group_exists(group: &str) -> bool {
    use std::ffi::CString;

    let initial_size = unsafe { libc::sysconf(libc::_SC_GETGR_R_SIZE_MAX) };
    let size = if initial_size > 0 {
        usize::try_from(initial_size)
            .unwrap_or(16 * 1024)
            .clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    };
    let mut buffer = vec![0_u8; size];
    let mut record = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut result = std::ptr::null_mut();
    if let Ok(gid) = group.parse::<libc::gid_t>() {
        return unsafe {
            libc::getgrgid_r(
                gid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            ) == 0
                && !result.is_null()
        };
    }
    let Ok(group) = CString::new(group) else {
        return false;
    };
    unsafe {
        libc::getgrnam_r(
            group.as_ptr(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        ) == 0
            && !result.is_null()
    }
}

#[cfg(not(unix))]
pub(crate) fn system_user_home(_user: &str) -> Option<PathBuf> {
    None
}

#[cfg(not(unix))]
pub(crate) fn system_user_exists(_user: &str) -> bool {
    true
}

#[cfg(not(unix))]
pub(crate) fn system_group_exists(_group: &str) -> bool {
    true
}

#[cfg(not(unix))]
pub(crate) fn system_user_is_root(user: &str) -> bool {
    user == "root" || user.parse::<u32>().is_ok_and(|uid| uid == 0)
}

pub(crate) fn read_optional_token(
    path: &Option<PathBuf>,
    label: &str,
) -> Result<Option<String>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let token = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {} {}: {}", label, path.display(), e))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(format!("{} {} is empty", label, path.display()));
    }
    Ok(Some(token))
}

pub(crate) fn validate_user_api_token(token: &str) -> Result<(), String> {
    if token.trim().starts_with("wc_agent_") {
        return Err(
            "This is a Runner transport token and cannot be used for project/runtime APIs. Use the generated webcodex-user-token instead."
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) fn resolve_user_api_token(
    token: &Option<String>,
    token_file: &Option<PathBuf>,
    env_file: &Option<PathBuf>,
) -> Result<Option<String>, String> {
    if let Some(token) = token {
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err("--token cannot be empty".to_string());
        }
        validate_user_api_token(&token)?;
        return Ok(Some(token));
    }
    if let Some(token) = read_optional_token(token_file, "--token-file")? {
        validate_user_api_token(&token)?;
        return Ok(Some(token));
    }
    if let Some(path) = env_file {
        if let Some(token) = read_env_file_value(path, "WEBCODEX_TOKEN")? {
            let token = token.trim().to_string();
            if !token.is_empty() {
                validate_user_api_token(&token)?;
                return Ok(Some(token));
            }
        }
    }
    if let Ok(token) = std::env::var("WEBCODEX_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            validate_user_api_token(&token)?;
            return Ok(Some(token));
        }
    }
    Ok(None)
}

pub(crate) fn read_optional_user_api_token(
    path: &Option<PathBuf>,
    label: &str,
) -> Result<Option<String>, String> {
    let token = read_optional_token(path, label)?;
    if let Some(token) = token.as_deref() {
        validate_user_api_token(token)?;
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_api_token_validation_rejects_runner_transport_tokens_without_echoing_them() {
        let token = "wc_agent_do_not_echo_0123456789";
        let error = validate_user_api_token(token).unwrap_err();
        assert!(error.contains("Runner transport token"));
        assert!(error.contains("webcodex-user-token"));
        assert!(!error.contains(token));
    }

    #[test]
    fn user_api_token_validation_accepts_user_tokens() {
        validate_user_api_token("wc_pat_user_api_token_0123456789").unwrap();
        validate_user_api_token("shared-key-without-managed-prefix").unwrap();
    }

    #[test]
    fn root_system_identities_include_numeric_zero() {
        assert!(system_user_is_root("root"));
        assert!(system_user_is_root("0"));
        assert!(system_user_is_root("000"));
    }
}
