//! 交互式模型选择流程。

use std::sync::Arc;

use astrcode_protocol::{
    commands::UiResponseValue,
    events::{ClientNotification, UiRequestKind},
};

use super::HandlerError;
use crate::config_manager::ConfigManager;

const TARGET_REQUEST_ID: &str = "model.target";
const MODEL_REQUEST_ID: &str = "model.model";
const MAIN_OPTION: &str = "Main model";
const SMALL_OPTION: &str = "Small model";

/// 模型选择目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::handler) enum ModelTarget {
    Main,
    Small,
}

impl ModelTarget {
    fn display_name(self) -> &'static str {
        match self {
            Self::Main => "Main",
            Self::Small => "Small",
        }
    }
}

/// 当前等待客户端响应的步骤。
#[derive(Debug, Clone)]
pub(in crate::handler) enum ModelSelectionStep {
    Target,
    Model { target: ModelTarget },
}

impl ModelSelectionStep {
    pub(in crate::handler) fn request_id(&self) -> &'static str {
        match self {
            Self::Target => TARGET_REQUEST_ID,
            Self::Model { .. } => MODEL_REQUEST_ID,
        }
    }
}

pub(in crate::handler) struct ModelSelectionTransition {
    pub next_step: Option<ModelSelectionStep>,
    pub notification: ClientNotification,
}

pub(in crate::handler) struct ModelSelectionController {
    flow: ModelSelectionFlow,
    pending: Option<ModelSelectionStep>,
}

impl ModelSelectionController {
    pub fn is_idle(&self) -> bool {
        self.pending.is_none()
    }

    pub fn new(config_manager: Arc<ConfigManager>) -> Self {
        Self {
            flow: ModelSelectionFlow::new(config_manager),
            pending: None,
        }
    }

    pub async fn set_main_model(&self, model_id: &str) -> Result<ClientNotification, HandlerError> {
        let (profile, model) = parse_model_option(model_id)?;
        self.flow
            .apply_selection(ModelTarget::Main, &profile, &model)
            .await?;
        Ok(ModelSelectionFlow::success_notification(
            ModelTarget::Main,
            &profile,
            &model,
        ))
    }

    pub fn start(&mut self) -> ClientNotification {
        self.pending = Some(ModelSelectionStep::Target);
        ModelSelectionFlow::target_request()
    }

    pub async fn handle_response(
        &mut self,
        request_id: String,
        value: UiResponseValue,
    ) -> Result<ClientNotification, HandlerError> {
        let Some(step) = self.pending.take() else {
            return Err(HandlerError::InvalidRequest(format!(
                "No pending UI request: {request_id}"
            )));
        };

        if step.request_id() != request_id {
            self.pending = Some(step);
            return Err(HandlerError::InvalidRequest(format!(
                "Unexpected UI response request ID: {request_id}"
            )));
        }

        let transition = self.flow.advance(step, value).await?;
        self.pending = transition.next_step;
        Ok(transition.notification)
    }
}

struct ModelSelectionFlow {
    config_manager: Arc<ConfigManager>,
}

impl ModelSelectionFlow {
    fn new(config_manager: Arc<ConfigManager>) -> Self {
        Self { config_manager }
    }

    fn target_request() -> ClientNotification {
        select_request(
            TARGET_REQUEST_ID,
            "Select which model to change:",
            vec![MAIN_OPTION.into(), SMALL_OPTION.into()],
        )
    }

    async fn advance(
        &self,
        step: ModelSelectionStep,
        response: UiResponseValue,
    ) -> Result<ModelSelectionTransition, HandlerError> {
        match step {
            ModelSelectionStep::Target => {
                let target = parse_target(response)?;
                Ok(ModelSelectionTransition {
                    next_step: Some(ModelSelectionStep::Model { target }),
                    notification: self.model_request(target)?,
                })
            },
            ModelSelectionStep::Model { target } => {
                let selected = parse_select(response)?;
                let (profile, model) = parse_model_option(&selected)?;
                self.apply_selection(target, &profile, &model).await?;
                Ok(ModelSelectionTransition {
                    next_step: None,
                    notification: Self::success_notification(target, &profile, &model),
                })
            },
        }
    }

