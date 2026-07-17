# `rustos-sysconfig` — the boot-time system-configuration store engine

`lib/sysconfig` is the single definition of RustOS's administrator-settable
boot-time configuration store: the document at
`/System/Settings/Configuration/system.conf`
(`rustos_sysconfig::CONFIG_PATH`). It owns the line grammar, the **closed**
key registry, each key's typed value set, the bounded fail-closed parser,
and the canonical render. The `configure` command app
(`userland/apps/configure`) writes the store through this engine and every
boot-time consumer reads it through the same engine, so producer and
consumer can never diverge.

## The store

One small text document: `key value` settings, one per line; `#` comments
and blank lines are ignored; every key appears at most once. The document
is bounded (`MAX_CONFIG_LEN`, 4 KiB) and the parser refuses — never guesses
at — anything it does not fully understand: an unknown key, a value outside
its key's set, a duplicate, an oversized document (`ConfigError`). An
**absent** store is not an error: it means every key is at its documented
default (`SystemConfig::default`), which is how a fresh installation and a
pre-unlock boot behave.

The store lives inside `/System/Settings` on the encrypted root volume, so
it can only be read after the operator's `ARXFS passphrase:`
unlocks the root — a boot-time consumer therefore parses it post-unlock by
construction, and runs on defaults before that. Write authority is the
existing `/System/Settings` per-inode policy under the caller's
kernel-attested identity: no new capability exists for it, and an ordinary
account can read the settings but not change them.

## The registry

| Key            | Values               | Consumer                                                        |
|----------------|----------------------|-----------------------------------------------------------------|
| `os.loginType` | `text` \| `graphical` | `login`: the boot-default session type (`text` keeps the prompt; `graphical` starts the desktop directly when one is available, degrading to text otherwise) |

Adding a key is adding a `Key` variant (plus its `SystemConfig` field and
match arms) **and** its consumer in the same change — the compiler then
forces every reader to state what the new key means for it. There is no
free-form key namespace and no second store.

## API shape

- `SystemConfig::parse(&str) -> Result<SystemConfig, ConfigError>` — the
  bounded, fail-closed parse.
- `SystemConfig::render() -> String` — the canonical document (header
  comment plus every registry key, so render→parse round-trips exactly and
  the file a user opens always shows the whole registry).
- `SystemConfig::get/set(Key, …)` — the typed per-key access `configure`
  lists and edits through.
- `Key::{ALL, name, from_name, values}` — the closed registry, for
  listings and stated-choice diagnostics.

The crate is `no_std` + `alloc`, performs no I/O, holds no authority, and
is host-unit-tested in `src/lib.rs`. Stability tier: experimental
(`lib/sysconfig/README.md`).
