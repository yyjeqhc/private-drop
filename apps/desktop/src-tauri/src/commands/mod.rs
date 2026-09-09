use crate::activity::ActivityEntry;
use crate::desktop_shell;
use crate::error::DesktopError;
use crate::models::{DesktopStateSnapshot, ProjectSelection, TunnelProxyMode};
use crate::state::AppState;
use crate::tray;
use serde::Deserialize;
use tauri::{AppHandle, State};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRequest {
    pub project_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSetupRequest {
    pub project_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSetupRequest {
    pub server_url: String,
    pub pairing_code: String,
    pub project_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickShareRequest {
    pub project_path: String,
    pub provider: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelDesktopOperationRequest {
    pub operation_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelProxyRequest {
    pub mode: TunnelProxyMode,
    pub custom_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchAtLoginRequest {
    pub enabled: bool,
}

fn project_state_result(
    app: &AppHandle,
    result: Result<DesktopStateSnapshot, DesktopError>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    if let Ok(snapshot) = &result {
        tray::refresh_from_snapshot(app, snapshot);
    }
    result
}

#[tauri::command]
pub async fn get_desktop_state(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    let snapshot = state.get_state();
    tray::refresh_from_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
pub fn open_powershell_install_guide() -> Result<(), DesktopError> {
    crate::platform::open_powershell_install_guide()
}

#[tauri::command]
pub async fn get_launch_at_login(app: AppHandle) -> Result<bool, DesktopError> {
    match desktop_shell::launch_at_login_enabled(&app) {
        Ok(enabled) => {
            tray::set_launch_at_login_observation(&app, Some(enabled));
            Ok(enabled)
        }
        Err(error) => {
            tray::set_launch_at_login_observation(&app, None);
            Err(error)
        }
    }
}

#[tauri::command]
pub async fn set_launch_at_login(
    request: LaunchAtLoginRequest,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<bool, DesktopError> {
    match desktop_shell::set_launch_at_login(&app, request.enabled) {
        Ok(enabled) => {
            tray::set_launch_at_login_observation(&app, Some(enabled));
            Ok(enabled)
        }
        Err(error) => {
            tray::set_launch_at_login_observation(&app, None);
            let snapshot = state.get_state();
            tray::refresh_from_snapshot(&app, &snapshot);
            Err(error)
        }
    }
}

#[tauri::command]
pub async fn refresh_runtime_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    project_state_result(&app, state.refresh_runtime_status().await)
}

#[tauri::command]
pub async fn resume_saved_runtime(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    project_state_result(&app, state.resume_saved_runtime().await)
}

#[tauri::command]
pub async fn update_tunnel_proxy(
    request: TunnelProxyRequest,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    let result = state
        .update_tunnel_proxy(request.mode, request.custom_url.as_deref())
        .await;
    project_state_result(&app, result)
}

#[tauri::command]
pub async fn inspect_project(
    request: ProjectRequest,
    state: State<'_, AppState>,
) -> Result<ProjectSelection, DesktopError> {
    state.inspect_project(&request.project_path).await
}

#[tauri::command]
pub async fn configure_local_setup(
    request: LocalSetupRequest,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    let result = state
        .configure_local_setup(request.project_path.as_deref())
        .await;
    project_state_result(&app, result)
}

#[tauri::command]
pub async fn configure_remote_setup(
    request: RemoteSetupRequest,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    let result = state
        .configure_remote_setup(
            &request.server_url,
            &request.pairing_code,
            &request.project_path,
        )
        .await;
    project_state_result(&app, result)
}

#[tauri::command]
pub async fn start_quick_share(
    request: QuickShareRequest,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    let result = state
        .start_quick_share(&request.project_path, &request.provider)
        .await;
    project_state_result(&app, result)
}

#[tauri::command]
pub async fn stop_quick_share(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    project_state_result(&app, state.stop_quick_share().await)
}

#[tauri::command]
pub async fn start_regular_tunnel(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    project_state_result(&app, state.start_regular_tunnel().await)
}

#[tauri::command]
pub async fn stop_regular_tunnel(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    project_state_result(&app, state.stop_regular_tunnel().await)
}

#[tauri::command]
pub async fn stop_local_runtime(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    project_state_result(&app, state.stop_local_runtime().await)
}

#[tauri::command]
pub async fn cancel_desktop_operation(
    request: CancelDesktopOperationRequest,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    project_state_result(&app, state.cancel_operation(&request.operation_id))
}

#[tauri::command]
pub async fn get_bounded_activity(
    state: State<'_, AppState>,
) -> Result<Vec<ActivityEntry>, DesktopError> {
    Ok(state.activity())
}
