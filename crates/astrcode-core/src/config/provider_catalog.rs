//! Built-in provider specs used by config-facing UI and provider setup flows.
//!
//! Runtime provider construction still reads user profiles from `Config`. This
//! catalog describes stable presets at the config boundary: provider family,
//! wire format, auth scheme, common endpoint presets, and seed model names.

use super::{ProviderAuthScheme, ProviderWireFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderEndpointPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: Option<&'static str>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSpecCapabilities {
    pub prompt_cache_key: bool,
    pub stream_usage: bool,
    pub reasoning_effort: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub provider_kind: &'static str,
    pub wire_format: ProviderWireFormat,
    pub auth_scheme: ProviderAuthScheme,
    pub default_model: &'static str,
    pub api_key_env_vars: &'static [&'static str],
    pub endpoints: &'static [ProviderEndpointPreset],
    pub capabilities: ProviderSpecCapabilities,
}

const OPENAI_ENDPOINTS: &[ProviderEndpointPreset] = &[ProviderEndpointPreset {
    id: "official",
    label: "Official",
    base_url: Some("https://api.openai.com/v1"),
    is_default: true,
}];

const ANTHROPIC_ENDPOINTS: &[ProviderEndpointPreset] = &[ProviderEndpointPreset {
    id: "official",
    label: "Official",
    base_url: Some("https://api.anthropic.com/v1"),
    is_default: true,
}];

const GEMINI_ENDPOINTS: &[ProviderEndpointPreset] = &[ProviderEndpointPreset {
    id: "official",
    label: "Official",
    base_url: Some("https://generativelanguage.googleapis.com/v1beta"),
    is_default: true,
}];

const DEEPSEEK_ENDPOINTS: &[ProviderEndpointPreset] = &[ProviderEndpointPreset {
    id: "official",
    label: "Official",
    base_url: Some("https://api.deepseek.com"),
    is_default: true,
}];

const QWEN_ENDPOINTS: &[ProviderEndpointPreset] = &[ProviderEndpointPreset {
    id: "dashscope-compatible",
    label: "DashScope Compatible",
    base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
    is_default: true,
}];

const ARK_ENDPOINTS: &[ProviderEndpointPreset] = &[ProviderEndpointPreset {
    id: "ark-beijing",
    label: "Ark Beijing",
    base_url: Some("https://ark.cn-beijing.volces.com/api/v3"),
    is_default: true,
}];

const ZHIPU_ENDPOINTS: &[ProviderEndpointPreset] = &[ProviderEndpointPreset {
    id: "coding-paas",
    label: "Coding PAAS",
    base_url: Some("https://open.bigmodel.cn/api/coding/paas/v4"),
    is_default: true,
}];

const OPENAI_COMPATIBLE_ENDPOINTS: &[ProviderEndpointPreset] = &[ProviderEndpointPreset {
    id: "custom",
    label: "Custom",
    base_url: None,
    is_default: true,
}];

const OPENAI_RESPONSES_CAPABILITIES: ProviderSpecCapabilities = ProviderSpecCapabilities {
    prompt_cache_key: true,
    stream_usage: true,
    reasoning_effort: true,
};

const OPENAI_CHAT_CAPABILITIES: ProviderSpecCapabilities = ProviderSpecCapabilities {
    prompt_cache_key: false,
    stream_usage: false,
    reasoning_effort: false,
};

const BASIC_CAPABILITIES: ProviderSpecCapabilities = ProviderSpecCapabilities {
    prompt_cache_key: false,
    stream_usage: false,
    reasoning_effort: false,
};

