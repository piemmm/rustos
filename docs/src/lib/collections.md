# `tairix-collections`

`no_std`, allocation-free collections that are not in `core` or `alloc`.

## `BitSet256`

A 256-bit fixed-capacity set backed by four `u64` words. Constant-time
`insert` / `remove` / `contains`, plus `union`, `intersection`,
`difference`, `is_subset_of`, `len`, and an ascending iterator.

The bitset has two concrete callers that justify its existence in this
crate (`AGENTS.md` §2.3):

1. `tairix-caps` stores a process's capability membership.
2. `kernel/sched` (planned in Stage 2) will use it for a per-CPU
   ready-task bitmap.

## What this crate is not

It is not a general-purpose collections crate. Nothing speculative gets
added; every type must serve at least two independent callers.
