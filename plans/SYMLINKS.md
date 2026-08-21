# SYMLINKS — first-class links

Binding under `AGENTS.md` (§3, §15.18). This plan owns everything about
links, symbolic and hard: the ABI kind and flag, the syscalls, the driver
contract, VFS resolution, the on-disk spellings per format, the link-count
lifecycle, and the userland surface. It exists because nothing in the tree
had links at all — no `FileKind`, no syscall, no `NodeKind`, no on-disk kind
— and a desktop shortcut is a symlink to an app bundle
(`plans/PINBOARD.md`, `plans/NEW-TASKBAR.md` T16).

`done` — **S1–S7 complete.** The settled decisions and bounds below are
binding on anything that touches a link; each stage's section records what
that part now guarantees. Hard links were an open User decision, were
approved, and have landed, so this plan's subject is links generally rather
than only the symbolic kind.

## Read first (§15.18)

- `AGENTS.md` §2.13 (evolve in place), §5.4 (fail closed), §9 (the ABI
  discipline a new syscall must meet), §16.5 (a bundle is self-contained),
  §19.3 (supply-chain integrity), §27 (a foundational primitive is complete).
- `docs/src/filesystem/drives.md` — the binding storage-namespace spec.
- `docs/src/filesystem/arxfs-spec.md` — the native format's binding spec.
- `plans/PINBOARD.md`, `plans/NEW-TASKBAR.md` — the desktop shortcut that
  consumes this.

---

## Settled decisions

1. **A link's target is stored verbatim and is never resolved at creation.**
   It is *data*, not a path the kernel walks, so `fs_symlink` checks only the
   caller's right to create a name in the link's own parent. A link may
   legitimately dangle, and creating one grants no authority over what it
   names — authority is decided at each later *use*, per component, under the
   caller's attested identity.

2. **`lstat` is spelled `OpenFlags::NO_FOLLOW`, not a `fs_stat` operand.**
   `fs_stat` is fd-based, so the fd holder already chose its follow posture
   at open; a second operand on `fs_stat` would duplicate the flag and create
   a contradiction case (opened `NO_FOLLOW`, stat says follow). One decision
   point, recorded on the open handle. *This supersedes the "`StatFlags`
   operand" sketch in the originating task, which assumed a POSIX
   `stat(path)`/`lstat(path)` pair this ABI does not have.*

3. **Targets are full POSIX, including `..`.** Decided by the User against
   the recommendation, and recorded here as theirs: the alternative was
   absolute-and-normalised targets validated by the one `Path::parse`, which
   would have kept the VFS entirely free of traversal logic. The security
   burden this creates is carried by decisions 4 and 5.

4. **`..` is resolved physically, never lexically.** Resolution walks a stack
   of real nodes and `..` pops that stack, so it names the directory the walk
   actually came through — not one a link's spelling suggests. Collapsing
   `..` textually before the walk is the classic symlink-escape bug
   (`/a/link/../b` with `link -> /elsewhere` must reach `/elsewhere/../b`,
   never `/a/b`) and is forbidden here.

5. **A link cannot escape the volume that stores it.** The walk's stack
   starts at the mounted volume's own root and `..` never pops past index 0
   (`/..` is `/`, as POSIX specifies), so an absolute target on a foreign
   volume resolves against *that volume's* root. A USB stick's link therefore
   cannot name `/System/Security`. This is deliberately stricter than POSIX,
   where an absolute target names the global root.

6. **Following a link never bypasses a permission check.** A spliced target's
   components are authorised exactly as typed ones are: search permission is
   required on every directory the resolution traverses, whether the caller
   spelled it or a link supplied it.

7. **A link is not byte-readable.** Its content is a path, reached only with
   `fs_readlink`; `fs_read` against one fails closed. Symmetrically
   `FilesystemWrite::create` refuses `NodeKind::Symlink` — a link carries a
   target that call has nowhere to put — so links are created only by
   `create_link`.

8. **A format without links refuses, never approximates.**
   `FilesystemRead::read_link`, `FilesystemWrite::create_link`, and
   `FilesystemWrite::link` all default to `Unsupported`, so a driver that
   stores no link of the asked-for kind fails closed rather than substituting
   a copy, an empty file, a file whose contents merely look like a path, or —
   for a second name — a second file that diverges on the next write.

