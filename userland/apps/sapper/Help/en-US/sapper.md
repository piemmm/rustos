## NAME

sapper — clear the minefield without striking a mine

## SYNOPSIS

`sapper`

## DESCRIPTION

Opens a desktop window holding a grid of covered cells. Some hide a mine. Every
cell you uncover that does not shows how many of its eight neighbours do, and
those numbers are all you need to work out where the mines are. Uncover every
cell that is not a mine and you have won; uncover one that is and the game ends.

The first cell you uncover is always safe, and so are the eight around it, so an
opening move can never lose and always opens a region to reason from.

Click a covered cell to uncover it. Click it with the secondary button to plant
a flag, again for a question mark if those are on, and again to clear it. A
flagged cell is protected: clicking it does nothing.

Once a cell is open its number can do the work for you. Click an open number
whose flags already match it and every remaining neighbour is uncovered at once.
Click it with the secondary button when its covered neighbours are exactly as
many as its number and they are all flagged at once. The middle button does the
same as the first, from anywhere on the cell.

The counter on the left shows how many mines are left to find, less the flags
you have placed; it goes negative if you place more flags than there are mines.
The clock on the right starts on your first uncovered cell and stops when the
game ends. The button between them starts a new game, and its face tells you how
the current one went.

Arrow keys move a ring around the board. `Space` uncovers the cell inside it, or
chords it when it is already open. `F` marks the cell, `Shift+F` flags all its
covered neighbours, `N` starts a new game, and `1`, `2` and `3` choose the
beginner, intermediate and expert boards. The same choices, and the question-mark
setting, are on the game's icon-bar menu.

Your best time on each of the three standard boards is remembered between
sessions. A board of your own size keeps none, because no two such boards are
the same game.

The game is launched from the desktop's Program Library, under Games, or by name
from a shell. It requires a running graphical session: without one the window
channel is unreachable and the game reports the refusal on the standard error
stream and exits.

## EXIT STATUS

Zero after a clean close; non-zero when the window channel or the shared frame
region was refused (the reason is stated on the standard error stream).
