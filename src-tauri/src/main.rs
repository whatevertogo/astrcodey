#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod commands;
mod instance;
mod paths;

use std::sync::Arc;

use instance::{InstanceBootstrap, InstanceCoordinator};
use tauri::Manager;

fn main() {
    if let Err(e) = run() {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let coordinator = match InstanceCoordinator::bootstrap()? {
        InstanceBootstrap::Primary(c) => c,
        InstanceBootstrap::ActivatedExisting => return Ok(()),
    };

    let coord_setup = Arc::clone(&coordinator);
    let coord_run = Arc::clone(&coordinator);

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(std::sync::Arc::new(commands::SidecarState::new()))
        .setup(move |app| {
            coord_setup.attach_app_handle(app.handle().clone());

            let coord_win = Arc::clone(&coord_setup);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                coord_win.mark_main_window_ready();
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::start_server,
            commands::stop_server,
            commands::select_directory,
            commands::minimize_window,
            commands::maximize_window,
            commands::close_window,
        ])
        .build(tauri::generate_context!())
        .expect("error building tauri application")
        .run(move |app_handle, event| {
            if matches!(event, tauri::RunEvent::ExitRequested { .. }) {
                coord_run.shutdown();
                if let Some(state) = app_handle.try_state::<std::sync::Arc<commands::SidecarState>>()
                {
                    state.shutting_down();
                }
            }
        });

    Ok(())
}
