//! Session picker 相关的展示辅助函数。

use std::path::Path;

use chrono::{DateTime, Utc};

/// 将工作目录路径规范化以用于会话过滤比较。
///
/// 优先调用 `canonicalize` 解析符号链接和相对路径，失败时回退到去尾部斜杠的形式。
/// 这样即使 session 元数据中的路径风格略有差异（结尾斜杠、相对路径），
/// 也能正确匹配到当前进程的 cwd。
pub fn canonicalize_working_dir(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    if let Ok(canon) = std::fs::canonicalize(Path::new(path)) {
        return canon.to_string_lossy().into_owned();
    }
    // 路径不存在或不可访问时的回退：去掉尾部斜杠（根 `/` 除外）。
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_string()
    }
}

/// 将 RFC3339/ISO8601 时间字符串格式化为相对当前时间的简短描述。
///
/// 示例输出：`now`、`5m`、`2h`、`3d`、`2w`、`6mo`、`1y`。
/// 解析失败时返回空字符串，调用方可据此决定是否展示。
pub fn format_relative_time(timestamp: &str, now: DateTime<Utc>) -> String {
    let parsed = match DateTime::parse_from_rfc3339(timestamp) {
        Ok(t) => t.with_timezone(&Utc),
        Err(_) => return String::new(),
    };
    let delta = now.signed_duration_since(parsed);
    // 未来时间（时钟漂移）按 now 处理。
    let secs = delta.num_seconds().max(0);

    if secs < 60 {
        "now".into()
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else if secs < 7 * 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs < 30 * 86_400 {
        format!("{}w", secs / (7 * 86_400))
    } else if secs < 365 * 86_400 {
        format!("{}mo", secs / (30 * 86_400))
    } else {
        format!("{}y", secs / (365 * 86_400))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap()
    }

    #[test]
    fn formats_recent_as_now() {
        let ts = (now() - chrono::Duration::seconds(30)).to_rfc3339();
        assert_eq!(format_relative_time(&ts, now()), "now");
    }

    #[test]
    fn formats_minutes_hours_days() {
        let m = (now() - chrono::Duration::minutes(5)).to_rfc3339();
        assert_eq!(format_relative_time(&m, now()), "5m");
        let h = (now() - chrono::Duration::hours(3)).to_rfc3339();
        assert_eq!(format_relative_time(&h, now()), "3h");
        let d = (now() - chrono::Duration::days(2)).to_rfc3339();
        assert_eq!(format_relative_time(&d, now()), "2d");
    }

    #[test]
    fn formats_weeks_months_years() {
        let w = (now() - chrono::Duration::days(10)).to_rfc3339();
        assert_eq!(format_relative_time(&w, now()), "1w");
        let mo = (now() - chrono::Duration::days(60)).to_rfc3339();
        assert_eq!(format_relative_time(&mo, now()), "2mo");
        let y = (now() - chrono::Duration::days(400)).to_rfc3339();
        assert_eq!(format_relative_time(&y, now()), "1y");
    }

    #[test]
    fn returns_empty_on_invalid_input() {
        assert_eq!(format_relative_time("not-a-date", now()), "");
        assert_eq!(format_relative_time("", now()), "");
    }

    #[test]
    fn handles_future_timestamps_as_now() {
        let ts = (now() + chrono::Duration::minutes(10)).to_rfc3339();
        assert_eq!(format_relative_time(&ts, now()), "now");
    }

    #[test]
    fn canonicalize_strips_trailing_slash_when_path_missing() {
        // 不存在的路径走回退分支
        assert_eq!(canonicalize_working_dir("/no/such/path/"), "/no/such/path");
        assert_eq!(canonicalize_working_dir("/"), "/");
        assert_eq!(canonicalize_working_dir(""), "");
    }

    #[test]
    fn canonicalize_resolves_existing_path() {
        // 用 std::env::temp_dir 这种存在的路径验证 canonicalize 路径起效
        let tmp = std::env::temp_dir();
        let with_slash = format!("{}/", tmp.display());
        let canon = canonicalize_working_dir(&with_slash);
        // canonicalize 后没有尾部斜杠
        assert!(!canon.ends_with('/') || canon == "/");
        // 与直接 canonicalize 同一路径结果相同
        let expected = std::fs::canonicalize(&tmp)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(canon, expected);
    }
}
