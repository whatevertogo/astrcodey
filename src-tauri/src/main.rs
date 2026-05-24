#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

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

#[cfg(debug_assertions)]
fn wait_for_dev_server() {
    use std::net::{SocketAddr, TcpStream};

    const DEV_ADDR: &str = "127.0.0.1:5173";
    const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

    let addr: SocketAddr = DEV_ADDR.parse().expect("invalid dev server address");
    let deadline = std::time::Instant::now() + MAX_WAIT;

    tracing::info!("waiting for Vite dev server at {DEV_ADDR}...");
    loop {
        if TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).is_ok() {
            tracing::info!("Vite dev server ready");
            return;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!("timed out waiting for Vite dev server, showing window anyway");
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 同步终止 sidecar 进程：先 child.kill()，再按 PID 杀进程树兜底。
fn shutdown_sidecar(app_handle: &tauri::AppHandle) {
    let Some(state) = app_handle.try_state::<std::sync::Arc<commands::SidecarState>>() else {
        return;
    };
    state.shutdown_blocking();
}

fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ASTRCODE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let coordinator = match InstanceCoordinator::bootstrap()? {
        InstanceBootstrap::Primary(c) => c,
        InstanceBootstrap::ActivatedExisting => return Ok(()),
    };

    let coord_setup = Arc::clone(&coordinator);
    let coord_run = Arc::clone(&coordinator);

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_http::init())
        .manage(std::sync::Arc::new(commands::SidecarState::new()))
        .setup(move |app| {
            coord_setup.attach_app_handle(app.handle().clone());

            #[cfg(debug_assertions)]
            {
                let window = app
                    .get_webview_window("main")
                    .expect("main window not found");
                let coord_win = Arc::clone(&coord_setup);
                std::thread::spawn(move || {
                    wait_for_dev_server();
                    let _ = window.show();
                    coord_win.mark_main_window_ready();
                });
            }

            #[cfg(not(debug_assertions))]
            {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                }
                let coord_win = Arc::clone(&coord_setup);
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    coord_win.mark_main_window_ready();
                });
            }

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
        .run(move |app_handle, event| match event {
            tauri::RunEvent::ExitRequested { .. } => {
                coord_run.shutdown();
                shutdown_sidecar(&app_handle);
            },
            tauri::RunEvent::Exit => {
                shutdown_sidecar(&app_handle);
            },
            _ => {},
        });

    Ok(())
}
