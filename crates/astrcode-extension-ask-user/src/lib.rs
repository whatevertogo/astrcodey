mod model;
mod registry;

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use astrcode_extension_sdk::{
    builder::{ExtensionToolDefinition, extension_event, manifest},
    extension::{
        Extension, ExtensionCapability, ExtensionError, ExtensionEvent, ExtensionHttpHandler,
        ExtensionHttpMethod, ExtensionHttpResponse, ExtensionHttpRoute, ExtensionManifest,
        HookMode, HookResult, HttpContext, LifecycleContext, LifecycleHandler, Registrar,
        StopReason, ToolContext, ToolHandler,
    },
    tool::{ToolExecutionResult, ToolPromptMetadata, ToolPromptTag, ToolResult},
};
use model::{
    ASK_USER_TOOL_NAME, AnswerRequest, AskUserInput, PendingQuestion, tool_definition,
    validate_input,
};
use registry::{PendingRegistry, Resolution, ResolveError};
use serde_json::json;

const EXTENSION_ID: &str = "astrcode-ask-user";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// 用户在此时间内未响应时自动选择推荐选项（无推荐则继续等待到总超时）。
const AUTO_SELECT_DELAY: Duration = Duration::from_secs(60);
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
    fn manifest(&self) -> ExtensionManifest {
        manifest(EXTENSION_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::AuthenticatedHttp)
            .capability(ExtensionCapability::EmitEvents)
            .build()
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.tool(
            ExtensionToolDefinition::from_definition(tool_definition()).with_prompt(
                ToolPromptMetadata::new(String::new()).prompt_tag(ToolPromptTag::Planning),
            ),
            Arc::new(AskUserToolHandler {
                registry: Arc::clone(&self.registry),
                timeout: self.timeout,
            }),
        );
        registrar.declare_event(
            extension_event(registry::PENDING_EVENT_TYPE)
                .durable(false)
                .build(),
        );
        registrar.declare_event(
            extension_event(registry::RESOLVED_EVENT_TYPE)
                .durable(false)
                .build(),
        );

        let http = Arc::new(AskUserHttpHandler {
            registry: Arc::clone(&self.registry),
        });
        registrar.http_route(
            ExtensionHttpRoute::authenticated(ExtensionHttpMethod::Get, "/questions"),
            http.clone(),
        );
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
        registrar.on_lifecycle(
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
    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let tool_name = context.tool_name();
        if tool_name != ASK_USER_TOOL_NAME {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        let input: AskUserInput = context.arguments()?;
        if let Err(error) = validate_input(&input) {
            return Ok(ToolResult::error(error).into());
        }

        let call_id = context.require_call_id()?.to_owned();
        let session_id = context.call().require_session_id()?.to_string();
        let events = context.events().clone();
        let auto_select_at = if input
            .questions
            .iter()
            .all(|question| question.options.iter().any(|option| option.recommended))
        {
            auto_select_deadline_millis()
        } else {
            None
        };
        let pending =
            PendingQuestion::new(session_id.clone(), call_id.clone(), input, auto_select_at);
        let (mut receiver, mut guard) = self.registry.register(pending.clone(), events)?;

        let deadline = tokio::time::Instant::now() + self.timeout;
        let auto_select_deadline = tokio::time::Instant::now() + AUTO_SELECT_DELAY;
        let auto_select = tokio::time::sleep_until(auto_select_deadline);
        let timeout_sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(auto_select);
        tokio::pin!(timeout_sleep);
        // 无推荐选项时禁用自动选择分支，仅等待用户响应或总超时。
        let mut auto_select_enabled = true;
        let resolution = loop {
            tokio::select! {
                biased;
                received = &mut receiver => break received.map_err(|_| {
                    ExtensionError::Internal("askUser resolution channel closed".into())
                })?,
                () = &mut auto_select, if auto_select_enabled => {
                    match self.registry.auto_select_recommended(&session_id, &call_id) {
                        // AlreadyResolved:用户答案与自动选择竞态,resolve() 先从
                        // pending 移除再 send,答案已在 receiver 里,照常收取。
                        Ok(()) | Err(ResolveError::AlreadyResolved) => {
                            break receiver.await.map_err(|_| {
                                ExtensionError::Internal("askUser auto-select resolution channel closed".into())
                            })?;
                        },
                        Err(ResolveError::NoRecommended) => auto_select_enabled = false,
                        Err(error) => return Err(resolve_error_to_extension(error)),
                    }
                },
                () = &mut timeout_sleep => {
                    match self.registry.timeout(&session_id, &call_id) {
                        Ok(()) | Err(ResolveError::AlreadyResolved) => {},
                        Err(error) => return Err(resolve_error_to_extension(error)),
                    }
                    break receiver.await.map_err(|_| {
                        ExtensionError::Internal("askUser timeout resolution channel closed".into())
                    })?;
                },
            }
        };
        guard.disarm();

        Ok(resolution_result(&pending, resolution).into())
    }
}

fn resolution_result(pending: &PendingQuestion, resolution: Resolution) -> ToolResult {
    match resolution {
        Resolution::Answered(answers) => ToolResult::success(
            json!({
                "questions": pending.questions,
                "answers": answers,
            })
            .to_string(),
        ),
        Resolution::AutoAnswered(answers) => ToolResult::success(
            json!({
                "questions": pending.questions,
                "answers": answers,
                "autoSelected": true,
            })
            .to_string(),
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

fn auto_select_deadline_millis() -> Option<u64> {
    SystemTime::now()
        .checked_add(AUTO_SELECT_DELAY)?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

struct AskUserSessionShutdown {
    registry: Arc<PendingRegistry>,
}

#[async_trait::async_trait]
impl LifecycleHandler for AskUserSessionShutdown {
    async fn handle(&self, context: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let session_id = context.call().require_session_id()?;
        self.registry.shutdown_session(session_id.as_str());
        Ok(HookResult::Allow)
    }
}

struct AskUserHttpHandler {
    registry: Arc<PendingRegistry>,
}

#[async_trait::async_trait]
impl ExtensionHttpHandler for AskUserHttpHandler {
    async fn handle(&self, ctx: HttpContext) -> Result<ExtensionHttpResponse, ExtensionError> {
        let request = ctx.request();
        if request.method == ExtensionHttpMethod::Get && request.path == "/questions" {
            return Ok(ExtensionHttpResponse::json(
                200,
                json!({ "questions": self.registry.list_all() }),
            ));
        }
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
                let answer_request = match ctx.json::<AnswerRequest>() {
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
        Err(ResolveError::NoRecommended) => ExtensionHttpResponse::error(
            409,
            "no_recommended",
            "question has no recommended option",
        ),
    }
}

fn resolve_error_to_extension(error: ResolveError) -> ExtensionError {
    ExtensionError::Internal(match error {
        ResolveError::NotFound => "askUser question not found".into(),
        ResolveError::AlreadyResolved => "askUser call id was already used".into(),
        ResolveError::InvalidAnswers(message) => message,
        ResolveError::NoRecommended => "askUser question has no recommended option".into(),
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use astrcode_extension_sdk::{
        extension::{
            ExtensionEventDecl, ExtensionEventEmitter, ExtensionHttpRequest,
            internal::{ExtensionEventSink, extension_event_emitter},
        },
        testing::{HttpContextBuilder, ToolContextBuilder},
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

    fn event_emitter(events: Arc<dyn ExtensionEventSink>) -> ExtensionEventEmitter {
        extension_event_emitter(
            [
                ExtensionEventDecl {
                    event_type: registry::PENDING_EVENT_TYPE.into(),
                    schema_version: 1,
                    durable: false,
                    max_payload_bytes: 64 * 1024,
                },
                ExtensionEventDecl {
                    event_type: registry::RESOLVED_EVENT_TYPE.into(),
                    schema_version: 1,
                    durable: false,
                    max_payload_bytes: 64 * 1024,
                },
            ],
            Some(events),
        )
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
                            recommended: true,
                        },
                        AskUserOption {
                            label: "B".into(),
                            description: "Second".into(),
                            preview: None,
                            recommended: false,
                        },
                    ],
                    multi_select: false,
                }],
                metadata: None,
            },
            Some(60_000),
        )
    }

    fn http_context(request: ExtensionHttpRequest) -> HttpContext {
        let route = ExtensionHttpRoute::public(request.method, request.path.clone());
        HttpContextBuilder::new(EXTENSION_ID, route, request).build()
    }

    #[tokio::test]
    async fn registry_auto_selects_recommended_option_on_timeout() {
        let registry = Arc::new(PendingRegistry::default());
        let events = event_emitter(Arc::new(RecordingEvents::default()));

        let question = pending("session-1", "auto-ok");
        let (receiver, mut guard) = registry.register(question.clone(), events.clone()).unwrap();

        registry
            .auto_select_recommended("session-1", "auto-ok")
            .unwrap();
        assert!(matches!(
            receiver.await.unwrap(),
            Resolution::AutoAnswered(answers) if answers["Which approach?"] == "A"
        ));
        guard.disarm();
        assert!(registry.list("session-1").is_empty());

        // 无推荐选项时返回 NoRecommended 且不改变 pending 状态。
        let no_recommended = PendingQuestion::new(
            "session-1".into(),
            "auto-none".into(),
            AskUserInput {
                questions: vec![AskUserQuestion {
                    question: "Pick one?".into(),
                    header: "Pick".into(),
                    options: vec![
                        AskUserOption {
                            label: "A".into(),
                            description: "First".into(),
                            preview: None,
                            recommended: false,
                        },
                        AskUserOption {
                            label: "B".into(),
                            description: "Second".into(),
                            preview: None,
                            recommended: false,
                        },
                    ],
                    multi_select: false,
                }],
                metadata: None,
            },
            None,
        );
        let (_, mut no_recommended_guard) =
            registry.register(no_recommended, events.clone()).unwrap();
        assert!(matches!(
            registry.auto_select_recommended("session-1", "auto-none"),
            Err(ResolveError::NoRecommended)
        ));
        assert_eq!(registry.list("session-1").len(), 1);
        no_recommended_guard.disarm();
    }

    #[test]
    fn recommended_options_follow_the_declared_single_and_multi_select_semantics() {
        let input = AskUserInput {
            questions: vec![
                AskUserQuestion {
                    question: "Pick one?".into(),
                    header: "Single".into(),
                    options: vec![
                        AskUserOption {
                            label: "A".into(),
                            description: "First".into(),
                            preview: None,
                            recommended: true,
                        },
                        AskUserOption {
                            label: "B".into(),
                            description: "Second".into(),
                            preview: None,
                            recommended: true,
                        },
                    ],
                    multi_select: false,
                },
                AskUserQuestion {
                    question: "Pick several?".into(),
                    header: "Multiple".into(),
                    options: vec![
                        AskUserOption {
                            label: "X".into(),
                            description: "First".into(),
                            preview: None,
                            recommended: true,
                        },
                        AskUserOption {
                            label: "Y".into(),
                            description: "Second".into(),
                            preview: None,
                            recommended: true,
                        },
                    ],
                    multi_select: true,
                },
            ],
            metadata: None,
        };

        assert_eq!(validate_input(&input), Ok(()));
        let answers = PendingQuestion::new("session-1".into(), "call-1".into(), input, None)
            .auto_recommended_answers()
            .unwrap();
        assert_eq!(answers["Pick one?"], "A");
        assert_eq!(answers["Pick several?"], "X, Y");
    }

    #[tokio::test]
    async fn answer_winning_before_auto_select_is_not_lost() {
        let registry = Arc::new(PendingRegistry::default());
        let events = event_emitter(Arc::new(RecordingEvents::default()));
        let (receiver, mut guard) = registry
            .register(pending("session-1", "race"), events)
            .unwrap();

        // 用户答案先完成;自动选择随后只应看到 AlreadyResolved。
        registry
            .answer(
                "session-1",
                "race",
                HashMap::from([("Which approach?".into(), "B".into())]),
            )
            .unwrap();
        assert!(matches!(
            registry.auto_select_recommended("session-1", "race"),
            Err(ResolveError::AlreadyResolved)
        ));
        // execute 循环此时仍从 receiver 收取,用户答案不丢。
        assert!(matches!(
            receiver.await.unwrap(),
            Resolution::Answered(answers) if answers["Which approach?"] == "B"
        ));
        guard.disarm();
        assert!(matches!(
            registry.auto_select_recommended("session-1", "race"),
            Err(ResolveError::NotFound)
        ));
    }

    #[tokio::test]
    async fn registry_enforces_session_validation_and_single_winner() {
        let registry = Arc::new(PendingRegistry::default());
        let events = Arc::new(RecordingEvents::default());
        let event_sink: Arc<dyn ExtensionEventSink> = events.clone();
        let (receiver, mut guard) = registry
            .register(pending("session-1", "call-1"), event_emitter(event_sink))
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
            Err(ResolveError::NotFound)
        );

        let (reused, mut reused_guard) = registry
            .register(
                pending("session-1", "call-1"),
                event_emitter(Arc::new(RecordingEvents::default())),
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
        let events = event_emitter(Arc::new(RecordingEvents::default()));

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
        let context = ToolContextBuilder::new(EXTENSION_ID, ASK_USER_TOOL_NAME)
            .session("session-1", ".", None)
            .call_id("call-1")
            .arguments(json!({ "questions": pending("session-1", "call-1").questions }))
            .events(event_emitter(events))
            .build();
        let tool_registry = Arc::clone(&registry);
        let tool_task = tokio::spawn(async move {
            AskUserToolHandler {
                registry: tool_registry,
                timeout: Duration::from_secs(1),
            }
            .execute(context)
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
        let listed = handler.handle(http_context(list)).await.unwrap();
        assert_eq!(listed.status, 200);
        assert_eq!(listed.body["questions"][0]["callId"], "call-1");

        let all = handler
            .handle(http_context(ExtensionHttpRequest::new(
                ExtensionHttpMethod::Get,
                "/questions",
            )))
            .await
            .unwrap();
        assert_eq!(all.status, 200);
        assert_eq!(all.body["questions"][0]["sessionId"], "session-1");
        assert!(all.body["questions"][0]["serverTime"].is_number());

        let mut invalid = ExtensionHttpRequest::new(
            ExtensionHttpMethod::Post,
            "/sessions/session-1/questions/call-1/respond",
        )
        .json_body(json!({ "answers": {} }));
        invalid
            .path_params
            .insert("sessionId".into(), "session-1".into());
        invalid.path_params.insert("callId".into(), "call-1".into());
        assert_eq!(
            handler.handle(http_context(invalid)).await.unwrap().status,
            400
        );

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
        assert_eq!(
            handler
                .handle(http_context(wrong_session))
                .await
                .unwrap()
                .status,
            404
        );

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
        assert_eq!(
            handler
                .handle(http_context(answer.clone()))
                .await
                .unwrap()
                .status,
            200
        );
        assert_eq!(
            handler.handle(http_context(answer)).await.unwrap().status,
            409
        );
        let result = tool_task.await.unwrap().unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("\"Which approach?\":\"A\""));
    }
}
