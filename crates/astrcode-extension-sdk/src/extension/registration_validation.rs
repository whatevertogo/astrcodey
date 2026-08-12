//! In-process `Registrar` 与 worker `HandlerRegistry` 共用的注册校验规则。
//!
//! 两个注册表的校验时机不同（`Registrar::finish` 集中校验 vs worker 插入时
//! 增量校验），错误类型与错误消息也各自保留；这里只沉淀规则本身，消息由
//! 调用点参数化。hook mode 也是注册期约束，因此与名称规则一并定义在这里；
//! HTTP 路由的匹配、冲突检测和注册期校验也属于这一策略层。

use std::collections::{BTreeMap, BTreeSet};

use astrcode_extension_contract::{
    extension_http::{ExtensionHttpRoute, MAX_EXTENSION_HTTP_BODY_BYTES},
    manifest::LifecycleEvent,
};

use super::{
    CustomEventSourceFilter, CustomEventSubscription, HookMode,
    MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN,
};

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

pub(crate) fn lifecycle_event_allows_blocking(event: &LifecycleEvent) -> bool {
    matches!(
        event,
        LifecycleEvent::TurnStart | LifecycleEvent::UserPromptSubmit
    )
}

pub fn fixed_hook_mode(event: &LifecycleEvent) -> Option<HookMode> {
    match event {
        LifecycleEvent::AfterProviderResponse => Some(HookMode::Advisory),
        LifecycleEvent::ContinueAfterStop
        | LifecycleEvent::UserMessageEnvelope
        | LifecycleEvent::PromptBuild => Some(HookMode::Blocking),
        _ => None,
    }
}

pub fn hook_mode_is_supported(event: &LifecycleEvent, mode: HookMode) -> bool {
    if let Some(required) = fixed_hook_mode(event) {
        return mode == required;
    }

    mode != HookMode::Blocking
        || matches!(
            event,
            LifecycleEvent::PreToolUse
                | LifecycleEvent::PostToolUse
                | LifecycleEvent::BeforeProviderRequest
        )
        || lifecycle_event_allows_blocking(event)
}

pub fn normalize_custom_event_subscription(subscription: &mut CustomEventSubscription) {
    canonical_registration_name(&mut subscription.id);
    canonical_registration_name(&mut subscription.event_type);
    if let CustomEventSourceFilter::Extension { extension_id } = &mut subscription.source {
        canonical_registration_name(extension_id);
    }
}

pub fn validate_custom_event_subscription(
    subscription: &CustomEventSubscription,
) -> Result<(), String> {
    if subscription.id.is_empty() || subscription.id.len() > MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN {
        return Err(format!(
            "invalid custom event subscription id `{}`",
            subscription.id
        ));
    }
    if subscription.consumer_version == 0 {
        return Err("custom event consumer version must be greater than zero".to_owned());
    }
    if subscription.event_type.is_empty() {
        return Err("custom event subscription type cannot be empty".to_owned());
    }
    if matches!(
        &subscription.source,
        CustomEventSourceFilter::Extension { extension_id } if extension_id.is_empty()
    ) {
        return Err("custom event subscription source extension cannot be empty".to_owned());
    }
    Ok(())
}

pub fn custom_event_subscription_matches(
    subscription: &CustomEventSubscription,
    extension_id: &str,
    event_type: &str,
) -> bool {
    subscription.event_type == event_type
        && match &subscription.source {
            CustomEventSourceFilter::Any => true,
            CustomEventSourceFilter::Extension {
                extension_id: expected,
            } => expected == extension_id,
        }
}

pub fn validate_extension_http_route(route: &ExtensionHttpRoute) -> Result<(), String> {
    if !valid_extension_http_route_path(&route.path) {
        return Err(format!("invalid extension HTTP route path: {}", route.path));
    }
    if route.max_body_bytes == 0 || route.max_body_bytes > MAX_EXTENSION_HTTP_BODY_BYTES {
        return Err(format!(
            "extension HTTP max_body_bytes must be between 1 and {MAX_EXTENSION_HTTP_BODY_BYTES}"
        ));
    }
    Ok(())
}

pub fn match_extension_http_route(pattern: &str, path: &str) -> Option<BTreeMap<String, String>> {
    let pattern_segments = extension_http_path_segments(pattern);
    let path_segments = extension_http_path_segments(path);
    if pattern_segments.len() != path_segments.len() {
        return None;
    }
    let mut params = BTreeMap::new();
    for (pattern_segment, path_segment) in pattern_segments.iter().zip(path_segments) {
        if let Some(name) = extension_http_param_name(pattern_segment) {
            params.insert(name.to_owned(), path_segment.to_owned());
        } else if pattern_segment != &path_segment {
            return None;
        }
    }
    Some(params)
}

pub fn extension_http_route_patterns_conflict(left: &str, right: &str) -> bool {
    let left_segments = extension_http_path_segments(left);
    let right_segments = extension_http_path_segments(right);
    left_segments.len() == right_segments.len()
        && left_segments
            .iter()
            .zip(right_segments)
            .all(|(left, right)| {
                left == &right
                    || extension_http_param_name(left).is_some()
                    || extension_http_param_name(right).is_some()
            })
}

fn valid_extension_http_route_path(path: &str) -> bool {
    if !path.starts_with('/') || path.ends_with('/') || path.contains("//") || path.contains("..") {
        return false;
    }
    let mut params = BTreeSet::new();
    path.split('/').skip(1).all(|segment| {
        if segment.is_empty() {
            return false;
        }
        match (segment.starts_with('{'), segment.ends_with('}')) {
            (false, false) => !segment.contains('{') && !segment.contains('}'),
            (true, true) => {
                let name = &segment[1..segment.len() - 1];
                !name.is_empty()
                    && name
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
                    && params.insert(name)
            },
            _ => false,
        }
    })
}

fn extension_http_path_segments(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn extension_http_param_name(segment: &str) -> Option<&str> {
    segment
        .strip_prefix('{')
        .and_then(|segment| segment.strip_suffix('}'))
        .filter(|name| !name.is_empty())
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

    #[test]
    fn custom_event_subscriptions_normalize_validate_and_match_at_registration() {
        let mut subscription = CustomEventSubscription::from_extension(" producer ", " completed ")
            .named(" consumer ");
        normalize_custom_event_subscription(&mut subscription);
        assert_eq!(subscription.id, "consumer");
        assert!(validate_custom_event_subscription(&subscription).is_ok());
        assert!(custom_event_subscription_matches(
            &subscription,
            "producer",
            "completed"
        ));
    }
}
