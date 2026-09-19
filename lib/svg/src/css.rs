//! The document's own `<style>` sheets: which declarations an element
//! matches, and in what order they win.
//!
//! An asset exported by a drawing tool routinely carries its colours in a
//! stylesheet rather than on the shapes (`.cls-1 { fill: #3a86e8 }`), so a
//! decoder that ignores `<style>` draws that artwork in the initial black.
//! Only the document's own sheets are read: an `@import` is an external
//! reference and this crate fetches nothing.
//!
//! # The subset
//!
//! Type (`rect`), class (`.cls`), id (`#id`), universal (`*`), any compound
//! of those (`rect.a.b`), a selector list (`a, b`), and the descendant and
//! child combinators. Specificity is CSS's `(id, class, type)` triple, ties
//! broken by source order.
//!
//! A rule whose selector is outside that subset — an attribute selector, a
//! pseudo-class, a sibling combinator — is **dropped**, and so is an at-rule.
//! That is what the construct means rather than a shortcut: a `@media print`
//! block does not apply to a rendered asset, and refusing the document over
//! one would lose an asset the decoder can draw perfectly. A declaration
//! whose property *is* understood but whose value is malformed is still an
//! error, exactly as a malformed presentation attribute is.

use alloc::vec::Vec;

use crate::error::SvgError;
use crate::xml::Element;

/// The most selectors one document may define, counted after a selector list
/// is expanded into one rule each.
///
/// A fixed security bound: every rule is tested against every element, so the
/// product is what a hostile sheet would otherwise grow without end.
const MAX_RULES: usize = 512;

/// The most declarations one document's sheets may hold in total.
const MAX_DECLARATIONS: usize = 2048;

/// The most compound selectors one selector may be built from.
///
/// A fixed security bound, and what keeps the matcher's scratch state on the
/// stack rather than sized from the document.
const MAX_SELECTOR_PARTS: usize = 8;

/// The most class and id conditions one compound selector may carry.
const MAX_COMPOUND_CONDITIONS: usize = 8;

/// One `name: value` declaration and whether it carries `!important`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Declaration<'a> {
    /// The property name, trimmed and lower-cased by the author.
    pub name: &'a str,
    /// The property value, trimmed, with any `!important` removed.
    pub value: &'a str,
    /// Whether the declaration was marked `!important`.
    pub important: bool,
}

/// Split a declaration block — a `style` attribute's text or a rule's body —
/// into its declarations.
///
/// The one splitter both callers use, so a `style` attribute and a stylesheet
/// rule can never disagree about where a value ends. A separator inside
/// brackets or quotes is text, not a separator, so `url(data:…;base64,…)`
/// stays one value.
pub fn declarations(text: &str) -> impl Iterator<Item = Declaration<'_>> {
    split_top_level(text, b';').filter_map(|piece| {
        let colon = top_level_index(piece, b':')?;
        let name = piece.get(..colon)?.trim();
        if name.is_empty() {
            return None;
        }
        let value = piece.get(colon + 1..)?.trim();
        let (value, important) = match strip_important(value) {
            Some(bare) => (bare, true),
            None => (value, false),
        };
        Some(Declaration {
            name,
            value,
            important,
        })
    })
}

/// `value` without a trailing `!important`, or `None` when it carries none.
fn strip_important(value: &str) -> Option<&str> {
    let keyword = value.len().checked_sub("important".len())?;
    if !value.get(keyword..)?.eq_ignore_ascii_case("important") {
        return None;
    }
    Some(
        value
            .get(..keyword)?
            .trim_end()
            .strip_suffix('!')?
            .trim_end(),
    )
}

/// Where the first `separator` outside brackets and quotes sits.
///
/// One inside either is text rather than a separator, so `url(data:…;base64,…)`
/// stays one value.
fn top_level_index(text: &str, separator: u8) -> Option<usize> {
    let mut depth = 0_u32;
    let mut quote: Option<u8> = None;
    for (index, byte) in text.bytes().enumerate() {
        match quote {
            Some(mark) if byte == mark => quote = None,
            Some(_) => {}
            None if byte == separator && depth == 0 => return Some(index),
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            },
        }
    }
    None
}

