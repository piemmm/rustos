# The capability + IPC model (§19.7 Silver)

`AGENTS.md` §19.7 requires the capability-critical core to carry
machine-checked specifications in addition to unit and property tests. The
**Silver** tier asks for "a TLA+ (or equivalent) model of the capability +
IPC state machine [that] is kept in sync with the code", checked by `cargo
xtask ci` on every PR that touches the modelled subsystems.

RustOS supplies the *equivalent*: an in-tree, exhaustive **explicit-state
model checker**. The charter forbids leaning on external code where a clean
first-party implementation is feasible (§2.12), and §19.6 already set the
"equivalent in-tree harness" precedent over an external runner; TLA+'s TLC
checker is an external Java tool, so the model and its checker live in the
workspace instead. The verifier — not the model — is the only oracle
(§19.7): a reachable state or transition that breaks an invariant fails the
build, fail-closed.

The model and checker are in `tools/xtask/src/commands/model_check.rs`; the
command is `cargo xtask model-check` (run unconditionally inside `cargo xtask
ci` because the search is exhaustive and finishes in milliseconds). This page
is the formal narrative kept in sync with that code.

## What is modelled

The model abstracts the combined capability + IPC core that three production
crates implement together:

- `kernel/sec::TaskCapabilities` — a task's authority under `derive`,
  `delegate`, and `revoke`.
- `lib/caps::CapabilitySet` — the subset algebra those operations use.
- `kernel/ipc::Port` — the capability-checked, bounded mailbox under `send`,
  `recv`, and `destroy`.

Capabilities are abstracted to the bits `0..N` of a small mask (the model
uses `N = 2`, which exercises every subset relation: empty, each singleton,
and the full set). The numbers are kept small deliberately: the whole point
of a model checker is to enumerate *every* reachable state, so the abstract
universe is sized to stay finite and exhaustively searchable while still
covering each qualitative case.

## State (`TypeOK`)

```
Subset      == 0 .. (2^N - 1)          \* a capability mask
Msg         == [ caps_ok: BOOLEAN, size_ok: BOOLEAN ]

State == [
  user_grant : Subset,   \* fixed at derive: the user's grant (upper bound)
  manifest   : Subset,   \* fixed at derive: the executable's request (upper bound)
  effective  : Subset,   \* evolves under delegate / revoke
  closed     : BOOLEAN,  \* the port has been destroyed
  queue      : Seq(Msg)  \* the FIFO mailbox, |queue| <= CAPACITY
]
```

`Msg` records the *genuine* authorisation facts that held when the port
accepted a message — independently of the policy decision — so an invariant
can catch a policy that accepted a message it should have refused.

## Initial states (`Init`)

`derive` sets the effective set to exactly `user_grant ∩ manifest` (§5.2 —
no ambient authority). Every bounds pairing is an initial state:

```
Init ==
  \E ug, mf \in Subset:
    /\ user_grant = ug
    /\ manifest   = mf
    /\ effective  = ug & mf          \* bitwise AND = set intersection
    /\ closed     = FALSE
    /\ queue      = << >>
```

## Transitions (`Next`)

```
Delegate(req) ==                      \* TaskCapabilities::delegate
  /\ IF req \subseteq effective       \* adopt only a subset; else refuse
       THEN effective' = req
       ELSE effective' = effective    \* refused: untouched
  /\ UNCHANGED << user_grant, manifest, closed, queue >>

Revoke(bit) ==                        \* TaskCapabilities::revoke
  /\ effective' = effective \ {bit}
  /\ UNCHANGED << user_grant, manifest, closed, queue >>

Send(sender_caps, len) ==             \* Port::send, fail-closed precedence
  LET caps_ok     == REQUIRED_SEND \subseteq sender_caps
      size_ok     == len <= MAX_LEN
      capacity_ok == Len(queue) < CAPACITY
      accept      == ~closed /\ caps_ok /\ size_ok /\ capacity_ok
  IN  /\ IF accept
           THEN queue' = Append(queue, [caps_ok |-> caps_ok, size_ok |-> size_ok])
           ELSE queue' = queue
      /\ UNCHANGED << user_grant, manifest, effective, closed >>

Recv ==                               \* Port::recv
  /\ queue' = IF queue = << >> THEN << >> ELSE Tail(queue)
  /\ UNCHANGED << user_grant, manifest, effective, closed >>

Destroy ==                            \* Port::destroy: close and drain
  /\ closed' = TRUE
  /\ queue'  = << >>
  /\ UNCHANGED << user_grant, manifest, effective >>

Next == \/ \E r \in Subset:        Delegate(r)
        \/ \E b \in 0 .. (N-1):    Revoke(b)
        \/ \E c \in Subset, l \in 0 .. MAX_LEN+1: Send(c, l)
        \/ Recv
        \/ Destroy
```

The `Send` precedence — `closed → caps → size → capacity` — is identical to
`Port::send`, and the model draws `len` up to `MAX_LEN + 1` so the oversize
(`size_ok = FALSE`) branch is always reachable.

## Invariants (`Inv`)

State invariants, checked at every reachable state:

- **`no-ambient-authority`** (§4, §5.2): `effective ⊆ user_grant` and
  `effective ⊆ manifest`. A task can never hold a capability outside its
  derive-time bounds — the unforgeability guarantee.
- **`ipc-fail-closed`** (§5.4): every queued `Msg` has `caps_ok ∧ size_ok`.
  No unauthorised or oversize message is ever in the mailbox.
- **`mailbox-capacity`**: `Len(queue) ≤ CAPACITY`.
- **`closed-port-drained`**: `closed ⇒ queue = << >>`.

Transition invariants, checked on every reachable step:

- **`authority-monotone`**: `effective' ⊆ effective` for *every* transition.
  This single relation subsumes delegate-never-widens, revoke-only-shrinks,
  and the impossibility of any action minting authority.
- **`fail-closed-admission`**: if a message was appended, the port was open,
  had spare capacity, and the message was genuinely authorised and sized.

## Keeping the model in sync

The model is faithful only as long as the production semantics it mirrors do
not drift. When `TaskCapabilities` or `Port` changes shape:

1. Update the transition relation above and in `model_check.rs` together.
2. Re-run `cargo xtask model-check`; a divergence between the policy and the
   recorded authorisation facts surfaces as an invariant counterexample with
   a minimal action trace.

The checker also carries fault-injection tests (a widening `delegate` and a
port that skips the capability check) that prove it *rejects* a model that
breaks an invariant — the oracle has teeth.

## Tiers beyond Silver

- **Bronze** (done) — the `proptest` stateful models; see
  [Stateful models for the capability core](../proptest_models.md).
- **Gold** (aspirational, tracked in `PLAN.md`) — Verus contracts on the
  `lib/caps` and `kernel/sec` capability-check paths, discharged by `cargo
  xtask verify`.
