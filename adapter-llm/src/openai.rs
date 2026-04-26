//! # OpenAI 家族 API 的 LLM 提供者
//!
//! 实现了 `LlmProvider` trait，对接 OpenAI Chat Completions、OpenAI Responses
//! 以及兼容 OpenAI 协议的后端（包括 OpenAI 自身、DeepSeek、本地 Ollama/vLLM 等）。
//!
//! ## 核心能力
//!
//! - 非流式/流式两种调用模式
//! - SSE 流式解析（`data: {...}` 行协议）
//! - 指数退避重试（瞬态故障自动恢复）
//! - 取消令牌支持（`select!` 分支中断）
//!
//! ## 缓存策略
//!
//! OpenAI 的 prompt caching 以自动前缀缓存为主：API 自动缓存 >= 1024 tokens 的 prompt
//! 前缀，无需额外显式 `cache_control`。官方 OpenAI endpoint 额外发送
//! `prompt_cache_key` 来提高相似请求的路由稳定性；第三方 OpenAI 兼容 endpoint
//! 默认不发送该字段，避免因未知参数破坏兼容性。
//!
//! ## 协议差异处理
//!
//! Chat Completions 与 Responses 都基于 SSE，但事件模型不同：
//! - Chat Completions 使用单行 `data: {...}`
//! - Responses 使用 `event: ...` + `data: {...}` 的事件块
//!
//! 因此本模块将 Responses 解析拆到独立子模块。

use std::{
    fmt,
    hash::{DefaultHasher, Hash, Hasher},
    sync::{Arc, Mutex},
};

use astrcode_core::{
    AstrError, CancelToken, LlmMessage, ReasoningContent, Result, ToolCallRequest, ToolDefinition,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::select;

use crate::{
    EventSink, FinishReason, LlmAccumulator, LlmClientConfig, LlmEvent, LlmOutput, LlmProvider,
    LlmRequest, LlmUsage, ModelLimits, PromptCacheGlobalStrategy, PromptCacheHints,
    Utf8StreamDecoder, build_http_client,
    cache_tracker::{CacheCheckContext, CacheTracker, stable_hash},
    emit_event, is_retryable_status, wait_retry_delay,
};

mod dto;
mod responses;

use dto::{
    OpenAiRequestMessage, OpenAiToolDef, OpenAiUsage, openai_usage_to_llm_usage, to_openai_message,
    to_openai_tool_def,
};

/// OpenAI 兼容 API 的 LLM 提供者实现。
///
/// 封装了 HTTP 客户端、认证信息和模型配置，提供统一的 `LlmProvider` 接口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiProviderCapabilities {
    pub supports_prompt_cache_key: bool,
    pub supports_stream_usage: bool,
}

impl OpenAiProviderCapabilities {
    pub fn for_endpoint(url: &str) -> Self {
        let is_official = is_official_openai_api_url(url);
        Self {
            supports_prompt_cache_key: is_official,
            supports_stream_usage: is_official,
        }
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    /// 共享的 HTTP 客户端（含统一超时策略）
    client: reqwest::Client,
    /// 当前 provider 使用的 HTTP / retry 配置。
    client_config: LlmClientConfig,
    /// 已解析好的 API endpoint。
    ///
    /// provider_factory 会先把用户配置标准化到最终请求地址，这里不再二次拼接。
    api_url: String,
    /// API 密钥（Bearer token 认证）
    api_key: String,
    /// 模型名称（如 `gpt-4o`、`deepseek-chat`）
    model: String,
    /// 运行时已解析好的模型 limits。
    ///
    /// 这样 provider 不再自己猜上下文窗口，也不会继续依赖过时的 profile 级配置。
    limits: ModelLimits,
    /// 兼容网关能力开关。
    capabilities: OpenAiProviderCapabilities,
    /// 缓存失效检测跟踪器。
    cache_tracker: Arc<Mutex<CacheTracker>>,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("client", &self.client)
            .field("api_url", &self.api_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("limits", &self.limits)
            .field("capabilities", &self.capabilities)
            .field("client_config", &self.client_config)
            .field("cache_tracker", &"<internal>")
            .finish()
    }
}

impl OpenAiProvider {
    /// 创建新的 OpenAI 兼容提供者实例。
    pub fn new(
        api_url: String,
        api_key: String,
        model: String,
        limits: ModelLimits,
        client_config: LlmClientConfig,
    ) -> Result<Self> {
        let capabilities = OpenAiProviderCapabilities::for_endpoint(&api_url);
        Self::new_with_capabilities(api_url, api_key, model, limits, client_config, capabilities)
    }

    pub fn new_with_capabilities(
        api_url: String,
        api_key: String,
        model: String,
        limits: ModelLimits,
        client_config: LlmClientConfig,
        capabilities: OpenAiProviderCapabilities,
    ) -> Result<Self> {
        Ok(Self {
            client: build_http_client(client_config)?,
            client_config,
            api_url,
            api_key,
            model,
            limits,
            capabilities,
            cache_tracker: Arc::new(Mutex::new(CacheTracker::new())),
        })
    }