9. **Links are refused where following one would defeat a security
   boundary**: inside an app bundle (§16.5 — the bundle is self-contained and
   its signature covers only what is inside it) and inside the driver store
   (§19.3 — a name outside the signed store must never decide what loads).
   Both refuse on the *listing's* structural kind, so an interior link is
   refused at the level it appears and is never descended. The bundle also
   refuses at the **read** (`FsBundleStore::read_file` keeps a final link and
   rejects one), because `AppInfo` is read before the content walk and would
   otherwise have been signature-checked against bytes from outside the
   bundle.

---

## Bounds (fixed security bounds, never capacities — §24.4)

| Bound | Value | Why |
|---|---|---|
| `FS_SYMLINK_MAX` | `FS_PATH_MAX` (4096) | A target *is* a path; derived, never restated, so the two cannot drift |
| `SYMLINK_HOP_MAX` | 40 | The conventional Unix `MAXSYMLINKS`; a cycle is refused, never walked |
| `MAX_RESOLVE_STEPS` | `MAX_PATH_COMPONENTS × (SYMLINK_HOP_MAX + 1)` | Derived from the only two step sources, so one resolution's work is bounded even when every component is a link |

`Errno::LinkLoop` / `VfsError::LinkLoop` answer every one of them.

---

## Stage S1 — ABI, driver contract, resolution — **landed**

- `lib/abi`: `FileKind::Symlink` (+ `is_symlink`, `mode_string` → `l`),
  `OpenFlags::NO_FOLLOW` (bit 7, `DEFINED_BITS`-checked),
  `FS_SYMLINK_MAX`, `Errno::LinkLoop` (42).
- `lib/abi::syscalls`: `fs_symlink` (113) and `fs_readlink` (114), specs at
  their dense array indices, with the cross-checked `kernel/syscall` half
  (handler trait + dispatch) and the `lib/abi-sys` C-callable stubs. The
  `include/` view is regenerated by `cargo xtask c-header --write`, never
  hand-edited.
- Driver contract: `NodeKind::Symlink`, `FilesystemRead::read_link`,
  `FilesystemWrite::create_link`, both defaulting to `Unsupported`.
- VFS: per-component resolution in `kernel/core/src/fs/delegate.rs`
  (`resolve_final` + `FinalLink`), the link-target grammar in
  `fs/path.rs` (`parse_link_target`, `TargetStep`) kept separate from
  `Path::parse` so the *caller* boundary stays as strict as it was, and
  `memfs` grew a real link node so the matrix runs against a live backing.
- Fail-closed arms at every site the widened enums reach, including the
  bundle walk, the driver store, the foreign-volume mode mapping, ext4's
  kind decode, and the file-manager transfer engine.

Tests: resolution follows a final and an interior link; a cycle, a
self-cycle, and an over-budget chain are refused with `LinkLoop`; a dangling
link reports `NotFound`; `..` after a link is physical; `..` cannot climb out
of the volume; search permission is enforced on a directory a link leads
through; the target grammar's bounds; and that caller paths still refuse
`.`/`..`.

## Stage S2 — the syscalls reach the VFS — **landed**

Both calls are real end to end: `Vfs::{symlink_via, readlink_via}` (+
`_secured`), `FilesystemService::{symlink, readlink}` across every
implementor, the `kernel/core/src/syscalls.rs` handlers with the
`FsAuditDetail::Symlink` record for the mutating half, and the `lib/rt`
`fs_symlink`/`fs_readlink` wrappers.

**The follow posture is a property of the descriptor, derived once.**
`FinalLink::for_open` is the single mapping from `OpenFlags::NO_FOLLOW`, and
every operation served for a descriptor re-derives it from the handle rather
than taking it as an operand — `fs_stat` (the `lstat` reading) and
`fs_readdir` (a link is not a directory to a `NO_FOLLOW` handle), plus the
wait-set's file member, which watches the node its open named. `open` honours
it too, which is what makes `lstat` reachable at all: a resolve-only
`NO_FOLLOW` handle on a *dangling* link is exactly what `ls -l` needs, and
following would report it absent. Asking that open for byte access to
something that really is a link is `LinkLoop`, as `OpenFlags::NO_FOLLOW`'s
own contract states.

**A target's grammar is checked at creation; the target is still not
resolved.** `create_link` runs the caller's bytes through the one
`parse_link_target` before anything is written, so a target this resolver
could never walk — empty, over `FS_SYMLINK_MAX`, an over-long component, more
than `MAX_PATH_COMPONENTS` steps, not UTF-8 — is refused rather than stored as
a link that can only ever fail. Parsing is not resolving: `..` and relative
spellings stay legal (decision 3), nothing is looked up, and a link may still
dangle (decision 1).

