//! Declarative tools: a tool definition is **data**, and the data is re-read every turn.
//!
//! The gap this fills. Joy's tool table was assembled at boot: built-ins in code, MCP servers
//! from `mcp.json`. When a task needed something nobody had written — "extract the text out of
//! this PDF", "resize these images" — the honest answer was "I have no tool for that", and the
//! only ways out were `run_command` (a new command string every time, no schema, invisible to
//! the tool table) or asking a human to write a Rust tool and rebuild.
//!
//! What makes a definition a *tool* and not a shell script is the schema: the model sees a name,
//! a description and an argument list, and calls it like any other tool. What makes it safe is
//! that it is **only** a command template — there is no code in the file, no environment, no
//! interpreter of its own:
//!
//! ```json
//! {
//!   "name": "pdf_extract",
//!   "description": "Extract the text from a PDF",
//!   "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]},
//!   "command": "pdftotext {path} -",
//!   "timeout_secs": 30
//! }
//! ```
//!
//! Three properties hold by construction, and they are the whole security story:
//!
//! 1. **The command still goes through [`crate::exec::execute`]** — the allowlist, the approval
//!    broker, the sandbox and the output spill are the same ones `run_command` uses. A
//!    declarative tool cannot do anything `run_command` could not; it only remembers the
//!    command and describes its arguments.
//! 2. **Arguments are shell-quoted as they are substituted.** `{path}` becomes `'…'` (with `'`
//!    escaped), so a value can never break out of its argument and turn "extract this file"
//!    into "extract this file; do something else". The template author fixes the shape; the
//!    model only fills holes.
//! 3. **Definitions live in `<home>/tools/`, not on the control plane.** `settings.json`,
//!    `hooks.json` and the memory files stay out of reach of `write_file`, so an injected page
//!    cannot grant itself a capability by editing them. A definition is inert without
//!    `JOY_EXEC=1`: with exec off, none of these tools exist at all.
//!
//! The claim "a tool is a capability, and capabilities are switched on by a human" still holds,
//! with one honest amendment: what a model may *define* is now broader than what it may *run*,
//! and the ceiling is exactly `JOY_EXEC_ALLOW`. Writing the definition is therefore not gated
//! separately — an approval card that says "allow `pdf_extract`?" would be asking about the
//! wrong thing, since the answer that matters ("may `pdftotext` run?") is asked by the gate on
//! the first call anyway.
//!
//! Skill packs (the C half): `<home>/skills/<skill>/tools/*.json` are loaded the same way, so a
//! skill can ship "how to do X" and "the tool for X" together — the model creates the skill with
//! `define_tool { skill: … }` and the next turn has both.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::exec::ExecPolicy;
use crate::{Tool, ToolCtx, ToolRegistry};

/// Longest accepted tool name. The cap is about the schema budget, not taste: 40 characters is
/// what fits beside a description without pushing a discovered tool out of `find_tools`.
const NAME_MAX_CHARS: usize = 40;

/// Longest accepted description. Same reasoning as [`NAME_MAX_CHARS`]: a definition is a
/// summary, and a paragraph per tool is how a tool table stops fitting in a request.
const DESCRIPTION_MAX_CHARS: usize = 1024;

/// Longest accepted command template.
const COMMAND_MAX_CHARS: usize = 2000;

/// Timeout used when the definition does not say.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Longest accepted timeout. `JOY_EXEC_TIMEOUT` can still be lower — this is a ceiling on what a
/// *definition* may ask for, so one tool cannot park a turn for an hour.
const MAX_TIMEOUT_SECS: u64 = 300;

/// How many definitions one directory may hold. Past this the table is no longer readable by a
/// model, and "which of these 200 tools do I call" becomes a worse problem than the one the
/// definitions solved.
const MAX_DEFINITIONS: usize = 64;

/// A parsed, validated tool definition.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub command: String,
    pub timeout_secs: u64,
}

impl ToolSpec {
    /// The `{placeholders}` the command uses, in order of first appearance.
    pub fn placeholders(&self) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        let bytes = self.command.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'{' {
                index += 1;
                continue;
            }
            let Some(close) = self.command[index + 1..].find('}') else {
                break;
            };
            let name = &self.command[index + 1..index + 1 + close];
            let plausible = !name.is_empty()
                && name.len() <= NAME_MAX_CHARS
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if plausible && !found.iter().any(|seen| seen == name) {
                found.push(name.to_string());
            }
            index += close + 2;
        }
        found
    }
}

fn text_field(value: &Value, key: &str) -> Result<String, String> {
    match value.get(key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_string()),
        Some(Value::String(_)) | None => Err(format!("`{key}` is missing or empty")),
        Some(_) => Err(format!("`{key}` has to be a string")),
    }
}

