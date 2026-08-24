//! Lowering a resolved pipeline into a spawn wiring plan.
//!
//! The [`ProcessHost`](crate::ProcessHost) receives a fully-resolved
//! [`LaunchSpec`]; this module computes, purely and with no I/O,
//! everything the runtime host must *do* to launch it over the spawn
//! attach block (`plans/SPAWN.md` SP10): which targets to open, how each
//! member's standard descriptors are wired, which opened handles travel to
//! which child, and which byte-pumping work the shell performs on its own
//! pipe ends between spawn and wait. Keeping the lowering pure keeps every
//! decision host-testable without a kernel; the runtime host merely executes
//! the plan through
//! `fs_open`/`resource_open`/`pipe_create`/`spawn_attached`, plus one System
//! Information API query for a read of a value-backed reference, which no
//! kernel backing can represent.
//!
//! Fail closed: a redirection the attach block cannot express (a descriptor
//! outside fd 0–3), a duplication of an unopened dynamic descriptor, or a
//! mixed-direction multios refuses the whole launch before anything is
//! opened — a plan either lowers completely or not at all.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::fs::OpenFlags;
use tairix_abi::Errno;
use tairix_resref::{KnownNamespace, NamespaceBacking, ResourceRef};

use crate::host::{LaunchSpec, RedirAction, RedirTarget, ResolvedCommand};
use crate::parser::OpenMode;

/// Number of wirable standard descriptors (fd 0–3), the attach block's
/// per-child wire count ([`tairix_abi::STD_STREAM_COUNT`]).
pub const STD_FDS: usize = tairix_abi::STD_STREAM_COUNT;

/// Identifies one planned open within a [`WirePlan`] — an index into
/// [`WirePlan::opens`]. The executor maps each id to the real descriptor the
/// open produced.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct OpenId(pub usize);

/// One handle the executor must produce before any member spawns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedOpen {
    /// Open a filesystem path with `flags` (`fs_open`).
    Path {
        /// The expanded target path.
        path: String,
        /// The open flags the redirection's mode derived.
        flags: OpenFlags,
    },
    /// Open a resource reference with `flags` (`resource_open`). The
    /// reference is carried in its canonical spelled form; the kernel
    /// re-parses and authorises it.
    Resource {
        /// The reference's canonical spelling (e.g. `sys:null`).
        reference: String,
        /// The open flags the redirection's mode derived.
        flags: OpenFlags,
    },
    /// Read the value-backed reference through the System Information API and
    /// present it to the child as a pipe.
    ///
    /// The kernel resolver cannot serve `info:`/`state:`/`stats:` — resolving
    /// a typed broker value kernel-side would bypass the broker's
    /// per-principal scoping — so the shell reads it under its own attested
    /// identity and the child reads an ordinary descriptor
    /// (`plans/ALIAS.md` §6.2).
    ///
    /// This entry is the pipe's **read end** and, unlike
    /// [`PlannedOpen::PipeRead`], has no paired write-end entry: the executor
    /// fills and closes the write end within the open phase. Only the
    /// reference spelling travels in the plan — a fact may be sensitive
    /// (`info:system/machine-id` is), and the plan outlives the read.
    ValuePipe {
        /// The reference's canonical spelling (e.g. `info:mem/physical`).
        reference: String,
    },
    /// Mint a pipe (`pipe_create`). This planned open is the pipe's **read
    /// end**; its paired write end is the [`PlannedOpen::PipeWrite`] that
    /// names this id.
    PipeRead,
    /// The write end of the pipe whose read end is `read`. Always planned
    /// immediately after its read end, so one `pipe_create` satisfies both.
    PipeWrite {
        /// The paired read end's id.
        read: OpenId,
    },
}

/// How one of a member's standard descriptors is backed — the plan-level
/// mirror of [`tairix_abi::FdWire`], with handles still abstract ids.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PlannedWire {
    /// The base table's own slot (the unwired default).
    Inherit,
    /// The base table's slot `n` — how `2>&1` onto an inherited stream is
    /// spelled.
    InheritSlot(u32),
    /// No backing; every access denies.
    Closed,
    /// A planned open's handle, cloned into the child at this fd.
    Handle(OpenId),
}

