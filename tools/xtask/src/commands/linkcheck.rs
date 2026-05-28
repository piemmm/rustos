//! Minimal Markdown link checker for the RustOS mdBook.
//!
//! The mdbook preprocessor ecosystem trails the mdBook release schedule by
//! enough that pinning a working pair becomes a maintenance burden in its
//! own right. This module owns the project's link policy directly:
//!
//! - Every `[text](target)` link in the book source under `docs/src/` is
//!   visited.
//! - Relative links (everything that is not `http://`, `https://`, or
//!   `mailto:`) must resolve to an existing file under `docs/src/`. The
//!   fragment part (`#anchor`) is not validated — anchor generation is an
//!   mdBook detail we should not duplicate.
//! - Absolute URLs are accepted without contacting the network so CI does
//!   not depend on the public internet being reachable.
//!
//! A failure lists every broken link found in one pass; the caller does
//! not have to fix and re-run for each individual breakage.

use std::fs;
use std::path::{Path, PathBuf};

/// Check every relative link in the book sources rooted at `book_src`.
pub fn run(book_src: &Path) -> Result<(), String> {
    let mut broken: Vec<String> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    collect_markdown(book_src, &mut files)
        .map_err(|e| format!("link-check: walking {}: {e}", book_src.display()))?;

    for file in &files {
        let text = match fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => {
                broken.push(format!(
                    "{}: could not read: {e}",
                    display_relative(book_src, file)
                ));
                continue;
            }
        };
        for target in extract_link_targets(&text) {
            if is_external(&target) {
                continue;
            }
            // Strip an optional `#fragment`.
            let target_path = target.split('#').next().unwrap_or(&target);
            if target_path.is_empty() {
                // Pure anchor link, e.g. `[here](#section)` — nothing to check.
                continue;
            }
            let parent = file.parent().unwrap_or(book_src);
            let resolved = parent.join(target_path);
            if !resolved.exists() {
                broken.push(format!(
                    "{}: dangling link `{}` (resolved to `{}`)",
                    display_relative(book_src, file),
                    target,
                    resolved.display(),
                ));
            }
        }
    }

    if broken.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "link-check: {} broken link(s):\n  - {}",
            broken.len(),
            broken.join("\n  - ")
        ))
    }
}

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_markdown(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

fn is_external(target: &str) -> bool {
    target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with("tel:")
}

/// Extract the URL component of every inline Markdown link: `[text](url)`.
///
/// This handles the common case used in the RustOS book; reference-style
/// links (`[text][id]` + `[id]: url`) are intentionally rejected by lint —
/// the book is small enough that inline links keep it readable.
fn extract_link_targets(markdown: &str) -> Vec<String> {
    let bytes = markdown.as_bytes();
    let mut targets = Vec::new();
    let mut i = 0;
    let mut in_fence = false;
    let mut line_start = true;

    while i < bytes.len() {
        // Track triple-backtick fences so we don't pick up links inside
        // code samples (e.g. shell snippets with parentheses).
        if line_start && bytes[i..].starts_with(b"```") {
            in_fence = !in_fence;
            // Skip to end of line.
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            line_start = true;
            continue;
        }
        line_start = bytes[i] == b'\n';

        if in_fence {
            i += 1;
            continue;
        }

        // Inline links: `[label](target)`. We scan for `](` and walk back
        // to ensure the `[` opener exists.
        if bytes[i] == b']' && bytes.get(i + 1) == Some(&b'(') {
            // Find the matching `)`.
            let mut j = i + 2;
            let mut depth = 1;
            while j < bytes.len() && depth > 0 {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            if depth == 0 {
                let url_bytes = &bytes[i + 2..j - 1];
                if let Ok(url) = std::str::from_utf8(url_bytes) {
                    // Inline links can include a title: `(url "title")`.
                    let url = url.split_whitespace().next().unwrap_or(url);
                    if !url.is_empty() {
                        targets.push(url.to_string());
                    }
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    targets
}

fn display_relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_inline_links() {
        let md = "See [foo](./foo.md) and [bar](https://example.com).";
        let urls = extract_link_targets(md);
        assert_eq!(
            urls,
            vec!["./foo.md".to_string(), "https://example.com".to_string()]
        );
    }

    #[test]
    fn ignores_links_in_fenced_blocks() {
        let md = "Text [a](./a.md).\n\n```\n[b](./b.md)\n```\n[c](./c.md)\n";
        let urls = extract_link_targets(md);
        assert_eq!(urls, vec!["./a.md".to_string(), "./c.md".to_string()]);
    }

    #[test]
    fn handles_titles_inside_url_parens() {
        let md = r#"[t](./t.md "title here")"#;
        let urls = extract_link_targets(md);
        assert_eq!(urls, vec!["./t.md".to_string()]);
    }

    #[test]
    fn classifies_external_schemes() {
        assert!(is_external("https://x"));
        assert!(is_external("http://x"));
        assert!(is_external("mailto:a@b"));
        assert!(!is_external("./local.md"));
        assert!(!is_external("#anchor"));
    }
}
