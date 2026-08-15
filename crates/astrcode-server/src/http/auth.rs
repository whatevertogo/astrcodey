//! HTTP 鉴权中间件、Bearer token 加载与 CORS 来源收集。
//!
//! 鉴权已停用（服务器不再校验 Authorization 头）；token 仍生成并写入
//! `run.json` 以兼容现有客户端，`ASTRCODE_HTTP_TOKEN` 环境变量保留。

use axum::http::HeaderValue;
use uuid::Uuid;

const ASTRCODE_HTTP_TOKEN_ENV: &str = "ASTRCODE_HTTP_TOKEN";

fn generate_auth_token() -> String {
    Uuid::new_v4().simple().to_string()
}

pub(super) fn configured_auth_token() -> String {
    std::env::var(ASTRCODE_HTTP_TOKEN_ENV)
        .ok()
        .filter(|token| !token.trim().is_empty())
        .unwrap_or_else(generate_auth_token)
}

pub(super) fn collect_allowed_origins() -> Vec<HeaderValue> {
    let mut origins = vec![
        "http://localhost:5173",
        "http://localhost:3000",
        "http://127.0.0.1:5173",
        "http://127.0.0.1:3000",
        "tauri://localhost",
        "http://tauri.localhost",
        "https://tauri.localhost",
    ]
    .into_iter()
    .filter_map(|s| s.parse::<HeaderValue>().ok())
    .collect::<Vec<_>>();
    if let Ok(extra) = std::env::var("ASTRCODE_CORS_ORIGINS") {
        for origin in extra.split(',') {
            if let Ok(hv) = origin.trim().parse::<HeaderValue>() {
                origins.push(hv);
            }
        }
    }
    origins
}
