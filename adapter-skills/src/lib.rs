//! Skill resource discovery, parsing, and catalog resolution.

mod builtin_skills;
mod skill_catalog;
mod skill_loader;
mod skill_spec;

pub use builtin_skills::load_builtin_skills;
pub use skill_catalog::{LayeredSkillCatalog, merge_skill_layers};
pub use skill_loader::{
    SKILL_FILE_NAME, SKILL_TOOL_NAME, SkillFrontmatter, collect_asset_files, load_project_skills,
    load_user_skills, parse_skill_md, skill_roots_cache_marker,
};
pub use skill_spec::{SkillSource, SkillSpec, is_valid_skill_name, normalize_skill_name};
