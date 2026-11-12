//! The plaintext memory library: one directory = one knowledge base.
//!
//! `<home>/memory/` is the authoritative store for what Joy believes — a folder of
//! Markdown pages a human can open, edit and `git diff`, in the OKF shape:
//!
//! ```text
//! memory/
//!   index.md        the catalog: one line per page (regenerated, never hand-edited)
//!   changelog.md    append-only: what was written, when, with which digest
//!   alex.md         one page per concept, header declares its type
//! ```
//!
//! Two rules that make the directory trustworthy:
//!
//! 1. **Files win.** SQLite is a rebuildable index; if a page is edited by hand, the
//!    body digest no longer matches the row and the next reindex takes the file's word.
//! 2. **Every write is atomic and logged.** `tmp` + `rename` (a crash mid-write leaves
//!    the old page, never half of two), then a changelog line — an append-only trail
//!    that survives even if the index is thrown away.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest as _, Sha256};

use crate::frontmatter;

/// What a page can be about. A closed set on purpose: an open `type` is a field nobody
/// can group by, and the memory center sorts by exactly this.
pub const KINDS: [&str; 5] = ["fact", "person", "project", "reference", "concept"];

/// The catalog and the trail are files, not pages.
pub const INDEX_FILE: &str = "index.md";
pub const CHANGELOG_FILE: &str = "changelog.md";

fn kind_of(raw: &str) -> &'static str {
    let lowered = raw.trim().to_lowercase();
    KINDS
        .iter()
        .find(|known| **known == lowered)
        .copied()
        .unwrap_or("concept")
}