    fn uses_responses_api(&self) -> bool {
        self.api_url
            .split('?')
            .next()
            .unwrap_or(self.api_url.as_str())
            .trim_end_matches('/')
            .ends_with("/responses")
    }

    /// 构建 OpenAI Chat Completions API 请求体。
    ///
    /// - 如果存在系统提示块，将每个块作为独立的 `role: "system"` 消息插入
    /// - 如果没有系统提示块但有 system_prompt，使用单一 system 消息
    /// - 将 `LlmMessage` 转换为 OpenAI 格式的消息结构
    /// - 如果启用了工具，附加工具定义和 `tool_choice: "auto"`
    ///
    /// ## 缓存策略
    ///
    /// OpenAI 的 prompt caching 是**自动的**：
    /// 不需要显式标记 `cache_control`，API 自动缓存 >= 1024 tokens 的 prompt 前缀。
    /// 分层 system blocks 的排列顺序（Stable → SemiStable → Inherited → Dynamic）天然提供稳定的
    /// 前缀，对 OpenAI 的自动 prefix matching 最友好。
    fn build_chat_completions_request<'a>(
        &'a self,
        input: OpenAiBuildRequestInput<'a>,
    ) -> OpenAiChatRequest<'a> {
        let OpenAiBuildRequestInput {
            messages,
            tools,
            system_prompt,
            system_prompt_blocks,
            prompt_cache_hints,
            max_output_tokens_override,
            stream,
        } = input;
        let effective_max_output_tokens = max_output_tokens_override
            .unwrap_or(self.limits.max_output_tokens)
            .min(self.limits.max_output_tokens);
        let ordered_tools = order_tools_for_cache(tools);
        let system_count = if !system_prompt_blocks.is_empty() {
            system_prompt_blocks.len()
        } else if system_prompt.is_some() {
            1
        } else {
            0
        };
        let mut request_messages = Vec::with_capacity(messages.len() + system_count);

