//! 服务端能力实现。
//!
//! 将已有的核心服务（LlmProvider, EventReader 等）适配为能力接口。

mod event_query;
mod llm_invoker;
mod model_info_cap;
mod session_storage;

pub use event_query::ServerEventQuery;
pub use llm_invoker::ServerLlmInvoker;
pub use model_info_cap::ServerSmallModelId;
pub use session_storage::ServerSessionStorage;
