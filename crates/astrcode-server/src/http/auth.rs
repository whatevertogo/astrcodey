//! CORS 来源收集。HTTP 鉴权已停用，token 管道已移除。

use axum::http::HeaderValue;

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
