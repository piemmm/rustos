//! `cargo xtask charter-cite` implementation.
//!
//! The charter forbids a comment from citing one of its own section numbers: a
//! section number restates *what* a rule is, where a comment must say *why*
//! the code does what it does. References to anything *outside* the charter —
//! a plan, a `docs/` page, an RFC, a hardware manual — are legitimate and must
//! survive, so the scan distinguishes the two by the source named next to the
//! reference rather than by the notation alone.
//!
//! Two rules, both over comments in tracked sources:
//!
//! 1. A comment must not name the charter file beside a section reference.
//!    Naming it in prose ("the charter forbids this duplication") is fine; it
//!    is the section number that restates the rule instead of the reason.
//! 2. A `§N` whose number is one of the charter's own section labels must have
//!    a source named just before it, so `RFC 9293 §3.2` and
//!    `` `plans/APPS.md` §4 `` pass while a bare `(§2.2)` — which a reader can
//!    only resolve as the charter — does not. The source may sit anywhere in
//!    the same comment paragraph, so a reference wrapped across lines still
//!    reads as anchored.
//!
//! A comment is a comment whatever the file spells it with, so the scan covers
//! every tracked file type that has one: Rust, the assembly stubs, `Cargo.toml`
//! and its siblings, the CI shell scripts, and the workflow YAML. Prose
//! *documents* — `README.md`, `docs/src/**`, `plans/*.md` — are deliberately
//! outside it: they cite the rules they explain or implement, and naming the
//! charter as a source there is the cross-reference the charter asks for.
//!
//! A `Cargo.toml` `description` is scanned too, as a third rule. It is a value
//! rather than a comment, but it is the crate's own prose about why it exists —
//! the same job its module doc does — and it is read through `cargo metadata`
//! and the generated SBOM, away from the charter, where a bare section number
//! resolves to nothing at all.
//!
//! A generated file is skipped: the charter's one sanctioned citation is the
//! provenance a generator stamps onto what it emits, and hand-editing that
//! banner to satisfy this check would only make the generated view drift from
//! its generator. Fix such a banner in the generator, which is hand-written
//! code and is scanned.
//!
//! The label set is read from `AGENTS.md` itself, so the check tracks the
//! charter rather than a copy of its table of contents.

use std::collections::BTreeSet;
use std::path::Path;

/// The charter file, both the label source and the name rule 1 refuses.
const CHARTER: &str = "AGENTS.md";

/// This checker's own file, skipped because it necessarily names [`CHARTER`].
const SELF_FILE: &str = "charter_cite.rs";

/// The banner every in-tree generator stamps onto what it emits.
const GENERATED: &str = "GENERATED FILE";

/// The comment and literal spelling of a scanned file type.
///
/// One variant per spelling rather than per extension: the CI shell scripts
/// and the workflow YAML share a comment and string grammar, so they share a
/// scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    /// `//`, nesting `/* … */`, `"…"` and `r#"…"#` strings, `'c'` chars.
    Rust,
    /// The LLVM integrated assembler: `//` and `/* … */` whatever the target,
    /// plus `#` where a target spells its comment that way.
    Asm,
    /// `#` to the end of the line, `'…'`/`"…"` and their triple-quoted forms.
    Toml,
    /// `#` at the start of a word, `'…'`/`"…"` scalars. Shell and YAML.
    Script,
}

impl Syntax {
    /// The syntax a file named `name` is scanned with, or `None` to skip it.
    fn of(name: &str) -> Option<Self> {
        let (_, ext) = name.rsplit_once('.')?;
        match ext {
            "rs" => Some(Self::Rust),
            "s" => Some(Self::Asm),
            "toml" => Some(Self::Toml),
            "sh" | "yml" | "yaml" => Some(Self::Script),
            _ => None,
        }
    }

    /// Whether `//` opens a comment and `/* … */` a block.
    fn slash_comments(self) -> bool {
        matches!(self, Self::Rust | Self::Asm)
    }

    /// Whether a `#` with these neighbours opens a comment.
    ///
    /// TOML spells one with `#` outside a string unconditionally. Shell and
    /// YAML need it at the start of a word, so `${#list[@]}` and `"$#"` stay
    /// code. The assembler needs whitespace after it, which is what tells a
    /// comment marker from the AArch64 immediate prefix that binds to its
    /// value with none.
    fn hash_comment(self, prev: Option<char>, next: Option<char>) -> bool {
        match self {
            Self::Rust => false,
            Self::Toml => true,
            Self::Script => prev.is_none_or(char::is_whitespace),
            Self::Asm => next.is_none_or(char::is_whitespace),
        }
    }

