use std::path::Path;

use astrcode_core::{
    permission::ApprovalSource,
    tool::{ExecutionMode, ToolDefinition, ToolPlanningContext, access::ToolPlan},
};
use astrcode_extension_sdk::extension::{
    PreToolUseAdmission, internal::runtime_pre_tool_use_context,
};

use super::{ToolCalls, events::declare_tool_batch};
use crate::{
    deferred_tools::{tool_is_visible, unavailable_tool_guidance},
    permission::{PermissionContext, PermissionResolution},
    tool_deduplicator::{SameStepCheck, ToolCallDeduplicator},
    tool_json_repair::parse_and_repair_json,
    tool_types::{
        PreparedToolApproval, PreparedToolDisposition, PreparedToolInvocation, StreamedToolCall,
        ToolBatch,
    },
    turn_context::TurnError,
    turn_publish::TurnEvents,
    turn_stages::TurnState,
};

impl ToolCalls {
    /// 准备单个工具调用：JSON 解析、可见性检查、PreToolUse 钩子、权限链、去重。
    async fn prepare_single_tool_call(
        &self,
        tc: &StreamedToolCall,
        index: usize,
        tools: &[ToolDefinition],
        deduplicator: &mut ToolCallDeduplicator,
    ) -> Result<PreparedToolInvocation, TurnError> {
        let args = match parse_and_repair_json(&tc.arguments, &tc.name) {
            Ok(arguments) => arguments,
            Err(error) => return Ok(reject_malformed_tool_call(tc, index, &error)),
        };

        if !tool_is_visible(tools, &tc.name) {
            let guidance =
                unavailable_tool_guidance(&tc.name, tools, &self.tool_registry.list_definitions());
            return Ok(PreparedToolInvocation {
                index,
                call_id: tc.call_id.clone(),
                name: tc.name.clone(),
                tool_input: args,
                raw_arguments: None,
                plan: ToolPlan::default(),
                mode: ExecutionMode::Sequential,
                discovery_gate: None,
                disposition: PreparedToolDisposition::Rejected { error: guidance },
            });
        }

        let call = self.turn.shared.hook_call_context();
        let transform_ctx = runtime_pre_tool_use_context(
            call.clone(),
            tc.call_id.clone().into(),
            tc.name.clone(),
            args,
            self.turn.shared.approval_mode,
            tools.to_vec(),
        );
        let mut approvals = Vec::new();
        let mut tool_input = self
            .extension_runner
            .transform_tool_input(transform_ctx)
            .await?;
        let mut disposition = PreparedToolDisposition::Execute;

        let mut plan = ToolPlan::default();
        if let Err(error) = self
            .tool_registry
            .normalize_final_arguments(&tc.name, &mut tool_input)
        {
            disposition = PreparedToolDisposition::Rejected {
                error: format!("failed to normalize final tool arguments: {error}"),
            };
        } else {
            let admission_ctx = runtime_pre_tool_use_context(
                call,
                tc.call_id.clone().into(),
                tc.name.clone(),
                tool_input.clone(),
                self.turn.shared.approval_mode,
                tools.to_vec(),
            );
            match self
                .extension_runner
                .emit_pre_tool_use(admission_ctx)
                .await?
            {
                PreToolUseAdmission::Allow => {},
                PreToolUseAdmission::Block { reason } => {
                    disposition = PreparedToolDisposition::Rejected {
                        error: format!("Tool execution blocked by hook: {reason}"),
                    };
                },
                PreToolUseAdmission::Ask { requirements } => {
                    for requirement in requirements {
                        match requirement.rule_key.as_deref() {
                            Some(key)
                                if self.turn.shared.approval_history.is_denied_always(key) =>
                            {
                                disposition = PreparedToolDisposition::Rejected {
                                    error: format!(
                                        "Denied by session approval memory for extension rule \
                                         `{key}`"
                                    ),
                                };
                                break;
                            },
                            Some(key)
                                if self.turn.shared.approval_history.is_allowed_always(key) => {},
                            _ => approvals.push(PreparedToolApproval {
                                prompt: requirement.prompt,
                                rule_key: requirement.rule_key,
                                source: ApprovalSource::Extension,
                            }),
                        }
                    }
                },
            }

            if matches!(disposition, PreparedToolDisposition::Execute) {
                match self
                    .tool_registry
                    .plan(
                        &tc.name,
                        &tool_input,
                        &tool_planning_context(
                            &self.turn.shared,
                            &tc.call_id,
                            self.cancellation_token.clone(),
                        ),
                    )
                    .await
                {
                    Ok(planned) => plan = planned,
                    Err(error) => {
                        disposition = PreparedToolDisposition::Rejected {
                            error: format!("failed to plan tool resources: {error}"),
                        };
                    },
                }
            }
        }

        if matches!(disposition, PreparedToolDisposition::Execute) {
            disposition = compose_permission_resolution(
                approvals,
                self.evaluate_permission_chain(&tc.name, &tool_input, &plan),
            );
        }

        let same_step = deduplicator.check_same_step(&tc.call_id, &tc.name, &tool_input);
        // 同 step 内相同 (toolName, args) 的调用只有一种结果，重复调用一律复用 Primary
        // 的最终结果——即使本调用已被拒绝或待审批（同一输入下其拒绝/审批结果与
        // Primary 一致），避免对同一调用重复发出拒绝/审批事件。这是刻意的去重语义。
        if same_step == SameStepCheck::Duplicate {
            disposition = PreparedToolDisposition::ReuseSameStep;
        }

        let mode = match &disposition {
            PreparedToolDisposition::Execute => self.tool_registry.execution_mode(&tc.name),
            PreparedToolDisposition::Rejected { .. }
            | PreparedToolDisposition::ReuseSameStep
            | PreparedToolDisposition::AwaitApprovals(_) => ExecutionMode::Sequential,
        };

        Ok(PreparedToolInvocation {
            index,
            call_id: tc.call_id.clone(),
            name: tc.name.clone(),
            tool_input,
            raw_arguments: None,
            plan,
            mode,
            discovery_gate: self
                .tool_registry
                .find_prompt_metadata(&tc.name)
                .and_then(|metadata| metadata.deferred_discovery_gate),
            disposition,
        })
    }

