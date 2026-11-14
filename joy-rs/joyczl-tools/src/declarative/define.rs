//! `define_tool` — the writing half of the declarative-tool feature.
//!
//! Split from `declarative.rs` because the two halves answer different questions: that module
//! reads definitions from disk and turns them into tools (a loader), this one validates a
//! definition and writes it down (a writer). They share [`super::parse`] and nothing else.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{load, parse, MAX_DEFINITIONS};
use crate::{Tool, ToolCtx};

/// `define_tool`: write a definition. It lands in `<home>/tools/`, or inside an existing skill
/// when `skill` is given, and is callable from **the next turn** (the table is rebuilt per turn,
/// see `attach`).
///
/// Writing the definition is not gated beyond `JOY_EXEC`; running the command it names is gated
/// exactly like `run_command`. See the module note for why that is the honest split.
pub fn define_tool(home: PathBuf) -> Tool {
    Tool {
        name: "define_tool".to_string(),
        description: "Define a new tool for yourself: give it a name, a description, a JSON Schema for its arguments and a shell command template with {placeholders}. It is available from your next turn and runs through the same sandbox, allowlist and approval as run_command. Use it when a task needs the same command more than once — a tool you can see and describe beats re-typing the command every round. Definitions are plain JSON files: they contain no code, only a command template."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Lowercase letters, digits and underscores, starting with a letter (e.g. `pdf_extract`)."
                },
                "description": {
                    "type": "string",
                    "description": "One line the model reads to decide when to call it."
                },
                "input_schema": {
                    "type": "object",
                    "description": "JSON Schema for the arguments: {\"type\": \"object\", \"properties\": {...}, \"required\": [...]}."
                },
                "command": {
                    "type": "string",
                    "description": "The shell command, with {placeholders} matching the schema's properties (e.g. `pdftotext {path} -`). Arguments are shell-quoted for you."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "How long the command may run (1-300, default 30)."
                },
                "skill": {
                    "type": "string",
                    "description": "Put the definition inside this existing skill instead of the general shelf (`<home>/tools/`), so the skill carries its own tool."
                },
                "replace": {
                    "type": "boolean",
                    "description": "Overwrite an existing definition of the same name (default false: a definition is not silently swapped)."
                }
            },
            "required": ["name", "description", "input_schema", "command"]
        }),
        handler: Arc::new(move |_ctx: ToolCtx, args: Value| {
            let home = home.clone();
            Box::pin(async move {
                let source = "the definition passed to define_tool";
                let spec = match parse(&args.to_string(), source) {
                    Ok(spec) => spec,
                    Err(why) => return Ok(format!("Error: {why}")),
                };

                let dir = match args.get("skill").and_then(Value::as_str).map(str::trim) {
                    None | Some("") => home.join("tools"),
                    Some(skill) => {
                        let skill_dir = home.join("skills").join(skill);
                        if !skill_dir.join("SKILL.md").is_file() {
                            return Ok(format!(
                                "Error: there is no skill named {skill:?} (define the skill first, or omit `skill` to use the general shelf)."
                            ));
                        }
                        if load(&skill_dir.join("tools")).len() >= MAX_DEFINITIONS {
                            return Ok(format!(
                                "Error: the skill {skill:?} already holds {MAX_DEFINITIONS} tools; that is the cap."
                            ));
                        }
                        skill_dir.join("tools")
                    }
                };
                if dir == home.join("tools") && load(&dir).len() >= MAX_DEFINITIONS {
                    return Ok(format!(
                        "Error: <home>/tools already holds {MAX_DEFINITIONS} definitions; that is the cap. Remove one you no longer use, or put this in a skill."
                    ));
                }

                let path = dir.join(format!("{}.json", spec.name));
                let replace = args
                    .get("replace")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if path.exists() && !replace {
                    return Ok(format!(
                        "Error: a tool named {} is already defined at {}. Nothing was written — pass replace=true only if you mean to replace it.",
                        spec.name,
                        path.display()
                    ));
                }
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    return Ok(format!(
                        "Error: could not create {}: {e}",
                        dir.display()
                    ));
                }
                let body = json!({
                    "name": spec.name,
                    "description": spec.description,
                    "input_schema": spec.input_schema,
                    "command": spec.command,
                    "timeout_secs": spec.timeout_secs,
                });
                // Write beside the target, then rename: a half-written definition that the next
                // turn's loader picks up would be a tool nobody authored.
                let staging = dir.join(format!(".{}.json.tmp", spec.name));
                let rendered = match serde_json::to_string_pretty(&body) {
                    Ok(text) => text + "\n",
                    Err(e) => return Ok(format!("Error: could not serialise the definition: {e}")),
                };
                if let Err(e) = std::fs::write(&staging, rendered) {
                    return Ok(format!("Error: could not write {}: {e}", staging.display()));
                }
                if let Err(e) = std::fs::rename(&staging, &path) {
                    let _ = std::fs::remove_file(&staging);
                    return Ok(format!("Error: could not place {}: {e}", path.display()));
                }

                Ok(format!(
                    "Defined the tool '{}' at {}.\nIt is in the table from your next turn (a definition is read once per turn, so no restart is needed). Calling it runs `{}` through the same sandbox, allowlist and approval as run_command — if the allowlist does not cover it, the first call will ask.",
                    spec.name,
                    path.display(),
                    spec.command
                ))
            })
        }),
    }
}