const BUILTIN_PROVIDER_CATALOG: &[ProviderSpec] = &[
    ProviderSpec {
        id: "openai",
        display_name: "OpenAI",
        provider_kind: "openai",
        wire_format: ProviderWireFormat::OpenAiResponses,
        auth_scheme: ProviderAuthScheme::Bearer,
        default_model: "gpt-4.1",
        api_key_env_vars: &["OPENAI_API_KEY"],
        endpoints: OPENAI_ENDPOINTS,
        capabilities: OPENAI_RESPONSES_CAPABILITIES,
    },
    ProviderSpec {
        id: "anthropic",
        display_name: "Anthropic",
        provider_kind: "anthropic",
        wire_format: ProviderWireFormat::AnthropicMessages,
        auth_scheme: ProviderAuthScheme::XApiKey,
        default_model: "claude-sonnet-4-6",
        api_key_env_vars: &["ANTHROPIC_API_KEY"],
        endpoints: ANTHROPIC_ENDPOINTS,
        capabilities: BASIC_CAPABILITIES,
    },
    ProviderSpec {
        id: "gemini",
        display_name: "Google Gemini",
        provider_kind: "gemini",
        wire_format: ProviderWireFormat::GoogleGenAi,
        auth_scheme: ProviderAuthScheme::XGoogApiKey,
        default_model: "gemini-2.5-flash",
        api_key_env_vars: &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        endpoints: GEMINI_ENDPOINTS,
        capabilities: BASIC_CAPABILITIES,
    },
    ProviderSpec {
        id: "deepseek",
        display_name: "DeepSeek",
        provider_kind: "deepseek",
        wire_format: ProviderWireFormat::OpenAiChatCompletions,
        auth_scheme: ProviderAuthScheme::Bearer,
        default_model: "deepseek-v4-flash",
        api_key_env_vars: &["DEEPSEEK_API_KEY"],
        endpoints: DEEPSEEK_ENDPOINTS,
        capabilities: OPENAI_CHAT_CAPABILITIES,
    },
    ProviderSpec {
        id: "qwen",
        display_name: "Qwen",
        provider_kind: "qwen",
        wire_format: ProviderWireFormat::OpenAiChatCompletions,
        auth_scheme: ProviderAuthScheme::Bearer,
        default_model: "qwen3-coder-plus",
        api_key_env_vars: &["DASHSCOPE_API_KEY", "QWEN_API_KEY"],
        endpoints: QWEN_ENDPOINTS,
        capabilities: OPENAI_CHAT_CAPABILITIES,
    },
    ProviderSpec {
        id: "ark",
        display_name: "Volcengine Ark",
        provider_kind: "ark",
        wire_format: ProviderWireFormat::OpenAiChatCompletions,
        auth_scheme: ProviderAuthScheme::Bearer,
        default_model: "doubao-seed-1-6",
        api_key_env_vars: &["ARK_API_KEY", "VOLCENGINE_API_KEY"],
        endpoints: ARK_ENDPOINTS,
        capabilities: OPENAI_CHAT_CAPABILITIES,
    },
    ProviderSpec {
        id: "zhipu",
        display_name: "Zhipu",
        provider_kind: "zhipu",
        wire_format: ProviderWireFormat::OpenAiChatCompletions,
        auth_scheme: ProviderAuthScheme::Bearer,
        default_model: "glm-5.2",
        api_key_env_vars: &["ZHIPU_API_KEY", "BIGMODEL_API_KEY"],
        endpoints: ZHIPU_ENDPOINTS,
        capabilities: OPENAI_CHAT_CAPABILITIES,
    },
    ProviderSpec {
        id: "openai-compatible",
        display_name: "OpenAI Compatible",
        provider_kind: "openai-compatible",
        wire_format: ProviderWireFormat::OpenAiChatCompletions,
        auth_scheme: ProviderAuthScheme::Bearer,
        default_model: "gpt-4.1",
        api_key_env_vars: &["OPENAI_API_KEY"],
        endpoints: OPENAI_COMPATIBLE_ENDPOINTS,
        capabilities: OPENAI_CHAT_CAPABILITIES,
    },
];

pub fn builtin_provider_catalog() -> &'static [ProviderSpec] {
    BUILTIN_PROVIDER_CATALOG
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn builtin_catalog_ids_are_unique() {
        let mut seen = BTreeSet::new();
        for spec in builtin_provider_catalog() {
            assert!(seen.insert(spec.id), "duplicate provider spec {}", spec.id);
        }
    }

    #[test]
    fn builtin_catalog_entries_have_one_default_endpoint() {
        for spec in builtin_provider_catalog() {
            let default_count = spec
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.is_default)
                .count();
            assert_eq!(default_count, 1, "provider spec {}", spec.id);
        }
    }

    #[test]
    fn qwen_and_ark_are_openai_chat_compatible_presets() {
        for id in ["qwen", "ark"] {
            let spec = builtin_provider_catalog()
                .iter()
                .find(|spec| spec.id == id)
                .expect("provider spec exists");
            assert_eq!(spec.wire_format, ProviderWireFormat::OpenAiChatCompletions);
            assert_eq!(spec.auth_scheme, ProviderAuthScheme::Bearer);
            assert!(spec.endpoints[0].base_url.is_some());
        }
    }
}
