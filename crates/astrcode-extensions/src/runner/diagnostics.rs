use std::{collections::BTreeMap, time::Duration};

use super::ExtensionRunner;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExtensionStageStatus {
    #[default]
    Unknown,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionStageDiagnostics {
    pub status: ExtensionStageStatus,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionDiagnostics {
    pub load: ExtensionStageDiagnostics,
    pub register: ExtensionStageDiagnostics,
    pub start: ExtensionStageDiagnostics,
    pub hook_calls: u64,
    pub hook_timeouts: u64,
    pub last_hook: Option<String>,
    pub last_duration_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ExtensionDiagnosticStage {
    Load,
    Register,
    Start,
}

pub(super) enum ExtensionStageOutcome {
    Succeeded,
    Failed(String),
    Skipped,
}

/// 一次主动健康检查的扩展级结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionHealthReport {
    pub extension_id: String,
    pub error: Option<String>,
}

impl ExtensionHealthReport {
    pub fn is_healthy(&self) -> bool {
        self.error.is_none()
    }
}

impl ExtensionRunner {
    /// 主动采样已运行扩展的健康状态，不创建后台轮询任务。
    pub async fn check_health(&self) -> Vec<ExtensionHealthReport> {
        let extensions = self.extensions.read().await.clone();
        let mut reports = Vec::with_capacity(extensions.len());
        for extension in extensions {
            let extension_id = extension.id().to_string();
            let error = self
                .run_with_timeout(extension.health())
                .await
                .err()
                .map(|error| error.to_string());
            reports.push(ExtensionHealthReport {
                extension_id,
                error,
            });
        }
        reports
    }

    pub fn record_extension_load_success(&self, extension_id: &str, elapsed: Option<Duration>) {
        self.record_stage_result(
            extension_id,
            ExtensionDiagnosticStage::Load,
            elapsed,
            ExtensionStageOutcome::Succeeded,
        );
    }

    pub fn record_extension_load_failure(
        &self,
        extension_id: &str,
        error: impl Into<String>,
        elapsed: Option<Duration>,
    ) {
        self.record_stage_result(
            extension_id,
            ExtensionDiagnosticStage::Load,
            elapsed,
            ExtensionStageOutcome::Failed(error.into()),
        );
    }

    pub(super) fn record_stage_running(&self, extension_id: &str, stage: ExtensionDiagnosticStage) {
        let mut diagnostics = self.diagnostics.write();
        let entry = diagnostics.entry(extension_id.to_string()).or_default();
        let stage = stage_diagnostics_mut(entry, stage);
        stage.status = ExtensionStageStatus::Running;
        stage.duration_ms = None;
        stage.error = None;
    }

    pub(super) fn record_stage_result(
        &self,
        extension_id: &str,
        stage: ExtensionDiagnosticStage,
        elapsed: Option<Duration>,
        outcome: ExtensionStageOutcome,
    ) {
        let (status, error) = match outcome {
            ExtensionStageOutcome::Succeeded => (ExtensionStageStatus::Succeeded, None),
            ExtensionStageOutcome::Failed(error) => (ExtensionStageStatus::Failed, Some(error)),
            ExtensionStageOutcome::Skipped => (ExtensionStageStatus::Skipped, None),
        };
        let mut diagnostics = self.diagnostics.write();
        let entry = diagnostics.entry(extension_id.to_string()).or_default();
        let stage = stage_diagnostics_mut(entry, stage);
        stage.status = status;
        stage.duration_ms = elapsed.map(|duration| duration.as_millis() as u64);
        stage.error = error;
    }

    pub(super) fn record_hook_result(
        &self,
        extension_id: &str,
        hook: &'static str,
        elapsed: Duration,
        error: Option<String>,
        timed_out: bool,
    ) {
        let mut diagnostics = self.diagnostics.write();
        let entry = diagnostics.entry(extension_id.to_string()).or_default();
        entry.hook_calls = entry.hook_calls.saturating_add(1);
        if timed_out {
            entry.hook_timeouts = entry.hook_timeouts.saturating_add(1);
        }
        entry.last_hook = Some(hook.to_string());
        entry.last_duration_ms = Some(elapsed.as_millis() as u64);
        entry.last_error = error;
    }

    pub fn diagnostics_snapshot(&self) -> BTreeMap<String, ExtensionDiagnostics> {
        self.diagnostics.read().clone()
    }
}

fn stage_diagnostics_mut(
    diagnostics: &mut ExtensionDiagnostics,
    stage: ExtensionDiagnosticStage,
) -> &mut ExtensionStageDiagnostics {
    match stage {
        ExtensionDiagnosticStage::Load => &mut diagnostics.load,
        ExtensionDiagnosticStage::Register => &mut diagnostics.register,
        ExtensionDiagnosticStage::Start => &mut diagnostics.start,
    }
}
