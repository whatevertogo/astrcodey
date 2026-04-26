//! Skill 文件系统加载与解析。
//!
//! 本模块只负责本地目录扫描、`SKILL.md` 解析和资产枚举。
//! Skill 来源的合并、优先级覆盖和运行时解析由 [`SkillCatalog`](crate::SkillCatalog)
//! 统一负责，避免把 orchestration 逻辑散落在 loader 调用点。
//!
//! # Skill 目录结构
//!
//! 每个 skill 是一个文件夹，必须包含 `SKILL.md` 文件。`SKILL.md` 的 YAML frontmatter
//! 仅包含 `name` 和 `description` 两个字段，正文为 skill 的详细指南。
//!
//! ```text
//! skills/
//!   git-commit/
//!     SKILL.md          # frontmatter + 指南正文
//!     references/       # 可选：参考资料
//!     scripts/          # 可选：辅助脚本
//! ```
//!
//! # 两阶段模型
//!
//! 1. **索引阶段**：解析 `SKILL.md` 的 frontmatter，获取 name + description
//! 2. **按需加载**：当模型调用 `Skill` tool 时，加载完整 guide 和 asset_files
//!
//! 用户可以在 `~/.astrcode/skills/` 或 `~/.claude/skills/` 中放置自定义 skill，
//! 在项目祖先链上的 `.astrcode/skills/` 或 `.claude/skills/` 中放置项目特定 skill；
//! 这些层级的最终覆盖顺序由 `SkillCatalog` 决定。

use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use astrcode_support::hostpaths::resolve_home_dir;
use log::warn;
use serde::Deserialize;

use crate::{SkillSource, SkillSpec, is_valid_skill_name};

/// Skill 文件名（固定为 SKILL.md）。
pub const SKILL_FILE_NAME: &str = "SKILL.md";

/// 内置 Skill 能力的 tool 名称。
///
/// 此常量在 prompt 贡献者（判断 tool list 中是否包含 "Skill"）
/// 和 runtime tool 实现之间共享。
pub const SKILL_TOOL_NAME: &str = "Skill";

/// Claude-style skill 的 YAML frontmatter 结构。
///
/// 设计意图：frontmatter 仅保留发现所需的最小信息（name + description），
/// 真正的执行元数据由 runtime 代码管理，不放入 markdown frontmatter。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
}

/// 解析 `SKILL.md` 内容为 [`SkillSpec`]。
///
/// # 校验规则
///
/// - 必须有 YAML frontmatter（`---` 包裹）
/// - frontmatter 必须包含 `name` 和 `description`
/// - `name` 必须与文件夹名（`fallback_id`）一致
/// - `name` 必须为合法的 kebab-case 格式
/// - 正文不能为空
///
/// 任何校验失败都会返回 `None` 并记录警告日志。
pub fn parse_skill_md(content: &str, fallback_id: &str, source: SkillSource) -> Option<SkillSpec> {
    let normalized = normalize_skill_content(content);
    if normalized.trim().is_empty() {
        return None;
    }

    let (frontmatter, body) = match split_frontmatter(&normalized) {
        Some((frontmatter, body)) => match serde_yaml::from_str::<SkillFrontmatter>(frontmatter) {
            Ok(frontmatter) => (frontmatter, body),
            Err(error) => {
                warn!("failed to parse frontmatter for skill '{fallback_id}': {error}");
                return None;
            },
        },
        None => {
            warn!("skill '{fallback_id}' is missing YAML frontmatter; expected name + description");
            return None;
        },
    };

    let name = frontmatter.name.trim().to_string();
    if name != fallback_id {
        warn!(
            "skill frontmatter name '{}' must match its kebab-case folder name '{}'",
            name, fallback_id
        );
        return None;
    }
    if !is_valid_skill_name(&name) {
        warn!(
            "skill '{}' must be kebab-case with lowercase ascii letters, digits, and hyphens only",
            name
        );
        return None;
    }

    let description = frontmatter.description.trim().to_string();
    if description.is_empty() {
        warn!("skill '{fallback_id}' is missing required frontmatter description");
        return None;
    }

    let guide = body.trim().to_string();
    if guide.is_empty() {
        warn!("skill '{fallback_id}' is missing required markdown body");
        return None;
    }

    Some(SkillSpec {
        id: name.clone(),
        name,
        description,
        guide,
        skill_root: None,
        asset_files: Vec::new(),
        allowed_tools: Vec::new(),
        source,
    })
}

