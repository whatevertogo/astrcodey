//! 服务器组装：路由注册、TCP 启动、`run.json` 写入。

use std::sync::Arc;

use astrcode_protocol::events::ClientNotification;
use axum::{
    Router,
    http::{Method, header},
    middleware,
    routing::{delete, get, post},
    serve::ListenerExt,
};
use tokio::sync::broadcast;
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::{
    HttpState,
    auth::{auth_middleware, collect_allowed_origins, configured_auth_token},
    routes::{config, lifecycle, models, sessions},
    stream,
};
use crate::{bootstrap::ServerRuntime, handler::CommandHandler};

/// HTTP server startup and runtime errors.
#[derive(Debug, thiserror::Error)]
pub enum HttpServerError {
    /// Failed to generate or read auth token.
    #[error("auth token error")]
    Auth(getrandom::Error),
    /// I/O error during server operation.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<getrandom::Error> for HttpServerError {
    fn from(e: getrandom::Error) -> Self {
        HttpServerError::Auth(e)
    }
}

/// Build an axum router for the HTTP/SSE API.
///
/// Returns `(Router, auth_token)` — the token must be passed to the frontend
/// so it can include it in `Authorization: Bearer <token>` headers.
pub fn router(
    runtime: Arc<ServerRuntime>,
    event_tx: broadcast::Sender<ClientNotification>,
) -> Result<(Router, String), HttpServerError> {
    let auth_token = configured_auth_token()?;
    let event_bus = Arc::new(crate::server_event_bus::ServerEventBus::new(
        runtime.event_store.clone(),
        event_tx.clone(),
    ));
    {
        let event_bus = Arc::clone(&event_bus);
        runtime
            .session_manager
            .set_attach_hook(Arc::new(move |session| {
                event_bus.attach(session);
            }));
    }
    let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), Arc::clone(&event_bus));
    let state = HttpState {
        runtime,
        handler,
        event_bus,
    };
    let expected_bearer = format!("Bearer {auth_token}");

    let allowed_origins = collect_allowed_origins();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed_origins))
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            header::CACHE_CONTROL,
        ]);

    let app = Router::new()
        .route(
            "/api/sessions",
            post(sessions::create_session).get(sessions::list_sessions),
        )
        .route(
            "/api/sessions/{id}/conversation",
            get(sessions::conversation_snapshot),
        )
        .route("/api/sessions/{id}/stream", get(stream::session_stream))
        .route("/api/sessions/{id}/prompt", post(sessions::submit_prompt))
        .route("/api/sessions/{id}/commands", get(sessions::list_commands))
        .route(
            "/api/sessions/{id}/compact",
            post(sessions::compact_session),
        )
        .route("/api/sessions/{id}/abort", post(sessions::abort_session))
        .route("/api/sessions/{id}/fork", post(sessions::fork_session))
        .route("/api/sessions/{id}", delete(sessions::delete_session))
        .route("/api/projects", delete(sessions::delete_project))
        .route("/api/config", get(config::get_config))
        .route("/api/config/reload", post(config::reload_config))
        .route(
            "/api/config/active-selection",
            post(config::update_active_selection),
        )
        .route("/api/models/current", get(models::get_current_model))
        .route("/api/models", get(models::list_models))
        .route("/api/models/test", post(models::test_model))
        .route("/api/shutdown", post(lifecycle::shutdown))
        .layer(middleware::from_fn_with_state(
            expected_bearer,
            auth_middleware,
        ))
        .layer(cors)
        .with_state(state);

    Ok((app, auth_token))
}

/// Convenience wrapper: build router and run until graceful shutdown.
///
/// 关于 TCP_NODELAY：SSE 末尾事件常常是单独一小条（如 `turn_completed`），不开
/// TCP_NODELAY 时 Linux Nagle 会把短小写积累 ~40-200ms 再 flush，体感上 UI
/// 一直停在「生成中」。两条 HTTP 入口都要做这一步：
///
/// - 这里：被 `astrcode-cli` 用
/// - `http_main.rs::main`：独立 HTTP 二进制
///
/// 改一处时记得同步另一处。`ListenerExt::tap_io` 返回的类型与 `TcpListener`
/// 不兼容，所以两处都要直接对 bind 结果链式调 `tap_io`，不便共用 helper。
pub async fn run_http_server(
    runtime: Arc<ServerRuntime>,
    addr: std::net::SocketAddr,
) -> Result<(), HttpServerError> {
    let (event_tx, _) = broadcast::channel(256);
    let shutdown_token = runtime.shutdown_token.clone();
    let (app, auth_token) = router(Arc::clone(&runtime), event_tx)?;
    tracing::info!(
        "Auth token: {}...{}",
        &auth_token[..4],
        &auth_token[auth_token.len() - 4..]
    );
    let listener = tokio::net::TcpListener::bind(addr).await?.tap_io(|stream| {
        let _ = stream.set_nodelay(true);
    });
    let local_port = addr.port();
    write_run_info(local_port, &auth_token);
    tracing::info!("HTTP server ready at http://{addr}");
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
            tracing::info!("graceful shutdown triggered");
        })
        .await;
    remove_run_info();
    result?;
    Ok(())
}

/// 将运行时端口写入 `~/.astrcode/run.json`，供前端 dev server 发现后端地址。
///
/// 文件权限设为 600（仅属主可读写），因为其中含 auth token。
pub fn write_run_info(port: u16, auth_token: &str) {
    let dir = astrcode_support::hostpaths::astrcode_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(path = %dir.display(), error = %e, "failed to create astrcode dir for run.json");
        return;
    }
    let path = dir.join("run.json");
    let content = serde_json::json!({
        "port": port,
        "authToken": auth_token,
    })
    .to_string();
    if let Err(e) = std::fs::write(&path, &content) {
        tracing::warn!(path = %path.display(), error = %e, "failed to write run.json");
        return;
    }
    // 防止同机用户通过 `~/.astrcode/run.json` 读取到该进程的 auth token
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(&path, perms) {
            tracing::warn!(path = %path.display(), error = %e, "failed to chmod 600 run.json");
        }
    }
}

/// 退出时清理 `run.json`。
pub fn remove_run_info() {
    let path = astrcode_support::hostpaths::astrcode_dir().join("run.json");
    let _ = std::fs::remove_file(path);
}