    /// Whether `quote` opens a string rather than a character constant.
    ///
    /// Rust and the assembler spell a character constant with single quotes,
    /// so only the double quote opens a string there; TOML, shell, and YAML
    /// take both.
    fn opens_string(self, quote: char) -> bool {
        match quote {
            '"' => true,
            '\'' => matches!(self, Self::Toml | Self::Script),
            _ => false,
        }
    }

    /// Whether an unterminated `"…"` stays open across the newline.
    ///
    /// Rust and the assembler continue one only through a trailing `\`; TOML
    /// forbids a raw newline in either single-line form; a shell or YAML
    /// quoted scalar simply carries on.
    fn string_spans_lines(self, line: &str) -> bool {
        match self {
            Self::Rust | Self::Asm => line.trim_end().ends_with('\\'),
            Self::Toml => false,
            Self::Script => true,
        }
    }
}

/// Whether `src` declares itself generated.
///
/// Every in-tree generator writes the banner as the emitted file's *first*
/// line, as a plain comment in that file's own syntax. Requiring exactly that
/// keeps a hand-written generator — whose own `//!` module doc naturally
/// mentions the banner it writes — inside the scan.
fn is_generated(src: &str, syntax: Syntax) -> bool {
    let Some(first) = src.lines().next().map(str::trim_start) else {
        return false;
    };
    let opens = match syntax {
        Syntax::Rust | Syntax::Asm => {
            first.starts_with("//") && !first.starts_with("///") && !first.starts_with("//!")
        }
        Syntax::Toml | Syntax::Script => first.starts_with('#'),
    };
    opens && first.contains(GENERATED)
}

/// How far back from a `§` a named source is looked for. One clause: long
/// enough for `` (`docs/src/filesystem/arxfs-spec.md` §11) `` to anchor its
/// reference, short enough that an unrelated earlier sentence cannot.
const LOOKBEHIND: usize = 45;

/// Source names that anchor a section reference to something outside the
/// charter — every one of them cited by section somewhere in the tree, so the
/// list is evidence rather than a wish list. A reference whose source is named
/// only further away than [`LOOKBEHIND`] must repeat the name; that is the
/// price of an unambiguous citation and is what the diagnostic asks for. A
/// newly-cited specification adds its name here, in the same change.
///
/// A lowercase word that is also a substring of ordinary prose (`spec` in
/// "specific") carries a trailing space, so it anchors only where it is
/// actually the source: `spec §13`.
const SOURCES: &[&str] = &[
    // In-tree documents: a plan, a `docs/` page, or the shorthand a crate's own
    // module doc anchors.
    ".md",
    "plan ",
    "spec ",
    "SYSLOG",
    // Requests for comments and other numbered standards.
    "RFC",
    "ISO",
    "ITU",
    "IANA",
    // Bus, controller, and device specifications.
    "virtio",
    "xHCI",
    "USB",
    "UAS",
    "CBI",
    "BOT",
    "UFI",
    "PCI",
    "ATA",
    "SPC-4",
    "SBC-3",
    "SAM-5",
    "ACPI",
    "UEFI",
    "Multiboot",
    "VBE",
    "GICv",
    "HID",
    "VL805",
    // Architectures and vendors whose manuals are cited by section.
    "Arm",
    "ARM",
    "RISC-V",
    "Intel",
    "AMD",
    "SDM",
    "NXP",
    "Maxim",
    "Broadcom",
    // Wire and file formats.
    "DHCP",
    "DNS",
    "ICMP",
    "TCP",
    "Ethernet",
    "PNG",
    "T.81",
    "ext4",
];

/// What kind of text a citation sits in, so the report names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Comment,
    /// A `Cargo.toml` `description`: the crate's own prose, not a comment.
    Description,
}

impl Surface {
    /// How the report names this surface.
    fn label(self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::Description => "package description",
        }
    }
}

/// A comment or package description that cites the charter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub line: usize,
    pub surface: Surface,
    /// Why the citation was refused, phrased for the developer.
    pub reason: &'static str,
    pub text: String,
}