/// `text` cut at each top-level `separator`.
fn split_top_level(text: &str, separator: u8) -> impl Iterator<Item = &str> {
    let mut rest = Some(text);
    core::iter::from_fn(move || {
        let current = rest?;
        let Some(at) = top_level_index(current, separator) else {
            rest = None;
            return Some(current);
        };
        rest = current.get(at + 1..);
        current.get(..at)
    })
}

/// How a compound selector is joined to the one on its left.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Combinator {
    /// Any ancestor.
    Descendant,
    /// The immediate parent.
    Child,
}

/// One compound selector: an optional element name plus its id and class
/// conditions, all of which must hold.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Compound<'a> {
    element: Option<&'a str>,
    ids: Vec<&'a str>,
    classes: Vec<&'a str>,
}

impl Compound<'_> {
    /// Whether `node` satisfies every condition of this compound.
    fn matches(&self, node: &Element<'_>) -> bool {
        if self.element.is_some_and(|name| name != node.name) {
            return false;
        }
        if !self.ids.is_empty() {
            let id = node.attr("id");
            if !self.ids.iter().all(|want| id == Some(*want)) {
                return false;
            }
        }
        if self.classes.is_empty() {
            return true;
        }
        let Some(list) = node.attr("class") else {
            return false;
        };
        self.classes
            .iter()
            .all(|want| list.split_ascii_whitespace().any(|held| held == *want))
    }
}

/// One selector: its compounds left to right, each with the combinator that
/// joins it to the one before, and how specific the whole is.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Selector<'a> {
    parts: Vec<(Combinator, Compound<'a>)>,
    specificity: (u16, u16, u16),
}

impl Selector<'_> {
    /// Whether this selector matches the last element of `path`, whose
    /// ancestors precede it in document order.
    ///
    /// One left-to-right sweep of the path carrying, per compound, whether it
    /// matched the element just passed and whether it matched any earlier
    /// one. That answers both combinators exactly — a child needs the
    /// previous element, a descendant any earlier one — where matching
    /// right-to-left and taking the nearest ancestor does not: `.a > .b .c`
    /// against `.a > .b > .b > .c` would take the inner `.b`, find its parent
    /// is not `.a`, and wrongly fail.
    fn matches(&self, path: &[&Element<'_>]) -> bool {
        let Some(last) = self.parts.len().checked_sub(1) else {
            return false;
        };
        let mut previous = [false; MAX_SELECTOR_PARTS];
        let mut earlier = [false; MAX_SELECTOR_PARTS];
        let mut here = [false; MAX_SELECTOR_PARTS];
        for (index, node) in path.iter().enumerate() {
            let subject = index + 1 == path.len();
            for (part, (combinator, compound)) in self.parts.iter().enumerate() {
                // Only the last compound may match the subject, and only the
                // subject may satisfy it.
                here[part] = (part == last) == subject
                    && compound.matches(node)
                    && match part.checked_sub(1) {
                        None => true,
                        Some(before) => match combinator {
                            Combinator::Child => previous[before],
                            Combinator::Descendant => earlier[before],
                        },
                    };
            }
            for part in 0..self.parts.len() {
                earlier[part] |= here[part];
            }
            previous = here;
        }
        here[last]
    }
}

/// One parsed rule: which elements it applies to, what it sets, and where in
/// the document it was written.
#[derive(Clone, Debug)]
struct Rule<'a> {
    selector: Selector<'a>,
    block: usize,
    order: usize,
}

/// Every rule the document's own `<style>` elements define.
#[derive(Clone, Debug, Default)]
pub struct Stylesheet<'a> {
    blocks: Vec<Vec<Declaration<'a>>>,
    rules: Vec<Rule<'a>>,
}

impl<'a> Stylesheet<'a> {
    /// Parse every `<style>` element in the tree, in document order.
    ///
    /// A sheet whose `type` is neither absent nor `text/css`, or whose
    /// `media` names something other than every medium or the screen, is not
    /// for a rendered asset and is skipped.
    ///
    /// # Errors
    /// [`SvgError::TooComplex`] once the sheets exceed the rule or
    /// declaration bound.
    pub fn collect(root: &'a Element<'a>) -> Result<Self, SvgError> {
        let mut sheet = Self::default();
        sheet.gather(root)?;
        Ok(sheet)
    }