/// One member of the pipeline, ready to spawn: its argv/env views index the
/// originating [`ResolvedCommand`], and `wires` is its complete fd 0–3 map.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberPlan {
    /// Index of this member's [`ResolvedCommand`] in the launch spec.
    pub command: usize,
    /// The member's standard-descriptor wiring, indexed by fd.
    pub wires: [PlannedWire; STD_FDS],
}

/// Byte-pumping work the shell performs on its **own** retained pipe ends
/// after every member has spawned and every transferred end is closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PumpTask {
    /// Write `content` into the pipe write end `into`, then close it — the
    /// here-string / here-document feed.
    WriteContent {
        /// The retained pipe write end.
        into: OpenId,
        /// The exact bytes the child reads (trailing newline included).
        content: String,
    },
    /// Read the pipe read end `from` until end-of-stream, writing every
    /// chunk to each sink in order — the all-output multios fan-out.
    FanOut {
        /// The retained pipe read end the child's stream arrives on.
        from: OpenId,
        /// The opened sinks, in source order.
        sinks: Vec<OpenId>,
    },
    /// Read each source in order to end-of-stream, writing the bytes into
    /// the pipe write end `into`, then close it — the all-input multios
    /// concatenation.
    Concat {
        /// The retained pipe write end feeding the child.
        into: OpenId,
        /// The opened sources, in source order.
        sources: Vec<OpenId>,
    },
}

/// The complete, validated launch recipe for one pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WirePlan {
    /// Every handle to produce, in order, before any member spawns.
    pub opens: Vec<PlannedOpen>,
    /// One spawn per pipeline member, left to right. The last member is the
    /// job leader whose status becomes `$?`.
    pub members: Vec<MemberPlan>,
    /// Pumping the shell performs on its retained ends after the spawns.
    pub pumps: Vec<PumpTask>,
}

impl WirePlan {
    /// The ids of every planned open that is *transferred* to a child (wired
    /// into some member), in id order. The executor closes these in the
    /// parent immediately after the last spawn, so pipe end-of-stream and
    /// broken-pipe semantics see only the children as holders.
    #[must_use]
    pub fn transferred(&self) -> Vec<OpenId> {
        let mut ids: Vec<OpenId> = Vec::new();
        for member in &self.members {
            for wire in &member.wires {
                if let PlannedWire::Handle(id) = wire {
                    if !ids.contains(id) {
                        ids.push(*id);
                    }
                }
            }
        }
        ids.sort_unstable_by_key(|id| id.0);
        ids
    }

    /// The ids of every planned open the shell *retains* for pumping (the
    /// pump-side pipe ends and the multios sinks/sources), in id order. The
    /// executor closes these once the pumps have run.
    #[must_use]
    pub fn retained(&self) -> Vec<OpenId> {
        let mut ids: Vec<OpenId> = Vec::new();
        let mut keep = |id: &OpenId| {
            if !ids.contains(id) {
                ids.push(*id);
            }
        };
        for pump in &self.pumps {
            match pump {
                PumpTask::WriteContent { into, .. } => keep(into),
                PumpTask::FanOut { from, sinks } => {
                    keep(from);
                    sinks.iter().for_each(&mut keep);
                }
                PumpTask::Concat { into, sources } => {
                    keep(into);
                    sources.iter().for_each(&mut keep);
                }
            }
        }
        ids.sort_unstable_by_key(|id| id.0);
        ids
    }
}

