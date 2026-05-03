use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::{
    storage::{StorageError, ToolResultArtifactReader, ToolResultArtifactSlice},
    tool::*,
    types::SessionId,
};
use serde_json::Value;

use super::{shared::MAX_INLINE_IMAGE_BASE64_BYTES, *};

fn empty_ctx() -> ToolExecutionContext {
    ToolExecutionContext {
        session_id: String::new(),
        working_dir: String::new(),
        model_id: String::new(),
        available_tools: vec![],
        tool_call_id: None,
        event_tx: None,
        tool_result_reader: None,
    }
}

fn ctx_with_call_id(call_id: &str) -> ToolExecutionContext {
    ToolExecutionContext {
        tool_call_id: Some(call_id.into()),
        ..empty_ctx()
    }
}

struct FixedToolResultReader {
    path: String,
}

#[async_trait::async_trait]
impl ToolResultArtifactReader for FixedToolResultReader {
    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        assert_eq!(session_id, "session-1");
        assert_eq!(path, self.path);
        assert_eq!(char_offset, 2);
        assert!(max_chars <= 60_000);
        Ok(ToolResultArtifactSlice {
            path: self.path.clone(),
            bytes: 6,
            char_offset,
            returned_chars: 3,
            next_char_offset: Some(5),
            has_more: true,
            content: "cde".into(),
        })
    }
}

struct RejectingToolResultReader;

