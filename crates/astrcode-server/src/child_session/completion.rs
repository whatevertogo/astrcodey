use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use astrcode_core::types::{SessionId, TurnId};
use astrcode_session::{TurnError, TurnHandle, TurnShutdownHandle};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot, watch};

use super::{ChildCleanup, ChildCompletion, ChildOutcome, ChildSessionCompletionConfig};
use crate::task_utils::OwnedTaskAdmission;

const AGENT_NOTIFICATION_OUTPUT_MAX_BYTES: usize = 16 * 1024;

/// 只等待 `TurnHandle` 并记录 outcome；不写父 session 事件。
pub(super) struct ChildSessionCompletionGuard {
    config: ChildSessionCompletionConfig,
    completion_tx: watch::Sender<Option<ChildCompletion>>,
    completion_rx: watch::Receiver<Option<ChildCompletion>>,
    shutdown_handle: Option<TurnShutdownHandle>,
    child_settled: AtomicBool,
    force_recycle: AtomicBool,
    retry_attempt: AtomicU32,
    terminal_tx: Mutex<Option<oneshot::Sender<Result<(), String>>>>,
}

fn try_set_completion(tx: &watch::Sender<Option<ChildCompletion>>, completion: ChildCompletion) {
    let _ = tx.send_if_modified(|current| {
        if current.is_none() {
            *current = Some(completion);
            true
        } else {
            false
        }
    });
}

impl ChildSessionCompletionGuard {
    pub(super) fn new(handle: &TurnHandle, config: ChildSessionCompletionConfig) -> Self {
        Self::with_terminal_sender(handle, config, None)
    }

    pub(super) fn new_sync(
        handle: &TurnHandle,
        config: ChildSessionCompletionConfig,
    ) -> (Self, oneshot::Receiver<Result<(), String>>) {
        let (terminal_tx, terminal_rx) = oneshot::channel();
        (
            Self::with_terminal_sender(handle, config, Some(terminal_tx)),
            terminal_rx,
        )
    }

    fn with_terminal_sender(
        handle: &TurnHandle,
        config: ChildSessionCompletionConfig,
        terminal_tx: Option<oneshot::Sender<Result<(), String>>>,
    ) -> Self {
        let (completion_tx, completion_rx) = watch::channel(None);
        let shutdown_handle = handle.shutdown_handle();

        Self {
            config,
            completion_tx,
            completion_rx,
            shutdown_handle: Some(shutdown_handle),
            child_settled: AtomicBool::new(false),
            force_recycle: AtomicBool::new(false),
            retry_attempt: AtomicU32::new(0),
            terminal_tx: Mutex::new(terminal_tx),
        }
    }

    pub(super) fn start(
        &self,
        admission: OwnedTaskAdmission,
        handle: TurnHandle,
        completed_tx: mpsc::UnboundedSender<SessionId>,
    ) {
        let completion_tx = self.completion_tx.clone();
        let shutdown_handle = self.shutdown_handle.clone();
        let parent_session_id = self.config.parent_session_id.clone();

        admission.spawn_named("child_session_completion_guard", async move {
            let result = handle.wait().await;
            let completion = match result {
                Some(result) => {
                    let outcome = match result.output {
                        Ok(output) => ChildOutcome::Completed {
                            output: output.text,
                        },
                        Err(TurnError::Aborted) => ChildOutcome::Aborted,
                        Err(error) => ChildOutcome::Failed {
                            error: error.to_string(),
                        },
                    };
                    ChildCompletion {
                        outcome,
                        finalization: Some(result.finalization),
                    }
                },
                None => ChildCompletion {
                    outcome: ChildOutcome::Aborted,
                    finalization: shutdown_handle.and_then(|handle| handle.finalization()),
                },
            };
            try_set_completion(&completion_tx, completion);
            let _ = completed_tx.send(parent_session_id);
        });
    }

