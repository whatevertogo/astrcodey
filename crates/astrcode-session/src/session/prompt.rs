use astrcode_core::{prompt::SystemPromptInput, storage::SessionReadModel};
use astrcode_support::hash::hex_fingerprint;

use super::{Session, SessionError, compact::normalize_extra_system_prompt};
use crate::payload::system_prompt_configured_payload;

impl Session {
    pub async fn refresh_prompt(
        &self,
        working_dir: &str,
        extra_system_prompt: Option<&str>,
        stored_fingerprint: Option<&str>,
    ) -> Result<bool, SessionError> {
        let model_id = self.runtime.model_id();
        self.refresh_prompt_with_state(
            working_dir,
            extra_system_prompt,
            stored_fingerprint,
            None,
            &model_id,
        )
        .await
    }

    pub(crate) async fn refresh_prompt_with_state(
        &self,
        working_dir: &str,
        extra_system_prompt: Option<&str>,
        stored_fingerprint: Option<&str>,
        cached_state: Option<&SessionReadModel>,
        model_id: &str,
    ) -> Result<bool, SessionError> {
        let resolved_extra = self
            .resolve_extra_system_prompt(extra_system_prompt, cached_state)
            .await?;
        let (text, fingerprint) = self
            .build_cached_system_prompt(working_dir, model_id, resolved_extra.as_deref())
            .await?;

        if stored_fingerprint == Some(fingerprint.as_str()) {
            self.runtime.update_prompt_extra(resolved_extra);
            return Ok(false);
        }

        self.runtime.update_prompt_extra(resolved_extra.clone());
        self.emit_durable(
            None,
            system_prompt_configured_payload(text, fingerprint, resolved_extra),
        )
        .await?;
        Ok(true)
    }

    async fn resolve_extra_system_prompt(
        &self,
        extra_system_prompt: Option<&str>,
        cached_state: Option<&SessionReadModel>,
    ) -> Result<Option<String>, SessionError> {
        if extra_system_prompt.is_some() {
            return Ok(normalize_extra_system_prompt(extra_system_prompt));
        }
        if let Some(extra) = self.runtime.prompt_extra() {
            return Ok(Some(extra));
        }
        Ok(match cached_state {
            Some(state) => state.extra_system_prompt.clone(),
            None => self.read_model().await?.extra_system_prompt,
        })
    }

    pub(crate) async fn build_cached_system_prompt(
        &self,
        working_dir: &str,
        model_id: &str,
        resolved_extra: Option<&str>,
    ) -> Result<(String, String), SessionError> {
        let prompt_files =
            astrcode_context::prompt_engine::load_system_prompt_files(working_dir).await;
        let tools_with_meta = self
            .runtime
            .loaded_tool_registry()
            .list_definitions_with_prompt_metadata();
        let tools: Vec<_> = tools_with_meta.iter().map(|(def, _)| def.clone()).collect();
        let tool_prompt_metadata = tools_with_meta
            .into_iter()
            .filter_map(|(def, meta)| meta.map(|m| (def.name, m)))
            .collect();
        let ext_data = crate::session_setup::collect_extension_prompt_data(
            self.caps.extension_runner(),
            self.id.as_str(),
            working_dir,
            model_id,
            &tools,
            tool_prompt_metadata,
        )
        .await?;
        let prompt_input = SystemPromptInput {
            working_dir: working_dir.to_string(),
            os: std::env::consts::OS.into(),
            shell: super::Session::resolve_shell_name(),
            identity: prompt_files.identity,
            user_rules: prompt_files.user_rules,
            project_rules: prompt_files.project_rules,
            tools,
            tool_prompt_metadata: ext_data.merged_tool_metadata,
            extension_blocks: ext_data.extension_blocks,
            extra_instructions: resolved_extra.map(str::to_string),
        };
        let stable_fingerprint =
            astrcode_context::prompt_engine::compute_stable_fingerprint(&prompt_input);

        let (text, fingerprint) = match self.runtime.stable_prefix_cache() {
            Some((cached_text, cached_fingerprint)) if cached_fingerprint == stable_fingerprint => {
                let dynamic = astrcode_context::prompt_engine::build_dynamic_suffix(&prompt_input);
                let text = if dynamic.is_empty() {
                    cached_text
                } else {
                    format!("{}\n\n{}", cached_text.trim(), dynamic.trim())
                };
                let fingerprint = hex_fingerprint(text.as_bytes());
                (text, fingerprint)
            },
            _ => {
                let text = astrcode_context::prompt_engine::build_system_prompt(&prompt_input);
                let fingerprint = hex_fingerprint(text.as_bytes());
                let stable_prefix =
                    astrcode_context::prompt_engine::build_stable_prefix(&prompt_input);
                self.runtime
                    .store_stable_prefix_cache(stable_prefix, stable_fingerprint);
                (text, fingerprint)
            },
        };
        Ok((text, fingerprint))
    }
}
