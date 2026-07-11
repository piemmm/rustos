# rustos-complete

Shared filename-completion engine for RustOS (`lib/complete`).

Stability tier: `experimental`.

More than one interactive program completes a partially typed filesystem
path — the shell's Tab completion and the tree file manager's destination
prompts. The policy is identical wherever it happens, so it lives here once
and every consumer imports it instead of embedding a private engine that
would drift: the directory-part/leaf split, the hidden-name (dotfile) rule,
the leaf-prefix candidate filter, and the longest-common-prefix Tab
discipline.

Presentation stays with each consumer: the shell escapes candidates so the
completed line still lexes as one word and merges path candidates with its
command and resource-reference candidates; the file manager inserts
candidates verbatim into a plain path prompt.

## API

- `DirEntryInfo` — one listed entry: name and whether it is a directory.
- `DirLister` — the injected, read-only directory-listing seam; listing is
  the only filesystem operation completion may perform.
- `split_path_word(word) -> (dir_part, leaf)` — the split at the last `/`.
- `list_target(dir_part, bare_dir)` — the directory the candidates are
  listed from (`bare_dir` is the consumer's notion of "here").
- `path_matches(word, bare_dir, lister)` — the name-ordered candidates
  extending the word's leaf prefix (dotfiles only when the prefix asks).
- `common_prefix(items)` — the longest common prefix, for the Tab
  extension when several candidates share a stem.

## Fail-closed

A listing the kernel refuses completes to nothing, never a guess. The seam
is read-only by construction, so completion can never create, write, or
run anything.
