//! Prompt 组装的诊断信息。
//!
//! 在 prompt 构建过程中，composer 会收集各类诊断信息：
//! block 被条件跳过、依赖缺失、模板变量缺失、渲染失败等。
//! 这些诊断可用于调试 prompt 组装问题，或在严格模式下触发错误。

use chrono::{DateTime, Utc};

/// 诊断信息的严重级别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticLevel {
    /// 信息性消息（如缓存命中/未命中）。
    Info,
    /// 警告（如依赖缺失但非严格模式）。
    Warning,
    /// 错误（在严格模式下会导致构建失败）。
    Error,
}

/// 诊断的具体原因。
///
/// 每种原因对应 prompt 组装管线中的一个特定事件或失败点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticReason {
    /// Block 的依赖项未就绪（被跳过或失败）。
    MissingDependency { dependency_id: String },
    /// 模板中的变量无法解析。
    TemplateVariableMissing { variable: String },
    /// 模板渲染过程出错。
    RenderFailed { message: String },
    /// Block 内容验证失败（如空标题、空内容）。
    ValidationFailed { message: String },
    /// Contributor / layer cache 命中，跳过了重新收集。
    CacheReuseHit {
        contributor_id: String,
        fingerprint: Option<String>,
    },
    /// Contributor / layer cache 未命中，执行了重新收集。
    CacheReuseMiss {
        contributor_id: String,
        fingerprint: Option<String>,
        invalidation_reason: Option<String>,
    },
}

/// 单条诊断信息。
///
/// 包含严重级别、关联的 block/contributor、原因和建议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDiagnostic {
    pub level: DiagnosticLevel,
    pub block_id: Option<String>,
    pub contributor_id: Option<String>,
    pub reason: DiagnosticReason,
    pub suggestion: Option<String>,
    pub timestamp: DateTime<Utc>,
}

/// 诊断信息集合。
///
/// 在 [`PromptComposer::build`](crate::composer::PromptComposer::build) 过程中累积，
/// 最终随 [`PromptBuildOutput`](crate::composer::PromptBuildOutput) 返回给调用者。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptDiagnostics {
    pub items: Vec<PromptDiagnostic>,
}

impl PromptDiagnostics {
    pub fn push(&mut self, diagnostic: PromptDiagnostic) {
        self.items.push(diagnostic);
    }

    pub fn push_cache_reuse_hit(
        &mut self,
        contributor_id: impl Into<String>,
        fingerprint: Option<String>,
    ) {
        let contributor_id = contributor_id.into();
        self.push(PromptDiagnostic {
            level: DiagnosticLevel::Info,
            block_id: None,
            contributor_id: Some(contributor_id.clone()),
            reason: DiagnosticReason::CacheReuseHit {
                contributor_id,
                fingerprint,
            },
            suggestion: None,
            timestamp: Utc::now(),
        });
    }

    pub fn push_cache_reuse_miss(
        &mut self,
        contributor_id: impl Into<String>,
        fingerprint: Option<String>,
        invalidation_reason: Option<String>,
    ) {
        let contributor_id = contributor_id.into();
        self.push(PromptDiagnostic {
            level: DiagnosticLevel::Info,
            block_id: None,
            contributor_id: Some(contributor_id.clone()),
            reason: DiagnosticReason::CacheReuseMiss {
                contributor_id,
                fingerprint,
                invalidation_reason,
            },
            suggestion: None,
            timestamp: Utc::now(),
        });
    }
}
