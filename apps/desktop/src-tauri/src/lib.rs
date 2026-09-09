mod activity;
mod commands;
mod deadline;
mod desktop_shell;
mod error;
mod models;
mod operation;
mod platform;
mod process;
mod state;
mod tray;
mod webcodex;

use state::AppState;
use tauri::Manager;
use tauri_plugin_autostart::MacosLauncher;

pub fn run() {
    let app = tauri::Builder::default()
        // Tauri recommends registering single-instance first so a secondary
        // process is rejected before any other plugin can initialize state.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            desktop_shell::handle_second_instance(app, &argv);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .setup(|app| {
            let data_dir = app.path().app_local_data_dir()?;
            let resource_dir = app.path().resource_dir()?;
            app.manage(AppState::new(data_dir, resource_dir)?);
            app.manage(desktop_shell::DesktopShellState::default());
            app.manage(tray::TrayPresentationCache::default());
            tray::setup(app.handle())?;

            let snapshot = app.state::<AppState>().get_state();
            tray::refresh_from_snapshot(app.handle(), &snapshot);
            if !desktop_shell::is_background_launch(std::env::args()) {
                desktop_shell::show_main_window(app.handle())?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_desktop_state,
            commands::open_powershell_install_guide,
            commands::get_launch_at_login,
            commands::set_launch_at_login,
            commands::refresh_runtime_status,
            commands::observe_chatgpt_activity,
            commands::resume_saved_runtime,
            commands::update_tunnel_proxy,
            commands::inspect_project,
            commands::configure_local_setup,
            commands::configure_remote_setup,
            commands::start_quick_share,
            commands::stop_quick_share,
            commands::start_regular_tunnel,
            commands::stop_regular_tunnel,
            commands::stop_local_runtime,
            commands::cancel_desktop_operation,
            commands::get_bounded_activity,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build WebCodex Desktop");

    app.run(|app_handle, event| match event {
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::CloseRequested { api, .. },
            ..
        } if label == desktop_shell::MAIN_WINDOW_LABEL => {
            let shell = app_handle.state::<desktop_shell::DesktopShellState>();
            if shell.close_disposition() == desktop_shell::CloseDisposition::HideWindow {
                api.prevent_close();
                let _ = desktop_shell::hide_main_window(app_handle);
            }
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen {
            has_visible_windows,
            ..
        } => {
            if !has_visible_windows {
                let _ = desktop_shell::show_main_window(app_handle);
            }
        }
        tauri::RunEvent::ExitRequested { .. } => {
            app_handle
                .state::<desktop_shell::DesktopShellState>()
                .mark_exit_requested();
            let state = app_handle.state::<AppState>();
            tauri::async_runtime::block_on(state.shutdown());
        }
        tauri::RunEvent::Exit => {
            let state = app_handle.state::<AppState>();
            tauri::async_runtime::block_on(state.shutdown());
        }
        _ => {}
    });
}
