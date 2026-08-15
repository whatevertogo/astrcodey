//! Auto-compact 门控的 provider count_tokens 调用纪律。
//!
//! 门控必须在「auto-compact 关闭」或「本地估算远低于阈值」时不打 provider
//! count API;只有逼近阈值才为精确性付费。

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use astrcode_context::ContextSettings;
use astrcode_core::{
    llm::{
        LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRequest, LlmTokenUsage,
        LlmTokenUsageSource, ModelLimits, ProviderInputTokenCount,
    },
    tool::ToolDefinition,
    types::new_turn_id,
};
use tokio::sync::mpsc;

mod common;

struct CountingLlm {
    count_calls: AtomicUsize,
    /// `count_input_tokens` 的返回值;`None` 走 trait 默认的 Unsupported。
    provider_count: Option<u64>,
}

#[async_trait::async_trait]
impl LlmProvider for CountingLlm {
    async fn generate_request(
        &self,
        _request: LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta { delta: "ok".into() });
        // 携带 usage,避免 turn 在流末走 provider count 的 usage 兜底,
        // 让计数器只反映 compact 门控自身的调用。
        let _ = tx.send(LlmEvent::Usage {
            usage: LlmTokenUsage {
                input_tokens: Some(64),
                output_tokens: Some(2),
                total_tokens: Some(66),
                source: Some(LlmTokenUsageSource::ProviderUsage),
                ..Default::default()
            },
        });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    async fn count_input_tokens(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<ProviderInputTokenCount, LlmError> {
        self.count_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderInputTokenCount::provider_count(
            self.provider_count.unwrap_or(0),
        ))
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 1024,
        }
    }
}

fn counting_llm(provider_count: Option<u64>) -> Arc<CountingLlm> {
    Arc::new(CountingLlm {
        count_calls: AtomicUsize::new(0),
        provider_count,
    })
}

#[tokio::test]
async fn disabled_auto_compact_never_calls_provider_count() {
    let llm = counting_llm(Some(10));
    let count_calls = Arc::clone(&llm);
    let (session, _, _, _) = common::spawn_session_with_context_and_services(
        llm,
        ContextSettings {
            auto_compact_enabled: false,
            ..Default::default()
        },
    )
    .await;

    let result = session
        .submit("hello".into(), new_turn_id(), None)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);
    assert_eq!(count_calls.count_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn small_context_stays_below_the_provider_count_gate() {
    let llm = counting_llm(Some(190_000));
    let count_calls = Arc::clone(&llm);
    let (session, _, _) = common::spawn_session_with_llm_assembler(
        llm,
        ContextSettings {
            auto_compact_enabled: true,
            ..Default::default()
        },
    )
    .await;

    let result = session
        .submit("hello".into(), new_turn_id(), None)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);
    assert_eq!(
        count_calls.count_calls.load(Ordering::SeqCst),
        0,
        "far below the threshold, the local anchored estimate must skip provider counting"
    );
}

#[tokio::test]
async fn near_threshold_context_pays_for_provider_count() {
    // 阈值压到地板级,一条短消息的本地估算即可越过 gate。
    let llm = counting_llm(Some(190_000));
    let count_calls = Arc::clone(&llm);
    let (session, _, _) = common::spawn_session_with_llm_assembler(
        llm,
        ContextSettings {
            auto_compact_enabled: true,
            compact_threshold_percent: 0.01,
            predictive_compact_enabled: false,
            ..Default::default()
        },
    )
    .await;

    let result = session
        .submit("hello".into(), new_turn_id(), None)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);
    assert!(
        count_calls.count_calls.load(Ordering::SeqCst) >= 1,
        "crossing the gate floor must consult the provider count"
    );
}