/// sha256 of the body, lowercase hex — the same shape the ledger uses for its rows.
pub fn digest_of(body: &str) -> String {
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The digest of a page body, normalised the same way on the way in and the way out.
fn body_digest(body: &str) -> String {
    digest_of(body.trim())
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// One memory page: header fields over a Markdown body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// File stem, and the handle the index links to.
    pub slug: String,
    pub kind: String,
    pub title: String,
    /// The memory subject this page belongs to (`alex`, `joyczl-agent`).
    pub subject: String,
    /// One line for the catalog — the only part a new session sees by default.
    pub summary: String,
    /// Where this came from (`ledger:<session>/<turn>`), so a claim can be traced back.
    pub sources: Vec<String>,
    pub body: String,
    pub updated: String,
    pub digest: String,
}

impl Page {
    pub fn new(
        slug: impl Into<String>,
        kind: &str,
        title: impl Into<String>,
        subject: impl Into<String>,
        summary: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        // Hash the *trimmed* body: `render` trims before writing and `parse` trims after
        // reading, so hashing the raw string would make every page look hand-edited on the
        // next pass (that bug cost an afternoon — the mirror skipped all its own files).
        let body = body.into();
        let digest = body_digest(&body);
        Self {
            slug: slug.into(),
            kind: kind_of(kind).to_string(),
            title: title.into(),
            subject: subject.into(),
            summary: summary.into(),
            sources: Vec::new(),
            body,
            updated: now(),
            digest,
        }
    }

    /// Recompute `digest`/`updated` after mutating `body` — the caller's one obligation.
    pub fn touch(&mut self) {
        self.digest = body_digest(&self.body);
        self.updated = now();
    }

    fn render(&self) -> String {
        frontmatter::render(
            &[
                ("type", self.kind.clone()),
                ("title", self.title.clone()),
                ("subject", self.subject.clone()),
                ("summary", self.summary.clone()),
                ("sources", self.sources.join(", ")),
                ("updated", self.updated.clone()),
                ("digest", self.digest.clone()),
            ],
            &self.body,
        )
    }

    fn parse(slug: String, text: &str) -> Option<Self> {
        let parsed = frontmatter::split(text)?;
        let body = parsed.body.trim().to_string();
        Some(Self {
            kind: kind_of(parsed.get("type").unwrap_or("concept")).to_string(),
            title: parsed.get("title")?.to_string(),
            subject: parsed.get("subject").unwrap_or(&slug).to_string(),
            summary: parsed.get("summary").unwrap_or("").to_string(),
            sources: parsed.list("sources"),
            updated: parsed.get("updated").unwrap_or("").to_string(),
            // A hand-edited body has a stale header digest: recompute it from the body so
            // "the file wins" is true even when nobody ran a reindex.
            digest: body_digest(&body),
            body,
            slug,
        })
    }
}

/// A memory bundle rooted at `<home>/memory`.
#[derive(Debug, Clone)]
pub struct Bundle {
    root: PathBuf,
}

impl Bundle {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join("memory"),
        }
    }

    /// For tests and for `JOY_MEMORY_DIR`-style overrides: point at the directory itself.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the directory and the two special files if they are missing. Idempotent:
    /// called on boot, on every write, and by the memory center's first read.
    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create the memory catalog at {}", self.root.display()))?;
        if !self.root.join(CHANGELOG_FILE).exists() {
            self.write_atomic(
                &self.root.join(CHANGELOG_FILE),
                "# Change log\n\nOne line per write, append-only.\n".as_bytes(),
            )?;
        }
        if !self.root.join(INDEX_FILE).exists() {
            self.rewrite_index()?;
        }
        Ok(())
    }

    /// Every page, sorted by kind then slug — stable order keeps `index.md` diffs small.
    pub fn pages(&self) -> Result<Vec<Page>> {
        let mut pages = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(pages),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.root.display()))
            }
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let Some(slug) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let lowered = slug.to_lowercase();
            if lowered == "index" || lowered == "changelog" {
                continue;
            }
            // A page we cannot parse is skipped, not fatal: one broken hand-edit must not
            // take the whole library away from the model.
            if let Ok(text) = fs::read_to_string(&path) {
                if let Some(page) = Page::parse(slug.to_string(), &text) {
                    pages.push(page);
                }
            }
        }
        pages.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.slug.cmp(&b.slug)));
        Ok(pages)
    }

    /// One page by slug; `Ok(None)` when it does not exist (not an error: "no such page"
    /// is an answer the model should read).
    pub fn read(&self, slug: &str) -> Result<Option<Page>> {
        let path = self.page_path(slug)?;
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Page::parse(slug.to_string(), &text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Write a page: atomic replace, one changelog line, regenerated catalog.
    pub fn write(&self, page: &Page) -> Result<()> {
        let path = self.page_path(&page.slug)?;
        fs::create_dir_all(&self.root)?;
        self.write_atomic(&path, page.render().as_bytes())?;
        self.changelog_append(&format!(
            "write {} {} digest={}",
            page.slug, page.kind, page.digest
        ))?;
        self.rewrite_index()?;
        Ok(())
    }

    /// Delete a page. `Ok(false)` when it was not there to begin with.
    pub fn remove(&self, slug: &str) -> Result<bool> {
        let path = self.page_path(slug)?;
        match fs::remove_file(&path) {
            Ok(()) => {
                self.changelog_append(&format!("remove {slug}"))?;
                self.rewrite_index()?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("delete {}", path.display())),
        }
    }

    /// The catalog: what a new session may carry. Regenerated from the pages, so it can
    /// never disagree with them.
    pub fn rewrite_index(&self) -> Result<()> {
        let pages = self.pages()?;
        let mut out = String::from("# Memory catalog\n\n");
        out.push_str(&format!(
            "{} pages · last updated {}\n\n",
            pages.len(),
            now()
        ));
        for page in &pages {
            let summary = if page.summary.trim().is_empty() {
                String::new()
            } else {
                format!(" — {}", page.summary.trim())
            };
            out.push_str(&format!(
                "- [{}]({}.md) · {}{}\n",
                page.title.trim(),
                page.slug,
                page.kind,
                summary
            ));
        }
        fs::create_dir_all(&self.root)?;
        self.write_atomic(&self.root.join(INDEX_FILE), out.as_bytes())
    }

    /// The catalog text as it stands on disk (what gets injected into the prompt).
    pub fn index_text(&self) -> Result<String> {
        match fs::read_to_string(self.root.join(INDEX_FILE)) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error).context("read the memory catalog"),
        }
    }

    /// One line in the trail. Append-only; a failure to log never undoes a write.
    pub fn changelog_append(&self, line: &str) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(CHANGELOG_FILE))
            .context("open the change log")?;
        writeln!(file, "- {} {}", now(), line).context("write the change log")?;
        Ok(())
    }

    /// Where a slug lives, refusing anything that could escape the bundle.
    ///
    /// The slug is a *file name*, so it is validated rather than sanitised: silently
    /// rewriting `../../etc/passwd` into `etc-passwd` would make a traversal look like a
    /// successful write.
    pub fn page_path(&self, slug: &str) -> Result<PathBuf> {
        let slug = slug.trim();
        if slug.is_empty() || slug.chars().count() > 64 {
            anyhow::bail!("a page name must be 1-64 characters");
        }
        // Case-insensitive: on macOS `Index.md` and `index.md` are the same file, so the
        // catalog would be silently overwritten by a page.
        let lowered = slug.to_lowercase();
        if lowered == "index" || lowered == "changelog" {
            anyhow::bail!("`{slug}` is a reserved file name");
        }
        // Unicode letters are allowed on purpose: a page about 张伟 should be `张伟.md`, not
        // a hash. What stays forbidden is everything that could be a path (`/`, `\\`, `.`)
        // or a shell surprise (whitespace).
        let ok = slug
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
        if !ok {
            anyhow::bail!("a page name may only use letters, digits, `-` and `_` (got `{slug}`)");
        }
        Ok(self.root.join(format!("{slug}.md")))
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("spill {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "bundle_tests.rs"]
mod bundle_tests;
