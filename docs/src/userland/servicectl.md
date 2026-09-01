# Service control (`servicectl`)

`servicectl` asks a service manager to change a registered service's
**running** state. It is the client half of the manager's control endpoint
and the first holder of `CAP_SERVICE_CONTROL`
(`plans/NEW-SERVICEMANAGER.md` SVC-8).

```
servicectl start SERVICE   bring a registered, currently-down service up now
servicectl stop SERVICE    stop a running service (and its dependents) gracefully
servicectl -h | -?         this tool's own short help
```

Exit status is the coreutils shape: `0` applied, `1` refused or the endpoint
unreachable, `2` a command line that was not understood. Nothing is sent when
the line does not parse.

## The authority is the endpoint, not the tool

PID 1 binds `SERVICE_CONTROL_ENDPOINT` as a **restricted-sender** call
endpoint requiring `CAP_SERVICE_CONTROL`. The kernel therefore refuses the
call from a task that does not hold it, *before* the manager sees it, and the
manager never re-checks a caller-supplied claim. The tool tests no capability
itself and holds no ambient authority: on an account whose ceiling lacks the
capability, `ipc_call` fails and the tool reports why.

`CAP_SERVICE_CONTROL` is part of `tairix_users::ADMINISTRATIVE_SET`, so an
administrator's ceiling carries it and an ordinary session's does not.
Stopping the device manager, the network stack, or the clock affects every
principal on the machine, which is why it is administrative rather than
baseline.

A reachable request can still be refused, and the reply says which:

| Refusal | Meaning |
|---|---|
| `NotFound` | no service by that name is registered |
| `Busy` | a readiness condition is unmet, or the service is mid-teardown |
| `NotSupported` | the load gate refused the spawn (the *target's* bundle, not the caller's authority) |

Every refusal is audited by the manager with its real cause, so the caller
learns *that* it was refused and an operator reads *why* in the log.

## Running is not enabled

`start` and `stop` change what is running now. Whether a service comes back at
the next boot is its **enrolment** in the registration store, reached over a
different path and deliberately not on this endpoint. A service stopped here
returns on reboot if it is still enrolled.

## How PID 1 answers without a second loop

PID 1 already had one loop, supervising a login session per console with a
blocking wait on children. Serving a control endpoint from that shape would
mean either a second dispatch loop or polling between the two — the
cooperative dispatch the charter forbids.

Instead the single loop parks on a **wait-set** carrying the control endpoint,
any-child readiness, and a timeout folded from the engine's own soonest armed
deadline (`Init::next_deadline`). One park, three wake reasons:

- a child exited → reap it, and either relaunch its console's session or hand
  it to the engine;
- the control endpoint is ready → decode one request, drive the engine, reply;
- the park lapsed → run every deadline that is now due
  (`Init::expire_due`).

So a control request is answered while the login session sits blocked on its
console, rather than waiting for some unrelated process to exit, and an idle
machine takes no timer interrupt at all (nothing armed means an indefinite
park).

`expire_due` guarantees that every deadline it finds lapsed is afterwards
either consumed or dropped. That is what makes the derived park length safe:
without it, a lapsed deadline whose guard declined would leave the next park
zero-length for ever and peg a core.

## Layering

The behaviour is a `no_std` library over two seams — `ControlChannel` for the
endpoint and `ToolIo` for the inherited streams — so every path including each
refusal is host-tested without a kernel. The freestanding `Run` binary binds
them to `ipc_call` and fds 1/2. The bundle ships on disk at
`/System/Commands/servicectl.app`, discovered from its own `AppInfo.toml` like
any other app; it is not embedded in the kernel and not on a compiled-in list.
