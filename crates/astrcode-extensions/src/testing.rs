//! 测试专用的一次性装配入口。
//!
//! 跨 crate 测试用它一次性构造装好固定扩展集合的 [`ExtensionRunner`],
//! 而不是直接调用 gated 的 `register` mutation API。

use std::{sync::Arc, time::Duration};

use astrcode_extension_sdk::extension::{Extension, ExtensionError};

use crate::{host_router::HostRouter, runner::ExtensionRunner};

/// 构造一个 runner，绑定可选的 host router,并按顺序注册全部扩展。
///
/// 重复扩展 ID(`register` 返回 `Ok(false)`)视为装配错误;任一步失败都会
/// 先 shutdown 已部分装配的 runner 再返回错误。
pub async fn extension_runner_with_extensions(
    operation_timeout: Duration,
    host_router: Option<Arc<HostRouter>>,
    extensions: Vec<Arc<dyn Extension>>,
) -> Result<Arc<ExtensionRunner>, ExtensionError> {
    let runner = Arc::new(ExtensionRunner::new(operation_timeout));
    if let Some(router) = host_router {
        runner.bind_host_router(router);
    }
    for ext in extensions {
        let extension_id = ext.manifest().id().to_owned();
        let registered = match runner.register(ext).await {
            Ok(registered) => registered,
            Err(error) => {
                runner.shutdown().await;
                return Err(error);
            },
        };
        if !registered {
            runner.shutdown().await;
            return Err(ExtensionError::Internal(format!(
                "duplicate extension id during test assembly: {extension_id}"
            )));
        }
    }
    Ok(runner)
}
