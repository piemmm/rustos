## NAME

settings — configure the desktop and this machine

## SYNOPSIS

`settings`

## DESCRIPTION

Opens a desktop window listing every category of setting this system has: what
the machine is, how the desktop looks, the screen, the network, the devices it
is driven with, the accounts that use it, and the volumes it holds. Choosing a
category from the sidebar shows its pane.

Settings holds no authority of its own. Every change is either a request to the
desktop session, which owns the user's own settings, or a re-authenticated run
of the command that already writes that store, so nothing here can raise a
privilege.

**Appearance** chooses whether the desktop is drawn light or dark, and
**Accessibility** groups the same contrast, density, motion and interface-scale
settings the way a reader looking for them would; both panes show them, because
a reader looks in either place. A row takes effect as soon as it is chosen, so
there is no button to press afterwards; if the desktop refuses a change, the
reason is reported on the standard error stream and the row goes back to what
the desktop actually holds.

A category this system cannot serve says so plainly and names what would have
to exist before it could. A category it can serve, whose controls this release
does not yet draw, names where the setting is read or set instead. A control
that would change nothing is never shown — which is why Accessibility states
that this desktop keeps no separate pointer size rather than offering one.

Type in the search field above the sidebar to filter it to the categories and
settings a word reaches. `Tab` and `Shift+Tab` move between the search field,
the location trail, the sidebar and the pane; `Up` and `Down` walk the sidebar
and `Enter` opens the row. A window too narrow for the sidebar sheds it, and
the leading crumb of the location trail then lists the categories.

It is launched from the *Settings…* row of the desktop's system menu, from the
desktop's Program Library, or by name from a shell. It requires a running
graphical session: without one the window channel is unreachable and it
reports the refusal on the standard error stream and exits.

## EXIT STATUS

Zero after a clean close; non-zero when the window channel or the shared frame
region was refused (the reason is stated on the standard error stream).
