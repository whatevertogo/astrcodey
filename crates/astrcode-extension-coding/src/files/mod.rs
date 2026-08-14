mod edit;
mod patch;
mod read;
mod search;
mod tool_result;
mod write;

use std::{collections::BTreeMap, sync::Arc};

use astrcode_extension_sdk::{
    builder::ExtensionToolDefinition,
    extension::Registrar,
    host::HostWorkspaceTextChange,
    tool::{ToolPromptMetadata, ToolPromptTag},
};
use serde_json::Value;

pub(super) fn register(registrar: &mut Registrar) {
    let filesystem_prompt =
        || ToolPromptMetadata::new(String::new()).prompt_tag(ToolPromptTag::Filesystem);
    registrar.tool(
        ExtensionToolDefinition::from_definition(read::definition())
            .with_prompt(filesystem_prompt()),
        Arc::new(read::ReadHandler),
    );
    registrar.tool(
        ExtensionToolDefinition::from_definition(tool_result::definition())
            .with_prompt(filesystem_prompt()),
        Arc::new(tool_result::ReadToolResultHandler),
    );
    registrar.tool(
        ExtensionToolDefinition::from_definition(write::definition())
            .with_prompt(filesystem_prompt()),
        Arc::new(write::WriteHandler),
    );
    registrar.tool(
        ExtensionToolDefinition::from_definition(edit::definition())
            .with_prompt(filesystem_prompt()),
        Arc::new(edit::EditHandler),
    );
    registrar.tool(
        ExtensionToolDefinition::from_definition(patch::definition())
            .with_prompt(filesystem_prompt()),
        Arc::new(patch::PatchHandler),
    );
    for (definition, handler) in search::handlers() {
        registrar.tool(
            ExtensionToolDefinition::from_definition(definition).with_prompt(filesystem_prompt()),
            handler,
        );
    }
}

fn absolute_path(working_dir: &std::path::Path, path: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        path.to_owned()
    } else {
        working_dir.join(path)
    }
}

fn text_change_metadata(change: &HostWorkspaceTextChange) -> BTreeMap<String, Value> {
    let mut metadata = BTreeMap::from([("newBytes".into(), Value::from(change.new_bytes))]);
    if let Some(old_bytes) = change.old_bytes {
        metadata.insert("oldBytes".into(), Value::from(old_bytes));
    }
    if let Some(diff) = &change.unified_diff {
        metadata.insert("diff".into(), Value::from(diff.clone()));
        metadata.insert("insertions".into(), Value::from(change.insertions as u64));
        metadata.insert("deletions".into(), Value::from(change.deletions as u64));
        metadata.insert("diffTruncated".into(), Value::from(change.diff_truncated));
    }
    metadata
}
