# rustos-glob

Shared filename-glob matcher for RustOS (`lib/glob`).

Several RustOS components need to match a name against a shell-style wildcard
pattern — the shell's filename generation and interactive completion first, and
later the file browser, `find`-class tooling, and any other wildcard resolver.
That matching is identical wherever it happens, so it lives here once and every
consumer imports it, rather than each embedding a private matcher. The shell
does not own a glob engine; it links this one.

## Glob, not regex

This crate implements POSIX shell **globbing**, deliberately not a general
regular-expression engine. Globs are what a shell expands and can be matched in
bounded time without backtracking blow-up. A full regex dialect invites
catastrophic backtracking on a hostile pattern and no current consumer needs
it; if one ever does, it is a separate engine, not a feature bolted on here.

## API

- `Pattern::new(pattern) -> Result<Pattern, GlobError>` — compile and validate a
  pattern. This is the only fallible step.
- `Pattern::matches(candidate) -> bool` — whether the whole candidate matches
  (anchored). Infallible and never panics.
- `GlobError` — why a pattern was rejected.
- `MAX_PATTERN_LEN` / `MAX_TOKENS` / `MAX_CLASS_ITEMS` — the fixed security
  bounds on an untrusted pattern.

## Syntax

- `*` — any run of zero or more characters.
- `?` — exactly one character.
- `[abc]`, `[a-z]`, `[!abc]` / `[^abc]` — bracket expression (membership,
  ranges, negation). A `]` first in the expression, or a `-` first or last, is
  literal.
- `\` — escapes the next character.

The path separator `/` is an ordinary character; a caller globbing a
multi-segment path splits it and matches each segment, so separator policy is
not duplicated here.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`.
- Fail-closed: a malformed or over-large pattern is a typed `GlobError`, never a
  silent "match it literally" fallback. It is a matcher of untrusted patterns,
  so pattern length, token count, and bracket size are fixed security bounds.
- No catastrophic backtracking: matching is the classic two-pointer glob
  algorithm, `O(tokens * candidate-chars)`, with no recursion.

## Stability

Tier: `experimental`.