/// Parse and validate one definition. Every error names the field and says what would be
/// accepted — a definition is written by a model, so the message is the next prompt.
pub fn parse(text: &str, source: &str) -> Result<ToolSpec, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("{source} is not valid JSON: {e}"))?;
    if !value.is_object() {
        return Err(format!("{source} has to be a JSON object"));
    }

    let name = text_field(&value, "name")?;
    let valid_name = name.len() <= NAME_MAX_CHARS
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid_name {
        return Err(format!(
            "the tool name {name:?} has to be lowercase letters, digits and underscores, at most \
             {NAME_MAX_CHARS} characters, starting with a letter (a name shows up in prompts and \
             logs, so it has to be one word)"
        ));
    }

    let description = text_field(&value, "description")?;
    if description.chars().count() > DESCRIPTION_MAX_CHARS {
        return Err(format!(
            "`description` is longer than {DESCRIPTION_MAX_CHARS} characters — this is the line the \
             model reads to decide whether to call the tool, not its documentation"
        ));
    }

    let input_schema = value
        .get("input_schema")
        .cloned()
        .ok_or_else(|| "`input_schema` is missing (a JSON Schema object describing the arguments; `{\"type\": \"object\", \"properties\": {}}` if it takes none)".to_string())?;
    if !input_schema.is_object()
        || input_schema.get("type").and_then(Value::as_str) != Some("object")
    {
        return Err(
            "`input_schema` has to be a JSON Schema object with \"type\": \"object\" — the model \
             fills named arguments, so the arguments are always an object"
                .to_string(),
        );
    }

    let command = text_field(&value, "command")?;
    if command.chars().count() > COMMAND_MAX_CHARS {
        return Err(format!(
            "`command` is longer than {COMMAND_MAX_CHARS} characters — put the long part in a script \
             and call the script"
        ));
    }

    let timeout_secs = match value.get("timeout_secs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_SECS,
        Some(item) => match item.as_u64() {
            Some(secs) if (1..=MAX_TIMEOUT_SECS).contains(&secs) => secs,
            _ => {
                return Err(format!(
                    "`timeout_secs` has to be a whole number from 1 to {MAX_TIMEOUT_SECS}"
                ));
            }
        },
    };

    let spec = ToolSpec {
        name,
        description,
        input_schema,
        command,
        timeout_secs,
    };

    // Every placeholder has to be a declared property. Without this the usual failure is a typo
    // that renders the literal text `{pth}` into a shell command — which either runs as garbage
    // or, worse, looks like it worked.
    let properties = spec
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for placeholder in spec.placeholders() {
        if !properties.contains_key(&placeholder) {
            let declared: Vec<&str> = properties.keys().map(String::as_str).collect();
            return Err(format!(
                "`command` uses {{{placeholder}}} but `input_schema.properties` does not declare it \
                 (declared: {})",
                if declared.is_empty() {
                    "none".to_string()
                } else {
                    declared.join(", ")
                }
            ));
        }
    }

    Ok(spec)
}

/// One directory's definitions, sorted by file name. Unreadable or invalid files are reported and
/// skipped: one broken definition must not take the working ones down with it.
pub fn load(dir: &Path) -> Vec<(ToolSpec, PathBuf)> {
    let Ok(reader) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = reader
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    let mut specs = Vec::new();
    for path in files {
        if specs.len() >= MAX_DEFINITIONS {
            eprintln!(
                "(joy) {} holds more than {MAX_DEFINITIONS} tool definitions; the rest are ignored",
                dir.display()
            );
            break;
        }
        let shown = path.display().to_string();
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("(joy) could not read the tool definition {shown}");
            continue;
        };
        match parse(&text, &shown) {
            Ok(spec) => specs.push((spec, path)),
            Err(why) => eprintln!("(joy) skipping the tool definition {shown}: {why}"),
        }
    }
    specs
}

/// Every definition the machine has: `<home>/tools/*.json` first, then each skill's own
/// `tools/*.json`. Duplicate names keep the first one — `tools/` is the user's own shelf, so it
/// wins over a pack, and a name collision is logged rather than silently resolved.
pub fn load_all(home: &Path) -> Vec<(ToolSpec, PathBuf)> {
    let mut specs = load(&home.join("tools"));
    let skills = home.join("skills");
    if let Ok(reader) = std::fs::read_dir(&skills) {
        let mut dirs: Vec<PathBuf> = reader
            .flatten()
            .map(|entry| entry.path().join("tools"))
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            for (spec, path) in load(&dir) {
                if specs.iter().any(|(seen, _)| seen.name == spec.name) {
                    eprintln!(
                        "(joy) the tool '{}' is defined twice ({} and {}); keeping the first",
                        spec.name,
                        specs[0].1.display(),
                        path.display()
                    );
                    continue;
                }
                specs.push((spec, path));
            }
        }
    }
    specs
}

