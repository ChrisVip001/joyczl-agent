//! The one definition of "where Joy may put bytes".
//!
//! Two consumers, one rule: the in-process write tools (`write_file` / `edit`) and the
//! OS sandbox that `run_command` builds (`(allow file-write*)` on macOS, `--bind` on Linux).
//!
//! They used to disagree: the write tools refused `<home>` while the shell got it. That made
//! the shell a way around the write gate, and worse — it let a model rewrite `settings.json`
//! (the exec allow table) or `hooks.json` (hooks run arbitrary commands) from inside the
//! sandbox, i.e. grant itself more than it had. One definition removes the disagreement.
//!
//! The rule: **the workspace this turn is bound to, plus `JOY_EXEC_WRITABLE_ROOTS`**.
//! `<home>` is not one of them. Joy's own state (`outbox`, `spill`, `jobs`, `state.db`, …) is
//! written in-process by the app-server, never by a sandboxed command, so nothing downstream
//! needs it; a human who does need a shell to write somewhere under home names that directory
//! explicitly. Two different kinds of input, two different checks:
//!
//! * the **chosen workspace** is a human's decision (they picked the folder, or the session
//!   remembers it). It is honoured even when it happens to be `<home>` — a folder picker that
//!   says "this one" is an authorization, not a typo.
//! * **extra roots** come from a config string. A typo there must not become a privilege, so
//!   `<home>`, any ancestor of it, and every entry of [`HOME_CONTROL`] are dropped.
//! * neither may be the disk root: `--bind /` hands back everything, control plane included.

use std::path::{Path, PathBuf};

use crate::ToolCtx;

/// Entries under `<home>` that are the control plane. Never writable through an extra root,
/// even when a human lists `<home>` (or one of these) in `JOY_EXEC_WRITABLE_ROOTS`.
///
/// The reason is uniform: writing any of them changes what Joy will do next, not what the
/// file says — `settings.json` is the exec allow table, `hooks.json` runs commands on
/// lifecycle events, `mcp.json`/`connectors.json` decide which servers get launched,
/// `models.json`/`secrets.json` decide which account is used.
pub const HOME_CONTROL: &[&str] = &[
    "settings.json",
    "hooks.json",
    "mcp.json",
    "models.json",
    "models-catalog.json",
    "model-routes.json",
    "secrets.json",
    "connectors.json",
    "webhooks.json",
    "notify.json",
    "schedules.json",
    "schedules-state.json",
    "workspaces.json",
    "state.db",
    "connector-auth",
    "skills",
    "memory",
    "traces",
    "ledger",
    "usage.jsonl",
    "skill-hits.jsonl",
    "schedule-runs.jsonl",
    "eval_runs.jsonl",
];

/// The roots one call may write, plus what was refused and why (for the startup log).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteRoots {
    pub workspaces: Vec<PathBuf>,
    /// Roots dropped because they would hand back the control plane. Empty in the normal case;
    /// non-empty means someone configured something dangerous and should be told.
    pub dropped: Vec<PathBuf>,
}

impl WriteRoots {
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }
}

/// Roots for the in-process write tools: the chosen workspace + extra roots.
pub fn workspace_roots(ctx: &ToolCtx, extra: &[PathBuf]) -> WriteRoots {
    let dynamic: Vec<PathBuf> = ctx.cwd.iter().cloned().collect();
    resolve_roots(&dynamic, extra, &ctx.home)
}

/// Roots for the exec sandbox. Same rule as [`workspace_roots`]: `cwd` is the resolved
/// working directory for this call (`args.cwd` > the turn's cwd), never `<home>`.
pub fn sandbox_roots(cwd: Option<&Path>, extra: &[PathBuf], home: &Path) -> WriteRoots {
    let dynamic: Vec<PathBuf> = cwd.into_iter().map(Path::to_path_buf).collect();
    resolve_roots(&dynamic, extra, home)
}

/// The rule itself. `dynamic` is what the call decided (this turn's working directory);
/// `extra` is what the config says (`JOY_EXEC_WRITABLE_ROOTS`). The two are checked
/// differently on purpose — see the module docs.
pub fn resolve_roots(dynamic: &[PathBuf], extra: &[PathBuf], home: &Path) -> WriteRoots {
    let mut workspaces: Vec<PathBuf> = Vec::with_capacity(dynamic.len() + extra.len());
    let mut dropped: Vec<PathBuf> = Vec::new();

    for path in dynamic {
        // Even a human's chosen folder should never be the whole disk: there is no
        // "workspace" left to be inside of.
        if is_disk_root(path) {
            dropped.push(path.clone());
        } else {
            push_unique(&mut workspaces, path);
        }
    }
    for root in extra {
        if is_disk_root(root) || is_control(root, home) {
            dropped.push(root.clone());
        } else {
            push_unique(&mut workspaces, root);
        }
    }

    WriteRoots {
        workspaces,
        dropped,
    }
}

fn push_unique(roots: &mut Vec<PathBuf>, path: &Path) {
    let real = resolve(path);
    if roots.iter().any(|kept| resolve(kept) == real) {
        return;
    }
    roots.push(path.to_path_buf());
}

/// Is this path the control plane? True for `<home>` itself, any ancestor of it, and every
/// [`HOME_CONTROL`] entry (or something inside one).
fn is_control(path: &Path, home: &Path) -> bool {
    let real_path = resolve(path);
    if real_path == Path::new("/") {
        return true;
    }
    let real_home = resolve(home);
    if real_home.as_os_str().is_empty() {
        return false;
    }
    if real_path == real_home || real_home.starts_with(&real_path) {
        return true;
    }
    HOME_CONTROL.iter().any(|entry| {
        let control = real_home.join(entry);
        real_path == control || real_path.starts_with(&control)
    })
}

fn is_disk_root(path: &Path) -> bool {
    path == Path::new("/") || resolve(path) == Path::new("/")
}

/// The real path, **including for a file that does not exist yet**: resolve the parent
/// recursively and append the last component. Plain `canonicalize` fails on a missing file,
/// and falling back to the literal text would compare `/var/…` against `/private/var/…` and
/// let a control-plane path through on macOS.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => resolve(parent).join(name),
        _ => path.to_path_buf(),
    }
}