/// 从指定目录加载所有 skill。
///
/// 遍历目录下的每个子文件夹，查找 `SKILL.md` 并解析。
/// 同时收集每个 skill 目录下的所有资产文件（用于运行时访问）。
pub fn load_skills_from_dir(dir: &Path, source: SkillSource) -> Vec<SkillSpec> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut children = entries.filter_map(Result::ok).collect::<Vec<_>>();
    children.sort_by_key(|entry| entry.file_name());

    let mut skills = Vec::new();
    for entry in children {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                warn!(
                    "failed to inspect skill directory entry {}: {error}",
                    entry.path().display()
                );
                continue;
            },
        };
        if !file_type.is_dir() {
            continue;
        }

        let skill_dir = entry.path();
        let skill_path = skill_dir.join(SKILL_FILE_NAME);
        if !skill_path.is_file() {
            continue;
        }

        let folder_name = entry.file_name();
        let fallback_id = folder_name.to_string_lossy();
        let content = match fs::read_to_string(&skill_path) {
            Ok(content) => content,
            Err(error) => {
                warn!("failed to read {}: {error}", skill_path.display());
                continue;
            },
        };

        if let Some(mut skill) = parse_skill_md(&content, &fallback_id, source.clone()) {
            skill.skill_root = Some(skill_dir.to_string_lossy().into_owned());
            skill.asset_files = collect_asset_files(&skill_dir);
            skills.push(skill);
        }
    }

    skills
}

/// 加载用户级 skill。
///
/// 从两个位置加载：
/// - `~/.claude/skills/`（兼容 Claude 风格的 skill）
/// - `~/.astrcode/skills/`（Astrcode 专属 skill）
///
/// 同名 skill 以 `.astrcode` 版本为准（后者覆盖前者）。
/// 这里只处理“用户层内部”的目录合并；更高层的 project/plugin/builtin 优先级
/// 统一交给 `SkillCatalog`。
pub fn load_user_skills() -> Vec<SkillSpec> {
    let Some(home_dir) = resolve_user_home_dir() else {
        return Vec::new();
    };

    load_user_skills_from_home_dir(&home_dir)
}

/// 加载项目级 skill。
///
/// 从工作目录祖先链上的 `.claude/skills/` 与 `.astrcode/skills/` 加载。
/// 越靠近当前 working_dir 的目录优先级越高，同名 skill 覆盖祖先目录定义。
pub fn load_project_skills(working_dir: &str) -> Vec<SkillSpec> {
    let excluded_dirs = resolve_user_home_dir()
        .map(|home_dir| user_skill_dir_candidates(&home_dir))
        .unwrap_or_default();
    load_project_skills_from_working_dir(Path::new(working_dir), &excluded_dirs)
}

pub(crate) fn load_project_skills_from_working_dir(
    working_dir: &Path,
    excluded_dirs: &[PathBuf],
) -> Vec<SkillSpec> {
    let mut project_skills = Vec::new();
    for dir in project_skill_dir_candidates(working_dir) {
        if excluded_dirs.iter().any(|excluded| excluded == &dir) {
            continue;
        }
        project_skills = crate::skill_catalog::merge_skill_layers(
            project_skills,
            load_skills_from_dir(&dir, SkillSource::Project),
        );
    }
    project_skills
}

/// 生成 skill 根目录的缓存标记。
///
/// 基于用户 skill 目录和项目祖先链上的 skill 目录文件元数据生成指纹，
/// 用于 prompt 层的 `SkillSummaryContributor` 检测 skill 目录变化以决定缓存是否失效。
pub fn skill_roots_cache_marker(working_dir: &str) -> String {
    let mut markers = Vec::new();

    if let Some(home_dir) = resolve_user_home_dir() {
        for dir in user_skill_dir_candidates(&home_dir) {
            markers.push(cache_marker_for_skill_root(&dir));
        }
    } else {
        markers.push("user-home=<unresolved>".to_string());
    }

    for dir in project_skill_dir_candidates(Path::new(working_dir)) {
        markers.push(cache_marker_for_skill_root(&dir));
    }

    markers.join("|")
}

