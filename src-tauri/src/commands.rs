use std::sync::{Arc, Mutex, atomic::{AtomicI32, Ordering}};

use tauri::State;
use tauri_plugin_shell::{process::CommandChild, ShellExt};

pub struct SidecarState {
    port: AtomicI32,
    child: tokio::sync::Mutex<Option<CommandChild>>,
}

impl SidecarState {
    pub fn new() -> Self {
        Self {
            port: AtomicI32::new(0),
            child: tokio::sync::Mutex::new(None),
        }
    }

    pub fn shutting_down(&self) {
        self.port.store(0, Ordering::SeqCst);
    }
}

#[tauri::command]
pub async fn start_server(
    app: tauri::AppHandle,
    state: State<'_, Arc<SidecarState>>,
) -> Result<i32, String> {
    let port = portpicker::pick_unused_port()
        .ok_or_else(|| "No available port found".to_string())?;

    let addr = format!("127.0.0.1:{port}");

    let sidecar_command = app
        .shell()
        .sidecar("binaries/astrcode-http-server")
        .map_err(|e| e.to_string())?
        .env("ASTRCODE_HTTP_ADDR", &addr);

    let (mut rx, child) = sidecar_command
        .spawn()
        .map_err(|e| format!("Failed to spawn sidecar: {e}"))?;

    state.port.store(port as i32, Ordering::SeqCst);
    *state.child.lock().await = Some(child);

    tauri::async_runtime::spawn(async move {
        use tauri_plugin_shell::process::CommandEvent;
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line) => {
                    tracing::info!("[sidecar stdout] {}", String::from_utf8_lossy(&line));
                }
                CommandEvent::Stderr(line) => {
                    tracing::info!("[sidecar stderr] {}", String::from_utf8_lossy(&line));
                }
                CommandEvent::Terminated(status) => {
                    tracing::warn!("[sidecar] exited with status: {status:?}");
                    break;
                }
                CommandEvent::Error(err) => {
                    tracing::error!("[sidecar error] {err}");
                    break;
                }
                _ => {}
            }
        }
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let health_url = format!("http://{addr}/api/sessions");
    for attempt in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if client.get(&health_url).send().await.is_ok() {
            tracing::info!("Server ready at {addr} (attempt {})", attempt + 1);
            return Ok(port as i32);
        }
    }

    Err(format!("Server did not become ready at {addr} within 10s"))
}

#[tauri::command]
pub async fn stop_server(state: State<'_, Arc<SidecarState>>) -> Result<(), String> {
    let mut guard = state.child.lock().await;
    if let Some(child) = guard.take() {
        child.kill().map_err(|e| format!("Failed to kill sidecar: {e}"))?;
    }
    state.port.store(0, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
pub fn select_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let dir = app
        .dialog()
        .file()
        .blocking_pick_folder()
        .map(|p| p.to_string());
    Ok(dir)
}

#[tauri::command]
pub fn minimize_window(window: tauri::WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn maximize_window(window: tauri::WebviewWindow) -> Result<(), String> {
    let is_max = window.is_maximized().map_err(|e| e.to_string())?;
    if is_max {
        window.unmaximize().map_err(|e| e.to_string())
    } else {
        window.maximize().map_err(|e| e.to_string())
    }
}

#[tauri::command]
pub fn close_window(window: tauri::WebviewWindow) -> Result<(), String> {
    window.close().map_err(|e| e.to_string())
}
