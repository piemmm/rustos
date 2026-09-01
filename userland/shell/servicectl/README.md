# `tairix-servicectl` — service runtime control

The `servicectl` tool (`/System/Commands/servicectl.app/Run`) is the first
holder of `CAP_SERVICE_CONTROL` and the client half of the service manager's
control endpoint (`plans/NEW-SERVICEMANAGER.md` SVC-8). It asks a service
manager to change a registered service's *running* state; the manager decides
and this tool only encodes the request and reports the answer.

Stability tier: **experimental**.

## Commands

```
servicectl start SERVICE   bring a registered, currently-down service up now
servicectl stop SERVICE    stop a running service (and its dependents) gracefully
servicectl -h | -?         this tool's own short help
```

Exit status follows the coreutils shape: `0` applied, `1` refused or the
endpoint unreachable, `2` a command line that was not understood.

## Why there is no capability check here

Reaching the endpoint *is* the authority. PID 1 binds
`SERVICE_CONTROL_ENDPOINT` as a restricted-sender call endpoint requiring
`CAP_SERVICE_CONTROL`, so the kernel refuses the call from a task that does
not hold it and the manager never re-checks a caller-supplied claim. The tool
therefore holds no ambient authority and tests no capability itself: on an
account whose ceiling lacks it, `ipc_call` fails and the tool says why.

`CAP_SERVICE_CONTROL` is part of `tairix_users::ADMINISTRATIVE_SET`, so an
administrator's ceiling carries it and an ordinary session's does not.

## Enablement is a separate concern

`start`/`stop` change what is *running*. What is *enabled* — whether a service
comes back at the next boot — is the registration store's, reached over a
different path, and is deliberately not on this endpoint. A service stopped
here returns on reboot if it is still enrolled.

## Layering

The behaviour is a `no_std` library (`src/lib.rs`) over two seams —
`ControlChannel` for the endpoint and `ToolIo` for the inherited streams — so
every path including each refusal is host-tested without a kernel. The
freestanding `Run` binary (`src/run.rs`) binds them to `ipc_call` and fds 1/2.
