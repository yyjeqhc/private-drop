use crate::activity::ActivityEntry;
use crate::error::DesktopError;
use crate::models::{DesktopStateSnapshot, ProjectSelection, TunnelProxyMode};
use crate::state::AppState;
use serde::Deserialize;
use tauri::State;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRequest {
    pub project_path: String,
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

#[tauri::command]
pub async fn get_desktop_state(
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    Ok(state.get_state())
}

#[tauri::command]
pub async fn refresh_runtime_status(
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state.refresh_runtime_status().await
}

#[tauri::command]
pub async fn resume_saved_runtime(
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state.resume_saved_runtime().await
}

#[tauri::command]
pub async fn update_tunnel_proxy(
    request: TunnelProxyRequest,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state
        .update_tunnel_proxy(request.mode, request.custom_url.as_deref())
        .await
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
    request: ProjectRequest,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state.configure_local_setup(&request.project_path).await
}

#[tauri::command]
pub async fn configure_remote_setup(
    request: RemoteSetupRequest,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state
        .configure_remote_setup(
            &request.server_url,
            &request.pairing_code,
            &request.project_path,
        )
        .await
}

#[tauri::command]
pub async fn start_quick_share(
    request: QuickShareRequest,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state
        .start_quick_share(&request.project_path, &request.provider)
        .await
}

#[tauri::command]
pub async fn stop_quick_share(
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state.stop_quick_share().await
}

#[tauri::command]
pub async fn start_regular_tunnel(
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state.start_regular_tunnel().await
}

#[tauri::command]
pub async fn stop_regular_tunnel(
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state.stop_regular_tunnel().await
}

#[tauri::command]
pub async fn stop_local_runtime(
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state.stop_local_runtime().await
}

#[tauri::command]
pub async fn cancel_desktop_operation(
    request: CancelDesktopOperationRequest,
    state: State<'_, AppState>,
) -> Result<DesktopStateSnapshot, DesktopError> {
    state.cancel_operation(&request.operation_id)
}

#[tauri::command]
pub async fn get_bounded_activity(
    state: State<'_, AppState>,
) -> Result<Vec<ActivityEntry>, DesktopError> {
    Ok(state.activity())
}