Note what this does *not* change: a path-recording descriptor re-resolves its
path, so a rename between open and stat can still name a different node. That
is a pre-existing property of every path-backed handle, not something links
introduce, and it is called out here rather than left implied.

Tests: `Keep` stats the link and `Follow` its target (different nodes, not
just different sizes); a dangling link is statable only under `Keep`; `Keep`
refuses to list a link as a directory; `readlink` returns the target verbatim
(including a relative one carrying `..`), reads a dangling link, refuses a
file, a directory and an absent path, and needs search permission on the way;
a created link resolves, reads back, and is owned by its creator; creation
refuses an existing name, an unwalkable target, a non-writable parent and a
read-only mount, leaving no name behind; a format without `create_link`
answers `NotSupported` while a plain create on it still works; the
posture-from-flags mapping; at the service seam the round trip, the read-only
refusal, both `open` refusals and the resolve-only handle that succeeds, a
listing that reports a link entry as a link; and at the handler seam the
posture reaching the service per descriptor for both `fs_stat` and
`fs_readdir`, the audit record on success and on refusal, the boundary
refusals of an empty/over-long/non-UTF-8 target, and `fs_readlink` copying
the whole target out or failing closed.

## Stage S3 — on-disk spellings — **landed**

**The follow posture is a property of each operation, and a walk reports a
*place*.** `DelegatedFs` resolves to the directory that holds the final name
*and* that name (`Walk`/`Place`), because the driver mutation surface is keyed
`(dir, name)` rather than by node. Under `FinalLink::Follow` that place is the
*target's* place, which is what makes `write`, `truncate`, truncate-on-open,
and append reach the target as POSIX requires; `unlink`, `rmdir`, `rename`,
`mkdir`, and `symlink` keep the name as typed. `create` takes the posture
explicitly, so `open`-with-`O_CREAT` follows (creating through a dangling link
creates the target) while `mkdir` does not (`AlreadyExists` over a link, live
or dangling). One walk serves both the read and the write side, so a vacant
final name is a *place* to the write side and `NotFound` to the read side
rather than two resolutions.

Two consequences recorded rather than left implied: the write permission this
VFS asks for on a write's parent — a pre-existing, non-POSIX choice, deliberately
unchanged — now applies to the parent of the *resolved* node, i.e. the
directory the bytes really land in; and the parent is authorised **before** the
occupant is inspected, so a caller who may not write the directory still learns
nothing about what the name holds.

**ARXFS** stores a link as an inode of on-disk kind `3` whose target is its
**node data**, so it reuses the whole existing pipeline — extents, checksum,
AEAD, logical hash, dedupe — with no second storage path. The reasoning that
made that the right choice, and the consequences that fall out of it, are in
`docs/src/filesystem/arxfs-spec.md` §20 (binding): the compressor is never
reached because a target is under one cluster; dedupe applies because excluding
one object kind would be a forbidden knob; and a link's blocks are *data*, so
the `is_dir` boolean at the accounting, freeing, and scrub sites was already
the correct discriminator once renamed to say so. `Inode::kind` became an
`InodeKind` enum precisely so the compiler forced that question at every site
— the ext4-S1 defect class — which surfaced the real defects: `node_info` and
`read_dir` reported a link as a regular file, `read_at`/`write_at`/`truncate`
would have read and rewritten a target as file bytes, `reflink` would have
cloned a link into a regular file holding the target's text, and `rescue` would
have emitted a target through a byte sink. All now refuse or skip fail-closed.

**The "older reader refuses rather than misreads" guarantee is an incompat
feature word, not a format-version bump.** No feature field existed, so one was
added: a `u64` at an offset every existing v2 volume already has zeroed, so no
v2 volume is invalidated — which a `FORMAT_VERSION` bump to 3 would have done
to all of them. `Superblock::try_decode` returns `Unsupported` for an
authenticated slot declaring a bit outside the supported set, so the refusal
states its reason instead of the ring scan reporting the volume as
unrecognisable. `INCOMPAT_SYMLINKS` is set by the **first** link, in that
transaction (and rolled back with it), so a link-free volume stays mountable by
a link-unaware reader; `check` widens a word that understates the volume.

**ext4** reads both spellings — fast (inline `i_block`) and slow
(block-backed) — discriminated as Linux's `ext4_inode_is_fast_symlink` does,
with the cluster size read from the superblock so `bigalloc` is exact rather
than approximated. It deliberately does **not** author links: `create_link`
stays `Unsupported`, which the VFS surfaces as `NotSupported`. An inline-data
link is the same honest refusal, since this driver decodes no inline data.

