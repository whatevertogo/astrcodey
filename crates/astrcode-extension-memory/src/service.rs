use std::sync::Arc;

use astrcode_extension_sdk::{
    extension::{ExtensionError, ServiceContext, ServiceHandler},
    host::HostError,
    wire::WireErrorCode,
};
use serde_json::{Value, json};

use crate::{
    handlers::{ListArgs, list_memories},
    store::MemoryStorePool,
};

pub(crate) const MEMORY_LIST_SERVICE: &str = "memory.entries.list@1";
pub(crate) struct MemoryListService {
    pub store_pool: Arc<MemoryStorePool>,
}
#[async_trait::async_trait]
impl ServiceHandler for MemoryListService {
    async fn invoke(&self, context: ServiceContext, input: Value) -> Result<Value, HostError> {
        let working_dir = context.working_dir().ok_or_else(|| {
            HostError::new(
                WireErrorCode::ContextUnavailable,
                "memory listing requires a workspace",
            )
        })?;
        if !input
            .as_object()
            .is_some_and(|fields| fields.keys().all(|key| key == "query" || key == "limit"))
        {
            return Err(HostError::new(
                WireErrorCode::InvalidInput,
                "expected query and limit only",
            ));
        }
        let args: ListArgs = serde_json::from_value(input)
            .map_err(|e| HostError::new(WireErrorCode::InvalidInput, e.to_string()))?;
        let entries = list_memories(
            self.store_pool.clone(),
            working_dir.to_string_lossy().into_owned(),
            args,
        )
        .await
        .map_err(|e| match e {
            ExtensionError::Host(error) => error,
            other => HostError::new(WireErrorCode::DispatchFailed, other.to_string()),
        })?;
        Ok(json!({ "entries": entries }))
    }
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::{
        extension::{ExtensionCall, internal::service_context},
        testing::ToolContextBuilder,
    };

    use super::*;
    #[tokio::test]
    async fn memory_service_uses_host_workspace_and_rejects_input_paths() {
        let root = tempfile::tempdir().unwrap();
        let pool = Arc::new(MemoryStorePool::new());
        pool.set_root(root.path().into()).unwrap();
        pool.get_scoped("/project-a")
            .unwrap()
            .project
            .append("general", "alpha project")
            .unwrap();
        pool.get_scoped("/project-b")
            .unwrap()
            .project
            .append("general", "beta project")
            .unwrap();
        let service = MemoryListService { store_pool: pool };
        let tool = ToolContextBuilder::new("astrcode.memory", "memory_list").build();
        let context = || {
            service_context(
                tool.call().clone(),
                "client".into(),
                Some("/project-a".into()),
                None,
            )
        };
        let output = service
            .invoke(context(), json!({"limit":10}))
            .await
            .unwrap();
        assert!(output.to_string().contains("alpha project"));
        assert!(!output.to_string().contains("beta project"));
        assert_eq!(
            service
                .invoke(context(), json!({"working_dir":"/project-b"}))
                .await
                .unwrap_err()
                .code,
            WireErrorCode::InvalidInput.as_str()
        );
        let unscoped = service_context(tool.call().clone(), "client".into(), None, None);
        assert_eq!(
            service.invoke(unscoped, json!({})).await.unwrap_err().code,
            WireErrorCode::ContextUnavailable.as_str()
        );
    }
}
