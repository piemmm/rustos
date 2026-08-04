# `tairix-wc` — print newline, word, and byte counts for each file

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Commands/wc.app/` so the shell resolves the bare word
`wc` to it. `wc` is the GNU coreutils tool: it counts lines, words,
characters, bytes, and the maximum line display width of each file
operand (or standard input), printing one aligned row per input in the
fixed lines/words/chars/bytes/max-line order and a `total` row per
`--total`. The GNU surface is implemented in full: `-c`/`--bytes`,
`-m`/`--chars`, `-l`/`--lines`, `-w`/`--words`, `-L`/`--max-line-length`,
`--total={auto,always,only,never}` (matched like GNU `argmatch`, so an
unambiguous prefix is accepted), `--files0-from=F` (NUL-separated operand
list, `-` from standard input, refusing file operands alongside it),
`--`, short-option bundles, and option/operand permutation — including
the GNU column-width rule (columns sized from the summed regular-file
operand sizes, the 7-column minimum for a non-regular input, unpadded
single-count/single-input and `--total=only` rows). `-h`/`-?`/`--help`
render the tool's own short help from its bundled `Help/` tree through
the shared `lib/help` engine, in the locale the inherited `LANG`
variable names.

Counting streams in constant memory with an incremental UTF-8 decoder:
`-m` counts decoded characters (an encoding-error byte counts as a byte,
not a character), words are maximal runs of non-whitespace, and `-L`
measures display columns through the one OS-wide width definition
(`tairix_vt::char_width` — the same table the OS terminal lays cells out
with), with tabs advancing to 8-column stops. An input that cannot be
read is diagnosed on standard error and the run continues; a row whose
read failed mid-stream still prints its partial counts, exactly as in
the GNU tool.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
audited `tairix-abi` vocabulary, the shared `tairix-help` engine, and
`tairix-vt`, so it never links a kernel or driver crate. Its manifest
(`AppInfo.toml`) requests `CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, and
`CAP_FS_ACCESS` — within the session baseline — and the secured VFS
still authorises every path per-inode under the caller's attested
identity.

## Usage

```
wc [-clmwL] [--total <when>] [--files0-from <file>] [--] [file...]

  -c, --bytes            print the byte count
  -m, --chars            print the character count
  -l, --lines            print the newline count
  -w, --words            print the word count
  -L, --max-line-length  print the widest line's display width
  --files0-from <file>   NUL-separated operand list (- reads stdin)
  --total <when>         auto | always | only | never
  -h, -?                 show this command's own short help
```

## Exit status

- `0` — every input was counted (or the short help was written).
- `1` — an input could not be read, or the output could not be delivered.
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
