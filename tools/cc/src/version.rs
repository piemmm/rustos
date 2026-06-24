//! Parsing the version strings `clang --version` / `ld.lld --version` print.
//!
//! The exact version is pinned (version-pinned external
//! tools; the supply-chain discipline), so the wrapper must extract it
//! from the tool's banner and compare it to the required value. The banner
//! format differs slightly between vendors (`Ubuntu clang version 18.1.3
//! (1ubuntu1)`, `clang version 18.1.3`, `Ubuntu LLD 18.1.3 (compatible with
//! GNU linkers)`, `LLD 18.1.3 ...`), so the parsers key off a stable anchor
//! token rather than a fixed column.

/// Extract the version token from `clang --version` output.
///
/// Looks for the `clang version <V>` anchor and returns `<V>` (the token up
/// to the next whitespace), or `None` if the anchor is absent.
#[must_use]
pub fn parse_clang_version(output: &str) -> Option<String> {
    token_after(output, "clang version")
}

/// Extract the version token from `ld.lld --version` output.
///
/// Looks for the `LLD <V>` anchor and returns `<V>`, or `None`.
#[must_use]
pub fn parse_lld_version(output: &str) -> Option<String> {
    token_after(output, "LLD")
}

/// Return the first whitespace-delimited token that follows `anchor` in
/// `text`, trimming a trailing `,`/`)` the banner sometimes appends.
fn token_after(text: &str, anchor: &str) -> Option<String> {
    let start = text.find(anchor)? + anchor.len();
    let rest = text.get(start..)?;
    let token = rest.split_whitespace().next()?;
    let token = token.trim_end_matches([',', ')', '(']);
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ubuntu_clang_banner() {
        let banner = "Ubuntu clang version 18.1.3 (1ubuntu1)\nTarget: x86_64-pc-linux-gnu\n";
        assert_eq!(parse_clang_version(banner).as_deref(), Some("18.1.3"));
    }

    #[test]
    fn parses_vanilla_clang_banner() {
        assert_eq!(
            parse_clang_version("clang version 17.0.6\n").as_deref(),
            Some("17.0.6")
        );
    }

    #[test]
    fn parses_ubuntu_lld_banner() {
        let banner = "Ubuntu LLD 18.1.3 (compatible with GNU linkers)\n";
        assert_eq!(parse_lld_version(banner).as_deref(), Some("18.1.3"));
    }

    #[test]
    fn parses_vanilla_lld_banner() {
        assert_eq!(
            parse_lld_version("LLD 18.1.3 (compatible with GNU linkers)").as_deref(),
            Some("18.1.3")
        );
    }

    #[test]
    fn missing_anchor_is_none() {
        assert_eq!(parse_clang_version("gcc (GCC) 13.2.0"), None);
        assert_eq!(parse_lld_version("GNU ld (GNU Binutils) 2.42"), None);
    }
}
