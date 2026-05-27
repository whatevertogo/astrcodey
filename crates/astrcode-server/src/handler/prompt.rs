//! Prompt 提交、注入与斜杠命令拦截。

use astrcode_core::{types::SessionId, user_prompt::UserPromptParts};
use astrcode_protocol::{
    commands::Attachment,
    events::{ClientNotification, SessionControlStateDto},
};
use astrcode_support::prompt_attachments::{self, PromptAttachmentError};

use super::{CommandHandler, HandlerError, PromptSubmission, slash};
use crate::{
    http::control_from_phase,
    turn_scheduler::{SubmitOutcome, UserInputOutcome},
};

impl CommandHandler {
    pub(super) async fn submit_prompt(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
    ) -> Result<(), HandlerError> {
        let input = user_prompt_from_wire(text, attachments)?;
        let sid = self.ensure_session().await?;
        match self.accept_user_input_for_session(sid, input).await {
            Ok(_) => Ok(()),
            Err(error) => {
                self.send_error(slash::command_error_code(&error), &error.to_string());
                Err(error)
            },
        }
    }

    pub(super) async fn submit_prompt_step(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
    ) -> Result<(), HandlerError> {
        let input = user_prompt_from_wire(text, attachments)?;
        let sid = self.ensure_session().await?;
        match self.scheduler.submit_prompt_step(sid.clone(), input).await {
            Ok(SubmitOutcome::Injected)
            | Ok(SubmitOutcome::Started { .. })
            | Ok(SubmitOutcome::Queued) => {
                self.broadcast_session_control(&sid).await;
                Ok(())
            },
            Err(error) => {
                let handler_error = HandlerError::from(error);
                self.send_error(
                    slash::command_error_code(&handler_error),
                    &handler_error.to_string(),
                );
                Err(handler_error)
            },
        }
    }

    pub(super) async fn inject_mid_turn_message(
        &mut self,
        text: String,
    ) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        self.inject_mid_turn_message_for_session(&sid, UserPromptParts::text_only(text))
            .await
    }

    pub(super) async fn inject_mid_turn_message_for_session(
        &self,
        sid: &SessionId,
        input: UserPromptParts,
    ) -> Result<(), HandlerError> {
        self.scheduler
            .inject(sid, input)
            .await
            .map_err(HandlerError::from)?;
        Ok(())
    }

    /// 斜杠解析 + [`TurnScheduler::accept_user_input`]，映射为 handler 交付语义。
    pub(in crate::handler) async fn accept_user_input_for_session(
        &mut self,
        sid: SessionId,
        input: UserPromptParts,
    ) -> Result<PromptSubmission, HandlerError> {
        let visible_text = if input.text.trim().starts_with('/') {
            input.text.clone()
        } else {
            input.display_text()
        };
        if let Some(command) = slash::parse_slash_command(&visible_text) {
            match self
                .execute_slash_command_for_session(sid.clone(), command, visible_text.clone())
                .await
            {
                Err(HandlerError::UnknownCommand(_)) => {},
                other => return other,
            }
        }

        let submission = match self.scheduler.accept_user_input(sid.clone(), input).await {
            Ok(UserInputOutcome::Queued) => PromptSubmission::Handled {
                message: "queued for next turn".into(),
            },
            Ok(UserInputOutcome::Started { turn_id }) => PromptSubmission::Accepted { turn_id },
            Err(error) => return Err(HandlerError::from(error)),
        };

        if matches!(
            &submission,
            PromptSubmission::Handled { message } if message == "queued for next turn"
        ) {
            self.broadcast_session_control(&sid).await;
        }
        Ok(submission)
    }

    pub async fn command_infos_for_session(
        &self,
        sid: &SessionId,
    ) -> Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError> {
        let state = self
            .runtime
            .event_store()
            .session_read_model(sid)
            .await
            .map_err(|e| HandlerError::SessionManager(e.into()))?;
        Ok(self.command_infos_for_working_dir(&state.working_dir).await)
    }

    pub(in crate::handler) async fn broadcast_session_control(&self, sid: &SessionId) {
        let Ok(state) = self.runtime.event_store().session_read_model(sid).await else {
            return;
        };
        let control = control_from_phase(state.phase, !state.messages.is_empty());
        self.event_bus
            .send_notification(ClientNotification::SessionControlUpdated {
                session_id: sid.to_string(),
                control: SessionControlStateDto::from_http(&control),
            });
    }
}

pub(crate) fn user_prompt_from_wire(
    text: String,
    attachments: Vec<Attachment>,
) -> Result<UserPromptParts, HandlerError> {
    let attachments: Vec<prompt_attachments::PromptAttachment> = attachments
        .into_iter()
        .map(|attachment| prompt_attachments::PromptAttachment {
            filename: attachment.filename,
            content: attachment.content,
            media_type: attachment.media_type,
        })
        .collect();
    prompt_attachments::build_user_prompt(text, &attachments).map_err(|error| match error {
        PromptAttachmentError::Empty => {
            HandlerError::InvalidRequest("prompt must include text or at least one image".into())
        },
        PromptAttachmentError::UnsupportedAttachment {
            filename,
            media_type,
        } => HandlerError::InvalidRequest(format!(
            "unsupported attachment `{filename}` ({media_type})"
        )),
        PromptAttachmentError::Image(image_error) => {
            HandlerError::InvalidRequest(format!("invalid image attachment: {image_error}"))
        },
    })
}

pub(crate) fn user_prompt_from_http(
    text: String,
    attachments: Vec<astrcode_protocol::http::PromptAttachmentDto>,
) -> Result<UserPromptParts, HandlerError> {
    let wire: Vec<Attachment> = attachments.into_iter().map(Into::into).collect();
    user_prompt_from_wire(text, wire)
}
