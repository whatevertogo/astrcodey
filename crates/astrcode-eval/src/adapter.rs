//! Benchmark 适配器接口。
//!
//! 定义将外部 benchmark 数据源（如 SWE-bench）转换为 EvalCase 的 trait。
//! 本模块仅定义接口，具体适配器在外部仓库实现。

use std::path::Path;

use crate::{EvalError, case::EvalCase};

/// 外部 benchmark 适配器。
///
/// 实现此 trait 可将任意格式的 benchmark 数据转换为 astrcode eval case。
pub trait BenchmarkAdapter: Send + Sync {
    /// 适配器名称。
    fn name(&self) -> &str;

    /// 从数据源目录加载并转换为 eval cases。
    fn load_cases(&self, source: &Path) -> Result<Vec<EvalCase>, EvalError>;
}
