# tairix-discovery

TAIRiX link-local service discovery client (`plans/ZEROCONF.md` Z4). Stability
tier: **experimental**.

A program browses for, resolves, and looks up link-local services through the
`discoveryd` service with this crate, never by speaking multicast DNS itself:
the network stack reserves the port and groups to that service. The pure layer
([`Session`], [`Held`]) runs over an injected transport and is host-tested; the
`program` feature adds the production glue that calls the discovery endpoint
and parks on the session's doorbell.

See `docs/src/lib/discovery.md`.
