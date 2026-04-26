//! 模型信息相关 DTO。
//!
//! 当前模型信息和模型选项已经是 `core::config::ModelSelection` 的共享语义，
//! 协议层直接复用 canonical owner。

pub use astrcode_core::{
    CurrentModelSelection as CurrentModelInfoDto, ModelOption as ModelOptionDto,
};