/// Lower `spec` into a [`WirePlan`].
///
/// # Errors
///
/// * [`Errno::NotImplemented`] for a redirection the attach block cannot
///   express: a descriptor or duplication source outside fd 0–3 (the
///   `{var}` dynamic-descriptor forms).
/// * [`Errno::OutOfRange`] for a multios whose targets mix directions or
///   number fewer than two — shapes the interpreter never emits, re-checked
///   here so a hostile or buggy caller still fails closed.
pub fn lower(spec: &LaunchSpec<'_>) -> Result<WirePlan, Errno> {
    let mut plan = WirePlan {
        opens: Vec::new(),
        members: Vec::new(),
        pumps: Vec::new(),
    };
    let count = spec.commands.len();
    // The read end each member's fd 0 inherits from the pipe its left-hand
    // neighbour writes; None for the first member.
    let mut carried_read: Option<OpenId> = None;
    for (index, command) in spec.commands.iter().enumerate() {
        let mut wires = [PlannedWire::Inherit; STD_FDS];
        if let Some(read) = carried_read.take() {
            wires[0] = PlannedWire::Handle(read);
        }
        if index + 1 < count {
            let (read, write) = plan_pipe(&mut plan.opens);
            wires[1] = PlannedWire::Handle(write);
            carried_read = Some(read);
        }
        apply_redirections(&mut plan, &mut wires, command)?;
        plan.members.push(MemberPlan {
            command: index,
            wires,
        });
    }
    Ok(plan)
}

/// Plan one pipe, returning `(read, write)` ids.
fn plan_pipe(opens: &mut Vec<PlannedOpen>) -> (OpenId, OpenId) {
    let read = OpenId(opens.len());
    opens.push(PlannedOpen::PipeRead);
    let write = OpenId(opens.len());
    opens.push(PlannedOpen::PipeWrite { read });
    (read, write)
}

/// Apply `command`'s redirections, in source order, onto `wires`.
fn apply_redirections(
    plan: &mut WirePlan,
    wires: &mut [PlannedWire; STD_FDS],
    command: &ResolvedCommand,
) -> Result<(), Errno> {
    for redirection in &command.redirections {
        let fd = wirable_fd(redirection.fd)?;
        match &redirection.action {
            RedirAction::Open { mode, target } => {
                let id = plan_open(&mut plan.opens, *mode, target);
                wires[fd] = PlannedWire::Handle(id);
            }
            RedirAction::Dup { source } => {
                let source = wirable_fd(*source)?;
                wires[fd] = match wires[source] {
                    // Duplicating an untouched slot aliases the base
                    // table's backing for that slot, not "inherit my own".
                    #[allow(clippy::cast_possible_truncation)]
                    PlannedWire::Inherit => PlannedWire::InheritSlot(source as u32),
                    other => other,
                };
            }
            RedirAction::Close => {
                wires[fd] = PlannedWire::Closed;
            }
            RedirAction::HereString { content } => {
                let (read, write) = plan_pipe(&mut plan.opens);
                wires[fd] = PlannedWire::Handle(read);
                plan.pumps.push(PumpTask::WriteContent {
                    into: write,
                    content: content.clone(),
                });
            }
            RedirAction::Multi { targets } => {
                apply_multi(plan, wires, fd, targets)?;
            }
        }
    }
    Ok(())
}

/// Lower one multios action: all-output targets fan out through a pipe the
/// shell drains; all-input targets concatenate through a pipe the shell
/// feeds. Mixed directions (or fewer than two targets) fail closed.
fn apply_multi(
    plan: &mut WirePlan,
    wires: &mut [PlannedWire; STD_FDS],
    fd: usize,
    targets: &[(OpenMode, RedirTarget)],
) -> Result<(), Errno> {
    if targets.len() < 2 {
        return Err(Errno::OutOfRange);
    }
    let all_input = targets
        .iter()
        .all(|(mode, _)| matches!(mode, OpenMode::Read));
    let all_output = targets
        .iter()
        .all(|(mode, _)| matches!(mode, OpenMode::Write { .. } | OpenMode::Append { .. }));
    if !(all_input || all_output) {
        return Err(Errno::OutOfRange);
    }
    let ids: Vec<OpenId> = targets
        .iter()
        .map(|(mode, target)| plan_open(&mut plan.opens, *mode, target))
        .collect();
    let (read, write) = plan_pipe(&mut plan.opens);
    if all_output {
        wires[fd] = PlannedWire::Handle(write);
        plan.pumps.push(PumpTask::FanOut {
            from: read,
            sinks: ids,
        });
    } else {
        wires[fd] = PlannedWire::Handle(read);
        plan.pumps.push(PumpTask::Concat {
            into: write,
            sources: ids,
        });
    }
    Ok(())
}

