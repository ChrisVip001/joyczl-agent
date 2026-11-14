//! File management: `delete_file` and `move_file`.
//!
//! Split out of `fs_write.rs` when that module passed the point where "what does this file do"
//! was answerable at a glance. The division is by question, not by size: `fs_write` changes a
//! file's **contents**, this module changes whether and where the file **exists**. Both go
//! through the same admission (`fs_write::admit` → roots → gate), because a delete and a move
//! are writes with a wider blast radius, not a different kind of action.
//!
//! Two deliberate refusals live here:
//!
//! * `delete_file` never removes a directory. `rm -rf` is exactly the one call the boundary
//!   exists to make impossible; a tree has to go through `run_command`, with its own gate.
//! * `move_file` refuses an existing destination instead of overwriting, and gates **both**
//!   ends: moving a file out of the writable area would otherwise be a way to write outside it.

use std::sync::Arc;

use anyhow::anyhow;
use serde_json::{json, Value};

use crate::fs_write::{admit, pass_write_gate, permission_hook, WritePolicy};
use crate::{require_str, Tool, ToolCtx};

/// `delete_file`: remove one file, inside the same boundary and through the same gate as
/// `write_file`.
///
/// The gap this fills is narrow but real: writing was one-way. A model could create a scratch
/// file and then had no way to clean it up — the only route was `run_command rm`, which means
/// `JOY_EXEC`, which is off in the configuration most people run. "Please delete that file" got
/// the answer "I have no tool for deleting", which is true and embarrassing.
///
/// Directories are refused on purpose: `rm -r` is exactly the kind of one-call disaster the
/// boundary exists to prevent, and a directory tree is what `run_command` (with its approval
/// path) is for.
pub fn delete_file(policy: WritePolicy) -> Tool {
    let policy = Arc::new(policy);
    Tool {
        name: "delete_file".to_string(),
        description: "Delete one file (not a directory). Same boundary and approval as write_file: only inside the working directory and JOY_EXEC_WRITABLE_ROOTS. Use it to clean up scratch files; for a whole directory, run_command is the tool that can ask properly."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file to delete. ~ is expanded."}
            },
            "required": ["path"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let policy = policy.clone();
            Box::pin(async move {
                let raw = require_str(&args, "path")?;
                let path = crate::fs::resolve_against_cwd(&ctx, crate::fs::expand_tilde(&raw));

                if let Err(why) = admit(
                    &ctx,
                    "delete_file",
                    "delete",
                    &path,
                    &raw,
                    &policy,
                    json!({ "path": raw }),
                )
                .await
                {
                    return Ok(format!("Error: {why}"));
                }

                match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_dir() => Ok(format!(
                        "Error: {path:?} is a directory — delete_file only removes files. Use run_command when a whole tree has to go."
                    )),
                    Ok(meta) => {
                        let size = meta.len();
                        std::fs::remove_file(&path)
                            .map_err(|e| anyhow!("Could not delete {path:?}: {e}"))?;
                        Ok(format!("Deleted {path:?} ({size} bytes)"))
                    }
                    Err(_) => Ok(format!(
                        "Error: {path:?} does not exist, so there is nothing to delete."
                    )),
                }
            })
        }),
    }
}

/// `move_file`: rename within the boundary — the same gate as `write_file`, on **both** ends.
///
/// Two gates because a move is a delete plus a write: an approved destination must not turn an
/// unapproved source into a moveable file, or the boundary would have a hole exactly the shape
/// of "move it out of the control plane".
///
/// An existing destination is refused rather than overwritten: `mv` quietly replacing a file the
/// model never read is the kind of success nobody notices until the content is gone.
pub fn move_file(policy: WritePolicy) -> Tool {
    let policy = Arc::new(policy);
    Tool {
        name: "move_file".to_string(),
        description: "Move or rename one file. Both ends must be inside the working directory and JOY_EXEC_WRITABLE_ROOTS, and an existing destination is refused (delete it first if that is really what you mean). Missing parent directories of the destination are created."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "from": {"type": "string", "description": "The current path. ~ is expanded."},
                "to": {"type": "string", "description": "The new path. ~ is expanded; missing parents are created."}
            },
            "required": ["from", "to"]
        }),
        handler: Arc::new(move |ctx: ToolCtx, args: Value| {
            let policy = policy.clone();
            Box::pin(async move {
                let from_raw = require_str(&args, "from")?;
                let to_raw = require_str(&args, "to")?;
                let from = crate::fs::resolve_against_cwd(&ctx, crate::fs::expand_tilde(&from_raw));
                let to = crate::fs::resolve_against_cwd(&ctx, crate::fs::expand_tilde(&to_raw));

                let (approval, blocked) = permission_hook(
                    &ctx,
                    "move_file",
                    json!({ "from": from_raw, "to": to_raw }),
                )
                .await;
                if let Some(why) = blocked {
                    return Ok(format!(
                        "Error: {why} (a PermissionRequest hook refused this move)"
                    ));
                }
                let roots = crate::roots::workspace_roots(&ctx, &policy.extra_roots).workspaces;
                if let Err(why) = pass_write_gate(
                    "move_file",
                    &from,
                    &from_raw,
                    &policy,
                    &roots,
                    approval.clone(),
                )
                .await
                {
                    return Ok(format!("Error: the source is out of bounds — {why}"));
                }
                if let Err(why) =
                    pass_write_gate("move_file", &to, &to_raw, &policy, &roots, approval).await
                {
                    return Ok(format!("Error: the destination is out of bounds — {why}"));
                }

                if !from.is_file() {
                    return Ok(format!(
                        "Error: {from:?} is not a file (or does not exist), so there is nothing to move."
                    ));
                }
                if to.exists() {
                    return Ok(format!(
                        "Error: {to:?} already exists — delete it first, or pick another name. Nothing was moved."
                    ));
                }
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| anyhow!("Could not create the parent directory {parent:?}: {e}"))?;
                }
                let size = std::fs::metadata(&from).map(|meta| meta.len()).unwrap_or(0);
                if let Err(rename_error) = std::fs::rename(&from, &to) {
                    // Across filesystems `rename` fails with EXDEV; copy then delete is the only
                    // way left, and it is the same operation with one more step to get wrong.
                    std::fs::copy(&from, &to).map_err(|copy_error| {
                        anyhow!(
                            "Could not move {from:?} to {to:?}: {rename_error} (and copying failed too: {copy_error})"
                        )
                    })?;
                    std::fs::remove_file(&from)
                        .map_err(|e| anyhow!("Copied to {to:?} but could not remove {from:?}: {e}"))?;
                }
                Ok(format!("Moved {from:?} → {to:?} ({size} bytes)"))
            })
        }),
    }
}
