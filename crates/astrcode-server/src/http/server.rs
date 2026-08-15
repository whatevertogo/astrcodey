//! 服务器组装：路由注册、TCP 启动、`run.json` 写入。

use std::{
    io::{self, Write},
    path::Path,
    sync::Arc,
};

use astrcode_protocol::http::RunInfoDto;
use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{Method, header},
    routing::{any, delete, get, post, put},
};
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::{
    HttpState,
    auth::{collect_allowed_origins, configured_auth_token},
    routes::{config, event_consumers, extensions, lifecycle, models, sessions},
    stream,
};
use crate::{bootstrap::ServerApp, server_event_bus::ServerEventBus};

type RouterParts = (Router, String, Arc<ServerEventBus>);

const MAX_PROMPT_HTTP_BODY_BYTES: usize = crate::turn_scheduler::MAX_PROMPT_TEXT_BYTES
    + astrcode_core::message_attachment::MAX_ATTACHMENTS
        * astrcode_core::message_attachment::MAX_ATTACHMENT_CONTENT_BYTES
    + 64 * 1024;
const RUN_INFO_TEMPFILE_PREFIX: &str = ".run.json.";

/// HTTP server startup and runtime errors.
#[derive(Debug, thiserror::Error)]
pub enum HttpServerError {
    /// I/O error during server operation.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Build an axum router for the HTTP/SSE API.
///
/// Returns `(Router, auth_token)` — the token is retained for `run.json`/client
/// compatibility but is no longer enforced by the server.
pub fn router(server_app: Arc<ServerApp>) -> Result<(Router, String), HttpServerError> {
    let (router, auth_token, _) = router_parts(server_app);
    Ok((router, auth_token))
}

fn router_parts(server_app: Arc<ServerApp>) -> RouterParts {
    let auth_token = configured_auth_token();
    let event_bus = Arc::clone(server_app.event_bus());
    let state = HttpState { app: server_app };

    let allowed_origins = collect_allowed_origins();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed_origins))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            header::CACHE_CONTROL,
        ]);

    let protected_api = Router::new()
        .route(
            "/api/sessions",
            post(sessions::create_session).get(sessions::list_sessions),
        )
        .route(
            "/api/sessions/{id}/conversation",
            get(sessions::conversation_snapshot),
        )
        .route(
            "/api/sessions/{id}/tools",
            put(sessions::configure_session_tools),
        )
        .route("/api/sessions/{id}/stream", get(stream::session_stream))
        .route(
            "/api/sessions/{id}/event-consumers",
            get(event_consumers::list_event_consumers),
        )
        .route(
            "/api/sessions/{id}/event-consumers/control",
            post(event_consumers::control_event_consumer),
        )
        .route(
            "/api/sessions/{id}/prompt",
            post(sessions::submit_prompt).layer(DefaultBodyLimit::max(MAX_PROMPT_HTTP_BODY_BYTES)),
        )
        .route("/api/sessions/{id}/inject", post(sessions::inject_message))
        .route(
            "/api/sessions/{id}/approve",
            post(sessions::resolve_tool_approval),
        )
        .route("/api/sessions/{id}/commands", get(sessions::list_commands))
        .route(
            "/api/sessions/{id}/commands/{name}/complete",
            post(sessions::complete_command),
        )
        .route(
            "/api/sessions/{id}/commands/{name}",
            post(sessions::invoke_command),
        )
        .route(
            "/api/sessions/{id}/compact",
            post(sessions::compact_session),
        )
        .route("/api/sessions/{id}/abort", post(sessions::abort_session))
        .route("/api/sessions/{id}/fork", post(sessions::fork_session))
        .route("/api/sessions/{id}", delete(sessions::delete_session))
        .route("/api/projects", delete(sessions::delete_project))
        .route("/api/config", get(config::get_config))
        .route(
            "/api/config/provider-catalog",
            get(config::get_provider_catalog),
        )
        .route(
            "/api/config/provider-preset/apply",
            post(config::apply_provider_preset),
        )
        .route(
            "/api/config/provider-preset/remove",
            post(config::remove_provider_preset),
        )
        .route("/api/config/reload", post(config::reload_config))
        .route(
            "/api/config/active-selection",
            post(config::update_active_selection),
        )
        .route(
            "/api/config/model-options",
            post(config::update_model_options),
        )
        .route("/api/extensions", get(extensions::list_extensions))
        .route(
            "/api/extensions/reload",
            post(extensions::reload_extensions),
        )
        .route("/api/extensions/set-enabled", post(extensions::set_enabled))
        .route(
            "/api/extensions/{extension_id}/{*path}",
            any(extensions::dispatch_authenticated_http),
        )
        .route("/api/models/current", get(models::get_current_model))
        .route("/api/models", get(models::list_models))
        .route("/api/models/test", post(models::test_model))
        .route(
            "/api/models/small/current",
            get(models::get_small_current_model),
        )
        .route("/api/models/small/test", post(models::test_small_model))
        .route("/api/shutdown", post(lifecycle::shutdown));

    let public_extension_http = Router::new()
        .fallback(extensions::dispatch_public_http)
        .layer(DefaultBodyLimit::max(
            astrcode_extension_sdk::extension::MAX_EXTENSION_HTTP_BODY_BYTES,
        ));
    let app = Router::new()
        .merge(protected_api)
        .merge(public_extension_http)
        .layer(cors)
        .with_state(state);

    (app, auth_token, event_bus)
}

