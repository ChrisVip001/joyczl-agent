//! One reader for the two file formats this crate owns: `SKILL.md` and memory pages.
//!
//! Both are `---`-delimited `key: value` headers over a Markdown body — the OKF shape.
//! Two parsers would drift (one accepting CRLF, the other not; one trimming quotes, the
//! other not), and the drift would show up as "my page disappeared" weeks later. So the
//! splitting rules live here once, and each format reads its own keys out of the map.

use std::collections::BTreeMap;

/// A parsed file: the header fields and the body that follows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub fields: BTreeMap<String, String>,
    pub body: String,
}

impl Frontmatter {
    /// The trimmed value, or `None` when absent **or empty** — a bare `schedule:` is
    /// not a schedule, and treating it as one has bitten this project before.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    }

    /// `true` unless the key is explicitly false-ish. Absent means yes: a format where
    /// silence disables a feature is a format people misread.
    pub fn flag(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            None => default,
            Some(value) => !matches!(value.to_lowercase().as_str(), "false" | "no" | "0"),
        }
    }

    /// A comma- or space-separated list; empty entries dropped.
    pub fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .map(|value| {
                value
                    .split([',', ' '])
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Split `---`-delimited frontmatter from its body.
///
/// CRLF is normalised first: files edited on Windows (or checked out by git there) use
/// `\r\n`, and hard-matching `---\n` fails all of them — a CI Windows job caught this
/// once already. Returns `None` when the file has no header at all.
pub fn split(text: &str) -> Option<Frontmatter> {
    let text = text.replace("\r\n", "\n");
    let rest = text.strip_prefix("---\n")?;
    let (front, body) = rest.split_once("\n---\n")?;

    let mut fields = BTreeMap::new();
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        fields.insert(
            key.to_string(),
            value.trim().trim_matches(['\'', '"']).to_string(),
        );
    }
    Some(Frontmatter {
        fields,
        body: body.to_string(),
    })
}

/// Render a header + body. Field order is the caller's (a `Vec`, not a map, because a
/// page that reads `type` then `title` should not be reordered by the hash of its keys).
pub fn render(fields: &[(&str, String)], body: &str) -> String {
    let mut out = String::from("---\n");
    for (key, value) in fields {
        out.push_str(key);
        out.push_str(": ");
        out.push_str(value.trim());
        out.push('\n');
    }
    out.push_str("---\n\n");
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

#[cfg(test)]
#[path = "frontmatter_tests.rs"]
mod frontmatter_tests;