**FAT32 / ADFS**: no link object type exists in either format, so both refuse
creation and report the limit. `delegate_tests.rs`'s `NoLinksFs` fixture stands
for exactly them.

**Closed in passing** (`AGENTS.md` §2.18): the ARXFS companion mirror covered a
copy that failed to *authenticate* but not one that failed to **read**, so a
single-sector media error defeated the redundancy it was meant to survive. All
three mirrored read paths — superblock slot, transaction root, metadata block —
now treat an unreadable copy as an absent one, with a read-fault injection hook
on the test device and a regression test each.

Tests: writing and truncating through a link reach the target and leave the link
intact; a write through a link needs write permission on the *target's* parent;
a write or truncate through a dangling link is `NotFound` and creates nothing;
`O_CREAT` through a dangling link creates the target and leaves the link a link;
`mkdir` over a live or dangling link is `AlreadyExists`; unlink and rename still
act on the link. In ARXFS: kind and target length reported, target round-trip
across a remount, a maximum-length multi-block target with one-byte-over and
empty refused, every byte-content refusal (`read_at`/`write_at`/`truncate`/
`reflink`), `create` refusing the kind and `read_link` refusing a non-link and
an undersized buffer, a link listed as a link with real allocated blocks whose
accounting survives a rebuild, rename replacing a link with a file, the feature
declared only on first use and undeclared after a rollback, an unsupported
declared feature refusing the mount, `check` widening an understated word,
scrub verifying a link's blocks as data, rescue counting rather than extracting
one, and each mirrored read path recovering from an unreadable primary. In ext4:
both spellings, a short target that is nevertheless block-backed, the non-link
and undersized-buffer refusals, the byte-read and creation refusals, the
inline-data limit, and a zero-length target refused as corrupt. The `fuzz_mount`
corpus gained a link in its base image and drives `read_link` over every entry
it decodes.

## Stage S4 — userland — **landed**

**`userland/apps/ln`** is a new command bundle carrying the GNU surface
`-s`, `-f`, `-i`, `-n`, `-t`, `-T`, `-v`, `--` and every operand shape
(`target`, `target link_name`, `target... directory`, `-t dir`). It is a
planner over three seams (`FileSystem`/`Prompt`/`Output`), so every operand
and replacement decision is host-provable.

**`-s` chooses the kind; the switches that do not depend on a kind stay
refused.** `-b`/`-S` because no backup machinery exists anywhere in the tree
(`cp`/`mv` omit them too), and `-r` because a target relative to the link's
own directory needs a canonicalising resolution the ABI does not offer, and a
*lexical* one would name a different node the moment a link were involved —
decision 4's own trap. (S6 made `-L`/`-P`/`-d`/`-F` real; this stage shipped
`-s` alone.)

**A replacement removes the name first.** `-f`, and an approved `-i`, unlink
the existing name *before* creating the link — a create or truncate follows a
final link, so leaving one in place would act on whatever it pointed at. A
directory is never replaced. This is the same rule `fstree` and the file
manager now apply.

**`ls`** implements the full GNU four-state dereference posture
(`Dereference`), because three is not enough to describe the tool: `-l`/`-d`/
`-F` show every link as itself; the default resolves a *command-line* link
**to a directory** (so `ls linkdir` lists it) and nothing else; `-H` resolves
every operand; `-L` resolves everywhere. The posture selects a per-path
`FinalLink`, so one listing takes both readings — which is what `-H` is for.
The long format prints `name -> target`, the target verbatim.