        // 优先使用 system_prompt_blocks（分层排列，为自动缓存提供稳定前缀）
        if !system_prompt_blocks.is_empty() {
            for block in system_prompt_blocks {
                request_messages.push(OpenAiRequestMessage {
                    role: "system".to_string(),
                    content: Some(block.render()),
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
        } else if let Some(text) = system_prompt {
            // 回退到单一 system prompt（向后兼容）
            request_messages.push(OpenAiRequestMessage {
                role: "system".to_string(),
                content: Some(text.to_string()),
                tool_call_id: None,
                tool_calls: None,
            });
        }

        request_messages.extend(messages.iter().map(to_openai_message));

        OpenAiChatRequest {
            model: &self.model,
            max_tokens: effective_max_output_tokens.min(u32::MAX as usize) as u32,
            messages: request_messages,
            prompt_cache_key: self.should_send_prompt_cache_key().then(|| {
                build_prompt_cache_key(
                    &self.model,
                    system_prompt,
                    system_prompt_blocks,
                    prompt_cache_hints,
                    &ordered_tools,
                )
            }),
            prompt_cache_retention: None,
            tools: if ordered_tools.is_empty() {
                None
            } else {
                Some(
                    ordered_tools
                        .iter()
                        .map(|tool| to_openai_tool_def(tool))
                        .collect(),
                )
            },
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
            stream,
            stream_options: (stream && self.should_send_stream_usage_options()).then_some(
                OpenAiStreamOptions {
                    include_usage: true,
                },
            ),
        }
    }

    #[cfg(test)]
    fn build_request<'a>(&'a self, input: OpenAiBuildRequestInput<'a>) -> OpenAiChatRequest<'a> {
        self.build_chat_completions_request(input)
    }

    fn should_send_prompt_cache_key(&self) -> bool {
        self.capabilities.supports_prompt_cache_key
    }

    fn should_send_stream_usage_options(&self) -> bool {
        self.capabilities.supports_stream_usage
    }

    fn build_cache_check_context(
        request: &OpenAiChatRequest<'_>,
        global_cache_strategy: PromptCacheGlobalStrategy,
        compacted: bool,
        tool_result_rebudgeted: bool,
    ) -> CacheCheckContext {
        let leading_system_messages: Vec<&OpenAiRequestMessage> = request
            .messages
            .iter()
            .take_while(|message| message.role == "system")
            .collect();
        CacheCheckContext {
            system_blocks_hash: stable_hash(&leading_system_messages),
            tool_schema_hash: stable_hash(&request.tools),
            model: request.model.to_string(),
            global_cache_strategy,
            compacted,
            tool_result_rebudgeted,
        }
    }

    fn apply_cache_diagnostics(
        &self,
        output: &mut LlmOutput,
        pending_cache_check: Option<crate::cache_tracker::PendingCacheCheck>,
    ) {
        let Some(pending_cache_check) = pending_cache_check else {
            return;
        };
        let Some(mut tracker) = self.cache_tracker.lock().ok() else {
            return;
        };
        output.prompt_cache_diagnostics = tracker.finalize(pending_cache_check, output.usage);
    }

    /// 执行单次 HTTP 请求并处理响应状态。
    ///
    /// 调用方在更外层统一管理 attempt 预算，使“建立响应”和“读取流式 body”
    /// 属于同一个重试边界。
    async fn send_request_once<T: Serialize + ?Sized>(
        &self,
        req: &T,
        cancel: CancelToken,
    ) -> Result<reqwest::Response> {
        let send_future = self
            .client
            .post(&self.api_url)
            .bearer_auth(&self.api_key)
            .json(req)
            .send();

        let response = select! {
            _ = crate::cancelled(cancel.clone()) => {
                return Err(AstrError::LlmInterrupted);
            }
            result = send_future => result
                .map_err(|error| AstrError::http_with_source(
                    "failed to call openai endpoint",
                    error.is_timeout() || error.is_connect() || error.is_body(),
                    error,
                ))?
        };

        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }

        let body = response.text().await.unwrap_or_default();
        Err(AstrError::LlmRequestFailed {
            status: status.as_u16(),
            body,
        })
    }

    fn should_retry_generation_error(&self, error: &AstrError) -> bool {
        if error.is_cancelled() {
            return false;
        }
        if error.is_retryable() {
            return true;
        }
        match error {
            AstrError::LlmRequestFailed { status, .. } => {
                reqwest::StatusCode::from_u16(*status).is_ok_and(is_retryable_status)
            },
            _ => false,
        }
    }

    async fn wait_before_generation_retry(
        &self,
        error: &AstrError,
        attempt: u32,
        cancel: CancelToken,
        sink: Option<&EventSink>,
    ) -> Result<()> {
        if let Some(sink) = sink {
            emit_event(
                LlmEvent::StreamRetryStarted {
                    attempt: attempt.saturating_add(2),
                    max_attempts: self.client_config.max_retries.saturating_add(1),
                    reason: error.to_string(),
                },
                &mut LlmAccumulator::default(),
                sink,
            );
        }
        wait_retry_delay(attempt, cancel, self.client_config.retry_base_delay).await
    }

    fn annotate_retry_exhausted(&self, error: AstrError, attempts: u32) -> AstrError {
        if !self.should_retry_generation_error(&error) || attempts <= 1 {
            return error;
        }
        match error {
            AstrError::HttpRequest {
                context,
                detail,
                retryable,
                source,
            } => AstrError::HttpRequest {
                context: format!("{context} after {attempts} attempts"),
                detail,
                retryable,
                source,
            },
            other => other,
        }
    }
}

// ===========================================================================
// ChatCompletionsSseProcessor — `data: {...}` 行协议
// ===========================================================================

/// Chat Completions 的 SSE 行协议处理器。
///
/// OpenAI Chat Completions 使用 `data: {...}` 单行协议，
/// 每行一个独立的 JSON chunk，流结束标记为 `data: [DONE]`。
struct ChatCompletionsSseProcessor {
    sse_buffer: String,
}

impl ChatCompletionsSseProcessor {
    fn new() -> Self {
        Self {
            sse_buffer: String::new(),
        }
    }
}

impl dto::SseProcessor for ChatCompletionsSseProcessor {
    fn process_chunk(
        &mut self,
        chunk_text: &str,
        accumulator: &mut LlmAccumulator,
        sink: &EventSink,
    ) -> Result<(bool, Option<String>, Option<LlmUsage>)> {
        let mut finish_reason = None;
        let mut usage = None;
        let done = consume_sse_text_chunk(
            chunk_text,
            &mut self.sse_buffer,
            accumulator,
            sink,
            &mut finish_reason,
            &mut usage,
        )?;
        Ok((done, finish_reason, usage))
    }

    fn flush(
        &mut self,
        accumulator: &mut LlmAccumulator,
        sink: &EventSink,
    ) -> Result<(Option<String>, Option<LlmUsage>)> {
        let mut finish_reason = None;
        let mut usage = None;
        flush_sse_buffer(
            &mut self.sse_buffer,
            accumulator,
            sink,
            &mut finish_reason,
            &mut usage,
        )?;
        Ok((finish_reason, usage))
    }

