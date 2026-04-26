use super::*;

type RegexAccessor = fn() -> &'static Regex;

#[derive(Clone, Copy)]
struct ReplacementRule {
    regex: RegexAccessor,
    replacement: &'static str,
}

#[derive(Clone, Copy)]
struct RouteKeyRule {
    key: &'static str,
    replacement: &'static str,
}

const SANITIZE_REPLACEMENT_RULES: &[ReplacementRule] = &[
    ReplacementRule {
        regex: direct_child_validation_regex,
        replacement: "direct-child validation rejected a stale child reference; use the live \
                      direct-child snapshot or the latest live tool result instead.",
    },
    ReplacementRule {
        regex: child_agent_reference_block_regex,
        replacement: "Child agent reference metadata existed earlier, but compacted history is \
                      not an authoritative routing source.",
    },
    ReplacementRule {
        regex: exact_agent_instruction_regex,
        replacement: "Use only the latest live child snapshot or tool result for agent routing.",
    },
    ReplacementRule {
        regex: raw_root_agent_id_regex,
        replacement: "<agent-id>",
    },
    ReplacementRule {
        regex: raw_agent_id_regex,
        replacement: "<agent-id>",
    },
    ReplacementRule {
        regex: raw_subrun_id_regex,
        replacement: "<subrun-id>",
    },
    ReplacementRule {
        regex: raw_session_id_regex,
        replacement: "<session-id>",
    },
];

const ROUTE_KEY_RULES: &[RouteKeyRule] = &[
    RouteKeyRule {
        key: "agentId",
        replacement: "${key}<latest-direct-child-agentId>",
    },
    RouteKeyRule {
        key: "childAgentId",
        replacement: "${key}<latest-direct-child-agentId>",
    },
    RouteKeyRule {
        key: "parentAgentId",
        replacement: "${key}<parent-agentId>",
    },
    RouteKeyRule {
        key: "subRunId",
        replacement: "${key}<direct-child-subRunId>",
    },
    RouteKeyRule {
        key: "parentSubRunId",
        replacement: "${key}<parent-subRunId>",
    },
    RouteKeyRule {
        key: "sessionId",
        replacement: "${key}<session-id>",
    },
    RouteKeyRule {
        key: "childSessionId",
        replacement: "${key}<child-session-id>",
    },
    RouteKeyRule {
        key: "openSessionId",
        replacement: "${key}<child-session-id>",
    },
];

struct CompiledRouteKeyRule {
    replacement: &'static str,
    regex: Regex,
}

pub(super) fn sanitize_compact_summary(summary: &str) -> String {
    let had_route_sensitive_content = summary_has_route_sensitive_content(summary);
    let mut sanitized = summary.trim().to_string();
    for rule in SANITIZE_REPLACEMENT_RULES {
        sanitized = (rule.regex)()
            .replace_all(&sanitized, rule.replacement)
            .into_owned();
    }
    for rule in route_key_rules() {
        sanitized = rule
            .regex
            .replace_all(&sanitized, rule.replacement)
            .into_owned();
    }
    sanitized = super::collapse_compaction_whitespace(&sanitized);
    if had_route_sensitive_content {
        ensure_compact_boundary_section(&sanitized)
    } else {
        sanitized
    }
}

pub(super) fn sanitize_recent_user_context_digest(digest: &str) -> String {
    super::collapse_compaction_whitespace(digest)
}

fn ensure_compact_boundary_section(summary: &str) -> String {
    if summary.contains("## Compact Boundary") {
        return summary.to_string();
    }
    format!(
        "## Compact Boundary\n- Historical `agentId`, `subRunId`, and `sessionId` values from \
         compacted history are non-authoritative.\n- Use the live direct-child snapshot or the \
         latest live tool result / child notification for routing.\n\n{}",
        summary.trim()
    )
}

fn summary_has_route_sensitive_content(summary: &str) -> bool {
    SANITIZE_REPLACEMENT_RULES
        .iter()
        .any(|rule| (rule.regex)().is_match(summary))
        || route_key_rules()
            .iter()
            .any(|rule| rule.regex.is_match(summary))
}

fn child_agent_reference_block_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?is)Child agent reference:\s*(?:\n- .*)+")
            .expect("child agent reference regex should compile")
    })
}

fn direct_child_validation_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)not a direct child of caller[^\n]*")
            .expect("direct child validation regex should compile")
    })
}

fn route_key_rules() -> &'static [CompiledRouteKeyRule] {
    static RULES: OnceLock<Vec<CompiledRouteKeyRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        ROUTE_KEY_RULES
            .iter()
            .map(|rule| CompiledRouteKeyRule {
                replacement: rule.replacement,
                regex: compile_route_key_regex(rule.key),
            })
            .collect()
    })
}

fn compile_route_key_regex(key: &str) -> Regex {
    Regex::new(&format!(
        r"(?i)(?P<key>`?{key}`?\s*[:=]\s*`?)[^`\s,;\])]+`?"
    ))
    .expect("route key regex should compile")
}

fn exact_agent_instruction_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?i)(use this exact `agentid` value[^\n]*|copy it byte-for-byte[^\n]*|keep `agentid` exact[^\n]*)",
        )
        .expect("exact agent instruction regex should compile")
    })
}

fn raw_root_agent_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\broot-agent:[A-Za-z0-9._:-]+\b")
            .expect("raw root agent id regex should compile")
    })
}

fn raw_agent_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\bagent-[A-Za-z0-9._:-]+\b").expect("raw agent id regex should compile")
    })
}

fn raw_subrun_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\bsubrun-[A-Za-z0-9._:-]+\b").expect("raw subrun regex should compile")
    })
}

fn raw_session_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\bsession-[A-Za-z0-9._:-]+\b").expect("raw session regex should compile")
    })
}

pub(super) fn strip_child_agent_reference_hint(content: &str) -> String {
    let Some((prefix, child_ref_block)) = content.split_once("\n\nChild agent reference:") else {
        return content.to_string();
    };
    let mut has_reference_fields = false;
    for line in child_ref_block.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("- agentId:")
            || trimmed.starts_with("- subRunId:")
            || trimmed.starts_with("- openSessionId:")
            || trimmed.starts_with("- status:")
        {
            has_reference_fields = true;
        }
    }
    let child_ref_summary = if has_reference_fields {
        "Child agent reference existed in the original tool result. Do not reuse any agentId, \
         subRunId, or sessionId from compacted history; rely on the latest live tool result or \
         current direct-child snapshot instead."
            .to_string()
    } else {
        "Child agent reference metadata existed in the original tool result, but compacted history \
         is not an authoritative source for later agent routing."
            .to_string()
    };
    let prefix = prefix.trim();
    if prefix.is_empty() {
        child_ref_summary
    } else {
        format!("{prefix}\n\n{child_ref_summary}")
    }
}