/// Read the charter's own section labels (`2.11`, `19.10`, …).
///
/// Three sources, all derived rather than copied, so the set tracks the charter
/// as it is edited: every `§N` the charter cites, its `##`/`###` headings, and
/// the ordered-list items under each heading — the charter numbers its rules as
/// list items (`§2.11`, `§5.4.5`), which no heading records.
pub fn charter_labels(root: &Path) -> Result<BTreeSet<String>, String> {
    let path = root.join(CHARTER);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("charter-cite: cannot read {}: {e}", path.display()))?;
    let mut out = BTreeSet::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find('§') {
        rest = &rest[at + '§'.len_utf8()..];
        if let Some(label) = leading_label(rest) {
            out.insert(label);
        }
    }
    let mut section: Option<String> = None;
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let head = line.trim_start_matches('#');
        if head.len() < line.len() {
            section = leading_label(head.trim_start());
            if let Some(label) = section.clone() {
                out.insert(label);
            }
            continue;
        }
        // `1. **No hacks.**` under `## 2.` is §2.1; the charter's rules are
        // numbered nowhere else.
        if let (Some(sec), Some(item)) = (section.as_deref(), ordered_item(line)) {
            out.insert(format!("{sec}.{item}"));
        }
    }
    Ok(out)
}

/// The number of an ordered-list item line (`  3. text` → `3`).
fn ordered_item(line: &str) -> Option<u32> {
    let (digits, _) = line.trim_start().split_once(". ")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The dotted decimal label at the start of `s`, if any (`2.11 foo` → `2.11`).
fn leading_label(s: &str) -> Option<String> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let label = s[..end].trim_end_matches('.');
    if label.is_empty() || !label.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(label.to_string())
}

/// Where a source scan is when it reaches a line boundary.
///
/// A section number inside a string literal is program output, which the
/// charter permits to name the rule a developer violated — a `compile_error!`
/// message, or the provenance banner a generator emits into a generated file.
/// Those literals span lines, so the scanner has to carry its state across
/// them rather than judging each line alone.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Lex {
    #[default]
    Code,
    /// Inside a single-line quoted literal, holding its delimiter.
    Str(char),
    /// Inside a triple-quoted literal, holding its delimiter.
    Triple(char),
    /// Inside an `r#"…"#` literal, holding its hash count.
    Raw(usize),
    /// Inside a `/* … */` comment, holding its nesting depth.
    Block(usize),
}

/// Advance `state` across `line`, returning the line's line-comment body.
fn comment_body<'a>(line: &'a str, state: &mut Lex, syntax: Syntax) -> Option<&'a str> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut k = 0;
    let at = |k: usize| chars.get(k).map(|(_, c)| *c);
    let byte = |k: usize| chars.get(k).map_or(line.len(), |(i, _)| *i);
    while k < chars.len() {
        match *state {
            // Only a double-quoted literal takes backslash escapes; every
            // single-quoted form the scan meets is literal.
            Lex::Str(quote) => {
                match at(k) {
                    Some('\\') if quote == '"' => k += 1,
                    Some(c) if c == quote => *state = Lex::Code,
                    _ => {}
                }
                k += 1;
            }
            Lex::Triple(quote) => {
                if (0..3).all(|d| at(k + d) == Some(quote)) {
                    *state = Lex::Code;
                    k += 3;
                } else {
                    k += 1;
                }
            }
            Lex::Raw(h) => {
                if at(k) == Some('"') && (1..=h).all(|d| at(k + d) == Some('#')) {
                    *state = Lex::Code;
                    k += 1 + h;
                } else {
                    k += 1;
                }
            }
            Lex::Block(depth) => {
                if at(k) == Some('*') && at(k + 1) == Some('/') {
                    *state = if depth == 1 {
                        Lex::Code
                    } else {
                        Lex::Block(depth - 1)
                    };
                    k += 2;
                } else if at(k) == Some('/') && at(k + 1) == Some('*') {
                    *state = Lex::Block(depth + 1);
                    k += 2;
                } else {
                    k += 1;
                }
            }
            Lex::Code => match at(k) {
                Some('/') if syntax.slash_comments() && at(k + 1) == Some('/') => {
                    return Some(&line[byte(k)..]);
                }
                Some('/') if syntax.slash_comments() && at(k + 1) == Some('*') => {
                    *state = Lex::Block(1);
                    k += 2;
                }
                Some('#') if syntax.hash_comment(k.checked_sub(1).and_then(at), at(k + 1)) => {
                    return Some(&line[byte(k)..]);
                }
                Some(quote) if syntax.opens_string(quote) => {
                    *state = if (1..3).all(|d| at(k + d) == Some(quote)) {
                        k += 2;
                        Lex::Triple(quote)
                    } else {
                        Lex::Str(quote)
                    };
                    k += 1;
                }
                Some('r') if syntax == Syntax::Rust => {
                    let h = (1..chars.len() - k)
                        .take_while(|d| at(k + d) == Some('#'))
                        .count();
                    if at(k + 1 + h) == Some('"') {
                        *state = Lex::Raw(h);
                        k += 2 + h;
                    } else {
                        k += 1;
                    }
                }
                // A character constant cannot open a string, so skip its body:
                // a quote between the ticks must not read as one.
                Some('\'') => {
                    let len = if at(k + 1) == Some('\\') { 2 } else { 1 };
                    k += if at(k + 1 + len) == Some('\'') {
                        2 + len
                    } else {
                        1
                    };
                }
                _ => k += 1,
            },
        }
    }
    if let Lex::Str(quote) = *state {
        // A single-quoted literal never carries an escape, so nothing can
        // continue it past the newline.
        if quote == '\'' || !syntax.string_spans_lines(line) {
            *state = Lex::Code;
        }
    }
    None
}