/// 规范化 skill 文件内容。
///
/// 去除 BOM（\u{feff}），统一换行符为 \n。
fn normalize_skill_content(content: &str) -> String {
    content
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// 分割 YAML frontmatter 和正文。
///
/// 查找 `---\n...\n---` 包裹的区域，返回 (frontmatter, body)。
/// 支持 frontmatter 在文件末尾结束（无后续正文）的情况。
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    if !content.starts_with("---\n") {
        return None;
    }

    let rest = &content[4..];
    if let Some(end) = rest.find("\n---\n") {
        return Some((&rest[..end], &rest[end + 5..]));
    }

    rest.find("\n---")
        .filter(|end| rest[*end + 4..].is_empty())
        .map(|end| (&rest[..end], ""))
}

/// 解析用户主目录。
///
/// 失败时返回 `None` 并记录警告，不会抛出错误。
/// 这是为了保证 skill 加载是"尽力而为"的——即使主目录无法解析，
/// 也不应阻塞整个 prompt 组装流程。
fn resolve_user_home_dir() -> Option<PathBuf> {
    match resolve_home_dir() {
        Ok(home_dir) => Some(home_dir),
        Err(error) => {
            warn!("failed to resolve home directory for skills: {error}");
            None
        },
    }
}

pub(crate) fn load_user_skills_from_home_dir(home_dir: &Path) -> Vec<SkillSpec> {
    let mut user_skills = Vec::new();
    for dir in user_skill_dir_candidates(home_dir) {
        user_skills = crate::skill_catalog::merge_skill_layers(
            user_skills,
            load_skills_from_dir(&dir, SkillSource::User),
        );
    }
    user_skills
}

pub(crate) fn user_skill_dir_candidates(home_dir: &Path) -> Vec<PathBuf> {
    vec![
        home_dir.join(".claude").join("skills"),
        home_dir.join(".astrcode").join("skills"),
    ]
}

fn project_skill_dir_candidates(working_dir: &Path) -> Vec<PathBuf> {
    let mut ancestors = working_dir.ancestors().collect::<Vec<_>>();
    ancestors.reverse();

    let mut dirs = Vec::new();
    for ancestor in ancestors {
        dirs.push(ancestor.join(".claude").join("skills"));
        dirs.push(ancestor.join(".astrcode").join("skills"));
    }
    dirs
}

fn cache_marker_for_path(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();

            format!("present:{}:{modified}", metadata.len())
        },
        Err(_) => "missing".to_string(),
    }
}

fn cache_marker_for_skill_root(root: &Path) -> String {
    if !root.exists() {
        return format!("{}=missing", root.display());
    }

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            warn!("failed to read skill directory {}: {error}", root.display());
            return format!("{}=unreadable", root.display());
        },
    };

    let mut markers = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        markers.push(format!(
            "{}=[{}]",
            entry.path().display(),
            cache_markers_for_skill_dir(&entry.path()).join(",")
        ));
    }
    markers.sort();

    format!("{}:[{}]", root.display(), markers.join(","))
}

/// 收集 skill 目录中的所有资产文件（排除 `SKILL.md`）。
///
/// 递归遍历 skill 目录，返回相对于 skill 根目录的文件路径列表。
/// 这些文件在 skill 执行时可能被引用（如脚本、参考文档）。
pub fn collect_asset_files(skill_dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files_recursive(skill_dir, skill_dir, &mut files);
    files.retain(|path| path != SKILL_FILE_NAME);
    files.sort();
    files
}

fn cache_markers_for_skill_dir(skill_dir: &Path) -> Vec<String> {
    let mut markers = Vec::new();
    let skill_path = skill_dir.join(SKILL_FILE_NAME);
    markers.push(format!(
        "{}={}",
        SKILL_FILE_NAME,
        cache_marker_for_path(&skill_path)
    ));
    for asset in collect_asset_files(skill_dir) {
        let path = skill_dir.join(asset.replace('/', std::path::MAIN_SEPARATOR_STR));
        markers.push(format!("{}={}", asset, cache_marker_for_path(&path)));
    }
    markers
}