#[async_trait::async_trait]
impl ToolResultArtifactReader for RejectingToolResultReader {
    async fn read_tool_result_artifact_by_path(
        &self,
        _session_id: &SessionId,
        _path: &str,
        _char_offset: usize,
        _max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        Err(StorageError::InvalidId(
            "tool result path belongs to a different session".into(),
        ))
    }
}

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("astrcode-tools-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("temp dir should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique_temp_dir(name: &str) -> TestDir {
    TestDir::new(name)
}

fn tool_descriptions() -> Vec<ToolDefinition> {
    let working_dir = PathBuf::from(".");
    vec![
        ReadFileTool {
            working_dir: working_dir.clone(),
        }
        .definition(),
        WriteFileTool {
            working_dir: working_dir.clone(),
        }
        .definition(),
        EditFileTool {
            working_dir: working_dir.clone(),
        }
        .definition(),
        ApplyPatchTool {
            working_dir: working_dir.clone(),
        }
        .definition(),
        FindFilesTool {
            working_dir: working_dir.clone(),
        }
        .definition(),
        GrepTool { working_dir }.definition(),
    ]
}

#[test]
fn file_tool_descriptions_separate_search_read_and_write_roles() {
    let definitions = tool_descriptions();
    let find_files = definitions
        .iter()
        .find(|definition| definition.name == "find")
        .expect("find definition should exist");
    let grep = definitions
        .iter()
        .find(|definition| definition.name == "grep")
        .expect("grep definition should exist");
    let read_file = definitions
        .iter()
        .find(|definition| definition.name == "read")
        .expect("read definition should exist");
    let write_file = definitions
        .iter()
        .find(|definition| definition.name == "write")
        .expect("write definition should exist");
    let edit_file = definitions
        .iter()
        .find(|definition| definition.name == "edit")
        .expect("edit definition should exist");

    assert!(find_files.description.contains("file paths only"));
    assert!(grep.description.contains("Search file contents"));
    assert!(grep.description.contains("files_with_matches"));
    assert!(read_file.description.contains("known file"));
    assert!(write_file.description.contains("complete final content"));
    assert!(
        edit_file
            .description
            .contains("narrow exact string replacement")
    );
}

#[tokio::test]
async fn read_file_reports_text_pagination_metadata() {
    let temp = unique_temp_dir("read-file-pagination");
    let file = temp.path().join("notes.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("seed write");
    let tool = ReadFileTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(
            serde_json::json!({
                "path": "notes.txt",
                "limit": 1,
                "maxChars": 8
            }),
            &ctx_with_call_id("read-page"),
        )
        .await
        .expect("read should execute");

    assert_eq!(result.call_id, "read-page");
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.metadata["totalLines"], serde_json::json!(3));
    assert_eq!(result.metadata["shownLines"], serde_json::json!(1));
    assert_eq!(result.metadata["offset"], serde_json::json!(0));
    assert_eq!(result.metadata["limit"], serde_json::json!(1));
    assert_eq!(result.metadata["returnedChars"], serde_json::json!(8));
    assert_eq!(result.metadata["nextCharOffset"], serde_json::json!(8));
    assert_eq!(result.metadata["hasMore"], serde_json::json!(true));
    assert_eq!(result.metadata["truncated"], serde_json::json!(true));
    assert_eq!(result.metadata["nextOffset"], serde_json::json!(1));
}

#[tokio::test]
async fn read_file_reads_persisted_tool_result_path() {
    let artifact_path = std::env::temp_dir()
        .join("session")
        .join("tool-results")
        .join("shell-call-1.txt");
    let artifact_path = artifact_path.display().to_string();
    let tool = ReadFileTool {
        working_dir: PathBuf::from("."),
    };
    let ctx = ToolExecutionContext {
        session_id: "session-1".into(),
        tool_call_id: Some("read-result".into()),
        tool_result_reader: Some(Arc::new(FixedToolResultReader {
            path: artifact_path.clone(),
        })),
        ..empty_ctx()
    };

    let result = tool
        .execute(
            serde_json::json!({
                "path": artifact_path.clone(),
                "charOffset": 2,
                "maxChars": 3
            }),
            &ctx,
        )
        .await
        .expect("read should read persisted result");

    assert_eq!(result.call_id, "read-result");
    assert_eq!(result.content, "cde");
    assert_eq!(result.metadata["path"], serde_json::json!(artifact_path));
    assert_eq!(
        result.metadata["source"],
        serde_json::json!("toolResultArtifact")
    );
    assert_eq!(result.metadata["hasMore"], serde_json::json!(true));
    assert_eq!(result.metadata["nextCharOffset"], serde_json::json!(5));
}

#[tokio::test]
async fn read_file_does_not_read_other_session_tool_result_path() {
    let artifact_path = std::env::temp_dir()
        .join("other-session")
        .join("tool-results")
        .join("shell-call-1.txt");
    let tool = ReadFileTool {
        working_dir: PathBuf::from("."),
    };
    let ctx = ToolExecutionContext {
        session_id: "session-1".into(),
        tool_call_id: Some("read-result".into()),
        tool_result_reader: Some(Arc::new(RejectingToolResultReader)),
        ..empty_ctx()
    };

    let result = tool
        .execute(
            serde_json::json!({
                "path": artifact_path.display().to_string()
            }),
            &ctx,
        )
        .await
        .expect("read should return a normal tool error");

    assert!(result.is_error);
    assert!(result.content.contains("escapes working directory"));
    assert!(!result.content.contains("different session"));
}

#[tokio::test]
async fn read_file_returns_inline_image_payload() {
    let temp = unique_temp_dir("read-file-image");
    let file = temp.path().join("pixel.png");
    let png_1x1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    std::fs::write(&file, png_1x1).expect("seed image");
    let tool = ReadFileTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(serde_json::json!({ "path": "pixel.png" }), &empty_ctx())
        .await
        .expect("read should execute");

    assert!(!result.is_error, "{result:?}");
    let payload: Value =
        serde_json::from_str(&result.content).expect("image output should be JSON");
    assert_eq!(payload["type"], serde_json::json!("image"));
    assert_eq!(payload["mediaType"], serde_json::json!("image/png"));
    assert!(
        payload["data"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(result.metadata["fileType"], serde_json::json!("image"));
}

#[tokio::test]
async fn read_file_rejects_oversize_image_payload() {
    let temp = unique_temp_dir("read-file-large-image");
    std::fs::write(temp.path().join("large.png"), vec![1u8; 800 * 1024]).expect("seed large image");
    let tool = ReadFileTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(serde_json::json!({ "path": "large.png" }), &empty_ctx())
        .await
        .expect("read should execute");

    assert!(result.is_error);
    assert_eq!(result.metadata["fileType"], serde_json::json!("image"));
    assert_eq!(
        result.metadata["maxBase64Bytes"],
        serde_json::json!(MAX_INLINE_IMAGE_BASE64_BYTES)
    );
}

#[tokio::test]
async fn read_file_marks_binary_files_without_reading_text() {
    let temp = unique_temp_dir("read-file-binary");
    std::fs::write(temp.path().join("data.bin"), b"hello\0world").expect("seed binary");
    let tool = ReadFileTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(serde_json::json!({ "path": "data.bin" }), &empty_ctx())
        .await
        .expect("read should execute");

    assert!(!result.is_error);
    assert_eq!(result.metadata["binary"], serde_json::json!(true));
}

#[tokio::test]
async fn edit_file_applies_multiple_edits_atomically() {
    let temp = unique_temp_dir("edit-file-multi");
    let file = temp.path().join("notes.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("seed file");
    let tool = EditFileTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(
            serde_json::json!({
                "path": "notes.txt",
                "edits": [
                    { "oldStr": "alpha", "newStr": "one" },
                    { "oldStr": "gamma", "newStr": "three" }
                ]
            }),
            &empty_ctx(),
        )
        .await
        .expect("edit should execute");

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.metadata["operationCount"], serde_json::json!(2));
    assert_eq!(result.metadata["replacements"], serde_json::json!(2));
    assert_eq!(
        std::fs::read_to_string(&file).expect("updated file should be readable"),
        "one\nbeta\nthree\n"
    );
}

#[tokio::test]
async fn edit_file_multi_edit_does_not_write_after_late_failure() {
    let temp = unique_temp_dir("edit-file-multi-failure");
    let file = temp.path().join("notes.txt");
    std::fs::write(&file, "alpha\nbeta\n").expect("seed file");
    let tool = EditFileTool {
        working_dir: temp.path().to_path_buf(),
    };

    let error = tool
        .execute(
            serde_json::json!({
                "path": "notes.txt",
                "edits": [
                    { "oldStr": "alpha", "newStr": "one" },
                    { "oldStr": "missing", "newStr": "nope" }
                ]
            }),
            &empty_ctx(),
        )
        .await
        .expect_err("late multiEdit failure should fail the call");

    assert!(error.to_string().contains("oldStr not found"));
    assert_eq!(
        std::fs::read_to_string(&file).expect("original file should be readable"),
        "alpha\nbeta\n"
    );
}

#[tokio::test]
async fn find_files_respects_gitignore_hidden_and_brace_glob() {
    let temp = unique_temp_dir("find-files-filters");
    std::fs::write(temp.path().join(".gitignore"), "ignored/\n").expect("seed gitignore");
    std::fs::write(temp.path().join("visible.json"), "{}").expect("seed visible");
    std::fs::write(temp.path().join("visible.toml"), "").expect("seed visible");
    std::fs::write(temp.path().join(".hidden.json"), "{}").expect("seed hidden");
    std::fs::create_dir_all(temp.path().join("ignored")).expect("create ignored");
    std::fs::write(temp.path().join("ignored").join("skip.json"), "{}").expect("seed ignored");
    let tool = FindFilesTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(
            serde_json::json!({
                "pattern": "*.{json,toml}",
                "includeHidden": false
            }),
            &empty_ctx(),
        )
        .await
        .expect("find should execute");

    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains("visible.json"));
    assert!(result.content.contains("visible.toml"));
    assert!(!result.content.contains(".hidden.json"));
    assert!(!result.content.contains("skip.json"));
}

#[tokio::test]
async fn find_files_reports_truncation_and_blocks_root_escape() {
    let temp = unique_temp_dir("find-files-truncated");
    for name in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(temp.path().join(name), "").expect("seed file");
    }
    let tool = FindFilesTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(
            serde_json::json!({ "pattern": "*.rs", "maxResults": 2 }),
            &empty_ctx(),
        )
        .await
        .expect("find should execute");

    assert_eq!(result.metadata["count"], serde_json::json!(2));
    assert_eq!(result.metadata["totalMatches"], serde_json::json!(3));
    assert_eq!(result.metadata["offset"], serde_json::json!(0));
    assert_eq!(result.metadata["nextOffset"], serde_json::json!(2));
    assert_eq!(result.metadata["truncated"], serde_json::json!(true));
    assert_eq!(result.metadata["hasMore"], serde_json::json!(true));
    assert_eq!(result.metadata["files"].as_array().map(Vec::len), Some(2));

    let escaped = tool
        .execute(
            serde_json::json!({ "pattern": "*.rs", "root": ".." }),
            &empty_ctx(),
        )
        .await
        .expect("find should return a structured error");
    assert!(escaped.is_error);
    assert_eq!(
        escaped.metadata["pathEscapesWorkingDir"],
        serde_json::json!(true)
    );
}

#[tokio::test]
async fn find_files_default_limit_keeps_path_lists_compact() {
    let temp = unique_temp_dir("find-files-default-limit");
    for index in 0..101 {
        std::fs::write(temp.path().join(format!("{index:03}.rs")), "").expect("seed file");
    }
    let tool = FindFilesTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(serde_json::json!({ "pattern": "*.rs" }), &empty_ctx())
        .await
        .expect("find should execute");

    assert_eq!(result.metadata["count"], serde_json::json!(100));
    assert_eq!(result.metadata["maxResults"], serde_json::json!(100));
    assert_eq!(result.metadata["totalMatches"], serde_json::json!(101));
    assert_eq!(result.metadata["nextOffset"], serde_json::json!(100));
    assert_eq!(result.metadata["hasMore"], serde_json::json!(true));
}

#[tokio::test]
async fn find_files_supports_offset_pagination() {
    let temp = unique_temp_dir("find-files-offset");
    for name in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(temp.path().join(name), "").expect("seed file");
    }
    let tool = FindFilesTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(
            serde_json::json!({
                "pattern": "*.rs",
                "offset": 1,
                "maxResults": 1
            }),
            &empty_ctx(),
        )
        .await
        .expect("find should execute");

    assert_eq!(result.content.lines().count(), 1);
    assert_eq!(result.metadata["count"], serde_json::json!(1));
    assert_eq!(result.metadata["totalMatches"], serde_json::json!(3));
    assert_eq!(result.metadata["offset"], serde_json::json!(1));
    assert_eq!(result.metadata["nextOffset"], serde_json::json!(2));
    assert_eq!(result.metadata["files"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn grep_literal_content_handles_punctuation() {
    let temp = unique_temp_dir("grep-literal");
    std::fs::write(temp.path().join("lib.rs"), "#[cfg(test)]\nmod tests {}\n").expect("seed file");
    let tool = GrepTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(
            serde_json::json!({
                "pattern": "#[cfg(test)]",
                "literal": true,
                "outputMode": "content"
            }),
            &empty_ctx(),
        )
        .await
        .expect("grep should execute");

    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains("#[cfg(test)]"));
    assert_eq!(result.metadata["outputMode"], serde_json::json!("content"));
}

#[tokio::test]
async fn grep_limits_files_and_count_modes() {
    let temp = unique_temp_dir("grep-max-files");
    for name in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(temp.path().join(name), "hit\n").expect("seed file");
    }
    let tool = GrepTool {
        working_dir: temp.path().to_path_buf(),
    };

    let files = tool
        .execute(
            serde_json::json!({
                "pattern": "hit",
                "outputMode": "files_with_matches",
                "maxMatches": 2
            }),
            &empty_ctx(),
        )
        .await
        .expect("grep should execute");
    assert_eq!(files.content.lines().count(), 2);
    assert_eq!(files.metadata["returned"], serde_json::json!(2));
    assert_eq!(files.metadata["nextOffset"], serde_json::json!(2));
    assert_eq!(files.metadata["hasMore"], serde_json::json!(true));

    let counts = tool
        .execute(
            serde_json::json!({
                "pattern": "hit",
                "outputMode": "count",
                "maxMatches": 1
            }),
            &empty_ctx(),
        )
        .await
        .expect("grep should execute");
    assert_eq!(counts.content.lines().count(), 1);
    assert_eq!(counts.metadata["returned"], serde_json::json!(1));
    assert_eq!(counts.metadata["nextOffset"], serde_json::json!(1));
    assert_eq!(counts.metadata["hasMore"], serde_json::json!(true));
}

#[tokio::test]
async fn grep_invalid_regex_points_to_literal_mode() {
    let temp = unique_temp_dir("grep-invalid-regex");
    std::fs::write(temp.path().join("lib.rs"), "#[cfg(test)]\n").expect("seed file");
    let tool = GrepTool {
        working_dir: temp.path().to_path_buf(),
    };

    let error = tool
        .execute(
            serde_json::json!({
                "pattern": "["
            }),
            &empty_ctx(),
        )
        .await
        .expect_err("invalid regex should fail");

    assert!(error.to_string().contains("literal"));
    assert!(error.to_string().contains("true"));
}

#[tokio::test]
async fn grep_respects_gitignore_and_skips_binary_files() {
    let temp = unique_temp_dir("grep-ignore-binary");
    std::fs::write(temp.path().join(".gitignore"), "ignored.rs\n").expect("seed gitignore");
    std::fs::write(temp.path().join("visible.rs"), "needle\n").expect("seed visible");
    std::fs::write(temp.path().join("ignored.rs"), "needle\n").expect("seed ignored");
    std::fs::write(temp.path().join("binary.rs"), b"needle\0").expect("seed binary");
    let tool = GrepTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(
            serde_json::json!({
                "pattern": "needle",
                "literal": true,
                "outputMode": "files_with_matches"
            }),
            &empty_ctx(),
        )
        .await
        .expect("grep should execute");

    assert!(result.content.contains("visible.rs"));
    assert!(!result.content.contains("ignored.rs"));
    assert!(!result.content.contains("binary.rs"));
    assert_eq!(result.metadata["skippedFiles"], serde_json::json!(1));
}

#[tokio::test]
async fn grep_multiline_matches_across_line_breaks() {
    let temp = unique_temp_dir("grep-multiline");
    std::fs::write(
        temp.path().join("lib.rs"),
        "fn start() {\n    work();\n    finish();\n}\n",
    )
    .expect("seed file");
    let tool = GrepTool {
        working_dir: temp.path().to_path_buf(),
    };

    let result = tool
        .execute(
            serde_json::json!({
                "pattern": "fn start\\(\\) \\{.*finish\\(\\);",
                "outputMode": "content",
                "multiline": true
            }),
            &empty_ctx(),
        )
        .await
        .expect("grep should execute");

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.metadata["multiline"], serde_json::json!(true));
    assert!(result.content.contains(":1-3:"));
    assert!(result.content.contains("fn start()"));
    assert!(result.content.contains("finish();"));
}