    pub(super) async fn completion(&self) -> ChildCompletion {
        let current = self.completion_rx.borrow().clone();
        if let Some(completion) = current {
            return completion;
        }
        let mut receiver = self.completion_rx.clone();
        let completion = match receiver.wait_for(Option::is_some).await {
            Ok(completion) => completion.clone().unwrap_or(ChildCompletion {
                outcome: ChildOutcome::Aborted,
                finalization: self
                    .shutdown_handle
                    .as_ref()
                    .and_then(TurnShutdownHandle::finalization),
            }),
            Err(_) => ChildCompletion {
                outcome: ChildOutcome::Aborted,
                finalization: self
                    .shutdown_handle
                    .as_ref()
                    .and_then(TurnShutdownHandle::finalization),
            },
        };
        completion
    }

    pub(super) async fn outcome(&self) -> ChildOutcome {
        self.completion().await.outcome
    }

    pub(super) fn is_complete(&self) -> bool {
        self.completion_rx.borrow().is_some()
    }

    pub(super) fn request_shutdown(&self) {
        if let Some(handle) = &self.shutdown_handle {
            handle.request_shutdown();
        }
    }

    pub(super) fn child_is_settled(&self) -> bool {
        self.child_settled.load(Ordering::Acquire)
    }

    pub(super) fn mark_child_settled(&self) {
        self.child_settled.store(true, Ordering::Release);
    }

    pub(super) fn force_recycle_on_completion(&self) {
        self.force_recycle.store(true, Ordering::Release);
    }

    pub(super) fn retry_delay_ms(&self) -> u64 {
        let attempt = self.retry_attempt.fetch_add(1, Ordering::AcqRel).min(5);
        50_u64 << attempt
    }

