//! 脚本化 LLM 测试脚手架。
//!
//! 仅在 `testing` feature 下编译;消费方通过 dev-dependencies 启用
//! (feature unification 使 cfg(test) 与 tests/ 都能访问),与
//! `astrcode-storage` 的 `testing` feature 同一惯例。

use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;

use super::{LlmError, LlmEvent, LlmProvider, LlmRequest, ModelLimits};

/// 测试惯例的输入上限,与既有手写 mock 的取值一致。
const TESTING_MAX_INPUT_TOKENS: usize = 200_000;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 1024;

/// 依脚本回放事件的 [`LlmProvider`]。
///
/// 第 n 次 `generate_request` 回放第 n 轮脚本,脚本耗尽后重复最后一轮——
/// 与手写静态 mock「每次调用返回相同事件序列」的行为一致。需要按请求内容
/// 分支、门控或阻塞的 mock 不适用本类型,应继续手写。
pub struct ScriptedLlm {
    rounds: Vec<Vec<LlmEvent>>,
    calls: AtomicUsize,
    limits: ModelLimits,
}

impl ScriptedLlm {
    /// 每次调用都回放同一事件序列。
    pub fn always(events: Vec<LlmEvent>) -> Self {
        Self::new(vec![events])
    }

    /// 逐轮回放事件序列。
    pub fn new(rounds: Vec<Vec<LlmEvent>>) -> Self {
        Self {
            rounds,
            calls: AtomicUsize::new(0),
            limits: ModelLimits::testing(DEFAULT_MAX_OUTPUT_TOKENS),
        }
    }

    /// 覆盖模型上限(如需模拟特定上下文窗口)。
    pub fn with_limits(mut self, limits: ModelLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 把一轮事件写入通道并返回接收端,供手写 mock 复用。
    pub fn event_channel(events: Vec<LlmEvent>) -> mpsc::UnboundedReceiver<LlmEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        for event in events {
            let _ = tx.send(event);
        }
        rx
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedLlm {
    async fn generate_request(
        &self,
        _request: LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        if self.rounds.is_empty() {
            // 空脚本只能来自构造时的空 Vec,属编程错误而非运行态。
            return Err(LlmError::Unsupported {
                message: "ScriptedLlm was built with an empty script".into(),
            });
        }
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        let index = round.min(self.rounds.len() - 1);
        Ok(Self::event_channel(self.rounds[index].clone()))
    }

    fn model_limits(&self) -> ModelLimits {
        self.limits.clone()
    }
}

impl ModelLimits {
    /// 测试惯例上限:输入 [`TESTING_MAX_INPUT_TOKENS`],输出由用例指定。
    pub fn testing(max_output_tokens: usize) -> Self {
        Self {
            max_input_tokens: TESTING_MAX_INPUT_TOKENS,
            max_output_tokens,
        }
    }
}

/// 断言永不被调用的桩 provider;任何请求都会以 `reason` panic。
///
/// 用于测试设计了「不跑 turn」路径的场景,panic 消息即失败原因。
pub struct NeverLlm(&'static str);

impl NeverLlm {
    pub fn new(reason: &'static str) -> Self {
        Self(reason)
    }
}

#[async_trait::async_trait]
impl LlmProvider for NeverLlm {
    async fn generate_request(
        &self,
        _request: LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        unreachable!("{}", self.0)
    }

    fn model_limits(&self) -> ModelLimits {
        // 沿用历史上各处 UnusedLlm 桩的 1024/1024,保持运行时服务装配行为不变。
        ModelLimits {
            max_input_tokens: 1024,
            max_output_tokens: 1024,
        }
    }
}
