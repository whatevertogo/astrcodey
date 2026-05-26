//! Model 列表 / 当前激活 / 连通性测试路由。

use std::sync::Arc;

use astrcode_protocol::http::{
    AvailableModelDto, CurrentModelResponseDto, ModelListResponseDto, ModelTestResponseDto,
};
use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};

use super::super::HttpState;

pub(in crate::http) async fn get_current_model(State(state): State<HttpState>) -> Response {
    let raw = state.runtime.config_manager().raw_config_snapshot();
    let eff = state.runtime.config_manager().read_effective();
    Json(CurrentModelResponseDto {
        profile_name: raw.active_profile.clone(),
        model_id: eff.llm.model_id.clone(),
        provider_kind: eff.llm.provider_kind.clone(),
    })
    .into_response()
}

pub(in crate::http) async fn list_models(State(state): State<HttpState>) -> Response {
    let raw = state.runtime.config_manager().raw_config_snapshot();
    let models: Vec<AvailableModelDto> = raw
        .profiles
        .iter()
        .flat_map(|p| {
            p.models.iter().map(|m| AvailableModelDto {
                profile_name: p.name.clone(),
                model_id: m.id.clone(),
                provider_kind: p.provider_kind.clone(),
            })
        })
        .collect();
    Json(ModelListResponseDto { models }).into_response()
}

async fn run_model_test(
    provider: &Arc<dyn astrcode_core::llm::LlmProvider>,
) -> ModelTestResponseDto {
    let start = std::time::Instant::now();
    match provider
        .generate(vec![astrcode_core::llm::LlmMessage::user("Hi")], vec![])
        .await
    {
        Ok(mut rx) => {
            while rx.recv().await.is_some() {}
            ModelTestResponseDto {
                success: true,
                message: format!("ok ({}ms)", start.elapsed().as_millis()),
            }
        },
        Err(error) => ModelTestResponseDto {
            success: false,
            message: error.to_string(),
        },
    }
}

pub(in crate::http) async fn test_model(State(state): State<HttpState>) -> Response {
    Json(run_model_test(&state.runtime.config_manager().read_llm_provider()).await).into_response()
}

pub(in crate::http) async fn get_small_current_model(State(state): State<HttpState>) -> Response {
    let eff = state.runtime.config_manager().read_effective();
    let raw = state.runtime.config_manager().raw_config_snapshot();
    let (profile_name, model) = match (&raw.active_small_profile, &raw.active_small_model) {
        (Some(p), Some(_)) => (p.clone(), &eff.small_llm),
        _ => (raw.active_profile.clone(), &eff.small_llm),
    };
    Json(CurrentModelResponseDto {
        profile_name,
        model_id: model.model_id.clone(),
        provider_kind: model.provider_kind.clone(),
    })
    .into_response()
}

pub(in crate::http) async fn test_small_model(State(state): State<HttpState>) -> Response {
    Json(run_model_test(&state.runtime.config_manager().read_small_llm_provider()).await)
        .into_response()
}