/// Plan one target open with the flags `mode` derives, returning its id.
fn plan_open(opens: &mut Vec<PlannedOpen>, mode: OpenMode, target: &RedirTarget) -> OpenId {
    let flags = open_flags(mode);
    let id = OpenId(opens.len());
    opens.push(match target {
        RedirTarget::Path(path) => PlannedOpen::Path {
            path: path.clone(),
            flags,
        },
        // `ResourceRef`'s `Display` renders the canonical spelling, which
        // re-parses to an equal reference on whichever side resolves it.
        RedirTarget::Resource(reference) if reads_a_value(mode, reference) => {
            PlannedOpen::ValuePipe {
                reference: reference.to_string(),
            }
        }
        RedirTarget::Resource(reference) => PlannedOpen::Resource {
            reference: reference.to_string(),
            flags,
        },
    });
    id
}

/// Whether this redirection is a *read* of a *value-backed* reference — the
/// one shape the shell serves itself.
///
/// A value-backed resource is changed by a typed service command, never by
/// writing text at it, so `>`, `>>`, and `<>` keep reaching the kernel
/// resolver and its refusal (`plans/ALIAS.md` §6.4). `<>` is refused despite
/// its read half: serving half a request would silently downgrade it. The
/// backing comes from [`KnownNamespace::backing`], the same classifier the
/// kernel refuses on, so the two cannot disagree about which are streams.
fn reads_a_value(mode: OpenMode, reference: &ResourceRef) -> bool {
    matches!(mode, OpenMode::Read)
        && reference.namespace().known().map(KnownNamespace::backing)
            == Some(NamespaceBacking::Value)
}

/// The [`OpenFlags`] each redirection [`OpenMode`] opens its target with —
/// the one place the shell's operator vocabulary maps onto the filesystem
/// ABI. Writing modes create the target; `>` truncates, `>>` appends, `<>`
/// creates without truncating (the POSIX `O_RDWR|O_CREAT` shape).
#[must_use]
pub fn open_flags(mode: OpenMode) -> OpenFlags {
    match mode {
        OpenMode::Read => OpenFlags::READ,
        OpenMode::ReadWrite => OpenFlags::READ
            .union(OpenFlags::WRITE)
            .union(OpenFlags::CREATE),
        OpenMode::Write { .. } => OpenFlags::WRITE
            .union(OpenFlags::CREATE)
            .union(OpenFlags::TRUNCATE),
        OpenMode::Append { .. } => OpenFlags::WRITE
            .union(OpenFlags::CREATE)
            .union(OpenFlags::APPEND),
    }
}

