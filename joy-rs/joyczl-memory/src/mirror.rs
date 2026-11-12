//! Mirroring facts into the plaintext bundle — one page per subject.
//!
//! The database is the index; `<home>/memory/` is the library a human opens. After every
//! distillation the two are reconciled here, with one rule that outranks convenience:
//! **a page a human edited is not overwritten.** We know the page we wrote (its digest is
//! stored on the facts), so a different digest means someone opened the file and changed
//! it — their edit wins, the mirror counts it as skipped, and stderr says which file.
//!
//! Not pruning is deliberate for now: a page whose facts were all forgotten stays until
//! the wiki layer (stage 2) learns to retire pages with their sources. A stale page is a
//! smaller problem than a page that vanishes under a human's cursor.

use anyhow::Result;
use joyczl_state::Facts;

use crate::bundle::{Bundle, Page};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorReport {
    pub written: usize,
    /// Pages left alone because a human had edited them.
    pub skipped: usize,
    /// Subjects whose facts are gone (pages left in place, see the module note).
    pub orphaned: usize,
}

/// Reconcile every subject's facts into one page.
pub async fn mirror_facts(facts: &Facts, bundle: &Bundle) -> Result<MirrorReport> {
    bundle.ensure()?;
    let mut report = MirrorReport::default();

    for (subject, count) in facts.subjects().await? {
        if count <= 0 {
            continue;
        }
        let rows = facts.for_subject(&subject).await?;
        if rows.is_empty() {
            report.orphaned += 1;
            continue;
        }

        // The page as we last wrote it: every fact on it carries the same digest.
        let mut expected_digest: Option<String> = None;
        for row in &rows {
            if let Some(meta) = facts.meta(row.id).await? {
                if meta.page_path.is_some() {
                    expected_digest = meta.digest.clone();
                    break;
                }
            }
        }
        let slug = slug_of(&subject);
        if let Some(existing) = bundle.read(&slug)? {
            let untouched = expected_digest
                .as_deref()
                .map(|digest| digest == existing.digest)
                // No recorded digest but a file exists: it was not ours to begin with.
                .unwrap_or(true);
            if !untouched {
                report.skipped += 1;
                eprintln!(
                    "(joy) 记忆页 {} 被手动改过，这次不覆盖（要覆盖就删掉它，下一轮会重新生成）",
                    bundle.page_path(&slug)?.display()
                );
                continue;
            }
        }

        let mut page = Page::new(
            slug.clone(),
            "fact",
            subject.clone(),
            subject.clone(),
            summary_of(&rows),
            render_body(&subject, &rows),
        );
        page.sources = rows
            .iter()
            .map(|row| format!("fact:{}", row.id))
            .collect::<Vec<_>>();
        bundle.write(&page)?;

        for row in &rows {
            facts
                .set_page(row.id, &relative_path(bundle, &slug), &page.digest)
                .await?;
        }
        report.written += 1;
    }
    Ok(report)
}

fn slug_of(subject: &str) -> String {
    let mut slug: String = subject
        .to_lowercase()
        .chars()
        .map(|c| {
            // Unicode letters survive: `张伟` becomes `张伟.md` (the page a human would name
            // themselves), and the hash fallback is only for themes with no letters at all.
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        // Chinese subjects have no ASCII left after the mapping: fall back to a stable id
        // derived from the bytes, so the page still exists and still sorts.
        return format!("subject-{:x}", md5_like(subject));
    }
    trimmed.chars().take(64).collect()
}

/// A tiny stable hash for slug fallbacks (FNV-1a): nothing here is security-relevant, and
/// pulling a hash crate in for one integer would not pay for itself.
fn md5_like(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce4_84222325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn relative_path(bundle: &Bundle, slug: &str) -> String {
    let dir = bundle
        .root()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("memory");
    format!("{dir}/{slug}.md")
}

fn summary_of(rows: &[joyczl_state::FactRow]) -> String {
    let first = rows[0].content.trim();
    let mut summary: String = first.chars().take(60).collect();
    if rows.len() > 1 {
        summary.push_str(&format!("…（共 {} 条）", rows.len()));
    }
    summary
}

fn render_body(subject: &str, rows: &[joyczl_state::FactRow]) -> String {
    let mut out = format!("# {subject}\n\n");
    for row in rows {
        out.push_str(&format!("- {}\n", row.content.trim()));
    }
    out.push_str("\n## 出处\n\n");
    for row in rows {
        out.push_str(&format!(
            "- fact:{} · {} · {}\n",
            row.id,
            row.source,
            row.created_at.as_deref().unwrap_or("-")
        ));
    }
    out
}

#[cfg(test)]
#[path = "mirror_tests.rs"]
mod mirror_tests;