fn collect_files_recursive(root: &Path, base_dir: &Path, files: &mut Vec<String>) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_files_recursive(&path, base_dir, files);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(base_dir) {
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use astrcode_core::test_support::TestEnvGuard;

    use super::*;

    fn write_skill(root: &Path, name: &str, content: &str) {
        let skill_dir = root.join(name);
        fs::create_dir_all(&skill_dir).expect("skill directory should be created");
        fs::write(skill_dir.join(SKILL_FILE_NAME), content).expect("skill file should be written");
    }

    #[test]
    fn parse_skill_md_with_claude_style_frontmatter() {
        let parsed = parse_skill_md(
            "---\nname: git-commit\ndescription: Use this skill when the user asks for a commit \
             workflow.\n---\n# Guide\nUse commit skill.\n",
            "git-commit",
            SkillSource::User,
        )
        .expect("frontmatter skill should parse");

        assert_eq!(parsed.id, "git-commit");
        assert_eq!(parsed.name, "git-commit");
        assert_eq!(
            parsed.description,
            "Use this skill when the user asks for a commit workflow."
        );
        assert_eq!(parsed.guide, "# Guide\nUse commit skill.");
        assert!(parsed.allowed_tools.is_empty());
        assert_eq!(parsed.source, SkillSource::User);
    }

    #[test]
    fn parse_skill_md_ignores_additional_claude_frontmatter_fields() {
        let parsed = parse_skill_md(
            "---\nname: git-commit\ndescription: Commit guide.\nlicense: MIT\nmetadata:\n  \
             author: demo\n---\n# Commit guide",
            "git-commit",
            SkillSource::User,
        )
        .expect("extra Claude metadata should be ignored");

        assert_eq!(parsed.id, "git-commit");
        assert_eq!(parsed.description, "Commit guide.");
    }

    #[test]
    fn load_skills_from_dir_scans_subdirs() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        write_skill(
            dir.path(),
            "git-commit",
            "---\nname: git-commit\ndescription: Commit guide.\n---\n# Commit guide",
        );
        write_skill(
            dir.path(),
            "repo-search",
            "---\nname: repo-search\ndescription: Search guide.\n---\n# Search guide",
        );

        let skills = load_skills_from_dir(dir.path(), SkillSource::User);
        let ids = skills.into_iter().map(|skill| skill.id).collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec!["git-commit".to_string(), "repo-search".to_string()]
        );
    }

    #[test]
    fn load_user_skills_from_home_dir_reads_claude_and_astrcode_layers() {
        let guard = TestEnvGuard::new();
        write_skill(
            &guard.home_dir().join(".claude").join("skills"),
            "clarify-first",
            "---\nname: clarify-first\ndescription: Ask clarifying questions.\n---\n# Clarify",
        );
        write_skill(
            &guard.home_dir().join(".astrcode").join("skills"),
            "clarify-first",
            "---\nname: clarify-first\ndescription: Astrcode override.\n---\n# Clarify",
        );
        write_skill(
            &guard.home_dir().join(".astrcode").join("skills"),
            "repo-search",
            "---\nname: repo-search\ndescription: Search the repo.\n---\n# Search",
        );

        let skills = load_user_skills_from_home_dir(guard.home_dir());
        assert_eq!(skills.len(), 2);
        assert_eq!(
            skills
                .iter()
                .find(|skill| skill.id == "clarify-first")
                .map(|skill| skill.description.as_str()),
            Some("Astrcode override.")
        );
        assert!(skills.iter().any(|skill| skill.id == "repo-search"));
    }

    #[test]
    fn load_project_skills_reads_claude_and_astrcode_from_ancestor_chain() {
        let guard = TestEnvGuard::new();
        let workspace = guard.home_dir().join("workspace");
        let nested = workspace.join("packages").join("app");
        fs::create_dir_all(&nested).expect("nested working dir should exist");

        write_skill(
            &workspace.join(".claude").join("skills"),
            "shared-skill",
            "---\nname: shared-skill\ndescription: Workspace skill.\nlicense: MIT\n---\n# Shared",
        );
        write_skill(
            &workspace.join(".astrcode").join("skills"),
            "workspace-only",
            "---\nname: workspace-only\ndescription: Workspace astrcode skill.\n---\n# Workspace",
        );
        write_skill(
            &nested.join(".claude").join("skills"),
            "shared-skill",
            "---\nname: shared-skill\ndescription: App override.\n---\n# App",
        );
        write_skill(
            &nested.join(".astrcode").join("skills"),
            "app-only",
            "---\nname: app-only\ndescription: App astrcode skill.\n---\n# App only",
        );

        let skills = load_project_skills(&nested.to_string_lossy());
        assert_eq!(
            skills
                .iter()
                .find(|skill| skill.id == "shared-skill")
                .map(|skill| skill.description.as_str()),
            Some("App override.")
        );
        assert!(skills.iter().any(|skill| skill.id == "workspace-only"));
        assert!(skills.iter().any(|skill| skill.id == "app-only"));
    }
}