/// Scan one comment paragraph for citations, each with its byte offset in
/// `text` so the caller can name the line it sits on.
///
/// `text` is the paragraph's lines joined by a space, so a reference wrapped
/// across lines still finds the source named before it.
fn scan_paragraph(text: &str, labels: &BTreeSet<String>) -> Vec<(usize, &'static str)> {
    let mut out = Vec::new();
    if text.contains('§') {
        if let Some(at) = text.find(CHARTER) {
            out.push((at, "cites the charter by section"));
        }
    }
    let mut rest = text;
    let mut consumed = 0usize;
    while let Some(at) = rest.find('§') {
        let abs = consumed + at;
        rest = &rest[at + '§'.len_utf8()..];
        consumed = abs + '§'.len_utf8();
        // `§X`, `§SYSRET`, and `§"Overflow"` name a section that has no number;
        // only a sign attached to nothing is a leftover.
        let Some(label) = leading_label(rest) else {
            if rest.starts_with(char::is_whitespace) || rest.is_empty() {
                out.push((abs, "section sign naming no section"));
            }
            continue;
        };
        if !labels.contains(&label) {
            continue;
        }
        let start = text[..abs]
            .char_indices()
            .rev()
            .take(LOOKBEHIND)
            .last()
            .map_or(0, |(i, _)| i);
        if !SOURCES.iter().any(|src| text[start..abs].contains(src)) {
            out.push((abs, "section number with no source named beside it"));
        }
    }
    out
}

/// Scan the workspace rooted at `root` for citations of the charter.
pub fn scan(root: &Path) -> Result<Vec<Violation>, String> {
    let labels = charter_labels(root)?;
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("charter-cite: cannot read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("charter-cite: dir entry: {e}"))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| format!("charter-cite: file type {}: {e}", path.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if file_type.is_dir() {
                if name == "target" || name == ".git" {
                    continue;
                }
                dirs.push(path);
            } else if file_type.is_file() && name != SELF_FILE {
                if let Some(syntax) = Syntax::of(&name) {
                    scan_file(&path, &relative(root, &path), syntax, &labels, &mut out)?;
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(out)
}

fn scan_file(
    path: &Path,
    rel: &str,
    syntax: Syntax,
    labels: &BTreeSet<String>,
    out: &mut Vec<Violation>,
) -> Result<(), String> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| format!("charter-cite: cannot read {}: {e}", path.display()))?;
    if is_generated(&src, syntax) {
        return Ok(());
    }
    // A paragraph is a run of comment lines with text; the citations in one are
    // scanned together so a wrapped reference keeps the source named before it.
    let mut state = Lex::default();
    let mut paragraphs: Vec<Vec<(usize, &str)>> = Vec::new();
    let mut para: Vec<(usize, &str)> = Vec::new();
    for (idx, line) in src.lines().enumerate() {
        let body = comment_body(line, &mut state, syntax)
            .map(comment_text)
            .filter(|b| !b.is_empty());
        match body {
            Some(b) => para.push((idx + 1, b)),
            None if !para.is_empty() => paragraphs.push(std::mem::take(&mut para)),
            None => {}
        }
    }
    if !para.is_empty() {
        paragraphs.push(para);
    }
    for para in &paragraphs {
        let text = para.iter().map(|(_, b)| *b).collect::<Vec<_>>().join(" ");
        for (at, reason) in scan_paragraph(&text, labels) {
            let Some((line, body)) = line_of(para, at) else {
                continue;
            };
            out.push(Violation {
                path: rel.to_string(),
                line,
                surface: Surface::Comment,
                reason,
                text: body.to_string(),
            });
        }
    }
    if syntax == Syntax::Toml {
        if let Some((line, text)) = description(&src) {
            for (_, reason) in scan_paragraph(&text, labels) {
                out.push(Violation {
                    path: rel.to_string(),
                    line,
                    surface: Surface::Description,
                    reason,
                    text: text.clone(),
                });
            }
        }
    }
    Ok(())
}

