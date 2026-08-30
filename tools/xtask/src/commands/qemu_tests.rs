//! QEMU integration-test driver invoked by `cargo xtask test --qemu`.
//!
//! the charter mandates that the QEMU tests share the same orchestrator
//! as host-side tests and that each QEMU run has a *strict* per-test
//! timeout with **no retries**. This module enforces both: it builds the
//! enrolled kernels per target triple, then drives each one through
//! [`tairix_qemu::Runner::run`], failing the whole `xtask test` invocation
//! if any guest fails or times out.
//!
//! The guests run **concurrently** through the shared weighted-concurrency
//! runner ([`super::parallel`]): each enrolment is independent (its own
//! per-binary backing images, a `-serial stdio` console, and a unique unix
//! monitor socket), so the only resource they contend for is host CPU. The
//! runner charges a uniprocessor guest two units (its vCPU plus emulator/I/O
//! work) and an SMP guest the whole budget — so an SMP guest runs alone,
//! because a co-scheduled SMP guest starves so badly it trips its own in-guest
//! lockup watchdog. The budget is one third of the host's logical CPUs:
//! deliberate headroom (a QEMU guest is far heavier than its lone vCPU thread,
//! and each guest runs its own real-time watchdogs) so guests are never
//! oversubscribed into missing their internal deadlines. Within that budget a
//! few uniprocessor guests overlap; each guest's deadline is itself an
//! *inactivity* budget, not a total-runtime one ([`tairix_qemu`]), so a guest
//! that runs a little slower co-scheduled keeps emitting serial output and is
//! never mistaken for a hung one. See [`run_once`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use tairix_itest_harness::pie::PieArch;
use tairix_qemu::{Outcome, ReservedSocket, Runner, Spec};

use super::image_apps::AppStoreFile;
use super::parallel::{self, Job};
use crate::{Context, LONG_BUILD_COMMAND_TIMEOUT};

/// One enrolled QEMU integration test.
struct QemuTest {
    /// Cargo package name (matches `[package].name`).
    package: &'static str,
    /// Binary name produced by the package (`[[bin]].name`).
    binary: &'static str,
    /// Rust target triple the binary is built for. Selects both the
    /// `cargo build --target` value and the per-arch QEMU `Spec`
    /// constructor (`x86_64-unknown-none` → `isa-debug-exit`;
    /// `riscv64gc-unknown-none-elf` → the `virt` board + `SiFive`
    /// Test finisher).
    target: &'static str,
    /// Number of emulated CPUs.
    cpus: u32,
    /// Inactivity (no-progress) budget: the longest the guest may fall silent
    /// on the serial console before the runner treats it as hung. Not a
    /// total-runtime deadline, so it is immune to how heavily the matrix is
    /// co-scheduled.
    timeout: Duration,
    /// When `Some(n)`, give the guest `n` mebibytes of RAM instead of the
    /// per-arch default. Only for an enrolment whose subject *is* the amount
    /// of RAM: a bigger guest costs the host every byte the kernel touches,
    /// so it is never headroom for its own sake.
    ram_mib: Option<u32>,
    /// When `Some(n)`, attach an `n`-sector raw virtio-blk backing
    /// image whose sector 0 carries the deterministic pattern
    /// `byte[i] = i mod 256` (which the kernel-side test verifies).
    disk_sectors: Option<u64>,
    /// How (if at all) to attach a virtio-net interface over a QEMU
    /// `dgram` unix-socket netdev with the harness-side `netpeer` link
    /// peer on its other end, dumping every frame to a `<binary>.pcap`
    /// capture beside the kernel image so a host can inspect the on-wire
    /// exchange. [`NetPeerMode::None`] attaches no interface.
    netstack_peer: NetPeerMode,
    /// When `true`, attach a QEMU `ramfb` display device (a
    /// firmware-programmed linear framebuffer in guest RAM). Used by the
    /// framebuffer-display vertical on the riscv64 `virt` board.
    ramfb: bool,
    /// Filesystem volume to plant on a raw virtio-blk backing image
    /// (independent of the `disk_sectors` sector-0 pattern). The
    /// kernel-side test mounts it through the real driver and
    /// round-trips a read and a write.
    fs_disk: FsDisk,
    /// When `Some((marker, key))`, attach a `virtio-keyboard-device` and
    /// inject `key` (a QEMU `QKeyCode`) once the guest prints `marker` on
    /// the serial console. Used by the aarch64 virtio-input vertical to
    /// make a real device→driver input event deterministic.
    keyboard: Option<(&'static str, &'static str)>,
    /// Ordered typed-keys script: for each `(marker, occurrences, text)`
    /// step, attach a `virtio-keyboard-device` and type `text` through
    /// paced monitor `sendkey`s once `marker` has appeared `occurrences`
    /// times on the serial console; the steps run strictly in order. The
    /// typed-dialogue path for a guest whose primary console is the
    /// display (a `ramfb` world), which the `serial` script cannot reach:
    /// typed keys buffer as console type-ahead until the guest's reader
    /// drains them. Used by the autoload vertical to type the unlock
    /// passphrase and then the login + graphical-choice dialogue at the
    /// seat keyboard.
    typed_keys: &'static [(&'static str, u32, &'static str)],
    /// Ordered, marker-gated QEMU monitor `screendump`s of the guest
    /// display — the host-side scan-out readbacks proving each composited
    /// frame of interest reached the surface (`plans/DISPLAY.md` D7d,
    /// `plans/APPWIN.md` AW3). Each dump is taken once its marker has
    /// appeared the required number of times on serial, holds later dumps
    /// and still-unsent pointer steps back until its image parses
    /// completely, and is checked by its own assertion after a PASS.
    screendumps: &'static [ScreendumpPlan],
    /// When `Some`, attach a `virtio-mouse-device` after the keyboard —
    /// the two-identical-virtio-input-nodes topology an interactive
    /// session presents, proving per-node driver instances — and drive
    /// the ordered, marker-gated pointer script the builder returns
    /// (moves and button clicks) through the QEMU monitor. A builder
    /// function rather than a static table because the click coordinates
    /// are computed from the production desktop's own layout code at run
    /// time — the same definition the guest renders with — never
    /// hand-copied literals (`plans/APPWIN.md` AW3).
    pointer_script: Option<PointerScriptBuilder>,
    /// Ordered serial-input script: for each `(marker, delay, line)` step,
    /// pipe QEMU's stdin, wait `delay` after `marker` appears past the previous
    /// match, then type `line` one paced byte at a time. The run fails if the
    /// guest exits before every step was sent, so an unreached prompt is a test
    /// failure. Used by the aarch64 interactive-session vertical to hold a
    /// deterministic multi-exchange dialogue with the blocked login.
    serial: &'static [(&'static str, Duration, &'static str)],
}

/// Builds an enrolment's ordered pointer script at run time (the click
/// coordinates come from the production desktop's own layout code), or
/// describes why it cannot.
type PointerScriptBuilder = fn() -> Result<Vec<tairix_qemu::PointerStep>, String>;

/// The pixel assertion a [`ScreendumpPlan`] applies to its dumped image.
type ScreendumpAssert = fn(&QemuTest, &Path) -> Result<(), String>;

/// One marker-gated screendump a [`QemuTest`] takes, with the assertion
/// its decoded pixels must satisfy after a PASS. Dumps run strictly in
/// declaration order (the runner holds later dumps and still-unsent
/// pointer steps back until the current dump's image parses completely).
struct ScreendumpPlan {
    /// Serial marker gating the dump — a guest-emitted witness that the
    /// frame of interest reached the scan-out surface.
    marker: &'static str,
    /// How many times the marker must appear before the dump is taken.
    occurrences: u32,
    /// File-name suffix distinguishing this dump's `.ppm` beside the
    /// kernel binary (`<binary>.<suffix>.screendump.ppm`).
    suffix: &'static str,
    /// The pixel assertion applied to the dumped image after a PASS.
    assert: ScreendumpAssert,
}

/// How a [`QemuTest`] attaches its virtio-net interface and drives the
/// harness-side `netpeer` link peer.
#[derive(Clone, Copy, Eq, PartialEq)]
enum NetPeerMode {
    /// No network interface attached.
    None,
    /// A v6-link-local-only peer: the device MAC is pinned to
    /// `tairix_test_netstack_wire::GUEST_MAC` so the guest's EUI-64
    /// link-local is deterministic, and the peer campaigns over that
    /// link-local alone (the two-process autoload vertical, whose guest
    /// has no admin-assigned IPv4).
    V6LinkLocal,
    /// A v6-link-local-only *passive TCP echo server* (the N5c stream
    /// vertical): same deterministic link-local addressing as
    /// [`Self::V6LinkLocal`], but the peer accepts the guest client's TCP
    /// connection on `tairix_test_netstack_wire::PEER_TCP_PORT`, echoes
    /// every received byte back, and injects bounded frame loss so the
    /// stream survives retransmission.
    V6TcpEcho,
    /// A v6-link-local-only *ECN-verifying passive TCP echo server* (the N13
    /// ECN vertical): same deterministic link-local addressing and echo
    /// transfer as [`Self::V6TcpEcho`], but the peer's connection is
    /// ECN-capable and it verifies RFC 3168 Explicit Congestion Notification
    /// on the wire — the guest's SYN offers ECN (ECE+CWR), the guest's data
    /// carries ECT(0), and, after the peer echoes ECE for an injected
    /// congestion mark, the guest sets CWR on a subsequent segment. Its
    /// verdict requires all three plus the full echoed transfer, so a stack
    /// that ignored the `net.tcp.ecn` toggle fails the run loud.
    V6TcpEchoEcn,
    /// A v6-link-local-only *active TCP client* (the N6b-2-β-2 listener
    /// vertical): same deterministic link-local addressing as
    /// [`Self::V6LinkLocal`], but the peer connects to the guest
    /// `tcpserve` server on `tairix_test_netstack_wire::GUEST_TCP_PORT`,
    /// streams the whole transfer, verifies the guest echoes every byte
    /// back, and injects bounded frame loss so the stream survives
    /// retransmission (the role-swapped mirror of [`Self::V6TcpEcho`]).
    V6TcpConnect,
    /// A v6-link-local-only *SYN-flood client* (the N16b
    /// connection-exhaustion vertical): same deterministic link-local
    /// addressing as [`Self::V6LinkLocal`], but the peer first fills the
    /// guest listener's half-open backlog with SYNs it never answers, then
    /// opens one real connection to `tairix_test_netstack_wire::GUEST_TCP_PORT`
    /// that the listener can only admit through a stateless RFC 4987 SYN
    /// cookie, and verifies the guest echoes the whole transfer back over it.
    /// Its verdict requires both the filled backlog and the verified echo, so
    /// a run that never engaged the cookie brake fails loud.
    V6TcpFlood,
    /// A v6-link-local-only *passive ICMP echo responder* (the N8b-2b-β
    /// `ping` vertical): same deterministic link-local addressing as
    /// [`Self::V6LinkLocal`], but the peer runs no campaign — it answers the
    /// guest's neighbour resolution and every `ICMPv6` echo request the guest
    /// `ping` tool sends over the shared IPv6 link-local wire, and its verdict
    /// requires at least one served request (so the guest must actually have
    /// reached it).
    V6PingResponder,
    /// A **telnet server** peer (the `plans/TELNET.md` vertical): same
    /// deterministic link-local addressing as [`Self::V6LinkLocal`], but the
    /// peer accepts the guest `telnet` client's connection on
    /// `tairix_test_netstack_wire::PEER_TELNET_PORT` and speaks the *server*
    /// half of RFC 854 — offering `SUPPRESS GO AHEAD`, asking for
    /// `TERMINAL TYPE`, `NAWS` and `LINEMODE`, and driving the RFC 1184 `MODE`
    /// and `SLC` exchange — before greeting the session and echoing the
    /// operator's probe line back upper-cased. Its verdict requires every step,
    /// so a client that connected but ignored the negotiation, declined
    /// LINEMODE, or never reported its window fails the run loud.
    V6TelnetServer,
    /// A **static-addressing** ICMP-campaign peer (the N9b-3-2-β-2-ii-b
    /// `match.node` vertical): the peer takes its own static address in the
    /// shared on-link `/64` ([`tairix_test_netstack_wire::PEER_STATIC_V6`])
    /// and campaigns over the guest's **static** address
    /// ([`tairix_test_netstack_wire::GUEST_STATIC_V6`]) — the address the
    /// guest holds only if its planted `network.conf` bound the NIC by
    /// `match.node` and assigned the static address. Unlike
    /// [`Self::V6LinkLocal`] the peer never pings the EUI-64 link-local, so a
    /// `match.node` mis-bind (the static address never assigned) leaves the
    /// campaign incomplete and fails the run loud.
    V6StaticEcho,
    /// A **DHCPv4-server** peer (the DHCP D3 vertical): the peer takes its
    /// own [`tairix_test_netstack_wire::DHCP_SERVER_V4`] in the shared `/24`,
    /// answers the guest's DHCP `DISCOVER`/`REQUEST` with an
    /// `OFFER`/`ACK` of [`tairix_test_netstack_wire::DHCP_LEASED_V4`], and
    /// then campaigns over that *leased* address. The guest's planted
    /// `network.conf` selects `ipv4.method dhcp` and disables IPv6, so it
    /// forms no address itself — the leased address is its only reachable
    /// one, so a broken lease leaves the campaign unanswered and fails the
    /// run loud. The IPv4 analogue of [`Self::V6StaticEcho`].
    V4DhcpEcho,
    /// A **DHCPv6-server** peer (the DHCP D4c vertical): the peer takes its
    /// own [`tairix_test_netstack_wire::DHCP6_SERVER_V6`] in the shared
    /// on-link `/64`, answers the guest's DHCPv6 `Solicit`/`Request` with an
    /// `Advertise`/`Reply` leasing [`tairix_test_netstack_wire::DHCP6_LEASED_V6`]
    /// (RFC 8415 stateful IA_NA), and — because DHCPv6 conveys no on-link
    /// prefix — also emits Router Advertisements naming the shared prefix
    /// on-link (non-autonomous) so the guest can reach it, then campaigns over
    /// the *leased* address. The guest's planted `network.conf` selects
    /// `ipv6.method dhcp` and disables IPv4, so it forms no global address
    /// itself — the leased address is its only reachable one, so a broken
    /// lease leaves the campaign unanswered and fails the run loud. The IPv6
    /// analogue of [`Self::V4DhcpEcho`].
    V6Dhcp6Echo,
    /// A **bond-failover** peer (the N9b-3-2-β-2-ii-b-bond vertical): the
    /// guest binds *two* virtio-net NICs as the members of one active-backup
    /// bond, so the runner attaches **two** `dgram` netdevs (`net0` pinned to
    /// [`tairix_test_netstack_wire::GUEST_MAC`], `net1` to `GUEST_MAC_2`) and
    /// the peer serves both wires at once, campaigning to the bond's static
    /// address ([`tairix_test_netstack_wire::GUEST_STATIC_V6`]). Mid-flow the
    /// runner drops the primary member's carrier over the QEMU monitor
    /// (`set_link net0 off`, gated on the guest's first served echo), forcing
    /// the failover the guest witnesses. Its verdict requires a reply, and
    /// the guest requires a post-failover served echo, so neither side can
    /// pass without the flow surviving the member drop.
    Bond,
    /// An **NTP-server** peer (the `plans/TIMESYNC.md` TS-2 vertical): the
    /// peer takes its own [`tairix_test_netstack_wire::PEER_STATIC_V6`] on the
    /// guest's on-link `/64` and answers each of the guest's NTP client
    /// requests **twice, spoof first** — a well-formed reply whose origin
    /// timestamp does not echo the request's nonce and which reports
    /// [`tairix_test_netstack_wire::NTP_SPOOF_SECS`], then the truthful reply
    /// echoing the nonce and reporting
    /// [`tairix_test_netstack_wire::NTP_FIXTURE_SECS`].
    ///
    /// That ordering is the discriminator: a guest that accepted the spoof
    /// would set its clock to the wrong instant, and a guest that let the
    /// spoof cancel its outstanding transaction would ignore the truthful
    /// reply and never set the clock at all. The peer's verdict requires a
    /// served request; the guest's own audit witness (the applied
    /// `wall_secs=`) says which reply it believed, so neither side passes
    /// alone. Its gate is deliberately **not** a completion gate: the peer
    /// trips as it sends, while the property under test is what the guest
    /// does next.
    NtpServer,
}

/// Which filesystem volume (if any) the host harness plants on the
/// test's virtio-blk backing image. Each variant names a shared
/// single-source-of-truth image fixture.
// `ARXFS` is the filesystem's product name and is spelled in full capitals
// everywhere; the mixed-case `Arxfs` the acronym lint would otherwise require
// is not an accepted spelling of the name.
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum FsDisk {
    /// No filesystem volume (the test uses `disk_sectors` or no disk).
    None,
    /// The shared [`tairix_test_fat32_image`] FAT32 volume.
    Fat32,
    /// The shared [`tairix_test_arxfs_image`] arxfs volume.
    ARXFS,
    /// The shared [`tairix_test_arxfs_image`] users-root volume:
    /// the standard filesystem tree with `/System/Security/Users` planted
    /// (`plans/PI.md` P11).
    UsersRoot,
    /// The shared [`tairix_test_encrypted_root_image`] whole-disk image: an
    /// MBR, a FAT boot partition carrying the `root.unlock` descriptor, and
    /// a passphrase-derived encrypted `ARXFS` root carrying
    /// `/System/Security/Users` — the root-mount->login vertical's backing
    /// (`plans/PI.md` P11 Chunk B-2).
    EncryptedRootDisk,
    /// The [`Self::EncryptedRootDisk`] layout whose **read-only `/System`
    /// volume** additionally carries the kernel-signed autoload driver
    /// bundles — the virtio-input keyboard driver, the framebuffer display
    /// service, and the virtio-net link-layer driver — in its `Drivers/`
    /// store. The bundles are the ones the `image_drivers` pipeline
    /// cross-compiles and signs ([`super::image_drivers::autoload_driver_store_files`]),
    /// planted through the same generic encrypted-root fixture; the pre-unlock
    /// driver-loading-by-discovery autoload vertical's backing (`plans/PI.md`
    /// design B / B2, `plans/NETWORK.md` N4e).
    AutoloadRootDisk,
    /// The [`Self::AutoloadRootDisk`] layout with **no** planted
    /// `os.loginType` document ([`login_type_plant`]). A machine nobody has
    /// configured is the state every fresh installation boots in, so this is
    /// the disk that exercises the compiled default: the display driver
    /// autoloads, the settings store is reachable and holds nothing, and
    /// login must reach the graphical login screen on its own
    /// (`plans/NEW-DESKTOP-LOGIN.md` G7.1).
    GreeterRootDisk,
    /// The [`Self::EncryptedRootDisk`] layout whose **read-only `/System`
    /// volume** additionally carries the test-only `memsoak` fixture bundle
    /// ([`super::image_apps::memsoak_store_files`]) — the memory-stability
    /// vertical's backing (`plans/APPS.md` "Immediate work" I2/I3). The
    /// fixture crate lives outside the userland discovery walk, so only
    /// this disk ever carries it; no production image ships it.
    MemsoakRootDisk,
    /// The [`Self::AutoloadRootDisk`] layout — the same graphical world, with
    /// the signed input and display driver bundles and the text-login
    /// document — whose store additionally carries the test-only `framestats`
    /// frame-sample fixture bundle
    /// ([`super::image_apps::framestats_store_files`]): the desktop-hover
    /// vertical's backing (`plans/FIX-DESKTOP-SPEEDUP.md` A.4).
    ///
    /// Its own disk rather than a bundle added to the shared autoload image,
    /// because the fixture declares a program-library folder and so appears in
    /// the popup: planting it on the shared image would move every other
    /// desktop vertical's library rows out from under the coordinates their
    /// scripts click.
    HoverRootDisk,
    /// The [`Self::EncryptedRootDisk`] layout whose **read-only `/System`
    /// volume** additionally carries the signed virtio-net driver bundle
    /// (only — no display/input driver, so the console stays the UART text
    /// console the serial script drives) plus the test-only `tcpecho`
    /// fixture bundle ([`super::image_apps::tcpecho_store_files`]) — the
    /// stream-socket vertical's backing (`plans/NETWORK.md` N5c). `devmgr`
    /// autoloads the NIC driver into its own process and `netstack` binds
    /// it, so the guest `tcpecho` client reaches the host echo peer over the
    /// live two-process network. Only this disk carries the fixtures; no
    /// production image ships them.
    StreamRootDisk,
    /// The [`Self::StreamRootDisk`] layout **plus** a planted
    /// `/System/Settings/Configuration/system.conf`
    /// ([`tairix_test_netstack_wire::ECN_SYSTEM_CONF`]) that turns
    /// `net.tcp.ecn` on stack-wide — the ECN vertical's backing
    /// (`plans/NETWORK.md` N13). `devmgr` reads the planted store pre-unlock
    /// and delivers `tcp_ecn = true` to `netstack`, so the guest `tcpecho`
    /// client's connection negotiates RFC 3168 ECN with the ECN-verifying
    /// host echo peer over the live two-process network. Same net-only driver
    /// set as [`Self::StreamRootDisk`] (so the console stays the UART text
    /// console the serial script drives); only this disk carries the
    /// fixtures, no production image ships them.
    EcnRootDisk,
    /// The [`Self::StreamRootDisk`] layout, but carrying the test-only
    /// `tcpserve` TCP-**listener** fixture bundle
    /// ([`super::image_apps::tcpserve_store_files`]) in place of the
    /// `tcpecho` client — the listener vertical's backing
    /// (`plans/NETWORK.md` N6b-2-β-2). Same net-only driver set (so the
    /// console stays the UART text console the serial script drives); the
    /// guest `tcpserve` server binds a privileged port and the host client
    /// peer connects to it over the live two-process network. Only this disk
    /// carries the fixtures; no production image ships them.
    ListenRootDisk,
    /// The net-only-driver encrypted-root layout carrying the **standard**
    /// signed application store (so the real shipping command bundles are
    /// present — no test-only fixture) plus the signed virtio-net driver
    /// bundle. `devmgr` autoloads the NIC driver into its own process and
    /// `netstack` binds it, so a guest *network tool* reaches its host peer
    /// over the live two-process network. The console stays the UART text
    /// console the serial script drives (no display/input driver).
    ///
    /// Shared by every vertical that exercises a shipped network command as a
    /// user would run it: `ping` (`plans/NETWORK.md` N8b-2b-β) and `telnet`
    /// (`plans/TELNET.md`). They differ only in the host peer and the serial
    /// script, so they share the one disk rather than each planting its own.
    NetToolRootDisk,
    /// The net-only-driver encrypted-root layout carrying the **standard**
    /// signed application store (the `netstack`/`devmgr` service bundles, no
    /// test-only fixture) **plus** a planted
    /// `/System/Settings/Network/network.conf`
    /// ([`tairix_test_netstack_wire::STATIC_NETWORK_CONF_AARCH64`]) — the
    /// static-addressing (`match.node`) vertical's backing
    /// (`plans/NETWORK.md` N9b-3-2-β-2-ii-b). `devmgr` autoloads the NIC
    /// driver into its own process, reads the planted config from the
    /// read-only `/System` volume, and binds the NIC to the `wan` alias by
    /// its stable bus location, assigning it the config's static IPv6
    /// address — all pre-unlock, so the guest needs no console dialogue. The
    /// host peer addresses the guest's *static* address, so a `match.node`
    /// mis-bind fails the run loud rather than falling back to the link-local
    /// the guest always forms. The console stays the UART text console (no
    /// display/input driver).
    StaticNetRootDisk,
    /// The [`Self::StaticNetRootDisk`] layout — the same net-only driver set,
    /// standard application store, and planted static-addressing
    /// `network.conf` — **plus** a planted
    /// `/System/Settings/Configuration/system.conf`
    /// ([`tairix_test_netstack_wire::TIMED_SYSTEM_CONF`]) on the *encrypted
    /// root*, naming the host peer as the one time server. The
    /// time-synchronisation vertical's backing (`plans/TIMESYNC.md` TS-2).
    ///
    /// The time store is planted on the root volume rather than the read-only
    /// `/System` one because `timed` reads it through the ordinary VFS, where
    /// `/System/Settings` resolves to the writable sub-mount the encrypted
    /// root backs — the same layer `os.loginType` is planted on. The console
    /// stays the UART text console the serial script drives (no display/input
    /// driver).
    TimeNetRootDisk,
    /// The net-only-driver encrypted-root layout carrying the **standard**
    /// signed application store **plus** a planted
    /// `/System/Settings/Network/network.conf`
    /// ([`tairix_test_netstack_wire::BOND_NETWORK_CONF`]) that binds two NICs
    /// by `match.mac` as the members of one active-backup bond carrying a
    /// static IPv6 address — the bond-failover vertical's backing
    /// (`plans/NETWORK.md` N9b-3-2-β-2-ii-b-bond). `devmgr` autoloads the NIC
    /// driver into a process per NIC and reads the planted config; `netstack`
    /// composes the bond over the two members and assigns it the static
    /// address — all pre-unlock, no console dialogue. The console stays the
    /// UART text console (no display/input driver).
    BondNetRootDisk,
    /// The net-only-driver encrypted-root layout carrying the **standard**
    /// signed application store **plus** a planted
    /// `/System/Settings/Network/network.conf`
    /// ([`tairix_test_netstack_wire::DHCP_NETWORK_CONF_AARCH64`]) that binds
    /// the NIC by `match.node`, selects `ipv4.method dhcp`, and disables IPv6
    /// — the DHCPv4 vertical's backing (`plans/DHCP.md` D3). `devmgr`
    /// autoloads the NIC driver into its own process and reads the planted
    /// config; `netstack` drives its DHCP client, which leases the interface
    /// its only address from the host DHCP-server peer — all pre-unlock, no
    /// console dialogue. The console stays the UART text console (no
    /// display/input driver).
    DhcpNetRootDisk,
    /// The net-only-driver encrypted-root layout carrying the **standard**
    /// signed application store **plus** a planted
    /// `/System/Settings/Network/network.conf`
    /// ([`tairix_test_netstack_wire::DHCP6_NETWORK_CONF_AARCH64`]) that binds
    /// the NIC by `match.node`, selects `ipv6.method dhcp`, and disables IPv4
    /// — the DHCPv6 vertical's backing (`plans/DHCP.md` D4c). `devmgr`
    /// autoloads the NIC driver into its own process and reads the planted
    /// config; `netstack` drives its DHCPv6 client, which leases the interface
    /// its only global address from the host DHCPv6-server peer — all
    /// pre-unlock, no console dialogue. The console stays the UART text
    /// console (no display/input driver).
    Dhcp6NetRootDisk,
}

/// `true` if `line` is exactly `value` followed by a single `\n`.
///
/// Used by the compile-time checks that keep the root-unlock-admission
/// vertical's serial script in lockstep with the shared
/// [`tairix_test_encrypted_root_image`] fixture: the serial table needs
/// `&'static str` literals, so each typed line is verified against the
/// fixture's own constant at build time (single source
/// of truth; drift fails the build rather than silently mistyping at the
/// prompt).
const fn is_line_of(line: &[u8], value: &[u8]) -> bool {
    if line.len() != value.len() + 1 {
        return false;
    }
    let mut i = 0;
    while i < value.len() {
        if line[i] != value[i] {
            return false;
        }
        i += 1;
    }
    line[value.len()] == b'\n'
}

/// The passphrase line the admission vertical types at `ARXFS passphrase: `.
const UNLOCK_PASSPHRASE_LINE: &str = "unlock-vertical correct horse battery staple\n";

/// The pre-boot-Supervisor ESC boot-screen serial script
/// (`plans/NEW-SUPERVISOR.md` §7), shared by every arch's Supervisor ESC
/// vertical so the byte-exact boot-screen contract has one definition, never a
/// per-arch copy. Each `(marker, delay, input)` step waits for the frozen
/// boot-screen `marker` on the console, then types `input`:
///
/// 1. `[Press ESC for supervisor]` (`root_mount::SUPERVISOR_ANNOUNCE`) → a
///    lone `ESC` (`0x1b`), dropping into the REPL (race-robust: if the 2 s
///    window elapses first, the same `ESC` is read as the passphrase line's
///    first byte and still drops in via `PassphraseReadError::Escape`).
/// 2. `Supervisor` (`root_mount::SUPERVISOR_ENTER_BANNER`, the collapse to
///    `ARXFS` then the `Supervisor` banner) → `help`, exercising a real
///    command at the `*` prompt.
/// 3. `commands:` (the dispatcher's host-independent `Supervisor commands:`
///    header that `help` renders) → `continue`, leaving the REPL.
/// 4. `ARXFS passphrase: ` (`root_mount::FS_UNLOCK_PROMPT`, redrawn *after*
///    the REPL exited) → the fixture passphrase, proving a Supervisor session
///    is transparent to boot.
///
/// PASS is keyed by the vertical's own audit sink on the unlock-service
/// install witness (`EventId(4139)`), which can only follow `continue`
/// resuming the normal unlock and that unlock mounting the encrypted `ARXFS`
/// root.
const SUPERVISOR_ESC_SCRIPT: &[(&str, Duration, &str)] = &[
    ("[Press ESC for supervisor]", Duration::ZERO, "\x1b"),
    ("Supervisor", Duration::ZERO, "help\n"),
    ("commands:", Duration::ZERO, "continue\n"),
    ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
];

/// The pre-boot-Supervisor serial script for the **live-passphrase-prompt**
/// ESC entry point (`plans/NEW-SUPERVISOR.md` §2 step 4), shared by every
/// arch that drives it. Unlike [`SUPERVISOR_ESC_SCRIPT`] — which enters at
/// the timed announcement window — this script waits for the redrawn
/// `ARXFS passphrase: ` prompt to appear *first* (which only happens after
/// the 2 s window has elapsed with no keypress), then types a lone `ESC` as
/// the first byte of the passphrase line. `read_passphrase_line` returns
/// `PassphraseReadError::Escape` for a first-byte lone `ESC`, dropping into
/// the same `enter_supervisor` REPL as the window path — the *other* entry
/// point the window-race-robust [`SUPERVISOR_ESC_SCRIPT`] only reaches
/// incidentally. Waiting for the prompt makes this path deterministic (never
/// the window race). It then drives the same `help` → `continue` →
/// passphrase round-trip, so PASS still keys on the unlock-service install
/// witness (`EventId(4139)`) — proving the passphrase-prompt drop is equally
/// transparent to boot. Steps:
///
/// 1. `ARXFS passphrase: ` (`root_mount::FS_UNLOCK_PROMPT`, the post-window
///    redraw) → a lone `ESC` (`0x1b`), dropping into the REPL via the
///    passphrase reader's `Escape` outcome.
/// 2. `Supervisor` (`root_mount::SUPERVISOR_ENTER_BANNER`) → `help`.
/// 3. `commands:` (the dispatcher's `Supervisor commands:` header) →
///    `continue`.
/// 4. `ARXFS passphrase: ` (the *next* occurrence — the normal unlock prompt
///    redrawn after the REPL exited) → the fixture passphrase.
const SUPERVISOR_ESC_AT_PROMPT_SCRIPT: &[(&str, Duration, &str)] = &[
    ("ARXFS passphrase: ", Duration::ZERO, "\x1b"),
    ("Supervisor", Duration::ZERO, "help\n"),
    ("commands:", Duration::ZERO, "continue\n"),
    ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
];

/// The pre-boot-Supervisor serial script for the in-REPL `mount` command
/// (`plans/NEW-SUPERVISOR.md` §2 step 6, §4.1), shared by every arch that
/// drives it. It enters the Supervisor at the announcement window (like
/// [`SUPERVISOR_ESC_SCRIPT`]), then instead of `continue` it runs `mount`:
/// the Supervisor performs the **real** root unlock *itself* under a typed
/// passphrase — a path distinct from `continue` resuming the normal unlock —
/// and on success returns `SupervisorExit::Mounted`, so boot proceeds with no
/// second prompt. PASS keys on the same install witness (`EventId(4139)`),
/// which the interactive unlock logs whenever it resolves to `Installed`
/// (including the `mount`-from-REPL path), so reaching it proves the in-REPL
/// mount mounted the encrypted `ARXFS` root. Steps:
///
/// 1. `[Press ESC for supervisor]` (`root_mount::SUPERVISOR_ANNOUNCE`) → a
///    lone `ESC` (race-robust exactly as [`SUPERVISOR_ESC_SCRIPT`]).
/// 2. `Supervisor` (`root_mount::SUPERVISOR_ENTER_BANNER`) → `mount`.
/// 3. `ARXFS passphrase: ` (the `mount` command's own prompt, `cmd_mount`) →
///    the fixture passphrase.
const SUPERVISOR_MOUNT_SCRIPT: &[(&str, Duration, &str)] = &[
    ("[Press ESC for supervisor]", Duration::ZERO, "\x1b"),
    ("Supervisor", Duration::ZERO, "mount\n"),
    ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
];

/// Serial marker after which the autoload-input vertical begins typing the
/// unlock passphrase at the virtio keyboard.
///
/// The autoloaded user-space virtio-input keyboard driver is *interrupt
/// driven*: `VirtioInput::open_armed` brings the device to `DRIVER_OK`,
/// posts its event-queue buffers, and only then runs its *arm* step — the
/// driver binding its granted device interrupt line through the `irq_bind`
/// syscall — before the pump parks on `irq_wait`
/// (`lib/drvrt::RtDriverHost::notify_wait`). A virtio-input device silently
/// drops events while its eventq has no posted buffers, so arming any
/// earlier would advertise readiness inside the drop window — the lost
/// keypress that made this vertical flaky. `irq_bind` is an audited syscall
/// (`lib/abi` `SyscallSpec { audit: true }`), and **only a user-space driver
/// issues the `irq_bind` *syscall*** — the in-kernel block path binds its
/// completion line through `IrqTable::bind` directly — so this dispatch
/// record appears exactly once **per autoloaded input-driver instance**, the
/// instant that instance is armed and waiting. The vertical attaches a
/// pointer sibling beside the keyboard (two virtio-input nodes, one driver
/// instance each), so the runner waits for the marker's **second**
/// occurrence before injecting — both instances armed, the keyboard's
/// included. Injecting then guarantees the device is active with posted
/// buffers, so the keypress is delivered (virtio-input interrupts are
/// level-triggered, so the assertion is held until the kernel routes+enables
/// the line on the driver's first park) rather than dropped against an
/// un-ready device. It is the user-space analogue of the in-kernel
/// `input_virtio_mmio` vertical's "eventq armed" readiness marker
/// (inject only once the driver can receive). The typed characters buffer
/// as console type-ahead in the seat's keyboard queue until the in-kernel
/// unlock kthread's `ARXFS passphrase:` prompt drains them — the
/// prompt itself renders on the video console, never on serial, so the
/// typing is gated on the armed-driver witness rather than the prompt text.
const AUTOLOAD_INPUT_KEY_MARKER: &str = "sc=irq_bind";

/// How many times [`AUTOLOAD_INPUT_KEY_MARKER`] must appear before typing:
/// once per autoloaded input-driver instance (keyboard + mouse), so the
/// keyboard's own arming — possibly the second — is never raced.
const AUTOLOAD_INPUT_ARMED_OCCURRENCES: u32 = 2;

/// [`AUTOLOAD_INPUT_ARMED_OCCURRENCES`] for a vertical that drives the
/// keyboard alone: the board carries a pointer device only when the test
/// scripts one, so a keyboard-only run raises the marker exactly once and
/// waiting for a second would hang until the runtime ceiling. The count is
/// the device set's, not a weaker gate — the keyboard is still armed and
/// parked on its interrupt before a key is injected.
const KEYBOARD_ONLY_ARMED_OCCURRENCES: u32 = 1;

/// Guest marker keying the second screendump (the served files window on
/// the dark desktop): the activating click's `Focus` + `Pressed` both
/// reached that window's own event port. The guest attributes the pair to
/// the window itself, so no other app or service can key the dump.
const AUTOLOAD_FILES_ACTIVATED_MARKER: &str =
    tairix_test_autoload_input_qemu_aarch64::FILES_WINDOW_ACTIVATED_MARKER;

/// The label the autostarted file manager's icon-bar slot carries, so the
/// reconstructed bar the script clicks against matches the guest's.
const FILES_BAR_APP_NAME: &str = tairix_test_autoload_input_qemu_aarch64::FILES_BAR_APP_NAME;

/// Guest marker gating the terminal stage's library-popup clicks: the
/// handshake click's `Pressed` reached the still-focused files window — a
/// wake boundary strictly past the verified second dump.
const AUTOLOAD_FILES_HANDSHAKE_MARKER: &str =
    tairix_test_autoload_input_qemu_aarch64::FILES_HANDSHAKE_MARKER;

/// Serial marker of one shared-frame *map* operation: the kernel
/// syscall-trace record for `shm_map`. A window's frame region is mapped
/// exactly once — when the window is **created** — and a *present* re-uses
/// that mapping, so counting these tracks window creation and never the
/// (timing-variable) number of repaints. Gating the terminal-window click
/// on it is therefore immune to the flaky-repaint race a `CallReplied`
/// (present-inclusive) count suffered. Rendered by the same `sc=<name>`
/// syscall trace the input-arming gate ([`AUTOLOAD_INPUT_KEY_MARKER`])
/// already keys on, so both gates share one serial convention.
const AUTOLOAD_WINDOW_MAP_MARKER: &str = "sc=shm_map";

/// How many [`AUTOLOAD_WINDOW_MAP_MARKER`] occurrences gate the *files*
/// window click (`plans/APPWIN.md` AW3): the boot framebuffer scan-out map,
/// then that window's own create map. The shared contract, so the click can
/// never race the window's existence — which a gate on any reply over the
/// shared window rendezvous did, firing on the Switchboard's start-up
/// desktop query while the desktop was still bare.
const AUTOLOAD_FILES_WINDOW_MAP_OCCURRENCES: u32 =
    tairix_test_autoload_input_qemu_aarch64::FILES_WINDOW_FRAME_MAPS;

/// How many [`AUTOLOAD_WINDOW_MAP_MARKER`] occurrences gate the
/// terminal-window click (`plans/APPWIN.md` AW4): the two above, then the
/// terminal window's create map — after which the terminal window exists at
/// its cascade slot and the click focuses it, no matter how many times any
/// window repainted.
const AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES: u32 =
    tairix_test_autoload_input_qemu_aarch64::TERMINAL_WINDOW_FRAME_MAPS;

/// The shell command the autoload vertical types into the focused
/// terminal at the seat keyboard — the shared contract (`sleep 3600`
/// plus Enter): the shell resolving and spawning it is the guest's AW4
/// round-trip witness, and the blocking foreground job the Ctrl-C step
/// then interrupts.
const AUTOLOAD_TERMINAL_COMMAND: &str = tairix_test_autoload_input_qemu_aarch64::TERMINAL_COMMAND;

/// Serial marker after which the vertical injects the pty Ctrl-C recovery
/// step: the test kernel prints it once it has witnessed the foreground
/// `sleep` spawn (`plans/PTY.md`), so the `Ctrl-C` lands against a live,
/// parked foreground job — never before one exists.
const AUTOLOAD_CTRL_C_ARM_MARKER: &str = tairix_test_autoload_input_qemu_aarch64::CTRL_C_ARM_MARKER;

/// Guest marker gating the terminal command typing: the terminal window
/// first becomes the focused key recipient (first app-ward delivery to the
/// second distinct window port). Gating on this, not a delivery count the
/// files window satisfies before the terminal exists, keeps the typed
/// command from racing ahead of the terminal-focus click.
const AUTOLOAD_TERMINAL_FOCUSED_MARKER: &str =
    tairix_test_autoload_input_qemu_aarch64::TERMINAL_FOCUSED_MARKER;

/// The pty Ctrl-C recovery keys the vertical types once the `sleep` spawn
/// has armed the step (the shared contract): a `Ctrl-C` (the `\u{3}` ETX
/// byte the runner sends as the QEMU `ctrl-c` chord, which the terminal
/// encodes as the `0x03` interrupt byte) then `true` + Enter. The shell,
/// unblocked from its `wait` on the interrupted `sleep`, spawns `true` —
/// the guest's pty job-control witness.
const AUTOLOAD_TERMINAL_CTRL_C_RECOVERY: &str =
    tairix_test_autoload_input_qemu_aarch64::TERMINAL_CTRL_C_RECOVERY;

/// Serial marker after which the autoload vertical types the login +
/// desktop-command dialogue: the serial rendering of the kernel's
/// `UsersDbLoaded` audit witness (`EventId` 4040), emitted the moment the
/// typed passphrase unlocked the encrypted root and the users database
/// installed. `login` prompts on the video console only after that, so
/// the dialogue buffers as console type-ahead until its reads drain it —
/// exactly the passphrase step's discipline. (The literal matches the
/// kernel `AuditEvent::UsersDbLoaded` message; drift makes the vertical
/// time out loudly, never pass on the wrong exchange.)
const AUTOLOAD_LOGIN_MARKER: &str = "users database loaded";

/// The login + desktop-command dialogue the autoload vertical types at
/// the seat keyboard: the fixture account's username and password
/// (`root`/`root`), then the `desktop` command word at the text shell's
/// prompt — the disk plants `os.loginType text`, so the authenticated
/// session is the shell and the desktop is started exactly as a user
/// starts it, by typing `desktop` (the system app store's `desktop.app`,
/// the bundle a graphical login also spawns).
/// Pinned against the fixture credentials below so the dialogue and the
/// planted account cannot drift; a renamed bundle makes the vertical
/// time out loudly at the [`AUTOLOAD_DESKTOP_REVEALED_MARKER`] gate, never
/// pass on the wrong exchange.
const AUTOLOAD_LOGIN_DIALOGUE: &str = "root\nroot\ndesktop\n";

/// Serial marker after which the desktop verticals take their screendump
/// **and** inject the mouse motion: the session's one-shot
/// `DESKTOP_REVEALED` log record — the witness that a composited frame at
/// full reveal strength reached the display. Imported from the session
/// crate's own definition, so the emitter and this consumer can never
/// drift.
///
/// The display service's first-present record is deliberately *not* the
/// gate: a session starts on the black the login screen left behind and
/// reveals itself over the theme's fade, so its first presented frame is
/// black by design and indistinguishable from a blank screen. A dump taken
/// there could no longer tell a composited desktop from no desktop at all.
///
/// Keying both the dump and the pointer on it makes the chain strictly
/// ordered: visible desktop → verified dump → mouse motion → the guest's
/// `kind=pointer` witness → PASS — a run can neither pass without showing
/// the desktop nor exit before the host holds the pixels. It also puts
/// every later stage after the fade *and* after the wallpaper has both been
/// prepared and finished dissolving in — the session holds the witness back
/// while the read and decode are in flight on a worker thread, and again
/// while the picture crossfades over the backdrop colour — so the dumps that
/// sample wallpaper measure the desktop rather than the fade, a frame
/// part-way through the dissolve, or the fallback colour.
const AUTOLOAD_DESKTOP_REVEALED_MARKER: &str = tairix_desktop_session::DESKTOP_REVEALED_MESSAGE;

/// Serial marker after which the icon-bar vertical reads the screen and
/// injects its next gesture: the session's per-window `WINDOW_SHOWN` log
/// record — the witness that a frame carrying that served window's first
/// painted pixels reached the display. Imported from the session crate's own
/// definition, so the emitter and this consumer can never drift.
///
/// It is the only honest gate for both. A create reply says the window
/// *exists*, not that anything has been drawn into it — its body is still the
/// session's own opening fill — so a dump taken there races the application's
/// first paint, and a click sent there races the session's own re-resolution
/// of the bar. Nothing on the window channel distinguishes a present from any
/// other request either: a present, a backdrop-blur change, a retitle and an
/// icon-bar declaration all answer with the same four-byte status reply. Only
/// the session can say a window is visible, so it does.
///
/// The vertical counts occurrences of it, which is attributable here because
/// the launched application is the only client that opens a window: the
/// desktop's own surfaces are session-painted compositor windows and never
/// call the window channel.
const APPBAR_WINDOW_SHOWN_MARKER: &str = tairix_desktop_session::WINDOW_SHOWN_MESSAGE;

/// Serial marker the elevated Date & Time vertical waits for before typing
/// credentials into the session's credential prompt — the prompt's own
/// announcement that it is on screen and holding the keyboard.
const ELEVATE_PROMPT_SHOWN_MARKER: &str = tairix_desktop_session::ELEVATE_PROMPT_SHOWN_MESSAGE;

/// `true` if `text` begins with `prefix`.
const fn starts_with_bytes(text: &[u8], prefix: &[u8]) -> bool {
    if text.len() < prefix.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if text[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// The username line the session-ceiling vertical types at the login
/// view's `Username:` field.
const SESSION_USERNAME_LINE: &str = "root\n";

/// The over-long username line the spawn-session verticals type at the login
/// view's `Username:` field: exactly the account format's `MAX_USERNAME_LEN`
/// bound plus one character, and nothing more. Login draws the login box in
/// the console's raw discipline (`round_begin` selects it once per process,
/// before any field is read), so the view sees every keystroke and refuses
/// the field the instant one character beyond the bound arrives
/// (`LengthOutOfRange`) — it never waits for a terminating newline. Login
/// then records the console error and exits fail-closed, and `init` reaps and
/// relaunches it (the reap→relaunch witness the vertical proves).
///
/// The payload deliberately carries **no** trailing byte past the one that
/// trips the refusal. That last `'x'` is both the byte that triggers the
/// exit *and* the final byte of the serial step, so the harness records the
/// step as fully sent (`serial_step` reaches its end) in the same instant it
/// writes the byte login needs to fail — strictly before login can consume
/// it, refuse, and exit. A trailing newline (or any extra byte) would be a
/// byte login never reads before exiting: the harness would still be dribbling
/// it out one byte per tick when the guest's own PASS finisher fires on the
/// relaunch, leaving the final step "incomplete" and failing the run
/// non-deterministically. Keeping the last byte the refusal trigger removes
/// that race by construction. The byte array is generated from the shared
/// bound and its fixed ASCII payload is valid UTF-8 by construction.
const OVERLONG_USERNAME_BYTES: [u8; tairix_users::MAX_USERNAME_LEN + 1] =
    [b'x'; tairix_users::MAX_USERNAME_LEN + 1];

/// String view of [`OVERLONG_USERNAME_BYTES`] for the serial dialogue table.
const OVERLONG_USERNAME: &str = match core::str::from_utf8(&OVERLONG_USERNAME_BYTES) {
    Ok(username) => username,
    Err(_) => "",
};

/// The password line the session-ceiling vertical types at the login
/// view's `Password` field.
const SESSION_PASSWORD_LINE: &str = "root\n";

/// Serial marker for a value-pipe read of `info:system/machine-id`: the
/// unprovisioned machine id, sixteen zero bytes until an installer mints one,
/// in the resolver's lowercase hex. Distinctive enough to be a marker and
/// exact enough to prove the whole read arrived; pinned to the rendering's
/// width by a unit test below.
const UNPROVISIONED_MACHINE_ID_MARKER: &str = "00000000000000000000000000000000";

/// The shell line reading the reference from the original defect report. Its
/// value is the machine's RAM size, which this script cannot predict, so the
/// assertion is on `cat`'s exit status: `&&` runs the `echo` only if the read
/// succeeded, standing in for a number that differs per machine.
const VALUE_PIPE_PHYSICAL_LINE: &str = "cat < info:mem/physical && echo VALUE-PIPE-PHYSICAL-OK\n";

/// Serial marker [`VALUE_PIPE_PHYSICAL_LINE`]'s `echo` produces on success.
const VALUE_PIPE_PHYSICAL_MARKER: &str = "VALUE-PIPE-PHYSICAL-OK";

/// The shell line reading the same reference as a bare **operand**, which the
/// tool resolves itself rather than the shell. Gated on `cat`'s exit status
/// for the same reason as [`VALUE_PIPE_PHYSICAL_LINE`].
const VALUE_OPERAND_PHYSICAL_LINE: &str =
    "cat info:mem/physical && echo VALUE-OPERAND-PHYSICAL-OK\n";

/// Serial marker [`VALUE_OPERAND_PHYSICAL_LINE`]'s `echo` produces on success.
const VALUE_OPERAND_PHYSICAL_MARKER: &str = "VALUE-OPERAND-PHYSICAL-OK";

/// Serial marker for a *write* of a value-backed reference: the errno the
/// kernel resource resolver refuses one with, in the shell's launch-failure
/// line. Pinned to the errno's own `Display` text by a unit test below.
const VALUE_PIPE_WRITE_REFUSED_MARKER: &str = "not supported by the backing";

/// Serial marker the memory-stability vertical waits for before typing the
/// shell `exit` that completes its PASS chain: the leading prefix of the
/// memsoak fixture's success report line. Pinned to the fixture's own
/// `tairix_test_memsoak::PASS_MARKER` by a unit test below, so the script
/// and the program cannot drift.
const MEMSOAK_PASS_PREFIX: &str = "MEMSOAK PASS baseline=";

/// Serial marker the stream-socket vertical waits for before typing the
/// shell `exit` that completes its PASS chain: the `tcpecho` client's success
/// report marker. Pinned to the fixture's own `tairix_test_tcpecho::PASS_MARKER`
/// by a unit test below, so the script and the program cannot drift.
const TCPECHO_PASS_PREFIX: &str = "TCPECHO PASS";

/// Serial marker the TCP-listener vertical waits for before typing the shell
/// `exit` that completes its PASS chain: the `tcpserve` server's success
/// report marker. Pinned to the fixture's own `tairix_test_tcpserve::PASS_MARKER`
/// by a unit test below, so the script and the program cannot drift.
const TCPSERVE_PASS_PREFIX: &str = "TCPSERVE PASS";

/// Serial marker gating the connection-exhaustion vertical: the `netstack`
/// `SYN_COOKIES_ENGAGED` audit message. Its appearance means the listener's
/// bounded half-open backlog overflowed and the stack fell back to stateless
/// RFC 4987 cookies — the one witness that distinguishes a cookie-admitted
/// connection from an ordinary one, so the vertical requires it *before* it
/// will await the fixture's PASS marker. It is the literal `netstack` audit
/// message (`userland/net/netstack/src/run.rs`), matching the established
/// pattern of gating on an audit message substring. It is the text of
/// `tairix_netstack::events::SYN_COOKIES_ENGAGED_MESSAGE`, named at the
/// emitter so the wording is deliberate on both sides; xtask keeps a literal
/// rather than depending on a userland service crate for one string, as the
/// sibling [`BOND_FAILOVER_TRIGGER_MARKER`] already does.
const SYN_COOKIES_MARKER: &str = "netstack: SYN backlog full, answering with stateless cookies";

/// The shell command line the `ping` vertical types at the prompt: three
/// `ICMPv6` echo requests to the host peer's link-local address (`fe80::2`,
/// formed from `tairix_test_netstack_wire::PEER_IID`). The address literal is
/// pinned to the shared wire constant by a unit test below, so the typed
/// target and the peer's own address cannot drift.
const PING_COMMAND_LINE: &str = "ping -c 3 fe80::2\n";

/// Serial marker the `ping` vertical waits for before typing the shell `exit`
/// that completes its PASS chain: the `icmp_seq=` field of a reply line, which
/// the `ping` tool prints **only** on a genuinely received echo reply. An
/// unanswered run never emits it, so the run times out fail-loud rather than
/// falsely passing.
const PING_REPLY_MARKER: &str = "icmp_seq=";

/// The shell command line the `telnet` vertical types at the prompt: a session
/// to the host peer's link-local address (`fe80::2`, formed from
/// `tairix_test_netstack_wire::PEER_IID`) with **no port operand**, so the run
/// exercises the tool's own default port. The address literal is pinned to the
/// shared wire constant by a unit test below, so the typed target and the
/// peer's own address cannot drift.
const TELNET_COMMAND_LINE: &str = "telnet fe80::2\n";

/// The line the `telnet` vertical types into the live session, once the peer's
/// banner has proven the option exchange completed.
const TELNET_PROBE_LINE: &str = "probe-telnet-1184\n";

/// The keystrokes that leave the session: the default escape character `^]`
/// (0x1D), which drops into the `telnet>` interpreter, then its `quit`
/// command. Typing them exercises the escape recognition and the command
/// interpreter on the live wire, not just in the host tests.
const TELNET_QUIT_SEQUENCE: &str = "\u{1d}quit\n";

/// `true` if the two byte strings are equal — the compile-time complement of
/// [`is_line_of`] for asserting a typed line does **not** match the fixture.
const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = {
    assert!(
        is_line_of(
            UNLOCK_PASSPHRASE_LINE.as_bytes(),
            tairix_test_encrypted_root_image::PASSPHRASE
        ),
        "UNLOCK_PASSPHRASE_LINE drifted from the fixture passphrase"
    );
    // The autoload login dialogue is `<username>\n<password>\n` from the
    // shared fixture account, then the `desktop` command typed at the
    // shell prompt the text login drops to.
    assert!(
        starts_with_bytes(
            AUTOLOAD_LOGIN_DIALOGUE.as_bytes(),
            SESSION_USERNAME_LINE.as_bytes()
        ),
        "AUTOLOAD_LOGIN_DIALOGUE must start with the fixture username line"
    );
    assert!(
        starts_with_bytes(
            AUTOLOAD_LOGIN_DIALOGUE
                .as_bytes()
                .split_at(SESSION_USERNAME_LINE.len())
                .1,
            SESSION_PASSWORD_LINE.as_bytes()
        ),
        "AUTOLOAD_LOGIN_DIALOGUE must continue with the fixture password line"
    );
    assert!(
        bytes_eq(
            AUTOLOAD_LOGIN_DIALOGUE
                .as_bytes()
                .split_at(SESSION_USERNAME_LINE.len() + SESSION_PASSWORD_LINE.len())
                .1,
            b"desktop\n"
        ),
        "AUTOLOAD_LOGIN_DIALOGUE must end with the `desktop` shell command"
    );
    assert!(
        is_line_of(
            SESSION_USERNAME_LINE.as_bytes(),
            tairix_test_encrypted_root_image::USERNAME.as_bytes()
        ),
        "SESSION_USERNAME_LINE drifted from the fixture account"
    );
    assert!(
        is_line_of(
            SESSION_PASSWORD_LINE.as_bytes(),
            tairix_test_encrypted_root_image::PASSWORD.as_bytes()
        ),
        "SESSION_PASSWORD_LINE drifted from the fixture account"
    );
};

// A `static` (not a `const`): several enrolments may share one built binary
// and `sidecar_path` disambiguates their planted images by each entry's
// stable index in this table, found via `std::ptr::eq`. A `const` is inlined
// at every use and its promoted array need not have a single address, so
// pointer identity against a re-materialised `TESTS.iter()` is unreliable; a
// `static` has one address, so the index lookup is sound.
static TESTS: &[QemuTest] = &[
    QemuTest {
        package: "tairix-test-memory-isolation",
        binary: "tairix-test-memory-isolation",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // ramzip b3 (`plans/SWAPSWAPSWAP.md`, `plans/SWAPSWAPSWAP.md`):
    // the x86_64 hardware referenced (Accessed) bit read and cleared
    // through the Arch HAL, driving the cold-page clock scan
    // (`kernel/mem::coldscan`). Single CPU suffices (the test builds and
    // probes one address space on the BSP); the 60-second budget matches
    // `memory_isolation`'s — a strictly bring-up test with no workload.
    QemuTest {
        package: "tairix-test-accessed-bit-qemu-x86_64",
        binary: "tairix-test-accessed-bit-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // ramzip b3 aarch64 (`plans/SWAPSWAPSWAP.md`,
    // `plans/SWAPSWAPSWAP.md`): the software-managed Access Flag
    // (the cold-page referenced bit) read and cleared through the Arch
    // HAL, and an access to a cleared-AF leaf resolved by the
    // synchronous-exception Access-Flag-fault path (setting AF + retry),
    // driving the cold-page clock scan (`kernel/mem::coldscan`). The QEMU
    // CPU is `cortex-a72` (no HAFDBS), so the software AF-fault path is
    // genuinely exercised. Single CPU suffices (the test builds and probes
    // one address space on the BSP); the 60-second budget matches
    // `memory_isolation`'s — a strictly bring-up test with no workload.
    QemuTest {
        package: "tairix-test-accessed-bit-qemu-aarch64",
        binary: "tairix-test-accessed-bit-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // ramzip b3 riscv64 (`plans/SWAPSWAPSWAP.md`,
    // `plans/SWAPSWAPSWAP.md`): the software-managed Accessed bit
    // (the cold-page referenced bit) read and cleared through the Arch
    // HAL, and an access to a cleared-A leaf resolved by the trap path's
    // A/D-setting page-fault handler (setting A + retry), driving the
    // cold-page clock scan (`kernel/mem::coldscan`). The riscv64 runner
    // pins the CPU to `svade=true,svadu=false`, so the software A/D fault
    // path is genuinely exercised. Single CPU suffices (the test builds
    // and probes one address space on the boot hart); the 60-second budget
    // matches `memory_isolation`'s — a strictly bring-up test with no
    // workload.
    QemuTest {
        package: "tairix-test-accessed-bit-qemu-riscv64",
        binary: "tairix-test-accessed-bit-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 3a (b) deliverable: AP bring-up + scheduler stress on real
    // (emulated) cores. The host-side `tairix-test-scheduler-stress`
    // workspace test continues to satisfy the unit / cross-
    // crate contract; this enrolment is the QEMU-on-real-cores half of
    // the same Stage-2 deliverable mandated by `PLAN.md` lines 154-158.
    QemuTest {
        package: "tairix-test-scheduler-stress-qemu",
        binary: "tairix-test-scheduler-stress-qemu",
        target: "x86_64-unknown-none",
        cpus: 4,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 3a (c7-bin) deliverable: boot the production
    // `tairix-kernel` boot pipeline (Multiboot2 → ACPI/MADT →
    // `X86_64Arch` → per-CPU init → `BootInfo` →
    // `kernel_core::kernel_main`) and assert
    // `AuditEvent::BootCompleted` (`EventId(4004)`) appears on the
    // audit sink. The test binary `tairix-test-kernel-arch-boot`
    // wraps the lib half of `tairix-kernel` with an audit-observer
    // Sink that flips `qemu_exit::exit_success` on observing
    // `BootCompleted` — see
    // `tests/integration/kernel_arch_boot/src/main.rs`. Single CPU
    // suffices: the (c7-bin) scope only brings up the BSP. The
    // 60-second budget matches `memory_isolation`'s — both are
    // strictly bring-up tests with no workload.
    QemuTest {
        package: "tairix-test-kernel-arch-boot",
        binary: "tairix-test-kernel-arch-boot",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/OPEN-DEFECTS.md` D55 deliverable: boot the production
    // `tairix-kernel` pipeline on a guest whose RAM tops the boot
    // trampoline's own identity window, and prove the kernel can reach all
    // of it by pointer. The observer grades two records: the boot path
    // widened the direct map past the trampoline's window (so it sized it
    // from the discovered memory map, not a build-time constant), and the
    // early-boot RAM self-test wrote and read back *every* usable byte
    // through that map — the frames above the old window included.
    //
    // 3584 MiB is the smallest `-m` for which QEMU's `pc` machine places any
    // RAM above 4 GiB (below that it fits the whole guest under the hole),
    // and RAM above 4 GiB is the entire point of the enrolment. The self-test
    // samples a word per page, so the host commits the guest's RAM for the
    // run; nothing here is headroom. Single CPU suffices — the widening is a
    // BSP-only boot step — and the 120-second silence budget is the
    // boot-then-fixed-work budget plus the sweep over that RAM, which prints
    // progress throughout and so keeps resetting the heartbeat.
    QemuTest {
        package: "tairix-test-physmap-qemu-x86_64",
        binary: "tairix-test-physmap-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: Some(3584),
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 2.7 follow-up (f6) deliverable: boot the production
    // `tairix-kernel` boot pipeline and, on observing
    // `AuditEvent::BootCompleted`, synthesise a Scheduler / CapTable /
    // KernelSyscallHandlers / Dispatcher quartet locally and drive
    // `Dispatcher::dispatch` with `(cap_query, CAP_TIME_SET)` then
    // `(exit, 0)`. The synthesised inner audit sink counts the
    // `SyscallInvoked` (`EventId(5000)`) record emitted by the
    // `exit` dispatch (the `cap_query` half is `audit: false` per
    // the abi-v1 table — observed via the dispatcher's return value
    // instead). The test bin flips `qemu_exit::exit_success` only
    // when both halves complete cleanly; anything else trips
    // `qemu_exit::exit_failure`. Single CPU suffices and the
    // 60-second budget matches `kernel_arch_boot`'s — same boot
    // pipeline plus a fixed-size dispatcher exercise.
    QemuTest {
        package: "tairix-test-syscall-dispatch-qemu",
        binary: "tairix-test-syscall-dispatch-qemu",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC2 deliverable (`plans/CCOMPAT.md`): the per-native-
    // target QEMU round-trip for the C-callable syscall stub runtime
    // (`lib/abi-sys`). Unlike `tairix-test-syscall-dispatch-qemu` (which
    // drives `Dispatcher::dispatch` directly and never executes a trap),
    // this test boots the production kernel pipeline and, on
    // `AuditEvent::BootCompleted`, overrides the syscall dispatch callback
    // and then *issues* the `abi-sys` `tairix_sys_cap_query` stub — exercising
    // the real x86_64 `syscall` instruction (`lib/abi-sys/src/trap.rs`) and
    // the kernel's `IA32_LSTAR` entry stub
    // (`kernel/arch/x86_64/src/syscall_entry.rs`) together. The installed
    // callback asserts the kernel-observed `(number, args)` are exactly
    // what `tairix_sys_cap_query` should have marshalled into the syscall
    // registers and flips `qemu_exit::exit_success`; any mismatch (or the
    // `syscall` returning to its caller at all) flips
    // `qemu_exit::exit_failure`. Single CPU suffices and the 60-second
    // budget matches the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-abi-sys-syscall-qemu",
        binary: "tairix-test-abi-sys-syscall-qemu",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC2 deliverable (`plans/CCOMPAT.md`): the riscv64
    // half of the `lib/abi-sys` syscall-stub round-trip. riscv64 has no
    // x86_64-style "trap identically from any privilege" shortcut — the
    // kernel routes only an `ecall` *from U-mode* to the syscall dispatch
    // callback (`kernel/arch/riscv64/src/syscall_entry.rs`) — so this test
    // stands up a minimal U-mode context with the Stage-3 Sv39 primitives:
    // it identity-maps the kernel (S-mode), aliases the `tairix_sys_cap_query`
    // stub page at a user virtual address with the U bit set plus a user
    // stack, installs the dispatch callback, sets `sstatus.SUM`, and
    // `sret`s to U-mode. The stub's real `ecall` (`lib/abi-sys/src/trap.rs`)
    // then traps into the kernel S-mode trap vector, and the installed
    // callback asserts the kernel-observed `(number, args)` are exactly
    // what `tairix_sys_cap_query` should have marshalled into `a7`/`a0` before
    // writing the `SiFive` Test PASS finisher; any mismatch (or the `ecall`
    // resuming in U-mode at all) writes a distinct failure finisher. Single
    // CPU suffices and the 60-second budget matches the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-abi-sys-syscall-qemu-riscv64",
        binary: "tairix-test-abi-sys-syscall-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC2 deliverable (`plans/CCOMPAT.md`): the aarch64
    // half of the `lib/abi-sys` syscall-stub round-trip. Like riscv64,
    // aarch64 has no x86_64-style "trap identically from any privilege"
    // shortcut — the kernel routes only an `svc` *from EL0* (a lower-EL
    // synchronous exception) to the syscall dispatch callback
    // (`kernel/arch/aarch64/src/exceptions.rs`) — so this test stands up a
    // minimal EL0 context with the Stage-3 stage-1 primitives: it
    // identity-maps the kernel (EL1), aliases the `tairix_sys_cap_query` stub
    // page at a user virtual address with EL0-executable attributes plus
    // an EL0 stack, installs the dispatch callback and the EL1 vector
    // table, and `eret`s to EL0. The stub's real `svc`
    // (`lib/abi-sys/src/trap.rs`) then traps into the EL1 vector, and the
    // installed callback asserts the kernel-observed `(number, args)` are
    // exactly what `tairix_sys_cap_query` should have marshalled into
    // `x8`/`x0` before the ARM semihosting PASS finisher; any mismatch (or
    // the `svc` resuming in EL0 at all) writes a distinct failure
    // finisher. Single CPU suffices and the 60-second budget matches the
    // other boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-abi-sys-syscall-qemu-aarch64",
        binary: "tairix-test-abi-sys-syscall-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC3 deliverable (`plans/CCOMPAT.md`): the x86_64
    // ring-3 exercise for the Arch HAL "enter user mode" primitive
    // (`kernel/arch/x86_64/src/userentry.rs`, `tairix_arch_api::EnterUser`). Unlike `tairix-test-abi-sys-syscall-qemu`, which
    // issues the same `abi-sys` stub from ring 0 (the x86_64 `syscall`
    // traps identically from any privilege level and never crosses a
    // boundary), this test boots the production kernel and, on
    // `AuditEvent::BootCompleted`, builds a ring-3 address space — a
    // user-accessible, executable, non-writable alias of the
    // `tairix_sys_cap_query` stub page (W^X) plus a USER read/write
    // stack — switches CR3, and `iretq`s to ring 3 through
    // `UserMode::new().enter_user(...)`. The stub's real `syscall`
    // (`lib/abi-sys/src/trap.rs`) then traps back through the kernel's
    // `IA32_LSTAR` entry stub; reaching the installed dispatch callback
    // at all proves the `iretq` entry succeeded, and the callback asserts
    // the kernel-observed `(number, args)` are exactly what
    // `tairix_sys_cap_query` should have marshalled into the syscall
    // registers before flipping `qemu_exit::exit_success`; any mismatch
    // flips `qemu_exit::exit_failure`. Single CPU suffices and the
    // 60-second budget matches the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-enter-user-qemu-x86_64",
        binary: "tairix-test-enter-user-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // x86_64 `syscall` register-preservation regression vertical
    // (`kernel/arch/x86_64/src/syscall_entry.rs`): the IA32_LSTAR entry
    // stub once tore its on-stack argument array down with a bare stack
    // drop instead of popping the values back into rdi/rsi/rdx/r10/r8/r9,
    // so after `sysretq` those registers held kernel dispatch residue —
    // miscompiling every syscall wrapper (the user-side trap stub declares
    // only rax/rcx/r11 clobbered) and leaking kernel register contents to
    // ring 3. This test enters ring 3 like the enter-user vertical, loads
    // sentinels into the six argument registers and the six callee-saved
    // registers, issues a real `syscall` whose callback returns a sentinel
    // rax, verifies every register survived the round-trip, and reports
    // the verdict through a second `syscall` (exit_success iff clean).
    // Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "tairix-test-syscall-regs-qemu-x86_64",
        binary: "tairix-test-syscall-regs-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC3 deliverable (`plans/CCOMPAT.md`): the riscv64
    // crt0-linked-program spawn round-trip. The build script compiles the
    // separate fixture program (`tests/integration/cc3_program`, crt0 +
    // abi-sys) position-independent and converts it to an `rxe` blob
    // (`tairix_itest_harness::elf2rxe`) carrying the kernel's syscall CFI tag.
    // On boot the test stands up an Sv39 address space (identity-mapping the
    // kernel + MMIO), activates it, installs the trap vector and a dispatch
    // callback, then calls the production capability-checked, audited spawn
    // caller (`tairix_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`)
    // to build the program's U-mode image — segments mapped + filled, user
    // stack, startup-vector block — and `sret`s into it through the Arch HAL
    // `EnterUser` primitive. The program (built via `build_process_image` at a
    // high `USER_BIAS`) parses `argv[1]`, returns it, and crt0 routes the
    // return through the `exit` syscall, whose `ecall` traps back through the
    // kernel S-mode vector to the dispatch callback, which asserts the code
    // equals the spawned decimal argument before the `SiFive` Test PASS
    // finisher; any mismatch (or a returning spawn) writes a distinct failure
    // finisher. Single CPU suffices and the 60-second budget matches the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-spawn-program-qemu-riscv64",
        binary: "tairix-test-spawn-program-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC3 deliverable (`plans/CCOMPAT.md`): the aarch64
    // crt0-linked-program spawn round-trip — the EL0 analogue of the riscv64
    // test above. The build script compiles the separate fixture program
    // (`tests/integration/cc3_program`, crt0 + abi-sys) position-independent
    // and converts it to an `rxe` blob (`tairix_itest_harness::elf2rxe`)
    // carrying the kernel's syscall CFI tag. On boot the test stands up a
    // stage-1 address space (identity-mapping the kernel + MMIO, EL1),
    // activates it, installs the EL1 vector table and a dispatch callback, then
    // calls the production capability-checked, audited spawn caller
    // (`tairix_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
    // build the program's EL0 image — segments mapped + filled, user stack,
    // startup-vector block — and `eret`s into it through the Arch HAL
    // `EnterUser` primitive. The program (built via `build_process_image` at a
    // high `USER_BIAS`) parses `argv[1]`, returns it, and crt0 routes the
    // return through the `exit` syscall, whose `svc` traps back through the
    // kernel EL1 vector to the dispatch callback, which asserts the code equals
    // the spawned decimal argument before the ARM semihosting PASS finisher;
    // any mismatch (or a returning spawn) writes a distinct failure finisher.
    // Single CPU suffices and the 60-second budget matches the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-spawn-program-qemu-aarch64",
        binary: "tairix-test-spawn-program-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC3 deliverable (`plans/CCOMPAT.md`): the x86_64
    // crt0-linked-program spawn round-trip — the ring-3 analogue of the
    // riscv64/aarch64 tests above, completing CC3. The build script compiles
    // the separate fixture program (`tests/integration/cc3_program`, crt0 +
    // abi-sys) position-independent and converts it to an `rxe` blob
    // (`tairix_itest_harness::elf2rxe`) carrying the kernel's syscall CFI tag.
    // Because the x86_64 ring-3 transition needs the GDT user selectors, the
    // TSS, and `syscall`/`IA32_LSTAR` entry installed, the test boots the
    // production kernel pipeline and, on `AuditEvent::BootCompleted`, enables
    // `IA32_EFER.NXE`, builds a fresh address space (low 32 MiB identity +
    // higher-half kernel window), switches CR3, installs a dispatch callback,
    // then calls the production capability-checked, audited spawn caller
    // (`tairix_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
    // build the program's ring-3 image — segments mapped + filled W^X (code RX,
    // data RW-NX, rodata R-NX), user stack, startup-vector block — and `iretq`s
    // into it through the Arch HAL `EnterUser` primitive. The program (built via
    // `build_process_image` at a high `USER_BIAS`) parses `argv[1]`, returns it,
    // and crt0 routes the return through the `exit` syscall, whose `syscall`
    // traps back through the kernel's `IA32_LSTAR` entry stub to the dispatch
    // callback, which asserts the code equals the spawned decimal argument
    // before `qemu_exit::exit_success`; any mismatch (or a returning spawn)
    // flips `qemu_exit::exit_failure`. Single CPU suffices and the 60-second
    // budget matches the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "tairix-test-spawn-program-qemu-x86_64",
        binary: "tairix-test-spawn-program-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC5 deliverable (`plans/CCOMPAT.md`): the riscv64
    // end-to-end C-program round-trip — the headline CC5 work. The build
    // script builds the Rust crt0 + `tairix_sys_*` runtime shim
    // (`tests/integration/cc5_program`) as a PIE `staticlib`, compiles the
    // genuinely C-language program (`cc5_program/csrc/main.c`) with the audited,
    // version-pinned, checksummed `clang`/`ld.lld` wrapper (`tools/cc`), links them into one PIE image, and converts it to an `rxe`
    // blob (`tairix_itest_harness::elf2rxe`) carrying the kernel's syscall CFI
    // tag. On boot the test stands up an Sv39 address space (identity-mapping
    // the kernel + MMIO), installs the trap vector and a dispatch callback, then
    // calls the production capability-checked, audited spawn caller
    // (`tairix_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to build
    // the program's U-mode image and `sret` into it. The C program checks a
    // Time64 value across the pre-1970/post-2038 boundaries, an ipc header,
    // and a sysinfo header, then issues `cap_query` + `clock_get`; the callback
    // services those (asserting the marshalled cap id, returning a 64-bit
    // sentinel) and asserts the `exit` code is 99 before the `SiFive` Test PASS
    // finisher. Proves the generated C header, the `tairix_sys_*` runtime, and crt0
    // agree with the Rust side end to end. Single CPU; 60-second run budget.
    QemuTest {
        package: "tairix-test-c-program-qemu-riscv64",
        binary: "tairix-test-c-program-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC5 deliverable (`plans/CCOMPAT.md`): the aarch64
    // end-to-end C-program round-trip — the EL0 analogue of the riscv64
    // vertical above. The build script builds the Rust crt0 + `tairix_sys_*`
    // runtime shim (`tests/integration/cc5_program`) as a PIE `staticlib`,
    // compiles the genuinely C-language program (`cc5_program/csrc/main.c`)
    // with the audited, version-pinned, checksummed `clang`/`ld.lld` wrapper
    // (`tools/cc`), links them into one PIE image, and converts
    // it to an `rxe` blob (`tairix_itest_harness::elf2rxe`) carrying the
    // kernel's syscall CFI tag. On boot the test enables `CPACR_EL1.FPEN`,
    // stands up a stage-1 address space (identity-mapping the kernel + MMIO,
    // EL1), installs the EL1 vector table and a dispatch callback, then calls
    // the production capability-checked, audited spawn caller
    // (`tairix_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
    // build the program's EL0 image and `eret` into it. The C program checks a
    // Time64 value across the pre-1970/post-2038 boundaries, an ipc header,
    // and a sysinfo header, then issues `cap_query` + `clock_get`; the callback
    // services those (asserting the marshalled cap id, returning a 64-bit
    // sentinel) and asserts the `exit` code is 99 before the ARM semihosting
    // PASS finisher. Single CPU; 60-second run budget.
    QemuTest {
        package: "tairix-test-c-program-qemu-aarch64",
        binary: "tairix-test-c-program-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // CCOMPAT stage CC5 deliverable (`plans/CCOMPAT.md`): the x86_64
    // end-to-end C-program round-trip — the ring-3 analogue of the
    // riscv64/aarch64 verticals above, completing CC5. The build script builds
    // the Rust crt0 + `tairix_sys_*` runtime shim (`tests/integration/cc5_program`)
    // as a PIE `staticlib`, compiles the genuinely C-language program
    // (`cc5_program/csrc/main.c`) with the audited, version-pinned, checksummed
    // `clang`/`ld.lld` wrapper (`tools/cc`), links them into one
    // PIE image, and converts it to an `rxe` blob (`tairix_itest_harness::elf2rxe`)
    // carrying the kernel's syscall CFI tag. Because the x86_64 ring-3
    // transition needs the GDT user selectors, the TSS, and `syscall`/
    // `IA32_LSTAR` entry installed, the test boots the production kernel pipeline
    // and, on `AuditEvent::BootCompleted`, enables `IA32_EFER.NXE`, builds a
    // fresh address space (low 32 MiB identity + higher-half kernel window),
    // switches CR3, installs a dispatch callback, then calls the production
    // capability-checked, audited spawn caller
    // (`tairix_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to build
    // the program's ring-3 image (W^X: code RX, data RW-NX, rodata R-NX) and
    // `iretq` into it. The C program checks a Time64 value across the
    // pre-1970/post-2038 boundaries, an ipc header, and a sysinfo header, then
    // issues `cap_query` + `clock_get`; the callback services those (asserting
    // the marshalled cap id, returning a 64-bit sentinel) and asserts the `exit`
    // code is 99 before `qemu_exit::exit_success`. Single CPU; 60-second budget.
    QemuTest {
        package: "tairix-test-c-program-qemu-x86_64",
        binary: "tairix-test-c-program-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 4 deliverable: boot the production kernel pipeline, instantiate
    // `tairix_drvhost::Host`, load a baked-in signed mock `.rxe` image,
    // exercise `load → snapshot → reload → unload`, then flip
    // `qemu_exit::exit_success`. Single CPU suffices and the 60-second budget
    // matches the other Stage 3a boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-drvhost-qemu",
        binary: "tairix-test-drvhost-qemu",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 4 first-driver vertical: boot the production kernel
    // pipeline, then on `AuditEvent::BootCompleted` load the signed
    // PS/2 input driver (`drivers/input/ps2`) through
    // `tairix_drvhost::Host` and drive it through load -> use ->
    // unload -> reload. "Use" is interrupt-driven: it binds the
    // keyboard line (ISA IRQ-1 -> GSI 1) in the production
    // `tairix_kernel_irq::IrqTable`, enables the i8042 keyboard-
    // interrupt config bit, masks the legacy PIC, unmasks GSI 1 at the
    // IO-APIC, then injects a deterministic scancode via the
    // controller's `0xD2` ("write keyboard output buffer") command —
    // using the same `X86PortIo8` backend the driver reads through —
    // which asserts the real IRQ-1 line. After `sti` it waits on
    // `IrqTable::try_wait_step` for the IO-APIC -> LAPIC -> IDT ->
    // dispatcher -> `IrqTable::fire` round-trip to report
    // `WaitStep::Ready`, then drains and decodes the resulting press
    // then release into platform-neutral `InputEvent`s through the
    // driver's `poll`. Any deviation flips `qemu_exit::exit_failure`.
    // The default `q35` machine exposes the i8042 and a 24-pin
    // IO-APIC, so no extra QEMU device is needed. Single CPU suffices
    // and the 60-second budget matches the other Stage-3/4
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-ps2-qemu-x86-64",
        binary: "tairix-test-ps2-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 4.D Item 2-tail.2 QEMU validation: boot the production
    // kernel pipeline, then drive a real hardware-interrupt round
    // trip on the legacy IRQ-0 GSI through the IO-APIC + PIT. The
    // test binary `tairix-test-irq-qemu-x86-64` installs an audit
    // sink that — on observing `AuditEvent::BootCompleted` — binds
    // the line in the published `IrqTable`, unmasks through the
    // production `IoApicController`, programs PIT channel 0 as a
    // one-shot, polls `IrqTable::try_wait_step` until
    // `WaitStep::Ready`, re-reads the IO-APIC redirection-entry
    // mask bit to verify the mask-before-wake invariant, and flips
    // `qemu_exit::exit_success`. Any deviation flips
    // `qemu_exit::exit_failure`. Single CPU suffices and a 60-second
    // budget matches the other Stage-3/4 boot-then-do-fixed-work
    // tests.
    QemuTest {
        package: "tairix-test-irq-qemu-x86-64",
        binary: "tairix-test-irq-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 4.D Item 4: `tairix-test-virtio-blk-pci-x86-64` performs a
    // full real virtio-blk-pci round-trip — boot → `mechanism_one`
    // PCI walk → map the four virtio register windows → route MSI-X →
    // mint a `KernelVirtioHost` over a per-device DMA pool → load the
    // signed virtio-blk `.rxe` → read sector 0 (verify the planted
    // `byte[i] = i mod 256` pattern) → write+read-back sector 1
    // (verify) → `qemu_exit`. The earlier ~30% single-CPU MSI
    // completion hang was a deadlock between the completion ISR's
    // `IrqTable::fire` and a parked `try_wait_step`; it was eliminated
    // by making `fire`/`try_wait_step` lock-free (per-line `bound` /
    // `ready` atomics, no shared `IrqTable` lock). Stability re-verified
    // across 90 consecutive QEMU runs (60 TCG via this exact runner
    // path + 30 KVM) with zero hangs, so it is enrolled here. The
    // 2048-sector backing image gives the planted sector-0 pattern plus
    // headroom for the sector-1 write/read-back. A 60-second budget
    // matches the other Stage-3/4 boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-virtio-blk-pci-x86-64",
        binary: "tairix-test-virtio-blk-pci-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: Some(2048),
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 5 end-to-end FAT32 vertical:
    // `tairix-test-fat32-virtio-blk-pci-x86-64` reuses the exact
    // virtio-blk-pci bring-up above, then instead of a raw sector round-trip it
    // mounts the planted FAT32 volume through the real FAT32 driver, verifies
    // the planted file, and creates+writes+reads-back a fresh file before
    // `qemu_exit`. The backing image is the shared `tairix-test-fat32-image`
    // FAT32 volume (`FsDisk::Fat32`), not the sector-0 pattern, so its geometry
    // is the image's own size. Single CPU and a 60-second budget match the
    // other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-fat32-virtio-blk-pci-x86-64",
        binary: "tairix-test-fat32-virtio-blk-pci-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::Fat32,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 5 end-to-end arxfs vertical:
    // `tairix-test-arxfs-virtio-blk-pci-x86-64` reuses the exact
    // virtio-blk-pci bring-up above, then instead of a raw sector round-trip it
    // mounts the planted arxfs volume through the real arxfs driver, verifies
    // the planted file, and creates+writes+reads-back a fresh file before
    // `qemu_exit`. The backing image is the shared `tairix-test-arxfs-image`
    // arxfs volume (`FsDisk::ARXFS`) — which the driver itself authored — not
    // the sector-0 pattern, so its geometry is the image's own size. Single CPU
    // and a 60-second budget match the FAT32 vertical and the other
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-arxfs-virtio-blk-pci-x86-64",
        binary: "tairix-test-arxfs-virtio-blk-pci-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::ARXFS,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 4.D Item 4: `tairix-test-kernel-arch-boot-riscv64` boots
    // the riscv64 `virt`-board pipeline (OpenSBI → S-mode entry →
    // FDT `/memory` parse → `RiscvArch` → `BootInfo` →
    // `kernel_core::kernel_main`) and asserts `AuditEvent::BootCompleted`
    // (`EventId(4004)`). The bin's audit sink writes the `SiFive` Test
    // PASS finisher on observing it. Single CPU suffices (the slice
    // brings up one hart) and a 60-second budget matches the x86_64
    // `kernel_arch_boot` bring-up test.
    QemuTest {
        package: "tairix-test-kernel-arch-boot-riscv64",
        binary: "tairix-test-kernel-arch-boot-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NEW-SUPERVISOR.md` §9 Stage E:
    // `tairix-test-supervisor-memtest-takeover-qemu-riscv64` boots the
    // production riscv64 `virt` pipeline and, on `AuditEvent::BootCompleted`
    // (the point where the Supervisor system is published and the kernel state
    // is fully built), drives the pre-boot Supervisor's one-way `memtest`
    // takeover through the real published `SupervisorSystem::memtest_takeover`
    // seam. On the wired riscv64 port the caller first quiesces every other
    // hart (the bounded cross-CPU IPI-halt handshake), then the
    // `MachineTakeover` body masks interrupts, flattens paging to bare mode,
    // and tests all of RAM continuously on a reserved stack (every pattern over
    // all of RAM, looping until reset). Once the guest completes one full test
    // loop the harness issues a QEMU-monitor `system_reset`; QEMU
    // (`-no-reboot`) then exits status 0 and the runner registers
    // `Outcome::Pass`. A boot that never completes a loop falls silent and
    // times out; a takeover that *returned* (refused/unsupported) writes a fail
    // finisher — so a regression that stops the test running fails loud. The
    // production riscv64 port is single-hart (`BootInfo::new(BOOT_CPU, ...)`,
    // `RiscvArchStorage<1>`; it brings up no secondaries), so this boots
    // single-hart and the quiesce runs its "no online peers, succeed
    // immediately" path — the same handshake code, with nothing to stop.
    // (Booting `-smp 4` would only expose OpenSBI handing the kernel a non-zero
    // boot hart it does not support; the genuine multi-core quiesce is proven
    // by the aarch64 and x86_64 siblings, which do bring up secondaries.) The
    // 60-second budget is the inactivity window between progress updates, not a
    // total-runtime cap.
    QemuTest {
        package: "tairix-test-supervisor-memtest-takeover-qemu-riscv64",
        binary: "tairix-test-supervisor-memtest-takeover-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NEW-SUPERVISOR.md` §9 Stage E (aarch64 sibling of the riscv64
    // takeover above): `tairix-test-supervisor-memtest-takeover-qemu-aarch64`
    // boots the production aarch64 `virt` pipeline and, on
    // `AuditEvent::BootCompleted` (the point where the Supervisor system is
    // published and the kernel state is fully built), drives the pre-boot
    // Supervisor's one-way `memtest` takeover through the real
    // published `SupervisorSystem::memtest_takeover` seam. On the wired aarch64
    // port the caller first quiesces every other CPU (the bounded cross-CPU
    // IPI-halt handshake), then the `MachineTakeover` body masks interrupts and
    // stops the watchdog cadence, flattens paging (MMU off), and tests all of
    // RAM continuously on a reserved stack (every pattern over all of RAM,
    // looping until reset). Once the guest completes one full test loop the
    // harness issues a QEMU-monitor `system_reset`; QEMU (`-no-reboot`) then
    // exits status 0 and the runner registers `Outcome::Pass`. A boot that
    // never completes a loop falls silent and times out; a takeover that
    // *returned* (refused/unsupported) writes a fail finisher — so a regression
    // that stops the test running fails loud. Single-core (embedded 1-CPU DTB),
    // so the quiesce runs its no-peers path here (like the riscv64 sibling); the
    // genuine multi-core quiesce is proven by the x86_64 sibling below, whose
    // CPUs come from ACPI rather than an embedded DTB. Keeping this
    // continuous-memtest guest single-core also keeps it a light citizen in the
    // parallel QEMU matrix. The 60-second budget is the inactivity window
    // between progress updates, not a total-runtime cap.
    QemuTest {
        package: "tairix-test-supervisor-memtest-takeover-qemu-aarch64",
        binary: "tairix-test-supervisor-memtest-takeover-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NEW-SUPERVISOR.md` §9 Stage E (x86_64 sibling of the riscv64 /
    // aarch64 takeovers above): `tairix-test-supervisor-memtest-takeover-qemu-x86-64`
    // boots the production x86_64 `tairix-kernel` pipeline and, on
    // `AuditEvent::BootCompleted` (the point where the Supervisor system is
    // published and the kernel state is fully built), drives the pre-boot
    // Supervisor's one-way `memtest` takeover through the real
    // published `SupervisorSystem::memtest_takeover` seam. On the wired x86_64
    // port the caller first quiesces every other CPU (the bounded cross-CPU
    // IPI-halt handshake), then the `MachineTakeover` body masks interrupts,
    // switches onto a reserved `.bss` stack, installs the reserved boot page
    // tables, and tests all of RAM continuously on that stack (every pattern
    // over all of RAM, looping until reset). The takeover never resets the
    // board itself; once the guest completes one full test loop the harness
    // issues a QEMU-monitor `system_reset` — so QEMU (`-no-reboot`)
    // exits and the runner registers `Outcome::Pass`. A boot that never
    // completes a loop falls silent and times out; a takeover that *returned*
    // (refused/unsupported) writes a fail finisher — so a regression that stops
    // the test running fails loud. Four CPUs so the takeover genuinely stops
    // its three application processors before tearing the machine down (proving
    // the quiesce); the 60-second budget is the inactivity window between
    // progress updates, not a total-runtime cap.
    QemuTest {
        package: "tairix-test-supervisor-memtest-takeover-qemu-x86-64",
        binary: "tairix-test-supervisor-memtest-takeover-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 4,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage RV-P3 (`plans/PI.md`): `tairix-test-spawn-init-qemu-riscv64`
    // boots the *production* riscv64 `tairix-kernel` pipeline
    // (`boot_riscv64::boot`) on the `virt` board, then drops into PID 1
    // (`init`) in U-mode through the `InitSpawn` seam `boot_riscv64` installs
    // into the `BootInfo` hand-off. After `kernel_core::kernel_main` emits
    // `AuditEvent::BootCompleted` it builds the embedded `init` (`Run`)
    // U-mode image through the capability-checked, audited `spawn_image` +
    // `admit_init` (emitting `ProcessSpawned`, `EventId(4030)`) and dispatches
    // it; `init` writes its banner through `stream_write` (over the SBI
    // console backing) and issues the audited `spawn` syscall, whose `ecall`
    // traps back through the S-mode vector to the production dispatch callback
    // (emitting `SyscallInvoked`, `EventId(5000)`). The audit sink reports
    // PASS through the `SiFive` Test finisher once it has seen `ProcessSpawned`
    // then `SyscallInvoked` — proving PID 1 reached U-mode, wrote its banner,
    // and trapped back (the riscv64 sibling of the aarch64 / x86_64
    // `spawn-init-qemu` verticals). Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-spawn-init-qemu-riscv64",
        binary: "tairix-test-spawn-init-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 3c: `tairix-test-timer-preempt-qemu-riscv64` is the riscv64
    // half of the Stage-3 "timer interrupt drives the scheduler"
    // per-sub-stage deliverable. It boots the `virt` board, reads the
    // device-tree `timebase-frequency`, installs a `preempt`
    // scheduler-tick callback, arms the SBI timer at 100 Hz + enables
    // `sie.STIE`, and idles on `wfi` until the supervisor-timer trap path
    // has driven the callback 20 times — proving the timer repeatedly
    // delivers and re-arms — then writes the `SiFive` Test PASS finisher.
    // A revert to no-timer scheduling never reaches the count, so the run
    // times out and the harness reports the failure. Single CPU (the
    // slice brings up one hart) and a 60-second budget match the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-timer-preempt-qemu-riscv64",
        binary: "tairix-test-timer-preempt-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 3c: `tairix-test-ipi-smp-qemu-riscv64` is the riscv64
    // multi-hart SMP deliverable. It boots the `virt` board with two
    // harts, derives the boot hart id at runtime (OpenSBI may boot on
    // either), starts the other hart through `smp::start_secondary` (the
    // SBI HSM `hart_start` call), waits for that hart to install its trap
    // vector and enable supervisor software interrupts, then sends it a
    // directed IPI through `RiscvArch::send_ipi` (the SBI IPI extension,
    // replacing the former no-op). The test passes once the secondary
    // hart's `sip.SSIP` trap path has run the IPI callback with the
    // secondary hart's id — proving both hart bring-up and IPI delivery.
    // A regression that fails to start the hart or deliver the IPI never
    // reaches the PASS finisher, so the run times out. Two CPUs (the
    // point of the test) and a 60-second budget.
    QemuTest {
        package: "tairix-test-ipi-smp-qemu-riscv64",
        binary: "tairix-test-ipi-smp-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 2,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // WIRING Stage W6 (`plans/WIRING.md` §3): the aarch64 multi-core SMP
    // deliverable — the EL1/GICv2 analogue of `ipi_smp_qemu_riscv64`. It
    // boots the `virt` board with four cores, starts cores 1–3 through
    // the PSCI `CPU_ON` path, waits for each core to bring up its GICv2
    // interface and enable the IPI SGI, then sends each a directed IPI
    // through `Aarch64Arch::send_ipi` (a GICv2 SGI,
    // replacing the former single-CPU self-target best-effort send). The
    // test passes once every secondary's IRQ path has run the IPI callback
    // with that core's id — proving bring-up and all three target-list bits.
    // A regression that fails to start a core or
    // deliver the IPI never reaches the PASS finisher, so the run times
    // out. Four CPUs mirror the Raspberry Pi 4 and use a 60-second budget.
    QemuTest {
        package: "tairix-test-ipi-smp-qemu-aarch64",
        binary: "tairix-test-ipi-smp-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 4,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 3c: `tairix-test-sched-drive-qemu-riscv64` is the riscv64
    // "arch primitives drive the live scheduler" deliverable — the wiring
    // that connects the `preempt` (timer + IPI) and `context` primitives
    // into the architecture-neutral `kernel/sched` `Scheduler`, rather
    // than the test-local counting callbacks the `timer_preempt` /
    // `ipi_smp` verticals use. It boots the `virt` board, performs a real
    // bidirectional `context::switch` round-trip (interrupts off), builds
    // a real `tairix-kernel-sched-mlfq::Scheduler` over `RiscvArch`,
    // installs the `preempt` timer callback and the IPI software-interrupt
    // callback so both drive `Scheduler::on_timer_tick`, arms the 100 Hz
    // SBI timer + IPI, spawns a batch of tasks, sends itself a directed
    // IPI, and drives the cooperative `step` loop until every task has
    // run. PASS once the supervisor-timer trap has driven the live
    // scheduler >= 20 times and the IPI software-interrupt path has driven
    // it at least once. A regression that fails to switch, dispatch,
    // tick, or deliver the IPI either trips a dedicated failure finisher
    // or never reaches PASS, so the run fails loudly. Single CPU (the
    // slice brings up one hart) and a 60-second budget match the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-sched-drive-qemu-riscv64",
        binary: "tairix-test-sched-drive-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // WIRING Stage W7 (`plans/WIRING.md` §3): the aarch64 "arch
    // primitives drive the live scheduler" deliverable — the EL1/GICv2
    // analogue of `sched_drive_qemu_riscv64`. It boots the `virt` board,
    // performs a real bidirectional `context::switch` round-trip
    // (interrupts off), builds a real `tairix-kernel-sched-mlfq::Scheduler`
    // over `Aarch64Arch`, installs the `preempt` generic-timer callback
    // and the GICv2 IPI (SGI) callback so both drive
    // `Scheduler::on_timer_tick`, brings up the EL1 vectors + GICv2, arms
    // the 100 Hz generic timer + IPI, spawns a batch of tasks, sends
    // itself a directed IPI, and drives the cooperative `step` loop until
    // every task has run. PASS once the generic-timer IRQ has driven the
    // live scheduler >= 20 times and the IPI SGI path has driven it at
    // least once. PI Stage P4 (`plans/PI.md`): the tick interval is sized
    // from the timer frequency *discovered* from the embedded `virt` DTB
    // (`kernel_arch::timer_frequency_hz`) and the GICv2 base is poisoned
    // then rediscovered (`gic::configure_from_fdt`) before `gic::init`, so
    // both the timer ticks and the IPI run over discovered values, not the
    // pre-discovery defaults. A regression that fails to switch, dispatch,
    // tick, or deliver the IPI either trips a dedicated failure finisher or
    // never reaches PASS, so the run fails loudly. Single CPU (the slice
    // brings up one core) and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-sched-drive-qemu-aarch64",
        binary: "tairix-test-sched-drive-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP1 (`plans/SPAWN.md` §1): the `kernel/core` kthread
    // runtime proven on real silicon — two kernel-thread tasks ping-pong
    // through the *real* `tairix_arch_api::ContextSwitch::switch` under the
    // live scheduler, making that primitive a production scheduling path
    // for the first time (until now it was exercised only by the W7
    // `sched_drive` round-trip). It boots the `virt` board, reads the
    // GICv2 base + timer rate from the embedded `virt` DTB and brings up
    // the EL1 vectors + GICv2 (interrupts stay masked — dispatch is the
    // cooperative `step` loop, so the kthread switches are the only
    // mechanism under test), builds a real `tairix-kernel-sched-mlfq`
    // `Scheduler` over `Aarch64Arch`, spawns two kthreads via
    // `kernel_core::spawn_kthread` whose bodies `yield_now` back and forth,
    // and drains the `step` loop. PASS once both kthreads have run their
    // full ping-pong count and exited; a switch that never resumed its
    // task stalls the drain and the harness reports a timeout (fail-loud). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-kthread-switch-qemu-aarch64",
        binary: "tairix-test-kthread-switch-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP1 (`plans/SPAWN.md` §1): the riscv64 sibling of the
    // aarch64 kthread-switch vertical above — the same "two kthreads
    // ping-pong through the *real* `tairix_arch_api::ContextSwitch::switch`
    // under the live scheduler" proof, now on the riscv64 `virt` board, so
    // the `kernel/core` kthread runtime is a production scheduling path on
    // riscv64 too. It boots `virt`, reads the generic-timer rate from the
    // firmware DTB (the verbatim `a1` pointer), builds a real
    // `tairix-kernel-sched-eevdf` `Scheduler` over `RiscvArch`, spawns two
    // kthreads via `kernel_core::spawn_kthread` whose bodies `yield_now`
    // back and forth (interrupts stay masked — dispatch is the cooperative
    // `step` loop), and drains the loop. PASS once both kthreads have run
    // their full ping-pong count and exited; a switch that never resumed
    // its task stalls the drain and the harness reports a timeout
    // (fail-loud). Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-kthread-switch-qemu-riscv64",
        binary: "tairix-test-kthread-switch-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP1 (`plans/SPAWN.md` §1): the x86_64 sibling of the
    // kthread-switch vertical — the same "two kthreads ping-pong through
    // the *real* `tairix_arch_api::ContextSwitch::switch` under the live
    // scheduler" proof on the multiboot-loaded x86_64 kernel, so the
    // `kernel/core` kthread runtime is a production scheduling path on
    // x86_64 too. On the boot CPU it installs the per-CPU GDT/IDT, builds a
    // real `tairix-kernel-sched-eevdf` `Scheduler` over the production
    // `X86_64Arch` handle (no AP bring-up, no LAPIC timer — interrupts stay
    // masked, so the spawn self-IPI is latched and never delivered), spawns
    // two kthreads via `kernel_core::spawn_kthread` whose bodies `yield_now`
    // back and forth, and drains the cooperative `step` loop. PASS once both
    // kthreads have run their full ping-pong count and exited; a switch that
    // never resumed its task stalls the drain and the harness reports a
    // timeout (fail-loud). Single CPU and a 60-second budget
    // match the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "tairix-test-kthread-switch-qemu-x86-64",
        binary: "tairix-test-kthread-switch-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // WIRING Stage W6 (`plans/WIRING.md` §3): the cross-CPU TLB-shootdown
    // HAL slice (`tairix_arch_api::CrossCpuTlbShootdown`) proven on real
    // emulated cores, one vertical per bare-metal port. riscv64: the boot
    // hart starts a second hart, then `RiscvArch::shootdown_page` runs the
    // local `sfence.vma` + the SBI RFENCE `remote_sfence_vma` firmware call
    // to the live hart, and the test asserts the firmware reports the
    // remote fence reached it. Two CPUs (the point of the test) and a
    // 60-second budget match the other multi-hart riscv64 tests.
    QemuTest {
        package: "tairix-test-cross-cpu-tlb-shootdown-qemu-riscv64",
        binary: "tairix-test-cross-cpu-tlb-shootdown-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 2,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // WIRING Stage W6: the aarch64 cross-CPU TLB-shootdown vertical. The
    // boot core starts a second core via PSCI `CPU_ON`, then
    // `Aarch64Arch::shootdown_page` issues the inner-shareable *broadcast*
    // `tlbi vaae1is` + `dsb ish`/`isb` — the hardware propagates it to
    // every PE in the domain, so no IPI or software acknowledge is needed.
    // Reaching PASS proves the broadcast executes on a real two-core
    // machine without faulting. Two CPUs and a 60-second budget.
    QemuTest {
        package: "tairix-test-cross-cpu-tlb-shootdown-qemu-aarch64",
        binary: "tairix-test-cross-cpu-tlb-shootdown-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 2,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // WIRING Stage W6: the x86_64 cross-CPU TLB-shootdown vertical — the
    // port whose cross-CPU invalidation is entirely hand-written software
    // (no broadcast `invlpg`). The BSP brings up an application processor
    // via INIT-SIPI-SIPI; both install the shootdown ISR; the BSP drives
    // `X86_64Arch::shootdown_page`, which IPIs the AP and spins on the
    // acknowledge counter, returning only once the AP's ISR has `invlpg`'d
    // and acknowledged. Reaching PASS proves the IPI + invalidation + ack
    // round-trip ran on a second real core. Two CPUs and a 60-second
    // budget.
    QemuTest {
        package: "tairix-test-cross-cpu-tlb-shootdown-qemu-x86-64",
        binary: "tairix-test-cross-cpu-tlb-shootdown-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 2,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 3c: `tairix-test-memory-isolation-qemu-riscv64` is the riscv64
    // half of the Stage-3 "memory-isolation test passes" per-sub-stage
    // deliverable — the riscv64 analogue of `tairix-test-memory-isolation`
    // (x86_64). It boots the `virt` board, builds a victim and an attacker
    // Sv39 `paging::AddressSpace` (each identity-maps the low 4 GiB) that
    // disagree on a single 64 GiB virtual address, installs a `fault`
    // handler, switches `satp` to the attacker space, and reads that
    // address: the MMU raises a load page fault, the handler confirms the
    // cause / faulting address / victim-intact invariants, and writes the
    // `SiFive` Test PASS finisher. A regression that fails to isolate the
    // address never faults and trips the failure finisher instead. Single
    // CPU (the slice brings up one hart) and a 60-second budget match the
    // other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-memory-isolation-qemu-riscv64",
        binary: "tairix-test-memory-isolation-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (riscv64 stage G1):
    // `tairix-test-stack-guard-qemu-riscv64` is the riscv64 sibling of
    // `tairix-test-stack-guard-qemu-aarch64` — it proves the live Sv39
    // block-split the riscv64 kthread kernel-stack guard page is built on.
    // It builds one `paging::AddressSpace` (identity-maps the low 4 GiB),
    // calls `AddressSpace::split_block` to shatter the coarse identity leaf
    // covering a dedicated `GUARD_PAGE` static down to 4 KiB pages
    // (preserving every mapping), installs the S-mode trap vector + a
    // `fault` handler, turns paging on, writes+reads-back a sentinel
    // through the guard page (proving the split preserved the mapping
    // live), then `unmap`s that single page through the Arch HAL +
    // `flush_page`s its stale TLB entry and reads it: the MMU raises a load
    // page fault, the handler confirms the cause / faulting address, and
    // writes the `SiFive` Test PASS finisher. A regression that fails to
    // split, preserve, or unmap either reports FAILURE explicitly or never
    // faults (timing out). Single CPU and a 60-second budget match the
    // other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-stack-guard-qemu-riscv64",
        binary: "tairix-test-stack-guard-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `tests/SECURITY.md` §5 / `PLAN.md` Stage 7 item E — the per-port
    // `copy_from_user` hardware fault fix-up verticals. Each takes a
    // *real* kernel-mode data fault inside the port's guarded user-copy
    // window (read side and write side) and PASSes only when the fault
    // surfaces as an error return from the copy — the trap handler
    // redirected the saved PC to the window's fix-up — with the CPU
    // continuing to run; the fatal fault handler reports FAILURE. The
    // riscv64/aarch64 members stand up a minimal identity-mapped kernel
    // around their trap-vector installers (which arm the Arch HAL
    // guarded-copy slot); all three drive the one shared
    // `tairix_arch_api::uaccess::conformance` checks. Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-uaccess-fault-qemu-riscv64",
        binary: "tairix-test-uaccess-fault-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    QemuTest {
        package: "tairix-test-uaccess-fault-qemu-aarch64",
        binary: "tairix-test-uaccess-fault-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The x86_64 member boots the **production** `tairix-kernel` pipeline
    // (the dedicated `#PF` entry install + guarded-copy arm live on the
    // real boot path) and drives the shared checks on `BootCompleted`.
    QemuTest {
        package: "tairix-test-uaccess-fault-qemu-x86_64",
        binary: "tairix-test-uaccess-fault-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (riscv64 stage G3c): the
    // *production* fault-form, the riscv64 sibling of
    // `tairix-test-stack-overrun-qemu-aarch64`.
    // `tairix-test-stack-overrun-qemu-riscv64` proves that an overrunning
    // kthread takes a synchronous store page fault, not a next-reschedule
    // canary detection. It builds an Sv39 identity `AddressSpace`, prepares
    // a 2 MiB-aligned guard arena (`AddressSpace::prepare_guard_arena`, G2),
    // carves one kthread stack region `[guard page | usable stack]` out of
    // it, installs the S-mode trap vector + a `fault` handler, turns paging
    // on, then `unmap`s the guard page through the Arch HAL + `flush_page`s
    // it — the production guard-page mechanism (G3b-2). It then builds the
    // live `tairix-kernel-sched-eevdf` `Scheduler` over `RiscvArch`, admits a
    // kthread on that stack via `kernel_core::spawn_kthread_with_stack`, and
    // drives the cooperative `step` loop. The kthread body overruns its
    // stack (writes the highest guard byte, the first byte a contiguous
    // downward overrun crosses); because the guard page is unmapped the
    // access raises a synchronous store page fault *while the kthread runs*,
    // the handler confirms the cause / faulting address, and writes the
    // `SiFive` Test PASS finisher. A regression that left the page mapped
    // lets the body return cleanly; the drain loop then reports FAILURE
    // explicitly rather than passing. Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-stack-overrun-qemu-riscv64",
        binary: "tairix-test-stack-overrun-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 4.D Item 4: `tairix-test-virtio-blk-mmio-riscv64` is the
    // riscv64 `virt`-board MMIO analogue of the x86_64 virtio-blk-pci
    // vertical — boot → build the virtio-MMIO bus from the device tree →
    // provision an `MmioTransport` through the capability-gated
    // `KernelMmioMapper` → arm the device's PLIC source + S-mode trap
    // path → mint a `KernelVirtioHost` over a carved per-device DMA pool
    // → load the signed virtio-blk `.rxe` → read sector 0 (verify the
    // planted `byte[i] = i mod 256` pattern) → write+read-back sector 1 →
    // `SiFive` Test PASS. The device-tail round-trip is the same shared
    // code the x86_64 vertical runs. The 2048-sector backing image gives
    // the planted sector-0 pattern plus headroom; single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-virtio-blk-mmio-riscv64",
        binary: "tairix-test-virtio-blk-mmio-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: Some(2048),
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 4 first-driver vertical (display class):
    // `tairix-test-framebuffer-display-qemu-riscv64` boots the riscv64
    // `virt`-board pipeline, programs QEMU's `ramfb` over the `fw_cfg`
    // MMIO DMA interface so a static guest-RAM surface becomes a real
    // scan-out framebuffer, publishes the geometry as a
    // `FramebufferConfig` boot hand-off, then loads the signed
    // framebuffer display `.rxe` through `tairix_drvhost::Host` and
    // drives it through load -> use -> unload -> reload. "Use" maps the
    // surface through the capability-gated `KernelMmioMapper` and
    // `present`s a frame; a second independently-mapped window reads the
    // pixels back to confirm they reached the scan-out memory. Any
    // deviation flips the `SiFive` Test failure finisher. Single CPU and
    // a 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-framebuffer-display-qemu-riscv64",
        binary: "tairix-test-framebuffer-display-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 4 first-driver vertical (display class, x86_64 sibling of the
    // framebuffer vertical): `tairix-test-vesa-qemu-x86-64` boots the
    // production kernel pipeline, programs QEMU's `ramfb` over the
    // `fw_cfg` IOport DMA interface so a static guest-RAM surface becomes
    // a real scan-out framebuffer, publishes a bootloader-captured VBE
    // `ModeInfoBlock` describing it as the boot hand-off, then loads the
    // signed vesa display `.rxe` through `tairix_drvhost::Host` and drives
    // it through load -> use -> unload -> reload. "Use" decodes the block
    // with `VesaFramebuffer::open`, maps the surface through the
    // capability-gated `KernelMmioMapper`, and `present`s a frame; a
    // second independently-mapped window reads the pixels back to confirm
    // they reached the scan-out memory. Any deviation flips
    // `qemu_exit::exit_failure`. Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-vesa-qemu-x86-64",
        binary: "tairix-test-vesa-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage P6c-2 (`plans/PI.md`): `tairix-test-kernel-arch-boot-aarch64`
    // boots the *production* aarch64 `tairix-kernel` pipeline
    // (`boot_aarch64::boot`) on the `virt` board all the way to
    // `AuditEvent::BootCompleted` — the aarch64 analogue of the x86_64
    // `kernel-arch-boot` and the riscv64 `kernel-arch-boot-riscv64` verticals.
    // It enables the stage-1 identity MMU + EL1 vectors, discovers the board
    // from the embedded `virt` device tree (QEMU's aarch64 `-kernel <ELF>` path
    // passes no `x0` DTB pointer), builds the `BootMemoryMap`, installs the
    // discovered-UART console + `svc` dispatch callback, and hands a validated
    // `BootInfo` to `kernel_core::kernel_main`; the audit sink reports PASS
    // through the ARM semihosting finisher — and only with the ramfb
    // framebuffer boot console active: the run attaches `-device ramfb`, so the
    // production pre-MMU video bring-up must discover the tree's `fw_cfg` node,
    // program the scan-out over `lib/fwcfg`, and switch the console to the
    // screen (`video::is_active`), proving the display path `cargo xtask run`
    // relies on end to end. The run is `-smp 4` (matching the embedded tree's
    // `/cpus`): after `EventId(4004)` the sink waits for the production SMP
    // bring-up to PSCI-start all three secondaries and for each to attest
    // `SecondaryCpuOnline` (`EventId(4072)`) from the kernel dispatch loop —
    // the end-to-end multi-core boot proof; a `SecondaryCpuStartFailed`
    // (`EventId(4071)`) is an immediate FAIL. A 60-second budget matches the
    // other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-kernel-arch-boot-aarch64",
        binary: "tairix-test-kernel-arch-boot-aarch64",
        target: "aarch64-unknown-none",
        cpus: 4,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage P6c-3 (`plans/PI.md`): `tairix-test-spawn-init-qemu-aarch64`
    // boots the *production* aarch64 `tairix-kernel` pipeline
    // (`boot_aarch64::boot`) on the `virt` board, then drops into PID 1
    // (`init`) in EL0 through the `InitSpawn` seam `boot_aarch64` installs
    // into the `BootInfo` hand-off. After `kernel_core::kernel_main` emits
    // `AuditEvent::BootCompleted` it builds the embedded `init` (`Run`) EL0
    // image through the capability-checked, audited `spawn_and_enter`
    // (emitting `ProcessSpawned`, `EventId(4030)`) and `eret`s into it;
    // `init` returns and the `tairix-rt` runtime routes the return through
    // the audited `exit` syscall, whose `svc` traps back through the EL1
    // vector to the production dispatch callback (emitting `SyscallInvoked`,
    // `EventId(5000)`). The audit sink reports PASS through the ARM
    // semihosting finisher once it has seen `ProcessSpawned` then
    // `SyscallInvoked` — proving PID 1 reached user mode and trapped back.
    // Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-spawn-init-qemu-aarch64",
        binary: "tairix-test-spawn-init-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP3b (`plans/SPAWN.md`) + `plans/PI.md` P11:
    // `tairix-test-spawn-session-qemu-aarch64` boots the *production*
    // aarch64 `tairix-kernel` pipeline (`boot_aarch64::boot`) on the `virt`
    // board with both the `InitSpawn` seam and the runtime `ProcessSpawn`
    // producer installed. The aarch64 production boot embeds no program
    // rows (`plans/APPS.md` deliverable 8): every service is spawned from
    // its verified on-disk `/System` store bundle, so this vertical carries
    // the shared encrypted-root whole-disk image (whose `/System` volume
    // ships the complete signed bundles). After `kernel_main` emits
    // `BootCompleted` it spawns PID 1 `init` into EL0 (`ProcessSpawned`,
    // `EventId(4030)` #1); `init` writes its banner and supervises the
    // session: it issues the audited `spawn` syscall (`SyscallInvoked`,
    // `EventId(5000)`) for each service bundle and `wait`s on the children.
    // The script first answers the root-unlock passphrase prompt; the
    // unlock loads the volume's users database, so `login` — which waits
    // for that database — draws its full-screen view (the `Username:`
    // label inside the login box) and **blocks** in `stream_read` on the
    // kernel-core `BlockingConsoleRead` backing. The runner then holds the
    // scripted dialogue below with it: it types `root`, waits for the
    // `Password` label (the minimal-diff renderer repaints only the
    // changed cells over `Username:`, proving login read the username
    // whole and advanced rather than crashing per keystroke), types a
    // wrong password the authenticator refuses — the view then paints the
    // red `1 failed attempt` line and returns to the username field — and
    // finally types one character beyond the account format's shared
    // `MAX_USERNAME_LEN` validation bound; the view refuses the over-long
    // username whole (`LengthOutOfRange`), login records the console error
    // and exits fail-closed; `init` reaps it and relaunches it. The audit sink
    // reports PASS through the ARM semihosting finisher once it has seen
    // the expected `ProcessSpawned` and audited-syscall counts — and the
    // runner fails the run if the guest exits before every scripted prompt
    // appeared and every line was sent, so a login that dies mid-dialogue
    // cannot pass on its relaunch alone. Together that proves the
    // disk-backed spawn path (read + verify + launch off the mounted
    // volume), the interactive raw-mode read path, and supervision (reap +
    // restart) end to end. Single CPU; the 120-second budget covers the
    // unlock's key derivation on top of the boot-then-do-fixed-work
    // baseline.
    QemuTest {
        package: "tairix-test-spawn-session-qemu-aarch64",
        binary: "tairix-test-spawn-session-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, "root\n"),
            ("Password", Duration::ZERO, "wrong\n"),
            ("1 failed attempt", Duration::ZERO, OVERLONG_USERNAME),
        ],
    },
    // PI Design D P-3 (`plans/PI.md`):
    // `tairix-test-devmgr-hwtree-qemu-aarch64` boots the *production* aarch64
    // `tairix-kernel` pipeline (`boot_aarch64::boot`) verbatim on the `virt`
    // board and proves the **device-manager service's reactive observe loop**
    // end to end. PID 1 `init` now launches the perpetual `devmgr` service
    // (`/System/Services/devmgr`, in `spawn_layout::SPAWN_PROGRAMS`) before the
    // login session; `devmgr` reads the discovered hardware tree
    // (`hw_tree_read`) and **truly parks** in `hw_tree_wait`, registering on the
    // kernel's `HW_TREE_WAITQ` (Design D P-2 — no busy poll). The test injects
    // an observing `HwTreeSource` (the same dependency-injection seam the boot
    // path exposes for the log/audit sinks): the `hw_tree_wait` handler calls
    // its `generation()` in `devmgr`'s own context, after registering and just
    // before parking, so a non-empty `HW_TREE_WAITQ` there is the "devmgr is
    // about to park" witness. On the first park the source appends a node to
    // the authoritative `HwTreeStore` — a real generation bump / simulated
    // hotplug that calls `hw_tree_wake` exactly as the floor bus bring-up does —
    // and on the re-park (devmgr woke, re-read, re-registered) it reports PASS
    // via the ARM semihosting finisher. Because the witness is driven by
    // `devmgr`'s own read/wait loop it needs **no** login dialogue to keep
    // events flowing (an earlier audit-sink-driven version was flaky because
    // that incidental traffic dried up before `devmgr` parked); the run needs
    // no scripted serial input at all. `hw_tree_read`/`hw_tree_wait` are
    // unaudited high-volume reactive syscalls, so the wake's *correctness* is
    // pinned by the host unit tests (`kernel/core/src/waitq.rs`,
    // `kernel/core/src/syscalls.rs`); this vertical proves the integrated
    // boot → spawn → read → park → real-generation-bump → no-starvation path on
    // the production pipeline. The aarch64 production boot embeds no
    // program rows (`plans/APPS.md` deliverable 8): `devmgr` is spawned
    // from its verified on-disk `/System` store bundle, so this vertical
    // carries the shared encrypted-root whole-disk image. The read-only
    // `/System` volume is mounted under its well-known key *before* any
    // passphrase dialogue, so the parked service spawns resolve without
    // touching the console and the run still needs no scripted serial
    // input (the unanswered unlock prompt is harmless to this vertical's
    // witness). Single CPU; the 120-second budget covers the disk bring-up
    // on top of the boot-then-do-fixed-work baseline.
    QemuTest {
        package: "tairix-test-devmgr-hwtree-qemu-aarch64",
        binary: "tairix-test-devmgr-hwtree-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP2c (`plans/SPAWN.md` §1): the aarch64 EL0↔EL0 timeshare
    // vertical — the first proof that two **user** (EL0) tasks timeshare one
    // CPU under the live scheduler, on the `virt` board. It reads the GICv2
    // base + timer rate from the embedded `virt` DTB (P3/P4), brings up the
    // EL1 vectors + GICv2 (interrupts stay masked — dispatch is the cooperative
    // `step` loop), and builds **two** hardware-isolated EL0 address spaces from
    // the pure-Rust `tairix-test-el0-yielder` fixture (built PIE + converted to
    // `rxe` by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It admits each as a resumable user kthread via
    // `spawn_user_kthread` (its `pre_resume` hook reactivates that task's
    // page-table root) and drains the `step` loop; the dispatch callback
    // maps each task's `yield`/`exit` `svc` to `reschedule_current`, suspending
    // the running task back to the dispatcher exactly as the production callback
    // does. PASS once both tasks yielded their full count and exited; a switch
    // that never resumes stalls the drain and the harness times out (fail-loud). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-spawn-el0-timeshare-qemu-aarch64",
        binary: "tairix-test-spawn-el0-timeshare-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage D2b-2b-A P-1 (`plans/PI.md`): the aarch64 involuntary-preemption
    // vertical — the proof that the production generic-timer IRQ preempts a
    // **runaway** EL0 task on the `virt` board (the P-1a behavioural test the
    // boot matrix only covered by non-regression). It reads the GICv2 base +
    // timer rate from the embedded `virt` DTB (P3/P4), brings up the EL1 vectors
    // + GICv2, and builds **one** hardware-isolated EL0 address space from the
    // pure-Rust `tairix-test-el0-spinner` fixture (a `black_box`-guarded busy
    // loop that issues no syscall, built PIE + converted to `rxe` by `build.rs`)
    // through the capability-checked, audited `kernel_core::spawn_image`. It
    // then arms the **production** preemption path verbatim (the `tairix_arch_aarch64::preempt` surface the bin crate's
    // `arm_preemption` uses): a per-CPU `PreemptStorage`, an EL0-preemption
    // callback that `reschedule_current(_, Yield)`s the running task, and the
    // periodic generic timer; EL0 runs preemptible (`SPSR_EL0T_PREEMPTIBLE`), so
    // a tick taken while the spinner runs traps to `LOWER_IRQ` and preempts it.
    // Because the loop never traps, the only way it leaves EL0 before its final
    // `exit` is an involuntary preemption. PASS once the preempt callback fired
    // at least once AND the task — resumed mid-loop after each preemption —
    // still completed and exited; a preemption that never fires (the `step`
    // spins forever inside EL0) or a botched resume (the task never exits)
    // times out (fail-loud). Single CPU; a 120-second budget
    // covers the multi-tick busy loop under QEMU TCG.
    QemuTest {
        package: "tairix-test-preempt-el0-qemu-aarch64",
        binary: "tairix-test-preempt-el0-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Regression vertical for the interrupt-return-to-EL0 need-resched fix: prove
    // a **non-timer** interrupt taken from EL0 involuntarily preempts a **sole**,
    // CPU-bound EL0 task. Where the sibling above proves the *timer* preempts a
    // *contended* task, this proves the behaviour the fix adds — the reported
    // `stress --cpu 1` hang. It brings up the EL1 vectors + GICv2, builds one
    // hardware-isolated EL0 address space from the same `tairix-test-el0-spinner`
    // fixture, and wires the **production** preemption surface: the latch-gated
    // EL0-preemption callback (reschedules only when `take_preempt_pending` is
    // set — the `production_preempt_dispatch` shape) and the reschedule-IPI
    // callback that latches need-resched (`production_ipi_dispatch` shape). The
    // spinner is the SOLE runnable task, so the tickless scheduler never arms the
    // preemption timer; just before entering EL0 its kthread sends itself a
    // reschedule SGI (a non-timer IRQ), which is taken on the first EL0
    // instruction, latches need-resched, and — via the fix — drives the
    // return-to-EL0 preempt point so the latch-gated callback reschedules. PASS
    // once the callback rescheduled at least once AND the spinner, resumed
    // mid-loop, still completed and exited. Before the fix the SGI only latched
    // (the preempt point ran solely for the timer PPI) and the sole spinner ran
    // unpreempted forever, timing out (fail-loud). Single CPU; a 120-second
    // budget matches the sibling under QEMU TCG.
    QemuTest {
        package: "tairix-test-preempt-wake-qemu-aarch64",
        binary: "tairix-test-preempt-wake-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Deterministic regression for the syscall-return continuation boundary:
    // a real EL0 parent completes an ordinary `clock_get` with a pending
    // reschedule, CFQ runs a competing child that parks, and only then may the
    // parent resume and receive its sentinel result. The child stays parked
    // until the parent exits, so a missing parent requeue or corrupted saved
    // exception frame cannot be hidden by another wake; either defect reaches
    // the bounded harness timeout rather than a raised application deadline.
    QemuTest {
        package: "tairix-test-syscall-resume-qemu-aarch64",
        binary: "tairix-test-syscall-resume-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage D2b-2b-A P-1b (`plans/PI.md`): the riscv64 involuntary-preemption
    // vertical — the cross-port sibling of the aarch64 preempt test, proving the
    // production supervisor-timer interrupt preempts a **runaway** U-mode task on
    // the `virt` board. It reads the `timebase-frequency` from the firmware DTB
    // (the `a1` pointer), installs the S-mode trap vector via
    // `trap::install_trap_vector` — NOT `init_traps`, so `sstatus.SIE` stays
    // clear and the kernel itself is never preempted — and builds **one**
    // hardware-isolated Sv39 U-mode address space from the pure-Rust
    // `tairix-test-el0-spinner` fixture (a `black_box`-guarded busy loop that
    // issues no syscall, built PIE + converted to `rxe` by `build.rs`) through
    // the capability-checked, audited `kernel_core::spawn_image`. It then arms
    // the **production** preemption path verbatim (the
    // `tairix_arch_riscv64::preempt` surface the bin crate's `arm_preemption`
    // uses): a per-hart `PreemptStorage`, a U-mode-preemption callback that
    // `reschedule_current(_, Yield)`s the running task, and the periodic SBI
    // timer (`init_local_preempt` sets `sie.STIE`). A supervisor-timer interrupt
    // is taken while the spinner runs in U-mode by the privilege rule U < S, so
    // the trap handler's SPP-gated preempt point fires. Because the loop never
    // traps, the only way it leaves U-mode before its final `exit` is an
    // involuntary preemption. PASS once the preempt callback fired at least once
    // AND the task — resumed mid-loop after each preemption — still completed
    // and exited; a preemption that never fires (the `step` spins forever inside
    // U-mode) or a botched resume (the task never exits) times out (fail-loud). Single CPU; a 120-second budget covers the multi-tick
    // busy loop under QEMU TCG.
    QemuTest {
        package: "tairix-test-preempt-el0-qemu-riscv64",
        binary: "tairix-test-preempt-el0-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage D2b-2b-A P-1c (`plans/PI.md`): the x86_64 involuntary-preemption
    // vertical — the cross-port sibling of the aarch64/riscv64 preempt tests,
    // proving the production LAPIC-timer interrupt preempts a **runaway**
    // ring-3 task. Unlike the other ports, the ring-3 transition needs the GDT
    // ring-3 selectors, the TSS, and `syscall`/`IA32_LSTAR` entry installed, so
    // the test boots the production `tairix-kernel` pipeline (which also
    // programs the periodic LAPIC timer in `preempt::init_local_preempt`); only
    // the audit sink is replaced. On `BootCompleted` it enables
    // `IA32_EFER.NXE`, builds **one** hardware-isolated ring-3 address space
    // from the pure-Rust `tairix-test-el0-spinner` fixture (a
    // `black_box`-guarded busy loop that issues no syscall, built PIE +
    // converted to `rxe` by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`, and admits it as a resumable user kthread
    // whose `pre_resume` hook reloads CR3 and repoints **both** the per-CPU
    // `syscall` entry stack (`syscall_entry::set_kernel_rsp0`) and the
    // `TSS.RSP0` trap stack (`percpu::install_tss_rsp0`) at the task's own
    // kernel stack. It then arms the **production** ring-3-preemption path
    // verbatim (the `tairix_arch_x86_64::preempt::set_preempt_callback` surface
    // the bin crate's `install_irq_dispatch` uses): a callback that
    // `reschedule_current(_, Yield)`s the running task. Ring 3 runs preemptible
    // (`userentry`'s `IF`-set `RFLAGS`), so a LAPIC-timer tick taken while the
    // spinner runs lands on the timer ISR and (gated on the saved `CS` RPL)
    // drives the preempt point. Because the loop never traps, the only way it
    // leaves ring 3 before its final `exit` is an involuntary preemption. PASS
    // once the preempt callback fired at least once AND the task — resumed
    // mid-loop after each preemption — still completed and exited; a preemption
    // that never fires (the `step` spins forever inside ring 3) or a botched
    // resume (the task never exits) times out (fail-loud). Single CPU; a
    // 120-second budget covers the multi-tick busy loop under QEMU TCG.
    QemuTest {
        package: "tairix-test-preempt-el0-qemu-x86-64",
        binary: "tairix-test-preempt-el0-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PLAN.md P-5 (2026-06-23 amendment): the aarch64
    // in-kernel interrupt-delivery / non-preemption vertical — the dual of the
    // `preempt-el0` tests. Where those prove a runaway **EL0** task IS
    // involuntarily preempted, this proves the property the serial-stall saga
    // turned on: a busy **in-kernel** kthread that issues no `yield` and no
    // syscall still takes the generic-timer IRQ *during* its span (the EL1 IRQ
    // path runs `on_timer_interrupt` and the tick callback records it), but
    // because the tick was taken from EL1 the running task is NOT rescheduled
    // (the kernel is non-preemptible), so the EL0-preemption callback never
    // fires. It reads the GICv2 base + timer rate from the embedded `virt` DTB,
    // brings up the EL1 vectors + GICv2, registers the production
    // `tairix_arch_aarch64::preempt` surface verbatim (a
    // per-CPU `PreemptStorage`, the EL0-preemption callback, a timer-tick
    // callback, and the enabled generic-timer PPI), builds a live eevdf
    // `Scheduler`, admits one in-kernel kthread that arms the timer one-shot and
    // busy-loops, and enables device IRQs at the PE (`exceptions::enable_irq`,
    // the aarch64 backing of `KernelArch::set_device_irqs(true)`). PASS once a
    // tick was taken during the busy span AND the EL0-preemption callback fired
    // zero times AND the kthread resumed and ran to its voluntary completion.
    // Under the old cooperative loop (device IRQs masked across the whole task
    // run) no tick would ever be taken and the kthread would spin forever, so
    // the run fails loudly — a failure finisher or the harness timeout. Single CPU; a 120-second budget covers the busy loop
    // under QEMU TCG.
    QemuTest {
        package: "tairix-test-preempt-inkernel-qemu-aarch64",
        binary: "tairix-test-preempt-inkernel-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // plans/WATCHDOG.md B3 (plans/OPEN-DEFECTS.md D13): the aarch64
    // non-maskable-FIQ masked-section watchdog self-sample vertical — the
    // runtime proof of the debug-only tool that observes a core wedged in a
    // `DAIF.I`-masked section the maskable IRQ cadence physically cannot see.
    // On boot it reads the GICv2 base + timer rate from the embedded `virt`
    // DTB, brings up the EL1 vectors + GICv2, and runs the production
    // `watchdog::probe_fiq_deliverability` capability probe. QEMU `virt` is a
    // single-Security-state GIC (`secure=off`), so Group 0 / FIQ reaches
    // non-secure EL1 and the probe reports `Supported` (a real Pi 4 GIC-400
    // keeps Group 0 secure and would report `Unsupported`, falling back to the
    // complete cross-CPU buddy detector — the fail-closed capability). It then
    // installs the production cadence callback, arms a short Group-0 (FIQ)
    // cadence, deliberately masks `DAIF.I`, and busy-spins in a
    // `#[inline(never)]` marker issuing no yield and no syscall. Because FIQ is
    // gated by the *separate* `DAIF.F` bit the diagnostics build leaves clear,
    // the cadence fires *through* the mask and the self-sample captures a LIVE
    // snapshot of the interrupted PC. PASS once the probe reported `Supported`,
    // the FIQ fired while `DAIF.I` was masked (the sampled `SPSR_EL1.I` proves
    // it), the sample interrupted kernel context, and the sampled PC *and* the
    // `capture_sample_backtrace` top land inside the masked-spin marker
    // (`sampled=live`, not the stale `pre_silence` a buddy would see). Any
    // shortfall writes a distinct failure finisher or times out (fail-loud).
    // Single CPU; a 60-second budget covers the short cadence under QEMU TCG.
    QemuTest {
        package: "tairix-test-fiq-selfsample-qemu-aarch64",
        binary: "tairix-test-fiq-selfsample-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PLAN.md Stage 4.HW: the aarch64 driver-spawn handshake vertical — the
    // proving slice of the kernel-side production driver spawner. The build
    // script compiles the pure-Rust driver-stub fixture
    // (`tairix-test-driver-register-program`) PIE and converts it to an
    // `rxe` blob carrying the kernel's syscall CFI tag, registered under a
    // `/System/Drivers/` path. On boot the test discovers the board from the
    // embedded `virt` DTB, enables the identity MMU + EL1 vectors, builds a
    // live `kernel/mem` FrameAllocator, binds the reply Port (send-gated on
    // a driver-class capability) into a live `RwLock<PortRegistry>`,
    // installs the production `KernelDispatchHook` through a
    // `DispatchCallbackSlot`, and spawns the stub through the production
    // parameterised `Aarch64ProcessSpawn` image builder via the exported
    // `KernelSpawnCtx` admit path — driver-class caps plus the reply
    // endpoint id in `arg(1)`, exactly the hand-off the driver host gives a
    // spawned driver. The host side drives the cooperative `step` loop,
    // polling `Port::recv` under a bounded budget; the stub reads `arg(1)`,
    // sends `DriverRegisterReply::registered(...)` over the production
    // `ipc_send` path (caller-context resolution, copy-in, capability-gated
    // `Port::send`), and exits. PASS once the fail-closed-decoded reply
    // round-trips the stub's pinned handle; any shortfall writes a distinct
    // failure finisher or times out (fail-loud). Single CPU
    // and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "tairix-test-driver-spawn-qemu-aarch64",
        binary: "tairix-test-driver-spawn-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // plans/USB.md U1: the aarch64 driver-*unload* vertical — the symmetric
    // partner of the driver-spawn handshake above. It reuses the same signed
    // driver-stub fixture and the production devmgr-driven autoload/spawn path
    // (discover the `virt` board, build the live registries,
    // `DeviceManager::autoload` through `SpawnDriverLoader` +
    // `InitCtxDriverProcessSpawn` over `Aarch64ProcessSpawn` image builder), so
    // the driver is admitted Ready with its capability record +
    // address-space-registry entry minted. It then drives the production unload
    // mechanism `InitSpawnCtx::terminate_driver_process` (the seam the
    // driver-store server runs for `StoreRequest::Unload`) and asserts the
    // scheduler task was reaped (live-task count 1→0) and its caps + address
    // space reclaimed, and that a second unload of the now-gone handle fails
    // closed with `NotFound` (idempotent). PASS once teardown reclaimed
    // everything; any shortfall writes a distinct failure finisher or times out
    // (fail-loud). The driver is never dispatched, so it issues no syscall and
    // needs no reply port. Single CPU and a 60-second budget match the
    // driver-spawn vertical.
    QemuTest {
        package: "tairix-test-driver-unload-qemu-aarch64",
        binary: "tairix-test-driver-unload-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP5b-2 (`plans/SPAWN.md` §1): the aarch64 `mem_map`/
    // `mem_unmap` vertical — the first proof that an EL0 process obtains and
    // releases anonymous `RW` memory at runtime via `abi-v1`, on the `virt`
    // board. It reads the GICv2 base + timer rate from the embedded `virt` DTB
    // (P3/P4), brings up the EL1 vectors + GICv2, and builds **one** hardware-
    // isolated EL0 address space from the pure-Rust `tairix-test-mem-map`
    // fixture (built PIE + converted to `rxe` by `build.rs`) through the
    // capability-checked, audited `kernel_core::spawn_image`. It **retains** that
    // space live behind a `kernel_core::MemMap` producer backed by
    // `kernel_mem::map_anonymous`/`unmap_anonymous`, admits the program as a
    // resumable user kthread (`spawn_user_kthread`), and routes the
    // program's `mem_map`/`mem_unmap` `svc`s through the producer. The fixture
    // maps a region (FIXED), writes+verifies a pattern, unmaps it, then touches
    // the released range; the fault handler reports the use-after-unmap data
    // abort as PASS. A verification failure exits early (a distinct finisher)
    // and a missing fault stalls the drain (fail-loud). Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "tairix-test-mem-map-qemu-aarch64",
        binary: "tairix-test-mem-map-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // M1 file-mapping remainder (`docs/src/architecture/memory.md` §7o): the
    // aarch64 demand-paged `file_map` vertical — the end-to-end proof of the
    // **production** `KernelSyscallHandlers` fault path. The chassis installs
    // the production `KernelDispatchHook` through a `DispatchCallbackSlot`
    // (production `LiveMemMap` producer for `mem_map` *and* `file_map`, a
    // read-only in-guest filesystem double serving one fixture file, a real
    // `KernelProcessWait`, a `ProgramRegistry` carrying the three child
    // roles) and binds the production user-fault resolver to the same slot.
    // The four-role fixture program's parent is spawned through the
    // production `InitSpawnCtx::spawn_driver_process` seam and drives the
    // children through production `spawn` + `wait`: `verify` demand-faults
    // the mapping's first/interior/EOF-straddle pages (bytes + zero fill),
    // proves the mapping survives `fs_close`, hands an untouched mapped page
    // to `fs_open` as its path buffer (the syscall copy-path fault-resolution
    // proof), and unmaps (exit 0); `wild` reads after unmap and `store`
    // writes to the read-only mapping — both fault-killed, exit 139, observed
    // by the parent through `wait`. PASS once the chassis reaps a parent exit
    // of 0; every failure site carries a distinct finisher (the parent's
    // diagnostic exit code is folded in). Single CPU and a 60-second budget
    // match the other boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-file-map-qemu-aarch64",
        binary: "tairix-test-file-map-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The riscv64 twin of the file-map vertical above: the same four-role
    // fixture program and production `KernelDispatchHook` chassis, driven on
    // the riscv64 `virt` board through the S-mode trap path (load *and*
    // store/AMO U-mode page faults offered to the production resolver) and
    // the riscv64 production spawn producer.
    QemuTest {
        package: "tairix-test-file-map-qemu-riscv64",
        binary: "tairix-test-file-map-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The SP11c demand-grown user-stack vertical
    // (`docs/src/architecture/memory.md` §7c): the end-to-end proof that a
    // spawned process's stack grows on fault inside its reserved span,
    // bounded by the settable `StackBytes` limit, with the below-span guard
    // page staying fatal. The chassis installs the production
    // `KernelDispatchHook` through a `DispatchCallbackSlot` (production
    // `LiveMemMap` producer backing both `mem_map` and the stack-growth
    // fault path, a real `KernelProcessWait`, a `ProgramRegistry` carrying
    // the three child roles with parameters derived from the one shared
    // `spawn_layout` policy) and binds the production user-fault resolver
    // to the same slot. The four-role fixture program's parent is spawned
    // through the production `InitSpawnCtx::spawn_driver_process` seam and
    // drives the children through production `spawn` + `wait`: `grow`
    // recurses far past the eagerly committed stack top, verifying every
    // frame's bytes survive the fault-driven growth (exit 0); `limit`
    // lowers its own `StackBytes` soft bound via `rlimit_set` and recurses
    // past it — fault-killed, exit 139; `guard` reads the unmapped guard
    // page below the reserved span — fault-killed, exit 139. PASS once the
    // chassis reaps a parent exit of 0; every failure site carries a
    // distinct finisher (the parent's diagnostic exit code is folded in).
    // Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-stack-grow-qemu-aarch64",
        binary: "tairix-test-stack-grow-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The STRESSTEST ST2 memory-pinning plus one-vCPU IPC control: end-to-end
    // proof of
    // the `mem_pin`/`mem_unpin` pair through the production
    // `KernelDispatchHook` on real traps. The five-role fixture program's
    // parent is spawned through the production
    // `InitSpawnCtx::spawn_driver_process` seam and drives the children
    // through production `spawn` + `wait`: `deny` (no `CAP_MEM_PIN`) sees
    // `mem_pin` refused `PermissionDenied` at the audited dispatcher gate
    // while the ungated `mem_unpin` still succeeds; `pin` lowers its own
    // `pinned-memory-bytes` bound via `rlimit_set`, pins itself
    // (idempotently), sees an over-budget `mem_map` refused `OutOfRange`
    // and a within-budget one succeed, unpins, and sees the formerly
    // refused map succeed; `child` is spawned by the *pinned* parent and
    // its over-budget map succeeds — the pin mark is never inherited even
    // though the lowered limit is. PASS once the chassis reaps a parent
    // exit of 0; every failure site carries a distinct finisher (the
    // parent's diagnostic exit code is folded in). It then performs 64
    // private call/reply cycles with saved integer/FP/stack/address-space
    // checks as the one-CPU migration control.
    QemuTest {
        package: "tairix-test-mem-pin-qemu-aarch64",
        binary: "tairix-test-mem-pin-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    QemuTest {
        package: "tairix-test-mem-pin-migration-qemu-aarch64",
        binary: "tairix-test-mem-pin-migration-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 4,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The USERS U4 service-ceiling vertical: the end-to-end proof that a
    // service account's compiled capability ceiling binds a lying manifest
    // through the production `KernelDispatchHook` on real traps. The
    // two-role fixture program's parent is spawned through the production
    // `InitSpawnCtx::spawn_driver_process` seam holding `CAP_PROC_SPAWN` +
    // `CAP_SPAWN_AS_USER` and switches the `svc` role into the devmgr
    // account through the production `spawn` syscall, with the real
    // compiled system identity table resolving the switch. `svc`'s
    // registered manifest deliberately requests devmgr's ceiling plus every
    // sibling service's defining capability; running as devmgr, its own
    // `SYSINFO_HW`-gated `hw_tree_read` succeeds while `spawn_as`,
    // `users_db_read`, `seat_switch`, and `sysinfo_introspect` are each
    // refused `PermissionDenied` at the audited dispatcher gate — a
    // compromised service cannot borrow a sibling's authority even when its
    // manifest lies (`plans/USERS.md` U4). PASS once the chassis reaps a
    // parent exit of 0; every failure site carries a distinct finisher (the
    // parent's diagnostic exit code is folded in). Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work aarch64
    // tests.
    QemuTest {
        package: "tairix-test-service-ceiling-qemu-aarch64",
        binary: "tairix-test-service-ceiling-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The FIX-IO IO2 block-transport fault vertical: the live-kernel proof
    // that the bounded submit/reap block seam contains a wedged device
    // (`plans/FIX-IO.md` IO2). The host doubles cannot express it, because
    // the per-request deadline, the `CallReply` wait-set source, and the
    // ticket lifecycle are all kernel machinery. The chassis installs the
    // production `KernelDispatchHook` and spawns one EL0 fixture holding only
    // `CAP_IPC_ENDPOINT`; the fixture stands up a healthy block service
    // (driven by the shared `blkio::serve_request_recovering` engine over a
    // fault-injecting device, consumed through the production `RemoteBlock`)
    // beside a wedged one that is never serviced. It proves a transient blip
    // is ridden out inside the shared per-class reissue budget, a blip that
    // outlasts it fails closed as the typed transient class, a bad sector
    // keeps its own medium-error class, and an outstanding wedged request
    // neither stalls the healthy device nor completes early before its
    // elapsed deadline wakes the parked reaper and the claim reaps
    // `TimedOut`. PASS once the chassis reaps a fixture exit of 0; every
    // failure site carries a distinct finisher (the fixture's diagnostic exit
    // code is folded in). Single CPU; the 60-second budget is the usual
    // inactivity ceiling — the run itself is dominated by the fixture's own
    // 300 ms wedged deadline.
    QemuTest {
        package: "tairix-test-blkio-fault-qemu-aarch64",
        binary: "tairix-test-blkio-fault-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The riscv64 twin of the stack-grow vertical above: the same
    // four-role fixture program and production `KernelDispatchHook`
    // chassis, driven on the riscv64 `virt` board through the S-mode trap
    // path (load *and* store/AMO U-mode page faults offered to the
    // production resolver) and the riscv64 production spawn producer.
    QemuTest {
        package: "tairix-test-stack-grow-qemu-riscv64",
        binary: "tairix-test-stack-grow-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The x86_64 twin of the stack-grow verticals above (SP11e): the same
    // four-role fixture program and production `KernelDispatchHook`
    // composition, driven through the shared production board bring-up
    // (`bring_up_bsp` — the exact per-CPU/#PF/`syscall`+TSS sequence the
    // production `boot()` runs, including the production dispatch callback
    // and user-fault resolver) with the hook installed into the production
    // `DISPATCH_SLOT`, so ring-3 `#PF`s (reads *and* writes) reach the
    // production stack-growth resolver over the x86_64 dedicated `#PF`
    // entry and the x86_64 production spawn producer.
    QemuTest {
        package: "tairix-test-stack-grow-qemu-x86_64",
        binary: "tairix-test-stack-grow-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The THREADS `T3b-u` lightweight-thread vertical (aarch64): prove threads
    // end to end over the production `KernelDispatchHook` chassis, which here
    // also carries a real `KernelProcessSignal` — a group `exit` has to drive
    // every sibling to its stopping point, and `thread_create` refuses a group
    // that cannot be stopped. The six-role fixture program's parent is spawned
    // through the production `InitSpawnCtx::spawn_driver_process` seam and
    // drives each child through production `spawn` + `wait`: `counter`
    // (contended futex `Mutex` over one address space), `rendezvous` (a
    // `Condvar` wait that completes only because it genuinely parked — a spin
    // would starve the notifier on this single-CPU cooperative drive), `tls`
    // (each thread reads its own magic through its psABI thread pointer, before
    // and after a trap), `exitearly` (the kernel's clear-on-exit word releases
    // a joiner), and `groupexit` (a sibling parked in the kernel, reapable only
    // because the group exit reached it). PASS once the chassis reaps a parent
    // exit of 0. Single CPU and a 60-second budget match the sibling
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-threads-qemu-aarch64",
        binary: "tairix-test-threads-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The riscv64 twin of the threads vertical above: the same six-role fixture
    // program and production chassis, driven on the riscv64 `virt` board through
    // the S-mode trap path, so each thread's `tp` is the port's own per-task
    // thread pointer.
    QemuTest {
        package: "tairix-test-threads-qemu-riscv64",
        binary: "tairix-test-threads-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The x86_64 twin of the threads verticals above: the same six-role fixture
    // program, driven through the shared production board bring-up
    // (`bring_up_bsp`) with the hook installed into the production
    // `DISPATCH_SLOT`, so each thread's `FS` base is reloaded by the kernel at
    // every switch-in (the register is privileged on this port).
    QemuTest {
        package: "tairix-test-threads-qemu-x86_64",
        binary: "tairix-test-threads-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The S8b parser-sandbox vertical (`docs/src/security/sandbox.md`;
    // `plans/APPS.md` S8b): prove the `lib/sandbox` seam end
    // to end over the S8a kernel sandbox spawn mode on the aarch64 `virt`
    // board. The chassis installs the production `KernelDispatchHook`
    // (LiveMemMap for the `tairix-rt` heaps, a real `KernelProcessWait`, a
    // `ProgramRegistry` carrying the fixture's three worker paths) and
    // spawns the four-role fixture program's parent through the production
    // `InitSpawnCtx::spawn_driver_process` seam. The parent drives the
    // seam over the real syscalls: container + instruction decode of valid
    // and malformed inputs through a genuinely sandboxed decode worker
    // (its own binary spawned via `SpawnAttach::sandbox` over pipes), real
    // crash containment (a worker that exits without serving yields a
    // typed error, a logged crash event, and a surviving caller), and the
    // syscall wall probed from inside a live sandbox (`fs_open`/`spawn`
    // refused while the pipe reply crosses). PASS once the chassis reaps a
    // parent exit of 0; every failure site carries a distinct finisher
    // (the parent's diagnostic exit code is folded in). Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work aarch64
    // tests.
    QemuTest {
        package: "tairix-test-sandbox-qemu-aarch64",
        binary: "tairix-test-sandbox-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage 5d-0-ii (b′)-2 (`plans/PI.md`): the aarch64 `mmio_map` vertical —
    // the first proof that an EL0 driver maps a **granted device MMIO window**
    // at runtime via `abi-v1` `mmio_map` over the per-task **retained live
    // address space**, on the `virt` board. It reads the GICv2 base + timer
    // rate from the embedded `virt` DTB (P3/P4), brings up the EL1 vectors +
    // GICv2, and builds **one** hardware-isolated EL0 address space from the
    // pure-Rust `tairix-test-mmio-map` fixture (built PIE + converted to `rxe`
    // by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It wraps that space in the production
    // `kernel_mem::LiveSpace` and admits the program through the production
    // `kernel_core::spawn_user_kthread_with_stack_live`, so the retained space
    // is published on the per-CPU live-space slot while the program runs
    // (exactly the production aarch64 spawn path). It mints the task a grant
    // for the first `virt` virtio-MMIO transport window and routes the
    // program's `mmio_map` `svc` through `with_current_live_space` +
    // `LiveSpace::map_device_window`; the program reads the device's
    // `MagicValue` register (`0x74726976`) back through the mapped, caching-
    // disabled window and exits 0, which the dispatch callback reports as PASS.
    // A refused map, the wrong register value, an unexpected syscall, or no
    // exit trips a distinct finisher or times out (fail-loud).
    // The registry-backed grant owner-check is host-proven in
    // `kernel/core`. Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-mmio-map-qemu-aarch64",
        binary: "tairix-test-mmio-map-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP5b-2 (`plans/SPAWN.md` §1): the riscv64 `mem_map`/
    // `mem_unmap` vertical — the riscv64 sibling of the aarch64 vertical above,
    // proving a U-mode process obtains and releases anonymous `RW` memory at
    // runtime via `abi-v1` on the `virt` board. It stands up an Sv39 address
    // space (identity-mapping the kernel + MMIO), activates `satp`, installs
    // the trap vector + a dispatch callback + a fault handler, and builds
    // **one** hardware-isolated U-mode address space from the same pure-Rust
    // `tairix-test-mem-map` fixture (built PIE + converted to `rxe` by
    // `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It **retains** that space live behind a
    // `kernel_core::MemMap` producer backed by
    // `kernel_mem::map_anonymous`/`unmap_anonymous`, then `sret`s straight into
    // the program (no scheduler — the single task only direct-returns from its
    // `ecall`s, so the riscv64 cooperative-switch trap-save path is not on the
    // critical path); the dispatch callback routes the program's
    // `mem_map`/`mem_unmap` `ecall`s through the producer. The fixture maps a
    // region (FIXED), writes+verifies a pattern, unmaps it, then touches the
    // released range; the fault handler reports the use-after-unmap page fault
    // as PASS. A verification failure exits early (a distinct finisher) and a
    // missing fault stalls (fail-loud). Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-mem-map-qemu-riscv64",
        binary: "tairix-test-mem-map-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The riscv64 trap-entry thread-pointer discipline
    // (`kernel/arch/riscv64/src/trap.s`). `tp` is both the RISC-V psABI thread
    // pointer U-mode writes freely and this port's per-hart kernel identity
    // anchor, so a vector that let the U-mode value survive into the handler
    // would let a task name a *different* hart and steer the kernel onto that
    // core's per-CPU state (resume handle, dispatch slot, live address space).
    // The adversarial fixture (`tests/integration/tp_probe_program`) writes a
    // hostile sentinel into `tp` before every `ecall` and checks its own value
    // came back; the dispatch callback reads `smp::current_hartid()` on every
    // `ecall` and fails the run if it is not the true boot hart. PASS needs all
    // three: the true hart id every time, a zero exit (both round trips
    // intact), and both rounds actually run — each shortfall has its own
    // finisher, and a guest that never exits trips the harness timeout.
    // **Two CPUs** so the sentinel's low-bit `1` names a real sibling CPU: with
    // one hart a hostile id would fail to map and fall back to the boot CPU,
    // masking the very defect this test witnesses.
    QemuTest {
        package: "tairix-test-tp-isolation-qemu-riscv64",
        binary: "tairix-test-tp-isolation-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 2,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage RV-X1 (`plans/PI.md` §X tail): the riscv64 single-resumable-
    // user-kthread vertical — the first proof that a U-mode task is admitted as
    // a *resumable* user kthread on riscv64 and cooperatively parks/resumes
    // under the live scheduler over the RV1 park-safe trap path, the cross-port
    // sibling of the x86_64 X1 vertical and the aarch64 SP2c timeshare (one
    // task; the two-task `sscratch` per-task repointing is RV-X2). On boot it
    // reads the generic-timer rate from the firmware device tree, stands up an
    // Sv39 address space (identity-mapping the kernel + MMIO), activates `satp`,
    // and installs the trap vector + a dispatch callback. It builds **one**
    // hardware-isolated U-mode address space from the pure-Rust
    // `tairix-test-el0-yielder` fixture (built PIE + converted to `rxe` by
    // `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`, and admits it via `spawn_user_kthread`. The
    // task's `pre_resume` hook reactivates the task's own `satp` root
    // (`paging::activate_user_root`, the RV-X1 primitive). The cooperative
    // `step` loop drives it; the dispatch callback maps each `yield`/`exit`
    // `ecall` to `reschedule_current`, so it ping-pongs with the dispatcher on
    // its own kernel stack. PASS once it yielded its full count and exited; a
    // wrong drain count, an unexpected syscall, or a stall flips
    // `qemu_exit::exit_failure` or times out (fail-loud). Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // riscv64 tests.
    QemuTest {
        package: "tairix-test-spawn-el0-resume-qemu-riscv64",
        binary: "tairix-test-spawn-el0-resume-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage RV-X2 (`plans/PI.md` §X tail): the riscv64 two-task EL0
    // timeshare vertical — the first proof that TWO U-mode tasks timeshare one
    // hart as resumable user kthreads on riscv64 under the live scheduler over
    // the RV1 park-safe trap path, the cross-port sibling of the x86_64 X2
    // vertical and the aarch64 SP2c timeshare. On boot it reads the
    // generic-timer rate from the firmware device tree, installs the trap vector
    // + a dispatch callback, and builds **two** hardware-isolated U-mode address
    // spaces (two `PageTablePool`s + a shared frame pool) from
    // the pure-Rust `tairix-test-el0-yielder` fixture (built PIE + converted to
    // `rxe` by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`, and admits each via `spawn_user_kthread`. Each
    // task's `pre_resume` hook reactivates its own `satp` root
    // (`paging::activate_user_root`); `sscratch` is per-task hardware state that
    // `userentry::enter_user` arms on first entry and the RV1 trap vector
    // re-arms from each task's own kernel-stack frame on every U-return, so no
    // dispatcher-side stack repointing is needed (unlike x86_64's per-CPU
    // `set_kernel_rsp0`). The cooperative `step` loop drives both; the dispatch
    // callback maps each `yield`/`exit` `ecall` to `reschedule_current`, so the
    // two ping-pong with the dispatcher on their own kernel stacks. PASS once
    // both yielded their full count and exited; a wrong drain count, an
    // unexpected syscall, or a stall flips `qemu_exit::exit_failure` or times
    // out (fail-loud). Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-spawn-el0-timeshare-qemu-riscv64",
        binary: "tairix-test-spawn-el0-timeshare-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // The `plans/OPEN-DEFECTS.md` D37 regression witness: two U-mode tasks fill
    // the whole floating-point register file with different patterns and
    // timeshare one hart, and neither may see the other's values. The fixture
    // is the only riscv64 user binary in the tree that emits floating-point
    // instructions, which is what makes the defect observable: before the
    // per-task state landed, firmware left `sstatus.FS = Dirty` and the port
    // saved none of `f0`-`f31`/`fcsr`, so the two patterns mixed. A mismatch,
    // a short yield count, or a stall flips `qemu_exit::exit_failure` or times
    // out (fail-loud). Single CPU and the same 60-second budget as its
    // timeshare sibling above.
    QemuTest {
        package: "tairix-test-fp-isolation-qemu-riscv64",
        binary: "tairix-test-fp-isolation-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage RV-X3 (`plans/PI.md` §X tail): the riscv64 runtime-`spawn`
    // concurrent-producer vertical — the cross-port sibling of
    // `spawn_session_qemu_aarch64` / `_x86_64`, proving a parent task's
    // `CAP_PROC_SPAWN`-gated `spawn` builds a fresh, hardware-isolated Sv39
    // child space and admits it Ready concurrently on the riscv64 `virt` board.
    // The build script compiles the pure-Rust `tairix-test-spawn-session-program`
    // fixture twice (the parent role and the child/session role, built PIE +
    // converted to `rxe`). On boot it reads the generic-timer rate from the
    // firmware device tree, installs the trap vector + a dispatch callback,
    // builds the parent a hardware-isolated Sv39 U-mode space via
    // `kernel_core::spawn_image` (capability-checked + audited), and admits it
    // via `spawn_user_kthread` onto a leaked-`'static` live scheduler. The
    // parent issues a real `spawn` `ecall`; the dispatch callback routes it to a
    // riscv64 `ProcessSpawn` producer that builds the child a fresh isolated
    // Sv39 space THROUGH THE PARENT'S IDENTITY WINDOW WITHOUT switching the
    // running parent's `satp` and admits it Ready concurrently. The callback
    // maps each `yield`/`exit` `ecall` to `reschedule_current`, so the parent
    // and child timeshare the hart on their own kernel stacks (the RV1 park-safe
    // path). PASS once the producer built the child and both tasks ran to
    // `exit`; a failed spawn, an unexpected syscall, a wrong drain count, or a
    // stall flips `qemu_exit::exit_failure` or times out (fail-loud). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-spawn-session-qemu-riscv64",
        binary: "tairix-test-spawn-session-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP5b-2 (`plans/SPAWN.md` §1): the x86_64 `mem_map`/
    // `mem_unmap` vertical — the x86_64 sibling of the aarch64/riscv64
    // verticals above, proving a ring-3 process obtains and releases anonymous
    // `RW` memory at runtime via `abi-v1`. Unlike those self-contained test
    // kernels the x86_64 ring-3 transition needs the GDT user selectors, the
    // TSS, and `syscall`/`IA32_LSTAR` entry, so it boots the production
    // `tairix-kernel` pipeline (like `spawn_program_qemu_x86_64`); that
    // pipeline now also installs the dedicated, error-code-aware page-fault
    // entry (`tairix_arch_x86_64::fault`), so the deliberate use-after-unmap
    // `#PF` is observable. On `BootCompleted` it enables `IA32_EFER.NXE`,
    // installs a `fault` observer, builds **one** hardware-isolated user
    // address space from the same pure-Rust `tairix-test-mem-map` fixture
    // (built PIE + converted to `rxe` by `build.rs`) through the capability-
    // checked, audited `kernel_core::spawn_image`, **retains** it live behind a
    // `kernel_core::MemMap` producer backed by
    // `kernel_mem::map_anonymous`/`unmap_anonymous`, and `iretq`s into it; the
    // dispatch callback routes the program's `mem_map`/`mem_unmap` `syscall`s
    // through the producer. The fixture maps a region (FIXED), writes+verifies
    // a pattern, unmaps it, then touches the released range; the fault observer
    // reports the use-after-unmap `#PF` as PASS. A verification failure, an
    // unexpected syscall, or a missing fault flips `qemu_exit::exit_failure` or
    // times out (fail-loud). Single CPU and a 60-second budget
    // match the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "tairix-test-mem-map-qemu-x86_64",
        binary: "tairix-test-mem-map-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage G1/G2 (`plans/PI.md`): the x86_64 guard-page fault-form
    // vertical — the proof that x86_64, the last `BlockSplit::Pending` port,
    // is now `BlockSplit::Supported`, the sibling of
    // `stack_guard_qemu_{aarch64,riscv64}`. Unlike those self-contained test
    // kernels, x86_64 long-mode bring-up (GDT, the dedicated error-code-aware
    // `#PF` entry, the bump heap) is the production boot pipeline's job, so it
    // boots the real `tairix-kernel` pipeline (like the x86_64 `mem_map`
    // vertical) and does the split / unmap / fault work on `BootCompleted`. It
    // builds a 4 GiB-identity `paging::AddressSpace`, activates it (CR3),
    // `split_block`s the 2 MiB huge page covering a dedicated guard static
    // (reached through its low-identity physical alias), proves the split
    // preserved the mapping (sentinel write/read-back), then `unmap`s +
    // `flush_page`s the single guard page and reads it — the
    // `tairix_arch_x86_64::fault` observer reports the supervisor not-present
    // `#PF` on exactly that page as PASS. A split/unmap failure, a read that
    // does not fault, or a fault elsewhere flips `qemu_exit::exit_failure` or
    // times out (fail-loud). Single CPU and a 60-second budget
    // match the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "tairix-test-stack-guard-qemu-x86_64",
        binary: "tairix-test-stack-guard-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage G3c (`plans/PI.md`): the x86_64 production guard-page
    // fault-form vertical — the proof that an *overrunning kthread* faults
    // synchronously in hardware under the live scheduler, the sibling of
    // `stack_overrun_qemu_aarch64`. Like the x86_64 `stack_guard` vertical it
    // boots the real `tairix-kernel` pipeline (so the GDT, the dedicated
    // error-code-aware `#PF` entry, and the bump heap are installed) and does
    // the work on `BootCompleted`: it builds a 4 GiB-identity
    // `paging::AddressSpace`, activates it (CR3), re-expresses a 2 MiB guard
    // arena at 4 KiB granularity (`prepare_guard_arena`), `unmap`s +
    // `flush_page`s one kthread stack's guard page, builds the live
    // `tairix-kernel-sched-eevdf` `Scheduler` over `X86_64Arch`, and admits a
    // kthread on that arena stack via `spawn_kthread_with_stack`. The
    // kthread's overrun into the unmapped guard page raises a supervisor
    // not-present `#PF`; the `tairix_arch_x86_64::fault` observer confirms the
    // cause + faulting address and reports PASS. A body that returns without
    // faulting (guard regression) drains the loop and flips
    // `qemu_exit::exit_failure`, or times out (fail-loud).
    // Single CPU and a 60-second budget match the other boot-then-do-fixed-
    // work x86_64 tests.
    QemuTest {
        package: "tairix-test-stack-overrun-qemu-x86_64",
        binary: "tairix-test-stack-overrun-qemu-x86_64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage X1 (`plans/PI.md` §X): the x86_64 single-resumable-user-kthread
    // vertical — the first proof that a ring-3 task is admitted as a *resumable*
    // user kthread on x86_64 and cooperatively parks/resumes under the live
    // scheduler, the cross-port sibling of the aarch64 SP2c timeshare (one task;
    // the two-task `gs:8` durable-save hazard is X2). Like the x86_64 `mem_map`
    // vertical it boots the production `tairix-kernel` pipeline (so the GDT user
    // selectors, the TSS, and `syscall`/`IA32_LSTAR` entry are installed). On
    // `BootCompleted` it enables `IA32_EFER.NXE`, builds **one** hardware-
    // isolated user address space from the pure-Rust `tairix-test-el0-yielder`
    // fixture (built PIE + converted to `rxe` by `build.rs`) through the
    // capability-checked, audited `kernel_core::spawn_image`, and admits it via
    // `spawn_user_kthread`. The task's `pre_resume` hook reloads CR3
    // (`paging::activate_user_root`) and repoints the per-CPU `syscall` entry
    // stack at *this* task's own kernel stack (`syscall_entry::set_kernel_rsp0`,
    // the X1 primitive the kthread seam hands the stack top to). The cooperative
    // `step` loop drives it; the dispatch callback maps each `yield`/`exit`
    // `syscall` to `reschedule_current`, so it ping-pongs with the dispatcher on
    // its own kernel stack. PASS once it yielded its full count and exited; a
    // wrong drain count, an unexpected syscall, or a stall flips
    // `qemu_exit::exit_failure` or times out (fail-loud). Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work x86_64
    // tests.
    QemuTest {
        package: "tairix-test-spawn-el0-resume-qemu-x86-64",
        binary: "tairix-test-spawn-el0-resume-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage X2 (`plans/PI.md` §X): the x86_64 two-task EL0 timeshare — the
    // cross-port sibling of the aarch64 SP2c timeshare, and the exerciser for
    // the two X2 structural fixes a concurrent mid-handler park needs: (1) the
    // durable user-`%rsp` save moved onto each task's own kernel-stack frame in
    // `syscall_entry_stub` (a concurrent task's syscall entry no longer
    // clobbers a parked task's saved user stack pointer through the shared
    // per-CPU `gs:8` slot), and (2) the `ContextSwitch::enter`/
    // `leave_cooperative_park` `swapgs` balance around the cooperative
    // mid-handler park in `kernel/core`'s kthread runtime (a parked task's entry
    // `swapgs` is balanced before the dispatcher enters a *different* task, so
    // the next ring-3 entry never observes an unbalanced GS-swap and `#DF`s).
    // Like the x86_64 `mem_map`/X1 verticals it boots the production
    // `tairix-kernel` pipeline (so the GDT user selectors, the TSS, and
    // `syscall`/`IA32_LSTAR` entry are installed). On `BootCompleted` it enables
    // `IA32_EFER.NXE`, builds **two** hardware-isolated user address spaces (two
    // PML4s, one shared frame pool) from the pure-Rust `tairix-test-el0-yielder`
    // fixture (built PIE + converted to `rxe` by `build.rs`) through the
    // capability-checked, audited `kernel_core::spawn_image`, and admits each
    // via `spawn_user_kthread`. Each task's `pre_resume` hook reloads CR3
    // (`paging::activate_user_root`) and repoints the per-CPU `syscall` entry
    // stack at *this* task's own kernel stack (`syscall_entry::set_kernel_rsp0`).
    // The cooperative `step` loop drives them; the dispatch callback maps each
    // `yield`/`exit` `syscall` to `reschedule_current`, so the two tasks
    // ping-pong with the dispatcher on their own kernel stacks. PASS once both
    // yielded their full count and exited; a wrong drain count, an unexpected
    // syscall, or a stall flips `qemu_exit::exit_failure` or times out
    // (fail-loud). Single CPU and a 60-second budget match the
    // other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "tairix-test-spawn-el0-timeshare-qemu-x86-64",
        binary: "tairix-test-spawn-el0-timeshare-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage X3a (`plans/PI.md` §X): the x86_64 PID 1 (`init`) ring-3
    // bring-up vertical — the cross-port sibling of the aarch64
    // `spawn-init-qemu-aarch64` (P6c-3), proving the production x86_64 boot
    // pipeline reaches ring 3 through the real `kernel_main` + `InitSpawn`
    // path (not a test-driven ad-hoc scheduler like X1/X2). It reuses
    // `tairix_kernel::boot`, which now installs the x86_64 PID 1 spawn seam
    // (`init_spawn_x86_64`, via `BootInfo::with_init`) and the COM1 console
    // backing (`BootInfo::with_consoles`); only the audit sink is replaced.
    // After `BootCompleted`, `kernel_main` builds `init`'s ring-3 image
    // through the capability-checked, audited `kernel_core::spawn_image`
    // (emitting `ProcessSpawned`, EventId 4030) and admits it as a resumable
    // user kthread, then drains the run queue. PID 1 `init` writes its gated
    // banner to fd 1 over the COM1 backing, then issues its (audited) `spawn`
    // syscall (EventId 5000; the runtime producer is X3b, so it fails closed)
    // and `exit`s. PASS once a `ProcessSpawned` and an audited `SyscallInvoked`
    // are observed — proving PID 1 reached and executed in ring 3 (the gated
    // banner landed before the audited syscall). A bad image, an entry fault,
    // or an unhandled first `syscall` never emits the audited syscall, so the
    // run times out (fail-loud). Single CPU and a 60-second
    // budget match the other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "tairix-test-spawn-init-qemu-x86-64",
        binary: "tairix-test-spawn-init-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage X3b + X4 follow-on (`plans/PI.md` §X): the x86_64 runtime
    // `spawn` concurrent producer **and** `init` session-supervision vertical —
    // the cross-port sibling of the aarch64 `spawn-session-qemu-aarch64`,
    // proving PID 1 `init` launches a second, hardware-isolated process under
    // the live scheduler and then reaps+relaunches it. It reuses
    // `tairix_kernel::boot`, which installs the runtime `ProcessSpawn` producer
    // + embedded-program registry (`spawn_producer_x86_64`, via
    // `BootInfo::with_spawn`) beside the X3a `with_init` seam and the COM1
    // console backing; only the audit sink is replaced. After `BootCompleted`,
    // `kernel_main` builds PID 1 `init`'s ring-3 image (`ProcessSpawned`,
    // EventId 4030, #1) and drains the run queue. `init` writes its gated
    // banner, then launches the boot services first — `sysinfod`, `netstack`,
    // `devmgr`, `seatmgr` (each an audited `spawn` building a fresh isolated
    // PML4, `ProcessSpawned` #2–#5) — and then the login session
    // `/System/Services/login.app/Run`, whose producer admits it Ready
    // (`ProcessSpawned` #6). `init` `wait`s on the children. This boot binds
    // no root disk, so the in-kernel unlock seam opens the console-0 gate at
    // once (`root_unlock::spawn_if_present`) and login owns console 0: its
    // `users_db_read` fails closed (no database), it wires the deny-all
    // authenticator and draws the `Username:` field, then blocks in
    // `stream_read` on the poll-backed COM1 receive queue. The scripted
    // serial dialogue below types one character past the account format's
    // `MAX_USERNAME_LEN` bound at that field (exactly the aarch64 sibling's
    // final step); the view refuses the over-long line whole
    // (`LengthOutOfRange`), login records the console error and `exit`s
    // fail-closed; `init`'s `wait` reaps it, returns to ring 3, and
    // **relaunches** the session (`ProcessSpawned` #7). PASS keys on **seven**
    // `ProcessSpawned` and **eight** audited `SyscallInvoked` (EventId 5000 —
    // `init`'s four service `spawn`s, the login `spawn`, login's `exit`,
    // `init`'s `wait`, and `init`'s relaunch `spawn`), proving the full
    // `wait`→reap→relaunch supervision cycle on x86_64. The runner fails the
    // run if the guest exits before the scripted `Username:` line was sent, so
    // a login that dies without ever reading the console cannot pass on its
    // relaunch alone. A regression that never builds, runs, reaps, or
    // relaunches the session never reaches the threshold, so the run times out
    // (fail-loud). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "tairix-test-spawn-session-qemu-x86-64",
        binary: "tairix-test-spawn-session-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[("Username:", Duration::ZERO, OVERLONG_USERNAME)],
    },
    // PI Stage P6e-3b prerequisite (`plans/PI.md`): the aarch64 heap-allocator
    // vertical — the proof that the `tairix-rt` `mem_map`-backed
    // `#[global_allocator]` works end to end in an EL0 process on the `virt`
    // board, so a first-party Rust program can use `alloc` (`Box`/`Vec`/
    // `String`) before the shell REPL is wired in. It reads the GICv2 base +
    // timer rate from the embedded `virt` DTB (P3/P4), brings up the EL1
    // vectors + GICv2, and builds **one** hardware-isolated EL0 address space
    // from the pure-Rust `tairix-test-heap` fixture (built PIE + converted to
    // `rxe` by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It **retains** that space live behind a
    // `kernel_core::MemMap` producer backed by
    // `kernel_mem::map_anonymous`/`unmap_anonymous`, admits the program as a
    // resumable user kthread (`spawn_user_kthread`), and routes the
    // program's allocator-issued `mem_map`/`mem_unmap` `svc`s through the
    // producer. The fixture Box-allocates, grows a `Vec` across several pages,
    // reallocates after freeing, verifies every value, and exits 0 — reported
    // as PASS. A non-zero exit, an unexpected syscall, or a fault writes a
    // distinct failure finisher; a stall times out (fail-loud). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-heap-qemu-aarch64",
        binary: "tairix-test-heap-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP6b (`plans/SPAWN.md` §1): the aarch64 `wait` vertical —
    // the proof that a parent process can block on, reap, and read back the
    // exit code of its own child under the live scheduler on the `virt` board.
    // It reads the GICv2 base + timer rate from the embedded `virt` DTB
    // (P3/P4), brings up the EL1 vectors + GICv2, and builds **two** hardware-
    // isolated EL0 address spaces — a child and a parent — from the pure-Rust
    // `tairix-test-wait` fixture (built PIE in both roles + converted to `rxe`
    // by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It records the parent/child link with a
    // `kernel_core::KernelProcessWait` producer, admits each as a resumable
    // user kthread (`spawn_user_kthread`), and routes the child's `exit`
    // and the parent's `wait`/`exit` `svc`s through the producer +
    // `reschedule_current`: the producer parks the parent until the child is
    // reapable, then the kernel copies the reaped exit code out to the parent's
    // `status` pointer. PASS once the parent reaped the child, read back the
    // agreed code, and exited 0; a wrong code, a missing reap, an unexpected
    // syscall, or a stall writes a distinct failure finisher (fail-loud). Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-wait-qemu-aarch64",
        binary: "tairix-test-wait-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // SPAWN Stage SP7b (`plans/SPAWN.md` §1): the aarch64 `signal` vertical —
    // the proof that a parent process can deliver a control signal
    // (`Signal::Terminate`) to its own child under the live scheduler on the
    // `virt` board, plus the `plans/STRESSTEST.md` ST3 signal-observation
    // half. It reads the GICv2 base + timer rate from the embedded `virt`
    // DTB (P3/P4), brings up the EL1 vectors + GICv2, and builds **three**
    // hardware-isolated EL0 address spaces — a child, a parent, and an
    // intake role — from the pure-Rust `tairix-test-signal` fixture (built
    // PIE per role + converted to `rxe` by `build.rs`) through the
    // capability-checked, audited `kernel_core::spawn_image`. It admits the
    // child, threads its scheduler-assigned PID into the parent's startup
    // arguments, records the parent/child link with a
    // `kernel_core::KernelProcessWait` producer, installs a
    // `kernel_core::KernelProcessSignal` producer over that bookkeeping +
    // the live scheduler (and as the console `ForegroundSignal` hook),
    // admits the parent as a resumable user kthread (`spawn_user_kthread`),
    // and routes the fixture `svc`s through the producers +
    // `reschedule_current`: the signal producer terminates the child on the
    // scheduler and records the 128+n status, then the parent reaps it and
    // the kernel copies the status out to the parent's `status` pointer.
    // The ST3 half marks the opted-in intake role foreground on a real
    // `ConsoleDevice` and pushes `^C` bytes through the production cooked
    // line discipline: the first is observed (drained via
    // `signal_intake(Take)`, not fatal), the second is recorded pending, and
    // the third escalates the occupied slot to the default terminate, reaped
    // as 130 under a synthetic supervisor. PASS once the parent script
    // verified **and** the intake saga completed; a wrong status, a missing
    // reap, an unexpected syscall, or a stall writes a distinct failure
    // finisher (fail-loud). Single CPU and a 60-second budget match the
    // other boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-signal-qemu-aarch64",
        binary: "tairix-test-signal-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage X4 (`plans/PI.md`): the x86_64 `wait` vertical — the cross-port
    // sibling of the aarch64 `wait_qemu_aarch64`, proving a parent ring-3
    // process can block on, reap, and read back the exit code of its own child
    // under the live scheduler on x86_64. It boots the production
    // `tairix-kernel` pipeline (so the GDT ring-3 selectors, the TSS, and
    // `syscall`/`IA32_LSTAR` entry are installed) and, on
    // `AuditEvent::BootCompleted`, builds **two** hardware-isolated ring-3
    // address spaces — a child and a parent — from the pure-Rust
    // `tairix-test-wait` fixture (built PIE in both roles + converted to `rxe`
    // by `build.rs`) through the capability-checked, audited
    // `kernel_core::spawn_image`. It records the parent/child link with a
    // `kernel_core::KernelProcessWait` producer, admits each as a resumable
    // user kthread (`spawn_user_kthread`), and routes the child's `exit`
    // and the parent's `wait`/`exit` syscalls through the producer +
    // `reschedule_current`: the producer parks the parent until the child is
    // reapable (exercising the resume-after-cooperative-park return-state path
    // on the x86_64 trap), then the kernel copies the reaped exit code out to
    // the parent's `status` pointer. PASS once the parent reaped the child,
    // read back the agreed code, and exited 0; a wrong code, a missing reap, an
    // unexpected syscall, or a stall writes a distinct failure finisher
    // (fail-loud). Single CPU and a 60-second budget match the
    // other boot-then-do-fixed-work x86_64 tests.
    QemuTest {
        package: "tairix-test-wait-qemu-x86-64",
        binary: "tairix-test-wait-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage RV-X4 (`plans/PI.md` §X tail): the riscv64 `wait` vertical —
    // the cross-port sibling of the aarch64 `wait_qemu_aarch64` / x86_64
    // `wait_qemu_x86_64`, proving a parent U-mode process can block on, reap,
    // and read back the exit code of its own child under the live scheduler on
    // the riscv64 `virt` board. The build script compiles the pure-Rust
    // `tairix-test-wait` fixture twice (child + parent roles, built PIE +
    // converted to `rxe`). On boot it reads the generic-timer rate from the
    // live OpenSBI device tree, installs the trap vector + a dispatch callback,
    // and builds **two** hardware-isolated Sv39 U-mode address spaces — a child
    // and a parent — through the capability-checked, audited
    // `kernel_core::spawn_image`. It records the parent/child link with a
    // `kernel_core::KernelProcessWait` producer, admits each as a resumable
    // user kthread (`spawn_user_kthread`), and routes the child's `exit`
    // and the parent's `wait`/`exit` `ecall`s through the producer +
    // `reschedule_current`: the producer parks the parent until the child is
    // reapable (the RV1 mid-handler-park-safe path), then the kernel copies the
    // reaped exit code out to the parent's `status` pointer. PASS once the
    // parent reaped the child, read back the agreed code, and exited 0; a wrong
    // code, a missing reap, an unexpected syscall, or a stall writes a distinct
    // failure finisher or times out (fail-loud). Single CPU and
    // a 60-second budget match the other boot-then-do-fixed-work riscv64 tests.
    QemuTest {
        package: "tairix-test-wait-qemu-riscv64",
        binary: "tairix-test-wait-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // PI Stage P2 (`plans/PI.md`): `tairix-test-uart-console-qemu-aarch64`
    // is the runtime proof of the board-discovered console. It boots the
    // `virt` board through the arch crate's EL1 trampoline, poisons the
    // console base with a deliberately-wrong value, parses the canonical
    // QEMU `virt` device tree embedded at build time (QEMU's aarch64
    // `-kernel <ELF>` path passes no DTB pointer in `x0`), and calls
    // `console::configure_from_fdt`. It then asserts the base moved off the
    // poison value to the PL011 the tree advertised and logs two lines over
    // the *discovered* console before the ARM semihosting PASS finisher —
    // proving the console MMIO base is now sourced from the firmware device
    // tree, not a compile-time constant, and that writes reach it. (The
    // Pi's specific console base is host-unit-tested against the
    // `raspi_like_arm` fixture and is an on-metal acceptance item: QEMU's
    // `raspi*` models do not model the GPU-firmware DTB hand-off.) Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "tairix-test-uart-console-qemu-aarch64",
        binary: "tairix-test-uart-console-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage W11 (`plans/WIRING.md` §3):
    // `tairix-test-virtio-blk-mmio-aarch64` is the aarch64 `virt`-board
    // MMIO analogue of the riscv64 virtio-blk-mmio vertical — boot the
    // arch crate's EL1 trampoline → build the virtio-MMIO bus from the
    // device tree → provision an `MmioTransport` through the capability-
    // gated `KernelMmioMapper` → arm the device's GICv2 SPI + EL1 IRQ
    // path → mint a `KernelVirtioHost` over a static per-device DMA pool →
    // load the signed virtio-blk `.rxe` → read sector 0 (verify the
    // planted `byte[i] = i mod 256` pattern) → write+read-back sector 1 →
    // ARM semihosting PASS. The device-tail round-trip is the same shared
    // code the riscv64 / x86_64 verticals run. The 2048-sector backing
    // image gives the planted sector-0 pattern plus headroom; single CPU
    // and a 60-second budget match the other boot-then-do-fixed-work
    // tests.
    QemuTest {
        package: "tairix-test-virtio-blk-mmio-aarch64",
        binary: "tairix-test-virtio-blk-mmio-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: Some(2048),
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/PI.md` P11 (root-volume read path at boot):
    // `tairix-test-users-db-qemu-aarch64` reuses the exact virtio-blk-mmio
    // bring-up above, then instead of a raw sector round-trip it mounts
    // the planted users-root arxfs volume through the real driver and
    // drives the kernel's boot-time users-database load
    // (`tairix_kernel_core::load_users_db`) — /System/Security/Users read
    // off the volume through the-checked VFS delegation — then
    // proves the parsed database authenticates the planted account and
    // refuses a wrong password before the ARM semihosting PASS. The
    // backing image is the fixture's users-root volume
    // (`FsDisk::UsersRoot`) — authored by the real arxfs driver — so its
    // geometry is the image's own size. Single CPU and a 60-second
    // budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-users-db-qemu-aarch64",
        binary: "tairix-test-users-db-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::UsersRoot,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/ARCHSUPPORT.md` A2: the x86_64 sibling of the users-database
    // vertical above — the first *live-boot* exercise of the x86_64 boot-time
    // users-database read path over the virtio-**PCI** bus. It reuses the exact
    // shared virtio-PCI bring-up the `root_unlock_login`
    // /`virtio_blk_pci_x86_64` verticals use (PCI walk to the modern virtio-blk
    // function, `PciTransport` provisioning through the capability-gated
    // `KernelMmioMapper`, MSI-X routing) and then drives the *same* shared
    // `users_db_load` tail the aarch64 vertical runs (one definition, generic
    // over the transport) over the same planted users-root arxfs volume
    // (`FsDisk::UsersRoot` — authored by the real arxfs driver): it mounts the
    // plaintext users-root volume, runs `tairix_kernel_core::load_users_db`
    // (/System/Security/Users read off the volume through the
    // capability-checked VFS delegation), and proves the parsed database
    // authenticates the planted account while a wrong password is refused —
    // before the QEMU debug-exit PASS. Unlike the encrypted-root verticals it
    // needs no passphrase (the users-root volume is plaintext), so there is no
    // scripted console dialogue. Single CPU and a 60-second budget match the
    // aarch64 vertical.
    QemuTest {
        package: "tairix-test-users-db-qemu-x86-64",
        binary: "tairix-test-users-db-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::UsersRoot,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/PI.md` P11 Chunk B-2 (root-mount->login): the
    // `tairix-test-root-unlock-login-qemu-aarch64` vertical reuses the
    // exact virtio-blk-mmio bring-up above, then drives the *production*
    // interactive unlock policy
    // (`tairix_kernel::root_mount::unlock_root_disk_interactively`) over a
    // planted **whole-disk** encrypted-root image (`FsDisk::EncryptedRootDisk`
    // — MBR + FAT boot carrying `root.unlock` + a passphrase-derived
    // encrypted ARXFS root): it reads the descriptor off the FAT boot
    // partition, types the fixture passphrase at the prompt over a scripted
    // console, mounts the encrypted root, installs the loaded users database
    // into a `LateUsersDb` cell, and proves the planted account authenticates
    // through the installed cell while a wrong password is refused — before
    // the ARM semihosting PASS. The backing image is the shared whole-disk
    // fixture's bytes — authored by the real in-tree drivers and split by the
    // `root_mount` host tests — so the planted layout and the guest's unlock
    // cannot drift. The root volume uses the format-floor
    // PBKDF2 cost so the per-boot key derivation stays bounded under QEMU TCG;
    // single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-root-unlock-login-qemu-aarch64",
        binary: "tairix-test-root-unlock-login-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/ARCHSUPPORT.md` A2: the x86_64 sibling of the root-mount->login
    // vertical above — the first *live-boot* exercise of the x86_64 unlock
    // policy over the virtio-**PCI** bus. It reuses the exact shared virtio-PCI
    // bring-up the `virtio_blk_pci_x86_64` vertical uses (PCI walk to the
    // modern virtio-blk function, `PciTransport` provisioning through the
    // capability-gated `KernelMmioMapper`, MSI-X routing) and then drives the
    // *same* shared `root_unlock_login` tail the aarch64 vertical runs (one
    // definition, generic over the transport) over the same planted whole-disk
    // encrypted-root image (`FsDisk::EncryptedRootDisk` — MBR + FAT boot
    // carrying `root.unlock` + a passphrase-derived encrypted ARXFS root): it
    // reads the descriptor off the FAT boot partition, types the fixture
    // passphrase over a scripted console, mounts the encrypted root, installs
    // the loaded users database into a `LateUsersDb` cell, and proves the
    // planted account authenticates through the installed cell while a wrong
    // password is refused — before the QEMU debug-exit PASS. Like the aarch64
    // vertical this drives the unlock *policy* directly (a scripted console,
    // not the production NULL-console read half), so it is independent of the
    // A2 kthread-admission console work. The `/System` bundles the image plants
    // are cross-compiled for x86_64 (`stores_for`); the root volume uses the
    // format-floor PBKDF2 cost so the per-boot key derivation stays bounded
    // under QEMU TCG; single CPU and a 60-second budget match the aarch64
    // vertical.
    QemuTest {
        package: "tairix-test-root-unlock-login-qemu-x86-64",
        binary: "tairix-test-root-unlock-login-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/PI.md` P11 Chunk B-2 INCREMENT (2): the
    // `tairix-test-root-unlock-admission-qemu-aarch64` vertical boots the
    // *production* aarch64 `tairix-kernel` pipeline (`boot_aarch64::boot`)
    // on the `virt` board with the same planted whole-disk encrypted-root
    // image (`FsDisk::EncryptedRootDisk`) attached as a virtio-blk-mmio
    // device — but, unlike `root_unlock_login` (which drives the unlock
    // *policy* directly), it proves the *kthread admission* path: the
    // bootstrap-floor virtio-MMIO bus enumeration
    // (`hwdiscovery::observe_virtio_mmio_block_devices`) probes the slot
    // and binds the virtio-blk root, the init seam admits the in-kernel
    // unlock kthread (`unlock_service::spawn_if_present`), and the kthread
    // brings the device up over the production device-IRQ path, prompts at
    // `ARXFS passphrase: `, reads the typed passphrase, mounts the encrypted
    // `ARXFS` root, and installs the users database into `LATE_USERS_DB`.
    // The kernel-side audit sink reports PASS through the ARM semihosting
    // finisher the instant it sees the unlock-service install message
    // (`EventId(4139)`) — the witness that the kthread-admission path
    // mounted the root end to end. The runner types the fixture passphrase
    // (verified against the shared fixture at compile time, `is_line_of`)
    // once the prompt appears; the database *content* authenticating
    // `root`/`root` is proven by `root_unlock_login`, and the per-console
    // `login` authenticating end to end into a real shell session is the
    // session-ceiling vertical's job (below), so both are out of this
    // vertical's scope.
    //
    // The secret prompt's two timed-wake behaviours — the `[input active...]`
    // animation advancing on the tickless one-shot, and the anti-brute-force
    // delay park after a wrong attempt expiring on it — are proven
    // deterministically by the `users_db_wait`/`irq_wait` epilogue host unit
    // tests (which pin the one-shot staying armed for a console waiter's
    // deadline across another queue's wait finishing) and the `console`
    // secret-feedback tick tests. This wall-clock-bounded QEMU run therefore
    // does *not* re-assert them: doing so keyed the run on guest-time console
    // delays (a per-second animation tick, then a multi-second wrong-attempt
    // park) that ballooned under parallel TCG saturation and blew the budget —
    // the load-dependent flake the charter forbids papering over with a bigger
    // ceiling. Typing the correct passphrase straight away keeps the vertical's
    // timing bounded by real work (boot + two bounded PBKDF2 derivations), not
    // by guest-time waits.
    //
    // The 120-second budget matches the session-ceiling vertical below,
    // which boots the same pipeline and unlocks the same disk before doing
    // strictly *more* (a full login + multi-command shell session); a
    // ceiling, never a wait — the run ends the moment the audit witness
    // fires. Single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-root-unlock-admission-qemu-aarch64",
        binary: "tairix-test-root-unlock-admission-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE)],
    },
    // `plans/NEW-SUPERVISOR.md` §7: the pre-boot **Supervisor** ESC vertical.
    // `tairix-test-supervisor-esc-qemu-aarch64` boots the *production* aarch64
    // pipeline over the same planted whole-disk encrypted-root image the
    // admission vertical above uses (`FsDisk::EncryptedRootDisk`), so the real
    // `SupervisorHost` is installed and the byte-exact ESC boot screen is
    // drawn. The serial script then walks the frozen boot-screen states in
    // order — reaching each marker *is* the byte-exact assertion, and the run
    // fails loud if the guest exits before every step was sent:
    //
    //  1. `[Press ESC for supervisor]` (`root_mount::SUPERVISOR_ANNOUNCE`) →
    //     type a lone `ESC` (`0x1b`). The window disambiguates it from a CSI
    //     editor sequence with a bounded re-poll; a lone `ESC` drops into the
    //     REPL. (If the 2 s window elapses before the byte lands, the same
    //     `ESC` is read as the first byte of the passphrase line and still
    //     drops in via `PassphraseReadError::Escape` — the `Supervisor` banner
    //     appears either way, so this step is race-robust.)
    //  2. `Supervisor` (`root_mount::SUPERVISOR_ENTER_BANNER`, the collapse to
    //     `ARXFS` then the `Supervisor` banner) → type `help`, exercising a
    //     real command at the `*` prompt.
    //  3. `commands:` (the host-independent `Supervisor commands:` header the
    //     dispatcher's `help` renders) → type `continue`, leaving the REPL.
    //  4. `ARXFS passphrase: ` (`root_mount::FS_UNLOCK_PROMPT`, redrawn *after*
    //     the REPL exited) → type the fixture passphrase, proving a Supervisor
    //     session is transparent to boot.
    //
    // The kernel-side audit sink reports PASS through the ARM semihosting
    // finisher the instant it sees the unlock-service install message
    // (`EventId(4139)`) — which can only follow `continue` resuming the normal
    // unlock and that unlock mounting the encrypted `ARXFS` root. A run where
    // ESC never enters the REPL, `continue` never resumes, or the resumed
    // unlock never mounts never reaches the message and the harness times out.
    // The database *content* authenticating `root`/`root` is proven by
    // `root_unlock_login`, so this vertical keys on the install witness. 120 s
    // matches the admission vertical it mirrors; single CPU like the other
    // full-boot verticals.
    QemuTest {
        package: "tairix-test-supervisor-esc-qemu-aarch64",
        binary: "tairix-test-supervisor-esc-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: SUPERVISOR_ESC_SCRIPT,
    },
    // `plans/NEW-SUPERVISOR.md` §7 (item 1): the pre-boot **Supervisor**
    // entered at the *live passphrase prompt* rather than the announcement
    // window. It reuses the very same production `tairix-kernel` bin as the
    // Supervisor ESC vertical above — the guest is byte-identical; only the
    // host-side serial script differs — so there is no duplicated bin. The
    // runner disambiguates the two enrolments' planted backing images by their
    // `TESTS` index (`sidecar_path`), so the shared binary is safe under the
    // concurrent matrix. The `SUPERVISOR_ESC_AT_PROMPT_SCRIPT` waits for the
    // redrawn `ARXFS passphrase: ` prompt to appear (which only happens after
    // the 2 s window elapses untouched), then types a lone `ESC` as the line's
    // first byte, exercising `read_passphrase_line`'s
    // `PassphraseReadError::Escape` drop — the entry point the
    // window-race-robust `SUPERVISOR_ESC_SCRIPT` reaches only incidentally. It
    // then drives `help` → `continue` → passphrase, so PASS keys on the same
    // install witness (`EventId(4139)`), proving the passphrase-prompt drop is
    // equally transparent to boot. 120 s and single CPU match the ESC vertical
    // it mirrors.
    QemuTest {
        package: "tairix-test-supervisor-esc-qemu-aarch64",
        binary: "tairix-test-supervisor-esc-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: SUPERVISOR_ESC_AT_PROMPT_SCRIPT,
    },
    // `plans/NEW-SUPERVISOR.md` §7 (item 1): the pre-boot **Supervisor**
    // `mount`-from-REPL path — the Supervisor performing the *real* root unlock
    // itself, distinct from `continue` resuming the normal unlock. It reuses
    // the same production bin as the two verticals above (no duplicated bin,
    // `TESTS`-index-disambiguated backing image). The `SUPERVISOR_MOUNT_SCRIPT`
    // enters at the announcement window, then types `mount` at the `*` prompt;
    // `cmd_mount` prints its own `ARXFS passphrase: ` and the script types the
    // fixture passphrase, so the Supervisor's `SupervisorHost::mount` runs
    // `mount_root_disk_and_load_users`
    // + `finish_install` and returns `SupervisorExit::Mounted`. The interactive
    // unlock then resolves to `Installed` and logs the install witness
    // (`EventId(4139)`) — the PASS the guest sink keys on — with no second
    // prompt, proving the in-REPL mount mounts the encrypted `ARXFS` root and
    // boots. 120 s and single CPU match the ESC vertical it mirrors.
    QemuTest {
        package: "tairix-test-supervisor-esc-qemu-aarch64",
        binary: "tairix-test-supervisor-esc-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: SUPERVISOR_MOUNT_SCRIPT,
    },
    // `plans/OPEN-DEFECTS.md` D7 + D8: the x86_64 disk-completion-interrupt
    // and two-kthread-admission regression. It boots the *production* x86_64
    // `tairix-kernel` pipeline (`boot_x86_64::boot`) with the planted
    // whole-disk encrypted-root image (`FsDisk::EncryptedRootDisk`) attached
    // as a virtio-blk-**pci** device. Production PCI discovery binds the
    // virtio-blk root, the init seam admits the in-kernel root-unlock kthread
    // (`unlock_service::spawn_if_present`), and that kthread brings the
    // device up over a **dedicated MSI-X vector**, mounts the read-only
    // `/System` volume, then unlocks the encrypted user-data root at the
    // scripted `ARXFS passphrase:` prompt and installs the users database.
    // Reaching the install requires the disk's completion MSI-X to be
    // delivered on its dedicated vector and to wake the scheduler-parked
    // bring-up over thousands of block reads, with a device IRQ preempting
    // ring-3 services without corrupting the per-CPU GS state — the exact
    // path the D7 triple fault broke (the external-IRQ ISR located the CPU
    // frame at the wrong stack offset and ran an unbalanced `swapgs`; the MSI
    // shared an IO-APIC pin's vector). It is the two-kthread admission path
    // (the interactive-unlock kthread and the driver-store serve kthread
    // sharing one boot disk through the pressure-governed
    // `BlockCache`/`SharedBlock`) that D8 reported stalling with no forward
    // progress; that stall is resolved (a consequence of the pre-fix
    // kernel-heap OOM/pressure condition the `kernel/mem` `MAX_ORDER` growth +
    // fallible-reserve read fix removed), and this witness is the regression
    // that keeps the admission install terminating. The guest audit sink
    // reports PASS through the `isa-debug-exit` device the instant it sees
    // `USERS_DB_INSTALLED_MESSAGE`; a D7 regression triple-faults on the
    // first disk read (before any mount) and a D8 regression stalls before
    // the install — both fail the run loud. 120 s matches the aarch64
    // admission vertical; single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-root-unlock-admission-qemu-x86-64",
        binary: "tairix-test-root-unlock-admission-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE)],
    },
    // `plans/NEW-SUPERVISOR.md` §7 / `plans/ARCHSUPPORT.md`: the x86_64 sibling
    // of the aarch64 pre-boot **Supervisor** ESC vertical above.
    // `tairix-test-supervisor-esc-qemu-x86-64` boots the *production* x86_64
    // `tairix-kernel` pipeline (`boot_x86_64::boot`) over the same planted
    // whole-disk encrypted-root image the x86_64 admission vertical uses
    // (`FsDisk::EncryptedRootDisk`), so the real `SupervisorHost` is installed
    // and the byte-exact ESC boot screen is drawn on COM1. It runs the shared
    // `SUPERVISOR_ESC_SCRIPT` — the one definition of the frozen boot-screen
    // contract, never a per-arch copy — walking the same ordered states as the
    // aarch64 sibling: `ESC` at the announcement drops into the REPL, `help`
    // renders the `Supervisor commands:` header, `continue` leaves the REPL,
    // and the fixture passphrase then unlocks the redrawn `ARXFS passphrase: `
    // prompt. The guest audit sink reports PASS through the `isa-debug-exit`
    // device the instant it sees the unlock-service install message
    // (`EventId(4139)`) — which can only follow `continue` resuming the normal
    // unlock and that unlock mounting the encrypted `ARXFS` root, proving a
    // Supervisor session is transparent to boot on x86_64. A run where ESC
    // never enters the REPL, `continue` never resumes, or the resumed unlock
    // never mounts never reaches the message and the harness times out. 120 s
    // matches the aarch64 sibling; single CPU like the other full-boot
    // verticals.
    QemuTest {
        package: "tairix-test-supervisor-esc-qemu-x86-64",
        binary: "tairix-test-supervisor-esc-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: SUPERVISOR_ESC_SCRIPT,
    },
    // `plans/CAPABILITY_USE.md` CU3: the session-ceiling acceptance vertical.
    // `tairix-test-session-ceiling-qemu-aarch64` boots the *production*
    // aarch64 pipeline with the planted encrypted-root disk, unlocks the
    // root at the passphrase prompt, authenticates `root`/`root` at the
    // console login (the planted account's grant is the shared
    // administrator ceiling, `tairix_users::administrator_ceiling` — the
    // same set `tools/mkimage`'s profile-keyed seeding gives a debug image),
    // and drives the spawned shell through a real session: `cd` into the
    // account's home (`CAP_FS_ACCESS` — the B3 regression), `pwd` proving
    // the move, typing the bare command word `ps` — resolved through the
    // shell's system-app-store search (`plans/APPS.md` §8) to
    // `/System/Commands/ps.app/Run` and spawned under `CAP_PROC_SPAWN` — and
    // seeing its process-list header, `man man` rendering the store-shipped
    // Help document end to end (`plans/APPS.md` §7 — resolution, the
    // `fs_*` read of the read-only /System volume, and the `lib/help`
    // render all in one exchange), `ls /System/Commands` listing the command
    // store through the `fs_stat`/`fs_readdir` syscalls (`plans/APPS.md`
    // deliverable 6 — the listing must show `man.app`, an entry only a real
    // directory read produces), then the negative half — a `ulimit` bound
    // pair is *lowered* (ungated; both bounds, since the default soft bound
    // is unlimited and a soft bound may never exceed its hard bound) and
    // the hard bound is then *raised*: the raise needs
    // `CAP_RLIMIT_RAISE`, which the ceiling carries but the shell's
    // session-baseline manifest does not request, so the effective
    // `manifest ∩ ceiling` set lacks it and the kernel refuses the
    // `rlimit_set` with `PermissionDenied` (an administrator account never
    // widens a program past its own manifest). Each line is typed only
    // after its marker appeared (`pwd`'s output and the shell's denial
    // message are themselves markers), and the guest audit sink reports
    // PASS only once the audited `rlimit_set` rejection has been seen
    // *and* the scripted `exit` that follows it dispatches — so the denial
    // provably reached the transcript before the run ended. A 120-second
    // budget covers boot + bounded PBKDF2 + the multi-exchange dialogue on
    // QEMU TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-session-ceiling-qemu-aarch64",
        binary: "tairix-test-session-ceiling-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            // The full-screen login view paints `Username:` once and the
            // minimal-diff renderer then repaints only the changed label
            // cells (`Password` over it), so the anchors are the labels
            // without their trailing blanks.
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            // The default prompt is `\u@\h \w% ` (tairix_elsh env.rs): login
            // spawns the shell as `root` with HOME=/Users/root, and the shell
            // defaults HOSTNAME to `tairix`, so at home the prompt renders
            // `root@tairix ~% ` (the home directory abbreviated to `~`).
            ("root@tairix ~% ", Duration::ZERO, "cd /Users/root\n"),
            ("root@tairix ~% ", Duration::ZERO, "pwd\n"),
            ("/Users/root", Duration::ZERO, "ps\n"),
            // The typed word `--bogus` must reach the child as `argv[1]`
            // through the spawn startup-strings block: `ps` refuses the
            // unknown option and prints its usage line — output that a
            // child running under its registered default argv could never
            // produce — proving caller-supplied arguments arrive end to end.
            ("PID  PPID", Duration::ZERO, "ps --bogus\n"),
            // `man man` (plans/APPS.md §7): the spawned tool resolves its
            // own bundle through the shared store-then-PATH policy, reads
            // `/System/Commands/man.app/Help/en-US/man.md` off the mounted
            // read-only /System volume through the `fs_*` syscalls, and
            // streams the rendered page (a serial console attests no
            // geometry, so no pager prompt). `SEE ALSO` is the page's final
            // section heading — seeing it proves the whole document arrived.
            ("usage: ps", Duration::ZERO, "man man\n"),
            // `ls /System/Commands` (plans/APPS.md deliverable 6): the spawned
            // tool stats the operand and reads the directory through the
            // `fs_stat`/`fs_readdir` syscalls under its own manifest's
            // `CAP_FS_ACCESS`. `man.app` in the output is an entry only a
            // real directory read of the mounted read-only /System volume
            // produces.
            ("SEE ALSO", Duration::ZERO, "ls /System/Commands\n"),
            ("man.app", Duration::ZERO, "ulimit processes 1000\n"),
            (
                "root@tairix ~% ",
                Duration::ZERO,
                "ulimit -H processes 2000\n",
            ),
            (
                "cannot raise hard limit (requires CAP_RLIMIT_RAISE)",
                Duration::ZERO,
                "exit\n",
            ),
        ],
    },
    // `plans/ALIAS.md` §6.2: the value-pipe vertical.
    // `tairix-test-value-pipe-qemu-aarch64` boots the *production* aarch64
    // pipeline with the planted encrypted-root disk, unlocks the root, logs in
    // as `root`, and reads three value-backed references with `cat` — whose own
    // manifest holds no `CAP_SYSINFO_*`, which is the point, since the shell
    // resolves them and hands over a pipe. `info:system/machine-id` is ungated;
    // `info:mem/page-size` is gated on `CAP_SYSINFO_KERNEL`, so `4096` proves
    // `SHELL_MANIFEST ∩ administrator_ceiling()` armed it; and
    // `info:mem/physical` — the reference from the original defect report — is
    // asserted by `&& echo` on `cat`'s exit status, its value being
    // machine-dependent, and read twice: once through the shell's pipe and once
    // as a bare operand `cat` resolves itself under its own manifest, the two
    // being separate readers. Then the negative half: `ls > info:mem/physical`
    // still reaches `resource_open` and is refused, because a value-backed
    // resource is changed by a typed service command.
    //
    // The guest sink passes only once that audited rejection has been seen
    // *and* the scripted `exit` after it dispatches, so the refusal provably
    // reached the transcript; the positive steps are asserted by the runner's
    // "every marker appeared and every line was sent" rule, which is what
    // makes a silently-empty value pipe fail. A 120-second budget matches the
    // sibling session-ceiling vertical; single CPU like the other full-boot
    // verticals.
    QemuTest {
        package: "tairix-test-value-pipe-qemu-aarch64",
        binary: "tairix-test-value-pipe-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            // An **ungated** value first (the `SYSTEM_IDENTITY` query
            // declares no capability), so a failure here isolates the value
            // pipe itself from the capability intersection.
            (
                "root@tairix ~% ",
                Duration::ZERO,
                "cat < info:system/machine-id\n",
            ),
            // Then a **gated** one on the `KERNEL_MEMORY_STATS` query: its
            // arrival proves `manifest ∩ ceiling` armed `CAP_SYSINFO_KERNEL`
            // for the shell and that `sysinfod` served the read.
            (
                UNPROVISIONED_MACHINE_ID_MARKER,
                Duration::ZERO,
                "cat < info:mem/page-size\n",
            ),
            // The 4 KiB page size of the `virt` board — deterministic, and
            // the value the previous line's read actually produced.
            ("4096", Duration::ZERO, VALUE_PIPE_PHYSICAL_LINE),
            // The same reference as a bare operand, which `cat` resolves
            // itself under its own manifest's `CAP_SYSINFO_KERNEL` — the
            // spelling from the original defect report.
            (
                VALUE_PIPE_PHYSICAL_MARKER,
                Duration::ZERO,
                VALUE_OPERAND_PHYSICAL_LINE,
            ),
            // The write direction is still the kernel's refusal.
            (
                VALUE_OPERAND_PHYSICAL_MARKER,
                Duration::ZERO,
                "ls > info:mem/physical\n",
            ),
            (VALUE_PIPE_WRITE_REFUSED_MARKER, Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/APPS.md` "Immediate work" I2/I3: the memory-stability vertical.
    // `tairix-test-memsoak-qemu-aarch64` boots the *production* aarch64
    // pipeline with the encrypted-root disk that carries the standard signed
    // store bundles **plus** the test-only `memsoak` fixture bundle
    // (`FsDisk::MemsoakRootDisk`), unlocks the root, authenticates
    // `root`/`root` at the console login, and types the bare word `memsoak` at
    // the shell. The fixture warms up, samples `KernelMemoryStats.free_bytes`
    // through sysinfod (its manifest's `CAP_SYSINFO_KERNEL`, enforced against
    // the kernel-attested origin), drives 32 measured cycles — each a
    // spawn+reap of `true.app` (the full teardown path), a timed `stream_read`
    // whose bound elapses (the `top -d0` refresh park), a self-scoped
    // process-list walk, and a live sysinfod IPC round trip — then requires the
    // final sample to equal the baseline **exactly**. On a stable soak it
    // prints `MEMSOAK PASS baseline=… final=…` and exits 0; on any failure it
    // prints the reason and parks forever (it never exits), so the run times
    // out fail-loud with the numbers in the transcript. The guest audit sink
    // arms on the fixture's audited `exit` (`sc=exit`, `comm=memsoak`) and
    // reports PASS on the next audited `exit` — the shell's, typed only after
    // the `MEMSOAK PASS` marker appeared — so the numeric verdict provably
    // reached the transcript before the run ended (the session-ceiling
    // arm-then-exit discipline). A 300-second budget covers boot + bounded
    // PBKDF2 + the 36-cycle soak on QEMU TCG (each cycle is a full spawn/reap
    // plus two sysinfod round trips, on top of the session-ceiling verticals'
    // 120-second boot-and-dialogue baseline); single CPU like the other
    // full-boot verticals.
    QemuTest {
        package: "tairix-test-memsoak-qemu-aarch64",
        binary: "tairix-test-memsoak-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::MemsoakRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            ("root@tairix ~% ", Duration::ZERO, "memsoak\n"),
            (MEMSOAK_PASS_PREFIX, Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/NETWORK.md` N5c: the stream-socket (TCP) vertical.
    // `tairix-test-netstack-stream-qemu-aarch64` boots the *production*
    // aarch64 pipeline with the encrypted-root disk that carries the standard
    // signed store bundles **plus** the signed virtio-net driver bundle and
    // the test-only `tcpecho` fixture bundle (`FsDisk::StreamRootDisk`), with
    // a virtio-net device attached and the harness-side passive TCP echo peer
    // on its `dgram` netdev (`NetPeerMode::V6TcpEcho`). It unlocks the root,
    // authenticates `root`/`root` at the console login, and types the bare
    // word `tcpecho` at the shell. The client opens a `SocketType::Stream`
    // socket (its manifest's `CAP_NET`, enforced by the netstack socket
    // dispatcher against the kernel-attested origin), connects to the peer's
    // echo server over the shared IPv6 link-local wire — retrying through the
    // boot window while the NIC driver is still autoloading — streams a fixed
    // deterministic 32 KiB run, and verifies the peer echoes every byte back
    // in order. The peer injects bounded frame loss, so a pass proves RFC 9293
    // retransmission carried the stream across the two-process boundary. On a
    // fully verified transfer the client prints `TCPECHO PASS …` and exits 0;
    // on any shortfall it prints the reason and parks forever (it never
    // exits), so the run times out fail-loud with the reason in the transcript.
    // The guest audit sink arms on the client's audited `exit` (`sc=exit`,
    // `comm=tcpecho`) and reports PASS on the next audited `exit` — the
    // shell's, typed only after the `TCPECHO PASS` marker appeared — so the
    // report provably reached the transcript before the run ended (the
    // session-ceiling arm-then-exit discipline). The harness additionally
    // requires the echo peer to report the whole transfer received and echoed,
    // so neither side can pass alone. A 300-second budget covers boot +
    // bounded PBKDF2 + the two-process net bring-up + the loss-recovered
    // transfer on QEMU TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-netstack-stream-qemu-aarch64",
        binary: "tairix-test-netstack-stream-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6TcpEcho,
        ramfb: false,
        fs_disk: FsDisk::StreamRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            ("root@tairix ~% ", Duration::ZERO, "tcpecho\n"),
            (TCPECHO_PASS_PREFIX, Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/NETWORK.md` N13: the RFC 3168 ECN vertical.
    // `tairix-test-netstack-ecn-qemu-aarch64` boots the *production* aarch64
    // pipeline with the encrypted-root disk that carries the standard signed
    // store bundles **plus** the signed virtio-net driver bundle and the
    // test-only `tcpecho` fixture bundle **plus** a planted `system.conf` that
    // turns `net.tcp.ecn` on stack-wide (`FsDisk::EcnRootDisk`), with a
    // virtio-net device attached and the harness-side ECN-verifying passive
    // TCP echo peer on its `dgram` netdev (`NetPeerMode::V6TcpEchoEcn`). It is
    // the stream vertical with ECN switched on end to end: `devmgr` reads the
    // planted store pre-unlock and delivers `tcp_ecn = true` to `netstack`, so
    // the guest `tcpecho` client's connection negotiates ECN. The same 32 KiB
    // deterministic transfer runs, but the host peer additionally verifies RFC
    // 3168 on the live wire — the guest's SYN offers ECN (ECE+CWR), the
    // guest's data segments carry ECT(0), and, after the peer echoes ECE for
    // an injected congestion mark, the guest reduces its window and sets CWR
    // on a subsequent segment. The peer's verdict requires all three plus the
    // full echoed transfer, so a stack that ignored the toggle (never
    // negotiating, marking, or responding) fails the run loud even though the
    // bytes still flow. The guest PASS keys exactly like the stream vertical
    // (client `exit` arms, the shell's next `exit` reports), and the harness
    // requires the peer's ECN verdict too, so neither side can pass alone. A
    // 300-second budget covers boot + bounded PBKDF2 + the two-process net
    // bring-up + the transfer + the ECN choreography on QEMU TCG; single CPU.
    QemuTest {
        package: "tairix-test-netstack-ecn-qemu-aarch64",
        binary: "tairix-test-netstack-ecn-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6TcpEchoEcn,
        ramfb: false,
        fs_disk: FsDisk::EcnRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            ("root@tairix ~% ", Duration::ZERO, "tcpecho\n"),
            (TCPECHO_PASS_PREFIX, Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/NETWORK.md` N6b-2-β-2: the TCP-**listener** vertical — the
    // role-swapped mirror of the stream vertical above.
    // `tairix-test-netstack-listener-qemu-aarch64` boots the *production*
    // aarch64 pipeline with the encrypted-root disk that carries the standard
    // signed store bundles **plus** the signed virtio-net driver bundle and
    // the test-only `tcpserve` fixture bundle (`FsDisk::ListenRootDisk`), with
    // a virtio-net device attached and the harness-side active TCP client peer
    // on its `dgram` netdev (`NetPeerMode::V6TcpConnect`). It unlocks the root,
    // authenticates `root`/`root` at the console login, and types the bare
    // word `tcpserve` at the shell. The server opens a `SocketType::Stream`
    // socket, binds the well-known (privileged) port — exercising
    // `CAP_NET_BIND_PRIVILEGED`, which the administrator ceiling now grants and
    // the netstack `Bind` gate enforces against the kernel-attested origin —
    // listens, accepts the host client's connection over the shared IPv6
    // link-local wire, echoes every received byte back, and verifies the
    // received run. The peer injects bounded frame loss, so a pass proves RFC
    // 9293 retransmission carried the stream both ways across the two-process
    // boundary. On a fully verified exchange the server prints `TCPSERVE PASS …`
    // and exits 0; on any shortfall it prints the reason and parks forever (it
    // never exits), so the run times out fail-loud with the reason in the
    // transcript. The guest audit sink arms on the server's audited `exit`
    // (`sc=exit`, `comm=tcpserve`) and reports PASS on the next audited `exit`
    // — the shell's, typed only after the `TCPSERVE PASS` marker appeared — so
    // the report provably reached the transcript before the run ended (the
    // session-ceiling arm-then-exit discipline). The harness additionally
    // requires the client peer to report the whole transfer echoed and
    // verified, so neither side can pass alone. A 300-second budget covers
    // boot + bounded PBKDF2 + the two-process net bring-up + the loss-recovered
    // transfer on QEMU TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-netstack-listener-qemu-aarch64",
        binary: "tairix-test-netstack-listener-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6TcpConnect,
        ramfb: false,
        fs_disk: FsDisk::ListenRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            ("root@tairix ~% ", Duration::ZERO, "tcpserve\n"),
            (TCPSERVE_PASS_PREFIX, Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/NETWORK.md` N16b: the connection-exhaustion vertical — the
    // listener vertical above, run against a *hostile* peer.
    // `tairix-test-netstack-synflood-qemu-aarch64` reuses that vertical's disk
    // and guest fixture unchanged (`FsDisk::ListenRootDisk`, the `tcpserve`
    // server): what is under test is the stack's behaviour under connection
    // exhaustion, not a new guest program. The peer
    // (`NetPeerMode::V6TcpFlood`) first fills the listener's bounded half-open
    // backlog with SYNs from distinct source ports that it never answers —
    // exactly a SYN flood — and only then opens one real connection, whose SYN
    // therefore meets a full backlog and can be admitted only by falling back
    // to a stateless RFC 4987 cookie (the server ISN a keyed MAC over the
    // 4-tuple, the connection reconstructed from the returning ACK with no
    // per-connection state held meanwhile). It then streams the whole
    // deterministic transfer over that cookie-admitted connection and verifies
    // the guest echoes every byte back. A stack whose backlog grew without
    // bound, or which refused the connection once the backlog filled, cannot
    // complete the transfer.
    // Three independent witnesses gate the PASS. The serial script requires
    // `SYN_COOKIES_MARKER` — the `netstack` `SYN_COOKIES_ENGAGED` audit
    // message — *before* it will await the fixture's PASS marker, which is what
    // distinguishes a cookie-admitted connection from an ordinary one (a run
    // where the flood never landed would otherwise look identical to a pass);
    // that step types nothing, it only orders the run. The guest fixture's
    // audited `exit` then witnesses a verified exchange (a shortfall parks
    // forever), and the shell's scripted `exit`, typed only after the
    // `TCPSERVE PASS` marker appeared, completes the arm-then-exit chain. The
    // harness additionally requires the flood peer to report *both* the whole
    // flood sent and the whole transfer echoed back verified. No side passes
    // alone. A 300-second budget matches the listener vertical it mirrors;
    // single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-netstack-synflood-qemu-aarch64",
        binary: "tairix-test-netstack-synflood-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6TcpFlood,
        ramfb: false,
        fs_disk: FsDisk::ListenRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            ("root@tairix ~% ", Duration::ZERO, "tcpserve\n"),
            // Expect-only: require the cookie brake to have engaged before
            // the transfer's PASS marker is awaited. Types nothing.
            (SYN_COOKIES_MARKER, Duration::ZERO, ""),
            (TCPSERVE_PASS_PREFIX, Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/NETWORK.md` N8b-2b-β: the ICMP-echo (`ping`) vertical.
    // `tairix-test-netstack-ping-qemu-aarch64` boots the *production* aarch64
    // pipeline with the encrypted-root disk that carries the **standard**
    // signed store bundles (so the real `ping` command bundle is present)
    // **plus** the signed virtio-net driver bundle (`FsDisk::NetToolRootDisk`),
    // with a virtio-net device attached and the harness-side passive ICMP
    // echo responder on its `dgram` netdev (`NetPeerMode::V6PingResponder`).
    // It unlocks the root, authenticates `root`/`root` at the console login,
    // and runs `ping -c 3 fe80::2` at the shell — the peer's EUI-64-free
    // link-local formed from `tairix_test_netstack_wire::PEER_IID`. The `ping`
    // tool opens an ICMP-echo socket (its manifest's `CAP_NET`+`CAP_NET_RAW`,
    // enforced by the netstack socket dispatcher against the kernel-attested
    // origin), resolves the peer over ND, and sends three echo requests over
    // the shared IPv6 link-local wire — retrying through the boot window while
    // the NIC driver is still autoloading. The peer answers each, so `ping`
    // prints a `… icmp_seq=… time=… ms` reply line per reply. The serial
    // `exit` step keys on `icmp_seq=`, which the tool prints **only** on a
    // genuinely received reply, so an unanswered run never reaches it and
    // times out fail-loud with the transcript. The guest audit sink arms on
    // `ping`'s audited `exit` (`sc=exit`, `comm=ping`) and reports PASS on the
    // next audited `exit` — the shell's, typed only after the `icmp_seq=`
    // marker appeared — so the received reply provably reached the transcript
    // before the run ended (the session-ceiling arm-then-exit discipline). The
    // harness additionally requires the responder to report at least one
    // served echo request, so neither side can pass alone (a guest that never
    // reached the peer, or a peer that never answered, both fail). A
    // 300-second budget covers boot + bounded PBKDF2 + the two-process net
    // bring-up on QEMU TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-netstack-ping-qemu-aarch64",
        binary: "tairix-test-netstack-ping-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6PingResponder,
        ramfb: false,
        fs_disk: FsDisk::NetToolRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            ("root@tairix ~% ", Duration::ZERO, PING_COMMAND_LINE),
            (PING_REPLY_MARKER, Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/TELNET.md`: the live telnet vertical.
    // `tairix-test-netstack-telnet-qemu-aarch64` boots the *production*
    // aarch64 pipeline against the shared net-tool disk — the standard signed
    // store bundles (so the real `telnet` command bundle is present) plus the
    // signed virtio-net driver (`FsDisk::NetToolRootDisk`) — with a
    // virtio-net device attached and the harness-side **telnet server** peer
    // on its `dgram` netdev (`NetPeerMode::V6TelnetServer`).
    //
    // It unlocks the root, authenticates `root`/`root` at the console login,
    // and types `telnet fe80::2` at the shell — the peer's link-local formed
    // from `tairix_test_netstack_wire::PEER_IID`, with **no** port operand, so
    // the tool's own default port is what is exercised. The store-then-`PATH`
    // resolution finds `/System/Commands/telnet.app/Run`, the disk-backed
    // spawn path verifies the signed bundle, and the tool runs with
    // `manifest ∩ administrator-ceiling` authority (`CAP_NET` for the stream
    // socket, `CAP_CONSOLE_READ` for the raw-mode relay). It retries `connect`
    // through the boot window while the NIC driver is still autoloading.
    //
    // The serial script's three gates are what make the run a proof rather
    // than a reachability check:
    //
    // * `wire::TELNET_BANNER` — the peer sends it **only** after the client
    //   accepted `DO SUPPRESS GO AHEAD`, named its terminal type, reported its
    //   window over NAWS, agreed `WILL LINEMODE`, stated a `MODE` mask and
    //   exported its SLC table. A client that connected but ignored the
    //   negotiation never sees it, so the script never types the next line.
    // * `wire::TELNET_ECHO` — the peer's upper-cased answer to the probe line
    //   the script then types. It proves the bytes made a full round trip
    //   through the telnet data path in both directions; the client's own
    //   local echo of the probe is lower case, so it cannot be mistaken for
    //   the answer.
    // * `TELNET_QUIT_SEQUENCE` — the default escape character `^]` followed by
    //   the interpreter's `quit`, so the escape recognition and the `telnet>`
    //   command interpreter are exercised live and the tool exits cleanly of
    //   its own accord rather than being killed.
    //
    // The guest audit sink arms on `telnet`'s own audited `exit`
    // (`comm=telnet`) and reports PASS on the **next** audited `exit` — the
    // shell's, typed only after the echo marker appeared — so the round trip
    // provably reached the transcript before the run ended. The harness
    // additionally requires the peer's own verdict, which names the first step
    // the client failed to complete, so neither side can pass alone. A
    // 300-second budget covers boot + bounded PBKDF2 + the two-process net
    // bring-up on QEMU TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-netstack-telnet-qemu-aarch64",
        binary: "tairix-test-netstack-telnet-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6TelnetServer,
        ramfb: false,
        fs_disk: FsDisk::NetToolRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            ("root@tairix ~% ", Duration::ZERO, TELNET_COMMAND_LINE),
            // Expect-only until the peer's banner proves the whole option
            // exchange completed; then type the probe line.
            (
                tairix_test_netstack_wire::TELNET_BANNER,
                Duration::ZERO,
                TELNET_PROBE_LINE,
            ),
            // The peer's upper-cased answer proves the round trip; leave the
            // session through the escape character and the interpreter.
            (
                tairix_test_netstack_wire::TELNET_ECHO,
                Duration::ZERO,
                TELNET_QUIT_SEQUENCE,
            ),
            ("root@tairix ~% ", Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/NETWORK.md` N9b-3-2-β-2-ii-b: the static-addressing
    // (`match.node`) live-boot vertical.
    // `tairix-test-netstack-static-qemu-aarch64` boots the *production*
    // aarch64 pipeline with the `static-net-root` disk: the net-only signed
    // driver set **plus** a planted `/System/Settings/Network/network.conf`
    // that binds the NIC to the `wan` alias by its stable bus location
    // (`<iface>.match.node` = the QEMU-virt virtio-net register base) and
    // assigns it a static IPv6 address (`FsDisk::StaticNetRootDisk`), with a
    // `virtio-net-device` attached and the harness-side static-addressing
    // peer on its `dgram` netdev (`NetPeerMode::V6StaticEcho`). Everything
    // runs **before** any root unlock — the `/System` store and its
    // `Settings/` config are on the read-only volume mounted before the
    // passphrase — so the guest needs no console dialogue (headless, no
    // serial script), exactly like the riscv64 autoload sibling. `devmgr`
    // autoloads the virtio-net driver into its own process (it publishes a
    // `netchan` node), reads the planted config, and binds the NIC to `wan`
    // by its resolved bus location; `netstack` assigns the config's static
    // IPv6 address and answers the peer's campaign to that static address.
    // The guest does not self-exit: the harness ends the run the instant the
    // peer's out-of-guest observer confirms it received the guest's reply at
    // the *static* address — the last link in the causal chain, so teardown
    // can never precede (and lose the race to) that reply leaving the
    // machine. The witness records (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s
    // `INTERFACE_CONFIG_APPLIED` and `INBOUND_ECHO_SERVED`) still reach the
    // serial transcript for diagnosis, and the peer's own campaign verdict
    // subsumes them, so a `match.node` mis-bind (answered on the link-local
    // instead) cannot pass. A 240-second budget covers boot + autoload +
    // service bring-up + the bind + the config apply + the paced
    // static-address echo campaign on QEMU TCG; single CPU like the other
    // full-boot verticals.
    QemuTest {
        package: "tairix-test-netstack-static-qemu-aarch64",
        binary: "tairix-test-netstack-static-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6StaticEcho,
        ramfb: false,
        fs_disk: FsDisk::StaticNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/DHCP.md` D3: the live DHCPv4 vertical.
    // `tairix-test-netstack-dhcp-qemu-aarch64` boots the *production* aarch64
    // pipeline with the `dhcp-net-root` disk: the net-only signed driver set
    // **plus** a planted `/System/Settings/Network/network.conf` that binds
    // the NIC to the `wan` alias by its stable bus location
    // (`<iface>.match.node`), selects `ipv4.method dhcp`, and disables IPv6
    // (`FsDisk::DhcpNetRootDisk`), with a `virtio-net-device` attached and the
    // harness-side DHCP-server peer on its `dgram` netdev
    // (`NetPeerMode::V4DhcpEcho`). Everything runs **before** any root unlock
    // (headless, no serial script), like the static sibling. `devmgr`
    // autoloads the virtio-net driver into its own process (it publishes a
    // `netchan` node), reads the planted config, and binds the NIC to `wan`;
    // `netstack` drives the DHCP client, which broadcasts DISCOVER, accepts
    // the peer's OFFER, REQUESTs it, and applies the peer's ACK — leasing the
    // interface its only address. The peer then pings the guest at that leased
    // address and the guest answers. The guest does not self-exit: the
    // harness ends the run the instant the peer's out-of-guest observer
    // confirms it received the guest's reply at the *leased* address — the
    // last link in the causal chain, so teardown can never precede (and lose
    // the race to) that reply leaving the machine. The witness records
    // (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s `DHCP_LEASE_ACQUIRED` and
    // `INBOUND_ECHO_SERVED`) still reach the serial transcript for diagnosis,
    // and the peer's own verdict (it offered, acked, and got the echo reply
    // at the leased address) subsumes them, so a broken lease cannot pass on
    // an address the guest formed itself (it forms none). A 240-second budget
    // covers boot + autoload + service bring-up + the bind + the DHCP
    // exchange + the paced echo campaign on QEMU TCG; single CPU like the
    // other full-boot verticals.
    QemuTest {
        package: "tairix-test-netstack-dhcp-qemu-aarch64",
        binary: "tairix-test-netstack-dhcp-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V4DhcpEcho,
        ramfb: false,
        fs_disk: FsDisk::DhcpNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/TIMESYNC.md` TS-2: the live clock-establishment vertical.
    // `tairix-test-timed-qemu-aarch64` boots the *production* aarch64 pipeline
    // with the `time-net-root` disk: the net-only signed driver set, the
    // standard application/service store (so the real `timed` bundle is
    // present), the planted static-addressing `network.conf`, and — on the
    // encrypted root, the layer `timed` reads through the ordinary VFS — a
    // planted `system.conf` naming the host peer as the one time server by
    // address literal (`FsDisk::TimeNetRootDisk`). A `virtio-net-device` is
    // attached with the harness-side NTP-server peer on its `dgram` netdev
    // (`NetPeerMode::NtpServer`).
    //
    // The guest boots with the wall clock `Unset` (no RTC is modelled), so
    // `timed` finds the clock urgent, waits its randomised initial delay, and
    // queries the peer. The peer answers **twice, spoof first**: a well-formed
    // reply whose origin timestamp does not echo the request's nonce and which
    // reports `NTP_SPOOF_SECS`, then the truthful reply echoing the nonce and
    // reporting `NTP_FIXTURE_SECS`. That ordering is the discriminator — a
    // guest that accepted the spoof would land on the wrong instant, and one
    // that let the spoof cancel its transaction would ignore the truthful
    // reply and never set the clock — so the serial witness requires the
    // *exact* applied seconds, which only the nonce-gated path can produce.
    //
    // Three gates, none sufficient alone: the serial script requires the
    // unlock (so the planted `system.conf` is reachable), then the login
    // dialogue, then — expect-only, typing nothing — `timed`'s `CLOCK_SET`
    // audit record carrying `wall_secs=<NTP_FIXTURE_SECS>`, and only then
    // types the shell `exit` that completes the chain. The peer must
    // additionally report a served request. A 300-second budget covers boot +
    // autoload + service bring-up + the unlock and login dialogue + the
    // randomised initial delay and any backoff while the interface comes up,
    // on QEMU TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-timed-qemu-aarch64",
        binary: "tairix-test-timed-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::NtpServer,
        ramfb: false,
        fs_disk: FsDisk::TimeNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            // The script ends at the session prompt. The guest itself exits
            // on the applied instant, which happens seconds later, so a step
            // gated on that record would still be pending when it does and
            // the run would fail as an unfinished script.
            ("root@tairix ~% ", Duration::ZERO, ""),
        ],
    },
    // `plans/NETWORK.md` N9b-3-2-β-2-ii-b-bond: the live bond-failover
    // vertical. `tairix-test-netstack-bond-qemu-aarch64` boots the
    // *production* aarch64 pipeline with the `bond-net-root` disk: the
    // net-only signed driver set **plus** a planted
    // `/System/Settings/Network/network.conf` that binds two NICs by
    // `match.mac` as the members of one active-backup bond (`wan`) carrying
    // a static IPv6 address (`FsDisk::BondNetRootDisk`), with **two**
    // `virtio-net-device`s attached and the harness-side bond peer serving
    // both wires (`NetPeerMode::Bond`). Everything runs **before** any root
    // unlock (headless, no serial script), like the static sibling. `devmgr`
    // autoloads the NIC driver into a process per NIC; `netstack` composes
    // the active-backup bond over the two members and assigns it the static
    // address, answering the peer's echo campaign over the primary member.
    // Once the flow is established (the guest's first served echo), the
    // runner drops the primary member's carrier over the QEMU monitor
    // (`set_link net0 off`); the driver's virtio config-change interrupt
    // makes `netstack` fail the bond over to the backup member, and the
    // guest keeps answering over the second wire. PASS once the log sink has
    // seen `netstack`'s `BOND_CONFIG_APPLIED`, `BOND_FAILOVER`, and an
    // `INBOUND_ECHO_SERVED` observed **after** the failover — the last
    // gating exit so the guest stays alive until a frame has been served
    // post-failover; the peer's own campaign verdict (a reply at the bond's
    // static address) is required too, so neither side can pass without the
    // flow surviving the member drop. A 240-second budget covers boot +
    // autoload of two NIC drivers + service bring-up + the bond compose +
    // the paced echo campaign and the mid-flow failover on QEMU TCG; single
    // CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-netstack-bond-qemu-aarch64",
        binary: "tairix-test-netstack-bond-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::Bond,
        ramfb: false,
        fs_disk: FsDisk::BondNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NETWORK.md` N9b-3-2-β-2-ii-b: the x86_64 static-addressing
    // (`match.node`) live-config vertical — the virtio-**PCI** analogue of
    // `tairix-test-netstack-static-qemu-aarch64`. It boots the *production*
    // x86_64 pipeline with the `static-net-root` disk (`FsDisk::StaticNetRootDisk`),
    // whose planted `network.conf` is the x86_64 variant
    // (`STATIC_NETWORK_CONF_X86_64`): it binds the NIC to the `wan` alias by
    // its stable bus location (`<iface>.match.node` = the virtio-PCI
    // config-window BAR base the kernel enumerator assigns, not the aarch64
    // mmio slot) and assigns it a static IPv6 address. A `virtio-net-pci`
    // device is attached and the harness-side static-addressing peer
    // (`NetPeerMode::V6StaticEcho`) campaigns to the guest's *static* address.
    // Everything runs **before** any root unlock — the `/System` store and its
    // `Settings/` config are on the always-readable volume — so the guest
    // needs no console dialogue (headless, no serial script). `devmgr`
    // autoloads the virtio-net driver into its own process (it publishes a
    // `netchan` node), reads the planted config, and binds the NIC to `wan` by
    // its resolved BAR base; `netstack` assigns the static address and answers
    // the peer's campaign. The guest does not self-exit: the harness ends the
    // run the instant the peer's out-of-guest observer confirms it received
    // the guest's reply at the *static* address — the last link in the causal
    // chain, so teardown can never precede (and lose the race to) that reply
    // leaving the machine. The witness records (`devmgr`'s `NETSTACK_BOUND`,
    // `netstack`'s `INTERFACE_CONFIG_APPLIED` and `INBOUND_ECHO_SERVED`) still
    // reach the serial transcript for diagnosis, and the peer's own campaign
    // verdict subsumes them, so a `match.node` mis-bind (answered on the
    // link-local instead) cannot pass. Single CPU and the same 240-second
    // budget as its siblings.
    QemuTest {
        package: "tairix-test-netstack-static-qemu-x86-64",
        binary: "tairix-test-netstack-static-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6StaticEcho,
        ramfb: false,
        fs_disk: FsDisk::StaticNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/DHCP.md` D3: the x86_64 live DHCPv4 vertical — the virtio-**PCI**
    // analogue of `tairix-test-netstack-dhcp-qemu-aarch64`. It boots the
    // *production* x86_64 pipeline with the `dhcp-net-root` disk
    // (`FsDisk::DhcpNetRootDisk`), whose planted `network.conf` is the x86_64
    // variant (`DHCP_NETWORK_CONF_X86_64`): it binds the NIC to the `wan` alias
    // by its stable bus location (`<iface>.match.node` = the virtio-PCI
    // config-window BAR base the kernel enumerator assigns, not the aarch64
    // mmio slot), selects `ipv4.method dhcp`, and disables IPv6 — so the
    // interface forms no address of its own. A `virtio-net-pci` device is
    // attached and the harness-side DHCP-server peer (`NetPeerMode::V4DhcpEcho`)
    // leases the guest its only address and campaigns to it. Everything runs
    // **before** any root unlock — the `/System` store and its `Settings/`
    // config are on the always-readable volume — so the guest needs no console
    // dialogue (headless, no serial script). `devmgr` autoloads the virtio-net
    // driver into its own process (it publishes a `netchan` node), reads the
    // planted config, and binds the NIC to `wan` by its resolved BAR base;
    // `netstack` drives its DHCP client, which broadcasts DISCOVER, accepts the
    // peer's OFFER, REQUESTs it, and applies the peer's ACK. The guest does
    // not self-exit: the harness ends the run the instant the peer's
    // out-of-guest observer confirms it received the guest's reply at the
    // *leased* address — the last link in the causal chain, so teardown can
    // never precede (and lose the race to) that reply leaving the machine.
    // The witness records (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s
    // `DHCP_LEASE_ACQUIRED` and `INBOUND_ECHO_SERVED`) still reach the serial
    // transcript for diagnosis, and the peer's own verdict (it offered,
    // acked, and got the echo reply at the leased address) subsumes them, so
    // a broken lease cannot pass on an address the guest formed itself (it
    // forms none). Single CPU and the same 240-second budget as its siblings.
    QemuTest {
        package: "tairix-test-netstack-dhcp-qemu-x86-64",
        binary: "tairix-test-netstack-dhcp-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V4DhcpEcho,
        ramfb: false,
        fs_disk: FsDisk::DhcpNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NETWORK.md` N9b-3-2-β-2-ii-b-bond: the x86_64 bond-failover
    // live-config vertical — the virtio-**PCI** analogue of
    // `tairix-test-netstack-bond-qemu-aarch64`. It boots the *production*
    // x86_64 pipeline with the `bond-net-root` disk (`FsDisk::BondNetRootDisk`),
    // whose planted `network.conf` (`BOND_NETWORK_CONF`, arch-neutral because
    // the members bind by `match.mac`) composes two NICs as an active-backup
    // bond (`wan`) carrying a static IPv6 address. **Two** `virtio-net-pci`
    // devices are attached and the harness-side bond peer serves both wires
    // (`NetPeerMode::Bond`). Everything runs **before** any root unlock
    // (headless, no serial script). `devmgr` autoloads the NIC driver into a
    // process per NIC; `netstack` composes the bond and assigns the static
    // address, answering the peer over the primary member. Once the flow is
    // established (the guest's first served echo), the runner drops the primary
    // member's carrier over the QEMU monitor (`set_link net0 off`); the
    // driver's virtio config-change interrupt fails the bond over to the backup
    // member, and the guest keeps answering over the second wire. PASS once the
    // log sink has seen `netstack`'s `BOND_CONFIG_APPLIED`, `BOND_FAILOVER`,
    // and an `INBOUND_ECHO_SERVED` observed **after** the failover — the last
    // gating exit so the guest stays alive until a frame has been served
    // post-failover; the peer's own campaign verdict is required too, so
    // neither side can pass without the flow surviving the member drop. Single
    // CPU and the same 240-second budget as its siblings.
    QemuTest {
        package: "tairix-test-netstack-bond-qemu-x86-64",
        binary: "tairix-test-netstack-bond-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::Bond,
        ramfb: false,
        fs_disk: FsDisk::BondNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NETWORK.md` N9b-3-2-β-2-ii-b: the riscv64 static-addressing
    // (`match.node`) live-config vertical — the virtio-**MMIO** analogue of
    // `tairix-test-netstack-static-qemu-aarch64` on the QEMU riscv64 `virt`
    // board. It boots the *production* riscv64 pipeline with the
    // `static-net-root` disk (`FsDisk::StaticNetRootDisk`), whose planted
    // `network.conf` is the riscv64 variant (`STATIC_NETWORK_CONF_RISCV64`): it
    // binds the NIC to the `wan` alias by its stable bus location
    // (`<iface>.match.node` = the NIC's virtio-mmio transport slot base the
    // board's enumeration resolves, distinct from the aarch64 board's slot and
    // the x86_64 BAR base) and assigns it a static IPv6 address. A
    // `virtio-net-device` is attached and the harness-side static-addressing
    // peer (`NetPeerMode::V6StaticEcho`) campaigns to the guest's *static*
    // address. Everything runs **before** any root unlock — the `/System`
    // store and its `Settings/` config are on the always-readable volume — so
    // the guest needs no console dialogue (headless, no serial script).
    // `devmgr` autoloads the virtio-net driver into its own process (it
    // publishes a `netchan` node), reads the planted config, and binds the NIC
    // to `wan` by its resolved slot base; `netstack` assigns the static
    // address and answers the peer's campaign. The guest does not self-exit:
    // the harness ends the run the instant the peer's out-of-guest observer
    // confirms it received the guest's reply at the *static* address — the
    // last link in the causal chain, so teardown can never precede (and lose
    // the race to) that reply leaving the machine. The witness records
    // (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s `INTERFACE_CONFIG_APPLIED`
    // and `INBOUND_ECHO_SERVED`) still reach the serial transcript for
    // diagnosis, and the peer's own campaign verdict subsumes them, so a
    // `match.node` mis-bind (answered on the link-local instead) cannot pass.
    // Single CPU and the same 240-second budget as its siblings.
    QemuTest {
        package: "tairix-test-netstack-static-qemu-riscv64",
        binary: "tairix-test-netstack-static-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6StaticEcho,
        ramfb: false,
        fs_disk: FsDisk::StaticNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/DHCP.md` D3: the riscv64 live DHCPv4 vertical — the
    // virtio-**MMIO** analogue of `tairix-test-netstack-dhcp-qemu-aarch64` on
    // the QEMU riscv64 `virt` board. It boots the *production* riscv64 pipeline
    // with the `dhcp-net-root` disk (`FsDisk::DhcpNetRootDisk`), whose planted
    // `network.conf` is the riscv64 variant (`DHCP_NETWORK_CONF_RISCV64`): it
    // binds the NIC to the `wan` alias by its stable bus location
    // (`<iface>.match.node` = the NIC's virtio-mmio transport slot base the
    // board's enumeration resolves, distinct from the aarch64 board's slot and
    // the x86_64 BAR base), selects `ipv4.method dhcp`, and disables IPv6 — so
    // the interface forms no address of its own. A `virtio-net-device` is
    // attached and the harness-side DHCP-server peer (`NetPeerMode::V4DhcpEcho`)
    // leases the guest its only address and campaigns to it. Everything runs
    // **before** any root unlock — the `/System` store and its `Settings/`
    // config are on the always-readable volume — so the guest needs no console
    // dialogue (headless, no serial script). `devmgr` autoloads the virtio-net
    // driver into its own process (it publishes a `netchan` node), reads the
    // planted config, and binds the NIC to `wan` by its resolved slot base;
    // `netstack` drives its DHCP client, which broadcasts DISCOVER, accepts the
    // peer's OFFER, REQUESTs it, and applies the peer's ACK. The guest does
    // not self-exit: the harness ends the run the instant the peer's
    // out-of-guest observer confirms it received the guest's reply at the
    // *leased* address — the last link in the causal chain, so teardown can
    // never precede (and lose the race to) that reply leaving the machine.
    // The witness records (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s
    // `DHCP_LEASE_ACQUIRED` and `INBOUND_ECHO_SERVED`) still reach the serial
    // transcript for diagnosis, and the peer's own verdict (it offered,
    // acked, and got the echo reply at the leased address) subsumes them, so
    // a broken lease cannot pass on an address the guest formed itself (it
    // forms none). Single CPU. Budgeted at 360 s, the same as its riscv64
    // DHCPv6 sibling, which does strictly more guest work: riscv64 is the
    // slowest TCG target, and the full boot + autoload + service bring-up +
    // bind + DHCP exchange takes materially longer on it.
    //
    // The budget is not the knob for a miss here. A healthy run trips the gate
    // within a few seconds of boot, so this carries roughly sixty-fold
    // headroom, and the one miss that looked like host load was in fact a
    // guest-side stall: an unbounded virtio completion wait parked the boot
    // task inside a disk request while it held the disk's lock, so `/System`'s
    // mount and the driver store never came up. Raising the budget only
    // lengthens the silence before the same stall is reported, which is why
    // this run now reports how long the guest had been silent when it was
    // killed — near zero means look at the peer, the link or host load; a
    // silence near the ceiling means look at the transcript's last line.
    QemuTest {
        package: "tairix-test-netstack-dhcp-qemu-riscv64",
        binary: "tairix-test-netstack-dhcp-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(360),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V4DhcpEcho,
        ramfb: false,
        fs_disk: FsDisk::DhcpNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/DHCP.md` D4c: the live DHCPv6 vertical, one per Tier-1 arch — the
    // IPv6 peer of the D3 DHCPv4 verticals. Each boots the *production*
    // pipeline for its arch with the `dhcp6-net-root` disk: the net-only
    // signed driver set **plus** a planted
    // `/System/Settings/Network/network.conf` that binds the NIC to the `wan`
    // alias by its stable bus location (`<iface>.match.node`), selects
    // `ipv6.method dhcp`, and disables IPv4 (`FsDisk::Dhcp6NetRootDisk`), with
    // a `virtio-net` device attached and the harness-side DHCPv6-server peer
    // on its `dgram` netdev (`NetPeerMode::V6Dhcp6Echo`). Everything runs
    // **before** any root unlock (headless, no serial script), like the D3
    // siblings. `devmgr` autoloads the virtio-net driver into its own process
    // (it publishes a `netchan` node), reads the planted config, and binds the
    // NIC to `wan`; `netstack` drives the DHCPv6 client, which Solicits,
    // accepts the peer's Advertise, Requests it, and applies the Reply —
    // leasing the interface its only global address. Because DHCPv6 grants no
    // on-link prefix, the peer also emits Router Advertisements naming the
    // shared `/64` on-link (non-autonomous) so the guest can route back; the
    // peer then pings the guest at the leased address and the guest answers.
    // The guest does not self-exit: the harness ends the run the instant the
    // peer's out-of-guest observer confirms it received the guest's reply at
    // the *leased* address — the last link in the causal chain, so teardown
    // can never precede (and lose the race to) that reply leaving the
    // machine. The witness records (`devmgr`'s `NETSTACK_BOUND`, `netstack`'s
    // `DHCP6_LEASE_ACQUIRED` and `INBOUND_ECHO_SERVED`) still reach the serial
    // transcript for diagnosis, and the peer's own verdict (it advertised,
    // replied, and got the echo reply at the leased address) subsumes them,
    // so a broken lease cannot pass on an address the guest formed itself (it
    // forms none). A 240-second budget covers boot + autoload + service
    // bring-up + the bind + the DHCPv6 exchange + the paced echo campaign on
    // QEMU TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-netstack-dhcp6-qemu-aarch64",
        binary: "tairix-test-netstack-dhcp6-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(360),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6Dhcp6Echo,
        ramfb: false,
        fs_disk: FsDisk::Dhcp6NetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/DHCP.md` D4c: the x86_64 DHCPv6 vertical — the virtio-**PCI**
    // sibling of the aarch64 one, binding the NIC by its config-window BAR
    // base. See the aarch64 entry above for the full choreography.
    QemuTest {
        package: "tairix-test-netstack-dhcp6-qemu-x86-64",
        binary: "tairix-test-netstack-dhcp6-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(360),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6Dhcp6Echo,
        ramfb: false,
        fs_disk: FsDisk::Dhcp6NetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/DHCP.md` D4c: the riscv64 DHCPv6 vertical — the virtio-**MMIO**
    // sibling of the aarch64 one on the QEMU riscv64 `virt` board. See the
    // aarch64 entry above for the full choreography.
    QemuTest {
        package: "tairix-test-netstack-dhcp6-qemu-riscv64",
        binary: "tairix-test-netstack-dhcp6-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(360),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6Dhcp6Echo,
        ramfb: false,
        fs_disk: FsDisk::Dhcp6NetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NETWORK.md` N9b-3-2-β-2-ii-b-bond: the riscv64 bond-failover
    // live-config vertical — the virtio-**MMIO** analogue of
    // `tairix-test-netstack-bond-qemu-aarch64` on the QEMU riscv64 `virt`
    // board. It boots the *production* riscv64 pipeline with the
    // `bond-net-root` disk (`FsDisk::BondNetRootDisk`), whose planted
    // `network.conf` (`BOND_NETWORK_CONF`, arch-neutral because the members
    // bind by `match.mac`) composes two NICs as an active-backup bond (`wan`)
    // carrying a static IPv6 address. **Two** `virtio-net-device`s are attached
    // and the harness-side bond peer serves both wires (`NetPeerMode::Bond`).
    // Everything runs **before** any root unlock (headless, no serial script).
    // `devmgr` autoloads the NIC driver into a process per NIC; `netstack`
    // composes the bond and assigns the static address, answering the peer over
    // the primary member. Once the flow is established (the guest's first
    // served echo), the runner drops the primary member's carrier over the
    // QEMU monitor (`set_link net0 off`); the driver's virtio config-change
    // interrupt fails the bond over to the backup member, and the guest keeps
    // answering over the second wire. PASS once the log sink has seen
    // `netstack`'s `BOND_CONFIG_APPLIED`, `BOND_FAILOVER`, and an
    // `INBOUND_ECHO_SERVED` observed **after** the failover — the last gating
    // exit so the guest stays alive until a frame has been served
    // post-failover; the peer's own campaign verdict is required too, so
    // neither side can pass without the flow surviving the member drop. Single
    // CPU and the same 240-second budget as its siblings.
    QemuTest {
        package: "tairix-test-netstack-bond-qemu-riscv64",
        binary: "tairix-test-netstack-bond-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::Bond,
        ramfb: false,
        fs_disk: FsDisk::BondNetRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/SPAWN.md` SP10b: the pipeline/redirection acceptance vertical.
    // `tairix-test-pipeline-qemu-aarch64` boots the *production* aarch64
    // pipeline with the planted encrypted-root disk, unlocks the root,
    // authenticates `root`/`root` at the console login, and drives the
    // spawned shell through real pipelines and redirections over the spawn
    // attach block and kernel pipes (`plans/SPAWN.md` SP10a):
    // `yes | head -n 2` proves back-pressure teardown end to end — `head`
    // exits after two lines, the pipe loses its last reader, `yes`'s next
    // write fails `BrokenPipe` and it exits, and the shell reaps the
    // non-leader member (the next prompt appearing at all is the witness;
    // a hung producer or an unreaped member times the run out).
    // `seq 1 1000 | wc -c` proves payload integrity: the consumer's `3893`
    // (the 2893 digits plus 1000 newlines of `1..=1000`) is arithmetic
    // over the pipe's entire byte stream, output the typed line itself
    // never contains. The `> file` / `< file` round trip proves spawn-time
    // file wiring both directions: `seq 776001 776005 > nums.txt` writes
    // through a shell-pre-opened create+truncate descriptor wired as the
    // child's stdout, and `cat < nums.txt` reads it back through a wired
    // stdin — `776005` is content only the written file could produce.
    // Each line is typed only after its marker appeared; every marker is
    // output, never an echo of a typed line. The guest audit sink arms on
    // the round trip `cat`'s audited `exit` (`sc=exit`, `comm=cat` — the
    // last scripted tool, which only runs after every pipeline step
    // completed) and reports PASS on the next audited `exit` — the
    // shell's, typed only after the content marker appeared — so the
    // verified bytes provably reached the transcript before the run ended
    // (the session-ceiling arm-then-exit discipline). A 120-second budget
    // covers boot + bounded PBKDF2 + the multi-exchange dialogue on QEMU
    // TCG; single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-pipeline-qemu-aarch64",
        binary: "tairix-test-pipeline-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            ("root@tairix ~% ", Duration::ZERO, "yes | head -n 2\n"),
            // The pipeline terminating at all (the prompt returning) is the
            // broken-pipe + member-reap witness.
            ("root@tairix ~% ", Duration::ZERO, "seq 1 1000 | wc -c\n"),
            // `lspci --help` then `lsusb --help` (plans/DEVICES.md DEVICE1
            // V2/V3) prove the resource-carrying bundles end to end: the
            // spawn's load gate re-hashes each whole on-disk bundle —
            // including the planted `Resources/pci.ids.bin` /
            // `Resources/usb.ids.bin` tables — against the signed `AppInfo`
            // content hash, so a mis-planted or tampered resource refuses the
            // spawn and times the run out; each help summary's token on the
            // transcript (`PCI/PCIe`, `USB devices` — tokens no other
            // scripted step emits, immune to render wrapping) is the witness
            // the tool ran. (The `virt` image drives virtio-mmio devices, so
            // the tree carries no PCI-function or USB-interface nodes to
            // list yet — the listing paths are host-proven in
            // `tairix-lspci`'s / `tairix-lsusb`'s tests.) Typed before the
            // round-trip `cat`, so the audit sink's arm-on-`cat` discipline
            // is untouched.
            ("3893", Duration::ZERO, "lspci --help\n"),
            ("PCI/PCIe", Duration::ZERO, "lsusb --help\n"),
            (
                "USB devices",
                Duration::ZERO,
                "seq 776001 776005 > /Users/root/nums.txt\n",
            ),
            (
                "root@tairix ~% ",
                Duration::ZERO,
                "cat < /Users/root/nums.txt\n",
            ),
            ("776005", Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/STRESSTEST.md` ST4: the `sysmon` monitor acceptance vertical.
    // `tairix-test-sysmon-qemu-aarch64` boots the *production* aarch64
    // pipeline with the planted encrypted-root disk, unlocks the root,
    // authenticates `root`/`root` at the console login, and starts
    // `sysmon` from its store bundle (the load gate verifies the whole
    // on-disk bundle; the granted `CAP_MEM_PIN` lets it pin itself). The
    // `Pres` gauge label on the transcript witnesses the
    // `MEMORY_PRESSURE` pressure gauge rendered on the first frame; `r`
    // then drives an immediate refresh over the raw console, and the
    // `hit%` token (a `RECLAIM_STATS` cache-effectiveness column header)
    // witnesses the reclaim detail panel rendered. `q` quits,
    // leaving the alternate screen; the shell prompt reappearing is the
    // intact-screen witness, after which the runner types `exit`. Each
    // line is typed only after its marker appeared; every marker is
    // output, never an echo of a typed line. The guest audit sink arms on
    // `sysmon`'s audited `exit` (`sc=exit`, `comm=sysmon`) and reports
    // PASS on the next audited `exit` — the shell's, typed only after the
    // restored prompt appeared — so the verified frames provably reached
    // the transcript before the run ended (the session-ceiling
    // arm-then-exit discipline). A 120-second budget covers boot + bounded
    // PBKDF2 + the interactive session on QEMU TCG; single CPU like the
    // other full-boot verticals.
    QemuTest {
        package: "tairix-test-sysmon-qemu-aarch64",
        binary: "tairix-test-sysmon-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            ("root@tairix ~% ", Duration::ZERO, "sysmon\n"),
            // The first frame's pressure gauge rendered (its `Pres` label).
            ("Pres", Duration::ZERO, "r"),
            // The refresh key was accepted (raw-mode input works); the
            // reclaim ledger panel's cache-hit-ratio column header rendered.
            ("hit%", Duration::ZERO, "q"),
            // The monitor quit and the shell repainted its prompt on the
            // restored screen.
            ("root@tairix ~% ", Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/STRESSTEST.md` ST5: the `stress` load-generator acceptance
    // vertical. `tairix-test-stress-qemu-aarch64` boots the *production*
    // aarch64 pipeline with the planted encrypted-root disk, unlocks the
    // root, authenticates `root`/`root` at the console login, and runs a
    // exact reported sequence: after the first authenticated shell prompt it
    // waits one second, types exactly
    // `stress --cpu 10 --timeout 120s --background`, requires the returned
    // prompt to accept `sysmon`, observes its `Pres` gauge frame, refreshes to
    // the reclaim (`hit%`) panel, and quits back to the shell while ten CPU-bound
    // workers saturate four CPUs. After the post-`sysmon` prompt it advances
    // its serial cursor past the launcher's early stress-worker syscalls and,
    // on the next `comm=stress` line (the detached controller waking to tear
    // its 120-second run down), types `exit`. PASS is decided by the guest
    // sink, which records three witnesses — both `comm=stress` exits (the
    // foreground launcher and the detached controller) and the `comm=elsh`
    // shell exit — and fires on whichever completes the set. The order is not
    // fixed on purpose: the controller's exit and the shell's scripted `exit`
    // are concurrent, so any ordering assumption would race. A 300-second
    // guest budget covers boot, bounded PBKDF2, the required 120-second load,
    // and teardown on QEMU TCG; four CPUs reproduce the RPi4 saturation shape.
    QemuTest {
        package: "tairix-test-stress-qemu-aarch64",
        binary: "tairix-test-stress-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 4,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::EncryptedRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[
            ("ARXFS passphrase: ", Duration::ZERO, UNLOCK_PASSPHRASE_LINE),
            ("Username:", Duration::ZERO, SESSION_USERNAME_LINE),
            ("Password", Duration::ZERO, SESSION_PASSWORD_LINE),
            (
                "root@tairix ~% ",
                Duration::from_secs(1),
                "stress --cpu 10 --timeout 120s --background\n",
            ),
            // The detached launcher returned and the shell accepts a command.
            ("root@tairix ~% ", Duration::ZERO, "sysmon\n"),
            // The monitor rendered while the CPU workers were active.
            ("Pres", Duration::ZERO, "r"),
            // Raw input and a fresh sysinfo round trip remain live under load.
            ("hit%", Duration::ZERO, "q"),
            // Advance past the prompt restored by `sysmon`; the launcher's
            // earlier stress-worker syscalls are now outside the search window.
            ("root@tairix ~% ", Duration::ZERO, ""),
            // The next `comm=stress` line past that prompt is the detached
            // controller waking to tear down its 120-second run, so the shell
            // `exit` is typed while the controller is finishing. PASS ordering
            // is not keyed on this marker: the guest sink fires once both
            // `comm=stress` exits and this `comm=elsh` exit are observed, in
            // any order, so the concurrent shell-exit / controller-exit race
            // that this marker cannot disambiguate cannot flake the run.
            ("comm=stress", Duration::ZERO, "exit\n"),
        ],
    },
    // `plans/PI.md` design B / B2 + `plans/DISPLAY.md` D7d (first stage): the
    // pre-unlock driver-loading-by-discovery autoload vertical, booted as a
    // *display* world. `tairix-test-autoload-input-qemu-aarch64` boots the
    // *production* aarch64 pipeline on the `virt` board with the
    // `FsDisk::AutoloadRootDisk` whole-disk image — whose read-only `/System`
    // volume carries the kernel-signed virtio-input keyboard driver bundle
    // *and* the framebuffer display-service bundle in its `Drivers/` store —
    // plus an attached `ramfb` display, a `virtio-keyboard-device`, and a
    // `virtio-mouse-device`. With the display attached the framebuffer boot
    // console comes up, the boot publishes the scan-out surface as the boot
    // display node, and the video console is the *only* console (the UART
    // carries the debug log alone), so the whole dialogue is typed at the
    // seat keyboard, never over serial. The boot binds the virtio-blk root
    // and discovers the virtio-input nodes; the unlock kthread mounts the
    // read-only `/System` volume and its autoload hook scans that volume's
    // signed store **before** any passphrase prompt, verifies each bundle
    // against the kernel's embedded driver trust anchor, matches the
    // keyboard/mouse bundles to the virtio-input nodes and the display
    // bundle to the boot display node, and spawns each into its own
    // user-space process with exactly its node's resource grants. Once both
    // input-driver instances have armed their interrupts (the audited
    // `irq_bind` syscall, twice), the runner types the fixture passphrase +
    // Enter at the virtio keyboard (paced `sendkey`s; the characters buffer
    // as console type-ahead until the video-console passphrase prompt drains
    // them); the first delivered keystroke emits the `kind=key` witness,
    // after which the runner injects the mouse motion. PASS once the
    // kernel-side audit sink has seen all four witnesses: `InputDelivered`
    // for both kinds (an autoloaded *user-space* driver instance delivered
    // each input class), `UsersDbLoaded` (the typed passphrase unlocked the
    // encrypted root end to end), and `CallEndpointCreated` for the reserved
    // `DISPLAY_ENDPOINT` (the autoloaded display service came up on its
    // granted surface and bound its rendezvous under
    // `CAP_IPC_BIND_PRIVILEGED`).
    //
    // D7d-2 grows the run into the desktop launch: after the unlock, the
    // second typed step (keyed on the `UsersDbLoaded` serial witness)
    // types the fixture account's `root`/`root` at login's video-console
    // prompt — the disk plants `os.loginType text`, so login drops to the
    // account's shell — and then the `desktop` command word, which the
    // shell resolves in the system application store and spawns: the desktop is
    // started exactly the way a user starts it from the command line.
    //
    // AW3 (`plans/APPWIN.md`) grows the presented desktop into the full
    // click-through: the session's one-shot `DESKTOP_REVEALED` witness
    // keys the first screendump (the dark composited desktop) and
    // the click on the autostarted file manager's own icon-bar slot (the
    // guest applies injected events strictly in device order, so the click
    // needs no extra gate); that click opens its window over the
    // reserved window rendezvous, and the endpoint's first *reply* on
    // serial gates the in-window click. From there every stage is keyed on
    // the kernel/ipc `MessageDelivered` records the desktop's app-ward
    // event deliveries emit — the shared interaction contract in the test
    // crate's lib target: delivery 2 (Focus + Pressed from the window
    // click) keys the second screendump (the served window on the dark
    // desktop), and a handshake click (delivery 3, injected only after
    // delivery 2 appeared and held while the second dump is pending) is
    // the wake boundary the terminal stage gates on.
    //
    // AW4 then takes the run into the windowed terminal: keyed on the
    // handshake's own delivery, the script clicks the taskbar's Library
    // button (the program-library popup opens over the catalog the guest
    // session merged from the planted machine store — `plans/NEW-TASKBAR.md`
    // T5) and then the popup's "Terminal" entry (spawning the terminal
    // bundle, which spawns the user's shell over one kernel pseudo-terminal
    // — `plans/PTY.md` — and serves its window at the second cascade slot);
    // the third window-frame map (the terminal's create) gates the
    // terminal-window click, after which the runner types `sleep 3600` +
    // Enter at the seat keyboard once the guest's terminal-focus marker
    // appears. The guest PASS gate latches the `appmgr` load of
    // `/System/Commands/sleep.app` — `sleep` is loaded only by the shell running
    // the typed command, so this witness is uniquely attributable (no
    // fragile delivery count), and it proves the whole keyboard → session →
    // library popup → terminal → pty → shell → load round trip. The runner
    // fails any run whose script or dumps did not complete.
    //
    // The pty stage then proves the cooked-mode line discipline end to end
    // (`plans/PTY.md`): `sleep 3600` is a *blocking* foreground job, so
    // once the guest witnesses its spawn it emits a marker gating a
    // `Ctrl-C` injection (the ETX byte the terminal encodes as `0x03`);
    // the pty's cooked `^C` signals the foreground `sleep` dead, the shell
    // — unblocked from its `wait` — reads and spawns a recovered `true`,
    // and that second spawn is the guest's job-control witness. A failed
    // interrupt leaves `sleep` blocking past the budget, so the run times
    // out (fail loud).
    // Every click coordinate is computed from the production shell's own
    // layout code (`autoload_desktop_pointer_script`), and the pin move
    // also delivers the `kind=pointer` witness, so the pointer decode path
    // stays separately proven. A 300-second budget covers the boot +
    // bounded PBKDF2 + autoload + driver bring-up + the ~4 s passphrase +
    // ~1 s login typing + session bring-up + the paced click script +
    // both app spawns + the typed command + the pty Ctrl-C job-control
    // round trip, on QEMU TCG. The file-manager stages (FM9/FM10/FM11) are
    // deliberately not driven here (host-tested in `lib/browse`); see the
    // sink doc in `src/main.rs` and `plans/OPEN-DEFECTS.md` D20.
    QemuTest {
        package: "tairix-test-autoload-input-qemu-aarch64",
        binary: "tairix-test-autoload-input-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::AutoloadRootDisk,
        keyboard: None,
        typed_keys: &[
            (
                AUTOLOAD_INPUT_KEY_MARKER,
                AUTOLOAD_INPUT_ARMED_OCCURRENCES,
                UNLOCK_PASSPHRASE_LINE,
            ),
            (AUTOLOAD_LOGIN_MARKER, 1, AUTOLOAD_LOGIN_DIALOGUE),
            // AW4 terminal stage: type the shell command once the terminal
            // gains focus (its spawn is the PASS gate's round-trip witness).
            // Gated on the guest focus marker, not a raw count the files
            // window satisfies before the terminal exists.
            (
                AUTOLOAD_TERMINAL_FOCUSED_MARKER,
                1,
                AUTOLOAD_TERMINAL_COMMAND,
            ),
            // The pty Ctrl-C job-control step (`plans/PTY.md`): held behind
            // the guest's sleep-spawn marker so the `Ctrl-C` lands against a
            // live, parked foreground job. It interrupts the foreground
            // `sleep` and types `true` — the recovered `true` load is the
            // guest's pty job-control witness (the vertical's final PASS
            // witness; no file-manager stage follows).
            (
                AUTOLOAD_CTRL_C_ARM_MARKER,
                1,
                AUTOLOAD_TERMINAL_CTRL_C_RECOVERY,
            ),
        ],
        screendumps: &[
            ScreendumpPlan {
                marker: AUTOLOAD_DESKTOP_REVEALED_MARKER,
                occurrences: 1,
                suffix: "desktop",
                assert: assert_dark_desktop_screendump,
            },
            ScreendumpPlan {
                marker: AUTOLOAD_FILES_ACTIVATED_MARKER,
                occurrences: 1,
                suffix: "window",
                assert: assert_files_window_dark_screendump,
            },
        ],
        pointer_script: Some(autoload_desktop_pointer_script),
        serial: &[],
    },
    // `plans/NEW-TASKBAR.md`: the desktop **icon-bar** vertical. A
    // deliberately short, dedicated sibling of the autoload desktop vertical
    // above rather than a further stage on it, so a gate mis-count in one
    // choreography cannot wedge the other
    // (`plans/OPEN-DEFECTS.md` D19/D20).
    //
    // It boots the same graphical world — the `FsDisk::AutoloadRootDisk`
    // whole-disk image, whose read-only `/System` volume carries the signed
    // virtio-input and framebuffer driver bundles, the complete app +
    // service store, and the seeded program-library catalog — types the
    // unlock passphrase, logs in, and starts `desktop`. The pointer script
    // then does what no host test can: opens the program library, launches
    // the terminal from its row, right-clicks the slot the session gave that
    // process on the bar, chooses the *New window* row of the menu the
    // application itself declared, and finally primary-clicks that same slot
    // to take the default action the declaration claimed.
    //
    // PASS needs both of the guest's witnesses: an `APP_LOADED` naming the
    // terminal's bundle, and three window creates served on the reserved
    // endpoint. The desktop's own surfaces are session-painted compositor
    // windows and never call the window channel, so the two creates after
    // the launch can only be the chosen row and the declared default action
    // reaching the application. Three dumps read the screen — the bar before
    // anything runs, the bar and window once the application is up, and both
    // windows with the application still holding exactly one slot — each
    // gated on the session's own witness that the frame is on screen, and
    // each verified before the runner sends the gesture that follows it. The
    // last window is opened by the last gesture, so the guest cannot exit
    // before the final dump is safely read back.
    //
    // Single CPU (PID 1, the unlock kthread, the autoloaded drivers, the
    // session, and its children share the boot CPU). The 300-second budget
    // is the same *inactivity* budget the sibling desktop vertical carries:
    // the longest the guest may fall silent, never a runtime deadline, so
    // co-scheduling cannot turn a merely slow guest into a timeout.
    QemuTest {
        package: "tairix-test-appbar-qemu-aarch64",
        binary: "tairix-test-appbar-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::AutoloadRootDisk,
        keyboard: None,
        typed_keys: &[
            // The encrypted-root passphrase, held until both autoloaded
            // input drivers have armed their interrupts so no keystroke
            // hits a dead device.
            (
                AUTOLOAD_INPUT_KEY_MARKER,
                AUTOLOAD_INPUT_ARMED_OCCURRENCES,
                UNLOCK_PASSPHRASE_LINE,
            ),
            // The fixture account's login, then `desktop` at the text
            // shell's prompt — the same bundle a graphical login spawns.
            (AUTOLOAD_LOGIN_MARKER, 1, AUTOLOAD_LOGIN_DIALOGUE),
        ],
        screendumps: &[
            ScreendumpPlan {
                marker: AUTOLOAD_DESKTOP_REVEALED_MARKER,
                occurrences: 1,
                suffix: APPBAR_BARE_BAR_DUMP,
                assert: assert_bare_bar_dark_screendump,
            },
            ScreendumpPlan {
                marker: APPBAR_WINDOW_SHOWN_MARKER,
                occurrences: 1,
                suffix: APPBAR_ONE_WINDOW_DUMP,
                assert: assert_one_window_dark_screendump,
            },
            ScreendumpPlan {
                marker: APPBAR_WINDOW_SHOWN_MARKER,
                occurrences: 2,
                suffix: APPBAR_TWO_WINDOWS_DUMP,
                assert: assert_two_windows_dark_screendump,
            },
        ],
        pointer_script: Some(appbar_pointer_script),
        serial: &[],
    },
    // Elevated Date & Time launch: right-click the taskbar clock, choose
    // *Set Date & Time…*, authenticate as the fixture root account through
    // the session's credential prompt, and witness the Date & Time window.
    // The guest latches APP_LOADED for datetime.app plus one window create
    // after that load; the host types credentials once the prompt announces
    // itself on serial. Single CPU, same inactivity budget as the icon-bar
    // sibling so co-scheduling cannot turn a merely slow guest into a timeout.
    QemuTest {
        package: "tairix-test-datetime-elevate-qemu-aarch64",
        binary: "tairix-test-datetime-elevate-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::AutoloadRootDisk,
        keyboard: None,
        typed_keys: &[
            (
                AUTOLOAD_INPUT_KEY_MARKER,
                AUTOLOAD_INPUT_ARMED_OCCURRENCES,
                UNLOCK_PASSPHRASE_LINE,
            ),
            (AUTOLOAD_LOGIN_MARKER, 1, AUTOLOAD_LOGIN_DIALOGUE),
            // The credential prompt is up and focused: offer the fixture
            // account. Tab moves from the account field to the password;
            // Enter submits. The broker starts datetime.app as that account.
            (ELEVATE_PROMPT_SHOWN_MARKER, 1, "root\troot\n"),
        ],
        screendumps: &[],
        pointer_script: Some(datetime_elevate_pointer_script),
        serial: &[],
    },
    // `plans/SMARTRAM.md` + `plans/ICONS.md`: the desktop keeps drawing its
    // real icon artwork while the machine is genuinely short of memory.
    //
    // Opening windows is how a user spends memory, so the script opens a
    // screenful of terminal windows — one per primary click on the
    // application's own icon-bar slot, each click gated on the session's
    // witness that the previous window reached the screen. On this board's
    // default RAM that takes free memory below the mild watermark, and the
    // guest refuses to pass unless the published band really left normal, so
    // the run can never report a pass without having tested the state it is
    // named for.
    //
    // The two dumps are the bare revealed desktop and the frame just before
    // the last window opens (`WINDOWS_SHOWN_AT_DUMP` — the guest's own witness
    // is a create reply, which precedes the frame). The assertion reads the
    // *file manager's* slot across them:
    // the script never touches that slot, so a picture that changes between
    // the frames changed because of what the desktop did to its own caches
    // under pressure — a desktop that dropped its decoded icons draws
    // built-in glyphs instead.
    //
    // Single CPU and the same 300-second *inactivity* budget as its siblings:
    // the longest the guest may fall silent, never a runtime deadline, so
    // co-scheduling cannot turn a slow guest into a timeout.
    QemuTest {
        package: "tairix-test-desktop-pressure-qemu-aarch64",
        binary: "tairix-test-desktop-pressure-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::AutoloadRootDisk,
        keyboard: None,
        typed_keys: &[
            (
                AUTOLOAD_INPUT_KEY_MARKER,
                AUTOLOAD_INPUT_ARMED_OCCURRENCES,
                UNLOCK_PASSPHRASE_LINE,
            ),
            (AUTOLOAD_LOGIN_MARKER, 1, AUTOLOAD_LOGIN_DIALOGUE),
        ],
        screendumps: &[
            ScreendumpPlan {
                marker: AUTOLOAD_DESKTOP_REVEALED_MARKER,
                occurrences: 1,
                suffix: DESKTOP_PRESSURE_ICONS_DRAWN_DUMP,
                assert: assert_icons_drawn_dark_screendump,
            },
            ScreendumpPlan {
                marker: APPBAR_WINDOW_SHOWN_MARKER,
                occurrences: tairix_test_desktop_pressure_qemu_aarch64::WINDOWS_SHOWN_AT_DUMP,
                suffix: DESKTOP_PRESSURE_UNDER_PRESSURE_DUMP,
                assert: assert_bar_artwork_survived_screendump,
            },
        ],
        pointer_script: Some(desktop_pressure_pointer_script),
        serial: &[],
    },
    // `plans/FIX-DESKTOP-SPEEDUP.md` A.4: the desktop **hover** vertical — the
    // regression gate on what a gesture costs the compositor, and the only
    // test that reads the published frame accounting back from a running
    // desktop.
    //
    // It boots the graphical world of its own `FsDisk::HoverRootDisk` image —
    // the autoload layout plus the test-only `framestats` fixture bundle,
    // which the seeded program-library catalog is derived from and so lists —
    // types the unlock passphrase, logs in, and starts `desktop`. The pointer
    // script then launches `framestats` from the library, sweeps the pointer
    // the whole length of the icon bar, and launches `framestats` again.
    //
    // The two samples bracket the sweep, and the guest judges the work between
    // them: per-frame damage as a share of the screen, overdraw per damaged
    // pixel, no recomputed frost, no re-rendered furniture, and no more driver
    // calls than rectangles and frames. Every bound is over counted work
    // rather than elapsed time, so the verdict is load-independent — and the
    // window must have composed frames at all, so an empty difference fails
    // rather than passing by measuring nothing. A whole-epoch figure could not
    // stand in: bring-up legitimately composes full-screen frames and owns
    // both the epoch's mean and its peak.
    //
    // No screendump: the claim is what the frames cost, which is a counter and
    // not a picture. Its siblings already assert the pixels.
    //
    // Single CPU and the same 300-second *inactivity* budget as its siblings:
    // the longest the guest may fall silent, never a runtime deadline, so
    // co-scheduling cannot turn a slow guest into a timeout.
    QemuTest {
        package: "tairix-test-desktop-hover-qemu-aarch64",
        binary: "tairix-test-desktop-hover-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::HoverRootDisk,
        keyboard: None,
        typed_keys: &[
            (
                AUTOLOAD_INPUT_KEY_MARKER,
                AUTOLOAD_INPUT_ARMED_OCCURRENCES,
                UNLOCK_PASSPHRASE_LINE,
            ),
            (AUTOLOAD_LOGIN_MARKER, 1, AUTOLOAD_LOGIN_DIALOGUE),
        ],
        screendumps: &[],
        pointer_script: Some(desktop_hover_pointer_script),
        serial: &[],
    },
    // `plans/NEW-DESKTOP-LOGIN.md` G7.1: a display-capable machine that
    // nobody has configured boots to the **graphical** login screen on its
    // own. The sibling verticals above all plant `os.loginType text` because
    // their scripts drive a shell; this one deliberately does not, so the
    // compiled default is what decides — the state every fresh installation
    // boots in, and the one no other vertical covers.
    //
    // The script types the unlock passphrase and nothing else: no account,
    // no `desktop` command. So the greeter's `SCREEN_READY` — emitted only
    // once it holds the seat and its first frame is on screen — can only be
    // login's own choice, made after the encrypted root mounted and its
    // settings store answered "no configuration" rather than "not here".
    //
    // Single CPU and the same 300-second *inactivity* budget as its
    // siblings: the longest the guest may fall silent, never a runtime
    // deadline, so co-scheduling cannot turn a slow guest into a timeout.
    QemuTest {
        package: "tairix-test-greeter-default-qemu-aarch64",
        binary: "tairix-test-greeter-default-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::GreeterRootDisk,
        keyboard: None,
        typed_keys: &[
            // The encrypted-root passphrase, held until the autoloaded
            // keyboard driver has armed its interrupt so no keystroke hits
            // a dead device. The run scripts no pointer, so the keyboard is
            // the only input device on the board. Nothing is typed after it.
            (
                AUTOLOAD_INPUT_KEY_MARKER,
                KEYBOARD_ONLY_ARMED_OCCURRENCES,
                UNLOCK_PASSPHRASE_LINE,
            ),
        ],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NETWORK.md` N4e-riscv64 (first stage): the riscv64
    // driver-loading-by-discovery autoload vertical — the `virt`-board
    // virtio-mmio analogue of the aarch64 `autoload_input` vertical, reduced
    // to the input-autoload path (no display world, no desktop). It boots the
    // *production* riscv64 pipeline on the `virt` board against the shared
    // `FsDisk::AutoloadRootDisk` whole-disk image — whose always-readable
    // `/System` store carries the kernel-signed virtio-input keyboard driver
    // bundle (cross-compiled for riscv64 by `image_drivers`) — with a
    // `virtio-keyboard-device` attached. The boot binds the virtio-blk root
    // and discovers the virtio-input node; the unlock kthread mounts `/System`
    // and serves its signed store **independently of** the encrypted-root
    // passphrase (the riscv64 SBI console has no interactive input drain this
    // slice, so the interactive unlock fails closed — no passphrase is typed),
    // and the user-space `devmgr` matches the signed bundle to the discovered
    // node and asks the kernel to spawn it into its own process. Once the
    // autoloaded driver arms its granted PLIC interrupt (the audited
    // `irq_bind` syscall — the `sc=irq_bind` serial marker), the runner sends
    // one key through the QEMU monitor; the eventq IRQ fires and the driver
    // decodes+injects it. PASS via the SiFive Test finisher once the
    // kernel-side audit sink has seen `AuditEvent::InputDelivered` with
    // `kind=key` — an autoloaded *user-space* driver instance delivered.
    // Single CPU (PID 1, the unlock/store kthread, the autoloaded driver, and
    // `devmgr` share the boot hart). A 60-second budget matches the other
    // boot-then-do-fixed-work verticals: boot + `/System` mount + autoload +
    // driver bring-up + the injected key complete in a few seconds on QEMU
    // TCG, with ample headroom.
    QemuTest {
        package: "tairix-test-autoload-input-qemu-riscv64",
        binary: "tairix-test-autoload-input-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::AutoloadRootDisk,
        // One virtio-input node → one autoloaded driver instance → one
        // `irq_bind`, so the injection gates on the marker's first appearance
        // (`with_virtio_keyboard`); the injected key is the whole observable
        // effect the `kind=key` witness proves.
        keyboard: Some((AUTOLOAD_INPUT_KEY_MARKER, "a")),
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/ARCHSUPPORT.md` A4: the x86_64 driver-loading-by-discovery
    // autoload vertical — the virtio-**PCI** analogue of the aarch64 /
    // riscv64 `autoload_input` verticals, reduced to the input-autoload path
    // (no display world, no desktop). It boots the *production* x86_64
    // pipeline on the `q35`/`pc` machine against the shared
    // `FsDisk::AutoloadRootDisk` whole-disk image — whose always-readable
    // `/System` store carries the kernel-signed virtio-input keyboard driver
    // bundle (cross-compiled for x86_64 by `image_drivers`) — with a
    // `virtio-keyboard-pci` device attached. The boot binds the virtio-blk-pci
    // root and discovers the virtio-input-PCI node; the in-kernel unlock
    // kthread mounts `/System` and serves its signed store **independently of**
    // the encrypted-root passphrase, and the user-space `devmgr` matches the
    // signed bundle to the discovered node and asks the kernel to spawn it into
    // its own process with the node's four role-tagged config windows + DMA +
    // routed MSI-X line. Once the autoloaded driver arms its granted interrupt
    // (the audited `irq_bind` syscall — the `sc=irq_bind` serial marker), the
    // runner sends one key through the QEMU monitor; the eventq IRQ fires and
    // the driver decodes+injects it. PASS via QEMU `isa-debug-exit` once the
    // kernel-side audit sink has seen `AuditEvent::InputDelivered` with
    // `kind=key` — an autoloaded *user-space* driver instance delivered over
    // virtio-PCI. Single CPU (PID 1, the unlock/store kthread, the autoloaded
    // driver, and `devmgr` share the boot CPU). A 60-second budget matches the
    // other boot-then-do-fixed-work verticals: boot + `/System` mount +
    // autoload + driver bring-up + the injected key complete in a few seconds
    // on QEMU TCG, with ample headroom.
    QemuTest {
        package: "tairix-test-autoload-input-qemu-x86-64",
        binary: "tairix-test-autoload-input-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::AutoloadRootDisk,
        // One virtio-input node → one autoloaded driver instance → one
        // `irq_bind`, so the injection gates on the marker's first appearance
        // (`with_virtio_keyboard`); the injected key is the whole observable
        // effect the `kind=key` witness proves.
        keyboard: Some((AUTOLOAD_INPUT_KEY_MARKER, "a")),
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NETWORK.md` N4e-β: the aarch64 **two-process** live-boot
    // netstack vertical.
    // `tairix-test-netstack-autoload-qemu-aarch64` boots the *production*
    // aarch64 `tairix-kernel` pipeline on the `virt` board against the
    // shared `FsDisk::AutoloadRootDisk` whole-disk image — whose read-only
    // `/System` store now carries the kernel-signed virtio-net driver
    // bundle beside the input and display bundles — with a
    // `virtio-net-device` attached (its MAC pinned to the wire constant so
    // the guest's EUI-64 link-local is deterministic) and the harness-side
    // `netpeer` link peer in its v6-link-local-only campaign mode. The
    // production autoload path spawns the virtio-net driver into its own
    // user process (it publishes a `netchan` node), the user-space
    // `devmgr` service calls `netstack` `BindDriver`, and `netstack`
    // provisions the channel and auto-configures the interface's EUI-64
    // link-local (no IPv4). PASS once the audit sink has seen `devmgr`'s
    // `NETSTACK_BOUND`, `netstack`'s `DRIVER_BOUND`, and `netstack`'s
    // `INBOUND_ECHO_SERVED` in the transcript. The guest does not
    // self-exit: the run ends on the peer's completion gate — it received
    // the guest's reply, the last link in the chain — so teardown can never
    // precede the reply leaving the machine. Booted as a **display world** (`ramfb`): the
    // framebuffer boot console is the primary console, so the login TUI
    // renders to the framebuffer and the UART carries only the debug log —
    // exactly as `autoload_input`. That frees the single CPU for the
    // reactive user-space `devmgr` autoload, which brings the virtio-net
    // driver up from the read-only `/System` store *before* any unlock
    // (the same pre-unlock autoload that arms `autoload_input`'s input
    // drivers), so `netstack` (an init service, already running) binds it
    // and answers the peer's link-local echo without any passphrase
    // dialogue. A 240-second budget covers boot + autoload + service
    // bring-up + the bind + the paced echo campaign on QEMU TCG.
    QemuTest {
        package: "tairix-test-netstack-autoload-qemu-aarch64",
        binary: "tairix-test-netstack-autoload-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6LinkLocal,
        ramfb: true,
        fs_disk: FsDisk::AutoloadRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NETWORK.md` N4e-riscv64: the riscv64 **two-process** live-boot
    // netstack vertical — the `virt`-board
    // virtio-mmio / PLIC analogue of the aarch64
    // `tairix-test-netstack-autoload-qemu-aarch64` vertical, reduced to the
    // headless boot world (no `ramfb`, no display): riscv64's SBI/NULL console
    // has no interactive input this slice, so the encrypted-root unlock fails
    // closed by design, but the `/System` store binds independently of the
    // passphrase and the network driver still autoloads. It boots the
    // *production* riscv64 `tairix-kernel` pipeline against the shared
    // `FsDisk::AutoloadRootDisk` whole-disk image — whose always-readable
    // `/System` store carries the kernel-signed virtio-net driver bundle
    // (cross-compiled for riscv64 by `image_drivers`) beside the input and
    // display bundles — with a `virtio-net-device` attached (its MAC pinned to
    // the wire constant so the guest's EUI-64 link-local is deterministic) and
    // the harness-side `netpeer` link peer in its v6-link-local-only campaign
    // mode. The production autoload path spawns the virtio-net driver into its
    // own user process (it publishes a `netchan` node), the user-space
    // `devmgr` service calls `netstack` `BindDriver`, and `netstack` provisions
    // the channel and auto-configures the interface's EUI-64 link-local (no
    // IPv4). PASS once the log sink has seen `devmgr`'s `NETSTACK_BOUND`,
    // `netstack`'s `DRIVER_BOUND`, and `netstack`'s `INBOUND_ECHO_SERVED` in
    // the transcript. The guest does not self-exit: the run ends on the peer's
    // completion gate — it received the guest's reply, the last link in the
    // chain — so teardown can never precede the reply leaving the machine. Single CPU (PID 1, the
    // unlock/store kthread, the autoloaded driver, `netstack`, and `devmgr`
    // share the boot hart, alongside the NULL-console `login` fast-respawn),
    // with the same 240-second budget as its aarch64 sibling covering boot +
    // autoload + service bring-up + the bind + the paced echo campaign on QEMU
    // TCG.
    QemuTest {
        package: "tairix-test-netstack-autoload-qemu-riscv64",
        binary: "tairix-test-netstack-autoload-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6LinkLocal,
        ramfb: false,
        fs_disk: FsDisk::AutoloadRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NETWORK.md` N4e / `plans/ARCHSUPPORT.md` A4: the x86_64
    // **two-process** live-boot netstack vertical — the virtio-**PCI**
    // analogue of the aarch64/riscv64 `netstack_autoload` verticals. It
    // boots the *production* x86_64 `tairix-kernel` pipeline on
    // the `q35`/`pc` machine against the shared `FsDisk::AutoloadRootDisk`
    // whole-disk image — whose always-readable `/System` store carries the
    // kernel-signed virtio-net driver bundle (cross-compiled for x86_64 by
    // `image_drivers`) beside the input and display bundles — with a
    // `virtio-net-pci` device attached (its MAC pinned to the wire constant so
    // the guest's EUI-64 link-local is deterministic) and the harness-side
    // `netpeer` link peer in its v6-link-local-only campaign mode. The
    // bootstrap-floor virtio-PCI enumeration discovers the NIC node with the
    // kernel enumerator routing its MSI-X; the production autoload path spawns
    // the virtio-net driver into its own user process (it publishes a
    // `netchan` node), the user-space `devmgr` calls `netstack` `BindDriver`,
    // and `netstack` provisions the channel and auto-configures the interface's
    // EUI-64 link-local (no IPv4). PASS once the log sink has seen `devmgr`'s
    // `NETSTACK_BOUND`, `netstack`'s `DRIVER_BOUND`, and `netstack`'s
    // `INBOUND_ECHO_SERVED` in the transcript. The guest does not self-exit:
    // the run ends on the peer's completion gate — it received the guest's
    // reply, the last link in the chain — so teardown can never precede that
    // reply leaving the machine. Single CPU, with the same 240-second budget
    // as its siblings covering boot + autoload + service bring-up + the bind
    // + the paced echo campaign on QEMU TCG.
    QemuTest {
        package: "tairix-test-netstack-autoload-qemu-x86-64",
        binary: "tairix-test-netstack-autoload-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::V6LinkLocal,
        ramfb: false,
        fs_disk: FsDisk::AutoloadRootDisk,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage W11-B (`plans/WIRING.md` §3): the aarch64 display vertical —
    // the EL1/GICv2 + ramfb analogue of the riscv64 framebuffer-display
    // vertical. `tairix-test-framebuffer-display-qemu-aarch64` brings the
    // `virt` board up to EL1 (FP enable + 2 GiB identity MMU + vectors,
    // shared from `virtio_qemu_support`), programs QEMU's `ramfb` over the
    // shared `fw_cfg` MMIO DMA interface so a static guest-RAM surface
    // becomes a real scan-out framebuffer, assembles the geometry as a
    // `FramebufferConfig`, then loads the signed framebuffer display
    // `.rxe` through `tairix_drvhost::Host` and drives it through
    // load -> use -> unload -> reload. "Use" maps the surface through the
    // capability-gated `KernelMmioMapper` and `present`s a frame; a second
    // independently-mapped window reads the pixels back to confirm they
    // reached the scan-out memory. Any deviation flips the ARM semihosting
    // failure finisher. Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-framebuffer-display-qemu-aarch64",
        binary: "tairix-test-framebuffer-display-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: true,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 3b: `tairix-test-timer-preempt-qemu-aarch64` is the aarch64
    // half of the Stage-3 "timer interrupt drives the scheduler"
    // per-sub-stage deliverable. It installs the EL1 vectors, brings up
    // the GICv2, arms the EL1 physical generic timer at 100 Hz, unmasks
    // IRQs, and idles on `wfi` until the generic-timer IRQ path has
    // driven the `preempt` callback 20 times — proving the timer
    // repeatedly delivers and re-arms — then reports PASS via semihosting.
    // Single CPU and a 60-second budget.
    QemuTest {
        package: "tairix-test-timer-preempt-qemu-aarch64",
        binary: "tairix-test-timer-preempt-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage 3b: `tairix-test-memory-isolation-qemu-aarch64` is the
    // aarch64 half of the Stage-3 "memory-isolation test passes"
    // per-sub-stage deliverable — the analogue of the x86_64 and riscv64
    // verticals. It builds a victim and an attacker stage-1
    // `paging::AddressSpace` (each identity-maps the low 2 GiB) that
    // disagree on a single 64 GiB page, installs the EL1 vectors and a
    // `fault` handler, switches `TTBR0_EL1` to the attacker (enabling the
    // MMU), and reads that page: the MMU raises a data abort, the handler
    // confirms the cause / faulting address, and reports PASS via
    // semihosting. A regression that fails to isolate the page reads it
    // without faulting and reports FAILURE explicitly. Single CPU and a
    // 60-second budget.
    QemuTest {
        package: "tairix-test-memory-isolation-qemu-aarch64",
        binary: "tairix-test-memory-isolation-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (stage G1):
    // `tairix-test-stack-guard-qemu-aarch64` proves the live mechanism the
    // kthread kernel-stack guard page is built on. It builds one stage-1
    // `paging::AddressSpace` (identity-maps the low 2 GiB), calls
    // `AddressSpace::split_block` to shatter the coarse identity block that
    // covers a dedicated `GUARD_PAGE` static down to 4 KiB pages
    // (preserving every mapping), installs the EL1 vectors + a `fault`
    // handler, enables the MMU, writes+reads-back a sentinel through the
    // guard page (proving the split preserved the mapping live), then
    // `unmap`s that single page through the Arch HAL + `flush_page`s its
    // stale TLB entry and reads it: the MMU raises a data abort, the
    // handler confirms the cause / faulting address, and reports PASS via
    // semihosting. A regression that fails to split, preserve, or unmap
    // either reports FAILURE explicitly or never faults (timing out).
    // Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-stack-guard-qemu-aarch64",
        binary: "tairix-test-stack-guard-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (stage G2):
    // `tairix-test-stack-arena-qemu-aarch64` proves the boot-time
    // kthread-stack guard arena (`AddressSpace::prepare_guard_arena`). It
    // builds one stage-1 `paging::AddressSpace` (identity-maps the low
    // 2 GiB), prepares a 2 MiB-aligned, 2 MiB guard arena at 4 KiB
    // granularity (the arena is its own L2 block, distinct from the block
    // holding the running code/stack), installs the EL1 vectors + a
    // `fault` handler, enables the MMU, writes+reads-back a sentinel
    // through an arena guard page (proving the split preserved the mapping
    // live), then `unmap`s that one page through the Arch HAL +
    // `flush_page`s it, proves the running stack (a different 2 MiB block)
    // and a neighbouring arena page still work, and reads the unmapped
    // page: the MMU raises a data abort, the handler confirms the cause /
    // faulting address, and reports PASS via semihosting. A regression
    // that shatters the running block, fails to preserve the arena, or
    // never faults either reports FAILURE explicitly or times out. Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "tairix-test-stack-arena-qemu-aarch64",
        binary: "tairix-test-stack-arena-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/FIX-KHEAP.md` slab tier: `tairix-test-kslab-qemu-aarch64` proves
    // the kernel heap's page accounting on real hardware. It enables the MMU
    // over an identity space, builds a `FrameAllocator` over a `.bss` pool,
    // installs the production `kernel/mem::FramePages` supply into the
    // `tairix-kalloc` global allocator, and then asserts through the plain
    // `GlobalAlloc` surface that a page-sized allocation costs exactly one
    // frame and starts on a page boundary (so it carries no header), that the
    // page is writable end to end through the live direct map, that a drained
    // page is kept back once and reused without a draw while the next is
    // returned, that every size class round-trips aligned to its own width,
    // and that a request above the granule still comes from the byte-granular
    // tier. A regression in the routing, the descriptor placement, or the page
    // provenance reports FAILURE explicitly. Single CPU and a 60-second budget
    // match the other boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-kslab-qemu-aarch64",
        binary: "tairix-test-kslab-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/PI.md` guard-page fault-form (stage G3c): the *production*
    // fault-form. `tairix-test-stack-overrun-qemu-aarch64` proves that an
    // overrunning kthread takes a synchronous data abort, not a
    // next-reschedule canary detection. It builds a stage-1 identity
    // `AddressSpace`, prepares a 2 MiB-aligned guard arena
    // (`AddressSpace::prepare_guard_arena`, G2), carves one kthread stack
    // region `[guard page | usable stack]` out of it, installs the EL1
    // vectors + a `fault` handler, enables the MMU, then `unmap`s the guard
    // page through the Arch HAL + `flush_page`s it — the production
    // guard-page mechanism (G3b-2). It then builds the live
    // `tairix-kernel-sched-eevdf` `Scheduler` over `Aarch64Arch`, admits a
    // kthread on that stack via `kernel_core::spawn_kthread_with_stack`, and
    // drives the cooperative `step` loop. The kthread body overruns its
    // stack (touches the highest guard byte, the first byte a contiguous
    // downward overrun crosses); because the guard page is unmapped the
    // access raises a synchronous data abort *while the kthread runs*, the
    // handler confirms the cause / faulting address, and reports PASS via
    // semihosting. A regression that left the page mapped lets the body
    // return cleanly; the drain loop then reports FAILURE explicitly rather
    // than passing. Single CPU and a 60-second budget match the other
    // boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-stack-overrun-qemu-aarch64",
        binary: "tairix-test-stack-overrun-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // WIRING Stage W3-B (`plans/WIRING.md` §3): the aarch64 device-IRQ
    // vertical — the EL1/GICv2-SPI analogue of `tairix-test-irq-qemu-x86-64`.
    // `tairix-test-irq-qemu-aarch64` installs the EL1 vectors, brings up the
    // GICv2, builds a kernel-neutral `tairix_kernel_irq::IrqTable`, binds the
    // PL031 RTC's shared-peripheral interrupt (INTID 34), routes that SPI to
    // CPU 0 through the new `gic::route_spi` (`GICD_ITARGETSR`), installs a
    // set-once device-IRQ dispatcher (`exceptions::set_device_irq_dispatch`)
    // that forwards the line to `IrqTable::fire` over a `GicController`
    // bridge, arms the RTC match, and unmasks IRQs. When the RTC fires, the
    // GIC delivers the SPI to EL1, the dispatcher masks the line and sets the
    // wait flag, and the main loop observes `WaitStep::Ready`; it then
    // re-reads the GIC enable bit and asserts the line is masked (the
    // mask-before-wake invariant, `docs/src/security/irq.md`) before the ARM
    // semihosting PASS finisher. A regression that fails to route, deliver,
    // or mask never reaches PASS, so the run times out. Single CPU and a
    // 60-second budget match the other boot-then-do-fixed-work aarch64 tests.
    QemuTest {
        package: "tairix-test-irq-qemu-aarch64",
        binary: "tairix-test-irq-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // P11 Chunk B-2 INCREMENT (1) (`plans/PI.md`): the aarch64 device-SPI
    // -> parked-kthread vertical, the proof that the production aarch64
    // device-IRQ subsystem can wake an in-kernel service kthread (the
    // prerequisite for INCREMENT (2)'s root-unlock kthread). Where
    // `tairix-test-irq-qemu-aarch64` proves the device-IRQ *delivery* path
    // against a hard-coded INTID and a `wfi` poll loop, this vertical proves
    // the two INCREMENT (1) pieces that path serves: (1) DTB SPI discovery
    // (`fdt::gic_device_intid` decodes the PL031 RTC node's `interrupts`
    // triple into its GICv2 INTID from the embedded `virt` tree — no board
    // constant), and (2) the kthread-cooperative
    // `tairix_kernel_core::KthreadIrqWaiter`, driven by a real in-kernel
    // service kthread (`spawn_kthread`) through the shared
    // `block_until_ready` loop on the live `tairix-kernel-sched-eevdf`
    // `Scheduler`. The kthread parks on the bound RTC SPI, yielding each
    // cooperative `step`; when the RTC fires the EL1 GICv2 path masks the
    // line and sets the ready flag, the kthread observes `WaitOutcome::Ready`
    // and exits, and the kernel asserts the GIC line re-reads masked
    // (mask-before-wake, `docs/src/security/irq.md`) before the semihosting
    // PASS. A regression that fails to discover, deliver, wake, or mask never
    // reaches PASS, so the run times out (fail-loud). Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // aarch64 tests.
    QemuTest {
        package: "tairix-test-irq-kthread-qemu-aarch64",
        binary: "tairix-test-irq-kthread-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // Stage W11-B (`plans/WIRING.md` §3): the aarch64 input vertical —
    // the `virt`-board virtio-input analogue of the x86_64 PS/2 vertical,
    // completing the `input` row of the QEMU matrix for aarch64.
    // `tairix-test-input-virtio-mmio-qemu-aarch64` brings the `virt` board
    // up to EL1 (FP enable + 2 GiB identity MMU + GICv2/EL1 IRQ path,
    // shared from `virtio_qemu_support`), builds the virtio-MMIO bus from
    // the embedded device tree, provisions an `MmioTransport` through the
    // capability-gated `KernelMmioMapper`, arms the device's GICv2 SPI,
    // mints a `KernelVirtioHost`, loads the signed virtio-input `.rxe`
    // through `tairix_drvhost::Host`, and drives it through
    // load -> use -> unload -> reload. "Use" is a real injected key: once
    // the guest logs the event-queue-armed readiness marker, the runner
    // sends a key through the QEMU monitor (`sendkey`), the eventq IRQ
    // fires, and the driver decodes the press then (after reload) the
    // release. The runner attaches the `virtio-keyboard-device` and drives
    // the injection; the guest never fabricates the event. Single CPU and
    // a 60-second budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-input-virtio-mmio-qemu-aarch64",
        binary: "tairix-test-input-virtio-mmio-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: Some(("virtio-qemu: virtio-input eventq armed", "a")),
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
    // `plans/NEW-FILEMANAGER.md` FM9-c: the aarch64 virtio-input
    // **pointer-button** vertical — the mouse-button sibling of the
    // `input_virtio_mmio` keyboard vertical. It brings the `virt` board up
    // to EL1, builds the virtio-MMIO bus from the embedded device tree,
    // provisions an `MmioTransport`, arms the device's GICv2 SPI, mints a
    // `KernelVirtioHost`, loads the signed virtio-input `.rxe`, and decodes
    // a real injected **secondary (right) button** press then release. The
    // runner attaches a `virtio-mouse-device` (implied by the pointer
    // script) and, once the guest logs the event-queue-armed marker, sends
    // `mouse_button` presses over the monitor; the eventq IRQ fires and the
    // shared `virtio_input_button` tail asserts the decoded button is
    // `BTN_RIGHT` (`0x111`), never the middle button (`0x112`). This guards
    // the `tools/qemu` fix for QEMU's mislabelled HMP `mouse_button` state
    // bits: before it, a scripted right-click was delivered as a middle
    // button and the file manager's right-click context menu was
    // unreachable in QEMU. No keyboard is attached (a mouse-only topology),
    // so the guest opens the single virtio-input node — the mouse. Single
    // CPU and a 60-second budget match the other boot-then-do-fixed-work
    // input verticals.
    QemuTest {
        package: "tairix-test-pointer-button-virtio-mmio-qemu-aarch64",
        binary: "tairix-test-pointer-button-virtio-mmio-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: None,
        typed_keys: &[],
        screendumps: &[],
        pointer_script: Some(pointer_button_script),
        serial: &[],
    },
    // WIRING (`plans/WIRING.md` §1/§3): the riscv64 input vertical —
    // the `virt`-board virtio-input MMIO analogue of the aarch64 input
    // vertical, completing the `input` row of the QEMU matrix for
    // riscv64. `tairix-test-input-virtio-mmio-qemu-riscv64` boots the
    // `virt`-board pipeline, builds the virtio-MMIO bus from the device
    // tree, provisions an `MmioTransport` through the capability-gated
    // `KernelMmioMapper`, arms the device's PLIC source + S-mode trap
    // path, mints a `KernelVirtioHost`, loads the signed virtio-input
    // `.rxe` through `tairix_drvhost::Host`, and drives it through
    // load -> use -> unload -> reload. "Use" is a real injected key: once
    // the guest logs the event-queue-armed readiness marker, the runner
    // sends a key through the QEMU monitor (`sendkey`), the eventq IRQ
    // fires, and the driver decodes the press then (after reload) the
    // release. The runner attaches the `virtio-keyboard-device` and drives
    // the injection; the guest never fabricates the event. The driver and
    // the shared `virtio_input_keypress` tail are the same code the
    // aarch64 vertical runs. Single CPU and a 60-second
    // budget match the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-input-virtio-mmio-qemu-riscv64",
        binary: "tairix-test-input-virtio-mmio-qemu-riscv64",
        target: "riscv64gc-unknown-none-elf",
        cpus: 1,
        timeout: Duration::from_secs(60),
        ram_mib: None,
        disk_sectors: None,
        netstack_peer: NetPeerMode::None,
        ramfb: false,
        fs_disk: FsDisk::None,
        keyboard: Some(("virtio-qemu: virtio-input eventq armed", "a")),
        typed_keys: &[],
        screendumps: &[],
        pointer_script: None,
        serial: &[],
    },
];

/// Rust target triple for the riscv64 enrolments; selects the
/// `Spec::for_riscv64_kernel` constructor in [`run_one`].
const RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

/// Rust target triple for the aarch64 enrolments; selects the
/// `Spec::for_aarch64_kernel` constructor in [`run_one`].
const AARCH64_TARGET: &str = "aarch64-unknown-none";

/// The enrolments selected by an optional `--only` package-substring
/// filter: every test whose package name contains `only`, or the whole
/// table when no filter is given.
///
/// An empty selection is refused rather than silently running nothing — a
/// mistyped filter must fail loudly, never report a vacuous green run.
fn enrolled(only: Option<&str>) -> Result<Vec<&'static QemuTest>, String> {
    let selected: Vec<&'static QemuTest> = match only {
        Some(needle) => TESTS
            .iter()
            .filter(|t| t.package.contains(needle))
            .collect(),
        None => TESTS.iter().collect(),
    };
    if selected.is_empty() {
        return Err(format!(
            "test --qemu: `--only {}` matches no enrolled package",
            only.unwrap_or_default()
        ));
    }
    Ok(selected)
}

/// Build every selected QEMU test once.
///
/// Call this before the (possibly repeated) [`run_once`] passes so a soak
/// re-runs the binaries rather than rebuilding them each pass ('s no-flaky-tests rule: the value of repetition is in the *runs*).
/// `only` restricts the build to the enrolments whose package name contains
/// it — the `--only` debugging filter.
pub fn build_all(ctx: &Context, only: Option<&str>) -> Result<(), String> {
    let selected = enrolled(only)?;
    eprintln!("xtask: [test --qemu] {} test(s) enrolled", selected.len());
    // Resolve the C toolchain once, up front, and export its path so every
    // CCOMPAT C-program build script uses the authoritative override rather
    // than re-running the full `clang`/`ld.lld` search per crate and target.
    prime_c_toolchain();
    // Group the selected packages by target triple and build each triple in a
    // single `cargo build`. One invocation per triple (rather than one per
    // enrolment) lets cargo compile that triple's packages concurrently and
    // share a single build-lock acquisition, instead of serialising behind the
    // lock once per test. The QEMU *runs* then execute concurrently under a
    // host-CPU budget — see [`run_once`] and `commands::parallel`.
    for target in build_targets() {
        let packages: Vec<&str> = selected
            .iter()
            .filter(|t| t.target == target)
            .map(|t| t.package)
            .collect();
        if packages.is_empty() {
            continue;
        }
        let mut cmd = ctx.cargo();
        cmd.args(["build", "--locked", "--target", target]);
        for pkg in &packages {
            cmd.args(["-p", pkg]);
        }
        let label = format!("test --qemu (build {target}: {} pkg)", packages.len());
        // One `cargo build` links every enrolled package for this target at
        // once; that can legitimately outrun an incremental host compile
        // pass, so it gets the longer budget rather than the default.
        ctx.run_with_timeout(&label, cmd, LONG_BUILD_COMMAND_TIMEOUT)?;
    }
    // Pre-warm each selected enrolment's per-arch `/System`-store bundle set
    // (`stores_for` composes only what the enrolment's `fs_disk` plants, for
    // its own target arch, and memoises per arch), so the (possibly
    // concurrent) run passes reuse one composition per arch instead of racing
    // to build it. An enrolment that plants no store resolves to empty sets
    // and pays no cross-compile.
    for t in &selected {
        stores_for(ctx, t)?;
    }
    Ok(())
}

/// Resolve the audited C toolchain once and export its path to the child
/// build scripts, so the CCOMPAT C-program builds skip the per-crate search.
///
/// Each `c_program_qemu_*` build script calls `tairix_cc::Toolchain::discover`,
/// which — absent an override — searches `PATH` and every known LLVM install
/// prefix, running `--version` on each candidate until one reports the pinned
/// release. Doing that independently in every C build script, for every target
/// triple, repeats the same subprocess "hunt" many times per run. Resolving it
/// here once and exporting `TAIRIX_CC_CLANG` / `TAIRIX_CC_LLD` makes each build
/// script take the authoritative-override fast path (a single `--version` on
/// the known binary) instead.
///
/// The discovery logic itself is not duplicated: this only *calls* the one
/// definition in `tairix-cc` and records its result. It is best-effort — an
/// operator override is left untouched, and a discovery failure is not fatal
/// here (a build script that actually needs the toolchain still fails closed
/// with the full install hint), so a run that builds no C program is never
/// blocked by an absent C toolchain.
fn prime_c_toolchain() {
    let plan = PrimePlan::from_env(
        std::env::var_os("TAIRIX_CC_CLANG").is_some(),
        std::env::var_os("TAIRIX_CC_LLD").is_some(),
    );
    if !plan.discover {
        return;
    }
    match tairix_cc::Toolchain::discover() {
        Ok(toolchain) => {
            if plan.set_clang {
                std::env::set_var("TAIRIX_CC_CLANG", &toolchain.clang.path);
            }
            if plan.set_lld {
                std::env::set_var("TAIRIX_CC_LLD", &toolchain.lld.path);
            }
            eprintln!(
                "xtask: [test --qemu] C toolchain primed: clang={} ld.lld={}",
                toolchain.clang.path.display(),
                toolchain.lld.path.display(),
            );
        }
        Err(err) => {
            eprintln!(
                "xtask: [test --qemu] C toolchain not pre-resolved ({err}); \
                 each C-program build script will resolve it on demand"
            );
        }
    }
}

/// What [`prime_c_toolchain`] should do given which override variables the
/// environment already pins — the pure, testable core of the decision.
///
/// An operator override is authoritative and must never be clobbered, so a
/// variable that is already set is left alone; discovery runs only if at least
/// one variable still needs a value.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct PrimePlan {
    /// Whether the toolchain must be resolved at all.
    discover: bool,
    /// Whether to export `TAIRIX_CC_CLANG` from the resolved toolchain.
    set_clang: bool,
    /// Whether to export `TAIRIX_CC_LLD` from the resolved toolchain.
    set_lld: bool,
}

impl PrimePlan {
    fn from_env(clang_pinned: bool, lld_pinned: bool) -> Self {
        let set_clang = !clang_pinned;
        let set_lld = !lld_pinned;
        Self {
            discover: set_clang || set_lld,
            set_clang,
            set_lld,
        }
    }
}

/// The distinct target triples across the enrolled tests, in first-seen
/// order, so each triple is built exactly once.
fn build_targets() -> Vec<&'static str> {
    let mut targets: Vec<&'static str> = Vec::new();
    for t in TESTS {
        if !targets.contains(&t.target) {
            targets.push(t.target);
        }
    }
    targets
}

/// Host-capacity weight for one QEMU guest within `host_budget`.
///
/// A uniprocessor guest charges two units — its vCPU plus one for QEMU's
/// emulator/main-loop/I/O threads. An SMP TCG guest charges the entire budget
/// and therefore runs alone: its mutually synchronising vCPU threads need
/// simultaneous host progress, and a full-boot SMP guest co-scheduled with a
/// busy matrix starves so badly it trips its *own* in-guest lockup watchdog
/// (observed: the four-vCPU scheduler-stress guest hard-locked when the whole
/// matrix ran at once). Isolating SMP guests keeps every one of their vCPUs on
/// a real core. A guest heavier than the whole budget still runs — alone, when
/// nothing else is in flight — rather than deadlocking ([`parallel::run`]).
#[must_use]
pub(crate) fn qemu_job_weight(cpus: u32, host_budget: usize) -> usize {
    let budget = host_budget.max(1);
    if cpus > 1 {
        budget
    } else {
        2.min(budget)
    }
}

/// QEMU matrix capacity: one **third** of the host's logical-CPU count.
///
/// The budget bounds the sum of in-flight guest weights ([`qemu_job_weight`]),
/// so on this hybrid 22-thread host it admits ~3 co-scheduled uniprocessor
/// guests (weight 2 each) while an SMP guest runs alone. A third is deliberate
/// headroom, not a full-host cap: a QEMU guest is far heavier than its lone
/// vCPU thread (translation, the RCU/main-loop and I/O threads), and every
/// guest also runs its *own* real-time watchdogs and timers. Admitting one
/// guest per logical CPU oversubscribes the host so badly that guests miss
/// those internal deadlines and hard-lock — observed directly when the matrix
/// ran near core-count wide. A third keeps the host comfortably
/// under-subscribed so no guest is starved of TCG time.
///
/// This modest concurrency is safe against *wall-clock* flakiness because the
/// runner's deadline is an *inactivity* budget, not a total-runtime one
/// (`tairix_qemu`): a guest that runs a little slower co-scheduled keeps
/// emitting serial output and is never killed for being slow. Raising the
/// budget further is a timing change that must be validated on the dedicated
/// soak host (`tools/ci/soak.sh`), never from a single green developer run.
#[must_use]
pub(crate) fn qemu_host_budget_for(logical_cpus: usize) -> usize {
    (logical_cpus / 3).max(1)
}

/// QEMU matrix capacity for this host.
#[must_use]
pub(crate) fn qemu_host_budget() -> usize {
    qemu_host_budget_for(parallel::host_parallelism())
}

/// Execute every enrolled QEMU test once, running guests concurrently.
///
/// The caller ([`super::run_test`]) owns the repeat loop so a duration
/// budget covers the whole matrix as a unit; this runs exactly one pass and
/// never retries on failure.
///
/// The enrolments are independent — each plants its own per-binary backing
/// images and drives a guest whose serial console is `-serial stdio` and
/// whose QEMU monitor is a unique per-run unix socket, so two guests share
/// no host resource except CPU. They are therefore run through the shared
/// weighted-concurrency runner ([`super::parallel`]): a uniprocessor guest is
/// charged two units and an SMP guest the whole budget (so it runs alone),
/// against a budget of one third of the host's logical CPUs
/// ([`qemu_host_budget_for`]). That deliberate headroom keeps the host
/// under-subscribed so no guest — least of all a full-boot SMP one — is
/// starved into missing its own internal real-time deadlines. On a single-core
/// host the budget collapses to one and the matrix runs strictly sequentially.
///
/// Within that budget a few uniprocessor guests overlap, and that does not
/// make a slow guest flaky because the deadline the runner enforces is an
/// *inactivity* budget, not a total-runtime one ([`tairix_qemu`]): a guest
/// that runs a little slower co-scheduled keeps emitting serial output and
/// resets its heartbeat, so it is killed only if it genuinely produces nothing
/// for its whole budget.
/// `only` restricts the pass to the enrolments whose package name contains
/// it — the `--only` debugging filter ([`enrolled`]).
pub fn run_once(ctx: &Context, only: Option<&str>) -> Result<(), String> {
    let selected = enrolled(only)?;
    let target_dir = ctx.target_dir();
    let budget = qemu_host_budget();
    // Resolve each enrolment's memoised (`'static`) `/System`-store bundle
    // set for its own target arch before the jobs are built, so the `'static`
    // job closures capture the resolved [`StoreSet`] (plain slices) by value
    // rather than the borrowed context.
    let jobs: Vec<Job> = selected
        .into_iter()
        .map(|t| {
            let label = format!("test --qemu (run {}) cpus={}", t.package, t.cpus);
            let weight = qemu_job_weight(t.cpus, budget);
            let target_dir = target_dir.clone();
            let stores = stores_for(ctx, t)?;
            // The pull-request matrix runs each enrolment exactly once, so
            // every run is replica zero and keeps the plain sidecar names.
            Ok(Job::closure(label, weight, move || {
                run_one(&target_dir, t, 0, stores)
            }))
        })
        .collect::<Result<Vec<Job>, String>>()?;
    parallel::run(jobs, budget)
}

/// One enrolled QEMU integration test exposed for the long-CI flake hunt
/// ([`super::ci_long`]).
///
/// It carries only what the flake hunt needs — a human label, the
/// emulated-CPU weight the concurrency runner charges against its budget, and
/// a handle to the enrolment itself — so a single enrolment can be run
/// repeatedly without re-exposing the private [`QemuTest`] table. Copy so a
/// per-repetition job factory can capture it freely.
#[derive(Copy, Clone)]
pub(crate) struct Enrolment {
    /// Cargo package name, used to label the flake-hunt jobs.
    pub package: &'static str,
    /// Emulated-CPU count; the concurrency runner's per-job weight, so
    /// concurrent replicas of this test never oversubscribe the host.
    pub cpus: u32,
    /// The enrolment to drive; private so callers go through [`Self::run`].
    test: &'static QemuTest,
}

impl Enrolment {
    /// Drive this enrolment to completion once, exactly as [`run_once`] does,
    /// with no retry. `target_dir` is where the pre-built kernel binaries live
    /// (see [`build_all`]); `stores` is the enrolment's own per-arch
    /// `/System`-store bundle set (see [`Self::stores`] / [`stores_for`]).
    ///
    /// `replica` is which concurrent run of this enrolment this is, so the
    /// flake hunt's simultaneous replicas plant and report to their own
    /// sidecar files instead of overwriting each other's
    /// ([`sidecar_path`]).
    pub(crate) fn run(
        &self,
        target_dir: &Path,
        replica: usize,
        stores: StoreSet,
    ) -> Result<(), String> {
        run_one(target_dir, self.test, replica, stores)
    }

    /// Resolve the `/System`-store bundle sets this enrolment plants,
    /// cross-compiled for its own target arch (see [`stores_for`]). Called
    /// off the `'static` job closure (it needs the build context) so the
    /// closure captures the resolved [`StoreSet`] by value.
    ///
    /// # Errors
    ///
    /// As [`stores_for`].
    pub(crate) fn stores(&self, ctx: &Context) -> Result<StoreSet, String> {
        stores_for(ctx, self.test)
    }
}

/// Every enrolled QEMU integration test, in registry order.
///
/// The single source of truth for the flake hunt's QEMU set is the same
/// `TESTS` table [`run_once`] drives, so the two can never diverge.
pub(crate) fn enrolments() -> Vec<Enrolment> {
    TESTS
        .iter()
        .map(|t| Enrolment {
            package: t.package,
            cpus: t.cpus,
            test: t,
        })
        .collect()
}

/// The whole-disk encrypted-root image an `EncryptedRootDisk` enrolment
/// boots: the shared fixture with the complete self-contained application
/// bundles — each discovered program's signed `AppInfo` + `Run` beside its
/// `Help/` tree — planted on the read-only `/System` volume, exactly as a
/// real image ships them (`plans/APPS.md` deliverable 8).
fn encrypted_root_disk_bytes(
    t: &QemuTest,
    apps: &[super::image_apps::AppStoreFile],
) -> Result<Vec<u8>, String> {
    let plants = root_plants(t, apps)?;
    let borrowed = root_plant_refs(&plants);
    let root_files: Vec<(&[&[u8]], &[u8])> = borrowed
        .iter()
        .map(|(components, bytes)| (components.as_slice(), *bytes))
        .collect();
    super::image_apps::with_plant_refs(apps, |files| {
        tairix_test_encrypted_root_image::build_image_with_apps(files, &root_files).map_err(|e| {
            format!(
                "test --qemu ({}): build encrypted-root image: {e:?}",
                t.package
            )
        })
    })
}

/// The machine-wide program-library catalog to plant on a vertical's
/// **encrypted root volume** (the home of `/System/Settings`, which the
/// writable child mount rebases onto): the root-volume-relative path
/// components of `tairix_proglib::LIBRARY_PATH` plus the document
/// derived from the planted bundles' own manifests through the production
/// `tools/mkimage` derivation — the same document a shipped image seeds —
/// so the guest desktop's Program Library lists the planted graphical
/// bundles (`plans/NEW-TASKBAR.md` T3/T5). One derivation for every
/// encrypted-root builder, never a per-builder copy.
fn library_plant(
    t: &QemuTest,
    apps: &[super::image_apps::AppStoreFile],
) -> Result<(Vec<String>, String), String> {
    let components = root_volume_components(t, tairix_proglib::LIBRARY_PATH)?;
    let conf = super::image_apps::with_plant_refs(apps, |files| {
        tairix_mkimage::library::library_catalog(files).map_err(|e| {
            format!(
                "test --qemu ({}): derive program-library catalog: {e:?}",
                t.package
            )
        })
    })?;
    Ok((components, conf))
}

/// Split an absolute `/System/…` path into the root-volume-relative
/// components a plant names. `/System/Settings` and `/System/Logs` are the
/// writable exceptions rebased onto this volume, so a document planted here
/// is the one a booted guest actually reads — the read-only `/System` volume
/// is shadowed at those prefixes and anything planted there is unreachable.
fn root_volume_components(t: &QemuTest, path: &str) -> Result<Vec<String>, String> {
    let relative = path.strip_prefix('/').ok_or_else(|| {
        format!(
            "test --qemu ({}): root-volume plant path {path} is not absolute",
            t.package
        )
    })?;
    Ok(relative.split('/').map(String::from).collect())
}

/// The `/System/Settings/Configuration/system.conf` document a vertical
/// plants on its **encrypted root volume**, or `None` when the machine is to
/// boot unconfigured.
///
/// A disk that carries a display would meet the graphical login screen, so a
/// vertical whose script drives a *shell* asks for the text prompt outright
/// rather than leaning on whatever the default happens to be — that is a
/// configured machine, not one that cannot run a graphical login. The
/// greeter disk deliberately plants nothing: an unconfigured machine is
/// exactly what it exercises.
///
/// The document is rendered by the configuration engine itself rather than
/// written out as a literal, so it cannot drift from the grammar the guest
/// parses.
fn login_type_plant(t: &QemuTest) -> Result<Option<(Vec<String>, String)>, String> {
    let settings = match t.fs_disk {
        FsDisk::AutoloadRootDisk | FsDisk::HoverRootDisk => tairix_sysconfig::SystemConfig {
            login_type: tairix_sysconfig::LoginType::Text,
            ..tairix_sysconfig::SystemConfig::default()
        },
        // The time vertical's server list lives on this same layer: `timed`
        // reads the store through the ordinary VFS, where `/System/Settings`
        // is the writable sub-mount the encrypted root backs, so a document
        // planted on the read-only `/System` volume would be invisible to it.
        FsDisk::TimeNetRootDisk => {
            tairix_sysconfig::SystemConfig::parse(tairix_test_netstack_wire::TIMED_SYSTEM_CONF)
                .map_err(|e| format!("the time vertical's planted system.conf must parse: {e}"))?
        }
        _ => return Ok(None),
    };
    let components = root_volume_components(t, tairix_sysconfig::CONFIG_PATH)?;
    Ok(Some((components, settings.render())))
}

/// Every document a vertical plants on its encrypted root volume, as the
/// borrowed `(components, bytes)` pairs the image builder takes. The one
/// place root-volume plants are assembled, so each encrypted-root builder
/// lays down the same set.
fn root_plants(
    t: &QemuTest,
    apps: &[super::image_apps::AppStoreFile],
) -> Result<Vec<(Vec<String>, String)>, String> {
    let mut plants = vec![library_plant(t, apps)?];
    plants.extend(login_type_plant(t)?);
    Ok(plants)
}

/// Borrow [`root_plants`] output as the component-slice pairs the image
/// builder's argument type needs.
fn root_plant_refs(plants: &[(Vec<String>, String)]) -> Vec<(Vec<&[u8]>, &[u8])> {
    plants
        .iter()
        .map(|(components, bytes)| {
            (
                components.iter().map(String::as_bytes).collect(),
                bytes.as_bytes(),
            )
        })
        .collect()
}

/// The composed `/System`-store bundle sets one enrolment plants on its
/// backing image, each resolved for the enrolment's own target arch and only
/// when its `fs_disk` actually plants it (empty otherwise, so no arch pays a
/// cross-compile it never uses). Copy `'static` slices, so a job closure
/// captures the set by value without borrowing the build context.
#[derive(Copy, Clone)]
pub(crate) struct StoreSet {
    /// The application/service bundles the encrypted-root, ping, and autoload
    /// plants lay on the read-only `/System` volume; the autoload set
    /// additionally carries a planted `system.conf` asking for the text login.
    apps: &'static [AppStoreFile],
    /// The memsoak-augmented application set the memory-stability vertical
    /// plants.
    apps_with_memsoak: &'static [AppStoreFile],
    /// The application/service bundles the desktop-hover vertical plants: the
    /// shared set plus the test-only `framestats` fixture bundle, which the
    /// seeded program-library catalog is derived from and so lists.
    apps_with_framestats: &'static [AppStoreFile],
    /// The signed autoload driver bundles the `-M virt` autoload verticals
    /// plant in the `/System/Drivers/` store.
    autoload_drivers: &'static [AppStoreFile],
    /// The application/service bundles the stream vertical plants: the shared
    /// set plus the test-only `tcpecho` fixture bundle.
    apps_with_tcpecho: &'static [AppStoreFile],
    /// The application/service bundles the ECN vertical plants: the stream
    /// vertical's `tcpecho`-augmented set plus a planted `system.conf` that
    /// turns `net.tcp.ecn` on stack-wide.
    apps_with_tcpecho_ecn: &'static [AppStoreFile],
    /// The application/service bundles the listener vertical plants: the
    /// shared set plus the test-only `tcpserve` server fixture bundle.
    apps_with_tcpserve: &'static [AppStoreFile],
    /// The signed driver bundles the stream/listener verticals plant: the
    /// virtio-net driver alone (no display/input driver, to keep the UART
    /// console the serial script drives).
    net_only_drivers: &'static [AppStoreFile],
    /// The application/service bundles the static-addressing vertical plants:
    /// the shared set plus the planted `network.conf` (no test-only bundle).
    static_net_apps: &'static [AppStoreFile],
    /// The application/service bundles the bond-failover vertical plants: the
    /// shared set plus the planted bond `network.conf` (no test-only bundle).
    bond_net_apps: &'static [AppStoreFile],
    /// The application/service bundles the DHCPv4 vertical plants: the shared
    /// set plus the planted DHCP `network.conf` (no test-only bundle).
    dhcp_net_apps: &'static [AppStoreFile],
    /// The application/service bundles the DHCPv6 vertical plants: the shared
    /// set plus the planted DHCPv6 `network.conf` (no test-only bundle).
    dhcpv6_net_apps: &'static [AppStoreFile],
}

/// Resolve exactly the `/System`-store bundle sets `t` plants, cross-compiled
/// for the target arch its triple names. Each set is composed only when `t`'s
/// `fs_disk` plants it, so a target never pays a cross-compile it never uses;
/// the underlying builders memoise per arch, so repeated calls are lookups.
///
/// # Errors
///
/// A string naming a target triple this pipeline cannot cross-compile, or a
/// failed bundle composition.
fn stores_for(ctx: &Context, t: &QemuTest) -> Result<StoreSet, String> {
    const EMPTY: &[AppStoreFile] = &[];
    let arch = PieArch::from_target_triple(t.target).ok_or_else(|| {
        format!(
            "test --qemu ({}): target {} is not a freestanding cross-compile target",
            t.package, t.target
        )
    })?;
    // Every QEMU vertical boots a `debug`-profile image (its kernel is the
    // `debug/` build read below, and the seeded `root` account is a
    // debug-only fixture), so its `/System` bundles are composed in the
    // matching Cargo profile — never the shippable `installer` `--release`
    // build.
    let profile = tairix_mkimage::ImageProfile::Debug;
    let apps = match t.fs_disk {
        FsDisk::EncryptedRootDisk
        | FsDisk::NetToolRootDisk
        | FsDisk::AutoloadRootDisk
        | FsDisk::GreeterRootDisk => super::image_apps::app_store_files(ctx, arch, profile)?,
        _ => EMPTY,
    };
    let apps_with_memsoak = match t.fs_disk {
        FsDisk::MemsoakRootDisk => super::image_apps::memsoak_store_files(ctx, arch, profile)?,
        _ => EMPTY,
    };
    let apps_with_framestats = match t.fs_disk {
        FsDisk::HoverRootDisk => super::image_apps::framestats_store_files(ctx, arch, profile)?,
        _ => EMPTY,
    };
    let autoload_drivers = match t.fs_disk {
        FsDisk::AutoloadRootDisk | FsDisk::GreeterRootDisk | FsDisk::HoverRootDisk => {
            super::image_drivers::autoload_driver_store_files(ctx, arch, profile)?
        }
        _ => EMPTY,
    };
    let apps_with_tcpecho = match t.fs_disk {
        FsDisk::StreamRootDisk => super::image_apps::tcpecho_store_files(ctx, arch, profile)?,
        _ => EMPTY,
    };
    let apps_with_tcpecho_ecn = match t.fs_disk {
        FsDisk::EcnRootDisk => super::image_apps::ecn_net_store_files(ctx, arch, profile)?,
        _ => EMPTY,
    };
    let apps_with_tcpserve = match t.fs_disk {
        FsDisk::ListenRootDisk => super::image_apps::tcpserve_store_files(ctx, arch, profile)?,
        _ => EMPTY,
    };
    let net_only_drivers = match t.fs_disk {
        FsDisk::StreamRootDisk
        | FsDisk::EcnRootDisk
        | FsDisk::ListenRootDisk
        | FsDisk::NetToolRootDisk
        | FsDisk::StaticNetRootDisk
        | FsDisk::TimeNetRootDisk
        | FsDisk::BondNetRootDisk
        | FsDisk::DhcpNetRootDisk
        | FsDisk::Dhcp6NetRootDisk => {
            super::image_drivers::net_driver_store_files(ctx, arch, profile)?
        }
        _ => EMPTY,
    };
    let static_net_apps = match t.fs_disk {
        // The time vertical takes the same planted static-addressing
        // `network.conf`: its own extra document is the `system.conf` on the
        // encrypted root, not another `/System` store file.
        FsDisk::StaticNetRootDisk | FsDisk::TimeNetRootDisk => {
            super::image_apps::static_net_store_files(ctx, arch, profile)?
        }
        _ => EMPTY,
    };
    let bond_net_apps = match t.fs_disk {
        FsDisk::BondNetRootDisk => super::image_apps::bond_net_store_files(ctx, arch, profile)?,
        _ => EMPTY,
    };
    let dhcp_net_apps = match t.fs_disk {
        FsDisk::DhcpNetRootDisk => super::image_apps::dhcp_net_store_files(ctx, arch, profile)?,
        _ => EMPTY,
    };
    let dhcpv6_net_apps = match t.fs_disk {
        FsDisk::Dhcp6NetRootDisk => super::image_apps::dhcp6_net_store_files(ctx, arch, profile)?,
        _ => EMPTY,
    };
    Ok(StoreSet {
        apps,
        apps_with_memsoak,
        apps_with_framestats,
        autoload_drivers,
        apps_with_tcpecho,
        apps_with_tcpecho_ecn,
        apps_with_tcpserve,
        net_only_drivers,
        static_net_apps,
        bond_net_apps,
        dhcp_net_apps,
        dhcpv6_net_apps,
    })
}

/// Path of a per-run sidecar file for `t` — a planted backing image, or the
/// run's serial transcript — built beside its kernel binary with extension
/// `ext`. `replica` distinguishes concurrent runs of the same enrolment.
///
/// Two runs must never write to the same sidecar, or the weighted-concurrency
/// runner could let one clobber another's image while its QEMU still has it
/// open, or attribute one run's transcript to another. Two things collide:
///
/// * **Enrolments sharing one built binary** — they drive the *same* guest
///   with different host-side serial scripts (the pre-boot Supervisor
///   verticals, which enter the REPL through different trigger points, are the
///   standing example). When a binary backs more than one enrolment the name
///   carries the entry's stable index in [`TESTS`].
/// * **Concurrent replicas of one enrolment** — the flake hunt
///   ([`super::ci_long`]) runs each enrolment `REPS` times at once, and each
///   replica re-plants its own image, so a shared name would rewrite a live
///   guest's disk underneath it. Replicas past the first carry their index.
///
/// The first replica of a singly-enrolled binary keeps the plain
/// `<binary>.<ext>` name, so the pull-request matrix's paths are unchanged.
fn sidecar_path(kernel: &Path, t: &QemuTest, replica: usize, ext: &str) -> PathBuf {
    use std::fmt::Write as _;

    let shared = TESTS.iter().filter(|e| e.binary == t.binary).count() > 1;
    let mut name = String::new();
    if shared {
        let idx = TESTS.iter().position(|e| std::ptr::eq(e, t)).unwrap_or(0);
        // Writing into a `String` is infallible.
        let _ = write!(name, "s{idx}.");
    }
    if replica > 0 {
        let _ = write!(name, "r{replica}.");
    }
    name.push_str(ext);
    kernel.with_extension(name)
}

fn run_one(
    target_dir: &Path,
    t: &QemuTest,
    replica: usize,
    stores: StoreSet,
) -> Result<(), String> {
    let kernel: PathBuf = target_dir.join(t.target).join("debug").join(t.binary);
    // Select the per-arch QEMU `Spec`: the riscv64 enrolments boot the
    // `virt` board through OpenSBI; everything else uses the x86_64
    // `isa-debug-exit` convention.
    let base = if t.target == RISCV64_TARGET {
        Spec::for_riscv64_kernel(&kernel)
    } else if t.target == AARCH64_TARGET {
        Spec::for_aarch64_kernel(&kernel)
    } else {
        Spec::for_x86_64_kernel(&kernel)
    };
    // One budget everywhere: the enrolment's own inactivity (no-progress)
    // ceiling, enforced identically on a developer machine and a CI runner.
    // There is no developer-only clamp. Because the runner's deadline counts
    // *silence* rather than wall-clock (`tairix_qemu`), a guest that runs a
    // little slower under the modest co-scheduling the 1/3 budget allows keeps
    // emitting output and resets its heartbeat, so the budget can never turn
    // into a load-dependent (flaky) timeout, which the charter forbids. Guest
    // *internal* real-time deadlines are protected separately, by that
    // headroom budget (`qemu_host_budget_for`): SMP guests run alone and the
    // host is never oversubscribed, so no guest is starved into a hard lockup.
    let mut spec = base.with_cpus(t.cpus).with_timeout(t.timeout);
    if let Some(ram_mib) = t.ram_mib {
        spec = spec.with_ram_mib(ram_mib);
    }

    // Attach a planted raw backing image for storage tests. Sector 0
    // carries the deterministic `byte[i] = i mod 256` pattern the
    // kernel-side test reads back and verifies; every other sector
    // reads as zero, so the test's write+read-back of sector 1 cannot
    // pass on stale data.
    if let Some(sectors) = t.disk_sectors {
        let image = sidecar_path(&kernel, t, replica, "blk.img");
        let sector0: Vec<u8> = (0..tairix_qemu::disk::SECTOR_BYTES)
            .map(|i| u8::try_from(i % 256).unwrap_or(0))
            .collect();
        tairix_qemu::disk::plant_raw_disk(&image, sectors, &[(0, &sector0)])
            .map_err(|e| format!("test --qemu ({}): plant backing disk: {e}", t.package))?;
        spec = spec.with_virtio_blk(&image);
    }

    // Attach the shared filesystem volume as the backing image, when the
    // enrolment names one. Only the non-zero sectors are planted; the
    // planter zero-fills the rest, matching a freshly-formatted volume.
    if let Some(fs) = fs_disk_image(t, stores)? {
        let image = sidecar_path(&kernel, t, replica, fs.extension);
        let sector_bytes = tairix_qemu::disk::SECTOR_BYTES;
        let planted: Vec<(u64, &[u8])> = fs
            .bytes
            .chunks(sector_bytes)
            .enumerate()
            .filter(|(_, chunk)| chunk.iter().any(|&b| b != 0))
            .map(|(lba, chunk)| (lba as u64, chunk))
            .collect();
        tairix_qemu::disk::plant_raw_disk(&image, fs.total_sectors, &planted)
            .map_err(|e| format!("test --qemu ({}): plant filesystem disk: {e}", t.package))?;
        spec = spec.with_virtio_blk(&image);
    }

    finish_run(t, &kernel, replica, spec)
}

/// Decode a dumped scan-out and assert the desktop session composited its
/// own wallpaper into it (see [`assert_desktop_wallpaper`], which this
/// decodes for).
fn assert_desktop_screendump(
    t: &QemuTest,
    path: &Path,
    theme: &tairix_theme::Theme,
) -> Result<(), String> {
    let image = read_screendump(t, path)?;
    assert_desktop_wallpaper(t, path, &image, theme, &[])
}

/// Pixels of clearance around a served window's edge: past the
/// anti-aliased rounded corners, the frame, and any shadow the compositor
/// casts, so a sampled pixel is unambiguously either inside the window's
/// body or on the bare desktop beside it.
const WINDOW_EDGE_CLEARANCE_PX: u32 = 16;

/// The decoded `image` is the composited desktop: every sample point that
/// lies in wallpaper-only territory carries exactly the pixel the
/// desktop's own wallpaper draws there. Split out so an assertion that
/// measures several regions of one dump decodes it once.
///
/// # What this proves
///
/// The expected pixels are recomputed on the host by
/// [`expected_wallpaper`] — the shipped default master decoded, placed,
/// and resampled through the very crates the guest's own render path runs
/// — so agreement at a spread of points across the frame can only come
/// from a session that decoded, placed, resampled, and composited that
/// same wallpaper. A boot console left on screen, a blank or flat-filled
/// frame, a session that never composited at all, and a wallpaper drawn at
/// the wrong fit, scale, or offset each differ at these points and are all
/// rejected. Equality is exact: the compositor's opaque blit and the
/// framebuffer encode are byte copies, so a tolerance could only hide a
/// real difference.
///
/// # What this does not prove
///
/// Only the sampled points are judged, and only where nothing may cover
/// the wallpaper: the taskbar's band, a box around the pointer the session
/// parks at the screen centre, a leading margin wide enough for a desktop
/// icon column, and every `excluded` rectangle a caller knows a served
/// window occupies are all skipped ([`desktop_chrome_regions`]). Nothing
/// is asserted about what the desktop draws *there* — icons, chrome, and
/// window content belong to the assertions that measure them.
fn assert_desktop_wallpaper(
    t: &QemuTest,
    path: &Path,
    image: &tairix_qemu::screendump::Image,
    theme: &tairix_theme::Theme,
    excluded: &[tairix_geometry::Rect],
) -> Result<(), String> {
    // A frame so covered that fewer points survive is not one this
    // assertion can judge, and says so rather than passing vacuously.
    const MIN_SAMPLES: usize = 8;
    let wallpaper = expected_wallpaper()?;
    if wallpaper.width != image.width || wallpaper.height != image.height {
        return Err(format!(
            "test --qemu ({}): screendump {} is {}x{}, but the desktop's wallpaper was \
             recomputed for a {}x{} screen",
            t.package,
            path.display(),
            image.width,
            image.height,
            wallpaper.width,
            wallpaper.height,
        ));
    }
    let covered = desktop_chrome_regions(theme, excluded);
    let samples = wallpaper_sample_points(image.width, image.height, &covered);
    if samples.len() < MIN_SAMPLES {
        return Err(format!(
            "test --qemu ({}): screendump {} leaves only {} sampleable wallpaper points \
             (expected >= {MIN_SAMPLES})",
            t.package,
            path.display(),
            samples.len(),
        ));
    }
    for (x, y) in samples {
        let expected = wallpaper.rgb_at(x, y).ok_or_else(|| {
            format!(
                "test --qemu ({}): screendump {}: sample point ({x}, {y}) is outside the \
                 recomputed wallpaper",
                t.package,
                path.display(),
            )
        })?;
        let pixel = image.pixel(x, y).map_err(|e| {
            format!(
                "test --qemu ({}): screendump {} lacks the sampled desktop point ({x}, {y}): {e}",
                t.package,
                path.display(),
            )
        })?;
        if pixel != expected {
            return Err(format!(
                "test --qemu ({}): screendump {} is not the composited desktop: ({x}, {y}) is \
                 {pixel:?}, but the desktop's own wallpaper draws {expected:?} there",
                t.package,
                path.display(),
            ));
        }
    }
    Ok(())
}

/// The regions of a desktop frame something other than bare wallpaper may
/// cover: the taskbar's own band, a box around the pointer the session
/// parks at the screen centre before any motion event, a leading margin
/// wide enough for the desktop's icon column, and every `excluded`
/// rectangle a caller knows a served window occupies (grown by
/// [`WINDOW_EDGE_CLEARANCE_PX`], so a shadow or an anti-aliased corner
/// never reaches a sampled point).
fn desktop_chrome_regions(
    theme: &tairix_theme::Theme,
    excluded: &[tairix_geometry::Rect],
) -> Vec<tairix_geometry::Rect> {
    /// Half-width of the box kept clear of the pointer: comfortably larger
    /// than the cursor artwork at unit scale.
    const CURSOR_CLEARANCE_PX: u32 = 64;
    /// Width of the margin kept clear of the desktop's icon column,
    /// whether or not a fixture's `Desktop` folder holds icons to draw.
    const ICON_COLUMN_PX: u32 = 224;
    let width = tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX;
    let height = tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX;
    let cursor_left = (width / 2).saturating_sub(CURSOR_CLEARANCE_PX);
    let cursor_top = (height / 2).saturating_sub(CURSOR_CLEARANCE_PX);
    let mut regions = vec![
        taskbar_bar_rect(theme),
        tairix_geometry::Rect::new(0, 0, ICON_COLUMN_PX, height),
        tairix_geometry::Rect::new(
            i32::try_from(cursor_left).unwrap_or(0),
            i32::try_from(cursor_top).unwrap_or(0),
            2 * CURSOR_CLEARANCE_PX,
            2 * CURSOR_CLEARANCE_PX,
        ),
    ];
    regions.extend(
        excluded
            .iter()
            .map(|window| grown_by(*window, WINDOW_EDGE_CLEARANCE_PX)),
    );
    regions
}

/// The screen rectangle the taskbar occupies, from the production bar's own
/// layout rather than a hand-copied band: the strip a sampled wallpaper
/// point must stay clear of. Only the bar's own extent is read, which no
/// pin, task, or tray entry moves.
fn taskbar_bar_rect(theme: &tairix_theme::Theme) -> tairix_geometry::Rect {
    let taskbar = tairix_taskbar::Taskbar::new(
        tairix_taskbar::TaskbarConfig::bottom_bar(
            tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX,
            tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX,
        ),
        theme,
    );
    taskbar.layout(tairix_geometry::Scale::ONE).bar
}

/// `rect` grown by `margin` pixels on every side, never starting before the
/// screen origin.
fn grown_by(rect: tairix_geometry::Rect, margin: u32) -> tairix_geometry::Rect {
    let inset = i32::try_from(margin).unwrap_or(0);
    tairix_geometry::Rect::new(
        rect.left().saturating_sub(inset),
        rect.top().saturating_sub(inset),
        rect.width.saturating_add(2 * margin),
        rect.height.saturating_add(2 * margin),
    )
}

/// A lattice of sample points across a `width`×`height` frame, keeping only
/// those outside every `covered` region.
///
/// Spread across the whole frame rather than clustered: the wallpaper is a
/// photograph, so points taken from many parts of it carry many different
/// colours, and a frame showing the wrong picture — or no picture — cannot
/// coincidentally agree with all of them.
fn wallpaper_sample_points(
    width: u32,
    height: u32,
    covered: &[tairix_geometry::Rect],
) -> Vec<(u32, u32)> {
    /// Lattice columns across the frame.
    const COLUMNS: u32 = 11;
    /// Lattice rows down the frame.
    const ROWS: u32 = 9;
    let column_step = width / (COLUMNS + 1);
    let row_step = height / (ROWS + 1);
    let mut points = Vec::new();
    for row in 1..=ROWS {
        for column in 1..=COLUMNS {
            let (x, y) = (column_step * column, row_step * row);
            let point = tairix_geometry::Point::new(
                i32::try_from(x).unwrap_or(i32::MAX),
                i32::try_from(y).unwrap_or(i32::MAX),
            );
            if covered.iter().any(|rect| rect.contains(point)) {
                continue;
            }
            points.push((x, y));
        }
    }
    points
}

/// The wallpaper the desktop session composites behind its icons and
/// windows, recomputed on the host by [`compute_expected_wallpaper`].
///
/// Straight-alpha RGBA8 samples, row-major, `width * height * 4` bytes.
/// Every sample is opaque (checked when the canvas is built), and both the
/// compositor's opaque blit and the framebuffer encode are byte copies, so
/// these are exactly the bytes a dumped scan-out carries wherever nothing
/// covers the wallpaper.
struct ExpectedWallpaper {
    /// Canvas width in pixels.
    width: u32,
    /// Canvas height in pixels.
    height: u32,
    /// Straight-alpha RGBA8 samples, row-major.
    pixels: Vec<u8>,
}

impl ExpectedWallpaper {
    /// The colour at `(x, y)`, or `None` when the point lies off the canvas.
    fn rgb_at(&self, x: u32, y: u32) -> Option<(u8, u8, u8)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = ((y as usize * self.width as usize) + x as usize) * 4;
        let sample = self.pixels.get(offset..offset + 4)?;
        Some((sample[0], sample[1], sample[2]))
    }
}

/// The desktop's default wallpaper as the guest's own pipeline produces it,
/// recomputed once per process and shared by every assertion that reads a
/// desktop frame.
///
/// # Errors
///
/// The recomputation's own message, unchanged on every later call: the
/// shipped master is missing from the graphics assets, the default pinboard
/// no longer draws a wallpaper image, or the decode, placement, or resample
/// of that master refuses.
fn expected_wallpaper() -> Result<&'static ExpectedWallpaper, String> {
    static CANVAS: std::sync::OnceLock<Result<ExpectedWallpaper, String>> =
        std::sync::OnceLock::new();
    CANVAS
        .get_or_init(compute_expected_wallpaper)
        .as_ref()
        .map_err(Clone::clone)
}

/// The shipped master and the fit the desktop's default pinboard comes up
/// with, read from the shared pinboard defaults rather than restated here,
/// so a new default wallpaper or fit reaches this reconstruction and the
/// guest identically.
///
/// # Errors
///
/// A message naming the drift: the default pinboard no longer draws a
/// wallpaper image at all, or the shipped graphics assets carry no master
/// under the default's category and name.
fn default_wallpaper_master() -> Result<
    (
        &'static tairix_syshelp::GraphicsFile,
        tairix_wallpaper::WallpaperFit,
    ),
    String,
> {
    let settings = tairix_wallpaper::PinboardSettings::default();
    if !matches!(
        settings.wallpaper,
        tairix_wallpaper::WallpaperChoice::Image(_)
    ) {
        return Err(format!(
            "desktop wallpaper: the default pinboard draws {:?} rather than a wallpaper image, \
             so a screendump can no longer be judged against one",
            settings.wallpaper,
        ));
    }
    // A master is identified by its category as well as its name: the
    // masters are filed one directory level deep and a name is unique only
    // within its own category.
    let master = tairix_syshelp::GRAPHICS_FILES
        .iter()
        .find(|asset| {
            asset.family == tairix_syshelp::GraphicsFamilyKind::Wallpaper
                && asset.category == Some(tairix_wallpaper::DEFAULT_WALLPAPER_CATEGORY)
                && asset.file == tairix_wallpaper::DEFAULT_WALLPAPER
        })
        .ok_or_else(|| {
            format!(
                "desktop wallpaper: the shipped graphics assets carry no wallpaper master {}/{}",
                tairix_wallpaper::DEFAULT_WALLPAPER_CATEGORY,
                tairix_wallpaper::DEFAULT_WALLPAPER,
            )
        })?;
    Ok((master, settings.fit))
}

/// How a diagnostic names a shipped graphics master: its
/// `<category>/<file>` path where its family files assets in categories,
/// its bare name where the family is flat.
fn master_name(master: &tairix_syshelp::GraphicsFile) -> String {
    match master.category {
        Some(category) => format!("{category}/{}", master.file),
        None => master.file.to_string(),
    }
}

/// Decode `master` at the emulated screen's extent, exactly as the guest's
/// wallpaper renderer does: one fit box of the destination, so the decoder
/// picks the same reduced scale on both sides.
///
/// # Errors
///
/// A message naming the master the desktop's own decoder refuses.
fn decode_wallpaper_master(
    master: &tairix_syshelp::GraphicsFile,
    width: u32,
    height: u32,
) -> Result<tairix_image::RasterImage, String> {
    // The JPEG format's own absolute frame-dimension ceiling (ITU-T T.81
    // B.2.2) with a generous coefficient budget. A decode limit is a
    // refusal ceiling, never an input to the decode's arithmetic, so
    // bounding this reconstruction at the format's own maximum cannot make
    // it disagree with the desktop about any master the desktop accepts.
    const JPEG_FRAME_DIMENSION_LIMIT: u32 = 0xFFFF;
    const COEFFICIENT_BUDGET: u64 = 256 * 1024 * 1024;
    let limits = tairix_image::DecodeLimits::new(
        JPEG_FRAME_DIMENSION_LIMIT,
        JPEG_FRAME_DIMENSION_LIMIT,
        u64::from(JPEG_FRAME_DIMENSION_LIMIT) * u64::from(JPEG_FRAME_DIMENSION_LIMIT),
        COEFFICIENT_BUDGET,
    );
    tairix_image::decode_fitted(
        master.bytes,
        &limits,
        tairix_image::FitBox::new(width, height),
    )
    .map_err(|e| {
        format!(
            "desktop wallpaper: {} does not decode: {e:?}",
            master_name(master)
        )
    })
}

/// Recompute the wallpaper the desktop session paints across the emulated
/// screen: the shipped default master decoded, placed, and resampled
/// through the very crates the guest's own render path runs, so both sides
/// share one definition of the artwork, the placement arithmetic, and the
/// resampler.
///
/// # Errors
///
/// Returns an actionable message when the default picture or fit has
/// drifted ([`default_wallpaper_master`]), the master does not decode, the
/// default fit no longer covers the whole screen in a single non-tiled
/// pass, the placement or resample refuses, or the result is not fully
/// opaque — each of which would make an exact pixel comparison meaningless
/// rather than merely inconvenient.
fn compute_expected_wallpaper() -> Result<ExpectedWallpaper, String> {
    let width = tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX;
    let height = tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX;
    let (master, fit) = default_wallpaper_master()?;
    let decoded = decode_wallpaper_master(master, width, height)?;
    let placement =
        tairix_wallpaper::place((decoded.width(), decoded.height()), (width, height), fit)
            .ok_or_else(|| {
                format!(
                    "desktop wallpaper: {} has no placement on a {width}x{height} screen",
                    master_name(master),
                )
            })?;
    let screen = tairix_geometry::Rect::new(0, 0, width, height);
    if placement.tiled() || placement.destination() != screen {
        return Err(format!(
            "desktop wallpaper: the default {fit:?} fit no longer covers the whole screen in one \
             pass (destination {:?}, tiled {}), so this reconstruction would not be what the \
             desktop draws",
            placement.destination(),
            placement.tiled(),
        ));
    }
    let source = placement.source();
    let master_pixels =
        tairix_raster::Rgba8Image::new(decoded.width(), decoded.height(), decoded.pixels())
            .map_err(|e| {
                format!(
                    "desktop wallpaper: {} decoded to pixels the resampler refuses: {e:?}",
                    master_name(master),
                )
            })?;
    let off_master = || {
        format!(
            "desktop wallpaper: the placement of {} begins outside the master",
            master_name(master),
        )
    };
    let region = tairix_raster::Region {
        x: u32::try_from(source.left()).map_err(|_| off_master())?,
        y: u32::try_from(source.top()).map_err(|_| off_master())?,
        width: source.width,
        height: source.height,
    };
    let pixels = tairix_raster::resample(&master_pixels, region, width, height).map_err(|e| {
        format!(
            "desktop wallpaper: {} does not resample onto the screen: {e:?}",
            master_name(master),
        )
    })?;
    if pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|sample| sample[3] != u8::MAX)
    {
        return Err(format!(
            "desktop wallpaper: {} does not cover the screen opaquely, so what the compositor \
             blends behind it would decide the dumped pixels",
            master_name(master),
        ));
    }
    Ok(ExpectedWallpaper {
        width,
        height,
        pixels,
    })
}

/// Read and fully decode a dumped scan-out image.
fn read_screendump(t: &QemuTest, path: &Path) -> Result<tairix_qemu::screendump::Image, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("test --qemu ({}): read screendump: {e}", t.package))?;
    tairix_qemu::screendump::parse_ppm(&bytes)
        .map_err(|e| format!("test --qemu ({}): decode screendump: {e}", t.package))
}

/// [`ScreendumpPlan`] assertion: the dark-theme composited desktop — the
/// session boots with the shared dark theme active.
fn assert_dark_desktop_screendump(t: &QemuTest, path: &Path) -> Result<(), String> {
    assert_desktop_screendump(t, path, &tairix_theme::Theme::dark())
}

/// [`ScreendumpPlan`] assertion: the served files window on the
/// dark-theme desktop (see [`assert_files_window_screendump`]).
fn assert_files_window_dark_screendump(t: &QemuTest, path: &Path) -> Result<(), String> {
    assert_files_window_screendump(t, path, &tairix_theme::Theme::dark())
}

/// The served files window is on the desktop rendered with `theme`. The
/// desktop's own wallpaper is still composited around the window, and the
/// footprint the session gives the first served window — the cascade
/// origin, the files app's own client size grown by the furniture band,
/// inset to stay clear of the anti-aliased rounded corners — is
/// overwhelmingly *not* that wallpaper: a composited window frame covers
/// it.
fn assert_files_window_screendump(
    t: &QemuTest,
    path: &Path,
    theme: &tairix_theme::Theme,
) -> Result<(), String> {
    let image = read_screendump(t, path)?;
    let window = served_window_layout(
        0,
        tairix_browse::WIN_WIDTH,
        tairix_browse::WIN_HEIGHT,
        tairix_browse::WIN_SIZING.resizable(),
        theme,
    )
    .outer;
    assert_desktop_wallpaper(t, path, &image, theme, &[window])?;
    assert_window_region_covered(t, path, &image, window, "files")
}

/// Where the session composites the `slot`-th window it opens, laid out by
/// the desktop's own rules: the shared cascade placement puts the window's
/// **outer** top-left at that slot's origin, and the window manager reserves
/// its furniture band — the border and title bar above, the thin frame rim on
/// the other three edges — around a client surface of `width`×`height`. A
/// resizable window reserves no more than a fixed one: its grab zone is
/// invisible, overlapping the client's own outer pixels.
///
/// Both rectangles come back so a caller takes the one it means: `outer` is
/// the window's footprint on the screen, which is what a screendump shows,
/// and `client` is the application's own viewport, the only part a scripted
/// click may aim at if it is to reach the app rather than the furniture.
/// Neither is re-derived here — the band is the one the compositor itself
/// decorates with — so a host assertion and the guest cannot disagree about
/// where a window is.
fn served_window_layout(
    slot: u64,
    width: u32,
    height: u32,
    resizable: bool,
    theme: &tairix_theme::Theme,
) -> tairix_controls::FrameLayout {
    let frame = window_frame(resizable);
    let scale = tairix_geometry::Scale::ONE;
    let insets = frame.insets(scale, theme);
    let origin = tairix_desktop_session::windows::cascade_origin_for(slot);
    let outer = tairix_geometry::Rect::new(
        origin.x,
        origin.y,
        width
            .saturating_add(insets.left)
            .saturating_add(insets.right),
        height
            .saturating_add(insets.top)
            .saturating_add(insets.bottom),
    );
    frame.layout(outer, scale, theme)
}

/// The window manager's furniture for a window the session decorates.
///
/// No furniture state moves an edge: `resizable` selects the hit map alone
/// (its resize edges overlap the client rather than widening the band), and
/// activation, movability and the restored/maximized state never did. One
/// definition, so every host-side reconstruction of a window's edges reads
/// the band the compositor itself decorates with.
fn window_frame(resizable: bool) -> tairix_controls::WindowFrame {
    tairix_controls::WindowFrame::new(tairix_controls::WindowFurnitureState {
        resizable,
        ..tairix_controls::WindowFurnitureState::default()
    })
}

/// How far inside its own top-left corner a "focus this window" click aims,
/// in physical pixels.
///
/// Far enough in that anti-aliasing on the client's first pixel column and
/// row cannot put the point on the furniture, that it clears the invisible
/// resize zone over a resizable client's outer pixels, and well inside the
/// smallest client any app opens with. The frame's own hit map pins that
/// clearance in this module's tests, so a deeper grab zone fails the build
/// rather than turning a scripted click into a resize.
const CLIENT_AIM_INSET_PX: i32 = 8;

/// A point inside the client of the `slot`-th window the session opens,
/// without assuming how large that client is.
///
/// [`served_window_layout`] is the right answer for a window whose client
/// size is a compiled-in constant, but the terminal's is not: it sizes
/// itself to what its 80×25 screen measures in the face the guest's font
/// service actually resolves, which no host reconstruction can know. The
/// client's *top-left* needs no such knowledge — it is the cascade slot plus
/// the same furniture band — so a click a short way in from there reaches
/// the application whatever extent it chose.
fn served_client_aim(
    slot: u64,
    resizable: bool,
    theme: &tairix_theme::Theme,
) -> tairix_geometry::Point {
    let insets = window_frame(resizable).insets(tairix_geometry::Scale::ONE, theme);
    let origin = tairix_desktop_session::windows::cascade_origin_for(slot);
    tairix_geometry::Point::new(
        origin.x + i32::try_from(insets.left).unwrap_or(0) + CLIENT_AIM_INSET_PX,
        origin.y + i32::try_from(insets.top).unwrap_or(0) + CLIENT_AIM_INSET_PX,
    )
}

/// The `window` region of the decoded `image` is composited: its inset body
/// is overwhelmingly *not* the wallpaper the desktop draws behind it, so a
/// window frame covers it. `what` names the window in the failure message.
fn assert_window_region_covered(
    t: &QemuTest,
    path: &Path,
    image: &tairix_qemu::screendump::Image,
    window: tairix_geometry::Rect,
    what: &str,
) -> Result<(), String> {
    // Inside the window body — inset from every edge — effectively every
    // pixel belongs to the window's frame; a sliver of tolerance covers
    // the cursor and anti-aliasing if they straddle the inset boundary.
    const MIN_WINDOW_SHARE: f64 = 0.95;
    let wallpaper = expected_wallpaper()?;
    #[allow(clippy::cast_sign_loss)] // A cascade slot is a positive screen offset.
    let (left, top) = (
        window.left() as u32 + WINDOW_EDGE_CLEARANCE_PX,
        window.top() as u32 + WINDOW_EDGE_CLEARANCE_PX,
    );
    let right = left + window.width - 2 * WINDOW_EDGE_CLEARANCE_PX;
    let bottom = top + window.height - 2 * WINDOW_EDGE_CLEARANCE_PX;
    let mut total = 0u64;
    let mut covered = 0u64;
    for y in top..bottom {
        for x in left..right {
            let pixel = image.pixel(x, y).map_err(|e| {
                format!(
                    "test --qemu ({}): screendump {} lacks the served {what} window region: {e}",
                    t.package,
                    path.display(),
                )
            })?;
            total += 1;
            if Some(pixel) != wallpaper.rgb_at(x, y) {
                covered += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)] // Window pixel counts are far below 2^52.
    let share = if total == 0 {
        0.0
    } else {
        covered as f64 / total as f64
    };
    if share < MIN_WINDOW_SHARE {
        return Err(format!(
            "test --qemu ({}): screendump {} shows no served {what} window at its cascade slot: \
             only {share:.3} of the window body differs from the desktop wallpaper behind it \
             (expected >= {MIN_WINDOW_SHARE})",
            t.package,
            path.display(),
        ));
    }
    Ok(())
}

/// The centre of `rect` in screen coordinates — where a pointer script aims
/// to hit the region the desktop's own layout code placed there. An empty
/// rectangle is a reconstruction failure, named by `what`, not a click at
/// the origin.
fn rect_centre(rect: tairix_geometry::Rect, what: &str) -> Result<tairix_geometry::Point, String> {
    if rect.is_empty() {
        return Err(format!("desktop pointer script: {what} region is empty"));
    }
    #[allow(clippy::cast_possible_wrap)] // Screen extents are far below i32::MAX.
    Ok(tairix_geometry::Point::new(
        rect.left() + (rect.width / 2) as i32,
        rect.top() + (rect.height / 2) as i32,
    ))
}

/// The smallest share of the bar's first application slot a drawn slot must
/// cover: a resting slot draws no plate at all, only its centred class glyph
/// on the bar's own fill, and that glyph is a shape rather than a filled box.
/// Well under what a glyph covers, and far above the nothing an empty slot
/// covers.
const MIN_APP_GLYPH_SHARE: f64 = 0.05;

/// The most of an *unoccupied* application slot that may differ from the same
/// slot before anything was running.
///
/// Zero: the bar is translucent chrome over a blurred backdrop, so an empty
/// slot's pixels are a function of the wallpaper and of whatever is behind
/// the bar — and the vertical's windows cascade from the top left and never
/// reach it. Nothing else moves there, so reading the same empty slot in two
/// frames of one run gives the same bytes, and any difference at all is a
/// slot the bar drew.
const MAX_BARE_APP_SLOT_SHARE: f64 = 0.0;

/// The strip index the icon-bar vertical's launched application occupies.
///
/// One, not zero: the file manager is a core desktop component the session
/// autostarts at bring-up, and it holds the leading slot for the life of the
/// session, so anything launched afterwards lands beside it. Reading slot
/// zero would drive and measure the file manager instead of the application
/// under test.
const APPBAR_LAUNCHED_SLOT: usize = 1;

/// The strip index that must stay empty: the one beyond the launched
/// application's. A regression that gave every *window* its own slot would
/// draw here.
const APPBAR_EMPTY_SLOT: usize = APPBAR_LAUNCHED_SLOT + 1;

/// The icon-bar vertical's screendump names. The frames carrying the running
/// application are read against the bare one, so the set is named here rather
/// than only in the plan that schedules them.
const APPBAR_BARE_BAR_DUMP: &str = "bare-bar";
const APPBAR_ONE_WINDOW_DUMP: &str = "one-window";
const APPBAR_TWO_WINDOWS_DUMP: &str = "two-windows";

/// The screen rectangle of the bar's application slot at `index`,
/// reconstructed through the production taskbar's own layout code.
///
/// The strip is laid out from its leading edge, one `app_extent` per slot, so
/// a slot's rectangle depends only on its index — which is what lets the same
/// helper name both the slot the running application occupies and the one
/// beside it, where a second slot *would* be drawn if the bar wrongly showed
/// windows rather than applications. Only the strip's *length* reaches the
/// layout, so a bare slot places identically to a fully resolved one.
fn appbar_slot_rect(
    theme: &tairix_theme::Theme,
    index: usize,
) -> Result<tairix_geometry::Rect, String> {
    use tairix_taskbar::{AppSlot, Taskbar, TaskbarConfig};
    let mut taskbar = Taskbar::new(
        TaskbarConfig::bottom_bar(
            tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX,
            tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX,
        ),
        theme,
    );
    taskbar.set_apps(
        (0..=index)
            .map(|_| {
                AppSlot::new(
                    tairix_test_appbar_qemu_aarch64::BAR_APP_NAME,
                    tairix_icon::IconKind::AppBundle,
                )
            })
            .collect(),
    );
    taskbar
        .layout(tairix_geometry::Scale::ONE)
        .apps
        .get(index)
        .copied()
        .ok_or_else(|| format!("icon-bar script: the bar reserves no application slot {index}"))
}

/// The share of the bar's application slot `index` whose pixels differ
/// between two frames of one run.
///
/// The bar is floating chrome: a translucent fill over a backdrop the
/// compositor blurs, so a slot has no single expected colour to test against.
/// What it does have is the pixels that very slot showed in another frame of
/// the same run. Reading one screen position across two frames holds the
/// wallpaper, the blur, and the bar's own fill fixed, so what is left is the
/// slot's own content — which is what makes the figure a direct measure of
/// change rather than of a colour the bar no longer has. Read against a bare
/// frame it measures what a gesture put in a slot; read across two running
/// frames it measures what the desktop did to a slot it was not asked to
/// touch.
fn app_slot_pixel_drift(
    t: &QemuTest,
    path: &Path,
    frames: (
        &tairix_qemu::screendump::Image,
        &tairix_qemu::screendump::Image,
    ),
    theme: &tairix_theme::Theme,
    index: usize,
) -> Result<f64, String> {
    let (image, bare) = frames;
    let slot = appbar_slot_rect(theme, index)?;
    #[allow(clippy::cast_sign_loss)] // The bar's slots are at positive screen offsets.
    let (left, top) = (slot.left() as u32, slot.top() as u32);
    let mut total = 0u64;
    let mut glyph = 0u64;
    for y in top..top + slot.height {
        for x in left..left + slot.width {
            let read = |from: &tairix_qemu::screendump::Image| {
                from.pixel(x, y).map_err(|e| {
                    format!(
                        "test --qemu ({}): screendump {} lacks the bar's first application slot: \
                         {e}",
                        t.package,
                        path.display(),
                    )
                })
            };
            total += 1;
            if read(image)? != read(bare)? {
                glyph += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)] // Slot pixel counts are far below 2^52.
    Ok(if total == 0 {
        0.0
    } else {
        glyph as f64 / total as f64
    })
}

/// The dump of the same run whose suffix is `baseline` rather than `path`'s
/// own, beside `path`.
///
/// The runner names a dump after the plan entry that scheduled it, so a later
/// frame's own path names the earlier one it is read against. A `path` that
/// carries none of the `taken` suffixes is a wiring mistake and fails closed
/// rather than reading some other frame.
fn baseline_dump_path(
    t: &QemuTest,
    path: &Path,
    taken: &[&str],
    baseline: &str,
) -> Result<PathBuf, String> {
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        format!(
            "test --qemu ({}): screendump {} has no readable file name",
            t.package,
            path.display(),
        )
    })?;
    let mut named = name.to_string();
    for suffix in taken {
        named = named.replace(&format!(".{suffix}."), &format!(".{baseline}."));
    }
    if named == name {
        return Err(format!(
            "test --qemu ({}): screendump {} is not one of the frames read against the \
             {baseline} frame, so the baseline it is compared with cannot be named",
            t.package,
            path.display(),
        ));
    }
    Ok(path.with_file_name(named))
}

/// The bare-bar dump the icon-bar vertical's running-application frames are
/// read against.
fn bare_bar_dump_path(t: &QemuTest, path: &Path) -> Result<PathBuf, String> {
    baseline_dump_path(
        t,
        path,
        &[APPBAR_ONE_WINDOW_DUMP, APPBAR_TWO_WINDOWS_DUMP],
        APPBAR_BARE_BAR_DUMP,
    )
}

/// [`ScreendumpPlan`] assertion for the icon-bar vertical's **first** dump,
/// taken on the first fully-revealed desktop frame: the dark-theme session
/// has composited its own wallpaper, and the bar carries no application the
/// script has launched yet.
///
/// This frame is also the baseline the later dumps are read against
/// ([`app_slot_pixel_drift`]), which is what turns "a glyph is in the slot"
/// into "this run's gesture put it there": the frames are the same screen, so
/// an application slot that differs between them differs *because of the
/// launch*.
fn assert_bare_bar_dark_screendump(t: &QemuTest, path: &Path) -> Result<(), String> {
    let theme = tairix_theme::Theme::dark();
    let image = read_screendump(t, path)?;
    assert_desktop_wallpaper(t, path, &image, &theme, &[])
}

/// [`ScreendumpPlan`] assertion for the icon-bar vertical's **second** dump,
/// taken once the launched application's first window has been created and
/// painted: the bar's first application slot now carries that application's
/// glyph, its window covers the first cascade slot, and the composited
/// desktop is still behind them.
///
/// Unlike the first dump this one deliberately does **not** sample the
/// wallpaper across the whole frame: the window covers a large part of this
/// output. The desktop's presence is measured where it is genuinely expected
/// instead — exactly, over the bare column beside the window.
fn assert_one_window_dark_screendump(t: &QemuTest, path: &Path) -> Result<(), String> {
    let theme = tairix_theme::Theme::dark();
    let image = read_screendump(t, path)?;
    let bare = read_screendump(t, &bare_bar_dump_path(t, path)?)?;
    assert_app_slot_drawn(t, path, (&image, &bare), &theme)?;
    assert_no_slot_beyond_the_launched_app(t, path, (&image, &bare), &theme)?;
    assert_cascade_slot_covered(t, path, &image, &theme, 0)
}

/// [`ScreendumpPlan`] assertion for the icon-bar vertical's **third** dump,
/// taken once the chosen *New window* row has opened a second window: both
/// cascade slots are covered, and the one application still holds exactly one
/// slot on the bar.
///
/// That last fact is the point of the frame: the bar shows *applications*, so
/// a second window of one application must not put a second slot beside it.
fn assert_two_windows_dark_screendump(t: &QemuTest, path: &Path) -> Result<(), String> {
    let theme = tairix_theme::Theme::dark();
    let image = read_screendump(t, path)?;
    let bare = read_screendump(t, &bare_bar_dump_path(t, path)?)?;
    assert_app_slot_drawn(t, path, (&image, &bare), &theme)?;
    assert_no_slot_beyond_the_launched_app(t, path, (&image, &bare), &theme)?;
    for slot in 0..2 {
        assert_cascade_slot_covered(t, path, &image, &theme, slot)?;
    }
    Ok(())
}

/// The desktop-under-pressure vertical's screendump names: the frame taken
/// before the script has spent any memory, and the frame taken once every
/// window is open and the machine has left the normal pressure band.
const DESKTOP_PRESSURE_ICONS_DRAWN_DUMP: &str = "icons-drawn";
const DESKTOP_PRESSURE_UNDER_PRESSURE_DUMP: &str = "under-pressure";

/// The strip index the desktop-under-pressure vertical reads: the leading
/// slot, the file manager the session autostarts.
///
/// It is the one slot in the frame the script never points at, clicks, hovers,
/// or launches, and its application neither opens nor closes a window for the
/// whole run — so its picture is a function of the desktop's own state and of
/// nothing the run did to it.
const DESKTOP_PRESSURE_UNTOUCHED_SLOT: usize = 0;

/// The most of the untouched slot that may differ between the two frames.
///
/// Zero. The slot is the same screen position in two frames of one run, with
/// the same wallpaper behind it, the same bar fill over it, the same
/// application in it, and no pointer near it; the vertical's windows cascade
/// from the top left and never reach the bar. So the bytes are the same bytes
/// — unless the desktop drew something else there, which under pressure means
/// it gave up the decoded artwork and fell back to a built-in glyph.
///
/// It is the right bound for the bands `plans/ICONS.md` promises the artwork
/// through — mild and moderate leave the cache alone — and it is asserted only
/// for a run that stayed within them
/// ([`PRESSURE_DEEPENED_MARKER`](tairix_test_desktop_pressure_qemu_aarch64::PRESSURE_DEEPENED_MARKER)).
const MAX_UNDER_PRESSURE_SLOT_DRIFT: f64 = 0.0;

/// [`ScreendumpPlan`] assertion for the desktop-under-pressure vertical's
/// **first** dump, taken on the first fully-revealed desktop frame: a real
/// composited dark-theme desktop, with the autostarted file manager already
/// holding the leading slot.
///
/// This frame is the artwork baseline the second one is read against.
fn assert_icons_drawn_dark_screendump(t: &QemuTest, path: &Path) -> Result<(), String> {
    let theme = tairix_theme::Theme::dark();
    let image = read_screendump(t, path)?;
    assert_desktop_wallpaper(t, path, &image, &theme, &[])
}

/// Whether the run that produced `path` reported its published pressure band
/// deepening past moderate.
///
/// Read from the persisted transcript, which the runner writes before it judges
/// a pass's dumps. A missing transcript is not treated as "stayed shallow": the
/// strict assertion would then be applied to a run whose bands are unknown, so
/// an unreadable log fails closed.
fn pressure_deepened_past_moderate(t: &QemuTest, path: &Path) -> Result<bool, String> {
    let log = sibling_serial_log(t, path)?;
    let text = std::fs::read_to_string(&log).map_err(|e| {
        format!(
            "test --qemu ({}): read the transcript {} to scope the artwork assertion: {e}",
            t.package,
            log.display(),
        )
    })?;
    Ok(text.contains(tairix_test_desktop_pressure_qemu_aarch64::PRESSURE_DEEPENED_MARKER))
}

/// The `serial.log` sidecar beside the screendump `path`.
fn sibling_serial_log(t: &QemuTest, path: &Path) -> Result<PathBuf, String> {
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        format!(
            "test --qemu ({}): screendump {} has no readable file name",
            t.package,
            path.display(),
        )
    })?;
    let stem = name
        .split_once(&format!(".{DESKTOP_PRESSURE_UNDER_PRESSURE_DUMP}."))
        .map(|(head, _)| head)
        .ok_or_else(|| {
            format!(
                "test --qemu ({}): screendump {} is not the under-pressure frame, so its transcript cannot be named",
                t.package,
                path.display(),
            )
        })?;
    Ok(path.with_file_name(format!("{stem}.serial.log")))
}

/// [`ScreendumpPlan`] assertion for the desktop-under-pressure vertical's
/// **second** dump, taken once every window is open and the guest has
/// witnessed the machine leave the normal pressure band: the icon bar still
/// draws exactly the artwork it drew before.
fn assert_bar_artwork_survived_screendump(t: &QemuTest, path: &Path) -> Result<(), String> {
    // At severe and critical pressure the desktop gives the decoded artwork up
    // and the built-in glyph is the honest answer (`plans/ICONS.md`), so the
    // slot legitimately differs and there is nothing here to assert. The band
    // is not steerable, and how deep a run goes turns on how much retained
    // content it accumulated — which is why asserting across every band failed
    // a busy host and held an idle one.
    if pressure_deepened_past_moderate(t, path)? {
        return Ok(());
    }
    let theme = tairix_theme::Theme::dark();
    let image = read_screendump(t, path)?;
    let drawn = read_screendump(
        t,
        &baseline_dump_path(
            t,
            path,
            &[DESKTOP_PRESSURE_UNDER_PRESSURE_DUMP],
            DESKTOP_PRESSURE_ICONS_DRAWN_DUMP,
        )?,
    )?;
    let drift = app_slot_pixel_drift(
        t,
        path,
        (&image, &drawn),
        &theme,
        DESKTOP_PRESSURE_UNTOUCHED_SLOT,
    )?;
    if drift > MAX_UNDER_PRESSURE_SLOT_DRIFT {
        return Err(format!(
            "test --qemu ({}): screendump {}: {:.1}% of the icon bar's untouched application \
             slot changed while the machine was under memory pressure (at most {:.1}% may) — \
             the desktop stopped drawing its decoded icon artwork",
            t.package,
            path.display(),
            drift * 100.0,
            MAX_UNDER_PRESSURE_SLOT_DRIFT * 100.0,
        ));
    }
    Ok(())
}

/// Assert a window body covers the cascade slot `slot` of the decoded
/// `image`: a probe square just inside that slot's client origin is
/// overwhelmingly *not* the wallpaper the desktop draws behind it.
///
/// A probe rather than the whole window rectangle, because the terminal's
/// window is whatever its character grid measures in the face the running
/// font service resolved — a size no host reconstruction can know. The
/// cascade *origin* and the frame's own insets are the session's and the
/// window manager's, so the corner the probe reads is exact.
fn assert_cascade_slot_covered(
    t: &QemuTest,
    path: &Path,
    image: &tairix_qemu::screendump::Image,
    theme: &tairix_theme::Theme,
    slot: u64,
) -> Result<(), String> {
    /// Side of the probe square, in pixels: comfortably inside the smallest
    /// window the terminal can open (one character cell plus its furniture).
    const PROBE_PX: u32 = 24;
    let aim = served_client_aim(slot, tairix_terminal::WIN_RESIZABLE, theme);
    #[allow(clippy::cast_sign_loss)] // A cascade slot is a positive screen offset.
    let probe = tairix_geometry::Rect::new(aim.x, aim.y, PROBE_PX, PROBE_PX);
    let wallpaper = expected_wallpaper()?;
    #[allow(clippy::cast_sign_loss)] // A cascade slot is a positive screen offset.
    let (left, top) = (probe.left() as u32, probe.top() as u32);
    let mut covered = 0u64;
    let mut total = 0u64;
    for y in top..top + probe.height {
        for x in left..left + probe.width {
            let pixel = image.pixel(x, y).map_err(|e| {
                format!(
                    "test --qemu ({}): screendump {} lacks cascade slot {slot}: {e}",
                    t.package,
                    path.display(),
                )
            })?;
            total += 1;
            if Some(pixel) != wallpaper.rgb_at(x, y) {
                covered += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)] // Probe pixel counts are tiny.
    let share = if total == 0 {
        0.0
    } else {
        covered as f64 / total as f64
    };
    if share < 0.95 {
        return Err(format!(
            "test --qemu ({}): screendump {} shows no window over cascade slot {slot}: only \
             {share:.3} of the probe differs from the wallpaper behind it",
            t.package,
            path.display(),
        ));
    }
    Ok(())
}

/// Assert the launched application's strip slot carries a drawn glyph,
/// measured against the same slot in the bare frame.
///
/// The slot read is the launched application's own
/// ([`APPBAR_LAUNCHED_SLOT`]), not the strip's first: the autostarted file
/// manager already holds that one in the bare frame, so reading it would
/// compare a glyph against itself and prove nothing.
fn assert_app_slot_drawn(
    t: &QemuTest,
    path: &Path,
    frames: (
        &tairix_qemu::screendump::Image,
        &tairix_qemu::screendump::Image,
    ),
    theme: &tairix_theme::Theme,
) -> Result<(), String> {
    let share = app_slot_pixel_drift(t, path, frames, theme, APPBAR_LAUNCHED_SLOT)?;
    if share < MIN_APP_GLYPH_SHARE {
        return Err(format!(
            "test --qemu ({}): screendump {} shows no application in the bar's slot \
             {APPBAR_LAUNCHED_SLOT}: only {share:.3} of the slot differs from the same slot \
             before anything was launched (expected >= {MIN_APP_GLYPH_SHARE})",
            t.package,
            path.display(),
        ));
    }
    Ok(())
}

/// Assert the bar has drawn **no** slot beyond the launched application's:
/// the strip's next slot along is exactly as it was before anything was
/// launched.
///
/// This is what "the bar shows applications, not windows" means as a claim
/// about pixels, and it is the reason the two-window frame is taken at all.
/// Without it a regression that gave every window its own slot would still
/// satisfy every other assertion in this vertical, because they all read the
/// launched application's own slot.
fn assert_no_slot_beyond_the_launched_app(
    t: &QemuTest,
    path: &Path,
    frames: (
        &tairix_qemu::screendump::Image,
        &tairix_qemu::screendump::Image,
    ),
    theme: &tairix_theme::Theme,
) -> Result<(), String> {
    let share = app_slot_pixel_drift(t, path, frames, theme, APPBAR_EMPTY_SLOT)?;
    if share > MAX_BARE_APP_SLOT_SHARE {
        return Err(format!(
            "test --qemu ({}): screendump {} shows an application slot beyond the launched \
             one: {share:.3} of slot {APPBAR_EMPTY_SLOT} differs from the same slot before \
             anything was launched (expected <= {MAX_BARE_APP_SLOT_SHARE}). One \
             application's windows must share one slot",
            t.package,
            path.display(),
        ));
    }
    Ok(())
}

/// Where the desktop draws the taskbar clock and, once that clock is
/// right-clicked, the *Set Date & Time…* row of the menu it opens.
///
/// Both points come from driving the **production** taskbar model at run
/// time — the same layout, hit-testing, and menu-building code the guest
/// renders with — so the script clicks where the guest actually draws rather
/// than at coordinates copied from a screenshot.
fn datetime_elevate_aim_points() -> Result<(tairix_geometry::Point, tairix_geometry::Point), String>
{
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_desktop_session::DesktopShell;
    use tairix_geometry::{Point, Scale};
    use tairix_input::{InputEvent, PointerButton};
    use tairix_log::DiscardSink;
    use tairix_reclaim::ReportedPressure;
    use tairix_taskbar::{TaskbarConfig, TaskbarInput};

    static NO_PRESSURE_FEED: ReportedPressure = ReportedPressure::unknown();
    static DISCARD_SINK: DiscardSink = DiscardSink;

    let width = tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX;
    let height = tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX;
    let scale = Scale::ONE;
    let now_ns: u64 = 0;
    let mut shell = DesktopShell::new(
        TaskbarConfig::bottom_bar(width, height),
        SEAT_PRIMARY,
        0,
        &NO_PRESSURE_FEED,
        &DISCARD_SINK,
    );
    // The guest attests a broker, so the set-time row is actionable and
    // lays out exactly as the host reconstructs it.
    shell
        .session_mut()
        .taskbar_mut()
        .set_elevation_available(true);

    let mut router = TaskbarInput::new();
    let mut press_at = |shell: &mut DesktopShell, at: Point, button: PointerButton| {
        let taskbar = shell.session_mut().taskbar_mut();
        router.handle(InputEvent::PointerMoved { to: at }, taskbar, scale, now_ns);
        router.handle(
            InputEvent::PointerPressed { button },
            taskbar,
            scale,
            now_ns,
        );
        router.handle(
            InputEvent::PointerReleased { button },
            taskbar,
            scale,
            now_ns,
        );
    };

    let bar = shell.session().taskbar().layout(scale);
    let clock = rect_centre(bar.clock, "clock")?;

    // Open the clock menu exactly as the first click will. A menu is what a
    // secondary press asks for: the clock is a reading, and a primary press
    // on it is claimed and inert.
    press_at(&mut shell, clock, PointerButton::Secondary);
    if !shell.session().taskbar().menu().is_open() {
        return Err(
            "datetime-elevate script: a secondary press on the clock opened no menu".to_string(),
        );
    }
    let menu_layout = shell
        .session()
        .taskbar()
        .menu_layout(scale)
        .ok_or_else(|| "datetime-elevate script: the open menu lays nothing out".to_string())?;
    let control = shell.session().taskbar().menu().control();
    let set_label = tairix_taskbar::clock_menu::SET_ROW_LABEL;
    let row = control
        .items()
        .iter()
        .position(|item| item.label() == set_label)
        .ok_or_else(|| format!("datetime-elevate script: the menu has no {set_label:?} row"))?;
    let set_row = rect_centre(
        control
            .row_rect(
                row,
                menu_layout.panel,
                scale,
                shell.session().taskbar().theme(),
            )
            .ok_or_else(|| {
                "datetime-elevate script: the set-time row lays nothing out".to_string()
            })?,
        "Set Date & Time row",
    )?;
    Ok((clock, set_row))
}

/// A pointer script under construction, tracking where it left the guest's
/// pointer.
///
/// The QEMU monitor moves the pointer by a *delta*, so a script that names
/// screen positions must remember the last one it aimed at; every script here
/// does, and each one open-coding that bookkeeping is how two of them come to
/// disagree about it.
struct PointerPen {
    at: tairix_geometry::Point,
    steps: Vec<tairix_qemu::PointerStep>,
}

impl PointerPen {
    /// A pen holding the pointer at the screen origin, gated on `marker`.
    ///
    /// The session centres the pointer at bring-up, so the opening move
    /// overshoots both axes leftward and upward on a `screen` of that size;
    /// the guest clamps at the origin, which is what makes every later
    /// displacement exact.
    fn pinned_at_origin(marker: &str, screen: (u32, u32)) -> Self {
        #[allow(clippy::cast_possible_wrap)] // Screen extents are far below i32::MAX.
        let overshoot = tairix_qemu::PointerAction::Move {
            dx: -(2 * screen.0 as i32),
            dy: -(2 * screen.1 as i32),
        };
        let mut pen = Self {
            at: tairix_geometry::Point::ORIGIN,
            steps: Vec::new(),
        };
        pen.push(marker, 1, overshoot);
        pen
    }

    /// Move the pointer to `to` once `marker` has appeared `occurrences`
    /// times.
    fn aim(&mut self, marker: &str, occurrences: u32, to: tairix_geometry::Point) {
        let delta = tairix_qemu::PointerAction::Move {
            dx: to.x - self.at.x,
            dy: to.y - self.at.y,
        };
        self.at = to;
        self.push(marker, occurrences, delta);
    }

    /// Sweep the pointer to `to` in `samples` evenly-spaced moves, every one
    /// gated on the same witness.
    ///
    /// A hover is a *run* of motion, not a jump: a single move reports one
    /// arrival, where crossing a strip of controls is what makes each of them
    /// report the enter and leave a repaint is owed for. The last sample lands
    /// exactly on `to`, so the pen's bookkeeping stays exact however the
    /// interpolation rounds.
    fn hover(&mut self, marker: &str, occurrences: u32, to: tairix_geometry::Point, samples: u32) {
        let from = self.at;
        let steps = samples.max(1);
        for step in 1..=steps {
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            // A sample index is bounded by `samples`, far below `i32::MAX`.
            let lerp =
                |start: i32, end: i32| start + (end - start) * (step as i32) / (steps as i32);
            self.aim(
                marker,
                occurrences,
                tairix_geometry::Point::new(lerp(from.x, to.x), lerp(from.y, to.y)),
            );
        }
    }

    /// Aim at `to` and click `button` there, both gated on the same witness.
    fn click(
        &mut self,
        marker: &str,
        occurrences: u32,
        button: tairix_qemu::MouseButton,
        to: tairix_geometry::Point,
    ) {
        self.aim(marker, occurrences, to);
        self.push(
            marker,
            occurrences,
            tairix_qemu::PointerAction::Click(button),
        );
    }

    /// The ordered script.
    fn steps(self) -> Vec<tairix_qemu::PointerStep> {
        self.steps
    }

    fn push(&mut self, marker: &str, occurrences: u32, action: tairix_qemu::PointerAction) {
        self.steps.push(tairix_qemu::PointerStep {
            ready_marker: marker.to_owned(),
            ready_occurrences: occurrences.max(1),
            action,
        });
    }
}

/// Right-click the taskbar clock, then click the *Set Date & Time…* row of the
/// menu that opens, so the session opens its credential prompt.
fn datetime_elevate_pointer_script() -> Result<Vec<tairix_qemu::PointerStep>, String> {
    use tairix_qemu::MouseButton;

    let (clock, set_row) = datetime_elevate_aim_points()?;
    let ready = AUTOLOAD_DESKTOP_REVEALED_MARKER;
    let mut pen = PointerPen::pinned_at_origin(ready, ramfb_screen());
    pen.click(ready, 1, MouseButton::Secondary, clock);
    pen.click(ready, 1, MouseButton::Primary, set_row);
    Ok(pen.steps())
}

/// Build the desktop-hover script: launch the `framestats` fixture from the
/// program library, sweep the pointer the whole length of the icon bar, then
/// launch it again.
///
/// The two launches are what bracket the sweep. Pointer steps fire **strictly
/// in script order**, so the sweep provably lies between the samples the two
/// runs take — no marker has to say when a hover ended, which is just as well,
/// because nothing observable says it. The gate then judges the work the
/// desktop did inside that window; a whole-epoch figure could not, because
/// bring-up's own full-screen frames own both its mean and its peak.
///
/// The sweep runs along the bar's own centre line from the leading end to the
/// trailing one, so it crosses every control the bar draws — the launcher
/// button, the running-application slots, the notification area, the clock,
/// and the Switchboard capsule — in one pass. Those endpoints are read from
/// the **production** bar layout, driven here over the shared ramfb console
/// geometry, so the sweep tracks the bar the guest actually draws rather than
/// coordinates copied from a screenshot. The bar is a desktop surface, never
/// covered by a window, which is why the gesture aims at it: a window's
/// controls would need whatever else is on screen reasoned about first, and
/// the per-control damage under test is the same sink either way.
///
/// Every gate is causal:
///
/// - The desktop's own reveal witness opens the script, so nothing is injected
///   before there is a bar to hit.
/// - The row click follows its library click immediately — that press is what
///   opens the popup, so the row is on screen by construction, and the guest
///   applies injected events strictly in device order.
/// - Everything after the first launch waits on the fixture's own sample
///   record: until that record exists there is no opened window to sweep
///   inside.
///
/// The final click is what completes the run: it launches the second sample,
/// whose record is the gate's verdict.
fn desktop_hover_pointer_script() -> Result<Vec<tairix_qemu::PointerStep>, String> {
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_desktop_session::DesktopShell;
    use tairix_geometry::Point;
    use tairix_log::DiscardSink;
    use tairix_qemu::MouseButton;
    use tairix_reclaim::ReportedPressure;
    use tairix_taskbar::{AppSlot, LibraryRow, TaskbarConfig};
    use tairix_test_desktop_hover_qemu_aarch64::{SAMPLE_APP_NAME, SWEEP_MOVES};

    // The shell exists only to reproduce the guest's layout arithmetic. It
    // rasterises nothing and owns no display, so it is wired truthfully rather
    // than plausibly: no display backing, and a gauge that has never been told
    // a band (which answers critical, so nothing is admitted).
    static NO_PRESSURE_FEED: ReportedPressure = ReportedPressure::unknown();
    static DISCARD_SINK: DiscardSink = DiscardSink;

    let (width, height) = ramfb_screen();
    let scale = RECONSTRUCTION_SCALE;
    let mut shell = DesktopShell::new(
        TaskbarConfig::bottom_bar(width, height),
        SEAT_PRIMARY,
        0,
        &NO_PRESSURE_FEED,
        &DISCARD_SINK,
    );
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(reconstructed_library(&[
            super::image_apps::FRAMESTATS_FIXTURE_CRATE,
        ])?);
    // The file manager is autostarted at bring-up, so the bar already carries
    // its slot by the time the script runs. The sweep's endpoints sit at the
    // bar's two ends and so do not move with the strip, but the strip is
    // modelled anyway: a sweep that did not cross a real slot would not
    // exercise the hover the gate is about.
    shell
        .session_mut()
        .taskbar_mut()
        .set_apps(vec![AppSlot::new(
            FILES_BAR_APP_NAME,
            tairix_icon::IconKind::AppBundle,
        )]);

    let taskbar = shell.session().taskbar();
    let bar = taskbar.layout(scale);
    let library_button = rect_centre(bar.library, "Library button")?;
    let sweep_end = rect_centre(bar.switchboard, "Switchboard capsule")?;
    // The sweep walks the bar's own centre line: its ends are the launcher
    // button and the Switchboard capsule, which anchor the two ends of the
    // bar, so one pass between them crosses everything drawn in between.
    let sweep_start = Point::new(library_button.x, sweep_end.y);

    // The popup's row for the fixture bundle, keyed by the bundle it launches
    // — the same on-disk identity the guest attributes the samples to — never
    // a display-name literal.
    let bundle = format!(
        "{}/{SAMPLE_APP_NAME}{}",
        tairix_abi::SYSTEM_COMMAND_STORE,
        tairix_abi::BUNDLE_SUFFIX
    );
    let library = taskbar.library();
    let row = library
        .rows()
        .iter()
        .position(|row| match row {
            LibraryRow::Entry { id, .. } => library
                .catalog()
                .entry(id)
                .is_some_and(|entry| entry.bundle().as_str() == bundle),
            LibraryRow::Folder { .. } => false,
        })
        .ok_or_else(|| format!("hover script: {bundle} is not listed in the program library"))?;
    let sample_row = rect_centre(
        taskbar
            .library_layout(scale)
            .rows
            .iter()
            .find(|(shown, _)| *shown == row)
            .map(|(_, rect)| *rect)
            .ok_or_else(|| {
                "hover script: the fixture's row is not visible in the popup".to_string()
            })?,
        "framestats library entry",
    )?;

    let revealed = AUTOLOAD_DESKTOP_REVEALED_MARKER;
    let sampled = tairix_test_framestats::SAMPLE_MESSAGE;
    let mut pen = PointerPen::pinned_at_origin(revealed, ramfb_screen());
    pen.click(revealed, 1, MouseButton::Primary, library_button);
    pen.click(revealed, 1, MouseButton::Primary, sample_row);
    // The first sample is in, so the bracketed window is open. Walk to the
    // leading end of the bar, then sweep it.
    pen.aim(sampled, 1, sweep_start);
    pen.hover(sampled, 1, sweep_end, SWEEP_MOVES);
    // Close the window: the second sample is the verdict.
    pen.click(sampled, 1, MouseButton::Primary, library_button);
    pen.click(sampled, 1, MouseButton::Primary, sample_row);
    Ok(pen.steps())
}

/// The board's display extent, the screen every reconstructed layout and
/// pointer script here is laid out against.
const fn ramfb_screen() -> (u32, u32) {
    (
        tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX,
        tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX,
    )
}

/// Open the program library, right-click the slot the session gives its process on
/// the bar, choose the *New window* row of the menu the application declared,
/// then primary-click that same slot to take its declared default action.
///
/// Every rectangle is reconstructed by driving the **production** taskbar
/// model — the same layout, hit-testing, and menu-building code the guest
/// runs — so the script clicks where the guest actually draws rather than at
/// coordinates copied from a screenshot. The declared menu itself comes from
/// the terminal's own `appbar` module, so the row this clicks is named once
/// rather than restated by position.
///
/// Every gate is **causal** rather than timed, and each is the strongest fact
/// the emitting side can honestly state:
///
/// - The desktop's own reveal witness opens the script, so nothing is
///   injected before there is a bar to hit.
/// - The two slot gestures wait on the session's per-window
///   [`APPBAR_WINDOW_SHOWN_MARKER`]: the *first* occurrence for the
///   right-click, the *second* for the final primary click. That witness
///   follows a frame the session actually put on screen, so by then the
///   application has declared its bar (it declares before it opens a window),
///   the session has grouped that window under its attested owner, and the
///   strip has been re-resolved and drawn. A create reply would say only that
///   the window exists.
/// - The menu row's click follows its right-click immediately — that press is
///   what opens the menu, so the row is on screen by construction.
///
/// The final primary click is also what keeps the guest alive long enough to
/// be photographed: it opens the third window, which is the create that
/// completes the guest's PASS, and the runner sends no pointer step until
/// every dump already asked for has been read back and parsed.
/// A host copy of the desktop model advanced to "the terminal has been
/// launched from the program library", with the screen points that gesture
/// used and the slot the session gives it.
///
/// Every rectangle is reconstructed by driving the **production** taskbar
/// model — the same layout, hit-testing, and menu-building code the guest runs
/// — so a script clicks where the guest actually draws rather than at
/// coordinates copied from a screenshot. Two verticals launch the terminal
/// this way and then diverge, so the launch itself is reconstructed once.
struct BarLaunch {
    /// The model, advanced to the launched state, for a caller that must keep
    /// driving it (opening the declared menu, say).
    shell: tairix_desktop_session::DesktopShell,
    /// Router carrying the model's input state, so a caller's further presses
    /// continue the same gesture history.
    router: tairix_taskbar::TaskbarInput,
    /// Centre of the program-library button.
    library_button: tairix_geometry::Point,
    /// Centre of the library popup's row for the terminal's bundle.
    entry_row: tairix_geometry::Point,
    /// Centre of the bar slot the session gives the launched application.
    slot: tairix_geometry::Point,
}

/// The scale every reconstruction here lays out at: the board presents one
/// display at the reference density.
const RECONSTRUCTION_SCALE: tairix_geometry::Scale = tairix_geometry::Scale::ONE;

/// One instant for a whole reconstruction: the model is driven only for
/// geometry, and no rectangle depends on how long a press was held.
const RECONSTRUCTION_INSTANT_NS: u64 = 0;

/// Press and release `button` at `at` in the reconstructed model.
fn bar_press(
    shell: &mut tairix_desktop_session::DesktopShell,
    router: &mut tairix_taskbar::TaskbarInput,
    at: tairix_geometry::Point,
    button: tairix_input::PointerButton,
) {
    use tairix_input::InputEvent;
    let taskbar = shell.session_mut().taskbar_mut();
    for event in [
        InputEvent::PointerMoved { to: at },
        InputEvent::PointerPressed { button },
        InputEvent::PointerReleased { button },
    ] {
        router.handle(
            event,
            taskbar,
            RECONSTRUCTION_SCALE,
            RECONSTRUCTION_INSTANT_NS,
        );
    }
}

/// Reconstruct the launch of the terminal from the program library.
fn reconstruct_bar_launch() -> Result<BarLaunch, String> {
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_desktop_session::{DesktopShell, FILES_LABEL};
    use tairix_input::PointerButton;
    use tairix_log::DiscardSink;
    use tairix_taskbar::{AppSlot, LibraryRow, TaskbarConfig, TaskbarInput};
    use tairix_test_appbar_qemu_aarch64::BAR_APP_NAME;

    // The shell exists only to reproduce the guest's layout arithmetic and
    // its input routing, so the script clicks exactly where the guest draws.
    // It never rasterises anything and owns no display, so it is wired
    // truthfully rather than plausibly: no display backing (a zero-sized
    // backing budgets nothing) and a gauge that has never been told a band
    // (which answers critical, so nothing is admitted).
    static NO_PRESSURE_FEED: tairix_reclaim::ReportedPressure =
        tairix_reclaim::ReportedPressure::unknown();
    static DISCARD_SINK: DiscardSink = DiscardSink;

    let (width, height) = ramfb_screen();
    let scale = RECONSTRUCTION_SCALE;
    let mut shell = DesktopShell::new(
        TaskbarConfig::bottom_bar(width, height),
        SEAT_PRIMARY,
        0,
        &NO_PRESSURE_FEED,
        &DISCARD_SINK,
    );
    // The popup lists the same catalog the guest session merges from the
    // planted machine store, so the reconstructed rows sit exactly where the
    // guest draws them.
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(reconstructed_library(&[])?);

    let bar = shell.session().taskbar().layout(scale);
    let library_button = rect_centre(bar.library, "Library button")?;

    let mut router = TaskbarInput::new();

    // Open the program-library popup, exactly as the first click will.
    bar_press(
        &mut shell,
        &mut router,
        library_button,
        PointerButton::Primary,
    );

    // The popup's row for the application whose bar this drives — keyed by
    // the bundle it launches (the same on-disk identity the guest PASS
    // witness attributes by), never a display-name literal.
    let bundle = format!(
        "{}/{BAR_APP_NAME}{}",
        tairix_abi::SYSTEM_APPLICATION_STORE,
        tairix_abi::BUNDLE_SUFFIX
    );
    let taskbar = shell.session().taskbar();
    let library = taskbar.library();
    let row = library
        .rows()
        .iter()
        .position(|row| match row {
            LibraryRow::Entry { id, .. } => library
                .catalog()
                .entry(id)
                .is_some_and(|entry| entry.bundle().as_str() == bundle),
            LibraryRow::Folder { .. } => false,
        })
        .ok_or_else(|| format!("icon-bar script: {bundle} is not listed in the program library"))?;
    let entry_row = rect_centre(
        taskbar
            .library_layout(scale)
            .rows
            .iter()
            .find(|(shown, _)| *shown == row)
            .map(|(_, rect)| *rect)
            .ok_or_else(|| {
                "icon-bar script: the terminal's row is not visible in the popup".to_string()
            })?,
        "terminal library entry",
    )?;

    // Launching from a row closes the popup and puts the application on the
    // bar, so the model is advanced to the state the guest will be in: the
    // autostarted file manager already in the leading slot, and the launched
    // application beside it carrying the declaration it makes.
    bar_press(&mut shell, &mut router, entry_row, PointerButton::Primary);
    let declared = tairix_terminal::appbar::declaration(0)
        .map_err(|err| format!("icon-bar script: the terminal's declaration is invalid: {err}"))?;
    let mut seated = (0..APPBAR_LAUNCHED_SLOT)
        .map(|_| AppSlot::new(FILES_LABEL, tairix_icon::IconKind::AppBundle))
        .collect::<Vec<_>>();
    seated.push(
        AppSlot::new(BAR_APP_NAME, tairix_icon::IconKind::AppBundle)
            .with_declaration(declared.menu, declared.default_action),
    );
    shell.session_mut().taskbar_mut().set_apps(seated);

    // The slot the session gives the launched application.
    let slot = rect_centre(
        appbar_slot_rect(shell.session().taskbar().theme(), APPBAR_LAUNCHED_SLOT)?,
        "slot",
    )?;
    Ok(BarLaunch {
        shell,
        router,
        library_button,
        entry_row,
        slot,
    })
}

/// Open the program library, right-click the slot the session gives its
/// process on the bar, choose the *New window* row of the menu the application
/// declared, then primary-click that same slot to take its declared default
/// action.
///
/// The launch itself is [`reconstruct_bar_launch`]; the declared menu comes
/// from the terminal's own `appbar` module, so the row this clicks is named
/// once rather than restated by position.
///
/// Every gate is **causal** rather than timed, and each is the strongest fact
/// the emitting side can honestly state:
///
/// - The desktop's own reveal witness opens the script, so nothing is
///   injected before there is a bar to hit.
/// - The two slot gestures wait on the session's per-window
///   [`APPBAR_WINDOW_SHOWN_MARKER`]: the *first* occurrence for the
///   right-click, the *second* for the final primary click. That witness
///   follows a frame the session actually put on screen, so by then the
///   application has declared its bar (it declares before it opens a window),
///   the session has grouped that window under its attested owner, and the
///   strip has been re-resolved and drawn. A create reply would say only that
///   the window exists.
/// - The menu row's click follows its right-click immediately — that press is
///   what opens the menu, so the row is on screen by construction.
///
/// The final primary click is also what keeps the guest alive long enough to
/// be photographed: it opens the third window, which is the create that
/// completes the guest's PASS, and the runner sends no pointer step until
/// every dump already asked for has been read back and parsed.
fn appbar_pointer_script() -> Result<Vec<tairix_qemu::PointerStep>, String> {
    use tairix_input::PointerButton;
    use tairix_qemu::MouseButton;

    let BarLaunch {
        mut shell,
        mut router,
        library_button,
        entry_row,
        slot,
    } = reconstruct_bar_launch()?;
    let scale = RECONSTRUCTION_SCALE;
    bar_press(&mut shell, &mut router, slot, PointerButton::Secondary);
    if !shell.session().taskbar().menu().is_open() {
        return Err("icon-bar script: a secondary press on the slot opened no menu".to_string());
    }
    // The *New window* row, named from the declaration rather than by
    // position: the rows the bar draws skip the declared separator, so
    // counting them here would restate a rule the model already applies.
    let menu_layout = shell
        .session()
        .taskbar()
        .menu_layout(scale)
        .ok_or_else(|| "icon-bar script: the open menu lays nothing out".to_string())?;
    let control = shell.session().taskbar().menu().control();
    let row = control
        .items()
        .iter()
        .position(|item| item.label() == TERMINAL_NEW_WINDOW_LABEL)
        .ok_or_else(|| {
            format!("icon-bar script: the menu has no {TERMINAL_NEW_WINDOW_LABEL:?} row")
        })?;
    let new_window = rect_centre(
        control
            .row_rect(
                row,
                menu_layout.panel,
                scale,
                shell.session().taskbar().theme(),
            )
            .ok_or_else(|| "icon-bar script: the chosen row lays nothing out".to_string())?,
        "New window row",
    )?;

    let ready = AUTOLOAD_DESKTOP_REVEALED_MARKER;
    let mut pen = PointerPen::pinned_at_origin(ready, ramfb_screen());
    pen.click(ready, 1, MouseButton::Primary, library_button);
    pen.click(ready, 1, MouseButton::Primary, entry_row);
    // The application's first window is on screen, so its slot is drawn and
    // carries the declaration it made before opening that window.
    pen.click(APPBAR_WINDOW_SHOWN_MARKER, 1, MouseButton::Secondary, slot);
    pen.click(
        APPBAR_WINDOW_SHOWN_MARKER,
        1,
        MouseButton::Primary,
        new_window,
    );
    // The chosen row's window is on screen too — the frame the third dump
    // reads. The pointer is still on that row, so this walks back to the slot
    // and presses it: the declaration claims the application handles a primary
    // click, so this is its default action rather than a raise, and the window
    // it opens is the guest's last witness.
    pen.click(APPBAR_WINDOW_SHOWN_MARKER, 2, MouseButton::Primary, slot);
    Ok(pen.steps())
}

/// Launch the terminal from the program library, then open one further window
/// per primary click on its icon-bar slot until the whole screenful is open.
///
/// The launch is [`reconstruct_bar_launch`]. Each further click is gated on the
/// session's per-window [`APPBAR_WINDOW_SHOWN_MARKER`], counted: the click that
/// opens window *n* waits for window *n − 1*'s frame to have reached the
/// screen. So the script never runs ahead of the desktop, however slowly a
/// guest short of memory opens the next one.
///
/// Between clicks the pointer is walked off the bar. Resting it on a slot is
/// the gesture that opens that application's hover window picker, and from the
/// second window onwards there is a picker to open; parking the pointer away
/// from the bar while the script waits keeps every click a click on the slot.
fn desktop_pressure_pointer_script() -> Result<Vec<tairix_qemu::PointerStep>, String> {
    use tairix_qemu::MouseButton;
    use tairix_test_desktop_pressure_qemu_aarch64::WINDOWS_OPENED;

    let BarLaunch {
        library_button,
        entry_row,
        slot,
        ..
    } = reconstruct_bar_launch()?;
    let (width, height) = ramfb_screen();
    // Clear of the bar along the bottom edge and of the window cascade in the
    // top left, so the pointer waits over bare wallpaper: nothing there dwells,
    // hovers, or is raised by a pointer resting on it.
    #[allow(clippy::cast_possible_wrap)] // Screen extents are far below i32::MAX.
    let rest = tairix_geometry::Point::new(width as i32 - 1, height as i32 / 2);

    let ready = AUTOLOAD_DESKTOP_REVEALED_MARKER;
    let mut pen = PointerPen::pinned_at_origin(ready, ramfb_screen());
    pen.click(ready, 1, MouseButton::Primary, library_button);
    pen.click(ready, 1, MouseButton::Primary, entry_row);
    // The launch opened the first window, so each remaining one is a click on
    // the slot once its predecessor is on screen.
    for opened in 1..WINDOWS_OPENED {
        pen.click(
            APPBAR_WINDOW_SHOWN_MARKER,
            opened,
            MouseButton::Primary,
            slot,
        );
        pen.aim(APPBAR_WINDOW_SHOWN_MARKER, opened, rest);
    }
    Ok(pen.steps())
}

/// The label the terminal gives its *New window* row, as the script names the
/// row to click.
///
/// The label is the application's own declaration text, so it is read back
/// from the row the model built rather than compared against a literal the
/// script keeps: this constant is only the spelling the terminal's own
/// `appbar` module declares.
const TERMINAL_NEW_WINDOW_LABEL: &str = "New window";

/// The virtio-input readiness marker the pointer-button vertical's guest
/// logs once its (single) mouse driver instance has armed its event
/// queue — the gate both button steps wait on before the runner injects.
const POINTER_BUTTON_READY_MARKER: &str = "virtio-qemu: virtio-input eventq armed";

/// Build the pointer-button vertical's injection script: press then
/// release the **secondary (right)** button, each once the guest's mouse
/// driver has armed its event queue. Proves a scripted right-click reaches
/// the emulated virtio-mouse as a right button (`BTN_RIGHT`, `0x111`),
/// guarding the `tools/qemu` button-mask fix that made the file manager's
/// right-click context menu reachable in QEMU (`plans/NEW-FILEMANAGER.md`
/// FM9-c). A single virtio-input node (the mouse) arms once, so each step
/// gates on the first occurrence of the readiness marker.
// The `Result` is required by the shared `PointerScriptBuilder` fn-pointer
// type (a fallible sibling like `autoload_desktop_pointer_script` can fail);
// this script is statically known and never errors.
#[allow(clippy::unnecessary_wraps)]
fn pointer_button_script() -> Result<Vec<tairix_qemu::PointerStep>, String> {
    use tairix_qemu::{MouseButton, PointerAction, PointerStep};
    Ok(vec![
        PointerStep {
            ready_marker: POINTER_BUTTON_READY_MARKER.to_string(),
            ready_occurrences: 1,
            action: PointerAction::Press(MouseButton::Secondary),
        },
        PointerStep {
            ready_marker: POINTER_BUTTON_READY_MARKER.to_string(),
            ready_occurrences: 1,
            action: PointerAction::Release(MouseButton::Secondary),
        },
    ])
}

/// Build the AW3+AW4 desktop click script: pin the pointer to the
/// top-left corner, click the autostarted file manager's icon-bar slot
/// (opening a window), click the served window's body (delivering `Focus`
/// and `Pressed` app-ward — the kernel-attested `MessageDelivered`
/// witnesses the second screendump keys on), and land a handshake click
/// on the still-focused window; then the AW4 terminal stage: click the
/// Library button (the program-library popup opens over the catalog the
/// guest merged from the planted machine store), click the popup's
/// "Terminal" entry (spawning the terminal bundle), and click the
/// terminal's served window at the second cascade slot — the deliveries
/// the typed shell command keys on. Every coordinate is computed by
/// reconstructing the production desktop shell — the same
/// `TaskbarConfig` and layout code the guest session runs over the
/// shared ramfb console geometry, with the popup rows derived from the
/// same `AppInfo.toml` manifest sources the planted store and its seeded
/// catalog are composed from ([`reconstructed_library`]) — so the script
/// and the rendered desktop cannot drift.
///
/// Step gating: the guest processes injected events strictly in device
/// order and the bar model updates synchronously on the press, so the
/// Files-slot click keys on the session's `DESKTOP_REVEALED`
/// witness alone (the runner already held it back until the first dump
/// verified). The in-window click waits for the reserved window
/// endpoint's first *reply* (the create round-trip completed, so the
/// window exists in the compositor and was presented by that wake). The
/// handshake click keys on the first click's deliveries (and is
/// additionally held while the second dump is pending), the
/// terminal-stage library-popup steps on the handshake's own delivery,
/// and the terminal-window click on the third window-frame map (the
/// terminal's create) — so each dump captures exactly the staged frame
/// and every stage is provably established before its step fires.
#[allow(clippy::too_many_lines)] // One linear, ordered click-through script; splitting it would obscure the staging.
fn autoload_desktop_pointer_script() -> Result<Vec<tairix_qemu::PointerStep>, String> {
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_desktop_session::DesktopShell;
    use tairix_geometry::{Point, Scale};
    use tairix_log::DiscardSink;
    use tairix_qemu::{MouseButton, PointerAction, PointerStep};
    use tairix_reclaim::ReportedPressure;
    use tairix_taskbar::{LibraryRow, TaskbarConfig};

    // The shell below exists only to reproduce the guest's layout
    // arithmetic, so the script clicks exactly where the guest draws. It
    // never rasterises anything and owns no display, so it is wired
    // truthfully rather than plausibly: no display backing (a zero-sized
    // backing budgets nothing) and a gauge that has never been told a
    // band (which answers critical, so nothing is admitted). Its asset
    // caches therefore stay empty instead of pretending to a reclaim
    // policy this tool cannot obey.
    static NO_PRESSURE_FEED: ReportedPressure = ReportedPressure::unknown();
    static DISCARD_SINK: DiscardSink = DiscardSink;

    let width = tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX;
    let height = tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX;
    let mut shell = DesktopShell::new(
        TaskbarConfig::bottom_bar(width, height),
        SEAT_PRIMARY,
        0,
        &NO_PRESSURE_FEED,
        &DISCARD_SINK,
    );
    // The popup lists the same catalog the guest session merges from the
    // planted machine store, so the reconstructed rows sit exactly where
    // the guest draws them.
    shell
        .session_mut()
        .taskbar_mut()
        .library_mut()
        .set_catalog(reconstructed_library(&[])?);
    // The file manager is a core desktop component the session autostarts at
    // bring-up, so by the time the script runs the bar already carries its
    // application slot — the leading one, because Files is the first process
    // the session sees. The strip is reconstructed with that one slot so the
    // click lands where the guest draws it. A slot is icon-only at a fixed
    // extent, so only its presence moves the geometry; the label is the one
    // the session resolves from the bundle's own signed manifest.
    shell
        .session_mut()
        .taskbar_mut()
        .set_apps(vec![tairix_taskbar::AppSlot::new(
            FILES_BAR_APP_NAME,
            tairix_icon::IconKind::AppBundle,
        )]);

    let taskbar = shell.session().taskbar();
    let bar = taskbar.layout(Scale::ONE);
    let files_slot = rect_centre(
        *bar.apps
            .first()
            .ok_or_else(|| "desktop pointer script: the bar carries no Files slot".to_string())?,
        "Files slot",
    )?;
    let library_button = rect_centre(bar.library, "Library button")?;
    // The popup's terminal entry — keyed by the bundle it launches (the
    // same on-disk identity the guest PASS witnesses attribute by), never
    // a display-name literal — in the deterministic freshly-opened state
    // the guest presents after the Library click (search cleared, every
    // folder expanded, scroll at the top).
    let library = taskbar.library();
    let terminal_bundle = format!("{}/terminal.app", tairix_abi::SYSTEM_APPLICATION_STORE);
    let terminal_index = library
        .rows()
        .iter()
        .position(|row| match row {
            LibraryRow::Entry { id, .. } => library
                .catalog()
                .entry(id)
                .is_some_and(|entry| entry.bundle().as_str() == terminal_bundle),
            LibraryRow::Folder { .. } => false,
        })
        .ok_or_else(|| {
            format!("desktop pointer script: no library entry launches {terminal_bundle}")
        })?;
    let terminal_entry = taskbar
        .library_layout(Scale::ONE)
        .rows
        .iter()
        .find(|&&(index, _)| index == terminal_index)
        .map(|&(_, rect)| rect)
        .ok_or_else(|| {
            "desktop pointer script: the terminal entry is not visible in the popup".to_string()
        })?;
    let terminal_entry = rect_centre(terminal_entry, "terminal library entry")?;
    // Each served window's own client viewport: the session cascades the
    // windows in open order through the one shared placement rule and
    // decorates them, each sized by its app's own constants — the same
    // values the dump assertion measures. A click aims into the client, so
    // it reaches the application rather than the furniture around it.
    let files_client = served_window_layout(
        0,
        tairix_browse::WIN_WIDTH,
        tairix_browse::WIN_HEIGHT,
        tairix_browse::WIN_SIZING.resizable(),
        shell.session().active_theme(),
    )
    .client;
    // The files-window "focus" clicks below aim at a column and row that hold
    // nothing actionable, so the click focuses the window (delivering `Focus` +
    // `Pressed` app-ward) without selecting a listing row or navigating. The
    // files app's frame therefore never changes — its single startup present is
    // the "sole present" the window-reply gate downstream counts on,
    // independent of how many entries the root lists. (A click on a row would
    // select it and repaint — correct app behaviour, but it would add presents
    // the fixed count gate must not see.)
    let path_bar_y =
        tairix_browse::render::toolbar_height(Scale::ONE, shell.session().active_theme())
            .saturating_add(4);
    #[allow(clippy::cast_possible_wrap)] // Screen extents are far below i32::MAX.
    let window = Point::new(
        files_client.left() + (tairix_browse::WIN_WIDTH / 2) as i32,
        files_client.top() + path_bar_y as i32,
    );
    let terminal_window = served_client_aim(
        1,
        tairix_terminal::WIN_RESIZABLE,
        shell.session().active_theme(),
    );

    // Relative-motion arithmetic: the pointer starts at an unknown
    // position (the session centres it), so the first move overshoots
    // both axes leftward/upward; the guest clamps at (0, 0), making every
    // later displacement exact.
    #[allow(clippy::cast_possible_wrap)] // Screen extents are far below i32::MAX.
    let pin = PointerAction::Move {
        dx: -(2 * width as i32),
        dy: -(2 * height as i32),
    };
    let move_by = |from: Point, to: Point| PointerAction::Move {
        dx: to.x - from.x,
        dy: to.y - from.y,
    };
    // One step per click: a press and a release scripted separately would sit
    // a poll tick apart, and a guest that acts on the press can exit inside
    // that gap with the release still owed.
    let click = PointerAction::Click(MouseButton::Primary);
    let step = |marker: &str, occurrences: u32, action: PointerAction| PointerStep {
        ready_marker: marker.to_owned(),
        ready_occurrences: occurrences,
        action,
    };
    Ok(vec![
        // Pin, then click the autostarted file manager's own icon-bar slot.
        // Files declares that it handles the primary click itself, so the
        // session relays it and the app opens a window at the user's home.
        // This first motion is also the run's `kind=pointer` delivery
        // witness; the slot click needs no extra gate — the guest applies
        // the injected events strictly in order, and the desktop-revealed
        // witness it keys on is raised after bring-up, by which point the
        // autostarted app has declared its presence and holds the slot.
        step(AUTOLOAD_DESKTOP_REVEALED_MARKER, 1, pin),
        step(
            AUTOLOAD_DESKTOP_REVEALED_MARKER,
            1,
            move_by(Point::ORIGIN, files_slot),
        ),
        step(AUTOLOAD_DESKTOP_REVEALED_MARKER, 1, click),
        // The spawned app's window frame has been mapped — its window is
        // created and sits at the first cascade slot — so click its body;
        // the session delivers `Focus` + `Pressed` to that window, which is
        // the second dump's key. Gating on the map, not on a reply over the
        // shared window rendezvous, is what keeps this click behind the
        // window's existence: every client of that rendezvous replies on it,
        // and the Switchboard's start-up desktop query once fired this step
        // against a bare desktop.
        step(
            AUTOLOAD_WINDOW_MAP_MARKER,
            AUTOLOAD_FILES_WINDOW_MAP_OCCURRENCES,
            move_by(files_slot, window),
        ),
        step(
            AUTOLOAD_WINDOW_MAP_MARKER,
            AUTOLOAD_FILES_WINDOW_MAP_OCCURRENCES,
            click,
        ),
        // The handshake click on the still-focused window: keyed on the
        // first click's own deliveries reaching that window and
        // additionally held while the second dump is pending, so the dump
        // captures the staged dark frame and the terminal stage below
        // starts in a strictly later wake.
        step(AUTOLOAD_FILES_ACTIVATED_MARKER, 1, click),
        // --- The AW4 terminal stage, keyed on the handshake click's own
        // delivery. Click the Library button — the program-library popup
        // opens (a session-owned surface: no app-ward delivery and no
        // window-frame map, so neither gate below can fire early) — then
        // the popup's "Terminal" entry, spawning the terminal bundle from
        // the on-disk store through the planted catalog.
        step(
            AUTOLOAD_FILES_HANDSHAKE_MARKER,
            1,
            move_by(window, library_button),
        ),
        step(AUTOLOAD_FILES_HANDSHAKE_MARKER, 1, click),
        step(
            AUTOLOAD_FILES_HANDSHAKE_MARKER,
            1,
            move_by(library_button, terminal_entry),
        ),
        step(AUTOLOAD_FILES_HANDSHAKE_MARKER, 1, click),
        // The terminal's window frame has been mapped — its window is
        // created and sits at the second cascade slot — so click its
        // body. This gate counts window-frame **maps** (one per window
        // creation), never window presents, so a files-window repaint can
        // no longer inflate the count and fire this click before the
        // terminal window exists (the flaky-repaint deadlock this fix
        // closes). The files window's unfocus, the terminal's focus, and
        // the press are the deliveries the typed shell command keys on
        // (the guest PASS gate's round-trip witness follows from the spawn
        // the command causes).
        step(
            AUTOLOAD_WINDOW_MAP_MARKER,
            AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES,
            move_by(terminal_entry, terminal_window),
        ),
        step(
            AUTOLOAD_WINDOW_MAP_MARKER,
            AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES,
            click,
        ),
    ])
}

/// The program library the guest desktop lists, reconstructed from the
/// same on-disk `AppInfo.toml` manifest sources the planted store — and
/// the catalog document seeded beside it — are composed from
/// (`discover_app_manifests`), so the popup rows the script clicks sit
/// exactly where the guest draws them: the app-store bundles that declare
/// a `library` folder are exactly the seeded catalog's entries.
fn reconstructed_library(fixtures: &[&str]) -> Result<tairix_proglib::Catalog, String> {
    use tairix_itest_harness::app_image::{discover_app_manifests, discover_crate_manifest};
    use tairix_proglib::{BundlePath, Catalog, DisplayName, EntryId, IconAsset, LibraryEntry};

    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .ok_or_else(|| "desktop pointer script: workspace root unreachable".to_string())?;
    let mut discovered = discover_app_manifests(&root.join("userland"))
        .map_err(|e| format!("desktop pointer script: manifest discovery: {e}"))?;
    // A vertical whose disk plants a test-only fixture bundle has it in the
    // seeded catalog too — that catalog is derived from the planted store —
    // so the reconstruction reads the fixture's own manifest source rather
    // than a copy of what it says.
    for fixture in fixtures {
        let dir = root.join(fixture);
        let app = discover_crate_manifest(&dir)
            .map_err(|e| format!("desktop pointer script: {fixture} manifest discovery: {e}"))?
            .ok_or_else(|| format!("desktop pointer script: {fixture} has no manifest source"))?;
        discovered.push(app);
    }

    let mut catalog = Catalog::new();
    for app in &discovered {
        let manifest = &app.manifest;
        if !manifest.kind.is_searched() {
            continue;
        }
        let Some(folder) = manifest.library else {
            continue;
        };
        let fail =
            |what: &str, e: &dyn core::fmt::Display| format!("desktop pointer script: {what}: {e}");
        let id = EntryId::new(&manifest.id).map_err(|e| fail(&manifest.id, &e))?;
        let name = DisplayName::new(&manifest.name).map_err(|e| fail(&manifest.id, &e))?;
        let bundle = BundlePath::new(&format!("{}/{}.app", manifest.kind.store(), manifest.name))
            .map_err(|e| fail(&manifest.id, &e))?;
        let icon = match &manifest.library_icon {
            Some(asset) => Some(IconAsset::new(asset).map_err(|e| fail(&manifest.id, &e))?),
            None => None,
        };
        catalog
            .insert(LibraryEntry::new(id, name, bundle, folder, icon))
            .map_err(|e| fail(&manifest.id, &e))?;
    }
    Ok(catalog)
}

/// A filesystem volume to plant on an enrolment's virtio-blk backing image.
struct FsImage {
    /// File extension of the backing image planted beside the kernel binary.
    extension: &'static str,
    /// The volume bytes the planter lays down (non-zero sectors only).
    bytes: Vec<u8>,
    /// The emulated device's total sector count.
    total_sectors: u64,
}

/// Sector count of a produced whole-disk image. A built encrypted-root
/// image describes its own size through its byte length — the fixture sizes
/// its `/System` partition to the planted content, so the total is derived
/// from the produced image, never a fixed constant that a larger arch's
/// store would outgrow.
fn image_total_sectors(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len() / tairix_test_encrypted_root_image::SECTOR_BYTES).unwrap_or(0)
}

/// The filesystem volume `t` plants on its virtio-blk backing image, or
/// `None` for an enrolment with no filesystem disk. The bytes come from a
/// single-source-of-truth image fixture the kernel-side tail also names, so
/// the planted on-disk layout and the guest's expectations cannot drift:
/// the FAT32 fixture is hand-built; the arxfs fixture is authored by the
/// real arxfs driver itself (format + plant).
fn fs_disk_image(t: &QemuTest, stores: StoreSet) -> Result<Option<FsImage>, String> {
    // Only the two plain encrypted-root disks name their app set directly
    // here; every driver-store (net-root) disk is authored in
    // `net_root_fs_disk_image`, which selects its own sets from `stores`.
    let StoreSet {
        apps,
        apps_with_memsoak,
        ..
    } = stores;
    Ok(match t.fs_disk {
        FsDisk::None => None,
        FsDisk::Fat32 => Some(FsImage {
            extension: "fat32.img",
            bytes: tairix_test_fat32_image::build_image(),
            total_sectors: tairix_test_fat32_image::TOTAL_SECTORS,
        }),
        FsDisk::ARXFS => Some(FsImage {
            extension: "arxfs.img",
            bytes: tairix_test_arxfs_image::build_image()
                .map_err(|e| format!("test --qemu ({}): build arxfs image: {e:?}", t.package))?,
            total_sectors: tairix_test_arxfs_image::TOTAL_SECTORS,
        }),
        FsDisk::UsersRoot => Some(FsImage {
            extension: "users.img",
            bytes: tairix_test_arxfs_image::build_users_root_image().map_err(|e| {
                format!("test --qemu ({}): build users-root image: {e:?}", t.package)
            })?,
            total_sectors: tairix_test_arxfs_image::TOTAL_SECTORS,
        }),
        FsDisk::EncryptedRootDisk => {
            let bytes = encrypted_root_disk_bytes(t, apps)?;
            let total_sectors = image_total_sectors(&bytes);
            Some(FsImage {
                extension: "encrypted-root.img",
                bytes,
                total_sectors,
            })
        }
        // The memsoak vertical uses the plain encrypted-root author (no
        // driver store): the same builder as `EncryptedRootDisk`, planting
        // the standard store plus the one test-only fixture bundle.
        FsDisk::MemsoakRootDisk => {
            let bytes = encrypted_root_disk_bytes(t, apps_with_memsoak)?;
            let total_sectors = image_total_sectors(&bytes);
            Some(FsImage {
                extension: "memsoak-root.img",
                bytes,
                total_sectors,
            })
        }
        // Every driver-store vertical (the autoload disk plus the net-only
        // TCP / ping / static / bond / DHCP disks) shares one whole-disk
        // author, selected in `net_root_fs_disk_image` — never a per-fixture
        // copy.
        FsDisk::AutoloadRootDisk
        | FsDisk::GreeterRootDisk
        | FsDisk::HoverRootDisk
        | FsDisk::StreamRootDisk
        | FsDisk::EcnRootDisk
        | FsDisk::ListenRootDisk
        | FsDisk::NetToolRootDisk
        | FsDisk::StaticNetRootDisk
        | FsDisk::TimeNetRootDisk
        | FsDisk::BondNetRootDisk
        | FsDisk::DhcpNetRootDisk
        | FsDisk::Dhcp6NetRootDisk => Some(net_root_fs_disk_image(t, stores)?),
    })
}

/// Build the whole-disk image for a **net-root** vertical — a signed driver
/// store plus a per-vertical app/config store — the one place each such
/// disk's driver set, app set, backing-file extension, and label are
/// selected. Every net-root disk routes through `net_root_image`, so they
/// cannot drift in how the whole disk is authored. Called only for the
/// net-root `FsDisk` variants (the caller's match guarantees it).
fn net_root_fs_disk_image(t: &QemuTest, stores: StoreSet) -> Result<FsImage, String> {
    let StoreSet {
        apps,
        apps_with_framestats,
        autoload_drivers,
        apps_with_tcpecho,
        apps_with_tcpecho_ecn,
        apps_with_tcpserve,
        net_only_drivers,
        static_net_apps,
        bond_net_apps,
        dhcp_net_apps,
        dhcpv6_net_apps,
        ..
    } = stores;
    let (drivers, app_set, extension, label) = match t.fs_disk {
        FsDisk::AutoloadRootDisk => (autoload_drivers, apps, "autoload-root.img", "autoload-root"),
        FsDisk::GreeterRootDisk => (autoload_drivers, apps, "greeter-root.img", "greeter-root"),
        FsDisk::HoverRootDisk => (
            autoload_drivers,
            apps_with_framestats,
            "hover-root.img",
            "hover-root",
        ),
        FsDisk::StreamRootDisk => (
            net_only_drivers,
            apps_with_tcpecho,
            "stream-root.img",
            "stream-root",
        ),
        FsDisk::EcnRootDisk => (
            net_only_drivers,
            apps_with_tcpecho_ecn,
            "ecn-root.img",
            "ecn-root",
        ),
        FsDisk::ListenRootDisk => (
            net_only_drivers,
            apps_with_tcpserve,
            "listen-root.img",
            "listen-root",
        ),
        FsDisk::NetToolRootDisk => (net_only_drivers, apps, "net-tool-root.img", "net-tool-root"),
        FsDisk::StaticNetRootDisk => (
            net_only_drivers,
            static_net_apps,
            "static-net-root.img",
            "static-net-root",
        ),
        FsDisk::TimeNetRootDisk => (
            net_only_drivers,
            static_net_apps,
            "time-net-root.img",
            "time-net-root",
        ),
        FsDisk::BondNetRootDisk => (
            net_only_drivers,
            bond_net_apps,
            "bond-net-root.img",
            "bond-net-root",
        ),
        FsDisk::DhcpNetRootDisk => (
            net_only_drivers,
            dhcp_net_apps,
            "dhcp-net-root.img",
            "dhcp-net-root",
        ),
        FsDisk::Dhcp6NetRootDisk => (
            net_only_drivers,
            dhcpv6_net_apps,
            "dhcp6-net-root.img",
            "dhcp6-net-root",
        ),
        _ => unreachable!("net_root_fs_disk_image is called only for net-root disks"),
    };
    net_root_image(t, drivers, app_set, extension, label)
}

/// Build a whole-disk encrypted-root image planting a driver set in
/// `/System/Drivers/` plus an app/service set — the one builder every
/// driver-store vertical (autoload, the two TCP verticals, `ping`, the
/// static-addressing vertical) shares, never a per-vertical copy. `drivers` is
/// the vertical's signed driver set and `apps` its app/service set (which may
/// additionally carry a planted `network.conf`); `extension` names the backing
/// file and `label` names the vertical in a build error.
fn net_root_image(
    t: &QemuTest,
    drivers: &[super::image_apps::AppStoreFile],
    apps: &[super::image_apps::AppStoreFile],
    extension: &'static str,
    label: &str,
) -> Result<FsImage, String> {
    // The seeded program-library catalog rides on every driver-store
    // vertical's encrypted root volume, exactly as on a shipped image
    // (the autoload vertical's desktop opens the popup over it), beside
    // whatever login configuration the vertical asks for.
    let plants = root_plants(t, apps)?;
    let borrowed = root_plant_refs(&plants);
    let root_files: Vec<(&[&[u8]], &[u8])> = borrowed
        .iter()
        .map(|(components, bytes)| (components.as_slice(), *bytes))
        .collect();
    let bytes = super::image_apps::with_plant_refs(drivers, |driver_files| {
        super::image_apps::with_plant_refs(apps, |app_files| {
            tairix_test_encrypted_root_image::build_image_with_contents(
                driver_files,
                app_files,
                &root_files,
                tairix_test_encrypted_root_image::PASSPHRASE,
            )
        })
    })
    .map_err(|e| format!("test --qemu ({}): build {label} image: {e:?}", t.package))?;
    let total_sectors = image_total_sectors(&bytes);
    Ok(FsImage {
        extension,
        bytes,
        total_sectors,
    })
}

/// Spawn the harness-side `netpeer` link peer for `mode` on the bound
/// `peer_sock`, ready before QEMU launches so no early guest frame is lost.
/// The one place each [`NetPeerMode`] maps to its peer role, kept out of
/// [`finish_run`] so that function stays within the line budget.
fn spawn_net_peer(
    mode: NetPeerMode,
    qemu_sock: &Path,
    peer_sock: &Path,
) -> Result<super::netpeer::NetPeer, String> {
    match mode {
        // Filtered out before this call; a peer is only spawned when attached.
        NetPeerMode::None => unreachable!("peer mode None is filtered above"),
        NetPeerMode::V6LinkLocal => super::netpeer::NetPeer::spawn(qemu_sock, peer_sock),
        NetPeerMode::V6TcpEcho => super::netpeer::NetPeer::spawn_tcp_echo(qemu_sock, peer_sock),
        NetPeerMode::V6TcpEchoEcn => {
            super::netpeer::NetPeer::spawn_tcp_echo_ecn(qemu_sock, peer_sock)
        }
        NetPeerMode::V6TcpConnect => {
            super::netpeer::NetPeer::spawn_tcp_connect(qemu_sock, peer_sock)
        }
        NetPeerMode::V6TcpFlood => super::netpeer::NetPeer::spawn_tcp_flood(qemu_sock, peer_sock),
        NetPeerMode::V6PingResponder => {
            super::netpeer::NetPeer::spawn_ping_responder(qemu_sock, peer_sock)
        }
        NetPeerMode::V6TelnetServer => super::netpeer::NetPeer::spawn_telnet(qemu_sock, peer_sock),
        NetPeerMode::V6StaticEcho => super::netpeer::NetPeer::spawn_static(qemu_sock, peer_sock),
        NetPeerMode::V4DhcpEcho => super::netpeer::NetPeer::spawn_dhcp(qemu_sock, peer_sock),
        NetPeerMode::V6Dhcp6Echo => super::netpeer::NetPeer::spawn_dhcp6(qemu_sock, peer_sock),
        NetPeerMode::NtpServer => super::netpeer::NetPeer::spawn_ntp(qemu_sock, peer_sock),
        // The bond peer needs two wires (two socket pairs), so it is attached
        // directly in `finish_run`, never through this single-wire spawner.
        NetPeerMode::Bond => {
            unreachable!("bond mode is attached in finish_run, not spawn_net_peer")
        }
    }
}

/// Serial marker gating the bond vertical's mid-flow member drop: the
/// `netstack` `INBOUND_ECHO_SERVED` audit message. Its first appearance means
/// the bond is up and has served the peer's echo over the primary member, so
/// dropping that member's carrier now exercises a genuine mid-flow failover
/// (never before the flow exists). It is the literal `netstack` audit message
/// (`userland/net/netstack/src/run.rs`), matching the established pattern of
/// gating monitor injections on an audit message substring.
const BOND_FAILOVER_TRIGGER_MARKER: &str = "netstack: inbound echo request served (reply queued)";

/// The `memtest` takeover verticals' binaries (`plans/NEW-SUPERVISOR.md` §9
/// Stage E), one per Tier-1 bare-metal target. [`finish_run`] recognises them
/// to end the intentionally-endless test with a monitor `system_reset` after
/// a proven loop and score that reset as success.
const MEMTEST_TAKEOVER_BINARIES: [&str; 3] = [
    "tairix-test-supervisor-memtest-takeover-qemu-x86-64",
    "tairix-test-supervisor-memtest-takeover-qemu-riscv64",
    "tairix-test-supervisor-memtest-takeover-qemu-aarch64",
];

/// Serial marker the continuous `memtest` prints when it finishes one full
/// test loop (every pattern over all of RAM). Its first appearance proves the
/// takeover quiesced the peers, flattened paging, swept every pattern over all
/// of RAM, and rendered the display. The runner keys on it twice: it issues a
/// QEMU-monitor `system_reset` to end the endless test deterministically, and
/// it gates the reset-is-success scoring on it, so a crash that reset before a
/// completed loop never printed it and fails loud
/// (`tairix_qemu::Spec::reset_success_marker`).
const MEMTEST_TAKEOVER_LOOP_MARKER: &str = "memtest: completed test loop";

/// Absolute wall-clock ceiling for a `memtest` takeover run
/// (`tairix_qemu::Spec::with_runtime_ceiling`).
///
/// These verticals are the one shape the derived ceiling (twice the silence
/// budget) describes wrongly: success *is* a full sweep of guest RAM, so the
/// run's length is set by the work and by host contention, not by how long the
/// guest may go quiet. Measured: 40 s for boot, one 256 MiB sweep, and the
/// reset on an idle host; ~4 minutes for the same sweep in the nightly soak,
/// where ~95 concurrent jobs share the runner. Fifteen minutes is over three
/// times that loaded measurement, so a progressing guest is never cut off,
/// while a guest that wedges mid-sweep yet keeps printing is still bounded
/// rather than stalling the matrix forever.
const MEMTEST_TAKEOVER_RUNTIME_CEILING: Duration = Duration::from_mins(15);

/// Attach `t`'s virtio-net interface(s) to `spec` and start the harness-side
/// `netpeer` link peer, returning the updated spec, the running peer (if any),
/// and the wire's reserved socket paths. Every frame is captured to a
/// `<binary>.pcap` beside the kernel image so a failing run leaves the on-wire
/// exchange to inspect.
///
/// The socket paths are minted by [`ReservedSocket`], which keeps them short
/// enough to bind (a unix socket's `sun_path` is 104 bytes on macOS and the
/// temp directory alone can take half of that) and unique per wire per
/// process, so concurrent runs stay on private wires. The returned guards must
/// outlive the run: dropping one removes its socket file. Kept out of
/// [`finish_run`] so that function stays within the line budget.
fn attach_net_peer(
    t: &QemuTest,
    kernel: &Path,
    mut spec: Spec,
) -> Result<(Spec, Option<super::netpeer::NetPeer>, Vec<ReservedSocket>), String> {
    let mut peer = None;
    let mut socks: Vec<ReservedSocket> = Vec::new();
    // One minting definition for every wire end: reserve the short path, keep
    // the guard alive in `socks` for the run, and hand back the path itself.
    let wire = |socks: &mut Vec<ReservedSocket>, role: &str| {
        let guard = ReservedSocket::reserve(role)
            .map_err(|e| format!("test --qemu ({}): {e}", t.package))?;
        let path = guard.path().to_path_buf();
        socks.push(guard);
        Ok::<PathBuf, String>(path)
    };
    match t.netstack_peer {
        NetPeerMode::None => {}
        // The bond vertical: two NICs (the bond's two members) on two private
        // wires, one bond peer serving both, and a mid-flow monitor `set_link`
        // that drops the primary member's carrier once the flow is established.
        NetPeerMode::Bond => {
            // Two private wires, one per bond member. `net0` carries the
            // primary member ([`GUEST_MAC`]); `net1` the backup ([`GUEST_MAC_2`]).
            let p_qemu = wire(&mut socks, "net0q")?;
            let p_peer = wire(&mut socks, "net0p")?;
            let b_qemu = wire(&mut socks, "net1q")?;
            let b_peer = wire(&mut socks, "net1p")?;
            let started = super::netpeer::NetPeer::spawn_bond(&p_qemu, &p_peer, &b_qemu, &b_peer);
            peer = Some(started.map_err(|e| format!("test --qemu ({}): {e}", t.package))?);
            // Attach the two members in order, each with its pinned MAC and
            // its own `.pcap`, so the bond's `match.mac` binding is
            // deterministic and `net0` is the primary member.
            spec = spec.with_virtio_net_dgram_mac(
                &p_qemu,
                &p_peer,
                kernel.with_extension("net0.pcap"),
                tairix_test_netstack_wire::GUEST_MAC_STR,
            );
            spec = spec.with_virtio_net_dgram_mac(
                &b_qemu,
                &b_peer,
                kernel.with_extension("net1.pcap"),
                tairix_test_netstack_wire::GUEST_MAC_2_STR,
            );
            // Once the guest has served its first inbound echo — the bond is
            // up and carrying the flow over the primary member — drop that
            // member's carrier. The driver's virtio config-change interrupt
            // reports the link down and `netstack` fails the bond over to the
            // backup member; the guest then serves a further echo over it,
            // proving the flow survived.
            spec = spec.with_monitor_command(
                BOND_FAILOVER_TRIGGER_MARKER,
                1,
                format!(
                    "set_link {} off",
                    tairix_test_netstack_wire::BOND_PRIMARY_NETDEV_ID
                ),
            );
        }
        // Every single-wire peer role: one NIC over one `dgram` netdev, the
        // MAC pinned to the wire constant both sides agree on (the guest
        // derives its link-local from it).
        _ => {
            let pcap = kernel.with_extension("pcap");
            let qemu_sock = wire(&mut socks, "net0q")?;
            let peer_sock = wire(&mut socks, "net0p")?;
            let started = spawn_net_peer(t.netstack_peer, &qemu_sock, &peer_sock)
                .map_err(|e| format!("test --qemu ({}): {e}", t.package))?;
            // These four roles all prove success through the peer's inbound
            // echo campaign, whose confirming event — the peer receiving the
            // guest's reply — is the *last* link in the causal chain. Their
            // guest bins are built not to self-exit, so the run is ended by
            // the peer's completion gate rather than by a guest debug-exit
            // that would race, and lose to, the reply leaving the machine.
            // The TCP roles are safe with a guest-driven exit instead: the
            // peer has already received the whole transfer before the guest
            // can conclude, so there is no race to lose. V6PingResponder is
            // guest-active — the guest is the pinger and the last link in its
            // chain is inside the guest itself — so it too keeps a
            // guest-driven exit. Every other single-wire role falls into one
            // of those two safe shapes, so its gate stays unset here.
            if matches!(
                t.netstack_peer,
                NetPeerMode::V6LinkLocal
                    | NetPeerMode::V6StaticEcho
                    | NetPeerMode::V4DhcpEcho
                    | NetPeerMode::V6Dhcp6Echo
            ) {
                spec = spec.with_completion_gate(started.success_gate());
            }
            peer = Some(started);
            spec = spec.with_virtio_net_dgram_mac(
                &qemu_sock,
                &peer_sock,
                &pcap,
                tairix_test_netstack_wire::GUEST_MAC_STR,
            );
        }
    }
    Ok((spec, peer, socks))
}

/// Apply the `memtest` takeover verticals' run gates to `spec`, or return it
/// untouched for any other `binary`.
///
/// Those verticals test all of RAM *continuously* and never stop on their own
/// (`plans/NEW-SUPERVISOR.md` §9 Stage E): the machine only leaves the test by
/// a reset. Once the guest completes a full test loop — printing
/// [`MEMTEST_TAKEOVER_LOOP_MARKER`] — a QEMU-monitor `system_reset` ends the
/// endless test deterministically. Under `-no-reboot` that reset exits QEMU
/// with status 0; it is accepted as a pass only when the marker was printed
/// first, so a crash that reset before a completed loop still fails loud. The
/// declared runtime ceiling replaces the derived one, which describes no part
/// of a whole-RAM sweep. The gates are identical on all three bare-metal
/// targets, and being a pure function of the binary name they are assertable
/// without spawning a guest.
fn memtest_takeover_gates(spec: Spec, binary: &str) -> Spec {
    if !MEMTEST_TAKEOVER_BINARIES.contains(&binary) {
        return spec;
    }
    spec.with_reset_success_marker(MEMTEST_TAKEOVER_LOOP_MARKER)
        .with_monitor_command(MEMTEST_TAKEOVER_LOOP_MARKER, 1, "system_reset")
        .with_runtime_ceiling(MEMTEST_TAKEOVER_RUNTIME_CEILING)
}

/// Attach `t`'s remaining devices (network capture, display, input, the
/// scripted serial dialogue) to `spec` and drive the guest to its outcome.
/// `kernel` is the enrolment's binary path, which names the sibling capture
/// file.
fn finish_run(t: &QemuTest, kernel: &Path, replica: usize, spec: Spec) -> Result<(), String> {
    // `_wire_socks` is held for the whole run: dropping a reserved socket
    // removes its file, which would pull the wire out from under the guest.
    let (mut spec, peer, _wire_socks) = attach_net_peer(t, kernel, spec)?;

    spec = memtest_takeover_gates(spec, t.binary);

    // Attach a QEMU `ramfb` display device for the framebuffer vertical.
    if t.ramfb {
        spec = spec.with_ramfb();
    }

    // Attach a `virtio-keyboard-device` for the input vertical and let the
    // runner inject the key once the guest signals readiness on serial.
    if let Some((marker, key)) = t.keyboard {
        spec = spec.with_virtio_keyboard(marker, key);
    }

    // Attach a `virtio-keyboard-device` and type the scripted dialogue at
    // it, step by step — each step once its own readiness marker has
    // appeared the required number of times — the console-input path for
    // a display-console guest, where the serial script cannot reach.
    for (marker, occurrences, text) in t.typed_keys {
        spec = spec.with_typed_keys(*marker, *occurrences, *text);
    }

    // Arm the ordered, marker-gated screendumps: the host-side scan-out
    // readbacks. Any stale dump from an earlier run is removed first, so
    // the runner's completeness check (and the pixel asserts below) can
    // never read old bytes. Still-unsent pointer steps are held while the
    // current dump is pending.
    let mut screendump_paths: Vec<(PathBuf, ScreendumpAssert)> = Vec::new();
    for plan in t.screendumps {
        let path = kernel.with_extension(format!("{}.screendump.ppm", plan.suffix));
        std::fs::remove_file(&path)
            .or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            })
            .map_err(|e| format!("test --qemu ({}): remove stale screendump: {e}", t.package))?;
        spec = spec.with_screendump(plan.marker, plan.occurrences, &path);
        screendump_paths.push((path, plan.assert));
    }

    // Attach the pointer sibling after the keyboard — the interactive
    // session's two-identical-virtio-input-nodes topology — and let the
    // runner drive the computed script step by step, each once its own
    // marker appears. Each driver instance arms and prints the readiness
    // marker once, so the key injection waits for both markers: injecting
    // on the first (possibly the mouse's) would race the keyboard's own
    // arming and lose the press.
    if let Some(build_script) = t.pointer_script {
        for step in build_script().map_err(|e| format!("test --qemu ({}): {e}", t.package))? {
            spec = spec.with_pointer_step(step.ready_marker, step.ready_occurrences, step.action);
        }
        spec = spec.with_keyboard_ready_occurrences(2);
    }

    // Pipe QEMU's stdin for the interactive-session vertical and let the
    // runner replay the scripted exchange, each line typed once the guest
    // prints that step's prompt.
    for (marker, delay_after_marker, line) in t.serial {
        spec = spec.with_serial_input(*marker, *delay_after_marker, *line);
    }

    let serial_log = sidecar_path(kernel, t, replica, "serial.log");
    std::fs::remove_file(&serial_log)
        .or_else(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })
        .map_err(|e| {
            format!(
                "test --qemu ({}): remove stale serial log {}: {e}",
                t.package,
                serial_log.display()
            )
        })?;

    // Always collect the peer's verdict, even when the run itself failed,
    // so the thread never outlives the run unjoined.
    let run = Runner::run(&spec).map_err(|e| format!("test --qemu ({}): {e}", t.package));
    let peer_verdict = peer.map(super::netpeer::NetPeer::stop_and_join);
    let outcome = run?;
    // Persist the transcript before the outcome is judged, for every run
    // including a pass. A pass is the verdict with the most riding on it and
    // the least evidence behind it: the runner scores it from an exit status,
    // and a run that ended sooner than its choreography needs looks exactly
    // like one that completed. Keeping the transcript is what lets a reader
    // check a suspicious pass instead of having to re-derive it.
    persist_serial(t.package, &serial_log, outcome.serial())?;
    match outcome {
        Outcome::Pass { .. } => {
            // The guest passed, but the run is not verified until its dumps
            // and its link peer agree.
            for (path, assert) in &screendump_paths {
                if let Err(e) = assert(t, path) {
                    return Err(format!("{e} (full serial: {})", serial_log.display()));
                }
            }
            if let Some(Err(e)) = peer_verdict {
                return Err(format!(
                    "test --qemu ({}): {e} (full serial: {})",
                    t.package,
                    serial_log.display()
                ));
            }
            Ok(())
        }
        Outcome::Fail { status, serial } => {
            Err(format!(
                "test --qemu ({}) FAILED (qemu status {status}; full serial: {})\n--- serial ---\n{serial}\n--- end ---",
                t.package,
                serial_log.display()
            ))
        }
        Outcome::Timeout {
            budget,
            serial,
            cpu_state,
        } => {
            let hang = persist_hang_state(t.package, &sidecar_path(kernel, t, replica, "hang.txt"), &cpu_state)?;
            Err(format!(
                "test --qemu ({}) HUNG: the guest fell silent for its whole {budget:?} inactivity budget; the transcript's last line is the stall point (no retries per AGENTS.md §7; full serial: {}; guest cpu state: {hang})\n--- serial ---\n{serial}\n--- guest cpu state at the kill ---\n{cpu_state}\n--- end ---",
                t.package,
                serial_log.display()
            ))
        }
        Outcome::RuntimeCeilingExceeded {
            ceiling,
            silent_for,
            serial,
            cpu_state,
        } => {
            // The silence at the kill is the first thing a reader needs: near
            // zero means the guest was alive and working but never finished
            // (a choreography waiting on a witness that never arrives, or a
            // service retrying on a timer), while a silence close to the
            // ceiling means the guest went quiet early and stalled — the
            // transcript's last line is then the stall point.
            let hang = persist_hang_state(t.package, &sidecar_path(kernel, t, replica, "hang.txt"), &cpu_state)?;
            Err(format!(
                "test --qemu ({}) UNFINISHED at the {ceiling:?} runtime ceiling: the guest was still alive and never completed; silent for {silent_for:?} at the kill (no retries per AGENTS.md §7; full serial: {}; guest cpu state: {hang})\n--- serial ---\n{serial}\n--- guest cpu state at the kill ---\n{cpu_state}\n--- end ---",
                t.package,
                serial_log.display()
            ))
        }
    }
}

/// Persist the per-vCPU state a killed guest was interrogated for beside its
/// transcript, and return the path.
///
/// The transcript of a hang ends where the guest stopped talking, which is
/// the one thing about a hang that is never in doubt. What the cores were
/// actually doing at the kill — every one halted (nothing runnable, so a wake
/// was lost) versus one still executing with interrupts masked (a spin) — is
/// what decides where to look, and it exists only for as long as QEMU does.
fn persist_hang_state(package: &str, path: &Path, cpu_state: &str) -> Result<String, String> {
    std::fs::write(path, cpu_state).map_err(|e| {
        format!(
            "test --qemu ({package}): persist guest cpu state {}: {e}",
            path.display()
        )
    })?;
    Ok(path.display().to_string())
}

/// Persist a guest's complete serial transcript beside its kernel, whatever
/// the run's outcome.
///
/// A failure report also includes the transcript inline, but build output can
/// exceed a terminal or CI log's display limit, and a *pass* prints none at
/// all. The sidecar keeps the original bytes available for diagnosis without
/// changing the guest or rerunning the workload — which is the only way to
/// check after the fact that a vertical's pass came from the choreography it
/// claims and not from an early exit that scored the same.
fn persist_serial(package: &str, path: &Path, serial: &str) -> Result<(), String> {
    std::fs::write(path, serial).map_err(|e| {
        format!(
            "test --qemu ({}): persist serial transcript {}: {e}",
            package,
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        appbar_pointer_script, autoload_desktop_pointer_script, build_targets,
        desktop_hover_pointer_script, login_type_plant, persist_serial,
        pressure_deepened_past_moderate, qemu_host_budget_for, qemu_job_weight, sibling_serial_log,
        sidecar_path, FsDisk, PrimePlan, QemuTest, DESKTOP_PRESSURE_ICONS_DRAWN_DUMP,
        DESKTOP_PRESSURE_UNDER_PRESSURE_DUMP, MEMSOAK_PASS_PREFIX, SUPERVISOR_ESC_AT_PROMPT_SCRIPT,
        SUPERVISOR_ESC_SCRIPT, SUPERVISOR_MOUNT_SCRIPT, TCPECHO_PASS_PREFIX, TCPSERVE_PASS_PREFIX,
        TESTS, UNLOCK_PASSPHRASE_LINE, UNPROVISIONED_MACHINE_ID_MARKER,
        VALUE_OPERAND_PHYSICAL_LINE, VALUE_OPERAND_PHYSICAL_MARKER, VALUE_PIPE_PHYSICAL_LINE,
        VALUE_PIPE_PHYSICAL_MARKER, VALUE_PIPE_WRITE_REFUSED_MARKER,
    };
    use std::time::Duration;

    /// Every desktop click a pointer script drives is **one** step.
    ///
    /// The runner sends at most one step per poll tick, and a desktop acts on
    /// the press: a click scripted as a separate press and release therefore
    /// leaves a tick in which a guest whose PASS witness is that press's own
    /// effect can exit with the release still owed, which fails the run for an
    /// incomplete script. The icon-bar vertical lost exactly that race when the
    /// terminal stopped spawning a shell before asking for its window, so the
    /// scripts state a click as the single gesture it is.
    #[test]
    fn every_desktop_pointer_click_is_one_step() {
        use tairix_qemu::PointerAction;

        for (label, script) in [
            ("icon bar", appbar_pointer_script()),
            ("autoload desktop", autoload_desktop_pointer_script()),
            ("desktop hover", desktop_hover_pointer_script()),
        ] {
            let steps = script.unwrap_or_else(|e| panic!("{label} script builds: {e}"));
            for (index, step) in steps.iter().enumerate() {
                assert!(
                    !matches!(
                        step.action,
                        PointerAction::Press(_) | PointerAction::Release(_)
                    ),
                    "{label} step {index} halves a click across poll ticks: {:?}",
                    step.action
                );
            }
            assert!(
                matches!(
                    steps.last().map(|step| step.action),
                    Some(PointerAction::Click(_))
                ),
                "{label} script ends on something other than the click its guest exits on"
            );
        }
    }

    /// The enrolment with `disk`, for a test that asserts on what that disk
    /// plants. `label` names it in the failure a removed enrolment would
    /// otherwise report as an unrelated panic.
    fn enrolment_with(disk: FsDisk, label: &str) -> &'static QemuTest {
        TESTS
            .iter()
            .find(|t| t.fs_disk == disk)
            .unwrap_or_else(|| panic!("the {label} vertical is enrolled"))
    }

    /// A vertical whose script drives a shell plants a text-login document,
    /// and it lands on the volume that actually backs `/System/Settings`.
    ///
    /// The regression this pins is the *path*, not the content. The document
    /// was planted on the read-only `/System` volume, which the writable
    /// root's rebased `/System/Settings` sub-mount shadows, so nothing ever
    /// read it: the guest fell back to the compiled default and only met the
    /// text prompt because login was separately misreading an absent store.
    /// Planted components are therefore relative to the **root** volume and
    /// keep the leading `System` segment.
    #[test]
    fn a_shell_driving_vertical_plants_its_text_login_on_the_root_volume() {
        let autoload = enrolment_with(FsDisk::AutoloadRootDisk, "autoload");
        let (components, conf) = login_type_plant(autoload)
            .expect("the plant path is absolute")
            .expect("a shell-driving vertical asks for the text prompt");
        assert_eq!(
            components.join("/"),
            tairix_sysconfig::CONFIG_PATH
                .strip_prefix('/')
                .expect("the store path is absolute"),
        );
        assert_eq!(components.first().map(String::as_str), Some("System"));
        let planted = tairix_sysconfig::SystemConfig::parse(&conf)
            .expect("the engine renders a document it can parse");
        assert_eq!(planted.login_type, tairix_sysconfig::LoginType::Text);
    }

    /// The greeter vertical plants nothing, because an unconfigured machine
    /// is the state it exercises: a planted document of any kind would decide
    /// the very thing the vertical is there to observe.
    #[test]
    fn the_greeter_vertical_boots_an_unconfigured_machine() {
        let greeter = enrolment_with(FsDisk::GreeterRootDisk, "greeter");
        assert!(login_type_plant(greeter)
            .expect("the plant path is absolute")
            .is_none());
    }

    /// A served window's footprint is its app's client surface grown by the
    /// furniture band the window manager reserves, anchored at the cascade
    /// slot the session places it in — so the screendump assertion samples
    /// the whole window, and a click measured from the client reaches the
    /// application rather than the title bar above it.
    ///
    /// The regression this pins: pairing the *outer* origin with the *client*
    /// size yields a rectangle that is neither, shifted a title bar's height
    /// up into the furniture. It is caught here by round-tripping the client
    /// back through the compositor's own inverse.
    #[test]
    fn served_window_layout_insets_the_client_inside_its_furniture() {
        use super::{served_client_aim, served_window_layout};
        use tairix_geometry::Scale;

        let theme = tairix_theme::Theme::dark();
        let layout = served_window_layout(
            0,
            tairix_browse::WIN_WIDTH,
            tairix_browse::WIN_HEIGHT,
            tairix_browse::WIN_SIZING.resizable(),
            &theme,
        );
        assert_eq!(
            layout.outer.origin,
            tairix_desktop_session::windows::cascade_origin_for(0),
        );
        assert_eq!(
            (layout.client.width, layout.client.height),
            (tairix_browse::WIN_WIDTH, tairix_browse::WIN_HEIGHT),
        );
        let frame = tairix_controls::WindowFrame::new(tairix_controls::WindowFurnitureState {
            resizable: tairix_browse::WIN_SIZING.resizable(),
            ..tairix_controls::WindowFurnitureState::default()
        });
        assert_eq!(
            layout.outer,
            frame.outer_for_client(layout.client, Scale::ONE, &theme),
        );
        assert!(layout.client.left() > layout.outer.left());
        assert!(layout.client.top() > layout.outer.top());
        assert!(layout.client.right() <= layout.outer.right());
        assert!(layout.client.bottom() <= layout.outer.bottom());

        // Resizability no longer moves an edge: the grab zone is invisible, so
        // a resizable window's footprint is a fixed-size one's exactly.
        let fixed = served_window_layout(
            0,
            tairix_browse::WIN_WIDTH,
            tairix_browse::WIN_HEIGHT,
            false,
            &theme,
        );
        assert_eq!(fixed.outer, layout.outer);
        assert_eq!(fixed.client, layout.client);

        // It does change the hit map, so the scripted "focus this window" aim
        // must clear the resize zone that overlaps the client's outer pixels.
        // The frame's own hit map is the oracle here: deepening the theme's hit
        // slop past the aim inset fails this test rather than silently
        // resizing a window in a QEMU vertical.
        let aim = served_client_aim(0, tairix_browse::WIN_SIZING.resizable(), &theme);
        assert_eq!(
            frame.hit(layout.outer, Scale::ONE, &theme, aim),
            tairix_controls::FurniturePart::Client,
        );
    }

    /// Every `memtest` takeover binary `finish_run` scores by reset is
    /// actually enrolled, and the per-loop marker the runner keys on matches
    /// the exact line the `MemtestUi` prints — so the scoring hook and the
    /// guest's success signal cannot silently drift apart.
    #[test]
    fn memtest_takeover_binaries_are_enrolled_and_marker_is_stable() {
        use super::{MEMTEST_TAKEOVER_BINARIES, MEMTEST_TAKEOVER_LOOP_MARKER};
        for binary in MEMTEST_TAKEOVER_BINARIES {
            assert!(
                TESTS.iter().any(|t| t.binary == binary),
                "takeover binary {binary} finish_run scores by reset must be enrolled",
            );
        }
        assert_eq!(MEMTEST_TAKEOVER_LOOP_MARKER, "memtest: completed test loop");
    }

    /// A takeover run must be bounded by a ceiling sized to its sweep, not by
    /// the default multiple of its silence budget: one full sweep takes ~35 s
    /// on an idle host and ~4 minutes in the nightly soak, so the derived
    /// 2x-of-60 s ceiling killed a healthy, progressing guest mid-sweep.
    #[test]
    fn a_memtest_takeover_run_is_bounded_by_its_sweep_not_by_its_silence_budget() {
        use super::{
            memtest_takeover_gates, MEMTEST_TAKEOVER_BINARIES, MEMTEST_TAKEOVER_RUNTIME_CEILING,
        };
        use tairix_qemu::Spec;

        for binary in MEMTEST_TAKEOVER_BINARIES {
            let t = TESTS
                .iter()
                .find(|t| t.binary == binary)
                .expect("every takeover binary is enrolled");
            let plain = Spec::for_aarch64_kernel("/tmp/k").with_timeout(t.timeout);
            let derived = plain.runtime_ceiling();
            let gated = memtest_takeover_gates(plain, binary);
            assert_eq!(
                gated.runtime_ceiling(),
                MEMTEST_TAKEOVER_RUNTIME_CEILING,
                "{binary}: the takeover gates must declare the sweep-sized ceiling",
            );
            assert!(
                gated.runtime_ceiling() > derived,
                "{binary}: the declared ceiling must outlast the derived {derived:?}, \
                 which cut a progressing sweep short",
            );
            // The measured loaded sweep, with headroom for a worse night.
            assert!(MEMTEST_TAKEOVER_RUNTIME_CEILING >= Duration::from_mins(12));
            // The silence budget stays exactly as tight as it was.
            assert_eq!(gated.timeout, t.timeout);
        }
    }

    /// Only the takeover verticals get those gates: every other enrolment
    /// keeps the derived ceiling, so the carve-out cannot silently widen.
    #[test]
    fn a_non_takeover_run_keeps_the_derived_ceiling() {
        use super::{memtest_takeover_gates, MEMTEST_TAKEOVER_BINARIES};
        use tairix_qemu::Spec;

        let other = TESTS
            .iter()
            .find(|t| !MEMTEST_TAKEOVER_BINARIES.contains(&t.binary))
            .expect("the matrix enrols more than the takeover verticals");
        let plain = Spec::for_aarch64_kernel("/tmp/k").with_timeout(other.timeout);
        let derived = plain.runtime_ceiling();
        assert_eq!(
            memtest_takeover_gates(plain, other.binary).runtime_ceiling(),
            derived,
        );
    }

    /// The memory-stability vertical's serial-script marker is the leading
    /// prefix of the memsoak fixture's own success report line: it starts
    /// with the fixture's `PASS_MARKER` and matches the exact rendering
    /// `report_line` produces, so the script and the program cannot drift.
    #[test]
    fn memsoak_script_marker_matches_the_fixture_report() {
        assert!(MEMSOAK_PASS_PREFIX.starts_with(tairix_test_memsoak::PASS_MARKER));
        let pass = tairix_test_memsoak::report_line(tairix_test_memsoak::Verdict::Stable, 7, 7);
        assert!(
            pass.starts_with(MEMSOAK_PASS_PREFIX),
            "fixture PASS line {pass:?} must start with the script marker {MEMSOAK_PASS_PREFIX:?}"
        );
    }

    /// The value-pipe vertical's machine-id marker is exactly what the
    /// resolver renders for an *unprovisioned* machine id: the kernel reports
    /// sixteen zero bytes until an installer mints one, and `info:` renders a
    /// machine id as lowercase hex, so the marker is 32 `0` characters. Pinned
    /// by width and content here so a change to either the sentinel or the
    /// hex rendering fails this test instead of silently making the QEMU
    /// vertical wait for a string the guest never prints.
    #[test]
    fn value_pipe_machine_id_marker_matches_the_unprovisioned_rendering() {
        // Two hex digits per byte of a 16-byte machine id.
        assert_eq!(UNPROVISIONED_MACHINE_ID_MARKER.len(), 16 * 2);
        assert!(UNPROVISIONED_MACHINE_ID_MARKER
            .bytes()
            .all(|byte| byte == b'0'));
    }

    /// The value-pipe vertical's write-refusal marker is exactly the `Display`
    /// text of the errno the kernel resource resolver refuses a value-backed
    /// namespace with, so the script waits for the wording the shell actually
    /// prints and the two cannot drift.
    #[test]
    fn value_pipe_write_refusal_marker_matches_the_errno_text() {
        assert_eq!(
            VALUE_PIPE_WRITE_REFUSED_MARKER,
            tairix_abi::Errno::NotSupported.to_string()
        );
    }

    /// The value-pipe vertical's success marker is the exact word its own
    /// `echo` prints, so the gate and the command cannot drift apart.
    #[test]
    fn value_pipe_physical_marker_is_the_line_it_echoes() {
        assert!(
            VALUE_PIPE_PHYSICAL_LINE.contains(VALUE_PIPE_PHYSICAL_MARKER),
            "{VALUE_PIPE_PHYSICAL_LINE:?} must echo {VALUE_PIPE_PHYSICAL_MARKER:?}"
        );
        // `&&` is what makes the marker an assertion about `cat`'s exit
        // status rather than an unconditional print.
        assert!(VALUE_PIPE_PHYSICAL_LINE.contains("&&"));
        assert!(VALUE_OPERAND_PHYSICAL_LINE.contains(VALUE_OPERAND_PHYSICAL_MARKER));
        assert!(VALUE_OPERAND_PHYSICAL_LINE.contains("&&"));
        // The two spellings differ only in the redirection, so a regression in
        // either reader cannot hide behind the other's marker.
        assert!(VALUE_PIPE_PHYSICAL_LINE.contains("< info:mem/physical"));
        assert!(!VALUE_OPERAND_PHYSICAL_LINE.contains('<'));
    }

    /// The stream vertical's serial-script marker is exactly the `tcpecho`
    /// fixture's own success marker, so the script waits for the marker the
    /// program actually prints and the two cannot drift.
    #[test]
    fn tcpecho_script_marker_matches_the_fixture_marker() {
        assert_eq!(TCPECHO_PASS_PREFIX, tairix_test_tcpecho::PASS_MARKER);
    }

    /// The listener vertical's serial-script marker is exactly the `tcpserve`
    /// fixture's own success marker, so the script waits for the marker the
    /// program actually prints and the two cannot drift.
    #[test]
    fn tcpserve_script_marker_matches_the_fixture_marker() {
        assert_eq!(TCPSERVE_PASS_PREFIX, tairix_test_tcpserve::PASS_MARKER);
    }

    /// All three pre-boot-Supervisor serial scripts must spell the frozen
    /// boot-screen states identically — the boot-screen wording is one
    /// contract, never a per-script copy that could silently drift
    /// (`plans/NEW-SUPERVISOR.md` §0). The canonical spellings live in
    /// [`SUPERVISOR_ESC_SCRIPT`]; the two sibling scripts for the other trigger
    /// points reuse exactly those markers.
    #[test]
    fn supervisor_scripts_share_the_frozen_boot_screen_markers() {
        let announce = SUPERVISOR_ESC_SCRIPT[0].0;
        let banner = SUPERVISOR_ESC_SCRIPT[1].0;
        let commands = SUPERVISOR_ESC_SCRIPT[2].0;
        let prompt = SUPERVISOR_ESC_SCRIPT[3].0;
        let esc = "\u{1b}";

        // ESC at the live passphrase prompt: enter at the *prompt* (not the
        // announcement) by typing a lone ESC as the line's first byte, then
        // the same banner/help-header/continue/passphrase round-trip.
        assert_eq!(
            SUPERVISOR_ESC_AT_PROMPT_SCRIPT.len(),
            4,
            "prompt-entry script is: prompt+ESC, banner+help, commands+continue, prompt+passphrase"
        );
        assert_eq!(SUPERVISOR_ESC_AT_PROMPT_SCRIPT[0].0, prompt);
        assert_eq!(SUPERVISOR_ESC_AT_PROMPT_SCRIPT[0].2, esc);
        assert_eq!(SUPERVISOR_ESC_AT_PROMPT_SCRIPT[1].0, banner);
        assert_eq!(SUPERVISOR_ESC_AT_PROMPT_SCRIPT[2].0, commands);
        assert_eq!(SUPERVISOR_ESC_AT_PROMPT_SCRIPT[3].0, prompt);
        assert_eq!(SUPERVISOR_ESC_AT_PROMPT_SCRIPT[3].2, UNLOCK_PASSPHRASE_LINE);

        // mount-from-REPL: enter at the announcement window, run `mount`, then
        // satisfy the `mount` command's own passphrase prompt.
        assert_eq!(
            SUPERVISOR_MOUNT_SCRIPT.len(),
            3,
            "mount script is: announcement+ESC, banner+mount, prompt+passphrase"
        );
        assert_eq!(SUPERVISOR_MOUNT_SCRIPT[0].0, announce);
        assert_eq!(SUPERVISOR_MOUNT_SCRIPT[0].2, esc);
        assert_eq!(SUPERVISOR_MOUNT_SCRIPT[1].0, banner);
        assert_eq!(SUPERVISOR_MOUNT_SCRIPT[1].2, "mount\n");
        assert_eq!(SUPERVISOR_MOUNT_SCRIPT[2].0, prompt);
        assert_eq!(SUPERVISOR_MOUNT_SCRIPT[2].2, UNLOCK_PASSPHRASE_LINE);
    }

    /// The artwork assertion's band scoping resolves the transcript beside the
    /// frame it is judging, and reads the marker out of it.
    ///
    /// The derivation is what decides whether a deep-pressure run is scoped
    /// out, and it only ever runs on a busy host — so it is pinned here rather
    /// than left to be exercised by the load that first needed it.
    #[test]
    fn the_artwork_assertion_scopes_itself_by_the_transcripts_band_marker() {
        let pressure = TESTS
            .iter()
            .find(|t| t.package == "tairix-test-desktop-pressure-qemu-aarch64")
            .expect("the pressure vertical is enrolled");
        let dir = std::env::temp_dir().join("tairix-pressure-band-scope");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let dump = dir.join(format!(
            "{}.{DESKTOP_PRESSURE_UNDER_PRESSURE_DUMP}.screendump.ppm",
            pressure.binary
        ));
        let log = dir.join(format!("{}.serial.log", pressure.binary));
        assert_eq!(
            sibling_serial_log(pressure, &dump).expect("the transcript is named"),
            log,
            "the transcript is the frame's own sibling"
        );

        // A run that stayed within the bands the artwork is promised through.
        std::fs::write(&log, "boot\ndesktop revealed\n").expect("write");
        assert!(!pressure_deepened_past_moderate(pressure, &dump).expect("read"));

        // A run that went deeper, where the glyph tier is the honest answer.
        std::fs::write(
            &log,
            format!(
                "boot\n{}\n",
                tairix_test_desktop_pressure_qemu_aarch64::PRESSURE_DEEPENED_MARKER
            ),
        )
        .expect("write");
        assert!(pressure_deepened_past_moderate(pressure, &dump).expect("read"));

        // No transcript at all fails closed rather than reading as shallow,
        // which would apply the strict bound to a run whose bands are unknown.
        std::fs::remove_file(&log).expect("remove");
        assert!(pressure_deepened_past_moderate(pressure, &dump).is_err());

        // A frame that is not the under-pressure one cannot name a transcript.
        let other = dir.join(format!(
            "{}.{DESKTOP_PRESSURE_ICONS_DRAWN_DUMP}.screendump.ppm",
            pressure.binary
        ));
        assert!(sibling_serial_log(pressure, &other).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No two runs of the matrix may write to the same sidecar path, or the
    /// concurrent runner could let one clobber another's image while its QEMU
    /// still has it open — rewriting a live guest's disk underneath it — or
    /// attribute one run's transcript to another. Two things collide and
    /// [`sidecar_path`] separates both: enrolments sharing one built binary
    /// (the pre-boot Supervisor verticals drive the byte-identical guest
    /// through different serial scripts), and the flake hunt's concurrent
    /// replicas of a single enrolment. Both sidecar kinds are checked, because
    /// both are per-run outputs. Replica zero of a singly-enrolled binary
    /// keeps the plain `<binary>.<ext>` name, so the pull-request matrix's
    /// paths are unchanged.
    #[test]
    fn sidecar_paths_never_collide_across_enrolments_or_replicas() {
        use std::collections::{HashMap, HashSet};
        use std::path::Path;

        // More replicas than `ci_long::REPS` so the check outlives a change
        // to the hunt's repetition count.
        const REPLICAS: usize = 32;

        let mut by_binary: HashMap<&str, Vec<&QemuTest>> = HashMap::new();
        for t in TESTS {
            by_binary.entry(t.binary).or_default().push(t);
        }
        for ext in ["arxfs.img", "serial.log"] {
            for (binary, group) in &by_binary {
                let kernel = Path::new("target").join("dummy").join(binary);
                let mut seen = HashSet::new();
                for t in group {
                    for replica in 0..REPLICAS {
                        let path = sidecar_path(&kernel, t, replica, ext);
                        assert!(
                            seen.insert(path.clone()),
                            "{ext} sidecar path {path:?} collides within binary {binary}"
                        );
                    }
                }
                if group.len() == 1 {
                    assert_eq!(
                        sidecar_path(&kernel, group[0], 0, ext),
                        kernel.with_extension(ext),
                        "replica zero of a single-enrolment binary must keep its plain {ext} name",
                    );
                }
            }
        }
    }

    /// The `ping` vertical types the peer's own link-local address as its
    /// target, so the typed literal and the address the responder forms from
    /// the shared wire identifier cannot drift.
    #[test]
    fn ping_command_targets_the_peer_link_local() {
        let peer = tairix_test_netstack_wire::link_local(tairix_test_netstack_wire::PEER_IID);
        let rendered = format!("{peer}");
        assert!(
            super::PING_COMMAND_LINE.contains(&rendered),
            "ping command {:?} must target the peer link-local {rendered}",
            super::PING_COMMAND_LINE
        );
        assert!(super::PING_COMMAND_LINE.ends_with('\n'));
    }

    /// The telnet vertical types the peer's own link-local address as its
    /// target, with no port operand so the tool's own default is exercised;
    /// and the line it then types into the session is the shared probe. Pin
    /// all three so a typed literal cannot drift from what the peer expects.
    #[test]
    fn telnet_command_targets_the_peer_link_local_on_the_default_port() {
        let peer = tairix_test_netstack_wire::link_local(tairix_test_netstack_wire::PEER_IID);
        let rendered = format!("{peer}");
        assert!(
            super::TELNET_COMMAND_LINE.contains(&rendered),
            "telnet command {:?} must target the peer link-local {rendered}",
            super::TELNET_COMMAND_LINE
        );
        assert_eq!(
            super::TELNET_COMMAND_LINE,
            format!("telnet {rendered}\n"),
            "no port operand: the run exercises the tool's own default port",
        );
        assert_eq!(
            tairix_test_netstack_wire::PEER_TELNET_PORT,
            23,
            "the peer listens on the port the tool defaults to",
        );
        assert_eq!(
            super::TELNET_PROBE_LINE,
            format!("{}\n", tairix_test_netstack_wire::TELNET_PROBE),
            "the typed probe is the shared constant the peer matches on",
        );
    }

    /// The quit sequence is the *default* escape character followed by the
    /// interpreter's `quit`, so the vertical exercises the escape recognition
    /// and the command interpreter rather than a value only the test knows.
    #[test]
    fn the_telnet_quit_sequence_uses_the_default_escape_character() {
        let bytes = super::TELNET_QUIT_SEQUENCE.as_bytes();
        assert_eq!(
            bytes[0],
            tairix_telnet::command::DEFAULT_ESCAPE,
            "the sequence opens with the tool's own default escape character",
        );
        assert_eq!(&bytes[1..], b"quit\n");
    }

    #[test]
    fn spawn_session_overlong_username_tracks_the_account_format_bound() {
        // Exactly one character past the bound, and nothing else: the payload
        // is `MAX_USERNAME_LEN + 1` printable characters with no trailing
        // newline (or any other byte). Login reads the field in the raw
        // discipline (`round_begin` selects it before any read), so the view
        // refuses the instant the over-bound character arrives — it never
        // waits for Enter. Keeping the refusal-triggering character the *last*
        // byte of the serial step is what makes the vertical deterministic:
        // the harness marks the step fully sent the moment it writes that
        // byte, strictly before login can consume it and exit, so login's
        // fail-closed exit (and the PASS finisher that rides its relaunch)
        // can never win the race against a still-unsent trailing byte. A
        // trailing newline here is exactly the byte login never reads before
        // exiting, and its re-introduction is the flaky failure this guards.
        assert_eq!(
            super::OVERLONG_USERNAME.len(),
            tairix_users::MAX_USERNAME_LEN + 1
        );
        assert!(
            !super::OVERLONG_USERNAME.ends_with('\n'),
            "a trailing newline is the unconsumed byte that reintroduces the flaky race"
        );
        assert!(super::OVERLONG_USERNAME.bytes().all(|byte| byte == b'x'));
    }

    #[test]
    fn every_served_window_click_gates_on_that_window_being_created() {
        // Regression guard for two defects with one cause: a click fired
        // before its target window existed.
        //
        // D10 keyed the terminal click on the window endpoint's
        // `CallReplied` count, which counts *presents* too, so a repaint
        // inflated it and clicked the empty desktop. D31 keyed the files
        // click on a reply over that same endpoint, which every client of
        // the shared rendezvous produces — the Switchboard's start-up
        // desktop query fired it half a second before the files window was
        // created. Both clicks now gate on window **creation**: exactly one
        // shared-frame `shm_map` per window, which no query, present or
        // reply can advance.
        let script = super::autoload_desktop_pointer_script().expect("build the pointer script");
        // Located by marker and occurrence count, never by position, since
        // the FM9-a file-manager stage appends further clicks after these
        // (`plans/NEW-FILEMANAGER.md` FM9-a).
        let click_on = |occurrences: u32| -> Vec<&tairix_qemu::PointerStep> {
            script
                .iter()
                .filter(|step| {
                    step.ready_marker == super::AUTOLOAD_WINDOW_MAP_MARKER
                        && step.ready_occurrences == occurrences
                })
                .collect()
        };

        // The present-inclusive marker the fragile D10 gate used, and the
        // shared-rendezvous reply the D31 one used, reconstructed exactly as
        // the script builds them so this test fails if either is restored.
        let mut endpoint_hex = [0u8; 16];
        let call_replied = format!(
            "{} endpoint={}",
            tairix_kernel_ipc::AuditEvent::CallReplied.message(),
            tairix_util::fmt::format_hex_u64(
                tairix_abi::window_ipc::WINDOW_ENDPOINT,
                &mut endpoint_hex,
            ),
        );

        for (window, occurrences) in [
            ("files", super::AUTOLOAD_FILES_WINDOW_MAP_OCCURRENCES),
            ("terminal", super::AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES),
        ] {
            let click = click_on(occurrences);
            assert_eq!(
                click.len(),
                2,
                "the {window}-window click is one move plus the click itself, both on the map \
                 marker"
            );
            for step in click {
                assert_ne!(
                    step.ready_marker, call_replied,
                    "the {window}-window click must not gate on a reply over the shared \
                     window rendezvous: it counts presents, and every client answers on it"
                );
            }
        }

        // The creation-based contract: the marker is the shared `sc=<name>`
        // syscall trace, and each window's own frame map is a distinct
        // position in one monotonic sequence — the boot framebuffer
        // scan-out, then the files window, then the terminal window.
        assert_eq!(super::AUTOLOAD_WINDOW_MAP_MARKER, "sc=shm_map");
        assert_eq!(
            super::AUTOLOAD_FILES_WINDOW_MAP_OCCURRENCES,
            tairix_test_autoload_input_qemu_aarch64::FILES_WINDOW_FRAME_MAPS
        );
        assert_eq!(
            super::AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES,
            tairix_test_autoload_input_qemu_aarch64::TERMINAL_WINDOW_FRAME_MAPS
        );
        const {
            assert!(
                tairix_test_autoload_input_qemu_aarch64::FILES_WINDOW_FRAME_MAPS
                    < tairix_test_autoload_input_qemu_aarch64::TERMINAL_WINDOW_FRAME_MAPS,
                "the files window is created before the terminal window"
            );
        }
        assert_eq!(
            tairix_test_autoload_input_qemu_aarch64::TERMINAL_WINDOW_FRAME_MAPS,
            3
        );
    }

    /// A `P6` screendump of the emulated screen's own extent whose pixels
    /// come from `colour`, in exactly the shape QEMU writes, so a fixture
    /// frame reaches an assertion through the production parser.
    fn synthetic_frame(
        colour: impl Fn(u32, u32) -> (u8, u8, u8),
    ) -> tairix_qemu::screendump::Image {
        let width = tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX;
        let height = tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX;
        let mut bytes = format!("P6\n{width} {height}\n255\n").into_bytes();
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = colour(x, y);
                bytes.extend_from_slice(&[r, g, b]);
            }
        }
        tairix_qemu::screendump::parse_ppm(&bytes).expect("a well-formed fixture frame")
    }

    /// The enrolment and dump path a fixture assertion names in its
    /// messages; neither is read, so any enrolment serves.
    fn fixture_subject() -> (&'static super::QemuTest, &'static std::path::Path) {
        (
            super::TESTS
                .first()
                .expect("the enrolment table is not empty"),
            std::path::Path::new("fixture.screendump.ppm"),
        )
    }

    /// The reconstruction resolves the default master by its category as
    /// well as its name, and names it as the image plants it. A master
    /// refiled under another category leaves `default_wallpaper_path()`
    /// spelling a path no image carries, which this catches on the host
    /// rather than as an unexplained pixel mismatch in a guest run.
    #[test]
    fn the_default_wallpaper_master_is_found_under_its_own_category() {
        let (master, _) = super::default_wallpaper_master().expect("the default master ships");
        assert_eq!(
            master.category,
            Some(tairix_wallpaper::DEFAULT_WALLPAPER_CATEGORY)
        );
        assert_eq!(master.file, tairix_wallpaper::DEFAULT_WALLPAPER);
        assert!(
            tairix_wallpaper::default_wallpaper_path().ends_with(&super::master_name(master)),
            "a master is named as the image plants it"
        );
    }

    /// The desktop assertion accepts exactly the frame the desktop's own
    /// wallpaper pipeline produces. Without this the three rejection tests
    /// below would be satisfied by an assertion that refuses everything.
    #[test]
    fn the_recomputed_wallpaper_is_the_frame_the_desktop_assertion_accepts() {
        let (t, path) = fixture_subject();
        let theme = tairix_theme::Theme::dark();
        let wallpaper = super::expected_wallpaper().expect("recompute the default wallpaper");
        let frame = synthetic_frame(|x, y| wallpaper.rgb_at(x, y).expect("a canvas pixel"));
        super::assert_desktop_wallpaper(t, path, &frame, &theme, &[])
            .expect("the recomputed wallpaper is the composited desktop");
    }

    /// A frame flat-filled with the theme's own desktop colour — the
    /// backdrop a session paints when it composites no wallpaper at all —
    /// is rejected.
    #[test]
    fn a_flat_theme_coloured_frame_is_not_the_composited_desktop() {
        let (t, path) = fixture_subject();
        let theme = tairix_theme::Theme::dark();
        let desktop = theme.palette().desktop;
        let frame = synthetic_frame(|_, _| (desktop.r, desktop.g, desktop.b));
        let err = super::assert_desktop_wallpaper(t, path, &frame, &theme, &[])
            .expect_err("a wallpaper-less desktop is refused");
        assert!(
            err.contains("is not the composited desktop"),
            "unexpected refusal: {err}"
        );
    }

    /// A dark frame with sparse light text — a boot console left on screen,
    /// the failure the desktop screendump assertions exist to catch — is
    /// rejected.
    #[test]
    fn a_boot_console_frame_is_not_the_composited_desktop() {
        let (t, path) = fixture_subject();
        let theme = tairix_theme::Theme::dark();
        let frame = synthetic_frame(|x, y| {
            if y % 16 < 8 && x % 8 < 4 {
                (0xD0, 0xD0, 0xD0)
            } else {
                (0, 0, 0)
            }
        });
        let err = super::assert_desktop_wallpaper(t, path, &frame, &theme, &[])
            .expect_err("a boot console is refused");
        assert!(
            err.contains("is not the composited desktop"),
            "unexpected refusal: {err}"
        );
    }

    /// The wallpaper with a single sampled pixel wrong is rejected: the
    /// assertion judges the pixels it claims to, so a frame that is only
    /// nearly the desktop's own wallpaper cannot pass.
    #[test]
    fn one_wrong_pixel_at_a_sample_point_is_not_the_composited_desktop() {
        let (t, path) = fixture_subject();
        let theme = tairix_theme::Theme::dark();
        let wallpaper = super::expected_wallpaper().expect("recompute the default wallpaper");
        let samples = super::wallpaper_sample_points(
            wallpaper.width,
            wallpaper.height,
            &super::desktop_chrome_regions(&theme, &[]),
        );
        let (wrong_x, wrong_y) = *samples.first().expect("a sampleable wallpaper point");
        let frame = synthetic_frame(|x, y| {
            let (r, g, b) = wallpaper.rgb_at(x, y).expect("a canvas pixel");
            if (x, y) == (wrong_x, wrong_y) {
                (r ^ 0xFF, g, b)
            } else {
                (r, g, b)
            }
        });
        let err = super::assert_desktop_wallpaper(t, path, &frame, &theme, &[])
            .expect_err("a wrong sampled pixel is refused");
        assert!(
            err.contains(&format!("({wrong_x}, {wrong_y})")),
            "the refusal must name the point that differs: {err}"
        );
    }

    #[test]
    fn guest_serial_is_persisted_verbatim() {
        let path = std::env::temp_dir().join(format!(
            "tairix-xtask-serial-{}-{}.log",
            std::process::id(),
            line!()
        ));
        let transcript = "boot\0serial\nfinal marker\n";
        persist_serial("fixture", &path, transcript).expect("persist transcript");
        let actual = std::fs::read_to_string(&path).expect("read transcript");
        std::fs::remove_file(&path).expect("remove transcript");
        assert_eq!(actual, transcript);
    }

    #[test]
    fn priming_resolves_and_exports_both_when_nothing_is_pinned() {
        let plan = PrimePlan::from_env(false, false);
        assert!(plan.discover, "must resolve the toolchain when unset");
        assert!(plan.set_clang);
        assert!(plan.set_lld);
    }

    #[test]
    fn priming_never_clobbers_an_operator_override() {
        // Both pinned: nothing to resolve, and neither variable is touched.
        let both = PrimePlan::from_env(true, true);
        assert!(!both.discover);
        assert!(!both.set_clang);
        assert!(!both.set_lld);

        // One pinned: resolve, but only export the *unpinned* one.
        let clang_only = PrimePlan::from_env(true, false);
        assert!(clang_only.discover);
        assert!(!clang_only.set_clang, "must not overwrite a pinned clang");
        assert!(clang_only.set_lld);

        let lld_only = PrimePlan::from_env(false, true);
        assert!(lld_only.discover);
        assert!(lld_only.set_clang);
        assert!(!lld_only.set_lld, "must not overwrite a pinned ld.lld");
    }

    #[test]
    fn qemu_weight_charges_process_headroom_and_isolates_smp() {
        // A uniprocessor guest charges its vCPU plus one emulator/I/O unit.
        assert_eq!(
            qemu_job_weight(1, 16),
            2,
            "one-vCPU guest charges vCPU + process headroom"
        );
        // An SMP guest charges the whole budget, so it runs alone — a
        // co-scheduled SMP guest starves into its own in-guest lockup.
        assert_eq!(
            qemu_job_weight(4, 16),
            16,
            "SMP guest reserves the whole budget and runs alone"
        );
        assert_eq!(
            qemu_job_weight(0, 16),
            2,
            "a mis-recorded zero-CPU enrolment still fails safe to one vCPU"
        );
        // Clamps: on a tiny budget a uniprocessor guest still fits, and an
        // SMP guest never charges below one.
        assert_eq!(qemu_job_weight(1, 1), 1);
        assert_eq!(qemu_job_weight(4, 0), 1);
    }

    #[test]
    fn qemu_budget_is_one_third_of_the_logical_cpus() {
        // One third of the host's logical CPUs, clamped to one — deliberate
        // headroom so guests are never oversubscribed into missing their own
        // internal deadlines. On a 22-thread host that is 7, admitting ~3
        // co-scheduled uniprocessor guests (weight 2 each) while an SMP guest
        // (weight = budget) runs alone.
        assert_eq!(qemu_host_budget_for(0), 1);
        assert_eq!(qemu_host_budget_for(1), 1);
        assert_eq!(qemu_host_budget_for(3), 1);
        assert_eq!(qemu_host_budget_for(6), 2);
        assert_eq!(qemu_host_budget_for(22), 7);
        assert_eq!(qemu_host_budget_for(64), 21);
    }

    /// The smallest inactivity (no-progress) budget any enrolment may carry.
    ///
    /// Every enrolled QEMU test is a boot-then-do-fixed-work vertical, and its
    /// budget is the longest it may fall silent before the runner treats it as
    /// hung — not a total-runtime deadline. Because the budget is measured
    /// against *silence* rather than wall-clock, it is immune to how many
    /// guests are co-scheduled: a slow guest keeps emitting output and is
    /// never killed. This floor is the reachable minimum the guard below
    /// enforces; the runner applies each enrolment's own
    /// [`super::QemuTest::timeout`] verbatim on a developer machine and a CI
    /// runner alike, with no split that could shorten it.
    const MIN_REACHABLE_BUDGET: Duration = Duration::from_secs(60);

    #[test]
    fn build_targets_are_distinct_and_cover_every_enrolment() {
        let targets = build_targets();
        // No triple appears twice — each is built in exactly one invocation.
        for (i, a) in targets.iter().enumerate() {
            for b in &targets[i + 1..] {
                assert_ne!(a, b, "duplicate build target {a}");
            }
        }
        // Every enrolled test's triple is covered by the grouped build.
        for t in TESTS {
            assert!(
                targets.contains(&t.target),
                "build_targets missing {}",
                t.target
            );
        }
    }

    /// Regression guard for the enrolment budgets. Every enrolment must carry
    /// an inactivity budget of at least [`MIN_REACHABLE_BUDGET`], and that
    /// budget is what the runner enforces verbatim — there is no
    /// developer-vs-CI split that could shorten it. The budget bounds how long
    /// a guest may fall *silent*, so it must comfortably exceed the longest
    /// gap between two consecutive lines of a healthy guest's output; nothing
    /// may re-introduce a budget, or a clamp, below this floor.
    #[test]
    fn every_enrolment_budget_is_at_least_the_reachable_floor() {
        for t in TESTS {
            assert!(
                t.timeout >= MIN_REACHABLE_BUDGET,
                "enrolment {} budget {:?} is below the reachable floor {:?}; the \
                 inactivity budget must exceed the longest gap in a healthy \
                 guest's serial output",
                t.package,
                t.timeout,
                MIN_REACHABLE_BUDGET,
            );
        }
    }
}