    /// Whether the document defined no rule at all, so no element need be
    /// matched against one.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    fn gather(&mut self, node: &'a Element<'a>) -> Result<(), SvgError> {
        if node.name == "style" && applies_to_screen(node) {
            self.parse(&node.text)?;
        }
        for child in &node.children {
            self.gather(child)?;
        }
        Ok(())
    }

    /// Parse one sheet's text into rules, dropping what the subset does not
    /// cover.
    fn parse(&mut self, text: &'a str) -> Result<(), SvgError> {
        let mut rest = text;
        loop {
            rest = skip_ignorable(rest);
            if rest.is_empty() {
                return Ok(());
            }
            if rest.starts_with('@') {
                rest = skip_at_rule(rest);
                continue;
            }
            let Some((prelude, body, after)) = take_block(rest) else {
                // No block to close the prelude: the rest of the sheet is one
                // unterminated rule and there is nothing further to read.
                return Ok(());
            };
            rest = after;
            let parsed: Vec<Selector<'a>> = split_top_level(prelude, b',')
                .filter_map(|one| parse_selector(one.trim()))
                .collect();
            if parsed.is_empty() {
                continue;
            }
            let declarations: Vec<Declaration<'a>> = declarations(body).collect();
            if declarations.is_empty() {
                continue;
            }
            if self.count() + declarations.len() > MAX_DECLARATIONS
                || self.rules.len() + parsed.len() > MAX_RULES
            {
                return Err(SvgError::TooComplex);
            }
            let block = self.blocks.len();
            self.blocks.push(declarations);
            for selector in parsed {
                let order = self.rules.len();
                self.rules.push(Rule {
                    selector,
                    block,
                    order,
                });
            }
        }
    }

    /// How many declarations the sheets hold so far.
    fn count(&self) -> usize {
        self.blocks.iter().map(Vec::len).sum()
    }

    /// Append the declarations matching the last element of `path` to `out`,
    /// in cascade order: normal declarations by specificity then source
    /// order, then the `!important` ones the same way.
    ///
    /// `out` is cleared first, so one buffer serves the whole walk.
    pub fn cascade(&self, path: &[&Element<'_>], out: &mut Vec<Declaration<'a>>) {
        out.clear();
        if self.rules.is_empty() || path.is_empty() {
            return;
        }
        let mut matched: Vec<&Rule<'a>> = self
            .rules
            .iter()
            .filter(|rule| rule.selector.matches(path))
            .collect();
        matched.sort_unstable_by_key(|rule| (rule.selector.specificity, rule.order));
        for important in [false, true] {
            for rule in &matched {
                let Some(block) = self.blocks.get(rule.block) else {
                    continue;
                };
                out.extend(
                    block
                        .iter()
                        .filter(|declaration| declaration.important == important),
                );
            }
        }
    }
}

/// Whether a `<style>` element's `type` and `media` admit it for a rendered
/// asset.
fn applies_to_screen(node: &Element<'_>) -> bool {
    let typed = node
        .attr("type")
        .is_none_or(|value| value.trim().eq_ignore_ascii_case("text/css"));
    let medium = node.attr("media").is_none_or(|value| {
        value.split(',').any(|one| {
            let one = one.trim();
            one.is_empty() || one.eq_ignore_ascii_case("all") || one.eq_ignore_ascii_case("screen")
        })
    });
    typed && medium
}

/// `text` from its first byte that is neither whitespace nor part of a
/// comment.
fn skip_ignorable(text: &str) -> &str {
    let mut rest = text.trim_start();
    while let Some(body) = rest.strip_prefix("/*") {
        rest = match body.find("*/") {
            Some(end) => body.get(end + 2..).unwrap_or("").trim_start(),
            // An unterminated comment runs to the end of the sheet.
            None => "",
        };
    }
    rest
}

/// `text` past one at-rule: its statement up to `;`, or its whole block.
fn skip_at_rule(text: &str) -> &str {
    let statement = top_level_index(text, b';');
    let block = take_block(text);
    match (statement, &block) {
        (Some(end), Some((prelude, _, _))) if end < prelude.len() => {
            text.get(end + 1..).unwrap_or("")
        }
        (_, Some((_, _, after))) => after,
        (Some(end), None) => text.get(end + 1..).unwrap_or(""),
        (None, None) => "",
    }
}

