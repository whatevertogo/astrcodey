//! 集成测试：s5r WASM 加载与 extension.json 契约。

use std::sync::Arc;

use astrcode_core::extension::{Extension, ExtensionHostServices};
use astrcode_extensions::{build_host_router, wasm_ext::WasmExtension};
use astrcode_storage::in_memory::InMemoryEventStore;

fn test_router() -> Arc<astrcode_extensions::HostRouter> {
    let store: Arc<dyn astrcode_core::storage::EventStore> = Arc::new(InMemoryEventStore::new());
    build_host_router(Arc::new(ExtensionHostServices::new(store, None)), None)
}

#[test]
fn s5r_guest_wasm_loads_via_extension_init() {
    let wasm_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("s5r-guest")
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join("s5r_guest_demo.wasm");
    if !wasm_path.exists() {
        eprintln!(
            "skip: build s5r-guest first: cargo build -p s5r-guest-demo --target wasm32-wasip1 \
             --release"
        );
        return;
    }
    let ext = WasmExtension::load(&wasm_path, 10_000_000, 64 * 1024 * 1024, test_router()).unwrap();
    assert_eq!(ext.id(), "s5r-guest-demo");
}