    async fn apply_selection(
        &self,
        target: ModelTarget,
        profile: &str,
        model: &str,
    ) -> Result<(), HandlerError> {
        let mut candidate = self.config_manager.raw_config_snapshot();
        validate_profile_model(&candidate, profile, model)?;

        match target {
            ModelTarget::Main => {
                candidate.active_profile = profile.to_string();
                candidate.active_model = model.to_string();
            },
            ModelTarget::Small => {
                candidate.active_small_profile = Some(profile.to_string());
                candidate.active_small_model = Some(model.to_string());
            },
        }

        candidate.clone().into_effective().map_err(|error| {
            HandlerError::InvalidRequest(format!("Invalid model selection: {error}"))
        })?;

        // 先应用到内存，再持久化到磁盘。
        // 如果 save 失败，内存配置已经更新，下次进程启动会回退到磁盘旧值。
        // 这种不对称比 save 成功后 apply 失败更可取：
        // 前者只是新配置没落盘（用户下次重选即可），后者会导致内存和磁盘配置不一致。
        self.config_manager
            .apply_raw_config_and_rebuild(candidate.clone())
            .map_err(|error| {
                HandlerError::InvalidRequest(format!("Failed to apply config: {error}"))
            })?;

        self.config_manager
            .config_store()
            .save(&candidate)
            .await
            .map_err(|error| {
                HandlerError::InvalidRequest(format!("Failed to write config: {error}"))
            })?;

        Ok(())
    }

    fn success_notification(target: ModelTarget, profile: &str, model: &str) -> ClientNotification {
        ClientNotification::ExtensionCommandResult {
            command_name: "model".into(),
            content: format!(
                "{} model set to {}/{}. it will work for next turn.",
                target.display_name(),
                profile,
                model
            ),
            is_error: false,
        }
    }

    fn model_request(&self, target: ModelTarget) -> Result<ClientNotification, HandlerError> {
        let config = self.config_manager.raw_config_snapshot();
        let models: Vec<String> = config
            .profiles
            .iter()
            .flat_map(|profile| {
                profile
                    .models
                    .iter()
                    .map(|model| format!("{}/{}", profile.name, model.id))
            })
            .collect();

        if models.is_empty() {
            return Err(HandlerError::InvalidRequest("No models configured".into()));
        }

        let active_model = match target {
            ModelTarget::Main => Some(format!("{}/{}", config.active_profile, config.active_model)),
            ModelTarget::Small => config
                .active_small_profile
                .as_ref()
                .zip(config.active_small_model.as_ref())
                .map(|(profile, model)| format!("{profile}/{model}")),
        };
        let options = if let Some(active) = active_model {
            if let Some(position) = models.iter().position(|model| model == &active) {
                let mut options = models;
                options.remove(position);
                options.insert(0, active);
                options
            } else {
                models
            }
        } else {
            models
        };

        Ok(select_request(
            MODEL_REQUEST_ID,
            format!(
                "Select a model for the {} model:",
                target.display_name().to_ascii_lowercase()
            ),
            options,
        ))
    }
}

fn parse_model_option(selected: &str) -> Result<(String, String), HandlerError> {
    let Some((profile, model)) = selected.split_once('/') else {
        return Err(HandlerError::InvalidRequest(format!(
            "Invalid model selection: {selected}"
        )));
    };
    if profile.is_empty() || model.is_empty() {
        return Err(HandlerError::InvalidRequest(format!(
            "Invalid model selection: {selected}"
        )));
    }
    Ok((profile.to_string(), model.to_string()))
}

fn parse_target(response: UiResponseValue) -> Result<ModelTarget, HandlerError> {
    match parse_select(response)?.as_str() {
        MAIN_OPTION => Ok(ModelTarget::Main),
        SMALL_OPTION => Ok(ModelTarget::Small),
        selected => Err(HandlerError::InvalidRequest(format!(
            "Invalid model target selection: {selected}"
        ))),
    }
}

fn parse_select(response: UiResponseValue) -> Result<String, HandlerError> {
    match response {
        UiResponseValue::Select { selected } => Ok(selected),
        _ => Err(HandlerError::InvalidRequest(
            "Expected select response".into(),
        )),
    }
}

fn validate_profile_model(
    config: &astrcode_core::config::Config,
    profile: &str,
    model: &str,
) -> Result<(), HandlerError> {
    let profile_config = config
        .profiles
        .iter()
        .find(|candidate| candidate.name == profile)
        .ok_or_else(|| HandlerError::InvalidRequest(format!("Profile not found: {profile}")))?;

    if profile_config
        .models
        .iter()
        .any(|candidate| candidate.id == model)
    {
        Ok(())
    } else {
        Err(HandlerError::InvalidRequest(format!(
            "Model not found in profile {profile}: {model}"
        )))
    }
}

fn select_request(
    request_id: &str,
    message: impl Into<String>,
    options: Vec<String>,
) -> ClientNotification {
    ClientNotification::UiRequest {
        request_id: request_id.into(),
        kind: UiRequestKind::Select,
        message: message.into(),
        options: Some(options),
        timeout_secs: 300,
    }
}
