//! In-process `Registrar` 与 worker `HandlerRegistry` 共用的注册校验规则。
//!
//! 两个注册表的校验时机不同（`Registrar::finish` 集中校验 vs worker 插入时
//! 增量校验），错误类型与错误消息也各自保留；这里只沉淀规则本身，消息由
//! 调用点参数化。hook mode 的规则原语已单份定义在 `super::events`
//! （`lifecycle_event_allows_blocking` / `fixed_hook_mode` /
//! `hook_mode_is_supported`），HTTP 路由校验共享 `route.validate()` 与
//! `extension_http_route_patterns_conflict`，本模块不再重复收录。

/// 把扩展作者提供的注册名规范化为 trim 后的形式。
///
/// 两条注册路径都以规范名入库与判重，因此 `"  review  "` 与 `"review"` 同名。
pub fn canonical_registration_name(name: &mut String) {
    *name = name.trim().to_owned();
}

/// 重复注册判定：候选名与任一已注册名精确相等（大小写敏感）。
///
/// `registered` 与 `candidate` 都须先经 [`canonical_registration_name`] 规范化。
pub fn has_duplicate_registration_name<'a>(
    registered: impl IntoIterator<Item = &'a str>,
    candidate: &str,
) -> bool {
    registered.into_iter().any(|name| name == candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_registration_name_trims_author_whitespace() {
        let mut name = "  review  ".to_owned();
        canonical_registration_name(&mut name);
        assert_eq!(name, "review");
    }

    #[test]
    fn duplicate_detection_is_exact_and_case_sensitive() {
        assert!(has_duplicate_registration_name(
            ["review", "inspect"],
            "review"
        ));
        assert!(!has_duplicate_registration_name(["review"], "Review"));
        assert!(!has_duplicate_registration_name(["review"], "revi"));
        assert!(!has_duplicate_registration_name([], "review"));
    }
}
