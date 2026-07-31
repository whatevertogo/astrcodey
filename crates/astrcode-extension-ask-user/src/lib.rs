mod model;
mod registry;

use std::{sync::Arc, time::Duration};

use astrcode_extension_sdk::{
    extension::{
        Extension, ExtensionCapability, ExtensionError, ExtensionEvent, ExtensionHttpHandler,
        ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
        HookMode, HookResult, LifecycleContext, LifecycleHandler, Registrar, StopReason,
        ToolHandler,
    },
    tool::{
        ExtensionToolContext, ToolExecutionResult, ToolPromptMetadata, ToolPromptTag, ToolResult,
    },
};
pub use model::{
    ASK_USER_TOOL_NAME, AskUserInput, AskUserMetadata, AskUserOption, AskUserQuestion,
    PendingQuestion,
};
use model::{AnswerRequest, tool_definition, validate_input};
use registry::{PendingRegistry, Resolution, ResolveError};
use serde_json::json;

const EXTENSION_ID: &str = "astrcode-ask-user";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
const CAPABILITIES: &[ExtensionCapability] = &[
    ExtensionCapability::AuthenticatedHttp,
    ExtensionCapability::EmitEvents,
];

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(AskUserExtension::new(DEFAULT_TIMEOUT))
}

struct AskUserExtension {
    registry: Arc<PendingRegistry>,
    timeout: Duration,
}

impl AskUserExtension {
    fn new(timeout: Duration) -> Self {
        Self {
            registry: Arc::new(PendingRegistry::default()),
            timeout,
        }
    }
}

#[async_trait::async_trait]
impl Extension for AskUserExtension {
    fn id(&self) -> &str {
        EXTENSION_ID
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        CAPABILITIES
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.tool(
            tool_definition(),
            Arc::new(AskUserToolHandler {
                registry: Arc::clone(&self.registry),
                timeout: self.timeout,
            }),
        );
        registrar.tool_metadata(std::collections::HashMap::from([(
            ASK_USER_TOOL_NAME.to_owned(),
            ToolPromptMetadata::new(String::new()).prompt_tag(ToolPromptTag::Planning),
        )]));
        registrar
            .extension_event(registry::PENDING_EVENT_TYPE)
            .durable(false)
            .register();
        registrar
            .extension_event(registry::RESOLVED_EVENT_TYPE)
            .durable(false)
            .register();

        let http = Arc::new(AskUserHttpHandler {
            registry: Arc::clone(&self.registry),
        });
        registrar.http_route(
            ExtensionHttpRoute::authenticated(
                ExtensionHttpMethod::Get,
                "/sessions/{sessionId}/questions",
            ),
            http.clone(),
        );
        registrar.http_route(
            ExtensionHttpRoute::authenticated(
                ExtensionHttpMethod::Post,
                "/sessions/{sessionId}/questions/{callId}/respond",
            ),
            http.clone(),
        );
        registrar.http_route(
            ExtensionHttpRoute::authenticated(
                ExtensionHttpMethod::Post,
                "/sessions/{sessionId}/questions/{callId}/reject",
            ),
            http,
        );
        registrar.on_event(
            ExtensionEvent::SessionShutdown,
            HookMode::Advisory,
            0,
            Arc::new(AskUserSessionShutdown {
                registry: Arc::clone(&self.registry),
            }),
        );
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        self.registry.shutdown_extension();
        Ok(())
    }
}

struct AskUserToolHandler {
    registry: Arc<PendingRegistry>,
    timeout: Duration,
}

