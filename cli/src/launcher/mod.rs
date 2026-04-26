use std::{
    collections::VecDeque,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use astrcode_core::LocalServerInfo;
use astrcode_support::hostpaths::resolve_home_dir;
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, ChildStdout, Command},
    time::timeout,
};

const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub server_origin: Option<String>,
    pub bootstrap_token: Option<String>,
    pub working_dir: Option<PathBuf>,
    pub run_info_path: Option<PathBuf>,
    pub server_binary: Option<PathBuf>,
    pub ready_timeout: Duration,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            server_origin: None,
            bootstrap_token: None,
            working_dir: None,
            run_info_path: None,
            server_binary: None,
            ready_timeout: DEFAULT_READY_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionSource {
    Explicit,
    RunInfo,
    SpawnedLocal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConnection {
    pub origin: String,
    pub bootstrap_token: String,
    pub working_dir: Option<PathBuf>,
    pub source: ConnectionSource,
}

#[derive(Debug)]
pub struct LauncherSession<M> {
    connection: ResolvedConnection,
    managed_server: Option<M>,
}

impl<M> LauncherSession<M> {
    pub fn connection(&self) -> &ResolvedConnection {
        &self.connection
    }

    pub fn managed_server_mut(&mut self) -> Option<&mut M> {
        self.managed_server.as_mut()
    }
}

impl<M> LauncherSession<M>
where
    M: ManagedServerHandle,
{
    pub async fn shutdown(mut self) -> Result<(), LauncherError> {
        if let Some(server) = self.managed_server.as_mut() {
            server.terminate().await?;
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LauncherError {
    #[error("missing bootstrap token for server '{origin}'")]
    MissingBootstrapToken { origin: String },
    #[error("bootstrap token was rejected by server '{origin}'")]
    InvalidBootstrapToken { origin: String },
    #[error("run info is unavailable: {message}")]
    RunInfoUnavailable { message: String },
    #[error("server probe failed for '{origin}': {message}")]
    ProbeFailed { origin: String, message: String },
    #[error("server binary is unavailable: '{binary}'")]
    BinaryNotFound { binary: PathBuf },
    #[error("server spawn failed: {message}")]
    SpawnFailed { message: String },
    #[error("repo-aware cargo spawn failed in '{workspace_root}': {message}")]
    CargoSpawnFailed {
        workspace_root: PathBuf,
        message: String,
    },
    #[error("ready handshake failed: {message}")]
    ReadyHandshakeFailed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnTarget {
    Binary(PathBuf),
    CargoWorkspace { workspace_root: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub working_dir: Option<PathBuf>,
    pub target: SpawnTarget,
}

#[async_trait]
pub trait ManagedServerHandle: Send {
    async fn wait_ready(&mut self, timeout: Duration) -> Result<LocalServerInfo, LauncherError>;

    async fn terminate(&mut self) -> Result<(), LauncherError>;
}

#[async_trait]
pub trait LauncherBackend: Send + Sync {
    type ManagedServer: ManagedServerHandle;

    async fn read_run_info(&self, path: &Path) -> Result<Option<LocalServerInfo>, LauncherError>;

    async fn probe_bootstrap_token(&self, origin: &str) -> Result<Option<String>, LauncherError>;

    async fn spawn_server(
        &self,
        request: &SpawnRequest,
    ) -> Result<Self::ManagedServer, LauncherError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemLauncherBackend;

pub struct Launcher<B = SystemLauncherBackend> {
    backend: B,
}

impl Launcher<SystemLauncherBackend> {
    pub fn new() -> Self {
        Self {
            backend: SystemLauncherBackend,
        }
    }
}

impl<B> Launcher<B> {
    pub fn with_backend(backend: B) -> Self {
        Self { backend }
    }
}

impl<B> Default for Launcher<B>
where
    B: Default,
{
    fn default() -> Self {
        Self {
            backend: B::default(),
        }
    }
}

impl<B> Launcher<B>
where
    B: LauncherBackend,
{
    pub async fn resolve(
        &self,
        options: LaunchOptions,
    ) -> Result<LauncherSession<B::ManagedServer>, LauncherError> {
        if let Some(origin) = options.server_origin.as_ref() {
            let bootstrap_token =
                require_explicit_token(origin, options.bootstrap_token.as_deref())?.to_string();
            validate_bootstrap_token(&self.backend, origin, &bootstrap_token).await?;
            return Ok(LauncherSession {
                connection: ResolvedConnection {
                    origin: normalize_origin(origin),
                    bootstrap_token,
                    working_dir: options.working_dir,
                    source: ConnectionSource::Explicit,
                },
                managed_server: None,
            });
        }

        let run_info_path = resolve_run_info_path(options.run_info_path.as_ref())?;
        if let Some(run_info) = self.backend.read_run_info(&run_info_path).await? {
            if run_info_is_fresh(&run_info) {
                let origin = format!("http://127.0.0.1:{}", run_info.port);
                if validate_bootstrap_token(&self.backend, &origin, &run_info.token)
                    .await
                    .is_ok()
                {
                    return Ok(LauncherSession {
                        connection: ResolvedConnection {
                            origin,
                            bootstrap_token: run_info.token,
                            working_dir: options.working_dir,
                            source: ConnectionSource::RunInfo,
                        },
                        managed_server: None,
                    });
                }
            }
        }

        let spawn_request =
            resolve_spawn_request(options.working_dir.clone(), options.server_binary.clone())?;
        let mut managed_server = self.backend.spawn_server(&spawn_request).await?;
        let ready = managed_server.wait_ready(options.ready_timeout).await?;
        Ok(LauncherSession {
            connection: ResolvedConnection {
                origin: format!("http://127.0.0.1:{}", ready.port),
                bootstrap_token: ready.token,
                working_dir: options.working_dir,
                source: ConnectionSource::SpawnedLocal,
            },
            managed_server: Some(managed_server),
        })
    }
}

fn resolve_spawn_request(
    working_dir: Option<PathBuf>,
    server_binary: Option<PathBuf>,
) -> Result<SpawnRequest, LauncherError> {
    resolve_spawn_request_with(working_dir, server_binary, binary_is_available)
}

fn resolve_spawn_request_with<F>(
    working_dir: Option<PathBuf>,
    server_binary: Option<PathBuf>,
    mut binary_available: F,
) -> Result<SpawnRequest, LauncherError>
where
    F: FnMut(&Path, Option<&Path>) -> bool,
{
    if let Some(binary) = server_binary {
        return if binary_available(&binary, working_dir.as_deref()) {
            Ok(SpawnRequest {
                working_dir,
                target: SpawnTarget::Binary(binary),
            })
        } else {
            Err(LauncherError::BinaryNotFound { binary })
        };
    }

    let default_binary = default_server_binary_path();
    if binary_available(&default_binary, working_dir.as_deref()) {
        return Ok(SpawnRequest {
            working_dir,
            target: SpawnTarget::Binary(default_binary),
        });
    }

    if let Some(workspace_root) = find_workspace_root(working_dir.as_deref()) {
        return Ok(SpawnRequest {
            working_dir,
            target: SpawnTarget::CargoWorkspace { workspace_root },
        });
    }

    Err(LauncherError::BinaryNotFound {
        binary: default_binary,
    })
}

fn resolve_run_info_path(custom_path: Option<&PathBuf>) -> Result<PathBuf, LauncherError> {
    if let Some(path) = custom_path {
        return Ok(path.clone());
    }

    let home_dir = resolve_home_dir().map_err(|error| LauncherError::RunInfoUnavailable {
        message: error.to_string(),
    })?;
    Ok(home_dir.join(".astrcode").join("run.json"))
}

fn require_explicit_token<'a>(
    origin: &str,
    token: Option<&'a str>,
) -> Result<&'a str, LauncherError> {
    let Some(token) = token.filter(|token| !token.trim().is_empty()) else {
        return Err(LauncherError::MissingBootstrapToken {
            origin: normalize_origin(origin),
        });
    };
    Ok(token.trim())
}

fn normalize_origin(origin: &str) -> String {
    origin.trim_end_matches('/').to_string()
}

fn run_info_is_fresh(run_info: &LocalServerInfo) -> bool {
    current_timestamp_ms() <= run_info.expires_at_ms
}

fn current_timestamp_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

async fn validate_bootstrap_token<B>(
    backend: &B,
    origin: &str,
    expected_token: &str,
) -> Result<(), LauncherError>
where
    B: LauncherBackend,
{
    match backend.probe_bootstrap_token(origin).await? {
        Some(token) if token == expected_token => Ok(()),
        _ => Err(LauncherError::InvalidBootstrapToken {
            origin: normalize_origin(origin),
        }),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunInfoProbeResponse {
    token: String,
}

pub struct SystemManagedServer {
    child: Child,
    stdout: Option<BufReader<ChildStdout>>,
    debug_tap: DebugLogTap,
}

#[derive(Debug, Clone, Default)]
pub struct DebugLogTap {
    inner: Arc<Mutex<VecDeque<String>>>,
}

impl DebugLogTap {
    pub fn drain(&self) -> Vec<String> {
        let mut inner = self.inner.lock().expect("debug tap lock poisoned");
        inner.drain(..).collect()
    }

    fn push(&self, line: impl Into<String>) {
        let mut inner = self.inner.lock().expect("debug tap lock poisoned");
        if inner.len() >= 256 {
            inner.pop_front();
        }
        inner.push_back(line.into());
    }
}

impl SystemManagedServer {
    pub fn debug_tap(&self) -> DebugLogTap {
        self.debug_tap.clone()
    }
}

#[async_trait]
impl LauncherBackend for SystemLauncherBackend {
    type ManagedServer = SystemManagedServer;

    async fn read_run_info(&self, path: &Path) -> Result<Option<LocalServerInfo>, LauncherError> {
        match tokio::fs::read_to_string(path).await {
            Ok(raw) => serde_json::from_str(&raw).map(Some).map_err(|error| {
                LauncherError::RunInfoUnavailable {
                    message: format!("failed to parse '{}': {error}", path.display()),
                }
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(LauncherError::RunInfoUnavailable {
                message: format!("failed to read '{}': {error}", path.display()),
            }),
        }
    }

    async fn probe_bootstrap_token(&self, origin: &str) -> Result<Option<String>, LauncherError> {
        let url = format!("{}/__astrcode__/run-info", normalize_origin(origin));
        let response = reqwest::get(&url)
            .await
            .map_err(|error| LauncherError::ProbeFailed {
                origin: normalize_origin(origin),
                message: error.to_string(),
            })?;

        if response.status() != StatusCode::OK {
            return Ok(None);
        }

        let body = response
            .json::<RunInfoProbeResponse>()
            .await
            .map_err(|error| LauncherError::ProbeFailed {
                origin: normalize_origin(origin),
                message: format!("failed to decode run-info probe response: {error}"),
            })?;

        Ok(Some(body.token))
    }

    async fn spawn_server(
        &self,
        request: &SpawnRequest,
    ) -> Result<Self::ManagedServer, LauncherError> {
        let debug_tap = DebugLogTap::default();
        let mut command = match &request.target {
            SpawnTarget::Binary(binary) => {
                let mut command = Command::new(binary);
                if let Some(working_dir) = request.working_dir.as_ref() {
                    command.current_dir(working_dir);
                }
                command
            },
            SpawnTarget::CargoWorkspace { workspace_root } => {
                let mut command = Command::new("cargo");
                command
                    .current_dir(workspace_root)
                    .arg("run")
                    .arg("-p")
                    .arg("astrcode-server");
                command
            },
        };
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped());

        let mut child = command.spawn().map_err(|error| match &request.target {
            SpawnTarget::Binary(binary) => LauncherError::SpawnFailed {
                message: format!("failed to spawn '{}': {error}", binary.display()),
            },
            SpawnTarget::CargoWorkspace { workspace_root } => LauncherError::CargoSpawnFailed {
                workspace_root: workspace_root.clone(),
                message: error.to_string(),
            },
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LauncherError::SpawnFailed {
                message: "spawned server did not expose stdout".to_string(),
            })?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stream(
                stderr,
                debug_tap.clone(),
                "astrcode-server stderr",
            ));
        }

        Ok(SystemManagedServer {
            child,
            stdout: Some(BufReader::new(stdout)),
            debug_tap,
        })
    }
}

fn default_server_binary_path() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("astrcode-server.exe")
    } else {
        PathBuf::from("astrcode-server")
    }
}

fn binary_is_available(binary: &Path, working_dir: Option<&Path>) -> bool {
    if is_path_like(binary) {
        return explicit_binary_exists(binary, working_dir);
    }

    if let Some(current_dir) = working_dir
        .map(Path::to_path_buf)
        .or_else(current_directory)
    {
        if current_dir.join(binary).is_file() {
            return true;
        }
    }

    let Some(path_env) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path_env).any(|directory| named_binary_exists_in_dir(&directory, binary))
}

fn explicit_binary_exists(binary: &Path, working_dir: Option<&Path>) -> bool {
    if binary.is_absolute() {
        return binary.is_file();
    }

    let Some(base_dir) = working_dir
        .map(Path::to_path_buf)
        .or_else(current_directory)
    else {
        return false;
    };
    base_dir.join(binary).is_file()
}

fn is_path_like(path: &Path) -> bool {
    path.is_absolute()
        || path.parent().is_some()
        || path.components().count() > 1
        || path.to_string_lossy().contains(std::path::MAIN_SEPARATOR)
}

fn named_binary_exists_in_dir(directory: &Path, binary: &Path) -> bool {
    let has_extension = binary.extension().is_some();
    if directory.join(binary).is_file() {
        return true;
    }

    if cfg!(windows) && !has_extension {
        return windows_binary_extensions()
            .into_iter()
            .map(|extension| {
                let mut candidate: OsString = binary.as_os_str().to_os_string();
                candidate.push(extension);
                directory.join(candidate)
            })
            .any(|candidate| candidate.is_file());
    }

    false
}

fn windows_binary_extensions() -> Vec<String> {
    static WINDOWS_BINARY_EXTENSIONS: OnceLock<Vec<String>> = OnceLock::new();
    WINDOWS_BINARY_EXTENSIONS
        .get_or_init(|| {
            env::var_os("PATHEXT")
                .map(|raw| {
                    raw.to_string_lossy()
                        .split(';')
                        .filter_map(|item| {
                            let trimmed = item.trim();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(trimmed.to_ascii_lowercase())
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|extensions| !extensions.is_empty())
                .unwrap_or_else(|| {
                    vec![
                        ".com".to_string(),
                        ".exe".to_string(),
                        ".bat".to_string(),
                        ".cmd".to_string(),
                    ]
                })
        })
        .clone()
}

fn current_directory() -> Option<PathBuf> {
    env::current_dir().ok()
}

fn find_workspace_root(start: Option<&Path>) -> Option<PathBuf> {
    let start_dir = start.map(Path::to_path_buf).or_else(current_directory)?;
    start_dir.ancestors().find_map(|candidate| {
        let cargo_toml = candidate.join("Cargo.toml");
        let server_crate = candidate.join("crates").join("server");
        if !cargo_toml.is_file() || !server_crate.is_dir() {
            return None;
        }

        let cargo_content = fs::read_to_string(&cargo_toml).ok()?;
        if cargo_content.contains("\"crates/server\"") {
            Some(candidate.to_path_buf())
        } else {
            None
        }
    })
}

async fn drain_stream<R>(stream: R, debug_tap: DebugLogTap, channel: &'static str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        debug_tap.push(format!("[{channel}] {line}"));
    }
}

#[async_trait]
impl ManagedServerHandle for SystemManagedServer {
    async fn wait_ready(
        &mut self,
        timeout_duration: Duration,
    ) -> Result<LocalServerInfo, LauncherError> {
        let mut lines = self
            .stdout
            .take()
            .ok_or_else(|| LauncherError::ReadyHandshakeFailed {
                message: "server stdout stream is not available".to_string(),
            })?
            .lines();

        let ready = timeout(timeout_duration, async {
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match LocalServerInfo::parse_ready_line(&line) {
                        Ok(Some(info)) => return Ok((info, lines)),
                        Ok(None) => continue,
                        Err(error) => {
                            return Err(LauncherError::ReadyHandshakeFailed {
                                message: format!("failed to parse ready line: {error}"),
                            });
                        },
                    },
                    Ok(None) => {
                        return Err(LauncherError::ReadyHandshakeFailed {
                            message: "server exited before reporting ready".to_string(),
                        });
                    },
                    Err(error) => {
                        return Err(LauncherError::ReadyHandshakeFailed {
                            message: format!("failed to read server stdout: {error}"),
                        });
                    },
                }
            }
        })
        .await
        .map_err(|_| LauncherError::ReadyHandshakeFailed {
            message: "timed out waiting for server ready handshake".to_string(),
        })??;

        let (info, mut lines) = ready;
        let debug_tap = self.debug_tap.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                debug_tap.push(format!("[astrcode-server stdout] {line}"));
            }
        });

        Ok(info)
    }

    async fn terminate(&mut self) -> Result<(), LauncherError> {
        self.child
            .kill()
            .await
            .map_err(|error| LauncherError::SpawnFailed {
                message: format!("failed to terminate managed server: {error}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use super::*;

    #[derive(Debug)]
    struct MockManagedServer {
        ready_result: Result<LocalServerInfo, LauncherError>,
        terminated: bool,
    }

    #[async_trait]
    impl ManagedServerHandle for MockManagedServer {
        async fn wait_ready(
            &mut self,
            _timeout: Duration,
        ) -> Result<LocalServerInfo, LauncherError> {
            self.ready_result.clone()
        }

        async fn terminate(&mut self) -> Result<(), LauncherError> {
            self.terminated = true;
            Ok(())
        }
    }

    #[derive(Debug)]
    enum MockCall {
        ReadRunInfo(Result<Option<LocalServerInfo>, LauncherError>),
        Probe(Result<Option<String>, LauncherError>),
        Spawn {
            expected: SpawnRequest,
            result: Result<MockManagedServer, LauncherError>,
        },
    }

    #[derive(Debug, Default, Clone)]
    struct MockBackend {
        calls: Arc<Mutex<VecDeque<MockCall>>>,
    }

    impl MockBackend {
        fn push(&self, call: MockCall) {
            self.calls
                .lock()
                .expect("mock lock poisoned")
                .push_back(call);
        }
    }

    #[async_trait]
    impl LauncherBackend for MockBackend {
        type ManagedServer = MockManagedServer;

        async fn read_run_info(
            &self,
            _path: &Path,
        ) -> Result<Option<LocalServerInfo>, LauncherError> {
            match self
                .calls
                .lock()
                .expect("mock lock poisoned")
                .pop_front()
                .expect("expected read call")
            {
                MockCall::ReadRunInfo(result) => result,
                other => panic!("expected read call, got {other:?}"),
            }
        }

        async fn probe_bootstrap_token(
            &self,
            _origin: &str,
        ) -> Result<Option<String>, LauncherError> {
            match self
                .calls
                .lock()
                .expect("mock lock poisoned")
                .pop_front()
                .expect("expected probe call")
            {
                MockCall::Probe(result) => result,
                other => panic!("expected probe call, got {other:?}"),
            }
        }

        async fn spawn_server(
            &self,
            request: &SpawnRequest,
        ) -> Result<Self::ManagedServer, LauncherError> {
            match self
                .calls
                .lock()
                .expect("mock lock poisoned")
                .pop_front()
                .expect("expected spawn call")
            {
                MockCall::Spawn { expected, result } => {
                    assert_eq!(request, &expected);
                    result
                },
                other => panic!("expected spawn call, got {other:?}"),
            }
        }
    }

    fn sample_run_info() -> LocalServerInfo {
        LocalServerInfo {
            port: 5529,
            token: "bootstrap-token".to_string(),
            pid: 42,
            started_at: "2026-04-15T12:00:00+08:00".to_string(),
            expires_at_ms: current_timestamp_ms() + 30_000,
        }
    }

    #[tokio::test]
    async fn attaches_to_existing_server_from_run_info() {
        let backend = MockBackend::default();
        backend.push(MockCall::ReadRunInfo(Ok(Some(sample_run_info()))));
        backend.push(MockCall::Probe(Ok(Some("bootstrap-token".to_string()))));

        let launcher = Launcher::with_backend(backend);
        let session = launcher
            .resolve(LaunchOptions::default())
            .await
            .expect("run info attach should succeed");

        assert_eq!(
            session.connection(),
            &ResolvedConnection {
                origin: "http://127.0.0.1:5529".to_string(),
                bootstrap_token: "bootstrap-token".to_string(),
                working_dir: None,
                source: ConnectionSource::RunInfo,
            }
        );
    }

    #[tokio::test]
    async fn spawns_local_server_when_run_info_is_missing() {
        let backend = MockBackend::default();
        let current_binary = std::env::current_exe().expect("current test binary");
        backend.push(MockCall::ReadRunInfo(Ok(None)));
        backend.push(MockCall::Spawn {
            expected: SpawnRequest {
                working_dir: None,
                target: SpawnTarget::Binary(current_binary.clone()),
            },
            result: Ok(MockManagedServer {
                ready_result: Ok(sample_run_info()),
                terminated: false,
            }),
        });

        let launcher = Launcher::with_backend(backend);
        let session = launcher
            .resolve(LaunchOptions {
                server_binary: Some(current_binary),
                ..LaunchOptions::default()
            })
            .await
            .expect("spawn should succeed");

        assert_eq!(session.connection().source, ConnectionSource::SpawnedLocal);
        assert!(session.connection().origin.ends_with(":5529"));
    }

    #[tokio::test]
    async fn fails_when_spawned_server_never_reports_ready() {
        let backend = MockBackend::default();
        let current_binary = std::env::current_exe().expect("current test binary");
        backend.push(MockCall::ReadRunInfo(Ok(None)));
        backend.push(MockCall::Spawn {
            expected: SpawnRequest {
                working_dir: None,
                target: SpawnTarget::Binary(current_binary.clone()),
            },
            result: Ok(MockManagedServer {
                ready_result: Err(LauncherError::ReadyHandshakeFailed {
                    message: "stdout closed before ready".to_string(),
                }),
                terminated: false,
            }),
        });

        let launcher = Launcher::with_backend(backend);
        let error = launcher
            .resolve(LaunchOptions {
                server_binary: Some(current_binary),
                ..LaunchOptions::default()
            })
            .await
            .expect_err("ready failure should surface");

        assert!(matches!(error, LauncherError::ReadyHandshakeFailed { .. }));
    }

    #[tokio::test]
    async fn rejects_invalid_remote_bootstrap_token() {
        let backend = MockBackend::default();
        backend.push(MockCall::Probe(Ok(Some("different-token".to_string()))));

        let launcher = Launcher::with_backend(backend);
        let error = launcher
            .resolve(LaunchOptions {
                server_origin: Some("http://remote.example".to_string()),
                bootstrap_token: Some("bad-token".to_string()),
                ..LaunchOptions::default()
            })
            .await
            .expect_err("invalid remote token should fail");

        assert_eq!(
            error,
            LauncherError::InvalidBootstrapToken {
                origin: "http://remote.example".to_string(),
            }
        );
    }

    #[test]
    fn resolve_spawn_request_uses_repo_aware_cargo_fallback_when_binary_is_missing() {
        let request = resolve_spawn_request_with(
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
            None,
            |_binary, _cwd| false,
        )
        .expect("workspace fallback should resolve");

        assert!(matches!(
            request.target,
            SpawnTarget::CargoWorkspace { ref workspace_root }
                if workspace_root.ends_with("Astrcode")
        ));
    }

    #[test]
    fn resolve_spawn_request_reports_missing_explicit_binary() {
        let error = resolve_spawn_request_with(
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
            Some(PathBuf::from("__missing_astrcode_server__")),
            |_binary, _cwd| false,
        )
        .expect_err("missing explicit binary should fail fast");

        assert_eq!(
            error,
            LauncherError::BinaryNotFound {
                binary: PathBuf::from("__missing_astrcode_server__"),
            }
        );
    }

    #[tokio::test]
    async fn uses_repo_aware_cargo_fallback_when_default_binary_is_unavailable() {
        let backend = MockBackend::default();
        let working_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = working_dir
            .ancestors()
            .nth(2)
            .expect("workspace root")
            .to_path_buf();

        backend.push(MockCall::ReadRunInfo(Ok(None)));
        backend.push(MockCall::Spawn {
            expected: SpawnRequest {
                working_dir: Some(working_dir.clone()),
                target: SpawnTarget::CargoWorkspace {
                    workspace_root: workspace_root.clone(),
                },
            },
            result: Ok(MockManagedServer {
                ready_result: Ok(sample_run_info()),
                terminated: false,
            }),
        });

        let launcher = Launcher::with_backend(backend);
        let session = launcher
            .resolve(LaunchOptions {
                working_dir: Some(working_dir),
                ..LaunchOptions::default()
            })
            .await
            .expect("repo-aware fallback should succeed");

        assert_eq!(session.connection().source, ConnectionSource::SpawnedLocal);
    }
}
