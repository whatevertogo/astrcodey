use std::time::Duration;

use anyhow::Result;
use astrcode_client::{
    ClientTransport, CompactSessionRequest, ConversationBannerErrorCodeDto,
    ConversationErrorEnvelopeDto, ConversationStreamItem, CreateSessionRequest,
    ExecutionControlDto, PromptRequest, PromptSkillInvocation, SaveActiveSelectionRequest,
    SwitchModeRequest,
};

use super::{
    Action, AppController, SnapshotLoadedAction, filter_model_options, filter_resume_sessions,
    model_query_from_input, required_working_dir, resume_query_from_input,
    slash_candidates_with_local_commands, slash_query_from_input,
};
use crate::{
    command::{Command, InputAction, PaletteAction, classify_input, filter_slash_candidates},
    state::{PaletteState, StreamRenderMode},
};

impl<T> AppController<T>
where
    T: ClientTransport + 'static,
{
    fn dispatch_async<F>(&self, operation: F)
    where
        F: std::future::Future<Output = Option<Action>> + Send + 'static,
    {
        let sender = self.actions_tx.clone();
        tokio::spawn(async move {
            if let Some(action) = operation.await {
                let _ = sender.send(action);
            }
        });
    }

    pub(super) async fn submit_current_input(&mut self) {
        let input = self.state.take_input();
        match classify_input(input, &self.state.conversation.slash_candidates) {
            InputAction::Empty => {},
            InputAction::SubmitPrompt { text } => {
                self.submit_prompt_request(text, None).await;
            },
            InputAction::RunCommand(command) => {
                self.execute_command(command).await;
            },
        }
    }

    pub(super) async fn execute_palette_action(&mut self, action: PaletteAction) -> Result<()> {
        match action {
            PaletteAction::SwitchSession { session_id } => {
                self.state.close_palette();
                self.begin_session_hydration(session_id).await;
            },
            PaletteAction::SelectModel {
                profile_name,
                model,
            } => {
                self.state.close_palette();
                self.apply_model_selection(profile_name, model).await;
            },
            PaletteAction::ReplaceInput { text } => {
                self.state.close_palette();
                self.state.replace_input(text);
            },
            PaletteAction::RunCommand(command) => {
                self.state.close_palette();
                self.execute_command(command).await;
            },
        }
        Ok(())
    }

    pub(super) async fn execute_command(&mut self, command: Command) {
        match command {
            Command::New => {
                let working_dir = match required_working_dir(&self.state) {
                    Ok(path) => path.display().to_string(),
                    Err(error) => {
                        self.state.set_error_status(error.to_string());
                        return;
                    },
                };
                let client = self.client.clone();
                self.state.set_status("creating session");
                self.dispatch_async(async move {
                    let result = client
                        .create_session(CreateSessionRequest { working_dir })
                        .await;
                    Some(Action::SessionCreated(result))
                });
            },
            Command::Resume { query } => {
                let query = query.unwrap_or_default();
                let items =
                    filter_resume_sessions(&self.state.conversation.sessions, query.as_str());
                self.state.replace_input(if query.is_empty() {
                    "/resume".to_string()
                } else {
                    format!("/resume {query}")
                });
                self.state.set_resume_query(query, items);
                self.refresh_sessions().await;
            },
            Command::Model { query } => {
                let query = query.unwrap_or_default();
                let items = filter_model_options(&self.state.shell.model_options, query.as_str());
                self.state.replace_input(if query.is_empty() {
                    "/model".to_string()
                } else {
                    format!("/model {query}")
                });
                self.state.set_model_query(query.clone(), items);
                self.refresh_model_options(query).await;
            },
            Command::Mode { query } => {
                let query = query.unwrap_or_default();
                if query.is_empty() {
                    let Some(session_id) = self.state.conversation.active_session_id.clone() else {
                        let available = self
                            .state
                            .shell
                            .available_modes
                            .iter()
                            .map(|mode| mode.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        if available.is_empty() {
                            self.state.set_error_status("no active session");
                        } else {
                            self.state.set_error_status(format!(
                                "no active session · available modes: {available}"
                            ));
                        }
                        return;
                    };
                    let client = self.client.clone();
                    self.state.set_status("loading mode state");
                    self.dispatch_async(async move {
                        let result = client.get_session_mode(&session_id).await;
                        Some(Action::SessionModeLoaded { session_id, result })
                    });
                    return;
                }

                let Some(session_id) = self.state.conversation.active_session_id.clone() else {
                    self.state.set_error_status("no active session");
                    return;
                };
                let requested_mode_id = query;
                let client = self.client.clone();
                self.state
                    .set_status(format!("switching mode to {requested_mode_id}"));
                self.dispatch_async(async move {
                    let result = client
                        .switch_mode(
                            &session_id,
                            SwitchModeRequest {
                                mode_id: requested_mode_id.clone(),
                            },
                        )
                        .await;
                    Some(Action::ModeSwitched {
                        session_id,
                        requested_mode_id,
                        result,
                    })
                });
            },
            Command::Compact => {
                let Some(session_id) = self.state.conversation.active_session_id.clone() else {
                    self.state.set_error_status("no active session");
                    return;
                };
                if self
                    .state
                    .conversation
                    .control
                    .as_ref()
                    .is_some_and(|control| !control.can_request_compact)
                {
                    self.state
                        .set_error_status("compact is not available right now");
                    return;
                }
                let client = self.client.clone();
                self.state.set_status("requesting compact");
                self.dispatch_async(async move {
                    let result = client
                        .request_compact(
                            &session_id,
                            CompactSessionRequest {
                                control: Some(ExecutionControlDto {
                                    manual_compact: Some(true),
                                }),
                                instructions: None,
                            },
                        )
                        .await;
                    Some(Action::CompactRequested { session_id, result })
                });
            },
            Command::SkillInvoke { skill_id, prompt } => {
                let text = prompt.clone().unwrap_or_default();
                self.submit_prompt_request(
                    text,
                    Some(PromptSkillInvocation {
                        skill_id,
                        user_prompt: prompt,
                    }),
                )
                .await;
            },
            Command::Unknown { raw } => {
                self.state
                    .set_error_status(format!("unknown slash command: {raw}"));
            },
        }
    }

    pub(super) async fn begin_session_hydration(&mut self, session_id: String) {
        self.pending_session_id = Some(session_id.clone());
        if let Some(stream_task) = self.stream_task.take() {
            stream_task.abort();
        }
        self.stream_pacer.reset();
        self.state
            .set_status(format!("hydrating session {}", session_id));
        let client = self.client.clone();
        self.dispatch_async(async move {
            let result = client.fetch_conversation_snapshot(&session_id, None).await;
            Some(Action::SnapshotLoaded(Box::new(SnapshotLoadedAction {
                session_id,
                result,
            })))
        });
    }

    pub(super) async fn open_stream_for_active_session(&mut self) {
        if let Some(stream_task) = self.stream_task.take() {
            stream_task.abort();
        }
        self.stream_pacer.reset();
        let Some(session_id) = self.state.conversation.active_session_id.clone() else {
            return;
        };
        let cursor = self.state.conversation.cursor.clone();
        match self
            .client
            .stream_conversation(&session_id, cursor.as_ref(), None)
            .await
        {
            Ok(mut stream) => {
                let sender = self.actions_tx.clone();
                let pacer = self.stream_pacer.clone();
                self.stream_task = Some(tokio::spawn(async move {
                    while let Ok(Some(item)) = stream.recv().await {
                        let mut items = vec![item];
                        if matches!(pacer.mode(), StreamRenderMode::CatchUp) {
                            while items.len() < 6 {
                                match tokio::time::timeout(Duration::from_millis(2), stream.recv())
                                    .await
                                {
                                    Ok(Ok(Some(next))) => items.push(next),
                                    _ => break,
                                }
                            }
                        }
                        pacer.note_enqueued(items.len());
                        if sender
                            .send(Action::StreamBatch {
                                session_id: session_id.clone(),
                                items,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            },
            Err(error) => self.apply_banner_error(error),
        }
    }

    pub(super) async fn refresh_sessions(&self) {
        let client = self.client.clone();
        self.dispatch_async(async move {
            let result = client.list_sessions().await;
            Some(Action::SessionsRefreshed(result))
        });
    }

    pub(super) async fn refresh_current_model(&self) {
        let client = self.client.clone();
        self.dispatch_async(async move {
            let result = client.get_current_model().await;
            Some(Action::CurrentModelLoaded(result))
        });
    }

    pub(super) async fn refresh_modes(&self) {
        let client = self.client.clone();
        self.dispatch_async(async move {
            let result = client.list_modes().await;
            Some(Action::ModesLoaded(result))
        });
    }

    pub(super) async fn refresh_model_options(&self, query: String) {
        let client = self.client.clone();
        self.dispatch_async(async move {
            let result = client.list_models().await;
            Some(Action::ModelOptionsLoaded { query, result })
        });
    }

    pub(super) async fn open_slash_palette(&mut self, query: String) {
        if !self
            .state
            .interaction
            .composer
            .as_str()
            .trim_start()
            .starts_with('/')
        {
            self.state.replace_input("/".to_string());
        }
        let candidates = slash_candidates_with_local_commands(
            &self.state.conversation.slash_candidates,
            &self.state.shell.available_modes,
            query.as_str(),
        );
        let items = if query.trim().is_empty() {
            candidates
        } else {
            filter_slash_candidates(&candidates, &query)
        };
        self.state.set_slash_query(query.clone(), items);
        self.refresh_slash_candidates(query).await;
    }

    pub(super) async fn refresh_slash_candidates(&self, query: String) {
        let Some(session_id) = self.state.conversation.active_session_id.clone() else {
            return;
        };
        let client = self.client.clone();
        self.dispatch_async(async move {
            let result = client
                .list_conversation_slash_candidates(&session_id, Some(query.as_str()))
                .await;
            Some(Action::SlashCandidatesLoaded { query, result })
        });
    }

    pub(super) async fn refresh_palette_query(&mut self) {
        match &self.state.interaction.palette {
            PaletteState::Resume(_) => {
                if !self
                    .state
                    .interaction
                    .composer
                    .as_str()
                    .trim_start()
                    .starts_with("/resume")
                {
                    self.state.close_palette();
                    return;
                }
                let query = resume_query_from_input(self.state.interaction.composer.as_str());
                let items =
                    filter_resume_sessions(&self.state.conversation.sessions, query.as_str());
                self.state.set_resume_query(query, items);
            },
            PaletteState::Slash(_) => {
                if !self
                    .state
                    .interaction
                    .composer
                    .as_str()
                    .trim_start()
                    .starts_with('/')
                {
                    self.state.close_palette();
                    return;
                }
                let query = self.slash_query_for_current_input();
                let candidates = slash_candidates_with_local_commands(
                    &self.state.conversation.slash_candidates,
                    &self.state.shell.available_modes,
                    query.as_str(),
                );
                self.state
                    .set_slash_query(query.clone(), filter_slash_candidates(&candidates, &query));
                self.refresh_slash_candidates(query).await;
            },
            PaletteState::Model(_) => {
                if !self
                    .state
                    .interaction
                    .composer
                    .as_str()
                    .trim_start()
                    .starts_with("/model")
                {
                    self.state.close_palette();
                    return;
                }
                let query = model_query_from_input(self.state.interaction.composer.as_str());
                self.state.set_model_query(
                    query.clone(),
                    filter_model_options(&self.state.shell.model_options, query.as_str()),
                );
                self.refresh_model_options(query).await;
            },
            PaletteState::Closed => {},
        }
    }

    pub(super) fn refresh_resume_palette(&mut self) {
        let PaletteState::Resume(resume) = &self.state.interaction.palette else {
            return;
        };
        let items =
            filter_resume_sessions(&self.state.conversation.sessions, resume.query.as_str());
        self.state.set_resume_query(resume.query.clone(), items);
    }

    pub(super) async fn apply_stream_event(
        &mut self,
        session_id: &str,
        item: ConversationStreamItem,
    ) {
        match item {
            ConversationStreamItem::Delta(envelope) => {
                self.state.clear_banner();
                self.state.apply_stream_envelope(*envelope);
            },
            ConversationStreamItem::RehydrateRequired(error) => {
                self.state.set_banner_error(error);
                self.begin_session_hydration(session_id.to_string()).await;
            },
            ConversationStreamItem::Lagged { skipped } => {
                self.state.set_banner_error(ConversationErrorEnvelopeDto {
                    code: ConversationBannerErrorCodeDto::CursorExpired,
                    message: format!("stream lagged by {skipped} events, rehydrating"),
                    rehydrate_required: true,
                    details: None,
                });
                self.begin_session_hydration(session_id.to_string()).await;
            },
            ConversationStreamItem::Disconnected { message } => {
                self.state.set_banner_error(ConversationErrorEnvelopeDto {
                    code: ConversationBannerErrorCodeDto::StreamDisconnected,
                    message,
                    rehydrate_required: false,
                    details: None,
                });
            },
        }
    }

    pub(super) fn slash_query_for_current_input(&self) -> String {
        slash_query_from_input(self.state.interaction.composer.as_str())
    }

    async fn apply_model_selection(&mut self, profile_name: String, model: String) {
        self.state.set_status(format!("switching model to {model}"));
        let client = self.client.clone();
        self.dispatch_async(async move {
            let result = client
                .save_active_selection(SaveActiveSelectionRequest {
                    active_profile: profile_name.clone(),
                    active_model: model.clone(),
                })
                .await;
            Some(Action::ModelSelectionSaved {
                profile_name,
                model,
                result,
            })
        });
    }

    async fn submit_prompt_request(
        &mut self,
        text: String,
        skill_invocation: Option<PromptSkillInvocation>,
    ) {
        let Some(session_id) = self.state.conversation.active_session_id.clone() else {
            self.state.set_error_status("no active session");
            return;
        };
        self.state.set_status("submitting prompt");
        let client = self.client.clone();
        self.dispatch_async(async move {
            let result = client
                .submit_prompt(
                    &session_id,
                    PromptRequest {
                        text,
                        skill_invocation,
                    },
                )
                .await;
            Some(Action::PromptSubmitted { session_id, result })
        });
    }
}
