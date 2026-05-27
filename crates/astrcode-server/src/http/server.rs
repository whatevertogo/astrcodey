//! 服务器组装：路由注册、TCP 启动、`run.json` 写入。

use std::{path::Path, sync::Arc};

use astrcode_protocol::events::ClientNotification;
use astrcode_support::event_fanout::EventFanout;
use axum::{
    Router,
    http::{Method, header},
    middleware,
    routing::{delete, get, post},
};
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::{
    HttpState,
    auth::{auth_middleware, collect_allowed_origins, configured_auth_token},
    routes::{acp, config, extensions, lifecycle, models, sessions},
    stream,
};
use crate::bootstrap::ServerRuntime;

/// HTTP server startup and runtime errors.
#[derive(Debug, thiserror::Error)]
pub enum HttpServerError {
    /// I/O error during server operation.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Build an axum router for the HTTP/SSE API.
///
/// Returns `(Router, auth_token)` — the token must be passed to the frontend
/// so it can include it in `Authorization: Bearer <token>` headers.
pub fn router(
    runtime: Arc<ServerRuntime>,
    event_tx: Arc<EventFanout<ClientNotification>>,
) -> Result<(Router, String), HttpServerError> {
    let auth_token = configured_auth_token();
    let server_system = crate::bootstrap::spawn_server_system(&runtime, Arc::clone(&event_tx));
    let state = HttpState {
        runtime,
        handler: server_system.handler,
        event_bus: server_system.event_bus,
        event_tx: Arc::clone(&event_tx),
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
        .route("/api/extensions", get(extensions::list_extensions))
        .route(
            "/api/extensions/reload",
            post(extensions::reload_extensions),
        )
        .route("/api/extensions/set-enabled", post(extensions::set_enabled))
        .route("/api/models/current", get(models::get_current_model))
        .route("/api/models", get(models::list_models))
        .route("/api/models/test", post(models::test_model))
        .route(
            "/api/models/small/current",
            get(models::get_small_current_model),
        )
        .route("/api/models/small/test", post(models::test_small_model))
        .route("/api/shutdown", post(lifecycle::shutdown))
        .route("/api/acp/ws", get(acp::acp_websocket))
        .layer(middleware::from_fn_with_state(
            expected_bearer,
            auth_middleware,
        ))
        .layer(cors)
        .with_state(state);

    Ok((app, auth_token))
}

/// Convenience wrapper: build router and run until graceful shutdown.
pub async fn run_http_server(
    runtime: Arc<ServerRuntime>,
    addr: std::net::SocketAddr,
) -> Result<(), HttpServerError> {
    let event_tx = Arc::new(EventFanout::new(1024));
    let shutdown_token = runtime.shutdown_token().clone();
    let runtime_for_shutdown = Arc::clone(&runtime);
    let (app, auth_token) = router(Arc::clone(&runtime), event_tx)?;
    tracing::info!("Auth token: {}", masked_token(&auth_token));

    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|error| {
        tracing::error!("failed to bind HTTP server at {addr}: {error}");
        HttpServerError::Io(error)
    })?;
    let local_addr = listener.local_addr()?;
    let local_port = local_addr.port();
    write_run_info(local_port, &auth_token);
    tracing::info!("HTTP server ready at http://{local_addr}");
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
            tracing::info!("graceful shutdown triggered");
            runtime_for_shutdown.shutdown_extensions().await;
        })
        .await;
    remove_run_info_if_current(local_port, &auth_token);
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
    write_run_info_at(&path, port, auth_token);
}

fn write_run_info_at(path: &Path, port: u16, auth_token: &str) {
    let content = serde_json::json!({
        "port": port,
        "authToken": auth_token,
    })
    .to_string();
    if let Err(e) = std::fs::write(path, &content) {
        tracing::warn!(path = %path.display(), error = %e, "failed to write run.json");
    }
    // 防止同机用户通过 `~/.astrcode/run.json` 读取到该进程的 auth token
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(path, perms) {
            tracing::warn!(path = %path.display(), error = %e, "failed to chmod 600 run.json");
        }
    }
}

/// 退出时清理 `run.json`。
pub fn remove_run_info() {
    let path = astrcode_support::hostpaths::astrcode_dir().join("run.json");
    let _ = std::fs::remove_file(path);
}

fn remove_run_info_if_current(port: u16, auth_token: &str) {
    let path = astrcode_support::hostpaths::astrcode_dir().join("run.json");
    remove_run_info_if_current_at(&path, port, auth_token);
}

fn remove_run_info_if_current_at(path: &Path, port: u16, auth_token: &str) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return;
    };
    let matches_current = value.get("port").and_then(serde_json::Value::as_u64)
        == Some(port as u64)
        && value.get("authToken").and_then(serde_json::Value::as_str) == Some(auth_token);
    if matches_current {
        let _ = std::fs::remove_file(path);
    }
}

fn masked_token(token: &str) -> String {
    let chars: Vec<_> = token.chars().collect();
    if chars.len() <= 8 {
        return "<redacted>".into();
    }
    let prefix: String = chars.iter().take(4).collect();
    let suffix: String = chars.iter().skip(chars.len().saturating_sub(4)).collect();
    format!("{prefix}...{suffix}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{masked_token, remove_run_info_if_current_at, write_run_info_at};

    #[test]
    fn masked_token_handles_short_env_tokens() {
        assert_eq!(masked_token("abc"), "<redacted>");
        assert_eq!(masked_token("12345678"), "<redacted>");
        assert_eq!(masked_token("123456789"), "1234...6789");
    }

    #[test]
    fn remove_run_info_only_removes_matching_server() {
        let root = std::env::temp_dir().join(format!(
            "astrcode-run-info-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("run.json");

        write_run_info_at(&path, 1111, "old-token");
        write_run_info_at(&path, 2222, "new-token");

        remove_run_info_if_current_at(&path, 1111, "old-token");
        assert!(path.exists());

        remove_run_info_if_current_at(&path, 2222, "new-token");
        assert!(!path.exists());

        let _ = fs::remove_dir_all(root);
    }
}