#[async_trait::async_trait]
impl ToolHandler for AskUserToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        _working_dir: &str,
        context: &ExtensionToolContext,
    ) -> Result<ToolExecutionResult, ExtensionError> {
        if tool_name != ASK_USER_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        let input: AskUserInput = match serde_json::from_value(arguments) {
            Ok(input) => input,
            Err(error) => {
                return Ok(ToolResult::error(format!(
                    "invalid args for {ASK_USER_TOOL_NAME}: {error}"
                ))
                .into());
            },
        };
        if let Err(error) = validate_input(&input) {
            return Ok(ToolResult::error(error).into());
        }

        let call_id =
            context.scope.tool_call_id.clone().ok_or_else(|| {
                ExtensionError::Internal("askUser requires a tool call id".into())
            })?;
        let session_id = context.scope.session_id.to_string();
        let events = context
            .events
            .clone()
            .ok_or_else(|| ExtensionError::Internal("askUser event sink unavailable".into()))?;
        let pending = PendingQuestion::new(session_id.clone(), call_id.clone(), input);
        let (mut receiver, mut guard) = self.registry.register(pending.clone(), events)?;

        let sleep = tokio::time::sleep(self.timeout);
        tokio::pin!(sleep);
        let resolution = tokio::select! {
            biased;
            received = &mut receiver => received.map_err(|_| {
                ExtensionError::Internal("askUser resolution channel closed".into())
            })?,
            () = &mut sleep => {
                match self.registry.timeout(&session_id, &call_id) {
                    Ok(()) | Err(ResolveError::AlreadyResolved) => {},
                    Err(error) => return Err(resolve_error_to_extension(error)),
                }
                receiver.await.map_err(|_| {
                    ExtensionError::Internal("askUser timeout resolution channel closed".into())
                })?
            },
        };
        guard.disarm();

        Ok(resolution_result(&pending, resolution).into())
    }
}

fn resolution_result(pending: &PendingQuestion, resolution: Resolution) -> ToolResult {
    match resolution {
        Resolution::Answered(answers) => ToolResult::success(
            serde_json::to_string(&json!({
                "questions": pending.questions,
                "answers": answers,
            }))
            .unwrap_or_else(|_| "{}".into()),
        ),
        Resolution::Rejected => ToolResult::error("User rejected the question"),
        Resolution::TimedOut => ToolResult::error("Timed out waiting for user response"),
        Resolution::TurnCancelled => ToolResult::error("Turn cancelled while waiting for user"),
        Resolution::SessionShutdown => ToolResult::error("Session closed while waiting for user"),
        Resolution::ExtensionShutdown => {
            ToolResult::error("askUser extension stopped while waiting for user")
        },
    }
}

struct AskUserSessionShutdown {
    registry: Arc<PendingRegistry>,
}

#[async_trait::async_trait]
impl LifecycleHandler for AskUserSessionShutdown {
    async fn handle(&self, context: LifecycleContext) -> Result<HookResult, ExtensionError> {
        self.registry.shutdown_session(&context.session_id);
        Ok(HookResult::Allow)
    }
}

struct AskUserHttpHandler {
    registry: Arc<PendingRegistry>,
}

