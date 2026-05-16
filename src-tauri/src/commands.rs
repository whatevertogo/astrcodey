use std::sync::Arc;

use tauri::State;
use tauri_plugin_shell::{ShellExt, process::CommandChild};

const SIDECAR_NAME: &str = "astrcode-http-server";
const SIDECAR_ADDR_ENV: &str = "ASTRCODE_HTTP_ADDR";
const SIDECAR_TOKEN_ENV: &str = "ASTRCODE_HTTP_TOKEN";

/// sidecar 进程的运行时状态：port 和 child 绑定在同一个锁内，
/// 避免单独操作 port / child 时出现竞态。
struct Inner {
    port: i32,
    token: String,
    child: Option<CommandChild>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartServerResponse {
    port: i32,
    token: String,
}

pub struct SidecarState {
    startup: tokio::sync::Mutex<()>,
    inner: std::sync::Mutex<Inner>,
}

impl SidecarState {
    pub fn new() -> Self {
        Self {
            startup: tokio::sync::Mutex::new(()),
            inner: std::sync::Mutex::new(Inner {
                port: 0,
                token: String::new(),
                child: None,
            }),
        }
    }

    pub fn port(&self) -> i32 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).port
    }

    pub fn shutting_down(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.port = 0;
        guard.token.clear();
        if let Some(child) = guard.child.take() {
            let _ = child.kill();
        }
    }
}

fn lock_inner(state: &SidecarState) -> std::sync::MutexGuard<'_, Inner> {
    state.inner.lock().unwrap_or_else(|e| e.into_inner())
}

fn generate_auth_token() -> String {
    rand::random::<[u8; 32]>()
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tauri::command]
pub async fn start_server(
    app: tauri::AppHandle,
    state: State<'_, Arc<SidecarState>>,
) -> Result<StartServerResponse, String> {
    let _startup_guard = state.startup.lock().await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    // Hold inner lock through the entire check + cleanup + spawn sequence
    // to prevent stop_server from racing between the steps.
    let (port, mut rx) = {
        let mut inner = lock_inner(&state);
        if inner.port > 0 && inner.child.is_some() {
            return Ok(StartServerResponse {
                port: inner.port,
                token: inner.token.clone(),
            });
        }
        if let Some(child) = inner.child.take() {
            let _ = child.kill();
        }
        inner.port = 0;
        inner.token.clear();

        let port =
            portpicker::pick_unused_port().ok_or_else(|| "No available port found".to_string())?;
        let token = generate_auth_token();

        let addr = format!("127.0.0.1:{port}");

        let sidecar_command = app
            .shell()
            .sidecar(SIDECAR_NAME)
            .map_err(|e| format!("Failed to resolve sidecar `{SIDECAR_NAME}`: {e}"))?
            .env(SIDECAR_ADDR_ENV, &addr)
            .env(SIDECAR_TOKEN_ENV, &token);

        let (rx, child) = sidecar_command
            .spawn()
            .map_err(|e| format!("Failed to spawn sidecar: {e}"))?;

        inner.port = port as i32;
        inner.token = token;
        inner.child = Some(child);
        // Release the inner lock before the health check loop.
        (port, rx)
    };

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
                    let mut inner = lock_inner(&sidecar_state);
                    inner.port = 0;
                    inner.token.clear();
                    inner.child.take();
                    break;
                },
                CommandEvent::Error(err) => {
                    tracing::error!("[sidecar error] {err}");
                    let mut inner = lock_inner(&sidecar_state);
                    inner.port = 0;
                    inner.token.clear();
                    inner.child.take();
                    break;
                },
                _ => {},
            }
        }
    });

    let health_url = format!("http://127.0.0.1:{port}/api/sessions");
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let still_tracked = {
            let inner = lock_inner(&state);
            inner.port == port as i32 && inner.child.is_some()
        };
        let token = {
            let inner = lock_inner(&state);
            inner.token.clone()
        };
        let ready = client
            .get(&health_url)
            .bearer_auth(token)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if still_tracked && ready {
            tracing::info!("Server ready on port {port}");
            let token = {
                let inner = lock_inner(&state);
                inner.token.clone()
            };
            return Ok(StartServerResponse {
                port: port as i32,
                token,
            });
        }
    }

    {
        let mut inner = lock_inner(&state);
        if let Some(child) = inner.child.take() {
            let _ = child.kill();
        }
        inner.port = 0;
        inner.token.clear();
    }

    Err("Server did not become ready within 10s".to_string())
}

#[tauri::command]
pub async fn stop_server(state: State<'_, Arc<SidecarState>>) -> Result<(), String> {
    let port = {
        let inner = lock_inner(&state);
        inner.port
    };
    let token = {
        let inner = lock_inner(&state);
        inner.token.clone()
    };

    // Try graceful shutdown via HTTP first
    if port > 0 {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .map_err(|e| e.to_string())?;
        let _ = client
            .post(format!("http://127.0.0.1:{port}/api/shutdown"))
            .bearer_auth(token)
            .send()
            .await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    // Force kill if still running
    let mut inner = lock_inner(&state);
    if let Some(child) = inner.child.take() {
        let _ = child.kill();
    }
    inner.port = 0;
    inner.token.clear();
    Ok(())
}

#[tauri::command]
pub async fn select_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |path| {
        let _ = tx.send(path.map(|p| p.to_string()));
    });
    rx.await.map_err(|_| "dialog cancelled".to_string())
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
