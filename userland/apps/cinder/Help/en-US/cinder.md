## NAME

cinder — a desktop companion who lives in a playpen and roams the desktop

## SYNOPSIS

`cinder`

## DESCRIPTION

Opens a small playpen window with Cinder in it. Cinder is the TAIRiX mascot: a
small rust-and-charcoal creature who potters about, grooms, naps, and notices
where your pointer is.

He can be let out. Choose **Let Cinder out** from his icon-bar menu and he
leaves the pen for the desktop itself, where he wanders between your windows.
Meeting one he will climb onto it and sit on its title bar, flatten himself and
slip underneath it, or simply walk around it — which he does depends on the
window's shape and on his mood. Move the pointer near him and he will watch it,
trot after it, and pounce on it if he catches up.

Click him to pet him, in the pen or out on the desktop. Petting cheers him up.
Out on the desktop only he catches the click: the transparent space around him
belongs to whatever is behind, so a companion sitting over your work never eats
a click meant for it.

In the pen you can also pick him up and put him down anywhere on the floor, and
bat the ball about.

**The menu.** **Let Cinder out** / **Bring Cinder home** toggles where he is.

**Quit** ends him and takes him off the desktop.

**Closing the pen.** Closing the pen window does not quit. Cinder is a resident application: closing
the pen with him inside puts him away, and closing it while he is out leaves him
roaming. Click his icon on the bar to open the pen again. *Quit* is what ends
him.

**Mood.** Cinder has three things he wants: rest, play, and company. Running about spends
his energy and a nap restores it; sitting still gets him bored and chasing the
pointer entertains him; being petted cheers him up. You do not have to manage
any of this — there is nothing to feed him and nothing that goes wrong if you
leave him alone. It is there so you can tell at a glance what sort of mood he
is in.

How he is feeling, and whether he was out, are remembered between sessions.

**When he cannot go out.** Being on the desktop outside a window needs the `CAP_DESKTOP_LAYER` capability,
which this application requests in its signed manifest and which your account's
grants must also allow. If they do not — or if there is no graphical session —
**Let Cinder out** states the reason in the pen and on standard error, and the
pen keeps working. Cinder simply stays in.

## EXIT STATUS

`0` on a clean quit. A non-zero status is always accompanied by the reason on
standard error.

## SEE ALSO

`sapper`
