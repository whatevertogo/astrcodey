//! 能力注入系统。
//!
//! Capability 是 native (in-process) 扩展获取宿主服务的唯一通道。
//! 扩展通过 [`CapabilityRegistry`] 声明和获取能力，由运行时按信任等级注入。
//!
//! # Native-only
//!
//! Capability 和 CapabilityRegistry 仅用于 in-process (native) 扩展。
//! WASM 扩展通过宿主导入函数（host imports）访问等效能力——
//! 宿主侧 `wasm_api.rs` 在实现 host import 时内部使用 CapabilityRegistry，
//! 但 WASM 扩展不直接引用此模块。
//!
//! # Newtype 包装
//!
//! 所有能力必须用 newtype 包装（如 `SessionOpsCap`），
//! 因为 `dyn Trait` 不满足 `Sized`，无法作为 `TypeId` 泛型参数。
//! newtype 是零开销方案——`cap.0.method()` 没有运行时成本。

mod event_query;
mod llm_invoker;
mod model_info_cap;
mod session_ops;
mod session_storage;
mod view_types;

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

pub use event_query::{EventQueryCap, EventQueryInner};
pub use llm_invoker::{LlmInvokerCap, LlmInvokerInner, LlmStreamEvent};
pub use model_info_cap::{SmallModelIdCap, SmallModelIdInner};
pub use session_ops::{SessionOpsCap, SessionOpsInner};
pub use session_storage::{SessionStorageCap, SessionStorageInner};
pub use view_types::{
    ConversationView, ModelInfo, PromptMessage, PromptRole, SessionSummaryView, TurnView,
};

// ─── Capability Trait ──────────────────────────────────────────────────

/// 能力标记 trait。要求 `'static` 以支持 `TypeId`。
///
/// 每个能力用一个 newtype struct 实现此 trait，例如：
///
/// ```ignore
/// pub struct SessionOpsCap(Arc<dyn SessionOpsInner>);
/// impl Capability for SessionOpsCap {}
/// ```
pub trait Capability: Send + Sync + 'static {}

// ─── CapabilityRegistry ────────────────────────────────────────────────

/// 能力注册表。存储在 `ToolExecutionContext` 和 `ExtensionCtx` 中。
///
/// 注册时以具体 newtype 类型为键；获取时按同一类型 downcast。
/// 内部存储 `Arc<dyn Any + Send + Sync>`——`Arc` 天然支持 `Clone`，
/// 取出时通过 `downcast_ref::<Arc<T>>()` 恢复具体类型。
pub struct CapabilityRegistry {
    caps: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self {
            caps: HashMap::new(),
        }
    }

    /// 注册一个能力。同一类型重复注册会覆盖。
    pub fn register<T: Capability>(&mut self, cap: Arc<T>) {
        self.caps.insert(TypeId::of::<T>(), Arc::new(cap));
    }

    /// 按类型获取能力。
    pub fn get<T: Capability>(&self) -> Option<Arc<T>> {
        self.caps
            .get(&TypeId::of::<T>())
            .and_then(|any| any.downcast_ref::<Arc<T>>())
            .cloned()
    }

    /// 移除一个能力。
    pub fn unregister<T: Capability>(&mut self) {
        self.caps.remove(&TypeId::of::<T>());
    }

    /// 注册表是否为空。
    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }
}

impl Clone for CapabilityRegistry {
    fn clone(&self) -> Self {
        Self {
            caps: self.caps.clone(),
        }
    }
}

impl std::fmt::Debug for CapabilityRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityRegistry")
            .field("count", &self.caps.len())
            .finish()
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── TrustLevel ────────────────────────────────────────────────────────

/// 扩展信任等级，决定可注入的能力范围。
///
/// | 等级 | 说明 | 可获得的能力 |
/// |------|------|-------------|
/// | `Local` | 本地进程内扩展（WASM 沙箱 / 磁盘加载） | Tier 1 (Implicit) + Tier 2 (Declared) |
/// | `Bundled` | 受信 bundled 扩展（编译进二进制） | 全部，包括 Tier 3 (Trusted-only) |
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrustLevel {
    /// 本地进程内扩展（WASM 沙箱 / 磁盘加载）。
    Local,
    /// 受信 bundled 扩展（编译进二进制）。
    Bundled,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestCap(Arc<dyn TestInner>);
    impl Capability for TestCap {}

    #[async_trait::async_trait]
    trait TestInner: Send + Sync + 'static {
        fn value(&self) -> i32;
    }

    struct TestImpl(i32);
    impl TestInner for TestImpl {
        fn value(&self) -> i32 {
            self.0
        }
    }

    #[test]
    fn register_and_get() {
        let mut reg = CapabilityRegistry::new();
        assert!(reg.get::<TestCap>().is_none());

        reg.register(Arc::new(TestCap(Arc::new(TestImpl(42)))));
        let cap = reg.get::<TestCap>().expect("should find TestCap");
        assert_eq!(cap.0.value(), 42);
    }

    #[test]
    fn overwrite_existing() {
        let mut reg = CapabilityRegistry::new();
        reg.register(Arc::new(TestCap(Arc::new(TestImpl(1)))));
        reg.register(Arc::new(TestCap(Arc::new(TestImpl(2)))));
        let cap = reg.get::<TestCap>().expect("should find TestCap");
        assert_eq!(cap.0.value(), 2);
    }

    #[test]
    fn unregister() {
        let mut reg = CapabilityRegistry::new();
        reg.register(Arc::new(TestCap(Arc::new(TestImpl(1)))));
        reg.unregister::<TestCap>();
        assert!(reg.get::<TestCap>().is_none());
    }

    #[test]
    fn clone_preserves_capabilities() {
        let mut reg = CapabilityRegistry::new();
        reg.register(Arc::new(TestCap(Arc::new(TestImpl(99)))));
        let cloned = reg.clone();
        let cap = cloned.get::<TestCap>().expect("should find TestCap in clone");
        assert_eq!(cap.0.value(), 99);
    }

    #[test]
    fn multiple_capability_types() {
        struct CapA(Arc<dyn Send + Sync + 'static>);
        impl Capability for CapA {}

        struct CapB(Arc<dyn Send + Sync + 'static>);
        impl Capability for CapB {}

        let mut reg = CapabilityRegistry::new();
        reg.register(Arc::new(CapA(Arc::new(TestImpl(1)))));
        reg.register(Arc::new(CapB(Arc::new(TestImpl(2)))));

        assert!(reg.get::<CapA>().is_some());
        assert!(reg.get::<CapB>().is_some());
    }
}
