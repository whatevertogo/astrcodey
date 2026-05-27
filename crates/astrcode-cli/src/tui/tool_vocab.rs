pub(crate) fn tool_display_name(tool_name: &str) -> &str {
    match tool_name {
        "shell" => "Bash",
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "find" => "Find",
        "grep" => "Search",
        "patch" => "Patch",
        "agent" => "Task",
        "switchMode" => "Mode",
        other => other,
    }
}