    fn take_completed_output(&mut self) -> Option<LlmOutput> {
        None
    }
}

// ===========================================================================
// 共享流式 SSE 处理骨架
// ===========================================================================

impl OpenAiProvider {
    /// 共享的 SSE 流式处理骨架。
    ///
    /// 处理 UTF-8 解码、取消令牌监听、流结束收尾和 `LlmOutput` 组装。
    /// 不同 API 模式通过 `processor: impl SseProcessor` 注入各自的协议解析逻辑。
    async fn stream_response(
        &self,
        response: reqwest::Response,
        mut processor: impl dto::SseProcessor,
        cancel: CancelToken,
        sink: EventSink,
        pending_cache_check: Option<crate::cache_tracker::PendingCacheCheck>,
    ) -> Result<LlmOutput> {
        let mut body_stream = response.bytes_stream();
        let mut utf8_decoder = Utf8StreamDecoder::default();
        let mut accumulator = LlmAccumulator::default();
        let mut stream_finish_reason: Option<String> = None;
        let mut stream_usage: Option<LlmUsage> = None;

        loop {
            let next_item = select! {
                _ = crate::cancelled(cancel.clone()) => {
                    return Err(AstrError::LlmInterrupted);
                }
                item = body_stream.next() => item,
            };

            let Some(item) = next_item else {
                break;
            };

            let bytes = item.map_err(|error| {
                AstrError::http_with_source(
                    "failed to read openai response stream",
                    error.is_timeout()
                        || error.is_connect()
                        || error.is_body()
                        || error.is_decode(),
                    error,
                )
            })?;
            let Some(chunk_text) =
                utf8_decoder.push(&bytes, "openai response stream was not valid utf-8")?
            else {
                continue;
            };

            let (done, reason, usage) =
                processor.process_chunk(&chunk_text, &mut accumulator, &sink)?;
            if let Some(r) = reason {
                stream_finish_reason = Some(r);
            }
            if let Some(u) = usage {
                stream_usage = Some(u);
            }
            if done {
                return self.finalize_stream_output(
                    accumulator,
                    processor,
                    stream_finish_reason,
                    stream_usage,
                    pending_cache_check,
                );
            }
        }

        // 流结束后刷新 UTF-8 尾部缓冲区
        if let Some(tail_text) =
            utf8_decoder.finish("openai response stream was not valid utf-8")?
        {
            let (done, reason, usage) =
                processor.process_chunk(&tail_text, &mut accumulator, &sink)?;
            if let Some(r) = reason {
                stream_finish_reason = Some(r);
            }
            if let Some(u) = usage {
                stream_usage = Some(u);
            }
            if done {
                return self.finalize_stream_output(
                    accumulator,
                    processor,
                    stream_finish_reason,
                    stream_usage,
                    pending_cache_check,
                );
            }
        }

        // 流结束后刷新 SSE 缓冲区中剩余的不完整行/块
        let (reason, usage) = processor.flush(&mut accumulator, &sink)?;
        if let Some(r) = reason {
            stream_finish_reason = Some(r);
        }
        if let Some(u) = usage {
            stream_usage = Some(u);
        }
        self.finalize_stream_output(
            accumulator,
            processor,
            stream_finish_reason,
            stream_usage,
            pending_cache_check,
        )
    }

    fn finalize_stream_output(
        &self,
        accumulator: LlmAccumulator,
        mut processor: impl dto::SseProcessor,
        finish_reason: Option<String>,
        usage: Option<LlmUsage>,
        pending_cache_check: Option<crate::cache_tracker::PendingCacheCheck>,
    ) -> Result<LlmOutput> {
        let mut output = processor
            .take_completed_output()
            .unwrap_or_else(|| accumulator.finish());

        if let Some(r) = finish_reason {
            output.finish_reason = FinishReason::from_api_value(&r);
        }
        if output.usage.is_none() {
            output.usage = usage;
        }
        self.apply_cache_diagnostics(&mut output, pending_cache_check);
        Ok(output)
    }
}

fn is_official_openai_api_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|host| host.eq_ignore_ascii_case("api.openai.com"))
        })
        .unwrap_or(false)
}

fn build_prompt_cache_key(
    model: &str,
    system_prompt: Option<&str>,
    system_prompt_blocks: &[astrcode_runtime_contract::prompt::SystemPromptBlock],
    prompt_cache_hints: Option<&PromptCacheHints>,
    tools: &[&ToolDefinition],
) -> String {
    let mut hasher = DefaultHasher::new();
    "astrcode-openai-prompt-cache-v2".hash(&mut hasher);
    model.hash(&mut hasher);

    if let Some(hints) = prompt_cache_hints {
        if let Some(stable) = &hints.layer_fingerprints.stable {
            "stable".hash(&mut hasher);
            stable.hash(&mut hasher);
        }
        if let Some(semi_stable) = &hints.layer_fingerprints.semi_stable {
            "semi_stable".hash(&mut hasher);
            semi_stable.hash(&mut hasher);
        }
        if let Some(inherited) = &hints.layer_fingerprints.inherited {
            "inherited".hash(&mut hasher);
            inherited.hash(&mut hasher);
        }
        "global_cache_strategy".hash(&mut hasher);
        match hints.global_cache_strategy {
            PromptCacheGlobalStrategy::SystemPrompt => "system_prompt",
            PromptCacheGlobalStrategy::ToolBased => "tool_based",
        }
        .hash(&mut hasher);
        "compacted".hash(&mut hasher);
        hints.compacted.hash(&mut hasher);
        "tool_result_rebudgeted".hash(&mut hasher);
        hints.tool_result_rebudgeted.hash(&mut hasher);
    } else if !system_prompt_blocks.is_empty() {
        for block in system_prompt_blocks {
            format!("{:?}", block.layer).hash(&mut hasher);
            block.title.hash(&mut hasher);
            block.content.hash(&mut hasher);
        }
    } else if let Some(prompt) = system_prompt {
        prompt.hash(&mut hasher);
    }

    for tool in tools {
        tool.name.hash(&mut hasher);
        tool.description.hash(&mut hasher);
        if let Ok(parameters) = serde_json::to_string(&tool.parameters) {
            parameters.hash(&mut hasher);
        }
    }

    format!("astrcode-{:016x}", hasher.finish())
}