/// Split `text` at its first top-level `{ … }` into what precedes it, its
/// body, and what follows.
fn take_block(text: &str) -> Option<(&str, &str, &str)> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut open: Option<usize> = None;
    let mut depth = 0_u32;
    for (index, byte) in bytes.iter().copied().enumerate() {
        match quote {
            Some(mark) if byte == mark => quote = None,
            Some(_) => {}
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'{' => {
                    depth += 1;
                    if open.is_none() {
                        open = Some(index);
                    }
                }
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let start = open?;
                        return Some((
                            text.get(..start)?,
                            text.get(start + 1..index)?,
                            text.get(index + 1..)?,
                        ));
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// Parse one selector, or `None` when it uses something outside the subset.
fn parse_selector(text: &str) -> Option<Selector<'_>> {
    if text.is_empty() {
        return None;
    }
    let mut parts: Vec<(Combinator, Compound<'_>)> = Vec::new();
    let mut specificity = (0_u16, 0_u16, 0_u16);
    let mut combinator = Combinator::Descendant;
    let mut rest = text;
    loop {
        let (compound, after) = parse_compound(rest, &mut specificity)?;
        if parts.len() == MAX_SELECTOR_PARTS {
            return None;
        }
        parts.push((combinator, compound));
        let spaced = after.trim_start();
        if spaced.is_empty() {
            return Some(Selector { parts, specificity });
        }
        combinator = match spaced.strip_prefix('>') {
            Some(_) => Combinator::Child,
            // A space between compounds is the descendant combinator; no
            // space at all means the compound did not end where it should.
            None if spaced.len() < after.len() => Combinator::Descendant,
            None => return None,
        };
        rest = spaced.strip_prefix('>').unwrap_or(spaced).trim_start();
        if rest.is_empty() {
            return None;
        }
    }
}

/// Parse one compound selector from the head of `text`, adding what it
/// contributes to `specificity`.
fn parse_compound<'a>(
    text: &'a str,
    specificity: &mut (u16, u16, u16),
) -> Option<(Compound<'a>, &'a str)> {
    let mut compound = Compound::default();
    let mut rest = text;
    let mut first = true;
    loop {
        let head = rest.as_bytes().first().copied();
        match head {
            Some(b'*') if first => {
                rest = rest.get(1..)?;
            }
            Some(marker @ (b'.' | b'#')) => {
                let name = leading_ident(rest.get(1..)?);
                if name.is_empty()
                    || compound.ids.len() + compound.classes.len() == MAX_COMPOUND_CONDITIONS
                {
                    return None;
                }
                rest = rest.get(1 + name.len()..)?;
                if marker == b'#' {
                    compound.ids.push(name);
                    specificity.0 = specificity.0.saturating_add(1);
                } else {
                    compound.classes.push(name);
                    specificity.1 = specificity.1.saturating_add(1);
                }
            }
            Some(byte) if first && is_ident_start(byte) => {
                let name = leading_ident(rest);
                compound.element = Some(name);
                specificity.2 = specificity.2.saturating_add(1);
                rest = rest.get(name.len()..)?;
            }
            // A combinator or the selector's end closes the compound;
            // anything else — an attribute selector, a pseudo-class, a
            // sibling combinator — is outside the subset. A compound that
            // consumed nothing is not one at all, which is what refuses a
            // selector beginning with a combinator.
            None | Some(b' ' | b'\t' | b'\r' | b'\n' | b'>') => {
                return (rest.len() < text.len()).then_some((compound, rest))
            }
            Some(_) => return None,
        }
        first = false;
    }
}

/// The identifier at the head of `text`.
fn leading_ident(text: &str) -> &str {
    let end = text
        .bytes()
        .position(|byte| !is_ident_byte(byte))
        .unwrap_or(text.len());
    text.get(..end).unwrap_or("")
}

/// Whether `byte` may begin a CSS identifier this decoder recognises.
const fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

/// Whether `byte` may continue one.
const fn is_ident_byte(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit() || byte == b'-'
}

#[cfg(test)]
#[path = "css_tests.rs"]
mod tests;
