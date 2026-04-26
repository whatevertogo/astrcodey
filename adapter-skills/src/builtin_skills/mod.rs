//! 内置 skill 运行时加载器。
//!
//! 将编译期由 `build.rs` 打包的 builtin skill 定义转换为运行时 [`SkillSpec`]。
//! 与用户/project skill 不同，builtin skill 的解析失败会导致 panic（fail-fast），
//! 因为它们是 crate 内部维护的，不应出现格式错误。
//!
//! # 资产物化
//!
//! Builtin skill 的资产文件（如 `scripts/`、`references/`）在首次加载时
//! 物化到 `~/.astrcode/runtime/builtin-skills/<skill-id>/` 目录，
//! 以便运行时作为可执行/可读资源访问，而非仅存在于 prompt 文本中。

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use astrcode_support::hostpaths::resolve_home_dir;
use log::warn;

use crate::{
    SKILL_FILE_NAME, SkillSource, SkillSpec, collect_asset_files, is_valid_skill_name,
    parse_skill_md,
};

/// 编译期生成的 skill 定义结构。
///
/// 由 `build.rs` 生成的 `bundled_skills.generated.rs` 中使用。
struct BundledSkillDefinition {
    /// Skill 的唯一标识符（kebab-case 文件夹名）。
    id: &'static str,
    /// Skill 的所有资产文件（包括 SKILL.md 和 references/、scripts/ 等）。
    assets: &'static [BundledSkillAsset],
}

/// 编译期嵌入的单个资产文件。
struct BundledSkillAsset {
    /// 相对于 skill 根目录的路径。
    relative_path: &'static str,
    /// 文件内容（通过 `include_str!` 在编译期嵌入）。
    content: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/bundled_skills.generated.rs"));

/// 加载所有内置 skill。
///
/// 将编译期嵌入的 skill 定义解析为 [`SkillSpec`]，并物化资产文件到运行时目录。
///
/// # Fail-Fast
///
/// Builtin skill 由 crate 内部维护，因此解析失败时直接 panic，
/// 而非像用户 skill 那样静默跳过。这样可以在开发期就发现问题。
pub fn load_builtin_skills() -> Vec<SkillSpec> {
    BUNDLED_SKILLS
        .iter()
        .map(|definition| {
            // Bundled skills are authored inside this crate, so malformed
            // markdown should fail fast instead of silently disappearing.
            let skill_markdown = definition
                .assets
                .iter()
                .find(|asset| asset.relative_path == SKILL_FILE_NAME)
                .unwrap_or_else(|| panic!("bundled skill '{}' is missing SKILL.md", definition.id))
                .content;
            let mut skill = parse_skill_md(skill_markdown, definition.id, SkillSource::Builtin)
                .unwrap_or_else(|| panic!("invalid bundled skill '{}'", definition.id));
            assert_valid_builtin_skill_identity(definition.id, &skill);
            skill.allowed_tools = bundled_skill_allowed_tools(definition.id)
                .iter()
                .map(|tool| (*tool).to_string())
                .collect();
            if let Some(skill_root) = materialize_builtin_skill_assets(definition) {
                skill.asset_files = collect_asset_files(&skill_root);
                skill.skill_root = Some(skill_root.to_string_lossy().into_owned());
            }
            skill
        })
        .collect()
}

/// 获取指定 builtin skill 允许调用的工具列表。
///
/// Skill 合约在 markdown 中保持 Claude 兼容，
/// 而实际的工具边界在此处由 runtime 记录。
fn bundled_skill_allowed_tools(skill_id: &str) -> &'static [&'static str] {
    match skill_id {
        // The skill contract stays Claude-compatible in markdown, while runtime
        // records the actual tool boundary here for the Skill capability output.
        "git-commit" => &["shell", "readFile", "grep", "findFiles"],
        _ => &[],
    }
}

/// 校验 builtin skill 的身份一致性。
///
/// 确保 frontmatter 中的 name 与期望的 skill id 一致，
/// 且名称格式合法。此函数在开发期捕获配置错误。
fn assert_valid_builtin_skill_identity(expected_id: &str, skill: &SkillSpec) {
    assert_eq!(
        skill.name, expected_id,
        "bundled skill frontmatter name must match its kebab-case folder name"
    );
    assert!(
        is_valid_skill_name(&skill.name),
        "bundled skill names may only contain lowercase ascii letters, digits, and hyphens"
    );
}

/// 将 builtin skill 的资产物化到运行时目录。
///
/// 将编译期嵌入的资产文件写入 `~/.astrcode/runtime/builtin-skills/<skill-id>/`，
/// 使得 `scripts/` 和 `references/` 等资源在运行时可执行/可读。
///
/// # 增量写入
///
/// [`write_asset_if_changed`] 仅在内容变化时写入，避免不必要的 I/O。
fn materialize_builtin_skill_assets(definition: &BundledSkillDefinition) -> Option<PathBuf> {
    let home_dir = match resolve_home_dir() {
        Ok(home_dir) => home_dir,
        Err(error) => {
            warn!(
                "failed to resolve home directory for builtin skill '{}': {}",
                definition.id, error
            );
            return None;
        },
    };

    let skill_root = home_dir
        .join(".astrcode")
        .join("runtime")
        .join("builtin-skills")
        .join(definition.id);

    for asset in definition.assets {
        if !is_safe_relative_asset_path(asset.relative_path) {
            warn!(
                "skipping unsafe builtin skill asset '{}' for '{}'",
                asset.relative_path, definition.id
            );
            return None;
        }

        let asset_path = skill_root.join(
            asset
                .relative_path
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        if let Some(parent) = asset_path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                warn!(
                    "failed to create builtin skill directory '{}' for '{}': {}",
                    parent.display(),
                    definition.id,
                    error
                );
                return None;
            }
        }

        // Materialize the bundled tree so `scripts/` and `references/` stay
        // executable/readable at runtime instead of living only in prompt text.
        if let Err(error) = write_asset_if_changed(&asset_path, asset.content) {
            warn!(
                "failed to materialize builtin skill asset '{}' for '{}': {}",
                asset.relative_path, definition.id, error
            );
            return None;
        }
    }

    Some(skill_root)
}

/// 检查资产路径是否安全（不包含路径穿越）。
///
/// 拒绝绝对路径和 `..` 组件，防止恶意 skill 文件写入到非预期位置。
fn is_safe_relative_asset_path(relative_path: &str) -> bool {
    let path = Path::new(relative_path);
    !path.is_absolute()
        && path.components().all(|component| {
            matches!(component, Component::Normal(_)) || matches!(component, Component::CurDir)
        })
}

/// 仅在内容变化时写入文件。
///
/// 先读取现有文件内容对比，相同则跳过写入。
/// 这减少了不必要的磁盘 I/O，特别是在多次启动场景中。
fn write_asset_if_changed(path: &Path, content: &str) -> std::io::Result<()> {
    // 故意忽略：读取失败表示文件不存在或不可读，需要重写
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }

    fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use astrcode_core::test_support::TestEnvGuard;

    use super::*;

    #[test]
    fn bundled_skills_parse_from_claude_style_skill_directories() {
        let _guard = TestEnvGuard::new();
        let skills = load_builtin_skills();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "git-commit");
    }

    #[test]
    fn bundled_skills_materialize_directory_assets() {
        let _guard = TestEnvGuard::new();
        let skills = load_builtin_skills();

        let skill_root = skills[0]
            .skill_root
            .as_ref()
            .expect("builtin skill root should be materialized");
        assert!(Path::new(skill_root).join("SKILL.md").is_file());
    }
}
