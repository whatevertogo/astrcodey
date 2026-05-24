use std::{
    io::{Read, Write},
    net::TcpStream,
    sync::Arc,
    time::Duration,
};

use tauri::{Manager, State};
use tauri_plugin_shell::{ShellExt, process::CommandChild};

const SIDECAR_NAME: &str = "astrcode-http-server";
const SIDECAR_ADDR_ENV: &str = "ASTRCODE_HTTP_ADDR";
const SIDECAR_TOKEN_ENV: &str = "ASTRCODE_HTTP_TOKEN";
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(50);
const STARTUP_ATTEMPTS: usize = 200;
const HEALTH_TIMEOUT: Duration = Duration::from_millis(500);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(700);

/// sidecar 进程的运行时状态：port 和 child 绑定在同一个锁内，
/// 避免单独操作 port / child 时出现竞态。
struct Inner {
    port: i32,
    token: String,
    pid: Option<u32>,
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
                pid: None,
                child: None,
            }),
        }
    }

    pub fn shutdown_blocking(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let port = guard.port;
        let token = guard.token.clone();
        let pid = guard.pid.take();
        let child = guard.child.take();
        guard.port = 0;
        guard.token.clear();
        drop(guard);

        if port > 0 && !token.is_empty() {
            post_shutdown_blocking(port as u16, &token);
        }
        if let Some(child) = child {
            let _ = child.kill();
        }
        kill_process_tree(pid);
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

async fn server_ready(client: &reqwest::Client, port: u16, token: &str) -> bool {
    client
        .get(format!("http://127.0.0.1:{port}/api/sessions"))
        .bearer_auth(token)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

fn post_shutdown_blocking(port: u16, token: &str) {
    let Ok(mut stream) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        SHUTDOWN_TIMEOUT,
    ) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(SHUTDOWN_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SHUTDOWN_TIMEOUT));
    let request = format!(
        "POST /api/shutdown HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer \
         {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(request.as_bytes());
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
}

fn kill_process_tree(pid: Option<u32>) {
    #[cfg(target_os = "windows")]
    if let Some(pid) = pid {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[tauri::command]
pub async fn start_server(
    app: tauri::AppHandle,
    state: State<'_, Arc<SidecarState>>,
) -> Result<StartServerResponse, String> {
    let _startup_guard = state.startup.lock().await;
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let existing = {
        let inner = lock_inner(&state);
        (inner.port, inner.token.clone())
    };
    if existing.0 > 0 && server_ready(&client, existing.0 as u16, &existing.1).await {
        tracing::info!("Reusing ready server on port {}", existing.0);
        return Ok(StartServerResponse {
            port: existing.0,
            token: existing.1,
        });
    }
    let (port, token, expected_pid, mut rx) = {
        let mut inner = lock_inner(&state);
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
        inner.pid = Some(child.pid());
        inner.child = Some(child);
        // Release the inner lock before the health check loop.
        (port, inner.token.clone(), inner.pid.unwrap_or_default(), rx)
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
                    if inner.pid == Some(expected_pid) {
                        inner.pid = None;
                        inner.child.take();
                    }
                    break;
                },
                CommandEvent::Error(err) => {
                    tracing::error!("[sidecar error] {err}");
                    let mut inner = lock_inner(&sidecar_state);
                    if inner.pid == Some(expected_pid) {
                        inner.pid = None;
                        inner.child.take();
                    }
                    break;
                },
                _ => {},
            }
        }
    });

    for _ in 0..STARTUP_ATTEMPTS {
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
        let still_current = {
            let inner = lock_inner(&state);
            inner.pid == Some(expected_pid) && inner.port == port as i32 && inner.token == token
        };
        if !still_current {
            return Err("Server startup was superseded by another request".to_string());
        }
        if server_ready(&client, port, &token).await {
            tracing::info!("Server ready on port {port}");
            return Ok(StartServerResponse {
                port: port as i32,
                token,
            });
        }
    }

    {
        let mut inner = lock_inner(&state);
        let child = (inner.pid == Some(expected_pid))
            .then(|| inner.child.take())
            .flatten();
        if inner.pid == Some(expected_pid) {
            inner.pid = None;
            inner.port = 0;
            inner.token.clear();
        }
        if let Some(child) = child {
            let _ = child.kill();
        }
    }
    kill_process_tree(Some(expected_pid));

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
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|e| e.to_string())?;
        let _ = client
            .post(format!("http://127.0.0.1:{port}/api/shutdown"))
            .bearer_auth(token)
            .send()
            .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Force kill if still running
    let mut inner = lock_inner(&state);
    if let Some(child) = inner.child.take() {
        let _ = child.kill();
    }
    let pid = inner.pid.take();
    inner.port = 0;
    inner.token.clear();
    drop(inner);
    kill_process_tree(pid);
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
pub fn close_window(
    window: tauri::WebviewWindow,
    state: State<'_, Arc<SidecarState>>,
) -> Result<(), String> {
    state.shutdown_blocking();
    window.app_handle().exit(0);
    Ok(())
}