#[tokio::test]
async fn patch_creates_new_file() {
    let temp = unique_temp_dir("patch-create");
    let tool = ApplyPatchTool {
        working_dir: temp.path().to_path_buf(),
    };
    let patch = "--- /dev/null\n+++ b/hello.rs\n@@ -0,0 +1,3 @@\n+fn main() {\n+    \
                 println!(\"hello\");\n+}\n";

    let result = tool
        .execute(serde_json::json!({ "patch": patch }), &empty_ctx())
        .await
        .expect("patch should execute");

    assert!(!result.is_error, "{result:?}");
    assert!(temp.path().join("hello.rs").exists());
}

#[tokio::test]
async fn patch_updates_existing_file() {
    let temp = unique_temp_dir("patch-update");
    let file = temp.path().join("test.rs");
    std::fs::write(&file, "fn foo() {\n    old();\n}\n").expect("seed write");
    let tool = ApplyPatchTool {
        working_dir: temp.path().to_path_buf(),
    };
    let patch =
        "--- a/test.rs\n+++ b/test.rs\n@@ -1,3 +1,3 @@\nfn foo() {\n-    old();\n+    new();\n}\n";

    let result = tool
        .execute(serde_json::json!({ "patch": patch }), &empty_ctx())
        .await
        .expect("patch should execute");

    assert!(!result.is_error, "{result:?}");
    let content = std::fs::read_to_string(file).expect("updated file should be readable");
    assert!(content.contains("new()"));
    assert!(!content.contains("old()"));
}

