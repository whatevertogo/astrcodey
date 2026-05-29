//! Session 工具表与运行时初始化。

use std::sync::Arc;

use astrcode_tools::registry::ToolRegistry;

use super::{Session, SessionError};

impl Session {
    pub async fn refresh_tools(&self, working_dir: &str) -> Arc<ToolRegistry> {
        let timeout = self.caps.read_effective().agent.shell_timeout_secs;
        let tool_policy = self.runtime.child_tool_policy();
        let registry = crate::session_setup::build_tool_registry_snapshot(
            self.caps.extension_runner(),
            working_dir,
            timeout,
            tool_policy.as_ref(),
        )
        .await;
        let registry = Arc::new(registry);
        self.runtime.install_tool_registry(Arc::clone(&registry));
        registry
    }

    pub async fn initialize_runtime(&self, working_dir: &str) -> Result<(), SessionError> {
        self.refresh_tools(working_dir).await;
        self.refresh_prompt(working_dir, None, None).await?;
        Ok(())
    }

    pub async fn ensure_runtime_ready(&self) -> Result<(), SessionError> {
        let state = self.read_model().await?;
        if self
            .runtime
            .loaded_tool_registry()
            .list_definitions()
            .is_empty()
        {
            self.refresh_tools(&state.working_dir).await;
        }
        if state.system_prompt.is_none() {
            self.refresh_prompt(&state.working_dir, None, None).await?;
        }
        Ok(())
    }
}