/// A comment body stripped of its marker, so the citation scan sees prose.
fn comment_text(body: &str) -> &str {
    body.trim_start_matches(['/', '#'])
        .trim_start_matches('!')
        .trim()
}

/// The `description` value of a manifest, with the line it starts on.
///
/// A manifest declares at most one, and a `description.workspace = true`
/// inherits rather than states prose, so it is not one.
fn description(src: &str) -> Option<(usize, String)> {
    let mut lines = src.lines().enumerate();
    let (idx, first) = lines.by_ref().find_map(|(idx, line)| {
        let rest = line.trim_start().strip_prefix("description")?;
        Some((idx + 1, rest.trim_start().strip_prefix('=')?.trim_start()))
    })?;
    for quote in ['"', '\''] {
        let triple: String = std::iter::repeat_n(quote, 3).collect();
        if let Some(open) = first.strip_prefix(triple.as_str()) {
            let mut text = String::from(open.trim_start_matches('\\'));
            for (_, line) in lines {
                if let Some(end) = line.find(triple.as_str()) {
                    text.push_str(&line[..end]);
                    return Some((idx, text));
                }
                text.push(' ');
                text.push_str(line);
            }
            return Some((idx, text));
        }
        if let Some(open) = first.strip_prefix(quote) {
            let end = open.rfind(quote)?;
            return Some((idx, open[..end].to_string()));
        }
    }
    None
}

