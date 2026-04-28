//! 文件访问追踪模块。
//!
//! 记录最近访问的文件路径，用于压缩后恢复上下文时
//! 优先重新加载相关文件信息。

use std::collections::VecDeque;

/// 文件访问追踪器，使用 FIFO 策略在容量满时淘汰最旧的记录。
///
/// 当同一文件被重复访问时，会将其移到最新位置（类似 LRU 策略）。
pub struct FileAccessTracker {
    /// 按访问顺序排列的文件路径队列（最新在尾部）。
    order: VecDeque<String>,
    /// 最大追踪文件数量。
    max_tracked: usize,
}

impl FileAccessTracker {
    /// 创建一个新的文件访问追踪器。
    ///
    /// # 参数
    /// - `max_tracked`：最多追踪的文件数量，超过时淘汰最旧的记录
    pub fn new(max_tracked: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(max_tracked),
            max_tracked,
        }
    }

    /// 记录一次文件访问。
    ///
    /// 如果该文件已在追踪列表中，则将其移到最新位置；
    /// 如果追踪列表已满且是新文件，则淘汰最旧的记录。
    pub fn record(&mut self, path: &str) {
        // 如果是重复访问，先移除旧位置（移到末尾）
        if let Some(pos) = self.order.iter().position(|p| p == path) {
            self.order.remove(pos);
        } else if self.order.len() >= self.max_tracked {
            // 容量已满且是新文件，淘汰最旧的记录
            self.order.pop_front();
        }
        self.order.push_back(path.into());
    }

    /// 获取所有已追踪的文件路径，按最近访问优先排序。
    pub fn get_tracked(&self) -> Vec<String> {
        self.order.iter().rev().cloned().collect()
    }
}