    pub(crate) async fn prepare_tool_batch(
        &self,
        tool_calls: &[StreamedToolCall],
        visible_tools: &[ToolDefinition],
        state: &mut TurnState,
    ) -> Result<ToolBatch, TurnError> {
        let mut prepared = Vec::with_capacity(tool_calls.len());

        for (index, tool_call) in tool_calls.iter().enumerate() {
            let prepared_call = self
                .prepare_single_tool_call(
                    tool_call,
                    index,
                    visible_tools,
                    state.tool_deduplicator_mut(),
                )
                .await?;
            prepared.push(prepared_call);
        }

        Ok(ToolBatch { calls: prepared })
    }

    /// Persist provider tool requests after the assistant message has been durably recorded.
    ///
    /// The durable transcript preserves the provider protocol order:
    /// assistant(tool_calls) -> tool requests -> tool results.
    pub(crate) async fn declare_tool_batch(
        &self,
        batch: &ToolBatch,
        publisher: &TurnEvents,
    ) -> Result<(), TurnError> {
        declare_tool_batch(publisher, batch).await
    }

    fn evaluate_permission_chain(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        plan: &ToolPlan,
    ) -> PermissionResolution {
        let ctx = PermissionContext {
            tool_name,
            tool_input,
            working_dir: Path::new(&self.turn.shared.working_dir),
            resource_accesses: plan.resources(),
            approval_mode: self.turn.shared.approval_mode,
            tool_selection: self.turn.shared.tool_selection.as_ref(),
        };
        self.turn
            .shared
            .permission_chain
            .decide(&ctx, &self.turn.shared.approval_history)
    }
}

fn compose_permission_resolution(
    mut approvals: Vec<PreparedToolApproval>,
    resolution: PermissionResolution,
) -> PreparedToolDisposition {
    match resolution {
        PermissionResolution::Allow if approvals.is_empty() => PreparedToolDisposition::Execute,
        PermissionResolution::Allow => PreparedToolDisposition::AwaitApprovals(approvals),
        PermissionResolution::Deny { reason } => {
            PreparedToolDisposition::Rejected { error: reason }
        },
        PermissionResolution::Ask { requirements } => {
            approvals.extend(
                requirements
                    .into_iter()
                    .map(|requirement| PreparedToolApproval {
                        prompt: requirement.prompt,
                        rule_key: requirement.rule_key,
                        source: ApprovalSource::Core,
                    }),
            );
            PreparedToolDisposition::AwaitApprovals(approvals)
        },
    }
}

fn tool_planning_context(
    turn: &crate::turn_context::SharedTurnContext,
    tool_call_id: &str,
    cancellation: tokio_util::sync::CancellationToken,
) -> ToolPlanningContext {
    let mut context = ToolPlanningContext::new(
        turn.session_id.clone(),
        turn.working_dir.clone(),
        Some(tool_call_id.to_owned()),
    )
    .with_cancellation(cancellation);
    if let Some(turn_id) = &turn.turn_id {
        context = context.with_turn_id(turn_id.clone());
    }
    context
}

