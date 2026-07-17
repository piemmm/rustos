# `tairix-whoami` — print the current user's account name

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Apps/whoami.app/` so the shell resolves the bare word
`whoami` to it. `whoami` is the GNU coreutils tool: it prints the user name
associated with the caller's identity and nothing else. It takes no
operands (`extra operand`) and knows no options beyond the reserved
`-h`/`-?`/`--help` short-help switches, which render the tool's own Help
document from its bundled `Help/` tree through the shared `lib/help`
engine, in the locale the inherited `LANG` variable names, falling back to
the usage banner when the tree is unavailable.

TAIRiX has no `/etc/passwd` and no ambient identity file. The uid comes
from the caller's kernel-attested origin record (the ungated `self_origin`
syscall — a pure self-observer), and the uid → name pairing comes from the
ungated `USER_DIRECTORY` query the System Information API serves
(`sysinfod`; the public uid + username directory, no credential material)
through the shared `lib/procinfo` account-directory walk — the same one
`top`'s `USER` column uses, never a second copy. A uid with no directory
entry is the GNU `cannot find name for user ID` diagnostic; a failed
directory walk is reported as a service error, never misreported as a
missing name.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
shared `tairix-abi`, `tairix-procinfo`, and `tairix-help` crates, so it
never links a kernel or driver crate. Its manifest (`AppInfo.toml`)
requests `CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` — within the session
baseline — and the secured VFS still authorises every path per-inode under
the caller's attested identity.

## Usage

```
whoami

  -h, -?         show this command's own short help
```

Exit codes: `0` when the name (or a requested short help) was written; `1`
when the identity read, the directory lookup, or the output failed; `2` on
a usage error.

## Layout

- `src/lib.rs` — the pure, host-testable core: the GNU argument grammar,
  the typed errors, the injected `Identity`/`Transport`/`Output` seams,
  and the lookup engine, with its unit tests (including the per-locale
  switch-drift pin over the on-disk `Help/` tree).
- `src/run.rs` — the freestanding `Run` binary: wires `self_origin`,
  `IpcTransport`, `RtOutput`, and `BundleHelp` to the pure core; an inert
  stub on the host.
- `AppInfo.toml` — the signed-manifest source the app-bundle composer
  discovers.
- `Help/` — the internationalised structured-Markdown help tree
  (`en-US` canonical plus the standing required locales), authored here
  and planted onto `/System` by the image builder; never embedded in the
  binary.