fn order_tools_for_cache(tools: &[ToolDefinition]) -> Vec<&ToolDefinition> {
    let mut ordered: Vec<&ToolDefinition> = tools.iter().collect();
    ordered.sort_by(|left, right| {
        let left_key = (
            builtin_tool_rank(left.name.as_str()).unwrap_or(u8::MAX),
            left.name.as_str(),
        );
        let right_key = (
            builtin_tool_rank(right.name.as_str()).unwrap_or(u8::MAX),
            right.name.as_str(),
        );
        left_key.cmp(&right_key)
    });
    ordered
}

fn builtin_tool_rank(name: &str) -> Option<u8> {
    match name {
        "readFile" => Some(0),
        "findFiles" => Some(1),
        "grep" => Some(2),
        "shell" => Some(3),
        "editFile" => Some(4),
        "writeFile" => Some(5),
        "apply_patch" => Some(6),
        "enterPlanMode" => Some(7),
        "exitPlanMode" => Some(8),
        "upsertSessionPlan" => Some(9),
        "taskWrite" => Some(10),
        "tool_search" => Some(11),
        "Skill" => Some(12),
        "spawn" => Some(13),
        "send" => Some(14),
        "observe" => Some(15),
        "close" => Some(16),
        _ => None,
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn supports_cache_metrics(&self) -> bool {
        true
    }

    /// 执行一次模型调用。
    ///
    /// 根据 `sink` 是否存在选择流式或非流式路径：
    /// - **非流式**（`sink = None`）：等待完整响应后解析 JSON，提取文本和工具调用
    /// - **流式**（`sink = Some`）：逐块读取 SSE 响应，实时发射事件并累加
    async fn generate(&self, request: LlmRequest, sink: Option<EventSink>) -> Result<LlmOutput> {
        if self.uses_responses_api() {
            return self.generate_via_responses(request, sink).await;
        }

        let prompt_cache_hints = request.prompt_cache_hints.clone();
        let global_cache_strategy = prompt_cache_hints
            .as_ref()
            .map(|hints| hints.global_cache_strategy)
            .unwrap_or(PromptCacheGlobalStrategy::SystemPrompt);
        let cancel = request.cancel.clone();
        let max_retries = self.client_config.max_retries;

        for attempt in 0..=max_retries {
            let req = self.build_chat_completions_request(OpenAiBuildRequestInput {
                messages: &request.messages,
                tools: &request.tools,
                system_prompt: request.system_prompt.as_deref(),
                system_prompt_blocks: &request.system_prompt_blocks,
                prompt_cache_hints: prompt_cache_hints.as_ref(),
                max_output_tokens_override: request.max_output_tokens_override,
                stream: sink.is_some(),
            });
            let pending_cache_check = self.cache_tracker.lock().ok().map(|tracker| {
                tracker.prepare(&Self::build_cache_check_context(
                    &req,
                    global_cache_strategy,
                    prompt_cache_hints
                        .as_ref()
                        .is_some_and(|hints| hints.compacted),
                    prompt_cache_hints
                        .as_ref()
                        .is_some_and(|hints| hints.tool_result_rebudgeted),
                ))
            });
            let response = self.send_request_once(&req, cancel.clone()).await;
            let result = match (response, sink.as_ref()) {
                (Ok(response), None) => {
                    // 非流式路径：解析完整 JSON 响应
                    match response
                        .json::<OpenAiChatResponse>()
                        .await
                        .map_err(|error| {
                            AstrError::http_with_source(
                                "failed to parse openai response",
                                error.is_timeout() || error.is_connect() || error.is_body(),
                                error,
                            )
                        }) {
                        Ok(parsed) => {
                            let OpenAiChatResponse { choices, usage } = parsed;
                            let usage = usage.map(openai_usage_to_llm_usage);
                            match choices.into_iter().next() {
                                Some(first_choice) => {
                                    let mut output = message_to_output(
                                        first_choice.message,
                                        usage,
                                        first_choice.finish_reason,
                                    );
                                    self.apply_cache_diagnostics(&mut output, pending_cache_check);
                                    Ok(output)
                                },
                                None => Err(AstrError::LlmStreamError(
                                    "openai response did not include choices".to_string(),
                                )),
                            }
                        },
                        Err(error) => Err(error),
                    }
                },
                (Ok(response), Some(sink)) => {
                    self.stream_response(
                        response,
                        ChatCompletionsSseProcessor::new(),
                        cancel.clone(),
                        Arc::clone(sink),
                        pending_cache_check,
                    )
                    .await
                },
                (Err(error), _) => Err(error),
            };

            match result {
                Ok(output) => return Ok(output),
                Err(error)
                    if attempt < max_retries && self.should_retry_generation_error(&error) =>
                {
                    self.wait_before_generation_retry(
                        &error,
                        attempt,
                        cancel.clone(),
                        sink.as_ref(),
                    )
                    .await?;
                },
                Err(error) => {
                    return Err(self.annotate_retry_exhausted(error, attempt.saturating_add(1)));
                },
            }
        }

        Err(AstrError::Internal(
            "openai generation retry loop should have returned on all paths".into(),
        ))
    }

    /// 返回当前模型的上下文窗口估算。
    ///
    /// OpenAI provider 不再在这里临时猜测 limits，而是直接回放 provider
    /// 构造阶段已经解析好的逐模型配置。
    fn model_limits(&self) -> ModelLimits {
        self.limits
    }
}

impl OpenAiProvider {
    async fn generate_via_responses(
        &self,
        request: LlmRequest,
        sink: Option<EventSink>,
    ) -> Result<LlmOutput> {
        let cancel = request.cancel.clone();
        let max_retries = self.client_config.max_retries;

        for attempt in 0..=max_retries {
            let req = responses::build_request(self, &request, sink.is_some());
            let response = self.send_request_once(&req, cancel.clone()).await;
            let result = match (response, sink.as_ref()) {
                (Ok(response), None) => responses::parse_non_streaming_response(response).await,
                (Ok(response), Some(sink)) => {
                    self.stream_response(
                        response,
                        responses::ResponsesSseProcessor::new(),
                        cancel.clone(),
                        Arc::clone(sink),
                        None,
                    )
                    .await
                },
                (Err(error), _) => Err(error),
            };

            match result {
                Ok(output) => return Ok(output),
                Err(error)
                    if attempt < max_retries && self.should_retry_generation_error(&error) =>
                {
                    self.wait_before_generation_retry(
                        &error,
                        attempt,
                        cancel.clone(),
                        sink.as_ref(),
                    )
                    .await?;
                },
                Err(error) => {
                    return Err(self.annotate_retry_exhausted(error, attempt.saturating_add(1)));
                },
            }
        }

        Err(AstrError::Internal(
            "openai responses retry loop should have returned on all paths".into(),
        ))
    }
}

/// 将 OpenAI 响应消息转换为统一的 `LlmOutput`。
///
/// 处理文本内容、工具调用和推理内容（`reasoning_content` 字段，
/// 部分兼容 API 使用 `reasoning` 别名）。
///
/// ## 设计要点
///
/// - 工具调用参数可能不是合法 JSON，解析失败时回退为原始字符串
/// - 推理内容为空字符串时不保留（避免无意义的空 reasoning 对象）
/// - `usage` 参数由调用方传入；流式路径会在收到 usage trailer 后补入
/// - `finish_reason` 从响应 choice 中提取，用于检测 max_tokens 截断 (P4.2)
fn message_to_output(
    message: OpenAiResponseMessage,
    usage: Option<LlmUsage>,
    finish_reason: Option<String>,
) -> LlmOutput {
    let content = message.content.unwrap_or_default();
    let tool_calls: Vec<ToolCallRequest> = message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|call| ToolCallRequest {
            id: call.id,
            name: call.function.name,
            // NOTE: 参数可能不是合法 JSON，解析失败时回退为原始字符串
            args: serde_json::from_str::<Value>(&call.function.arguments)
                .unwrap_or(Value::String(call.function.arguments)),
        })
        .collect();

    let finish_reason = finish_reason
        .as_deref()
        .map(FinishReason::from_api_value)
        .unwrap_or_else(|| {
            // 无 finish_reason 时根据内容推断
            if !tool_calls.is_empty() {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        });

    LlmOutput {
        content,
        tool_calls,
        // 推理内容为空字符串时不保留
        reasoning: message
            .reasoning_content
            .filter(|value| !value.is_empty())
            .map(|content| ReasoningContent {
                content,
                signature: None,
            }),
        usage,
        finish_reason,
        prompt_cache_diagnostics: None,
    }
}

/// SSE 行解析结果。
///
/// OpenAI 兼容 API 的 SSE 格式为单行 `data: {...}`，每行独立一个 JSON chunk。
/// Chat Completions 的流格式较简单：每行以 `data: ` 开头，
/// 流结束由特殊的 `data: [DONE]` 标记。
enum ParsedSseLine {
    /// 空行或无 data 前缀的行，应忽略
    Ignore,
    /// `[DONE]` 标记，表示流结束
    Done,
    /// 解析出的流式 chunk
    Chunk(OpenAiStreamChunk),
}

/// 解析单行 SSE 文本。
///
/// 期望格式：`data: <json>` 或 `data: [DONE]`。
/// 空行或不带 `data: ` 前缀的行返回 `Ignore`。
///
/// ## 错误处理
///
/// JSON 解析失败会返回 `AstrError::Parse` 错误，这通常意味着后端响应格式异常。
fn parse_sse_line(line: &str) -> Result<ParsedSseLine> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(ParsedSseLine::Ignore);
    }

    let Some(after_prefix) = trimmed.strip_prefix("data:") else {
        return Ok(ParsedSseLine::Ignore);
    };
    let data = after_prefix.trim_start();

    if data == "[DONE]" {
        return Ok(ParsedSseLine::Done);
    }

    let chunk = serde_json::from_str::<OpenAiStreamChunk>(data)
        .map_err(|error| AstrError::parse("failed to parse streaming chunk", error))?;
    Ok(ParsedSseLine::Chunk(chunk))
}