/// Validate that `fd` names a wirable standard descriptor, returning it as
/// an index. The attach block wires only fd 0–3; a `{var}` dynamic
/// descriptor (≥ 10) cannot be expressed and fails closed rather than being
/// silently dropped.
fn wirable_fd(fd: u32) -> Result<usize, Errno> {
    let index = fd as usize;
    if index >= STD_FDS {
        return Err(Errno::NotImplemented);
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;

    use tairix_abi::fs::OpenFlags;
    use tairix_abi::Errno;

    use super::{lower, open_flags, OpenId, PlannedOpen, PlannedWire, PumpTask};
    use crate::host::{
        classify_redirect_target, LaunchSpec, RedirAction, RedirTarget, ResolvedCommand,
        ResolvedRedirection,
    };
    use crate::parser::OpenMode;

    fn command(argv: &[&str], redirections: Vec<ResolvedRedirection>) -> ResolvedCommand {
        ResolvedCommand {
            argv: argv.iter().copied().map(ToString::to_string).collect(),
            env_overrides: vec![],
            redirections,
        }
    }

    fn spec_plan(commands: &[ResolvedCommand]) -> Result<super::WirePlan, Errno> {
        lower(&LaunchSpec {
            commands,
            env: &[],
            background: false,
        })
    }

    fn open(fd: u32, mode: OpenMode, target: &str) -> ResolvedRedirection {
        ResolvedRedirection {
            fd,
            action: RedirAction::Open {
                mode,
                target: classify_redirect_target(target.to_string()).expect("test target"),
            },
        }
    }

    #[test]
    fn bare_command_inherits_everything() {
        let commands = [command(&["ps"], vec![])];
        let plan = spec_plan(&commands).expect("lowers");
        assert!(plan.opens.is_empty());
        assert!(plan.pumps.is_empty());
        assert_eq!(plan.members.len(), 1);
        assert_eq!(plan.members[0].wires, [PlannedWire::Inherit; 4]);
        assert!(plan.transferred().is_empty());
        assert!(plan.retained().is_empty());
    }

    #[test]
    fn pipeline_joints_wire_stdout_to_next_stdin() {
        let commands = [
            command(&["seq", "1", "100"], vec![]),
            command(&["wc", "-l"], vec![]),
        ];
        let plan = spec_plan(&commands).expect("lowers");
        assert_eq!(
            plan.opens,
            vec![
                PlannedOpen::PipeRead,
                PlannedOpen::PipeWrite { read: OpenId(0) }
            ]
        );
        assert_eq!(plan.members[0].wires[1], PlannedWire::Handle(OpenId(1)));
        assert_eq!(plan.members[0].wires[0], PlannedWire::Inherit);
        assert_eq!(plan.members[1].wires[0], PlannedWire::Handle(OpenId(0)));
        assert_eq!(plan.members[1].wires[1], PlannedWire::Inherit);
        // Both ends travel to children; the shell retains nothing.
        assert_eq!(plan.transferred(), vec![OpenId(0), OpenId(1)]);
        assert!(plan.retained().is_empty());
    }

    #[test]
    fn output_redirection_opens_with_truncate_and_wires_stdout() {
        let commands = [command(
            &["seq", "1", "5"],
            vec![open(1, OpenMode::Write { clobber: false }, "nums.txt")],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        assert_eq!(
            plan.opens,
            vec![PlannedOpen::Path {
                path: "nums.txt".to_string(),
                flags: OpenFlags::WRITE
                    .union(OpenFlags::CREATE)
                    .union(OpenFlags::TRUNCATE),
            }]
        );
        assert_eq!(plan.members[0].wires[1], PlannedWire::Handle(OpenId(0)));
    }

    #[test]
    fn resource_target_carries_its_canonical_spelling() {
        let commands = [command(
            &["ls"],
            vec![open(1, OpenMode::Write { clobber: false }, "sys:null")],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        let PlannedOpen::Resource { reference, .. } = &plan.opens[0] else {
            panic!("expected a resource open, got {:?}", plan.opens[0]);
        };
        assert_eq!(reference, "sys:null");
    }

    /// `cat < info:mem/physical`: a *read* of a value-backed reference lowers
    /// to a value pipe, whose read end is wired to the child's fd 0. This is
    /// the shape the kernel resolver cannot serve — a typed value is not a
    /// kernel byte stream — so the shell reads it over the System Information
    /// API instead (`plans/ALIAS.md` §6.2).
    #[test]
    fn reading_a_value_backed_reference_plans_a_value_pipe() {
        for reference in [
            "info:mem/physical",
            "info:system/machine-id",
            "state:net/resolver/servers",
            "stats:uptime",
        ] {
            let commands = [command(&["cat"], vec![open(0, OpenMode::Read, reference)])];
            let plan = spec_plan(&commands).expect("lowers");
            assert_eq!(
                plan.opens,
                vec![PlannedOpen::ValuePipe {
                    reference: reference.to_string()
                }],
                "{reference} reads as a value pipe"
            );
            // One planned open, not a read/write pair: the executor fills and
            // closes the write end itself, so no pump retains it.
            assert_eq!(plan.members[0].wires[0], PlannedWire::Handle(OpenId(0)));
            assert!(plan.pumps.is_empty(), "{reference} needs no pump");
            assert_eq!(plan.retained(), vec![], "{reference} retains nothing");
            assert_eq!(plan.transferred(), vec![OpenId(0)]);
        }
    }

    /// The plan carries the reference, never the value. A fact can be
    /// sensitive (`info:system/machine-id`), and the plan is a `Debug`-able
    /// structure that outlives the read, so the value belongs only in the
    /// executor's hand for as long as the pipe write takes.
    #[test]
    fn a_value_pipe_plan_holds_no_value() {
        let commands = [command(
            &["cat"],
            vec![open(0, OpenMode::Read, "info:system/machine-id")],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        let PlannedOpen::ValuePipe { reference } = &plan.opens[0] else {
            panic!("expected a value pipe, got {:?}", plan.opens[0]);
        };
        assert_eq!(reference, "info:system/machine-id");
        // Nothing anywhere in the plan but the spelling itself.
        assert!(plan.pumps.is_empty());
    }

    /// Every *write* direction keeps lowering to a kernel `resource_open`, so
    /// the kernel still refuses it with `Errno::NotSupported`: a value-backed
    /// reference is changed by a typed service command, never by writing text
    /// at it (`plans/ALIAS.md` §6.4). `<>` is refused too — half of what it
    /// asks for is unserviceable, so granting the read half would silently
    /// downgrade the request.
    #[test]
    fn writing_a_value_backed_reference_stays_the_kernels_refusal() {
        for mode in [
            OpenMode::Write { clobber: false },
            OpenMode::Write { clobber: true },
            OpenMode::Append { clobber: false },
            OpenMode::ReadWrite,
        ] {
            let commands = [command(&["ls"], vec![open(1, mode, "info:mem/physical")])];
            let plan = spec_plan(&commands).expect("lowers");
            assert!(
                matches!(&plan.opens[0], PlannedOpen::Resource { reference, .. }
                    if reference == "info:mem/physical"),
                "{mode:?} must reach the kernel resolver, got {:?}",
                plan.opens[0]
            );
        }
    }

    /// A *stream* namespace read is untouched: `sys:random` is a kernel
    /// backing, so it still opens through `resource_open`. The split is on the
    /// registry's own backing classification, so exactly one reader serves
    /// each namespace.
    #[test]
    fn reading_a_stream_reference_still_opens_through_the_kernel() {
        let commands = [command(
            &["head"],
            vec![open(0, OpenMode::Read, "sys:random")],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        assert!(
            matches!(&plan.opens[0], PlannedOpen::Resource { reference, .. }
                if reference == "sys:random"),
            "got {:?}",
            plan.opens[0]
        );
    }

    /// An all-input multios may mix value pipes with ordinary sources: each is
    /// a read end the shared `Concat` pump drains in order, so
    /// `cat < info:system/hostname < notes.txt` concatenates the two.
    #[test]
    fn an_input_multios_may_concatenate_a_value_pipe() {
        let commands = [command(
            &["cat"],
            vec![ResolvedRedirection {
                fd: 0,
                action: RedirAction::Multi {
                    targets: vec![
                        (
                            OpenMode::Read,
                            classify_redirect_target("info:system/hostname".to_string())
                                .expect("reference"),
                        ),
                        (
                            OpenMode::Read,
                            classify_redirect_target("notes.txt".to_string()).expect("path"),
                        ),
                    ],
                },
            }],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        assert_eq!(
            plan.opens[0],
            PlannedOpen::ValuePipe {
                reference: "info:system/hostname".to_string()
            }
        );
        assert!(matches!(&plan.opens[1], PlannedOpen::Path { path, .. } if path == "notes.txt"));
        assert_eq!(
            plan.pumps,
            vec![PumpTask::Concat {
                into: OpenId(3),
                sources: vec![OpenId(0), OpenId(1)],
            }]
        );
    }

    #[test]
    fn dup_after_open_shares_the_open_handle() {
        // `cmd > out 2>&1`: fd 2 aliases the same opened handle as fd 1, so
        // the kernel clones one shared open description into both slots.
        let commands = [command(
            &["cmd"],
            vec![
                open(1, OpenMode::Write { clobber: false }, "out"),
                ResolvedRedirection {
                    fd: 2,
                    action: RedirAction::Dup { source: 1 },
                },
            ],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        assert_eq!(plan.members[0].wires[1], PlannedWire::Handle(OpenId(0)));
        assert_eq!(plan.members[0].wires[2], PlannedWire::Handle(OpenId(0)));
    }

    #[test]
    fn dup_of_an_untouched_slot_aliases_the_base_table() {
        // A bare `2>&1` (no preceding open): the child's fd 2 must become
        // whatever backs the *base table's* fd 1.
        let commands = [command(
            &["cmd"],
            vec![ResolvedRedirection {
                fd: 2,
                action: RedirAction::Dup { source: 1 },
            }],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        assert_eq!(plan.members[0].wires[2], PlannedWire::InheritSlot(1));
    }

    #[test]
    fn close_wires_the_slot_closed() {
        let commands = [command(
            &["cmd"],
            vec![ResolvedRedirection {
                fd: 0,
                action: RedirAction::Close,
            }],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        assert_eq!(plan.members[0].wires[0], PlannedWire::Closed);
    }

    #[test]
    fn here_string_plans_a_pipe_and_a_write_pump() {
        let commands = [command(
            &["cat"],
            vec![ResolvedRedirection {
                fd: 0,
                action: RedirAction::HereString {
                    content: "hello\n".to_string(),
                },
            }],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        assert_eq!(plan.members[0].wires[0], PlannedWire::Handle(OpenId(0)));
        assert_eq!(
            plan.pumps,
            vec![PumpTask::WriteContent {
                into: OpenId(1),
                content: "hello\n".to_string(),
            }]
        );
        // The read end travels to the child; the write end stays for the pump.
        assert_eq!(plan.transferred(), vec![OpenId(0)]);
        assert_eq!(plan.retained(), vec![OpenId(1)]);
    }

    #[test]
    fn output_multios_fans_out_through_a_shell_drained_pipe() {
        let commands = [command(
            &["cmd"],
            vec![ResolvedRedirection {
                fd: 1,
                action: RedirAction::Multi {
                    targets: vec![
                        (
                            OpenMode::Write { clobber: false },
                            RedirTarget::Path("a".to_string()),
                        ),
                        (
                            OpenMode::Append { clobber: false },
                            RedirTarget::Path("b".to_string()),
                        ),
                    ],
                },
            }],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        // Opens: the two sinks, then the pipe pair.
        assert_eq!(plan.opens.len(), 4);
        assert_eq!(plan.members[0].wires[1], PlannedWire::Handle(OpenId(3)));
        assert_eq!(
            plan.pumps,
            vec![PumpTask::FanOut {
                from: OpenId(2),
                sinks: vec![OpenId(0), OpenId(1)],
            }]
        );
        assert_eq!(plan.transferred(), vec![OpenId(3)]);
        assert_eq!(plan.retained(), vec![OpenId(0), OpenId(1), OpenId(2)]);
    }

    #[test]
    fn input_multios_concatenates_through_a_shell_fed_pipe() {
        let commands = [command(
            &["cmd"],
            vec![ResolvedRedirection {
                fd: 0,
                action: RedirAction::Multi {
                    targets: vec![
                        (OpenMode::Read, RedirTarget::Path("part1".to_string())),
                        (OpenMode::Read, RedirTarget::Path("part2".to_string())),
                    ],
                },
            }],
        )];
        let plan = spec_plan(&commands).expect("lowers");
        assert_eq!(plan.members[0].wires[0], PlannedWire::Handle(OpenId(2)));
        assert_eq!(
            plan.pumps,
            vec![PumpTask::Concat {
                into: OpenId(3),
                sources: vec![OpenId(0), OpenId(1)],
            }]
        );
    }

    #[test]
    fn redirection_overrides_a_pipeline_joint() {
        // `a > out | b`: the explicit redirection wins over the joint (the
        // POSIX application order — redirections apply after pipe setup).
        let commands = [
            command(
                &["a"],
                vec![open(1, OpenMode::Write { clobber: false }, "out")],
            ),
            command(&["b"], vec![]),
        ];
        let plan = spec_plan(&commands).expect("lowers");
        assert_eq!(plan.members[0].wires[1], PlannedWire::Handle(OpenId(2)));
        assert_eq!(plan.members[1].wires[0], PlannedWire::Handle(OpenId(0)));
    }

    #[test]
    fn dynamic_descriptors_fail_closed() {
        // `{var}>out` allocates fd ≥ 10 — inexpressible over the fd 0–3
        // attach block, so the whole launch refuses.
        let commands = [command(
            &["cmd"],
            vec![open(10, OpenMode::Write { clobber: false }, "out")],
        )];
        assert_eq!(spec_plan(&commands), Err(Errno::NotImplemented));
    }

    #[test]
    fn dynamic_dup_source_fails_closed() {
        let commands = [command(
            &["cmd"],
            vec![ResolvedRedirection {
                fd: 1,
                action: RedirAction::Dup { source: 10 },
            }],
        )];
        assert_eq!(spec_plan(&commands), Err(Errno::NotImplemented));
    }

    #[test]
    fn mixed_direction_multios_fails_closed() {
        let commands = [command(
            &["cmd"],
            vec![ResolvedRedirection {
                fd: 1,
                action: RedirAction::Multi {
                    targets: vec![
                        (
                            OpenMode::Write { clobber: false },
                            RedirTarget::Path("a".to_string()),
                        ),
                        (OpenMode::Read, RedirTarget::Path("b".to_string())),
                    ],
                },
            }],
        )];
        assert_eq!(spec_plan(&commands), Err(Errno::OutOfRange));
    }

    #[test]
    fn undersized_multios_fails_closed() {
        let commands = [command(
            &["cmd"],
            vec![ResolvedRedirection {
                fd: 1,
                action: RedirAction::Multi {
                    targets: vec![(
                        OpenMode::Write { clobber: false },
                        RedirTarget::Path("a".to_string()),
                    )],
                },
            }],
        )];
        assert_eq!(spec_plan(&commands), Err(Errno::OutOfRange));
    }

    #[test]
    fn a_failed_lowering_plans_nothing() {
        // The failing member is the second one; the error must discard the
        // whole plan, never a half-lowered prefix.
        let commands = [
            command(&["a"], vec![]),
            command(
                &["b"],
                vec![open(4, OpenMode::Write { clobber: false }, "out")],
            ),
        ];
        assert_eq!(spec_plan(&commands), Err(Errno::NotImplemented));
    }

    #[test]
    fn open_flags_map_each_mode_onto_the_fs_abi() {
        assert_eq!(open_flags(OpenMode::Read), OpenFlags::READ);
        assert_eq!(
            open_flags(OpenMode::ReadWrite),
            OpenFlags::READ
                .union(OpenFlags::WRITE)
                .union(OpenFlags::CREATE)
        );
        assert_eq!(
            open_flags(OpenMode::Write { clobber: true }),
            OpenFlags::WRITE
                .union(OpenFlags::CREATE)
                .union(OpenFlags::TRUNCATE)
        );
        assert_eq!(
            open_flags(OpenMode::Append { clobber: false }),
            OpenFlags::WRITE
                .union(OpenFlags::CREATE)
                .union(OpenFlags::APPEND)
        );
    }

    #[test]
    fn every_planned_flag_set_is_kernel_admissible() {
        // The kernel re-validates flag combinations fail-closed
        // (`OpenFlags::from_bits`); every set the planner can emit must be
        // admissible, or a legal redirection would be refused at open time.
        for mode in [
            OpenMode::Read,
            OpenMode::ReadWrite,
            OpenMode::Write { clobber: false },
            OpenMode::Append { clobber: false },
        ] {
            let flags = open_flags(mode);
            assert_eq!(OpenFlags::from_bits(flags.bits()), Ok(flags));
        }
    }

    #[test]
    fn three_member_pipeline_chains_two_pipes() {
        let commands = [
            command(&["a"], vec![]),
            command(&["b"], vec![]),
            command(&["c"], vec![]),
        ];
        let plan = spec_plan(&commands).expect("lowers");
        let joints: Vec<_> = plan
            .opens
            .iter()
            .filter(|open| matches!(open, PlannedOpen::PipeRead))
            .collect();
        assert_eq!(joints.len(), 2);
        assert_eq!(plan.members[0].wires[1], PlannedWire::Handle(OpenId(1)));
        assert_eq!(plan.members[1].wires[0], PlannedWire::Handle(OpenId(0)));
        assert_eq!(plan.members[1].wires[1], PlannedWire::Handle(OpenId(3)));
        assert_eq!(plan.members[2].wires[0], PlannedWire::Handle(OpenId(2)));
    }
}
