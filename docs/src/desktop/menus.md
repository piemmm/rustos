# Menus

Every menu on a TAIRiX desktop is the desktop's. There is one chain per seat,
one renderer, and one place a submenu's rules live. An application describes
its menu and receives one answer; it never draws a plate pixel, never learns
where the pointer is inside one, and cannot hold one open.

## What a menu is

A menu is a **chain of session-owned plates**.

- A **plate** is one column of rows under a **title band**: a centred title and
  nothing else — no window commands, no resize edge. The band is the plate's
  drag handle.
- A **chain** is a root plate and the descendants open beneath it. A child is
  placed edge-adjacent to its parent at its parent row's top, flipped to the
  parent's other side when the screen edge leaves no room, and slid to stay on
  screen.
- A **child** is either a **submenu** — more rows from the same model — or an
  **attached window**: a surface hanging where a submenu's plate would hang.
- The chain is the **seat's singleton**. Opening a menu closes whatever was up
  and answers its requester `Dismissed`.

## Titles

A plate's title is derived, never a new field on the wire:

- a submenu's title is its parent row's label;
- an attached window's is its own row's label;
- the icon-bar menu's root title is the application's name from its **signed**
  manifest, so a menu cannot be titled as an application it is not;
- a per-window menu's root title is the application's, bounded and sanitised
  exactly as its row labels are.

A plate is **one** ground: the chain lays it once for the band and the rows
together, and the rows are painted into it (`Menu::render_rows`) rather than
laying a second plate of their own — which would rim the plate twice and notch
its ground where the rows' own corners rounded. A menu drawn on its own still
lays its plate and its rows in the one call.

The band is `lib/controls`' `TitleBar` seating no commands
(`TitleBarCommands::Empty`), never a second title-bar control. Two properties
follow from that emptiness rather than from knobs of their own: with no command
clusters the drag span is the whole band, and with no leading cluster to
justify against the title centres.

## Placement

One rule places every plate and everything that hangs where one would:
`tairix_controls::plate_rect`. It takes the plate's size, an **anchor region**,
a preferred side, a clearance, and the viewport. The plate is bounded to the
viewport, opens on the preferred side, flips to the opposite one when that side
has no room (and the roomier one wins when neither does), then slides along the
cross axis and clamps.

A zero-extent anchor is the point case, so a context menu at a press point and
a slot-anchored icon-bar menu resolve through the same arithmetic. A clearance
of zero is the edge-adjacency a chain needs: travelling from a parent row into
its own child crosses no dead space.

## Opening on arrival, with no timer

A submenu opens when the pointer **arrives on** its parent row — no click, no
hover delay, no timer. Two rules make that deterministic without one:

- a child plate is edge-adjacent to its parent, so there is no gap to cross;
- an open child closes when the pointer **settles on a different row of the
  same parent plate**, never merely because it left the parent row's rectangle.

A disabled row opens nothing and closes nothing.

## Attached windows

An attached window is the general form of the information panel, and the one
place an application's own pixels enter a chain.

- **Attached**, it lives and dies with the chain: it closes when the pointer
  settles on another row of its parent, when the chain dismisses, or when the
  chain's owner dies.
- **Clicking its row detaches it.** The window becomes an ordinary top-level
  window with the compositor's full furniture, and the chain dismisses. The
  detach is reported as an ordinary `MenuOutcome::Chosen` naming the row, so an
  application's handling stays one total `match`.
- **A submenu never detaches.** No gesture turns a submenu into a window.

Presenting one may not stall the chain. Arrival on the row sends the owning
application a request to present; the chain stays live and fully usable while
it answers, and a window that arrives after the pointer has moved on is refused
rather than shown. The chain — not the window engine — makes that call, because
only the chain knows the model and where the pointer has settled.

The information panel is the canonical instance and stays **session-drawn from
the signed manifest**: the application declares only that the row exists and
supplies none of the panel's text. A process with nothing attesting an identity
gets no information row rather than a fabricated panel.

## The grab

While a chain is up the seat's pointer and keyboard route to it:

- a press **inside** the chain acts there; an attached window's own input is
  the application's, as any window's is;
- a press **outside** dismisses the chain and is **consumed** — a dismissal
  never doubles as a click on whatever was behind the menu;
- **Escape** closes the deepest open child; with only the root open it
  dismisses the chain, so repeated Escape always gets the user out, and an
  attached panel closes before the menu that opened it;
- **traversal** is the service's: Up/Down within a plate, Home/End to its ends,
  Right into the highlighted row's child, Left back out, Enter/Space to
  activate;
- a **mode change** under the gesture — the seat's output resized, the UI scale
  or theme switched — dismisses the chain rather than re-placing it. A plate
  the user has dragged has a position that is theirs, and no rule can carry it
  onto a different screen.

## Dragging

A press on any plate's band moves **that plate and its descendants**; ancestors
stay put. Dragging pins the plate — its placement stops being derived from the
anchor — and its children re-place relative to their parent row as usual.

Dragging is not detaching. A dragged chain is still the seat's one chain, still
holds the grab, and still closes on an outside press. Nothing an application
sends can pin a menu open; the only thing that moves a plate is the user's own
drag.

## The model, and what an application may say

The service renders one model. The wire model an application sends
(`AppMenu`, `lib/abi/src/window_ipc.rs`) decodes **into** it and is a **bounded
subset** — bounded structurally, not by a check.

The desktop's own rows may state that *the system* lacks the authority for a
command, and draw the Authority Mark that says so. The wire model has no field
for an authority state, so a decoded application row always carries the default
one: an application cannot paint the system's refusal on its own row, because
there is nothing for it to send. Rows an application legitimately marks — a
tick for an independent setting, a bullet for the chosen member of a group —
cross the wire as they always did.

A declared separator becomes the next row's group break rather than a row of
its own, so a separator inside a submenu draws the divider it draws on the root
plate and no index the chain reports names a rule.

## Where it lives

- `userland/gui/session/src/menu.rs` — the chain: the model, the plates, the
  placement, the grab, traversal, dismissal and lifetime. It touches no
  compositor; the session presents what it lists and takes down what it no
  longer has.
- `lib/controls` — `Menu` and `MenuItem` for rows, `TitleBar` for bands,
  `plate_rect` for placement.
- `lib/abi/src/window_ipc.rs` — the wire model, the per-gesture open, and the
  one `MenuClosed` outcome.
- `lib/window` — the engine that keys an open to its attested owner, holds one
  unanswered open per window, and settles an attached window against the
  outcome.

The staged design is `plans/NEW-MENUS.md`.