/// 将 OpenAI 流式 chunk 转换为 `LlmEvent` 列表。
///
/// 每个 choice 的 delta 可能包含文本、推理内容或工具调用增量。
///
/// ## 设计要点
///
/// - 空字符串的文本和推理内容会被过滤，避免发射无意义的空增量
/// - 工具调用参数缺失时回退为空字符串，由累加器负责拼接
/// - 返回最后一个非 None 的 finish_reason（P4.2）
fn apply_stream_chunk(
    chunk: OpenAiStreamChunk,
) -> (Vec<LlmEvent>, Option<String>, Option<LlmUsage>) {
    let mut events = Vec::new();
    let mut last_finish_reason: Option<String> = None;
    let usage = chunk.usage.map(openai_usage_to_llm_usage);

    for choice in chunk.choices {
        // 提取 finish_reason，最后一个非 None 值有效
        if let Some(reason) = choice.finish_reason {
            last_finish_reason = Some(reason);
        }

        if let Some(content) = choice.delta.content {
            if !content.is_empty() {
                events.push(LlmEvent::TextDelta(content));
            }
        }

        if let Some(reasoning_content) = choice.delta.reasoning_content {
            if !reasoning_content.is_empty() {
                events.push(LlmEvent::ThinkingDelta(reasoning_content));
            }
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            for function_call in tool_calls {
                let (name, arguments_delta) = match function_call.function {
                    Some(function) => (function.name, function.arguments.unwrap_or_default()),
                    None => (None, String::new()),
                };

                events.push(LlmEvent::ToolCallDelta {
                    index: function_call.index,
                    id: function_call.id,
                    name,
                    arguments_delta,
                });
            }
        }
    }

    (events, last_finish_reason, usage)
}