The kind question is now asked by the *type*: `Row` carries the entry's kind
(never unknown — the directory stream supplies it even when a stat is
refused) beside an `Option<Metadata>`, so every former `is_dir()` site is an
exhaustive `match`. A path that cannot be inspected no longer ends the
listing: the reason goes to standard error, the path is skipped, and an
`Outcome` grades how serious it was into GNU's exit status (`0` / `1` for a
problem inside a listing / `2` for a command-line operand). A skipped entry
renders its type letter and `?` for every stat-derived cell. `-R` under `-L`
can walk into a directory a link names, so the walk carries the ancestor
chain of node identities and reports `not listing already-listed directory`
rather than looping. `lib/vt` gained `Role::Link` (bold cyan, GNU's `ln=`),
and the long format paints a link's target in the role of what it names.

**`lib/browse`** now carries *both* facts about a link, because a file
manager needs both: `EntryKind::Link(LinkTarget)` shows the link while
naming what it resolves to, and `Entry::target()` carries the stored spelling
for display and launch. Bundle-ness is decided from the **target's** leaf
name, so a desktop shortcut named `Editor` pointing at `/Apps/Editor.app`
reads as an application — the shape S5 builds on. `is_directory_backed()` is
`false` for every link however it resolves (a link is a leaf: removing one
unlinks it, and recursing into one would walk a tree the name only points
at), while `is_directory()`/`is_bundle()` follow the target, and
`resolved()` is the content reading a sort, icon, or association takes. A
`LinkReader` seam describes the links a listing reports (one production
`RtLinkReader` under the crate's new `rt` feature, shared by the file
manager, the desktop, the picker, and the wallpaper catalog); `NoLinks` is
the honest "describes nothing" for a tree that cannot hold one. Activation
descends or opens *through* the link and launches a bundle by its
**resolved** path, because the app-load gate judges the path it is handed.

**`userland/apps/fstree`** has the real operations and `OpError::IsALink` is
deleted. Copy recreates the link with the same stored target; move and
delete act on the link. Two defects were closed in the same change: the
destination probe followed a final link, so a planted link inside a
destination tree could have redirected a later create or truncate anywhere on
the volume (`stat_kind` is now `NO_FOLLOW`), and a rename silently replaced
an existing leaf of a different kind without asking. A conflict now names
what it would replace (`… exists as a symbolic link`).

**Closed in passing** (`AGENTS.md` §2.18): the graphical file manager's paste
byte-copied a link — leaving a regular file holding the target's text — and
followed it to decide directoryness. `CopyWalk` items now carry a `CopyKind`
(file / directory / link) and yield a `CopyLink` step the app satisfies with
`readlink` + `symlink`; the paste's source kind comes from a `NO_FOLLOW`
stat. One directory read serves both walks, each taking the reading it needs.

`lib/path` grew the two spellings this stage needed in more than one place —
`leaf_name` and `join` — rather than a fifth private `basename` and a twelfth
private `join` (§2.2). The private copies elsewhere in the tree have since
been swept onto those two.

A **`posix_fs_suite` symlink vertical** (`tests/symlink.rs`, 21 cases) drives
the matrix against a real ARXFS volume: create/readlink round trip (a
relative target with `..` included), `readlink`'s domain refusals, `lstat` vs
`stat` reporting *different nodes*, a dangling link describable only under
`Keep`, write and truncate reaching the target and leaving the link a link,
both refused through a dangling link with nothing created, `O_CREAT` through
a dangling link creating the target, `mkdir` over a live or dangling link
refused `AlreadyExists`, `symlink` never replacing a name, unlink and rename
acting on the link, a cycle and a self-cycle refused `LinkLoop`, a link
listed as a link, a link not byte-readable, an interior link always followed,
`..` in a target resolved *physically* (built so a lexical resolution would
succeed and reach a different file), a link unable to escape its volume, and
creation refused on a read-only mount and an unauthorised parent.

## Stage S5 — the desktop shortcut — **landed**

**A program-library row offers two things its own click cannot.** The entry
menu's rows are one list (`EntryRow` + `ENTRY_ROWS` in
`userland/gui/taskbar/src/menu.rs`): *Open* and *Create Desktop Shortcut*.
`rows_for` renders that list and `choose` reads the activated index back
through it, so a reordering cannot silently re-map what a row does, and
`EntryRow::label` is the one definition a test or a QEMU pointer script aims
by. The bar writes nothing: the row becomes
`TaskbarResponse::CreateDesktopShortcut { entry }` and closes the popup, for
the same reason a launch does — the popup is modal, and leaving it up would
stand between the user and the icon just asked for.

**The session names the link and the desktop owns the naming.**
`Desktop::shortcut_to(catalog, entry)` resolves the identifier through
`library::catalogued` — the *one* lookup a launch also uses, so a row can
never launch one bundle and link another — and answers a `DesktopAction`:

- The link takes the entry's **display name**, the target is its
  `BundlePath` verbatim. That is what exercises S4's classifier: bundle-ness
  reads off the *target's* leaf, so `Chess` → `/Apps/chess.app` is an
  application while the link's own name is just a name.
- A display name is not automatically a file name (the library permits `/`
  and `:` in one), so it is validated through the one shared
  `tairix_path::validate_file_name` and a refusal carries that rule's own
  reason.
- **A name already taken is the kernel's answer, not a worked-around one.**
  `fs_symlink` replaces nothing, so a collision is `AlreadyExists` at create
  time. Choosing a free name instead would silently make a second,
  differently-named shortcut for a user who already has one, off a listing
  the rate-limited re-list may have left stale.

