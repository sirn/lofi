pub const EXEC_TOOL_NAME: &str = "exec";

pub const EXEC_TOOL_DESCRIPTION: &str = "Compile and run a TypeScript program in a sandboxed QuickJS runtime. The program has access to a `lofi` object with file/shell/search tools (read, ls, find, grep, write, edit, patch, bash). Top-level await and return are supported. Await every foreground `lofi` call; unawaited calls are drained and any failure makes the exec result fail. Use `jobSpawn` for background work. The result reports success, errors, logs, and an explicitly returned value; keep it compact and final.";

#[must_use]
pub fn exec_tool_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "TypeScript source. Top-level await/return supported."
            },
            "strings": {
                "type": "object",
                "description": "Named string constants exposed as the global `lofi_strings` object."
            },
            "display": {
                "type": "object",
                "description": "Optional display metadata; ignored by the runtime."
            }
        },
        "required": ["code"]
    })
}