/// 处理单行 SSE 文本，返回 `(is_done, finish_reason)`。
///
/// 这是 SSE 处理链路的中间层：解析行 → 转换 chunk → 发射事件。
fn process_sse_line(
    line: &str,
    accumulator: &mut LlmAccumulator,
    sink: &EventSink,
) -> Result<(bool, Option<String>, Option<LlmUsage>)> {
    match parse_sse_line(line)? {
        ParsedSseLine::Ignore => Ok((false, None, None)),
        ParsedSseLine::Done => Ok((true, None, None)),
        ParsedSseLine::Chunk(chunk) => {
            let (events, finish_reason, usage) = apply_stream_chunk(chunk);
            for event in events {
                emit_event(event, accumulator, sink);
            }
            Ok((false, finish_reason, usage))
        },
    }
}

/// 消费一块 SSE 文本 chunk，按行分割并处理。
///
/// 由于 TCP 流可能将一行 SSE 分割到多个 chunk 中，
/// 本函数使用 `sse_buffer` 累积未完成的行，等待后续 chunk 补齐。
/// 返回 `(is_done, finish_reason)`，is_done 为 true 表示遇到 `[DONE]`，流应停止读取。
///
/// ## TCP 分片处理
///
/// TCP 是字节流协议，不保证消息边界。一个完整的 SSE 行可能被分成多个 TCP chunk，
/// 因此不能假设每个 `chunk_text` 包含完整的 `data: {...}` 行。
fn consume_sse_text_chunk(
    chunk_text: &str,
    sse_buffer: &mut String,
    accumulator: &mut LlmAccumulator,
    sink: &EventSink,
    finish_reason_out: &mut Option<String>,
    usage_out: &mut Option<LlmUsage>,
) -> Result<bool> {
    sse_buffer.push_str(chunk_text);

    while let Some(newline_idx) = sse_buffer.find('\n') {
        let line_with_newline: String = sse_buffer.drain(..=newline_idx).collect();
        let line = line_with_newline
            .trim_end_matches('\n')
            .trim_end_matches('\r');

        let (done, reason, usage) = process_sse_line(line, accumulator, sink)?;
        if let Some(r) = reason {
            *finish_reason_out = Some(r);
        }
        if let Some(usage) = usage {
            *usage_out = Some(usage);
        }
        if done {
            return Ok(true);
        }
    }

    Ok(false)
}