fn reject_malformed_tool_call(
    tool_call: &StreamedToolCall,
    index: usize,
    error: &serde_json::Error,
) -> PreparedToolInvocation {
    let message = format!(
        "tool call arguments are invalid JSON: {error}. Generate a new `{}` call with valid JSON \
         matching its schema",
        tool_call.name
    );
    PreparedToolInvocation {
        index,
        call_id: tool_call.call_id.clone(),
        name: tool_call.name.clone(),
        // `Value::String` keeps the provider output serializable and exact in
        // ToolCallRequested instead of disguising a parse failure as `{}`.
        tool_input: serde_json::Value::String(tool_call.arguments.clone()),
        raw_arguments: Some(tool_call.arguments.clone()),
        plan: ToolPlan::default(),
        mode: ExecutionMode::Sequential,
        discovery_gate: None,
        disposition: PreparedToolDisposition::Rejected { error: message },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use astrcode_core::{
        tool::{
            Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolExecutionResult, ToolOrigin,
            ToolPlanningContext,
        },
        types::{new_session_id, new_turn_id},
    };
    use astrcode_extension_sdk::{
        extension::{
            ExtensionError, PreToolUseAdmission, PreToolUseRequirement,
            internal::RuntimePreToolUseContext,
        },
        runtime_ports::TurnHooks,
    };
    use astrcode_storage::in_memory::InMemoryEventStore;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        ToolRegistry,
        permission::{
            PermissionChain, PermissionContext, PermissionPolicy, PermissionRequirement,
            PolicyDecision,
        },
        session::{Session, SessionCreateParams},
        session_runtime::SessionRuntimeState,
        test_support::test_runtime_services_with_hooks,
        tool_deduplicator::ToolCallDeduplicator,
        tool_exec::TurnToolContext,
    };

    #[derive(Default)]
    struct PreparationObservations {
        admission: Mutex<Vec<serde_json::Value>>,
        planning: Mutex<Vec<serde_json::Value>>,
        permission: Mutex<Vec<serde_json::Value>>,
    }

    struct CanonicalInputHooks(Arc<PreparationObservations>);

    #[async_trait::async_trait]
    impl TurnHooks for CanonicalInputHooks {
        async fn transform_tool_input(
            &self,
            _ctx: RuntimePreToolUseContext,
        ) -> Result<serde_json::Value, ExtensionError> {
            Ok(serde_json::json!({"enabled": "true", "optional": null}))
        }

        async fn emit_pre_tool_use(
            &self,
            ctx: RuntimePreToolUseContext,
        ) -> Result<PreToolUseAdmission, ExtensionError> {
            self.0
                .admission
                .lock()
                .unwrap()
                .push(ctx.tool_input().clone());
            Ok(PreToolUseAdmission::Ask {
                requirements: vec![PreToolUseRequirement {
                    prompt: "extension approval".into(),
                    rule_key: Some("extension:probe:canonical".into()),
                }],
            })
        }
    }

    struct RecordingPlanTool(Arc<PreparationObservations>);

    #[async_trait::async_trait]
    impl Tool for RecordingPlanTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "canonicalProbe".into(),
                description: String::new(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "enabled": {"type": "boolean"},
                        "optional": {"type": "string"}
                    },
                    "required": ["enabled"]
                }),
                strict: true,
                origin: ToolOrigin::Extension,
            }
        }

        async fn plan(
            &self,
            arguments: &serde_json::Value,
            _ctx: &ToolPlanningContext,
        ) -> Result<ToolPlan, ToolError> {
            self.0.planning.lock().unwrap().push(arguments.clone());
            Ok(ToolPlan::default())
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<ToolExecutionResult, ToolError> {
            unreachable!("preparation test does not execute tools")
        }
    }

    struct RecordingPermission(Arc<PreparationObservations>);

    impl PermissionPolicy for RecordingPermission {
        fn evaluate(&self, ctx: &PermissionContext<'_>) -> PolicyDecision {
            self.0
                .permission
                .lock()
                .unwrap()
                .push(ctx.tool_input.clone());
            PolicyDecision::Allow
        }
    }

    #[test]
    fn malformed_tool_call_preserves_provider_arguments_and_is_blocked() {
        let raw = r#"{"segments":[{"emotion":"NORMAL","text">"news"}]}"#;
        let tool_call = StreamedToolCall {
            call_id: "call-invalid".into(),
            name: "interact".into(),
            arguments: raw.into(),
        };
        let error = serde_json::from_str::<serde_json::Value>(raw)
            .expect_err("fixture should be malformed JSON");

        let prepared = reject_malformed_tool_call(&tool_call, 3, &error);

        assert_eq!(prepared.index, 3);
        assert_eq!(prepared.tool_input, serde_json::Value::String(raw.into()));
        assert_eq!(prepared.raw_arguments.as_deref(), Some(raw));
        let PreparedToolDisposition::Rejected { error } = prepared.disposition else {
            panic!("malformed arguments must never be executable");
        };
        assert!(error.contains("invalid JSON"));
        assert!(error.contains("expected `:`"));
    }

    #[tokio::test]
    async fn preparation_uses_one_canonical_input_for_admission_plan_and_permission() {
        let observations = Arc::new(PreparationObservations::default());
        let hooks: Arc<dyn TurnHooks> = Arc::new(CanonicalInputHooks(Arc::clone(&observations)));
        let runtime_services = test_runtime_services_with_hooks(Arc::clone(&hooks));
        let store: Arc<dyn astrcode_storage::SessionStore> = Arc::new(InMemoryEventStore::new());
        let session_id = new_session_id();
        let runtime = Arc::new(SessionRuntimeState::new(session_id, store));
        let session = Session::create_with_params(SessionCreateParams {
            working_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            model_id: "mock-model".into(),
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
            extra_system_prompt: None,
            initial_system_prompt: None,
            runtime,
            runtime_services: Arc::clone(&runtime_services),
        })
        .await
        .unwrap();

        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(RecordingPlanTool(Arc::clone(&observations))))
            .unwrap();
        let registry = Arc::new(registry);
        let visible_tools = registry.list_definitions();
        let cancellation = CancellationToken::new();
        let state = session.read_model().await.unwrap();
        let runtime_generation = runtime_services.pin_runtime_generation();
        let mut turn = TurnToolContext::for_turn(
            &session,
            &runtime_generation,
            &state,
            new_turn_id(),
            Default::default(),
            None,
            cancellation.clone(),
        );
        turn.shared.permission_chain = Arc::new(PermissionChain::new(vec![Box::new(
            RecordingPermission(Arc::clone(&observations)),
        )]));
        let calls = ToolCalls::new(turn, Arc::clone(&registry), hooks, session, cancellation, 1);
        let tool_call = StreamedToolCall {
            call_id: "call-canonical".into(),
            name: "canonicalProbe".into(),
            arguments: serde_json::json!({"provider": "raw"}).to_string(),
        };

        let prepared = calls
            .prepare_single_tool_call(
                &tool_call,
                0,
                &visible_tools,
                &mut ToolCallDeduplicator::new(),
            )
            .await
            .unwrap();

        let canonical = serde_json::json!({"enabled": true});
        assert_eq!(prepared.tool_input, canonical);
        assert!(matches!(
            prepared.disposition,
            PreparedToolDisposition::AwaitApprovals(ref approvals)
                if approvals.len() == 1
                    && approvals[0].prompt == "extension approval"
                    && matches!(approvals[0].source, ApprovalSource::Extension)
        ));
        assert_eq!(
            observations.admission.lock().unwrap().as_slice(),
            std::slice::from_ref(&canonical)
        );
        assert_eq!(
            observations.planning.lock().unwrap().as_slice(),
            std::slice::from_ref(&canonical)
        );
        assert_eq!(
            observations.permission.lock().unwrap().as_slice(),
            std::slice::from_ref(&canonical)
        );
    }

    #[test]
    fn extension_and_core_approval_requirements_compose_while_denial_wins() {
        let extension = PreparedToolApproval {
            prompt: "extension gate".into(),
            rule_key: Some("extension:guard:dangerous".into()),
            source: ApprovalSource::Extension,
        };
        let disposition = compose_permission_resolution(
            vec![extension.clone()],
            PermissionResolution::Ask {
                requirements: vec![
                    PermissionRequirement {
                        prompt: "process access".into(),
                        rule_key: Some("process-resource:run_tests".into()),
                    },
                    PermissionRequirement {
                        prompt: "opaque side effect".into(),
                        rule_key: Some("opaque-resource:run_tests".into()),
                    },
                ],
            },
        );
        let PreparedToolDisposition::AwaitApprovals(approvals) = disposition else {
            panic!("independent approval requirements must compose");
        };
        assert_eq!(approvals.len(), 3);
        assert!(matches!(approvals[0].source, ApprovalSource::Extension));
        assert!(matches!(approvals[1].source, ApprovalSource::Core));
        assert!(matches!(approvals[2].source, ApprovalSource::Core));

        assert!(matches!(
            compose_permission_resolution(
                vec![extension],
                PermissionResolution::Deny {
                    reason: "blocked".into(),
                },
            ),
            PreparedToolDisposition::Rejected { error } if error == "blocked"
        ));
    }
}
