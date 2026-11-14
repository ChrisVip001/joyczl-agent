//! Files HTTP endpoints.

use super::failed;
use crate::AppServer;
use axum::extract::{Extension, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use chrono::Local;
use joyczl_protocol::{codes, methods, ConfigReadResponse, ErrorObject};
use serde_json::json;
use std::sync::Arc;

/// Cap for a single uploaded file: 20MB.
pub(super) const UPLOAD_MAX_BYTES: usize = 20 * 1024 * 1024;

/// Extension whitelist: 1-8 alphanumeric chars; no double extensions or weird chars.
pub(super) fn safe_extension(name: &str) -> Option<String> {
    let ext = std::path::Path::new(name).extension()?.to_str()?;
    let ok = !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then(|| ext.to_ascii_lowercase())
}

/// `POST /api/upload?name=<original filename>` — body is the raw file bytes.
///
/// octet-stream instead of multipart: a single file needs no multipart envelope, and it
/// avoids the parser, a security minefield (zero features, zero transitive deps). The
/// on-disk name is **generated entirely server-side** (spill's discipline: user input
/// never enters a path); only the extension survives, through the whitelist. Returns a
/// home-relative path — the frontend injects it; the model reads it via read_file itself.
pub async fn upload(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > UPLOAD_MAX_BYTES {
        return failed(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: format!("the file is over the 20MB limit ({} bytes)", body.len()),
                data: None,
            },
        );
    }
    if body.is_empty() {
        return failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: "Empty files are not accepted.".into(),
                data: None,
            },
        );
    }
    let ext = params
        .get("name")
        .and_then(|n| safe_extension(n))
        .unwrap_or_else(|| "bin".to_string());
    // Home comes from the protocol (AppServer is a subprocess handle, no local field) —
    // upload is rare; an extra round trip keeps ops "protocol-only" discipline intact.
    let config: ConfigReadResponse = match app.request(methods::CONFIG_READ, json!({})).await {
        Ok(config) => config,
        Err(error) => return failed(StatusCode::BAD_GATEWAY, error),
    };
    let home = std::path::PathBuf::from(config.config.home);
    let now = Local::now();
    let dir = home.join("uploads").join(now.format("%Y%m%d").to_string());
    if let Err(why) = tokio::fs::create_dir_all(&dir).await {
        return failed(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("could not create the directory: {why}"),
                data: None,
            },
        );
    }
    // Only server-generated parts: timestamp + nanosecond suffix — no collisions, no overwrite.
    let name = format!(
        "file-{}-{:x}.{ext}",
        now.format("%H%M%S"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    let full = dir.join(&name);
    if let Err(why) = tokio::fs::write(&full, &body).await {
        return failed(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorObject {
                code: codes::INTERNAL_ERROR,
                message: format!("could not write the file: {why}"),
                data: None,
            },
        );
    }
    let date = now.format("%Y%m%d");
    Json(json!({ "path": format!("uploads/{date}/{name}") })).into_response()
}

/// The directory-browsing response. An ops-local UI view (as with /api/upload's response):
/// not in the protocol — neither the SDK nor the terminal needs "browse server directories".
#[derive(serde::Serialize)]
pub struct FsListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<FsEntry>,
}

#[derive(serde::Serialize)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
}

/// List the **subdirectories** of a directory (read-only, dirs only — picking a workdir has
/// no use for files). A pure function so it can be unit-tested without booting an app-server.
pub fn list_dirs(raw: &str, include_hidden: bool) -> Result<FsListing, String> {
    let path = std::path::PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(format!("'{raw}' must be an absolute path"));
    }
    if !path.is_dir() {
        return Err(format!("'{raw}' does not exist or is not a directory"));
    }
    let mut entries: Vec<FsEntry> = std::fs::read_dir(&path)
        .map_err(|why| format!("could not read {raw}: {why}"))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let is_dir = entry.file_type().ok()?.is_dir();
            if !is_dir {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Dot-prefixed entries hidden by default, shown only when the switch is on;
            // build noise (node_modules/target) hidden regardless — stepping in is pointless.
            if (name.starts_with('.') && !include_hidden)
                || name == "node_modules"
                || name == "target"
            {
                return None;
            }
            Some(FsEntry {
                path: entry.path().to_string_lossy().to_string(),
                name,
            })
        })
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    Ok(FsListing {
        path: path.to_string_lossy().to_string(),
        parent: path
            .parent()
            .filter(|p| p.as_os_str() != path.as_os_str())
            .map(|p| p.to_string_lossy().to_string()),
        entries,
    })
}

