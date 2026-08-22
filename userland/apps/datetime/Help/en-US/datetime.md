## NAME

datetime — set the machine's date and time

## SYNOPSIS

`datetime`

## DESCRIPTION

Opens a desktop window showing the machine's clock in six editable fields
— year, month and day on the first row, hour, minute and second on the
second — and sets the clock to what they say. Nothing changes until
**Set** is pressed.

The reading is UTC. TAIRiX keeps no timezone offset, so there is no local
time to show and none to enter.

The window is normally reached from the desktop clock's own menu:
right-click the clock in the icon bar and choose **Set Date & Time…**.
Setting the
clock needs an authority a desktop session does not have, so the desktop
asks for an account that does, and this app is started as that account
once the password is accepted.

Click a field to type in it, or press `Tab` to move to the next one.
Only digits are accepted, with a leading `-` allowed in the year for a
date before year 1. `Enter` sets the clock; `Escape` closes the window.

Every field is checked before anything is set, and the first fault is
stated in the window rather than corrected silently: a month outside 1 to
12, an hour outside 0 to 23, a minute or second outside 0 to 59, or a day
that does not exist in the month and year entered — 31 April, or 29
February outside a leap year. Nothing is set when a field is refused.

Dates before 1970 and long after 2038 are ordinary entries. The clock is
a signed 64-bit value, so neither is a limit.

If the machine's clock has never been set since it started, the fields
open **empty** and the window says so. They are not filled with the Unix
epoch, which would be a date the machine never claimed.

If the account this app is running as may not set the clock, the attempt
is refused, the window says so, and the clock is left exactly as it was.
The reason is also written to the standard error stream. The app keeps
running: a refused set is an answer, not a failure of the program.

## EXIT STATUS

Zero after a clean close, including when a set was refused. Non-zero when
the window could not be opened, the shared frame region was refused, or
the window channel was lost; the reason is stated on the standard error
stream.

## SEE ALSO

`sysinfo`, `uptime`