/// The paragraph line carrying byte offset `at` of the joined text.
///
/// `None` only for an empty paragraph, which carries no citation to report.
fn line_of<'a>(para: &[(usize, &'a str)], at: usize) -> Option<(usize, &'a str)> {
    let mut base = 0usize;
    let mut last = None;
    for entry in para {
        last = Some(*entry);
        if at < base + entry.1.len() + 1 {
            return last;
        }
        base += entry.1.len() + 1;
    }
    last
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Run the check, printing a report and failing when any citation remains.
pub fn run(root: &Path) -> Result<(), String> {
    use std::fmt::Write as _;
    let violations = scan(root)?;
    if violations.is_empty() {
        return Ok(());
    }
    let mut msg = String::from(
        "charter-cite: a comment or package description must state the reason, \
         not cite a charter section (AGENTS.md §2.11 / §15.17). Replace each \
         with the prose reason (\"fail closed\", \"zeroed on drop\"), or, for a \
         reference to another document, name that document beside the section \
         number:\n",
    );
    for v in &violations {
        let _ = writeln!(
            msg,
            "  {}:{}: {} {} — {}",
            v.path,
            v.line,
            v.surface.label(),
            v.reason,
            v.text
        );
    }
    Err(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p
    }

    fn labels() -> BTreeSet<String> {
        charter_labels(&workspace_root()).expect("charter labels")
    }

    #[test]
    fn workspace_carries_no_charter_citations() {
        let violations = scan(&workspace_root()).expect("scan");
        assert!(
            violations.is_empty(),
            "unexpected charter citations in comments: {violations:#?}"
        );
    }

    #[test]
    fn charter_labels_cover_the_rule_lists_and_the_headings() {
        let l = labels();
        for want in [
            "2.11", "2.24", "5.4", "15.17", "16.8", "19.10", "23.5", "26.7", "27.5",
        ] {
            assert!(l.contains(want), "missing charter label {want}");
        }
        assert!(
            !l.contains("6.2.2"),
            "an xHCI section is not a charter label"
        );
    }

    #[test]
    fn naming_the_charter_is_refused_in_every_comment_spelling() {
        for body in [
            "//! the one definition (`AGENTS.md` §2.2)",
            "/// fails closed (AGENTS.md §5.4)",
        ] {
            assert!(
                reasons(body).contains(&"cites the charter by section"),
                "not refused: {body}"
            );
        }
    }

    #[test]
    fn a_bare_charter_section_is_refused() {
        for body in [
            "/// one definition (§2.2).",
            "// fail closed (§5.4)",
            "//! never a linear scan under §26 load",
            "/// a fixed bound, not a capacity (§24.4)",
        ] {
            assert_eq!(
                reasons(body),
                vec!["section number with no source named beside it"],
                "not refused: {body}"
            );
        }
    }

    #[test]
    fn naming_the_charter_in_prose_is_accepted() {
        // The charter asks for the reason, and permits naming itself in prose.
        assert!(reasons("/// rather than retrying forever (the `AGENTS.md` ban).").is_empty());
    }

    #[test]
    fn a_sourced_reference_is_accepted() {
        for body in [
            "/// The RFC 9293 §3.3.2 connection states.",
            "//! (`plans/APPS.md` §4): one document per command.",
            "/// Endpoint context type field: Control (xHCI §6.2.3).",
            "/// Power-on-good ceiling (USB 2.0 §11.11).",
            "//! (`docs/src/filesystem/arxfs-spec.md` §11) queries the array.",
            "/// Split virtqueue management (virtio 1.1 §2.6).",
            "// the spec §13 authority treatment",
            "//! Reference: NXP PCF8523 data sheet, §8 (register overview).",
            "//! Reference: Maxim DS3231 data sheet, §9 (register map).",
        ] {
            assert!(
                reasons(body).is_empty(),
                "wrongly refused: {body} -> {:?}",
                reasons(body)
            );
        }
    }

    /// Comment bodies of `src`, threading the lexer state across its lines.
    fn bodies_of(src: &str, syntax: Syntax) -> Vec<&str> {
        let mut state = Lex::default();
        src.lines()
            .filter_map(|l| comment_body(l, &mut state, syntax))
            .collect()
    }

    /// [`bodies_of`] for a Rust source, the common case.
    fn bodies(src: &str) -> Vec<&str> {
        bodies_of(src, Syntax::Rust)
    }

    /// The reasons [`scan_paragraph`] gives for one comment body.
    fn reasons(body: &str) -> Vec<&'static str> {
        scan_paragraph(comment_text(body), &labels())
            .into_iter()
            .map(|(_, r)| r)
            .collect()
    }

    #[test]
    fn a_section_number_in_a_string_literal_is_program_output() {
        // A build or CI diagnostic may name the rule a developer violated.
        let src = r#"    compile_error!("exactly one scheduler feature (AGENTS.md §17.1)");"#;
        assert!(bodies(src).is_empty());
    }

    #[test]
    fn a_generated_file_is_skipped() {
        // The charter sanctions a generator stamping the governing rule onto
        // what it emits; the fix for such a banner is in the generator.
        let dir = std::env::temp_dir().join(format!("tairix-cc-gen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("atlas.rs");
        std::fs::write(
            &file,
            "// GENERATED FILE — DO NOT EDIT.\n\
             //\n\
             // (AGENTS.md §2.2: generated views are never hand-maintained).\n",
        )
        .expect("write");
        let mut out = Vec::new();
        scan_file(&file, "atlas.rs", Syntax::Rust, &labels(), &mut out).expect("scan");
        std::fs::remove_dir_all(&dir).ok();
        assert!(out.is_empty(), "{out:#?}");
    }

    #[test]
    fn a_hand_written_file_mentioning_generated_files_is_still_scanned() {
        let dir = std::env::temp_dir().join(format!("tairix-cc-hand-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("gen.rs");
        std::fs::write(
            &file,
            "//! Writes a GENERATED FILE banner.\n\
             //!\n\
             //! One definition (§2.2).\n",
        )
        .expect("write");
        let mut out = Vec::new();
        scan_file(&file, "gen.rs", Syntax::Rust, &labels(), &mut out).expect("scan");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(out.len(), 1, "{out:#?}");
        assert_eq!(
            out[0].reason,
            "section number with no source named beside it"
        );
    }

    #[test]
    fn a_generator_provenance_banner_is_program_output() {
        // The charter's one sanctioned citation: a generator stamping which
        // rule governs the artefact it emits. The literal spans lines, so the
        // scanner must not read its continuations as comments.
        let src = "        out.push_str(\n\
                   \x20            \"// GENERATED FILE — DO NOT EDIT.\\n\\\n\
                   \x20            // (AGENTS.md §2.2: generated views are never hand-maintained).\\n\",\n\
                   \x20        );";
        assert!(bodies(src).is_empty(), "{:?}", bodies(src));
    }

    #[test]
    fn a_comment_after_a_string_is_still_scanned() {
        let src = r#"    let s = "a // b"; // one definition (§2.2)"#;
        assert_eq!(bodies(src), vec!["// one definition (§2.2)"]);
        assert_eq!(
            reasons(bodies(src)[0]),
            vec!["section number with no source named beside it"]
        );
    }

    #[test]
    fn a_raw_string_and_a_char_literal_do_not_open_a_literal() {
        let src = "    let q = \'\"\'; let r = r#\"a \"b\" c\"#; // fail closed (§5.4)";
        assert_eq!(bodies(src), vec!["// fail closed (§5.4)"]);
    }

    #[test]
    fn a_blank_doc_line_separates_paragraphs() {
        // `//!` must read as blank, or a whole module doc becomes one
        // paragraph and a citation borrows an unrelated source.
        let src = "//! (`plans/APPS.md` §4) names the source.\n//!\n//! one definition (§2.2).";
        let bodies: Vec<&str> = bodies(src)
            .into_iter()
            .map(comment_text)
            .filter(|b| !b.is_empty())
            .collect();
        assert_eq!(bodies.len(), 2, "{bodies:?}");
        assert!(scan_paragraph(bodies[0], &labels()).is_empty());
        assert_eq!(
            scan_paragraph(bodies[1], &labels())
                .into_iter()
                .map(|(_, r)| r)
                .collect::<Vec<_>>(),
            vec!["section number with no source named beside it"]
        );
    }

    #[test]
    fn a_section_sign_attached_to_nothing_is_refused() {
        assert_eq!(
            reasons("// authentication must fail (§ relocation defence)"),
            vec!["section sign naming no section"]
        );
    }

    #[test]
    fn a_section_named_rather_than_numbered_is_accepted() {
        for body in [
            "/// (`plans/PI.md` §X), the riscv64 sibling of the aarch64 one.",
            "//! `STAR[63:48] + 8` (SDM Vol 2B §SYSRET).",
            "/// (`docs/src/architecture/scheduler.md` §\"Starvation freedom\").",
        ] {
            assert!(reasons(body).is_empty(), "wrongly refused: {body}");
        }
    }

    #[test]
    fn a_syntax_is_chosen_by_extension_and_prose_documents_are_out_of_scope() {
        for (name, want) in [
            ("lib.rs", Some(Syntax::Rust)),
            ("boot.s", Some(Syntax::Asm)),
            ("Cargo.toml", Some(Syntax::Toml)),
            ("soak.sh", Some(Syntax::Script)),
            ("ci.yml", Some(Syntax::Script)),
            ("action.yaml", Some(Syntax::Script)),
        ] {
            assert_eq!(Syntax::of(name), want, "{name}");
        }
        // The charter is a legitimate cross-reference in a prose document, so
        // those are deliberately never scanned.
        for name in [
            "README.md",
            "AGENTS.md",
            "PLAN.md",
            "Cargo.lock",
            "linker.ld",
        ] {
            assert_eq!(Syntax::of(name), None, "{name}");
        }
    }

    #[test]
    fn a_manifest_comment_is_scanned_and_a_string_value_is_not() {
        let src = "# one definition (§2.2)\n\
                   name = \"a # b (§5.4)\"\n\
                   path = 'c # d (§17.4)'\n";
        assert_eq!(
            bodies_of(src, Syntax::Toml),
            vec!["# one definition (§2.2)"]
        );
    }

    #[test]
    fn a_shell_comment_needs_a_word_boundary() {
        // A parameter expansion spells `#` inside a word, quoted or not.
        let src = "n=${#list[@]} # fail closed (§5.4)\n\
                   rc=${pid#done:}\n\
                   echo \"$#\"\n";
        assert_eq!(bodies_of(src, Syntax::Script), vec!["# fail closed (§5.4)"]);
    }

    #[test]
    fn an_assembler_comment_is_scanned_but_an_immediate_prefix_is_not() {
        // AArch64 spells an immediate `#0xff`, with no space; a comment marker
        // is followed by its text. Both `//` and `/* … */` are comments on
        // every target the integrated assembler serves.
        let src = "    mov     x5, #0xffff             // one definition (§2.2)\n\
                   /* fail closed (§5.4) */\n\
                   # one definition (§2.2)\n\
                       ldp     x21, x22, [sp, #16]\n";
        assert_eq!(
            bodies_of(src, Syntax::Asm),
            vec!["// one definition (§2.2)", "# one definition (§2.2)"]
        );
        assert_eq!(
            reasons("// one definition (§2.2)"),
            vec!["section number with no source named beside it"]
        );
    }

    /// Scan one file body of `syntax`, as the workspace walk would.
    ///
    /// `tag` names the scratch directory, so tests sharing a file name do not
    /// race each other under the parallel test runner.
    fn scan_source(tag: &str, name: &str, src: &str, syntax: Syntax) -> Vec<Violation> {
        let dir = std::env::temp_dir().join(format!("tairix-cc-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join(name);
        std::fs::write(&file, src).expect("write");
        let mut out = Vec::new();
        let res = scan_file(&file, name, syntax, &labels(), &mut out);
        std::fs::remove_dir_all(&dir).ok();
        res.expect("scan");
        out
    }

    #[test]
    fn a_package_description_citing_the_charter_is_refused() {
        let out = scan_source(
            "desc-charter",
            "Cargo.toml",
            "[package]\ndescription = \"The one definition (`AGENTS.md` §2.2).\"\n",
            Syntax::Toml,
        );
        assert_eq!(out.len(), 1, "{out:#?}");
        assert_eq!(out[0].surface, Surface::Description);
        assert_eq!(out[0].line, 2);
        assert_eq!(out[0].reason, "cites the charter by section");
    }

    #[test]
    fn a_package_description_naming_its_own_source_is_accepted() {
        for value in [
            "\"The netchan-v1 driver side (plans/NETWORK.md §2.3).\"",
            "\"An inert host stub; see `plans/PI.md` §0.2.\"",
        ] {
            let src = format!("[package]\ndescription = {value}\n");
            assert!(
                scan_source("desc-sourced", "Cargo.toml", &src, Syntax::Toml).is_empty(),
                "wrongly refused: {value}"
            );
        }
    }

    #[test]
    fn only_the_description_value_is_read() {
        // A neighbouring key's value is not the crate's own prose, and an
        // inherited `description.workspace = true` states none of its own.
        let src = "[package]\n\
                   readme = \"See `AGENTS.md` §2.2.\"\n\
                   description.workspace = true\n";
        assert!(
            scan_source("desc-inherited", "Cargo.toml", src, Syntax::Toml).is_empty(),
            "only a description is prose about the crate"
        );
    }

    #[test]
    fn a_multi_line_description_is_read_whole() {
        let src = "[package]\ndescription = \"\"\"\n\
                   The one definition\n\
                   (`AGENTS.md` §2.2).\n\
                   \"\"\"\n";
        let out = scan_source("desc-multiline", "Cargo.toml", src, Syntax::Toml);
        assert_eq!(out.len(), 1, "{out:#?}");
        assert_eq!(out[0].surface, Surface::Description);
    }

    #[test]
    fn a_generated_manifest_is_skipped() {
        // A generator stamps the governing rule onto what it emits, in that
        // file's own comment syntax.
        let out = scan_source(
            "generated",
            "generated.toml",
            "# GENERATED FILE — DO NOT EDIT.\n# (AGENTS.md §2.2).\n",
            Syntax::Toml,
        );
        assert!(out.is_empty(), "{out:#?}");
    }

    #[test]
    fn the_lexer_survives_adversarial_bytes_in_every_syntax() {
        // A panic here would block `ci` on a file the scan merely could not
        // lex, so the corpus drives every partial construct the four grammars
        // share: unterminated literals, a lone marker, multibyte boundaries.
        const ALPHABET: [&str; 24] = [
            "//",
            "/*",
            "*/",
            "#",
            "\"",
            "'",
            "\"\"\"",
            "'''",
            "r#\"",
            "\"#",
            "\\",
            "§",
            "§2.2",
            "AGENTS.md",
            "§",
            "0xff",
            " ",
            "\t",
            "€",
            "→",
            "—",
            "(",
            ")",
            "[",
        ];
        let syntaxes = [Syntax::Rust, Syntax::Asm, Syntax::Toml, Syntax::Script];
        let labels = labels();
        // Fixed seed: a failure reproduces without one being recorded.
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = |bound: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            usize::try_from(x % bound as u64).unwrap_or(0)
        };
        for _ in 0..4_000 {
            let mut src = String::new();
            for _ in 0..=next(12) {
                for _ in 0..=next(10) {
                    src.push_str(ALPHABET[next(ALPHABET.len())]);
                }
                src.push('\n');
            }
            for syntax in syntaxes {
                let mut state = Lex::default();
                for line in src.lines() {
                    if let Some(body) = comment_body(line, &mut state, syntax) {
                        scan_paragraph(comment_text(body), &labels);
                    }
                }
            }
            description(&format!("description = {src}"));
            description(&src);
        }
    }

    #[test]
    fn a_source_named_earlier_in_the_paragraph_anchors_a_wrapped_reference() {
        let text = "The scrub report (`docs/src/filesystem/arxfs-spec.md` \
                    §12). Counts accumulate across the resumable calls.";
        assert!(scan_paragraph(text, &labels()).is_empty());
    }
}