`run.rs` honours `DesktopAction::CreateShortcut` with `fs_symlink` — target
first, then link — through the same `settle_desktop_create` tail the new-folder
create uses, so both state a refusal identically and both show the fresh name
by re-listing. A refusal is loud and never fatal.

**Launching the resolved target was S4's; the reason it matters was
misstated.** `lib/appload` has **no** store-prefix rule: its gate is the
bundle layout, the manifest, the ABI version, the syscall-table hash, the
trust-anchored signature, the content hash, the request ∩ grants
intersection, and the `Run` image's PIE/W^X/CFI invariants. What makes the
*resolved* path necessary is the kernel: `appspawn::bundle_run_path` parses an
entry point as a strict `…/<Name>.app/Run` with no traversal, so a link named
`Chess` would yield `…/Chess/Run` and be refused before anything is read.
`FsBundleStore` additionally reads every bundle file `NO_FOLLOW` and refuses
one that is a link, and the store prefix decides only whether a verification
result may be *cached* (`AppStore::cacheable_bundle`). A bundle outside a store
is therefore admitted or refused on its **signature** and the caller's own read
permission, never on its path — which is the stronger property.

Tests: in the taskbar suite, the menu draws exactly the two rows it defines in
order, and each reports its own response with the popup closed. In the session
suite, a secondary press on a library row and a click on the shortcut row
deliver `CreateDesktopShortcut` for that entry with the popup shut. In
`desktop_tests`: the link is the entry's name inside the desktop folder with
the bundle path stored verbatim; an entry that left the catalog is refused;
each display name the shared rule rejects is refused with that rule's reason;
a name already taken is still asked for; and the activation matrix — a
shortcut to a bundle launches the **resolved** `Run`, one to a folder or a
file acts through the link, a dangling one is refused with its reason and
stays selected, and one that names nothing at all is refused rather than
opened.

---

## Stage S6 — hard links — **landed**

