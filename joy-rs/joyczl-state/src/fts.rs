//! Turn human input into a query FTS5 can safely digest.

/// Users type anything: quotes, asterisks, colons, half a bracket. Splicing it straight into
/// `MATCH` throws SQLite syntax errors — and the retrieval gate is **fail-open**, one error
/// degrades into "search every turn". So the input is split into words, each quoted, joined
/// with OR: always-legal grammar; any word matching suffices ("recall something related").
///
/// `None` means no usable words (all punctuation / empty); the caller should skip search.
///
/// The tokenizer is trigram (see migrations/0002): indexing by 3-char slices, CJK works
/// naturally. Words shorter than 3 chars yield no trigrams — [`like_pattern`] covers those.
pub fn to_match_expr(input: &str) -> Option<String> {
    // Split on "non-alphanumeric" rather than trimming ends: colons and slashes inside
    // break FTS5 grammar too (`a:b` is not a legal bare word, quoted or not, semantically).
    let tokens: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();

    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

/// The fallback when FTS misses: an ordinary LIKE substring scan.
///
/// Covers two cases: query terms shorter than 3 chars (two-char CJK words are common; no
/// trigrams), and trigram's inability on very short English words. A personal memory store
/// is a few thousand rows; one full LIKE scan is microseconds — no external tokenizer needed.
///
/// The wildcards `%` `_` are stripped, or a user could turn LIKE into a full-table scan
/// (only slow, but no reason to leave that door open).
pub fn like_pattern(input: &str) -> Option<String> {
    // Strip only LIKE's own two wildcards, keep everything else —
    // including spaces, since "alex morning" as one contiguous substring is what's wanted.
    let needle: String = input.chars().filter(|c| !matches!(c, '%' | '_')).collect();
    let needle = needle.trim();
    // At least one alphanumeric (CJK counts), or this scan is doomed to idle.
    if !needle.chars().any(char::is_alphanumeric) {
        None
    } else {
        Some(format!("%{needle}%"))
    }
}

#[cfg(test)]
mod fts_tests {
    use super::{like_pattern, to_match_expr};

    #[test]
    fn quotes_each_token() {
        assert_eq!(
            to_match_expr("alex morning meetings").as_deref(),
            Some("\"alex\" OR \"morning\" OR \"meetings\"")
        );
    }

    #[test]
    fn strips_punctuation_that_would_break_the_grammar() {
        // Every word gets quoted, so dangerous characters must be split off completely first.
        for bad in [
            "what's on?",
            "a:b",
            "\"quoted\"",
            "star*",
            "(unclosed",
            "--",
            "a/b",
        ] {
            if let Some(expr) = to_match_expr(bad) {
                // Check the quoted words one by one, not the whole string (it has quotes and OR anyway).
                for token in expr.split(" OR ") {
                    let inside = token.trim_matches('"');
                    for dangerous in ['(', ')', ':', '*', '"', '/', '-'] {
                        assert!(
                            !inside.contains(dangerous),
                            "{token} in the {expr} generated from {bad:?} still contains {dangerous}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn splits_on_inner_punctuation_too() {
        // Trimming only the ends would keep "a:b" whole.
        assert_eq!(to_match_expr("a:b").as_deref(), Some("\"a\" OR \"b\""));
    }

    #[test]
    fn punctuation_only_is_none() {
        assert_eq!(to_match_expr("???  ...  "), None);
        assert_eq!(to_match_expr(""), None);
        assert_eq!(like_pattern("???"), None);
    }

    #[test]
    fn like_pattern_keeps_chinese_and_drops_wildcards() {
        // A CJK fixture: the point is that a multi-byte token survives the LIKE pattern intact.
        assert_eq!(like_pattern("早上").as_deref(), Some("%早上%"));
        // A user-supplied % must not become a wildcard.
        assert_eq!(like_pattern("a%b").as_deref(), Some("%ab%"));
    }
}