#[tokio::test]
async fn patch_preserves_crlf_line_endings() {
    let temp = unique_temp_dir("patch-crlf");
    let file = temp.path().join("windows.rs");
    std::fs::write(&file, "fn foo() {\r\n    old();\r\n}\r\n").expect("seed write");
    let tool = ApplyPatchTool {
        working_dir: temp.path().to_path_buf(),
    };
    let patch =
        "--- a/windows.rs\n+++ b/windows.rs\n@@ -1,3 +1,3 @@\nfn foo() {\n-    old();\n+    \
         new();\n}\n";

    let result = tool
        .execute(serde_json::json!({ "patch": patch }), &empty_ctx())
        .await
        .expect("patch should execute");

    assert!(!result.is_error, "{result:?}");
    let content = std::fs::read_to_string(file).expect("updated file should be readable");
    assert_eq!(content, "fn foo() {\r\n    new();\r\n}\r\n");
}

#[tokio::test]
async fn patch_rejects_delete_when_content_differs() {
    let temp = unique_temp_dir("patch-delete-mismatch");
    let file = temp.path().join("old.txt");
    std::fs::write(&file, "line one\nline changed\n").expect("seed write");
    let tool = ApplyPatchTool {
        working_dir: temp.path().to_path_buf(),
    };
    let patch = "--- a/old.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-line one\n-line two\n";

    let result = tool
        .execute(serde_json::json!({ "patch": patch }), &empty_ctx())
        .await
        .expect("patch should execute");

    assert!(result.is_error);
    assert!(file.exists());
}