A second name for one inode: `fs_link` (**115**), `FilesystemWrite::link`
defaulting to `Unsupported`, the `link_via`/`link_via_secured` VFS pair with
an audited `FsAuditDetail::Link` handler, and a count carried up from the
driver (`NodeInfo::nlink` → `FileStat::nlink` → `ls -l`'s second column)
rather than invented in the VFS, which could not count names without walking
every directory on the volume.

**The follow posture is an operand, because the tool needs both readings.**
`LinkFlags::FOLLOW` selects between POSIX `link()` (empty: neither final
component followed, so the inode that gains a name is the one the caller
spelled and a planted link cannot redirect it) and
`linkat(AT_SYMLINK_FOLLOW)` (`ln -L`). The *new* name is never followed under
either: it is a name being created, and a create never replaces one. A link
with only one of those postures would have been the incomplete core §27
forbids, and the flag has its caller in the same change, so it is not
speculative surface.

**The load-bearing work is the lifecycle, not the create.** `arxfs::unlink`
freed the inode and every block unconditionally; it now decrements `nlink`
and frees only at zero, through the one `drop_name` a rename's replacement
path shares. The rule is uniform across kinds because `.`/`..` are real
entries: an inode's count is how many directory entries name it. Everything
that walks the *inode* tree was already correct for a multiply-named node —
the allocation-map rebuild, `scrub`, and `rescue` each visit it once — so
only `check`, which walks names, gained work: it counts the entries naming
each inode and rewrites a stored count that disagrees, the analogue of
widening an understated feature word.

That lifecycle is exactly why a link-unaware reader must be kept out, so a
volume holding a hard link declares `INCOMPAT_HARDLINKS` (`1 << 1`), set by
the first one in that transaction and rolled back with it. The argument is
**stronger** than S3's: such a reader would not misread a second name, it
would free an inode the other name still reaches.

**Refusals, each with its own answer.** A directory is refused in the VFS
rather than per format (`Errno::IsADirectory`) because the tree staying a
tree is what makes decision 4's physical `..` well-defined; a pair crossing a
mount is `Errno::CrossVolume`, through the *same* `delegate_pair_context` a
rename uses, for the same reason; and an overflow of the format's fixed
per-inode count is `Errno::TooManyLinks` rather than a wrap whose zero would
free live data. `IsADirectory` (43) and `TooManyLinks` (44) are new; the
first also retires the documented collapse of `VfsError::IsADirectory` onto
`Errno::OutOfRange`, which existed only because no dedicated code did.

Per format: ARXFS creates them; FAT32 and ADFS have no such object and
refuse, reporting `NodeInfo::SINGLE_NAME`. **ext4 reports the
`i_links_count` it reads and authors none** — and its unlink now honours that
count, which closed a real defect in passing: it zeroed the count and freed
the inode outright, so unlinking one name of a hard-linked file on a *foreign*
ext4 volume destroyed data the other name still reached.

`ln` makes both kinds and gained real `-L`/`-P`/`-d`/`-F`; `ls -l` gained the
link-count column and both tools lost their divergence notes. `-b`/`-S` and
`-r`/`--relative` stay refused for their own reasons, which are not about
hard links.

**`cp` reproduces links and preserves sharing.** `-l` gives the destination a
second name for the source's node rather than a copy of its bytes; `-s` makes
a symbolic link naming the source; `-P` reproduces a symbolic-link source as a
link storing the same target verbatim, so a relative or dangling one survives
the copy; and `--preserve=links` gives two sources naming one node two names
at the destination, keyed on the identity S7's record carries and remembering
only nodes whose count exceeds one. `-d` is exactly `-P --preserve=links`.
`-a` is refused, because `--preserve=all` includes timestamps no syscall can
set — a preservation reported but not performed would be worse than a refusal.

**The three minimal link commands are their own bundles** (`plans/APPS.md`
§12.1 Stage E): `link` and `unlink` take exactly the operands their POSIX
call takes and carry no option but the reserved `-?`/`--help`, because `ln`
and `rm` are the tools with the option surface — so a script that must make
one hard link, or remove one name, gets a tool that cannot replace a name,
follow a link, or recurse. `link` calls `fs_link` with an empty `LinkFlags`
word, so neither name is followed and all five refusals (`AlreadyExists`,
`IsADirectory`, `CrossVolume`, `TooManyLinks`, `NotSupported`) reach the
caller unchanged. `unlink` calls `fs_unlink` with an empty `UnlinkFlags`
word, so a directory is the kernel's refusal in the same locked walk that
would have removed the entry. `readlink` prints the stored spelling and
**refuses** GNU's `-f`/`-e`/`-m`: canonicalisation is this VFS's one
implementation, and a userland copy of it that disagreed by one rule would
print a path the kernel resolves differently.

Tests: two names reach one inode and one `FileId`; a write through one is
visible through the other; unlinking one leaves the other readable with its
blocks allocated and unlinking the last frees both; the count is reported and
moves with each name; a directory, a cross-volume pair, a taken name, a
missing source and an over-`u32` count are each refused with nothing created;
both postures over a symbolic link (the link itself versus its target), and a
dangling one with nothing to follow to; the feature bit declared on first use,
absent after a rollback, and refusing an unsupported mount; `check` correcting
a drifted count and leaving a sound volume alone; `scrub` and `rescue`
visiting a twice-named inode once; ext4's foreign-volume unlink keeping the
survivor readable across a remount; FAT32 and ADFS refusing both link calls;
the handler's audit record on success and refusal; and a **`posix_fs_suite`
hard-link vertical** (`tests/link.rs`, 14 cases) driving the matrix against a
real ARXFS volume.

---

## Stage S7 — the listing carries node identity — **landed**

`du` counted a multiply-named file once per name because the **userland**
`fs_readdir` record had nothing to key a seen-set on. `tairix_abi::fs::DirEntry`
now carries the child's `id: FileId` and `nlink: u32` beside its kind, sizes,
and mtime (`HEADER_LEN` 32 → 60), for the same reason the sizes are already
there: the listing driver has already read the child's record, so a walk that
needs the identity reads it from the one listing instead of re-`stat`ing every
child.

**One assembly of the identity, resolved once per listing.** The driver's
`DirEntry` always carried its `NodeId`; `DelegatedFs::list` was dropping it, so
it now returns a named `DelegatedEntry { node, info, name }` rather than an
`(info, name)` pair. `MountedFilesystemService` pairs it with the covering
mount's volume id through the same `node_identity` a `stat` uses — every child
of a directory is on the directory's own volume, so `volume_at` resolves the
mount once for the whole listing rather than per entry. A covered mount point
merged into its parent's listing reports `FileId::NONE` and
`NodeInfo::SINGLE_NAME`, for the same reason its sizes and stamp are the
stampless backing's placeholders: the parent volume holds no node of that name.

**The seen-set is bounded by the hard links a walk meets, not by the volume.**
`du` remembers only a non-directory whose `nlink` exceeds one — a node named
once cannot be reached twice, and a directory's count always exceeds one while
a tree walk reaches it once — so a 100 TB volume of ordinary files costs
nothing (§24.1, §26.6). The set grows on demand from no fixed ceiling; a heap
that refuses to grow it, or a node whose backing offers no identity, makes the
walk count every name and say so once on standard error with a non-zero exit —
an over-count that declares itself, never a silent one and never a merge that
would *lose* real storage. `-l`/`--count-links` is the GNU opt-out.

Tests: two names for one node decode to one `id` (ABI); the listing reports one
identity for two names and distinct identities for distinct nodes, and agrees
with `stat` on both (service); a merged mount point reports the placeholders;
and, for `du`, a hard link summed once, `-l` counting every name, a
deduplicated name keeping its own `-a` row at zero, two operands naming one
node counted once, a singly-named file given twice counted twice, and an
unidentified node counted with the diagnostic written exactly once.

---

## What the finished plan guarantees

Every stage has landed; these are the properties the tree now holds, each
covered by the tests its stage lists.

- **Every exhaustive match over `FileKind`/`NodeKind` has a real, fail-closed
  arm** — never a catch-all. `Inode::kind` became an `InodeKind` enum in S3
  precisely so the compiler asks the question at every site.
- **No caller-supplied path grammar was loosened.** `Path::parse` still
  refuses relative paths and `.`/`..`; the looser target grammar is
  `parse_link_target`'s alone, applied to on-disk data, never to a caller's
  spelling.
- **The resolution matrix holds:** a link cannot escape the volume that
  stores it, cannot bypass a permission check on any directory the walk
  traverses, and a cycle or an over-budget chain is `LinkLoop` rather than
  walked. `..` after a link resolves physically.
- **A format without links refuses rather than approximates**, for either
  kind, and the per-format support table in
  `docs/src/filesystem/overview.md` is the one statement of which does what.
- **Only the *format* may refuse; no wrapper may.** A `Filesystem*` impl
  that wraps an inner driver owes its inner's whole surface, and the four
  facet methods carrying an `Unsupported` default (`read_link`,
  `create_link`, `link`, `attrs_fs`) are the ones a wrapper can refuse by
  omission — silently, fail-closed, with no test noticing. The property is
  stated once, in `kernel/core/src/fs/wrapper_conformance.rs`, and every
  wrapper (`CachedFs`, `GroupMappedFs`, `Box<dyn KernelFs>`, the counting
  test shim) runs that suite against a reference driver that supports
  everything; a wrapper that *replaces* a method asserts the replacement
  instead, so the divergence is declared. The suite's own premise — that
  the reference driver supports every method — is itself a test, so no
  wrapper's check can pass vacuously.
- **Both link kinds are proved through the real service, not just the
  driver.** `volume_service_tests`'
  `link_round_trip_through_the_real_service_scenario` creates and reads
  both kinds through the global `FS_SERVICE` over an attached ARXFS volume
  served across a genuine call endpoint — so the link path is exercised
  through `MountedFilesystemService` → `Box<dyn KernelFs>` → `CachedFs` →
  `ARXFS`. Every other link test constructs a `Vfs` over a driver
  directly, which is precisely why three wrappers could answer
  `NotSupported` on every mounted volume while the `posix_fs_suite` link
  verticals stayed green.
- **A listing tells a second *name* from a second *node*.** Every
  `fs_readdir` record carries the child's `FileId` and name count, so a
  tree walk sums a hard-linked file once (`du`) without re-`stat`ing every
  child, and the identity it reports is the one `fs_stat` reports for the
  same node — assembled in one place from the covering mount's volume id
  and the driver's node number.
- **A node's storage outlives every name but the last.** The count is the
  format's own, read and never derived, and the free happens at zero through
  one shared path — so no unlink can destroy data another name still reaches,
  and a reader that does not understand that is kept out by a feature bit.
- **A directory has exactly one name, everywhere.** The refusal is the VFS's
  rather than each driver's, which is what keeps decision 4's physical `..`
  well-defined; each driver refuses too, because it owns the same invariant
  for its own tree.
- **Neither link kind can span two volumes.** A symbolic link's resolution
  cannot escape the volume that stores it (decision 5), and a hard link's two
  names must resolve under one mounted volume — the same `CrossVolume` a
  rename gives, through the same check.
- **Docs land with the code:** this plan, the §15.18 jump-sheet row,
  `docs/src/filesystem/{overview,arxfs-spec}.md` for the VFS and ARXFS
  spellings, `docs/src/desktop/{apps,taskbar,session}.md` for the userland
  and desktop surfaces, and each touched crate's `README.md`.
