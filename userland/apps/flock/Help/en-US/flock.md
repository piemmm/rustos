## NAME

flock — run a command holding an advisory file lock

## SYNOPSIS

`flock [options] file command [argument...]`

## DESCRIPTION

Takes an advisory lock covering the whole of `file`, runs `command`
while holding it, and exits with the command's own status. Two runs
naming the same `file` therefore never overlap, which is what a script
needs to keep one copy of itself running at a time.

The lock belongs to the open file this run holds, so the system releases
it when the run ends — by finishing, by being interrupted, or by
crashing. Nothing is written into `file` and nothing has to be cleaned
up afterwards, so there is no stale lock for the next run to wait out.
`file` is created if it does not exist.

Locks are **advisory**: they coordinate programs that agree to use them.
They grant no access and withhold none, so a program that does not take
a lock is not stopped from reading or writing the file. Who may read or
write it is decided by the file's own owner, permissions and access
list, exactly as for any other file.

Without `-n` or `-w` the run waits for as long as it takes. With `-n` it
gives up at once; with `-w` it gives up after the given time. Either
way, giving up exits with the conflict code (`1` unless `-E` says
otherwise) and the command does **not** run — so a script can always
tell "I could not get the lock" from "the command failed".

Three options of other systems' `flock` are deliberately absent rather
than accepted and ignored. `-u` and `-o` act on a file descriptor a
shell opened beforehand, which this command has no form for; `-c` runs
its argument through a shell, which is spelled explicitly here as
`flock file elsh -c '...'` so it is clear which shell runs. Asking for
any of them is a usage error, so a script is told rather than run
without the lock it asked for.

## OPTIONS

- `-s, --shared` — take a shared lock. Several runs may hold a shared
  lock at once, and all of them exclude a run asking for an exclusive
  one. This is the lock for readers.
- `-x, --exclusive` — take an exclusive lock, excluding every other
  holder. The default, and the lock for writers.
- `-n, --nonblock` — do not wait: if the lock is held, exit with the
  conflict code straight away.
- `-w, --timeout <seconds>` — wait at most this many whole seconds, then
  exit with the conflict code.
- `-E, --conflict-code <n>` — the status to exit with when `-n` or `-w`
  gives up. Defaults to `1`. Use a value the command itself never
  returns when a script must tell the two apart.
- `-v, --verbose` — report on standard error whether the lock was taken.
- `-?, --help` — show this command's own short help.

Options must come before `file`: everything from `file` onward belongs
to the command, so `flock lock ls -l` passes `-l` to `ls`. `--` ends
option parsing, so a file or command beginning with a dash is reachable.

## EXAMPLES

- `flock /Users/ian/Library/backup.lock backup-now` — run the backup,
  waiting if another copy is already running.
- `flock -n /Storage/db/data.lock compact` — compact the database, or
  exit `1` at once if something else holds the lock.
- `flock -s -w 30 /Storage/db/data.lock report` — take a reader's lock,
  waiting up to thirty seconds for any writer to finish.
- `flock -E 99 -n lock task` — exit `99`, rather than `1`, when the lock
  is held, so the script can tell that apart from `task` failing.

## EXIT STATUS

- the command's own status — the lock was taken and the command ran.
- the conflict code (`1` by default) — `-n` or `-w` gave up; the command
  did not run.
- `1` — the lock could not be taken for a reason waiting would not fix,
  or the command could not be run; the reason is printed on standard
  error.
- `2` — the command line was not understood; nothing was locked and
  nothing ran.
- `126` — the command was found but could not be run.
- `127` — the command was not found.

## ENVIRONMENT

- `PATH` — searched for the command, after the system and user program
  stores.
- `HOME` — locates the user's own program stores.
- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

elsh, ps, ulimit