/// Where browsing starts: the user's home directory, falling back to process cwd.
pub(super) fn default_browse_root() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .or_else(|_| std::env::current_dir().map(|p| p.to_string_lossy().to_string()))
        .unwrap_or_else(|_| "/".to_string())
}

/// `GET /api/fs?path=<dir>` — server-side directory browsing (dashboard picks a workdir).
///
/// Why server-side: the browser folder picker **cannot get absolute paths** (the File
/// System Access API gives handles, not paths), while a workdir needs exactly an absolute
/// path — so "pick a folder like a desktop app" falls to the filesystem-aware side (the
/// Jupyter / VS Code server approach). Read-only, directories only.
pub async fn fs_browse(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let raw: String = params
        .get("path")
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_browse_root);
    let include_hidden = params
        .get("hidden")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    match list_dirs(&raw, include_hidden) {
        Ok(listing) => Json(listing).into_response(),
        Err(why) => failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: why,
                data: None,
            },
        ),
    }
}

/// Create a subdirectory under parent, returning its absolute path. Pure fn for unit tests.
///
/// Same name discipline as upload's extension whitelist: **user input never
/// enters the path shape** — no separators, no `..`, no dot prefix (a created
/// directory should not be invisible by default), capped at 64 chars.
pub fn create_subdir(parent_raw: &str, name: &str) -> Result<String, String> {
    let parent = std::path::PathBuf::from(parent_raw);
    if !parent.is_absolute() {
        return Err(format!("parent '{parent_raw}' must be an absolute path"));
    }
    if !parent.is_dir() {
        return Err(format!(
            "parent '{parent_raw}' does not exist or is not a directory"
        ));
    }
    let name = name.trim();
    let bad = |why: &str| format!("directory name '{name}' is not valid: {why}");
    if name.is_empty() || name.len() > 64 {
        return Err(bad("1-64 characters"));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.') {
        return Err(bad(
            "no path shapes allowed (separators, .., a leading dot)",
        ));
    }
    let target = parent.join(name);
    if target.exists() {
        return Err(format!("'{name}' already exists under {parent_raw}"));
    }
    std::fs::create_dir(&target).map_err(|why| format!("could not create it: {why}"))?;
    Ok(target.to_string_lossy().to_string())
}

/// `POST /api/fs/mkdir?path=<parent>&name=<name>` — the picker's "new folder" button; the
/// only write action besides read-only browsing. Names go through the whitelist.
pub async fn fs_mkdir(Query(params): Query<std::collections::HashMap<String, String>>) -> Response {
    let parent = params.get("path").cloned().unwrap_or_default();
    let name = params.get("name").cloned().unwrap_or_default();
    match create_subdir(&parent, &name) {
        Ok(created) => Json(json!({ "path": created })).into_response(),
        Err(why) => failed(
            StatusCode::BAD_REQUEST,
            ErrorObject {
                code: codes::INVALID_PARAMS,
                message: why,
                data: None,
            },
        ),
    }
}

/// The "recents" response (ops-local view, same treatment as /api/fs).
pub(super) fn recents_response(home: &std::path::Path) -> Response {
    Json(json!({
        "paths": joyczl_config::workspaces::load_workspaces(home).paths,
    }))
    .into_response()
}

/// `GET /api/fs/recents` — recently used directories (quick entry for picking a workdir).
pub async fn fs_recents(Extension(app): Extension<Arc<AppServer>>) -> Response {
    let config: ConfigReadResponse = match app.request(methods::CONFIG_READ, json!({})).await {
        Ok(config) => config,
        Err(error) => return failed(StatusCode::BAD_GATEWAY, error),
    };
    recents_response(std::path::Path::new(&config.config.home))
}

/// `POST /api/fs/recents?path=<dir>` — record a picker choice (recorded even before a turn
/// runs: the user's intent is already expressed here).
pub async fn fs_recents_record(
    Extension(app): Extension<Arc<AppServer>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let config: ConfigReadResponse = match app.request(methods::CONFIG_READ, json!({})).await {
        Ok(config) => config,
        Err(error) => return failed(StatusCode::BAD_GATEWAY, error),
    };
    let home = std::path::PathBuf::from(config.config.home);
    if let Some(path) = params
        .get("path")
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        joyczl_config::workspaces::record_workspace(&home, path);
    }
    recents_response(&home)
}