/// 刷新 SSE 缓冲区中剩余的不完整行（流结束后的收尾处理）。
///
/// 当 HTTP 流结束时，缓冲区中可能还剩一行没有换行符。
/// 本函数处理这最后一行并清空缓冲区。
fn flush_sse_buffer(
    sse_buffer: &mut String,
    accumulator: &mut LlmAccumulator,
    sink: &EventSink,
    finish_reason_out: &mut Option<String>,
    usage_out: &mut Option<LlmUsage>,
) -> Result<()> {
    let remaining = std::mem::take(sse_buffer);
    let remaining = remaining.trim();
    if !remaining.is_empty() {
        let (done, reason, usage) = process_sse_line(remaining, accumulator, sink)?;
        if let Some(r) = reason {
            *finish_reason_out = Some(r);
        }
        if let Some(usage) = usage {
            *usage_out = Some(usage);
        }
        // 如果 flush 时遇到 [DONE]，忽略（正常流结束）
        // 故意忽略：消费 done 标志以避免未使用变量警告
        let _ = done;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// OpenAI API 请求/响应 DTO（仅用于 serde 序列化/反序列化）
// ---------------------------------------------------------------------------

/// OpenAI Chat Completions API 请求体。
///
/// 使用生命周期 `'a` 借用模型名称和工具选择字符串，
/// 避免不必要的字符串克隆。`stream` 字段为 `bool`（非 `Option`），
/// 因为 OpenAI API 始终需要该字段。
#[derive(Debug, Serialize)]
struct OpenAiChatRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<OpenAiRequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<OpenAiStreamOptions>,
}

struct OpenAiBuildRequestInput<'a> {
    messages: &'a [LlmMessage],
    tools: &'a [ToolDefinition],
    system_prompt: Option<&'a str>,
    system_prompt_blocks: &'a [astrcode_runtime_contract::prompt::SystemPromptBlock],
    prompt_cache_hints: Option<&'a PromptCacheHints>,
    max_output_tokens_override: Option<usize>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct OpenAiStreamOptions {
    include_usage: bool,
}

/// OpenAI Chat Completions API 非流式响应体。
///
/// 包含 `choices` 数组（通常只有一个元素）和可选的 `usage` 统计。
#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

/// OpenAI 响应中的单个 choice。
///
/// 非流式响应中通常只有一个 choice，流式响应中每个 chunk 也包含一个 choice。
#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// OpenAI 响应消息（从 choice 中提取）。
///
/// `reasoning_content` 字段通过 `#[serde(alias = "reasoning")]` 兼容
/// 部分 API 后端使用 `reasoning` 作为字段名的情况。
#[derive(Debug, Deserialize)]
struct OpenAiResponseMessage {
    content: Option<String>,
    /// 推理内容，部分兼容 API 使用 `reasoning` 字段名（通过 `alias` 兼容）。
    #[serde(alias = "reasoning")]
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OpenAiResponseFunctionCall>>,
}

/// OpenAI 响应中的函数调用。
///
/// 与请求体中的 `OpenAiRequestFunctionCall` 不同，
/// 响应体中的函数调用不包含 `type` 字段。
#[derive(Debug, Deserialize)]
struct OpenAiResponseFunctionCall {
    id: String,
    function: OpenAiResponseFunction,
}

/// OpenAI 响应中函数调用的函数部分。
///
/// `arguments` 为 JSON 字符串（未解析），调用方需要自行反序列化。
#[derive(Debug, Deserialize)]
struct OpenAiResponseFunction {
    name: String,
    arguments: String,
}

/// OpenAI 流式响应中的单个 chunk（对应一行 `data: {...}`）。
///
/// 每个 chunk 包含 `choices` 数组，每个 choice 的 delta 包含增量内容。
#[derive(Debug, Deserialize)]
struct OpenAiStreamChunk {
    #[serde(default)]
    choices: Vec<OpenAiStreamChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

/// OpenAI 流式 chunk 中的单个 choice。
///
/// `finish_reason` 保留以兼容 API 响应结构，但当前流结束判断由 `[DONE]` 标记决定。
#[derive(Debug, Deserialize)]
struct OpenAiStreamChoice {
    delta: OpenAiStreamDelta,
    finish_reason: Option<String>,
}

/// OpenAI 流式 delta（增量内容）。
///
/// `reasoning_content` 同样通过 `alias` 兼容 `reasoning` 字段名。
#[derive(Debug, Deserialize)]
struct OpenAiStreamDelta {
    content: Option<String>,
    /// 推理内容增量，部分兼容 API 使用 `reasoning` 字段名。
    #[serde(alias = "reasoning")]
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OpenAiStreamFunctionCall>>,
}

/// OpenAI 流式响应中的函数调用增量。
///
/// 流式工具调用分多个 chunk 到达：
/// - 首个 chunk 包含 `id` 和 `function.name`
/// - 后续 chunk 只包含 `function.arguments` 的片段
#[derive(Debug, Deserialize)]
struct OpenAiStreamFunctionCall {
    index: usize,
    id: Option<String>,
    function: Option<OpenAiStreamFunctionDelta>,
}

/// OpenAI 流式函数调用的函数增量部分。
///
/// `name` 和 `arguments` 均为 `Option`，因为不同 chunk 中可能只出现其中一个。
#[derive(Debug, Deserialize)]
struct OpenAiStreamFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
