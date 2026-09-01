# Service control (`servicectl`)

`servicectl` asks a service manager to change a registered service's
**running** state or its **persistent enrolment**. It is the client half of the
manager's two control endpoints and the first holder of `CAP_SERVICE_CONTROL`
(`plans/NEW-SERVICEMANAGER.md` SVC-8).

```
servicectl start SERVICE    bring a registered, currently-down service up now
servicectl stop SERVICE     stop a running service (and its dependents) gracefully
servicectl enable SERVICE   enrol it, so every boot brings it up, and start it now
servicectl disable SERVICE  unenrol it, so no boot brings it up, and stop it now
servicectl -h | -?          this tool's own short help
```

Exit status is the coreutils shape: `0` applied, `1` refused or the endpoint
unreachable, `2` a command line that was not understood. Nothing is sent when
the line does not parse.

## The authority is the endpoint, not the tool

PID 1 binds `SERVICE_CONTROL_ENDPOINT` and `SERVICE_ENROL_ENDPOINT` as
**restricted-sender** call endpoints, each requiring `CAP_SERVICE_CONTROL`. The
kernel therefore refuses the call from a task that does not hold it, *before*
the manager sees it, and the manager never re-checks a caller-supplied claim. The tool tests no capability
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

## Running is not enabled — two endpoints, not two operations

`start` and `stop` change what is running now; a service stopped that way
returns at the next boot if it is still enrolled. `enable` and `disable` change
the **enrolment record**, so they also survive one.

They travel to *different endpoints*. The acts differ in durability and the
answers differ in kind — control answers with a `ServiceState`, enrolment with
a `ServiceEnrolment` plus whether the request changed anything — so one frame
would have to carry a union of two unrelated results. Keeping them apart is
also what lets their gates diverge later without reshaping either protocol;
today both require the same capability, because nothing yet needs to trust a
principal to restart a wedged service without also trusting it to disable one.

An enrolment request names a service the manager already **knows**, so a typo
can never record a phantom enrolment that a later image would silently
activate. The authority boundary is the identity one `AuthorityScope` already
draws on the launch path — a manager may enrol only a service running under an
account it may manage — never a second capability derivation in the engine; the
kernel still derives `manifest ∩ account-ceiling` at spawn whatever the record
says.

The reply distinguishes "enabled it" from "it was already enabled", so a
provisioning script run twice succeeds without claiming work it did not do.

### The record is two layers

| Layer | Where | Written by |
|---|---|---|
| Vendor | the `enrolled` directives of PID 1's startup configuration | the build |
| Administrator | `/System/Settings/Services/overrides` on the encrypted root | PID 1, on a request |

The split is forced by the volume layout, not chosen: the whole
`/System/Settings` subtree resolves to the writable encrypted root, so nothing
there can be read before the unlock — and PID 1 must know what to bring up
before then — while the pre-unlock volume is read-only, so nothing there can be
written at runtime. The vendor layer is therefore not a document at all: no file
under `/System` is reliably readable at the instant the manager decides, and the
only sanctioned pre-unlock read is the store service's `CAP_DRV_LOAD`-gated
whitelist, which PID 1 must not hold to read a configuration file.

The administrator's layer holds only what *differs* from the image's, so a
system update shipping a different default takes effect at once for every
service the administrator has not spoken about; re-enabling something empties
its entry rather than pinning the old default.

PID 1 boots on the vendor layer alone and adopts the administrator's the moment
that document becomes readable — on a bounded doubling one-shot ladder, since
nothing announces the unlock — stopping anything it disables and recording
`SERVICE_ENROLMENT_REVOKED`. A disabled service therefore does run for the few
seconds before the unlock, every boot. Closing that window would need the
document readable pre-unlock, which the volume layout forbids; deferring the
whole enrolled tier until the ladder resolves would deny a never-unlocking
machine — a recovery session — its clock for the ladder's whole length, which is
worse. Nothing is *granted* by the document being unreadable, so this is a
narrowing that arrives late, not a fail-open.

**The decision reaches the disk before it is acknowledged.** PID 1 writes the
override document and only then replies success, so the tool's own
`is now disabled` line cannot come from a manager that failed to persist the
record.

## How PID 1 answers without a second loop

PID 1 already had one loop, supervising a login session per console with a
blocking wait on children. Serving a control endpoint from that shape would
mean either a second dispatch loop or polling between the two — the
cooperative dispatch the charter forbids.

Instead the single loop parks on a **wait-set** carrying both control
endpoints, any-child readiness, and a timeout folded from the engine's own
soonest armed deadline (`Init::next_deadline`) and the bounded one-shot ladder
it waits for the override document on. One park, four wake reasons:

- a child exited → reap it, and either relaunch its console's session or hand
  it to the engine;
- the control endpoint is ready → decode one request, drive the engine, reply;
- the enrolment endpoint is ready → the same, for the durable half, persisting
  the record before it answers;
- the park lapsed → try the override document if its rung is due, then run
  every deadline that is now due (`Init::expire_due`).

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
