use crate::error::{DesktopError, DesktopResult};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_autostart::ManagerExt as _;

pub const MAIN_WINDOW_LABEL: &str = "main";
pub const NAVIGATE_EVENT: &str = "desktop:navigate";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDisposition {
    HideWindow,
    AllowExit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationTarget {
    Activity,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShowPlan {
    show: bool,
    unminimize: bool,
    focus: bool,
}

#[derive(Default)]
pub struct DesktopShellState {
    exit_requested: AtomicBool,
}

impl DesktopShellState {
    pub fn close_disposition(&self) -> CloseDisposition {
        close_disposition(self.exit_requested.load(Ordering::SeqCst))
    }

    pub fn mark_exit_requested(&self) {
        self.exit_requested.store(true, Ordering::SeqCst);
    }
}

fn close_disposition(exit_requested: bool) -> CloseDisposition {
    if exit_requested {
        CloseDisposition::AllowExit
    } else {
        CloseDisposition::HideWindow
    }
}

fn show_plan(visible: bool, minimized: bool) -> ShowPlan {
    ShowPlan {
        show: !visible,
        unminimize: minimized,
        focus: true,
    }
}

pub fn is_background_launch<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|arg| arg.as_ref() == "--background")
}

pub fn second_instance_requests_focus(argv: &[String]) -> bool {
    !is_background_launch(argv.iter().map(String::as_str))
}

pub fn handle_second_instance(app: &AppHandle, argv: &[String]) {
    if second_instance_requests_focus(argv) {
        let _ = show_main_window(app);
    }
}

pub fn show_main_window(app: &AppHandle) -> DesktopResult<()> {
    let window = app.get_webview_window(MAIN_WINDOW_LABEL).ok_or_else(|| {
        DesktopError::new(
            "desktop_window_unavailable",
            "The main WebCodex window is unavailable",
            "Quit WebCodex and start it again.",
        )
    })?;
    let visible = window.is_visible().map_err(window_error)?;
    let minimized = window.is_minimized().map_err(window_error)?;
    let plan = show_plan(visible, minimized);
    if plan.unminimize {
        window.unminimize().map_err(window_error)?;
    }
    if plan.show {
        window.show().map_err(window_error)?;
    }
    if plan.focus {
        window.set_focus().map_err(window_error)?;
    }
    Ok(())
}

pub fn hide_main_window(app: &AppHandle) -> DesktopResult<()> {
    let window = app.get_webview_window(MAIN_WINDOW_LABEL).ok_or_else(|| {
        DesktopError::new(
            "desktop_window_unavailable",
            "The main WebCodex window is unavailable",
            "Quit WebCodex and start it again.",
        )
    })?;
    window.hide().map_err(window_error)
}

pub fn navigate(app: &AppHandle, target: NavigationTarget) -> DesktopResult<()> {
    show_main_window(app)?;
    app.emit(NAVIGATE_EVENT, target).map_err(window_error)
}

pub fn request_application_exit(app: &AppHandle) {
    app.state::<DesktopShellState>().mark_exit_requested();
    app.exit(0);
}

pub fn launch_at_login_enabled(app: &AppHandle) -> DesktopResult<bool> {
    app.autolaunch().is_enabled().map_err(autostart_error)
}

pub fn set_launch_at_login(app: &AppHandle, enabled: bool) -> DesktopResult<bool> {
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(autostart_error)?;
    } else {
        manager.disable().map_err(autostart_error)?;
    }
    manager.is_enabled().map_err(autostart_error)
}

fn window_error(error: tauri::Error) -> DesktopError {
    DesktopError::new(
        "desktop_window_action_failed",
        format!("Desktop window action failed: {error}"),
        "Retry the Desktop window action.",
    )
}

fn autostart_error(error: impl std::fmt::Display) -> DesktopError {
    DesktopError::new(
        "desktop_autostart_unavailable",
        format!("Could not update the operating system login registration: {error}"),
        "Retry Launch at Login from Desktop Settings.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_requested_background_decision_is_not_quit() {
        let shell = DesktopShellState::default();
        assert_eq!(shell.close_disposition(), CloseDisposition::HideWindow);
        shell.mark_exit_requested();
        assert_eq!(shell.close_disposition(), CloseDisposition::AllowExit);
    }

    #[test]
    fn background_startup_detection_is_exact() {
        assert!(is_background_launch(["WebCodex", "--background"]));
        assert!(!is_background_launch(["WebCodex", "--backgroundish"]));
        assert!(!is_background_launch(["WebCodex"]));
    }

    #[test]
    fn second_instance_normal_launch_requests_focus_but_background_does_not() {
        assert!(second_instance_requests_focus(&["WebCodex".into()]));
        assert!(!second_instance_requests_focus(&[
            "WebCodex".into(),
            "--background".into(),
        ]));
    }

    #[test]
    fn show_plan_handles_visible_hidden_and_minimized_windows_idempotently() {
        assert_eq!(
            show_plan(true, false),
            ShowPlan {
                show: false,
                unminimize: false,
                focus: true,
            }
        );
        assert_eq!(
            show_plan(false, false),
            ShowPlan {
                show: true,
                unminimize: false,
                focus: true,
            }
        );
        assert_eq!(
            show_plan(true, true),
            ShowPlan {
                show: false,
                unminimize: true,
                focus: true,
            }
        );
    }
}
