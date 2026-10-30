pub const EXEC_TOOL_NAME: &str = "exec";

/// Longest description head kept, in characters. The cut marker follows the
/// head, so a kept description reaches these characters plus the marker.
/// The executor cap and the chat gate share this constant, so a capped
/// description passes the chat gate whole. The decision gate keeps a
/// separate byte budget and may cut a capped description again; it shares
/// only the cut marker.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

/// The marker a capped description appends after its kept head. Used by the
/// executor cap and by both auto-mode gates whenever they cut a description
/// head.
pub const DESCRIPTION_CUT_MARKER: &str = " [...truncated]";

pub const EXEC_TOOL_DESCRIPTION: &str = "Compile and run a TypeScript program in a sandboxed QuickJS runtime. The program has access to a `lofi` object with file/shell/search tools (read, ls, find, grep, write, edit, patch, bash). Top-level await and return are supported. Await every foreground `lofi` call; unawaited calls are drained and any failure makes the exec result fail. Use `jobSpawn` for background shell work and `subagentSpawn` for background agent runs. The result reports success, errors, logs, and an explicitly returned value; keep it compact and final. State in `description`, before the code, what the call is trying to do in one line: it is required, it labels the call, and the shell gate reads it.";

#[must_use]
pub fn exec_tool_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "TypeScript source. Top-level await/return supported."
            },
            "description": {
                "type": "string",
                "description": format!("What this call is trying to do, in one line (~80 chars; head kept to {MAX_DESCRIPTION_CHARS}).")
            },
            "strings": {
                "type": "object",
                "description": "Named string constants exposed as the global `lofi_strings` object."
            }
        },
        "required": ["code", "description"]
    })
}
