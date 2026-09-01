## NAME

servicectl — start and stop system services

## SYNOPSIS

`servicectl [-h | -?] start|stop SERVICE`

## DESCRIPTION

Asks the service manager to change a registered service's runtime state,
over its capability-gated control endpoint. The manager decides: this tool
only encodes the request and reports the answer.

Reaching the endpoint is itself the authority. Without
`CAP_SERVICE_CONTROL` in your account's ceiling the kernel refuses the call
before the manager sees it, so an unprivileged account cannot even ask.

- `start SERVICE` — bring a registered, currently-down service up now. The
  readiness conditions it requires still apply: a service whose conditions
  are unmet is refused rather than started into a system that cannot
  support it.
- `stop SERVICE` — stop a running service gracefully, and its dependents
  in reverse-dependency order. The service is asked to exit and forced down
  only after its grace period.

On success one line names the state the manager left the service in.

Stopping a service affects every principal on the machine, not just your
own session, and a service that is enrolled comes back at the next boot:
this tool changes the *running* system, not what is enabled.

## OPTIONS

- `-h, -?` — show this command's own short help and exit.
- `--` — end the options, so a service whose name begins with a dash can
  still be named.

## EXIT STATUS

- `0` — the operation was applied, or the short help was shown.
- `1` — the manager refused the operation, or the control endpoint could
  not be reached.
- `2` — the command line was not understood; nothing was sent.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

- `ps`
- `man`
