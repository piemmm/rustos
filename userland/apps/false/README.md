# `rustos-false` — do nothing, unsuccessfully

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Apps/false.app/` so the shell resolves the bare word
`false` to it. `false` is the GNU coreutils tool: it ignores every
argument and exits `1`, giving scripts a command that always fails. A
**first** argument of `-h`/`-?`/`--help` — the position GNU honours
`--help` in — renders the tool's own short help from its bundled `Help/`
tree through the shared `lib/help` engine, in the locale the inherited
`LANG` variable names, falling back to the usage banner when the tree is
unavailable. One documented divergence: a served short help exits `0`
(the RustOS short-help convention), where GNU `false --help` exits `1`.

The crate is `no_std` (no `alloc` in the library), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its only dependency is the
shared `rustos-help` crate, so it never links a kernel or driver crate.
Its manifest (`AppInfo.toml`) requests `CAP_CONSOLE_WRITE` and
`CAP_FS_ACCESS` — within the session baseline — and the secured VFS still
authorises every path per-inode under the caller's attested identity.

## Usage

```
false [ignored arguments]

  -h, -?         (first argument only) show this command's own short help
```

## Exit status

- `1` — always (the tool's whole purpose).
- `0` — a requested short help was served.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
