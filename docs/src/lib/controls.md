# `tairix-controls` — the shared Reactive Alloy control behaviour

Reactive Alloy is TAIRiX's GUI control design language
(`plans/GUI-CONTROLS-DESIGN.md`), and `lib/controls` is the single home for
its behaviour. A control is typed Rust state resolved against the shared
design tokens (`lib/theme`) and drawn through the shared rasteriser
(`lib/raster`); no application carries a second copy of a control's
behaviour. The crate lives in `lib/*` because its consumers — the compositing
window manager, the taskbar, and the graphical apps — may not depend on one
another.

## The families

| Module | Controls |
|---|---|
| `button` | `Button`, `IconButton`, `SplitButton` |
| `selector` | `Toggle`, `Checkbox`, `Radio` |
| `value` | `Slider`, `Progress` |
| `meter` | `Meter` |
| `text` | `TextField`, `SearchField` |
| `menu`, `toolbar`, `tabs`, `combo` | `Menu`/`MenuItem`, `Toolbar`, `Tab`/`Tabs`, `ComboBox` |
| `collection` | `ListRow`, `TableRow`, `Card`, `Panel` |
| `scroll`, `scrollbar` | the geometry engine and the one `ScrollBar` over it |
| `window` | `WindowFrame`, `TitleBar`, `WindowControl`, `ResizeGrabber` |
| `shell` | `Notification`, `TaskbarItem`, `TraySignal` |
| `decision` | `Dialog`, `Tooltip`, `HelpTip` |

Every one of them resolves its colours, metrics, and corner radii from the
active `Theme` and `Scale` rather than a hard-coded pixel or hue, composes its
appearance from the typed `state` vocabulary, and emits a typed action for the
owning service to authorise. Two control values compare equal exactly when
they would draw the same pixels, so a host can skip a repaint by comparing
what it is about to draw against what it drew last.

## Grouped focus and anchored edges

Two of the design language's reactive state patterns describe a *relationship
between controls* rather than the state of any one of them, so both are
resolved in the crate's shared paint recipe and inherited by every family
instead of being drawn per surface.

### The Focus Field

`FocusState` carries two independent facts: whether a control holds the
keyboard, and whether it belongs to a group whose **Focus Field** is
highlighted. A row of related controls — a list row and the action buttons
that act on it — is one such group: the member the keyboard is actually on
takes the focus ring, and every other member states its membership by
lifting its rim part-way toward the active rim.

The lift is partial by design, so a member never looks like the focused
control; a control that is *both* focused and a member simply takes the ring,
because the language draws one or the other and never both on the same
control. A filled plate is left alone: its rim is its plate colour by
construction, and tinting one without the other would put a foreign edge on a
coloured control. Under a high-contrast theme the lift goes all the way to the
active rim — contrast comes before glow, and a partial blend would wash out.

Membership is the *weakest* claim a rim can carry. A disabled, denied,
needs-capability, failed-closed, or pending control keeps the rim its
disposition gave it and draws identically whether or not its group is
highlighted: each of those is telling the user something they need far more
than which row a control belongs to, and a control that cannot be actioned
must never look livelier than a resting one that can. Only an ordinary
interactive control — including one merely awaiting confirmation, which is
still actionable and still takes its plain role emphasis — is lifted.

### The Edge Wake

An anchored control that content scrolls past does not move, which leaves a
still frame ambiguous: did the column stay put, or is it merely where the rows
left it? The **Edge Wake** answers that on the control's edge. The Switchboard
lights the leading edge of its action column for exactly as long as the rows
beside it are displaced from the top of the list.

It is a state, not an animation. There is nothing to fade, so a reduced-motion
theme needs no second path and a screenshot carries the same information as a
live surface. The seam is drawn at the shared seam breadth in the active rim
colour, doubled under heavy contrast like every other edge in the theme. A
section whose items are cards has no wake: a card draws its own footer actions
inside itself, so no anchored column stands beside the list.

## Masked text entry

`TextField::secret(max_len)` puts a field into masked mode for credential
entry — a password, a passphrase, a PIN — and `TextField::is_secret` reports
it. A `SearchField` has no such mode: a query is not a credential. Nothing
else about the field changes. The plate, rim, focus ring, validation rim,
Authority Mark, read-only and disabled rendering, high contrast, and reduced
motion behave exactly as for a plain field, and every editing key, the pointer
caret placement, and drag-selection work identically. The control offers no
way to reveal the buffer.

### One bead per character, not a repeated glyph

A masked field paints one filled round bead per `char`, at a fixed advance
derived from the theme's selector extent and the active `Scale`, through the
same shared circle primitive the Signal Bead uses. It draws beads rather than
a repeated masking character for two reasons:

- the drawn run's width then depends only on the buffer's *length*, never on
  which characters it holds, so the rendering cannot report anything about the
  secret through its width; and
- no particular masking glyph has to exist in the font.

The caret stands between bead cells and the selection highlight covers whole
cells, both through the same painting a plain field uses, so a masked field
measures exactly as tall as an unmasked one. The pointer hit test divides the
pointer offset by the fixed cell advance and resolves the resulting cell to a
`char` boundary — never a byte index derived from glyph widths — so a click can
never land mid-scalar. An empty field still shows its placeholder: a
placeholder is not a secret.

### The buffer is reserved once, up front

Masked mode is inseparable from its character bound, and the bound is the
reason. It lets the editor reserve the worst case UTF-8 needs for `max_len`
characters the moment the mode is set, so the buffer can never grow while it
fills. A `String` that grows copies its contents to a fresh allocation and
releases the old block with everything typed so far still written in it — a
copy of the credential that no later erase can reach, because nothing holds
its address any more. Reserving the whole capacity up front means there is
only ever one copy to erase.

### Discarded bytes are erased

Every path that drops buffer content — replacing the text, overwriting a
selection, clearing, truncating to the bound, and the editor's `Drop` —
overwrites the bytes it discards before releasing them. The erase is the
workspace's shared `tairix_util::secret::wipe` rather than a plain fill: on
the drop path the bytes are freed immediately afterwards and nothing reads
them back, so an ordinary store is dead by the language's own rules and a
release build is entitled to delete it outright, leaving the plaintext in the
released block. The shared wipe writes volatile and fences, so the erasure
survives optimisation.

The erase runs in plain mode too. It is cheap, it is harmless, and one editor
is better than two. A `TextField`'s `Debug` output redacts a masked buffer,
printing its character count in place of its content, so a diagnostic dump
cannot carry a password.

## Where it sits

`#![no_std]`, and `#![forbid(unsafe_code)]`. The crate depends only on other
`lib/*` crates — `tairix-geometry`, `tairix-theme`, `tairix-raster`,
`tairix-font`, `tairix-icon`, `tairix-input`, and `tairix-util` for the shared
secret erase — and never on `kernel/*`, `drivers/*`, or `userland/*`, so the
desktop depends on it and never the reverse.
