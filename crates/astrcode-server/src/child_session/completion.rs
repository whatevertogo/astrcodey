use std::sync::Arc;

use astrcode_core::types::{SessionId, TurnId};
use astrcode_session::{TurnError, TurnHandle, TurnShutdownHandle};
use tokio::sync::{mpsc, watch};

use super::{ChildCleanup, ChildOutcome, ChildSessionCompletionConfig};
use crate::session_manager::SessionManager;

const AGENT_NOTIFICATION_OUTPUT_MAX_BYTES: usize = 16 * 1024;

/// 只等待 `TurnHandle` 并记录 outcome；不写父 session 事件。
pub(super) struct ChildSessionCompletionGuard {
    config: ChildSessionCompletionConfig,
    outcome_tx: watch::Sender<Option<ChildOutcome>>,
    outcome_rx: watch::Receiver<Option<ChildOutcome>>,
    shutdown_handle: TurnShutdownHandle,
}

fn try_set_outcome(tx: &watch::Sender<Option<ChildOutcome>>, outcome: ChildOutcome) {
    let _ = tx.send_if_modified(|current| {
        if current.is_none() {
            *current = Some(outcome);
            true
        } else {
            false
        }
    });
}

impl ChildSessionCompletionGuard {
    pub(super) fn spawn(
        handle: TurnHandle,
        config: ChildSessionCompletionConfig,
        completed_tx: mpsc::Sender<SessionId>,
    ) -> Self {
        let (outcome_tx, outcome_rx) = watch::channel(None);
        let outcome_tx_for_task = outcome_tx.clone();
        let shutdown_handle = handle.shutdown_handle();
        let parent_session_id = config.parent_session_id.clone();

        crate::task_utils::spawn_traced("child_session_completion_guard", async move {
            let result = handle.wait().await;
            let outcome = match result {
                Some(result) => match result.output {
                    Ok(output) => ChildOutcome::Completed {
                        output: output.text,
                    },
                    Err(TurnError::Aborted) => ChildOutcome::Aborted,
                    Err(error) => ChildOutcome::Failed {
                        error: error.to_string(),
                    },
                },
                None => ChildOutcome::Aborted,
            };
            try_set_outcome(&outcome_tx_for_task, outcome);
            let _ = completed_tx.send(parent_session_id).await;
        });

        Self {
            config,
            outcome_tx,
            outcome_rx,
            shutdown_handle,
        }
    }

    pub(super) async fn outcome(&self) -> ChildOutcome {
        let current = self.outcome_rx.borrow().clone();
        if let Some(outcome) = current {
            return outcome;
        }
        let mut receiver = self.outcome_rx.clone();
        let outcome = match receiver.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone().unwrap_or(ChildOutcome::Aborted),
            Err(_) => ChildOutcome::Aborted,
        };
        outcome
    }

    pub(super) fn is_complete(&self) -> bool {
        self.outcome_rx.borrow().is_some()
    }

    pub(super) fn request_shutdown(&self) {
        self.shutdown_handle.request_shutdown();
    }

    pub(super) fn force_timeout(&self) {
        self.shutdown_handle.force_kill();
        try_set_outcome(&self.outcome_tx, ChildOutcome::TimedOut);
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
        self.config.cleanup
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

async fn append_parent_agent_event(
    session_manager: &Arc<SessionManager>,
    parent_session_id: &SessionId,
    child_session_id: &SessionId,
    payload: astrcode_core::event::DurableEventPayload,
    failure_log: &'static str,
) {
    if let Ok(parent_session) = session_manager.open(parent_session_id.clone()).await {
        if let Err(error) = parent_session.emit_durable(None, payload).await {
            tracing::warn!(
                parent_session_id = %parent_session_id,
                child_session_id = %child_session_id,
                error = %error,
                "{failure_log}"
            );
        }
    }
}

pub(super) async fn write_agent_completed(
    session_manager: &Arc<SessionManager>,
    parent_session_id: &SessionId,
    child_session_id: &SessionId,
    summary: &str,
) {
    append_parent_agent_event(
        session_manager,
        parent_session_id,
        child_session_id,
        astrcode_session::payload::agent_session_completed_payload(
            child_session_id.clone(),
            one_line_summary(summary),
        ),
        "failed to append AgentSessionCompleted event",
    )
    .await;
}

pub(super) async fn write_agent_failed(
    session_manager: &Arc<SessionManager>,
    parent_session_id: &SessionId,
    child_session_id: &SessionId,
    error: &str,
) {
    append_parent_agent_event(
        session_manager,
        parent_session_id,
        child_session_id,
        astrcode_session::payload::agent_session_failed_payload(
            child_session_id.clone(),
            error.to_string(),
        ),
        "failed to append AgentSessionFailed event",
    )
    .await;
}

fn one_line_summary(text: &str) -> String {
    crate::presentation::inline_preview(text, 159)
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
    fn outcome_is_first_write_wins() {
        let (tx, rx) = watch::channel(None);
        try_set_outcome(
            &tx,
            ChildOutcome::Completed {
                output: "first".into(),
            },
        );
        try_set_outcome(
            &tx,
            ChildOutcome::Failed {
                error: "second".into(),
            },
        );
        assert_eq!(
            rx.borrow().clone(),
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
