use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use fs2::FileExt;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::paths::{instance_info_path, instance_lock_path};

const RETRY_INTERVAL: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstanceInfo {
    port: u16,
    token: String,
    pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivationRequest {
    token: String,
    action: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivationResponse {
    ok: bool,
}

pub enum InstanceBootstrap {
    Primary(Arc<InstanceCoordinator>),
    ActivatedExisting,
}

pub struct InstanceCoordinator {
    _lock_file: File,
    info_path: std::path::PathBuf,
    app_handle: Arc<Mutex<Option<AppHandle>>>,
    main_window_ready: Arc<AtomicBool>,
    pending_focus: Arc<AtomicBool>,
    listener_shutdown: Arc<AtomicBool>,
    _listener_thread: Mutex<Option<JoinHandle<()>>>,
}

impl InstanceCoordinator {
    pub fn bootstrap() -> Result<InstanceBootstrap> {
        let lock_path = instance_lock_path()?;
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建实例目录失败: {}", parent.display()))?;
        }

        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("打开实例锁失败: {}", lock_path.display()))?;

        match lock_file.try_lock_exclusive() {
            Ok(()) => Self::start_primary(lock_file).map(InstanceBootstrap::Primary),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::PermissionDenied) => {
                Self::notify_existing()?;
                Ok(InstanceBootstrap::ActivatedExisting)
            }
            Err(e) => Err(e).with_context(|| format!("获取实例锁失败: {}", lock_path.display())),
        }
    }

    fn start_primary(lock_file: File) -> Result<Arc<Self>> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .context("绑定实例监听端口失败")?;
        listener.set_nonblocking(true)?;

        let info_path = instance_info_path()?;
        if let Some(parent) = info_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let info = InstanceInfo {
            port: listener.local_addr()?.port(),
            token: random_hex_token(),
            pid: std::process::id(),
        };

        let payload = serde_json::to_string_pretty(&info)?;
        fs::write(&info_path, &payload)?;

        let listener_shutdown = Arc::new(AtomicBool::new(false));
        let pending_focus = Arc::new(AtomicBool::new(false));
        let app_handle: Arc<Mutex<Option<AppHandle>>> = Arc::new(Mutex::new(None));
        let main_window_ready = Arc::new(AtomicBool::new(false));

        let listener_thread = {
            let listener_shutdown = Arc::clone(&listener_shutdown);
            let pending_focus = Arc::clone(&pending_focus);
            let app_handle = Arc::clone(&app_handle);
            let main_window_ready = Arc::clone(&main_window_ready);
            let token = info.token.clone();
            std::thread::spawn(move || {
                run_listener(
                    listener,
                    &listener_shutdown,
                    &app_handle,
                    &main_window_ready,
                    &pending_focus,
                    &token,
                );
            })
        };

        Ok(Arc::new(Self {
            _lock_file: lock_file,
            info_path,
            app_handle,
            main_window_ready,
            pending_focus,
            listener_shutdown,
            _listener_thread: Mutex::new(Some(listener_thread)),
        }))
    }

    fn notify_existing() -> Result<()> {
        let info_path = instance_info_path()?;
        let deadline = std::time::Instant::now() + ACTIVATION_TIMEOUT;

        loop {
            if let Ok(raw) = fs::read_to_string(&info_path) {
                if let Ok(info) = serde_json::from_str::<InstanceInfo>(&raw) {
                    if send_focus_request(&info).is_ok() {
                        return Ok(());
                    }
                }
            }

            if std::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "已有实例在运行，但未在 {:?} 内响应",
                    ACTIVATION_TIMEOUT
                ));
            }
            std::thread::sleep(RETRY_INTERVAL);
        }
    }

    pub fn attach_app_handle(&self, handle: AppHandle) {
        let mut slot = self.app_handle.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(handle);
        self.flush_pending_focus();
    }

    pub fn mark_main_window_ready(&self) {
        self.main_window_ready.store(true, Ordering::SeqCst);
        self.flush_pending_focus();
    }

    pub fn shutdown(&self) {
        self.listener_shutdown.store(true, Ordering::SeqCst);

        if let Ok(raw) = fs::read_to_string(&self.info_path) {
            if let Ok(info) = serde_json::from_str::<InstanceInfo>(&raw) {
                if info.pid == std::process::id() {
                    let _ = fs::remove_file(&self.info_path);
                }
            }
        }
    }

    fn flush_pending_focus(&self) {
        if !self.pending_focus.swap(false, Ordering::SeqCst) {
            return;
        }
        trigger_focus(&self.app_handle, &self.main_window_ready, &self.pending_focus);
    }
}

fn send_focus_request(info: &InstanceInfo) -> Result<()> {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], info.port)),
        CONNECT_TIMEOUT,
    )?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;

    let payload = serde_json::to_vec(&ActivationRequest {
        token: info.token.clone(),
        action: "focus".to_string(),
    })?;
    stream.write_all(&payload)?;
    stream.shutdown(Shutdown::Write)?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let resp: ActivationResponse = serde_json::from_str(response.trim())?;
    if !resp.ok {
        return Err(anyhow!("激活请求被拒绝"));
    }
    Ok(())
}

fn run_listener(
    listener: TcpListener,
    shutdown: &AtomicBool,
    app_handle: &Arc<Mutex<Option<AppHandle>>>,
    main_window_ready: &AtomicBool,
    pending_focus: &AtomicBool,
    expected_token: &str,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut payload = String::new();
                let accepted = stream
                    .read_to_string(&mut payload)
                    .ok()
                    .and_then(|_| serde_json::from_str::<ActivationRequest>(payload.trim()).ok())
                    .map(|req| req.token == expected_token && req.action == "focus")
                    .unwrap_or(false);

                let resp = ActivationResponse { ok: accepted };
                if let Ok(data) = serde_json::to_vec(&resp) {
                    let _ = stream.write_all(&data);
                }
                if accepted {
                    trigger_focus(app_handle, main_window_ready, pending_focus);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(RETRY_INTERVAL);
            }
            Err(_) => {
                std::thread::sleep(RETRY_INTERVAL);
            }
        }
    }
}

fn trigger_focus(
    app_handle: &Arc<Mutex<Option<AppHandle>>>,
    main_window_ready: &AtomicBool,
    pending_focus: &AtomicBool,
) {
    if !main_window_ready.load(Ordering::SeqCst) {
        pending_focus.store(true, Ordering::SeqCst);
        return;
    }

    let handle = match app_handle.lock() {
        Ok(slot) => slot.clone(),
        Err(e) => {
            let slot = e.into_inner();
            slot.clone()
        },
    };
    let Some(handle) = handle else {
        pending_focus.store(true, Ordering::SeqCst);
        return;
    };

    if handle.get_webview_window("main").is_none() {
        pending_focus.store(true, Ordering::SeqCst);
        return;
    }

    let h = handle.clone();
    if handle.run_on_main_thread(move || {
        if let Some(w) = h.get_webview_window("main") {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
    }).is_err() {
        pending_focus.store(true, Ordering::SeqCst);
    }
}

fn random_hex_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
