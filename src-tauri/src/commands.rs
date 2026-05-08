use std::sync::{
    Arc,
    atomic::{AtomicI32, Ordering},
};

use tauri::State;
use tauri_plugin_shell::{ShellExt, process::CommandChild};

const SIDECAR_NAME: &str = "astrcode-http-server";
const SIDECAR_ADDR_ENV: &str = "ASTRCODE_HTTP_ADDR";

pub struct SidecarState {
    port: AtomicI32,
    startup: tokio::sync::Mutex<()>,
    child: tokio::sync::Mutex<Option<CommandChild>>,
}

impl SidecarState {
    pub fn new() -> Self {
        Self {
            port: AtomicI32::new(0),
            startup: tokio::sync::Mutex::new(()),
            child: tokio::sync::Mutex::new(None),
        }
    }

    pub fn port(&self) -> i32 {
        self.port.load(Ordering::SeqCst)
    }

    pub fn shutting_down(&self) {
        self.port.store(0, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(child) = guard.take() {
                let _ = child.kill();
            }
        }
    }
}

#[tauri::command]
pub async fn start_server(
    app: tauri::AppHandle,
    state: State<'_, Arc<SidecarState>>,
) -> Result<i32, String> {
    let _startup_guard = state.startup.lock().await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    // Single lock acquisition: check and cleanup atomically
    {
        let mut child_guard = state.child.lock().await;
        let current_port = state.port.load(Ordering::SeqCst);
        if current_port > 0 && child_guard.is_some() {
            return Ok(current_port);
        }
        if let Some(child) = child_guard.take() {
            let _ = child.kill();
        }
    }
    state.port.store(0, Ordering::SeqCst);

    let port =
        portpicker::pick_unused_port().ok_or_else(|| "No available port found".to_string())?;

    let addr = format!("127.0.0.1:{port}");

    let sidecar_command = app
        .shell()
        .sidecar(SIDECAR_NAME)
        .map_err(|e| format!("Failed to resolve sidecar `{SIDECAR_NAME}`: {e}"))?
        .env(SIDECAR_ADDR_ENV, &addr);

    let (mut rx, child) = sidecar_command
        .spawn()
        .map_err(|e| format!("Failed to spawn sidecar: {e}"))?;

    state.port.store(port as i32, Ordering::SeqCst);
    *state.child.lock().await = Some(child);

    let sidecar_state = Arc::clone(&state);
    tauri::async_runtime::spawn(async move {
        use tauri_plugin_shell::process::CommandEvent;
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line) => {
                    tracing::info!("[sidecar stdout] {}", String::from_utf8_lossy(&line));
                },
                CommandEvent::Stderr(line) => {
                    tracing::info!("[sidecar stderr] {}", String::from_utf8_lossy(&line));
                },
                CommandEvent::Terminated(status) => {
                    tracing::warn!("[sidecar] exited with status: {status:?}");
                    sidecar_state.port.store(0, Ordering::SeqCst);
                    let _ = sidecar_state.child.lock().await.take();
                    break;
                },
                CommandEvent::Error(err) => {
                    tracing::error!("[sidecar error] {err}");
                    sidecar_state.port.store(0, Ordering::SeqCst);
                    let _ = sidecar_state.child.lock().await.take();
                    break;
                },
                _ => {},
            }
        }
    });

    let health_url = format!("http://{addr}/api/sessions");
    for attempt in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let still_tracked =
            state.port.load(Ordering::SeqCst) == port as i32 && state.child.lock().await.is_some();
        if still_tracked && client.get(&health_url).send().await.is_ok() {
            tracing::info!("Server ready at {addr} (attempt {})", attempt + 1);
            return Ok(port as i32);
        }
    }

    {
        let mut child_guard = state.child.lock().await;
        if let Some(child) = child_guard.take() {
            let _ = child.kill();
        }
    }
    state.port.store(0, Ordering::SeqCst);

    Err(format!("Server did not become ready at {addr} within 10s"))
}

#[tauri::command]
pub async fn stop_server(state: State<'_, Arc<SidecarState>>) -> Result<(), String> {
    let port = state.port.load(Ordering::SeqCst);

    // Try graceful shutdown via HTTP first
    if port > 0 {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .map_err(|e| e.to_string())?;
        let _ = client
            .post(format!("http://127.0.0.1:{port}/api/shutdown"))
            .send()
            .await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    // Force kill if still running
    let mut guard = state.child.lock().await;
    if let Some(child) = guard.take() {
        let _ = child.kill();
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