/// Add every definition to `registry`. Names already taken are refused by
/// [`ToolRegistry::register`] — a definition may never displace a built-in.
///
/// Returns the names that made it into the table, so the caller can log one line instead of one
/// line per tool.
pub fn attach(registry: &mut ToolRegistry, home: &Path, policy: ExecPolicy) -> Vec<String> {
    let policy = Arc::new(policy);
    let mut added = Vec::new();
    for (spec, _) in load_all(home) {
        let before = registry.names().len();
        registry.register_exclusive(build(&spec, policy.clone()));
        // `register` refuses duplicates, so "did the table grow" is the only honest test of
        // whether this definition made it in.
        if registry.names().len() > before {
            added.push(spec.name);
        }
    }
    added
}

/// Render the command with this call's arguments.
///
/// Values are single-quoted and `'` is escaped, so an argument is always exactly one shell word.
/// Non-scalar values are refused rather than guessed at: turning a JSON array into shell words
/// is a decision the definition author did not make.
pub fn render(spec: &ToolSpec, args: &Value) -> Result<String, String> {
    let mut rendered = spec.command.clone();
    for placeholder in spec.placeholders() {
        let Some(value) = args.get(&placeholder) else {
            return Err(format!(
                "missing the argument `{placeholder}` (the command template needs it)"
            ));
        };
        let word = match value {
            Value::String(text) => shell_word(text),
            Value::Number(number) => shell_word(&number.to_string()),
            Value::Bool(flag) => shell_word(if *flag { "true" } else { "false" }),
            Value::Null => {
                return Err(format!(
                    "`{placeholder}` is null — pass a string, a number or a boolean"
                ));
            }
            Value::Array(_) | Value::Object(_) => {
                return Err(format!(
                    "`{placeholder}` is an array or an object; a command template can only take \
                     strings, numbers and booleans (join or format it yourself first)"
                ));
            }
        };
        rendered = rendered.replace(&format!("{{{placeholder}}}"), &word);
    }
    Ok(rendered)
}

/// One shell word: single-quoted, with `'` written the only way single quotes allow.
pub fn shell_word(value: &str) -> String {
    let mut word = String::with_capacity(value.len() + 2);
    word.push('\'');
    for character in value.chars() {
        if character == '\'' {
            word.push_str("'\\''");
        } else {
            word.push(character);
        }
    }
    word.push('\'');
    word
}

/// Turn a spec into the `Tool` the loop calls.
fn build(spec: &ToolSpec, policy: Arc<ExecPolicy>) -> Tool {
    let spec = spec.clone();
    Tool {
        name: spec.name.clone(),
        description: format!(
            "{} (a user-defined command template: it runs `{}` through the same sandbox, allowlist and approval as run_command)",
            spec.description, spec.command
        ),
        input_schema: spec.input_schema.clone(),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let spec = spec.clone();
            let policy = policy.clone();
            Box::pin(async move {
                let command = match render(&spec, &args) {
                    Ok(command) => command,
                    Err(why) => return Ok(format!("Error: {why}")),
                };
                // Working directory is this turn's, never an argument: a template that could
                // choose its own cwd would choose its own writable root too.
                let mut writable: Vec<PathBuf> = Vec::new();
                if let Some(cwd) = &ctx.cwd {
                    writable.push(cwd.clone());
                }

                let mut approval = ctx.approval.clone();
                if let Some(hooks) = &ctx.hooks {
                    let outcome = hooks
                        .fire(
                            crate::hooks::HookEvent::PermissionRequest,
                            json!({
                                "session_id": ctx.session_id,
                                "cwd": ctx.cwd,
                                "tool_name": spec.name,
                                "tool_input": args,
                            }),
                        )
                        .await;
                    if let Some(why) = outcome.blocked {
                        return Ok(format!(
                            "Error: {why} (a PermissionRequest hook refused this call)"
                        ));
                    }
                    if outcome.allowed {
                        approval = Some(Arc::new(crate::exec::AlreadyApproved));
                    }
                }

                let timeout_secs = policy.timeout_secs.max(1) as u64;
                let budget = crate::exec::ExecPolicy {
                    timeout_secs: spec
                        .timeout_secs
                        .min(timeout_secs)
                        .min(MAX_TIMEOUT_SECS) as i64,
                    ..(*policy).clone()
                };
                Ok(crate::exec::execute(
                    &command,
                    &budget,
                    &writable,
                    approval.as_deref(),
                    ctx.on_delta.clone(),
                )
                .await)
            })
        }),
    }
}

pub mod define;

pub use define::define_tool;