/// Convenience wrapper: build router and run until graceful shutdown.
pub async fn run_http_server(
    server_app: Arc<ServerApp>,
    addr: std::net::SocketAddr,
) -> Result<(), HttpServerError> {
    server_app.initialize().await;
    let shutdown_token = server_app.runtime().shutdown_token().clone();
    let (app, auth_token) = router(Arc::clone(&server_app))?;
    tracing::info!(
        "HTTP auth is disabled; legacy client token: {}",
        masked_token(&auth_token)
    );

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
        })
        .await;
    server_app.shutdown().await;
    remove_run_info_if_current(local_port, &auth_token);
    result?;
    Ok(())
}

/// 将运行时端口写入 `~/.astrcode/run.json`，供前端 dev server 发现后端地址。
///
/// 文件权限设为 600（仅属主可读写），因为其中含 auth token。
fn write_run_info(port: u16, auth_token: &str) {
    let dir = astrcode_core::config::defaults::astrcode_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(path = %dir.display(), error = %e, "failed to create astrcode dir for run.json");
        return;
    }
    let path = dir.join("run.json");
    write_run_info_at(&path, port, auth_token);
}

fn write_run_info_at(path: &Path, port: u16, auth_token: &str) {
    let run_info = RunInfoDto {
        port,
        auth_token: auth_token.into(),
    };
    let Ok(content) = serde_json::to_string(&run_info) else {
        tracing::error!("failed to serialize run.json");
        return;
    };
    if let Err(e) = replace_run_info(path, content.as_bytes()) {
        tracing::warn!(path = %path.display(), error = %e, "failed to write run.json");
    }
}

fn replace_run_info(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "run.json path must have a parent directory",
        )
    })?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(RUN_INFO_TEMPFILE_PREFIX);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }

    let mut temporary = builder.tempfile_in(parent)?;
    temporary.write_all(content)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

/// 退出时清理 `run.json`。
fn remove_run_info_if_current(port: u16, auth_token: &str) {
    let path = astrcode_core::config::defaults::astrcode_dir().join("run.json");
    remove_run_info_if_current_at(&path, port, auth_token);
}

fn remove_run_info_if_current_at(path: &Path, port: u16, auth_token: &str) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(run_info) = serde_json::from_str::<RunInfoDto>(&content) else {
        return;
    };
    let matches_current = run_info.port == port && run_info.auth_token == auth_token;
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

    use astrcode_protocol::http::RunInfoDto;

    use super::{
        RUN_INFO_TEMPFILE_PREFIX, remove_run_info_if_current_at, replace_run_info,
        write_run_info_at,
    };

    #[test]
    fn run_info_is_replaced_completely_and_removed_only_by_current_server() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("run.json");
        fs::write(&path, vec![b'x'; 4096]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }

        write_run_info_at(&path, 2222, "new-token");
        let content = fs::read_to_string(&path).unwrap();
        let run_info: RunInfoDto = serde_json::from_str(&content).unwrap();
        assert_eq!(run_info.port, 2222);
        assert_eq!(run_info.auth_token, "new-token");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let blocked_path = root.path().join("occupied");
        fs::create_dir(&blocked_path).unwrap();
        fs::write(blocked_path.join("old"), "old").unwrap();
        assert!(replace_run_info(&blocked_path, b"new").is_err());
        assert_eq!(fs::read_to_string(blocked_path.join("old")).unwrap(), "old");
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(RUN_INFO_TEMPFILE_PREFIX)
        }));

        remove_run_info_if_current_at(&path, 1111, "old-token");
        assert!(path.exists());

        remove_run_info_if_current_at(&path, 2222, "new-token");
        assert!(!path.exists());
    }
}
