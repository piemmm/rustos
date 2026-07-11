# `rustos-complete` — the shared filename-completion engine

`lib/complete` is the one definition of how a partially typed filesystem
path is completed interactively. Two programs complete paths today — the
shell's Tab completion (`userland/shell/elsh`) and the `fstree` file
manager's destination prompts — and the policy is identical in both, so
it lives here once and each consumer imports it instead of embedding an
engine that would drift.

## The policy

- **The split.** A path word divides at its last `/` into a directory
  part (kept on the insert, trailing separator included) and the leaf
  prefix being completed (`split_path_word`).
- **The listing target.** Candidates come from the directory the word
  names: the consumer's notion of "here" for a bare word (the shell's
  working directory, the file manager's listed directory), the root for
  `/`, otherwise the directory part itself (`list_target`).
- **The candidates.** The entries of that directory whose names extend
  the leaf prefix, name-ordered; dot-named (hidden) entries are offered
  only when the prefix itself starts with a dot (`path_matches`, over
  the `DirEntryInfo` vocabulary).
- **The Tab discipline.** A unique candidate completes the word (the
  consumer decides its closing: the shell appends a space, a directory
  stays open); several candidates extend to their longest common prefix
  (`common_prefix`) or are listed.

## What stays with the consumer

Presentation and authority. The shell escapes each insert so the
completed line still lexes as one word, and merges path candidates with
its command-word and resource-reference candidate classes; the file
manager inserts candidates verbatim into a plain path prompt and
resolves relative directory parts against the same base its submit path
uses. Neither re-derives the policy.

## Read-only and fail-closed

The engine reaches the filesystem only through the injected `DirLister`
seam, whose sole operation is listing a directory — completion can never
create, write, or run anything. A listing the kernel refuses yields an
empty candidate set, never a guess; whether the caller may *see* a
directory is decided kernel-side exactly as for any other listing.

The crate is `no_std` + `alloc`, forbids `unsafe`, and is host-tested
(the split, the dotfile rule, the root and sub-path listing targets, the
refused-listing degradation, and the common-prefix cases).