    pub(super) fn finish_terminal(&self, result: Result<(), String>) {
        if let Some(tx) = self.terminal_tx.lock().take() {
            let _ = tx.send(result);
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(super) fn has_terminal_waiter(&self) -> bool {
        self.terminal_tx.lock().is_some()
    }

    pub(super) fn force_timeout(&self) {
        if let Some(handle) = &self.shutdown_handle {
            handle.force_kill();
        }
        try_set_completion(
            &self.completion_tx,
            ChildCompletion {
                outcome: ChildOutcome::TimedOut,
                finalization: self
                    .shutdown_handle
                    .as_ref()
                    .and_then(TurnShutdownHandle::finalization),
            },
        );
    }

    pub(super) fn child_session_id(&self) -> &SessionId {
        &self.config.child_session_id
    }

    pub(super) fn parent_session_id(&self) -> &SessionId {
        &self.config.parent_session_id
    }

    pub(super) fn turn_id(&self) -> &TurnId {
        &self.config.turn_id
    }

    pub(super) fn cleanup_policy(&self) -> ChildCleanup {
        if self.force_recycle.load(Ordering::Acquire) {
            ChildCleanup::Recycle
        } else {
            self.config.cleanup
        }
    }

    pub(super) fn notify_text(&self) -> Option<&str> {
        self.config.notify_on_complete.as_deref()
    }

    pub(super) fn tool_call_id(&self) -> Option<&str> {
        self.config.tool_call_id.as_deref()
    }

    fn summary_hint(&self) -> Option<&str> {
        self.config
            .notify_on_complete
            .as_deref()
            .filter(|summary| !summary.trim().is_empty())
    }
}

pub(super) async fn build_background_agent_notification(
    guard: &ChildSessionCompletionGuard,
) -> String {
    let outcome = guard.outcome().await;
    let (status, error, output_body, output_truncated) = match &outcome {
        ChildOutcome::Completed { output } => {
            let (body, truncated) = truncate_notification_output(output);
            ("completed", None, body, truncated)
        },
        ChildOutcome::Failed { error } => ("failed", Some(error.as_str()), String::new(), false),
        ChildOutcome::Aborted => ("aborted", Some("aborted"), String::new(), false),
        ChildOutcome::TimedOut => ("timed_out", Some("timed out"), String::new(), false),
    };
    format_background_agent_notification(
        guard.child_session_id().as_str(),
        guard.tool_call_id(),
        status,
        error,
        guard.summary_hint(),
        &output_body,
        output_truncated,
    )
}

fn format_background_agent_notification(
    child_session_id: &str,
    tool_call_id: Option<&str>,
    status: &str,
    error: Option<&str>,
    summary_hint: Option<&str>,
    output_body: &str,
    output_truncated: bool,
) -> String {
    let tool_call_line = tool_call_id
        .map(|id| {
            let id = escape_notification_text(id);
            format!("\n<tool-call-id>{id}</tool-call-id>")
        })
        .unwrap_or_default();
    let error_line = error
        .map(|error| {
            let error = escape_notification_text(error);
            format!("\n<error>{error}</error>")
        })
        .unwrap_or_default();
    let output_truncated_line = if output_truncated {
        format!(
            "\n<output-truncated>Showing last {AGENT_NOTIFICATION_OUTPUT_MAX_BYTES} bytes; child \
             session transcript may contain more.</output-truncated>"
        )
    } else {
        String::new()
    };
    let output_section = if output_body.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n<output>{cdata}</output>{output_truncated_line}",
            cdata = wrap_agent_output_cdata(output_body),
        )
    };
    let summary = summary_hint
        .filter(|summary| !summary.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Background agent task {status}"));
    let child_session_id = escape_notification_text(child_session_id);
    let summary = escape_notification_text(&summary);
    format!(
        concat!(
            "<background-agent-notification>",
            "\n<child-session-id>{child_session_id}</child-session-id>{tool_call_line}",
            "\n<status>{status}</status>{error_line}{output_section}",
            "\n<summary>{summary}</summary>",
            "\n</background-agent-notification>",
        ),
        child_session_id = child_session_id,
        tool_call_line = tool_call_line,
        status = status,
        error_line = error_line,
        output_section = output_section,
        summary = summary,
    )
}

fn truncate_notification_output(text: &str) -> (String, bool) {
    let bytes = text.as_bytes();
    let truncated = bytes.len() > AGENT_NOTIFICATION_OUTPUT_MAX_BYTES;
    let start = bytes
        .len()
        .saturating_sub(AGENT_NOTIFICATION_OUTPUT_MAX_BYTES);
    (
        String::from_utf8_lossy(&bytes[start..]).into_owned(),
        truncated,
    )
}

fn wrap_agent_output_cdata(text: &str) -> String {
    if !text.contains("]]>") {
        return format!("<![CDATA[\n{text}\n]]>");
    }
    let escaped = text.replace("]]>", "]]]]><![CDATA[>");
    format!("<![CDATA[\n{escaped}\n]]>")
}

fn escape_notification_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_is_first_write_wins() {
        let (tx, rx) = watch::channel(None);
        try_set_completion(
            &tx,
            ChildCompletion {
                outcome: ChildOutcome::Completed {
                    output: "first".into(),
                },
                finalization: None,
            },
        );
        try_set_completion(
            &tx,
            ChildCompletion {
                outcome: ChildOutcome::Failed {
                    error: "second".into(),
                },
                finalization: None,
            },
        );
        assert_eq!(
            rx.borrow()
                .as_ref()
                .map(|completion| completion.outcome.clone()),
            Some(ChildOutcome::Completed {
                output: "first".into(),
            })
        );
    }

    #[test]
    fn background_agent_notification_includes_structured_output() {
        let message = format_background_agent_notification(
            "child-<1>&",
            Some("call-<9>"),
            "completed",
            None,
            Some("explore <task> & report"),
            "findings here",
            false,
        );
        assert!(message.contains("<child-session-id>child-&lt;1&gt;&amp;</child-session-id>"));
        assert!(message.contains("<tool-call-id>call-&lt;9&gt;</tool-call-id>"));
        assert!(message.contains("<status>completed</status>"));
        assert!(message.contains("findings here"));
        assert!(message.contains("\n<summary>explore &lt;task&gt; &amp; report</summary>"));
    }
}