#[async_trait::async_trait]
impl ExtensionHttpHandler for AskUserHttpHandler {
    async fn handle(
        &self,
        request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ExtensionError> {
        let Some(session_id) = request.path_params.get("sessionId") else {
            return Ok(ExtensionHttpResponse::error(
                404,
                "not_found",
                "session not found",
            ));
        };
        let response = match (request.method, request.path.as_str()) {
            (ExtensionHttpMethod::Get, _) => ExtensionHttpResponse::json(
                200,
                json!({ "questions": self.registry.list(session_id) }),
            ),
            (ExtensionHttpMethod::Post, path) if path.ends_with("/respond") => {
                let Some(call_id) = request.path_params.get("callId") else {
                    return Ok(ExtensionHttpResponse::error(
                        404,
                        "not_found",
                        "question not found",
                    ));
                };
                let answer_request = match serde_json::from_value::<AnswerRequest>(request.body) {
                    Ok(answer_request) => answer_request,
                    Err(error) => {
                        return Ok(ExtensionHttpResponse::error(
                            400,
                            "invalid_answers",
                            error.to_string(),
                        ));
                    },
                };
                resolve_http_result(self.registry.answer(
                    session_id,
                    call_id,
                    answer_request.answers,
                ))
            },
            (ExtensionHttpMethod::Post, path) if path.ends_with("/reject") => {
                let Some(call_id) = request.path_params.get("callId") else {
                    return Ok(ExtensionHttpResponse::error(
                        404,
                        "not_found",
                        "question not found",
                    ));
                };
                resolve_http_result(self.registry.reject(session_id, call_id))
            },
            _ => ExtensionHttpResponse::error(404, "not_found", "route not found"),
        };
        Ok(response)
    }
}

fn resolve_http_result(result: Result<(), ResolveError>) -> ExtensionHttpResponse {
    match result {
        Ok(()) => ExtensionHttpResponse::json(200, json!({ "ok": true })),
        Err(ResolveError::NotFound) => {
            ExtensionHttpResponse::error(404, "not_found", "question not found")
        },
        Err(ResolveError::AlreadyResolved) => {
            ExtensionHttpResponse::error(409, "already_resolved", "question was already resolved")
        },
        Err(ResolveError::InvalidAnswers(message)) => {
            ExtensionHttpResponse::error(400, "invalid_answers", message)
        },
    }
}

fn resolve_error_to_extension(error: ResolveError) -> ExtensionError {
    ExtensionError::Internal(match error {
        ResolveError::NotFound => "askUser question not found".into(),
        ResolveError::AlreadyResolved => "askUser call id was already used".into(),
        ResolveError::InvalidAnswers(message) => message,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use astrcode_extension_sdk::{
        extension::ExtensionEventSink,
        tool::{ToolCapabilities, ToolExecutionContext},
        types::SessionId,
    };

    use super::*;
    use crate::model::{AskUserOption, AskUserQuestion};

    #[derive(Default)]
    struct RecordingEvents(Mutex<Vec<(String, serde_json::Value)>>);

    impl ExtensionEventSink for RecordingEvents {
        fn emit(
            &self,
            event_type: &str,
            _schema_version: u32,
            payload: serde_json::Value,
        ) -> Result<(), ExtensionError> {
            self.0
                .lock()
                .unwrap()
                .push((event_type.to_owned(), payload));
            Ok(())
        }
    }

    fn pending(session_id: &str, call_id: &str) -> PendingQuestion {
        PendingQuestion::new(
            session_id.into(),
            call_id.into(),
            AskUserInput {
                questions: vec![AskUserQuestion {
                    question: "Which approach?".into(),
                    header: "Approach".into(),
                    options: vec![
                        AskUserOption {
                            label: "A".into(),
                            description: "First".into(),
                            preview: None,
                        },
                        AskUserOption {
                            label: "B".into(),
                            description: "Second".into(),
                            preview: None,
                        },
                    ],
                    multi_select: false,
                }],
                metadata: None,
            },
        )
    }

    #[tokio::test]
    async fn registry_enforces_session_validation_and_single_winner() {
        let registry = Arc::new(PendingRegistry::default());
        let events = Arc::new(RecordingEvents::default());
        let event_sink: Arc<dyn ExtensionEventSink> = events.clone();
        let (receiver, mut guard) = registry
            .register(pending("session-1", "call-1"), event_sink)
            .unwrap();

        assert_eq!(registry.list("session-1").len(), 1);
        assert!(matches!(
            registry.answer("wrong-session", "call-1", HashMap::new()),
            Err(ResolveError::NotFound)
        ));
        assert!(matches!(
            registry.answer("session-1", "call-1", HashMap::new()),
            Err(ResolveError::InvalidAnswers(_))
        ));

        registry
            .answer(
                "session-1",
                "call-1",
                HashMap::from([("Which approach?".into(), "A".into())]),
            )
            .unwrap();
        assert!(matches!(
            receiver.await.unwrap(),
            Resolution::Answered(answers) if answers["Which approach?"] == "A"
        ));
        guard.disarm();
        assert!(registry.list("session-1").is_empty());
        assert_eq!(
            registry.reject("session-1", "call-1"),
            Err(ResolveError::AlreadyResolved)
        );

        let (reused, mut reused_guard) = registry
            .register(
                pending("session-1", "call-1"),
                Arc::new(RecordingEvents::default()),
            )
            .unwrap();
        drop(guard);
        assert_eq!(registry.list("session-1").len(), 1);
        registry.reject("session-1", "call-1").unwrap();
        assert_eq!(reused.await.unwrap(), Resolution::Rejected);
        reused_guard.disarm();

        let event_types = events
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(event_type, _)| event_type.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            event_types,
            vec![
                registry::PENDING_EVENT_TYPE.to_owned(),
                registry::RESOLVED_EVENT_TYPE.to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn registry_resolves_reject_timeout_cancellation_and_shutdown() {
        let registry = Arc::new(PendingRegistry::default());
        let events: Arc<dyn ExtensionEventSink> = Arc::new(RecordingEvents::default());

        let (rejected, mut rejected_guard) = registry
            .register(pending("session-1", "reject"), events.clone())
            .unwrap();
        registry.reject("session-1", "reject").unwrap();
        assert_eq!(rejected.await.unwrap(), Resolution::Rejected);
        rejected_guard.disarm();

        let (timed_out, mut timeout_guard) = registry
            .register(pending("session-1", "timeout"), events.clone())
            .unwrap();
        registry.timeout("session-1", "timeout").unwrap();
        assert_eq!(timed_out.await.unwrap(), Resolution::TimedOut);
        timeout_guard.disarm();

        let (cancelled, cancelled_guard) = registry
            .register(pending("session-1", "cancel"), events.clone())
            .unwrap();
        drop(cancelled_guard);
        assert_eq!(cancelled.await.unwrap(), Resolution::TurnCancelled);

        let (session_shutdown, mut session_guard) = registry
            .register(pending("session-2", "shutdown"), events.clone())
            .unwrap();
        registry.shutdown_session("session-2");
        assert_eq!(session_shutdown.await.unwrap(), Resolution::SessionShutdown);
        session_guard.disarm();

        let (extension_shutdown, mut extension_guard) = registry
            .register(pending("session-3", "shutdown"), events)
            .unwrap();
        registry.shutdown_extension();
        assert_eq!(
            extension_shutdown.await.unwrap(),
            Resolution::ExtensionShutdown
        );
        extension_guard.disarm();
    }

    #[tokio::test]
    async fn authenticated_http_contract_uses_expected_statuses() {
        let registry = Arc::new(PendingRegistry::default());
        let events: Arc<dyn ExtensionEventSink> = Arc::new(RecordingEvents::default());
        let context = ExtensionToolContext::new(
            ToolExecutionContext::new(
                SessionId::new("session-1"),
                ".",
                Some("call-1".into()),
                None,
                ToolCapabilities::default(),
            ),
            Some(events),
        );
        let tool_registry = Arc::clone(&registry);
        let tool_task = tokio::spawn(async move {
            AskUserToolHandler {
                registry: tool_registry,
                timeout: Duration::from_secs(1),
            }
            .execute(
                ASK_USER_TOOL_NAME,
                json!({ "questions": pending("session-1", "call-1").questions }),
                ".",
                &context,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while registry.list("session-1").is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let handler = AskUserHttpHandler {
            registry: Arc::clone(&registry),
        };

        let mut list =
            ExtensionHttpRequest::new(ExtensionHttpMethod::Get, "/sessions/session-1/questions");
        list.path_params
            .insert("sessionId".into(), "session-1".into());
        let listed = handler.handle(list).await.unwrap();
        assert_eq!(listed.status, 200);
        assert_eq!(listed.body["questions"][0]["callId"], "call-1");

        let mut invalid = ExtensionHttpRequest::new(
            ExtensionHttpMethod::Post,
            "/sessions/session-1/questions/call-1/respond",
        )
        .json_body(json!({ "answers": {} }));
        invalid
            .path_params
            .insert("sessionId".into(), "session-1".into());
        invalid.path_params.insert("callId".into(), "call-1".into());
        assert_eq!(handler.handle(invalid).await.unwrap().status, 400);

        let mut wrong_session = ExtensionHttpRequest::new(
            ExtensionHttpMethod::Post,
            "/sessions/wrong/questions/call-1/reject",
        );
        wrong_session
            .path_params
            .insert("sessionId".into(), "wrong".into());
        wrong_session
            .path_params
            .insert("callId".into(), "call-1".into());
        assert_eq!(handler.handle(wrong_session).await.unwrap().status, 404);

        let mut answer = ExtensionHttpRequest::new(
            ExtensionHttpMethod::Post,
            "/sessions/session-1/questions/call-1/respond",
        )
        .json_body(json!({
            "answers": { "Which approach?": "A" }
        }));
        answer
            .path_params
            .insert("sessionId".into(), "session-1".into());
        answer.path_params.insert("callId".into(), "call-1".into());
        assert_eq!(handler.handle(answer.clone()).await.unwrap().status, 200);
        assert_eq!(handler.handle(answer).await.unwrap().status, 409);
        let result = tool_task.await.unwrap().unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("\"Which approach?\":\"A\""));
    }
}
