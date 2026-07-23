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
//! runner charges one-vCPU guests for the vCPU plus emulator/I/O work against
//! one quarter of the host's effective logical capacity. SMP TCG guests reserve
//! that complete budget and therefore run alone: their synchronising vCPUs must
//! make simultaneous host progress and cannot safely share a wall-clock budget
//! with other CPU-bound emulators. See [`run_once`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use tairix_itest_harness::pie::PieArch;
use tairix_qemu::{Outcome, Runner, Spec};

use super::image_apps::AppStoreFile;
use super::parallel::{self, Job};
use crate::Context;

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
    /// Hard wall-clock budget.
    timeout: Duration,
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
    /// A v6-link-local-only *active TCP client* (the N6b-2-β-2 listener
    /// vertical): same deterministic link-local addressing as
    /// [`Self::V6LinkLocal`], but the peer connects to the guest
    /// `tcpserve` server on `tairix_test_netstack_wire::GUEST_TCP_PORT`,
    /// streams the whole transfer, verifies the guest echoes every byte
    /// back, and injects bounded frame loss so the stream survives
    /// retransmission (the role-swapped mirror of [`Self::V6TcpEcho`]).
    V6TcpConnect,
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
    /// The [`Self::EncryptedRootDisk`] layout whose **read-only `/System`
    /// volume** additionally carries the test-only `memsoak` fixture bundle
    /// ([`super::image_apps::memsoak_store_files`]) — the memory-stability
    /// vertical's backing (`plans/APPS.md` "Immediate work" I2/I3). The
    /// fixture crate lives outside the userland discovery walk, so only
    /// this disk ever carries it; no production image ships it.
    MemsoakRootDisk,
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

/// Serial marker of an app-ward window-event delivery: the kernel/ipc
/// `MessageDelivered` audit record, emitted when a message lands in a
/// bound port's mailbox. In this vertical the desktop session's window
/// engine is the only port sender (window events to a served app), so
/// each occurrence is one delivered window event, kernel-attested —
/// imported from the kernel/ipc vocabulary, never a literal.
const AUTOLOAD_WINDOW_EVENT_MARKER: &str =
    tairix_kernel_ipc::AuditEvent::MessageDelivered.message();

/// How many [`AUTOLOAD_WINDOW_EVENT_MARKER`] occurrences key the second
/// screendump (the served files window on the dark desktop) — the
/// vertical's shared interaction contract, defined once beside the guest
/// PASS gate that also consumes it.
const AUTOLOAD_WINDOW_DUMP_OCCURRENCES: u32 =
    tairix_test_autoload_input_qemu_aarch64::WINDOW_DUMP_DELIVERIES;

/// How many [`AUTOLOAD_WINDOW_EVENT_MARKER`] occurrences key the third
/// screendump (the light-theme desktop with the window still composited)
/// — the same shared contract (a wake boundary past the re-theme
/// present, see the contract crate's rationale).
const AUTOLOAD_APPEARANCE_DUMP_OCCURRENCES: u32 =
    tairix_test_autoload_input_qemu_aarch64::APPEARANCE_DUMP_DELIVERIES;

/// The post-toggle in-window click's own delivery count: the first
/// handshake click is keyed on it, so it lands in a wake after the
/// re-themed frame was presented.
const AUTOLOAD_TOGGLE_CLICK_OCCURRENCES: u32 = 3;

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

/// How many [`AUTOLOAD_WINDOW_MAP_MARKER`] occurrences gate the
/// terminal-window click (`plans/APPWIN.md` AW4): the boot framebuffer
/// scan-out map, the files window's create map, then the terminal
/// window's create map — after which the terminal window exists at its
/// cascade slot and the click focuses it, no matter how many times any
/// window repainted. The shared contract, so the click can never race
/// the window's existence.
const AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES: u32 =
    tairix_test_autoload_input_qemu_aarch64::TERMINAL_WINDOW_FRAME_MAPS;

/// How many [`AUTOLOAD_WINDOW_EVENT_MARKER`] occurrences gate the typed
/// shell command: the terminal-window click's own deliveries (the files
/// window's unfocus, the terminal's focus, and the press), after which
/// the terminal is provably the focused key recipient — the shared
/// contract.
const AUTOLOAD_TERMINAL_TYPE_OCCURRENCES: u32 =
    tairix_test_autoload_input_qemu_aarch64::TERMINAL_TYPE_DELIVERIES;

/// The shell command the autoload vertical types into the focused
/// terminal at the seat keyboard — the shared contract (`true` plus
/// Enter): the shell resolving and spawning it is the guest's AW4
/// round-trip witness.
const AUTOLOAD_TERMINAL_COMMAND: &str = tairix_test_autoload_input_qemu_aarch64::TERMINAL_COMMAND;

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
/// prompt — login has no session selector (`os.loginType` defaults to
/// text), so the authenticated session is the shell and the desktop is
/// started exactly as a user starts it, by typing `desktop` (the system
/// app store's `desktop.app`, the bundle a graphical login also spawns).
/// Pinned against the fixture credentials below so the dialogue and the
/// planted account cannot drift; a renamed bundle makes the vertical
/// time out loudly at the `FIRST_PRESENT` gate, never pass on the wrong
/// exchange.
const AUTOLOAD_LOGIN_DIALOGUE: &str = "root\nroot\ndesktop\n";

/// Serial marker after which the autoload vertical takes its screendump
/// **and** injects the mouse motion: the display service's one-shot
/// `FIRST_PRESENT` log record — the witness that the desktop session's
/// composited frame reached the scan-out surface. Imported from the
/// driver crate's own definition, so the emitter and this consumer can
/// never drift. Keying both the dump and the pointer on it makes the
/// chain strictly ordered: present → verified dump → mouse motion → the
/// guest's `kind=pointer` witness → PASS — a run can neither pass
/// without presenting nor exit before the host holds the pixels.
const AUTOLOAD_FIRST_PRESENT_MARKER: &str = tairix_drv_display_framebuffer::FIRST_PRESENT_MESSAGE;

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

const TESTS: &[QemuTest] = &[
    QemuTest {
        package: "tairix-test-memory-isolation",
        binary: "tairix-test-memory-isolation",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
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
    // ramzip b3 (`plans/SWAPSWAPSWAP.md`, `.junie/swapswap-progress.md`):
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
    // `.junie/swapswap-progress.md`): the software-managed Access Flag
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
    // `.junie/swapswap-progress.md`): the software-managed Accessed bit
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
    // Stage 4 deliverable: boot the production kernel pipeline,
    // instantiate `tairix_drvhost::Host`, load a baked-in signed
    // mock `.rxe` image, exercise `load → snapshot → reload →
    // unload`, then flip `qemu_exit::exit_success`. Single CPU
    // suffices and the 60-second budget matches the other Stage 3a
    // boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-drvhost-qemu",
        binary: "tairix-test-drvhost-qemu",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
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
    // Stage 5 end-to-end FAT32 vertical: `tairix-test-fat32-virtio-blk-
    // pci-x86-64` reuses the exact virtio-blk-pci bring-up above, then
    // instead of a raw sector round-trip it mounts the planted FAT32
    // volume through the real FAT32 driver, verifies the planted file,
    // and creates+writes+reads-back a fresh file before `qemu_exit`.
    // The backing image is the shared `tairix-test-fat32-image` FAT32
    // volume (`FsDisk::Fat32`), not the sector-0 pattern, so its geometry
    // is the image's own size. Single CPU and a 60-second budget match
    // the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-fat32-virtio-blk-pci-x86-64",
        binary: "tairix-test-fat32-virtio-blk-pci-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
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
    // Stage 5 end-to-end arxfs vertical: `tairix-test-arxfs-virtio-blk-
    // pci-x86-64` reuses the exact virtio-blk-pci bring-up above, then
    // instead of a raw sector round-trip it mounts the planted arxfs
    // volume through the real arxfs driver, verifies the planted file,
    // and creates+writes+reads-back a fresh file before `qemu_exit`.
    // The backing image is the shared `tairix-test-arxfs-image` arxfs
    // volume (`FsDisk::ARXFS`) — which the driver itself authored — not
    // the sector-0 pattern, so its geometry is the image's own size.
    // Single CPU and a 60-second budget match the FAT32 vertical and the
    // other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-arxfs-virtio-blk-pci-x86-64",
        binary: "tairix-test-arxfs-virtio-blk-pci-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
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
    // `kernel-arch-boot` and the riscv64 `kernel-arch-boot-riscv64`
    // verticals. It enables the stage-1 identity MMU + EL1 vectors,
    // discovers the board from the embedded `virt` device tree (QEMU's
    // aarch64 `-kernel <ELF>` path passes no `x0` DTB pointer), builds the
    // `BootMemoryMap`, installs the discovered-UART console + `svc`
    // dispatch callback, and hands a validated `BootInfo` to
    // `kernel_core::kernel_main`; the audit sink reports PASS through the
    // ARM semihosting finisher — and only with the ramfb framebuffer boot
    // console active: the run attaches `-device ramfb`, so the production
    // pre-MMU video bring-up must discover the tree's `fw_cfg` node,
    // program the scan-out over `lib/fwcfg`, and switch the console to
    // the screen (`video::is_active`), proving the display path `cargo
    // xtask run` relies on end to end. The run is `-smp 4` (matching the
    // embedded tree's `/cpus`): after `EventId(4004)` the sink waits for
    // the production SMP bring-up to PSCI-start all three secondaries and
    // for each to attest `SecondaryCpuOnline` (`EventId(4072)`) from the
    // kernel dispatch loop — the end-to-end multi-core boot proof; a
    // `SecondaryCpuStartFailed` (`EventId(4071)`) is an immediate FAIL.
    // A 60-second budget matches the other boot-then-do-fixed-work tests.
    QemuTest {
        package: "tairix-test-kernel-arch-boot-aarch64",
        binary: "tairix-test-kernel-arch-boot-aarch64",
        target: "aarch64-unknown-none",
        cpus: 4,
        timeout: Duration::from_secs(60),
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
    // PI Design D P-3 (`.junie/next-pi-prompt.md`):
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
    // proving the production LAPIC-timer interrupt preempts a **runaway** ring-3
    // task. Unlike the other ports, the ring-3 transition needs the GDT ring-3
    // selectors, the TSS, and `syscall`/`IA32_LSTAR` entry installed, so the
    // test boots the production `tairix-kernel` pipeline (which also programs
    // the periodic LAPIC timer in `preempt::init_local_preempt`); only the audit
    // sink is replaced. On `BootCompleted` it enables `IA32_EFER.NXE`, builds
    // **one** hardware-isolated ring-3 address space from the pure-Rust
    // `tairix-test-el0-spinner` fixture (a `black_box`-guarded busy loop that
    // issues no syscall, built PIE + converted to `rxe` by `build.rs`) through
    // the capability-checked, audited `kernel_core::spawn_image`, and admits it
    // as a resumable user kthread whose `pre_resume` hook reloads CR3 and
    // repoints **both** the per-CPU `syscall` entry stack
    // (`syscall_entry::set_kernel_rsp0`) and the `TSS.RSP0` trap stack
    // (`percpu::install_tss_rsp0`) at the task's own kernel stack. It then arms
    // the **production** ring-3-preemption path verbatim (the
    // `tairix_arch_x86_64::preempt::set_preempt_callback` surface the bin crate's
    // `install_irq_dispatch` uses): a callback that `reschedule_current(_,
    // Yield)`s the running task. Ring 3 runs preemptible (`userentry`'s `IF`-set
    // `RFLAGS`), so a LAPIC-timer tick taken while the spinner runs lands on the
    // timer ISR and (gated on the saved `CS` RPL) drives the preempt point.
    // Because the loop never traps, the only way it leaves ring 3 before its
    // final `exit` is an involuntary preemption. PASS once the preempt callback
    // fired at least once AND the task — resumed mid-loop after each preemption —
    // still completed and exited; a preemption that never fires (the `step`
    // spins forever inside ring 3) or a botched resume (the task never exits)
    // times out (fail-loud). Single CPU; a 120-second budget
    // covers the multi-tick busy loop under QEMU TCG.
    QemuTest {
        package: "tairix-test-preempt-el0-qemu-x86-64",
        binary: "tairix-test-preempt-el0-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(120),
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
    // (discover the `virt` board, build the live registries, `DeviceManager::
    // autoload` through `SpawnDriverLoader` + `InitCtxDriverProcessSpawn` over
    // `Aarch64ProcessSpawn` image builder), so the driver is admitted Ready with
    // its capability record + address-space-registry entry minted. It then
    // drives the production unload mechanism `InitSpawnCtx::
    // terminate_driver_process` (the seam the driver-store server runs for
    // `StoreRequest::Unload`) and asserts the scheduler task was reaped
    // (live-task count 1→0) and its caps + address space reclaimed, and that a
    // second unload of the now-gone handle fails closed with `NotFound`
    // (idempotent). PASS once teardown reclaimed everything; any shortfall
    // writes a distinct failure finisher or times out (fail-loud). The driver
    // is never dispatched, so it issues no syscall and needs no reply port.
    // Single CPU and a 60-second budget match the driver-spawn vertical.
    QemuTest {
        package: "tairix-test-driver-unload-qemu-aarch64",
        binary: "tairix-test-driver-unload-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
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
    // `.junie/fstree-next-plan.md` S8b): prove the `lib/sandbox` seam end
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
    // vertical above — the first *live-boot* exercise of the x86_64
    // boot-time users-database read path over the virtio-**PCI** bus. It
    // reuses the exact shared virtio-PCI bring-up the `root_unlock_login`
    // /`virtio_blk_pci_x86_64` verticals use (PCI walk to the modern
    // virtio-blk function, `PciTransport` provisioning through the
    // capability-gated `KernelMmioMapper`, MSI-X routing) and then drives
    // the *same* shared `users_db_load` tail the aarch64 vertical runs (one
    // definition, generic over the transport, `AGENTS.md` §2.2) over the
    // same planted users-root arxfs volume (`FsDisk::UsersRoot` — authored
    // by the real arxfs driver): it mounts the plaintext users-root volume,
    // runs `tairix_kernel_core::load_users_db` (/System/Security/Users read
    // off the volume through the capability-checked VFS delegation), and
    // proves the parsed database authenticates the planted account while a
    // wrong password is refused — before the QEMU debug-exit PASS. Unlike the
    // encrypted-root verticals it needs no passphrase (the users-root volume
    // is plaintext), so there is no scripted console dialogue. Single CPU
    // and a 60-second budget match the aarch64 vertical.
    QemuTest {
        package: "tairix-test-users-db-qemu-x86-64",
        binary: "tairix-test-users-db-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
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
    // policy over the virtio-**PCI** bus. It reuses the exact shared
    // virtio-PCI bring-up the `virtio_blk_pci_x86_64` vertical uses (PCI walk
    // to the modern virtio-blk function, `PciTransport` provisioning through
    // the capability-gated `KernelMmioMapper`, MSI-X routing) and then drives
    // the *same* shared `root_unlock_login` tail the aarch64 vertical runs
    // (one definition, generic over the transport, `AGENTS.md` §2.2) over the
    // same planted whole-disk encrypted-root image (`FsDisk::EncryptedRootDisk`
    // — MBR + FAT boot carrying `root.unlock` + a passphrase-derived encrypted
    // ARXFS root): it reads the descriptor off the FAT boot partition, types
    // the fixture passphrase over a scripted console, mounts the encrypted
    // root, installs the loaded users database into a `LateUsersDb` cell, and
    // proves the planted account authenticates through the installed cell
    // while a wrong password is refused — before the QEMU debug-exit PASS.
    // Like the aarch64 vertical this drives the unlock *policy* directly (a
    // scripted console, not the production NULL-console read half), so it is
    // independent of the A2 kthread-admission console work. The `/System`
    // bundles the image plants are cross-compiled for x86_64 (`stores_for`);
    // the root volume uses the format-floor PBKDF2 cost so the per-boot key
    // derivation stays bounded under QEMU TCG; single CPU and a 60-second
    // budget match the aarch64 vertical.
    QemuTest {
        package: "tairix-test-root-unlock-login-qemu-x86-64",
        binary: "tairix-test-root-unlock-login-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(60),
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
    // The secret prompt's two timed-wake behaviours — the `[input
    // active...]` animation advancing on the tickless one-shot, and the
    // anti-brute-force delay park after a wrong attempt expiring on it —
    // are proven deterministically by the `users_db_wait`/`irq_wait`
    // epilogue host unit tests (which pin the one-shot staying armed for a
    // console waiter's deadline across another queue's wait finishing) and
    // the `console` secret-feedback tick tests. This wall-clock-bounded
    // QEMU run therefore does *not* re-assert them: doing so keyed the run
    // on guest-time console delays (a per-second animation tick, then a
    // multi-second wrong-attempt park) that ballooned under parallel TCG
    // saturation and blew the budget — the load-dependent flake the
    // charter forbids papering over with a bigger ceiling. Typing the
    // correct passphrase straight away keeps the vertical's timing bounded
    // by real work (boot + two bounded PBKDF2 derivations), not by
    // guest-time waits.
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
    // `/System/Apps/ps.app/Run` and spawned under `CAP_PROC_SPAWN` — and
    // seeing its process-list header, `man man` rendering the store-shipped
    // Help document end to end (`plans/APPS.md` §7 — resolution, the
    // `fs_*` read of the read-only /System volume, and the `lib/help`
    // render all in one exchange), `ls /System/Apps` listing the system app
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
            // `/System/Apps/man.app/Help/en-US/man.md` off the mounted
            // read-only /System volume through the `fs_*` syscalls, and
            // streams the rendered page (a serial console attests no
            // geometry, so no pager prompt). `SEE ALSO` is the page's final
            // section heading — seeing it proves the whole document arrived.
            ("usage: ps", Duration::ZERO, "man man\n"),
            // `ls /System/Apps` (plans/APPS.md deliverable 6): the spawned
            // tool stats the operand and reads the directory through the
            // `fs_stat`/`fs_readdir` syscalls under its own manifest's
            // `CAP_FS_ACCESS`. `man.app` in the output is an entry only a
            // real directory read of the mounted read-only /System volume
            // produces.
            ("SEE ALSO", Duration::ZERO, "ls /System/Apps\n"),
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
    // `plans/APPS.md` "Immediate work" I2/I3: the memory-stability vertical.
    // `tairix-test-memsoak-qemu-aarch64` boots the *production* aarch64
    // pipeline with the encrypted-root disk that carries the standard
    // signed store bundles **plus** the test-only `memsoak` fixture bundle
    // (`FsDisk::MemsoakRootDisk`), unlocks the root, authenticates
    // `root`/`root` at the console login, and types the bare word `memsoak`
    // at the shell. The fixture warms up, samples
    // `KernelMemoryStats.free_bytes` through sysinfod (its manifest's
    // `CAP_SYSINFO_KERNEL`, enforced against the kernel-attested origin),
    // drives 32 measured cycles — each a spawn+reap of `true.app` (the full
    // teardown path), a timed `stream_read` whose bound elapses (the
    // `top -d0` refresh park), a self-scoped process-list walk, and a live
    // sysinfod IPC round trip — then requires the final sample to equal the
    // baseline **exactly**. On a stable soak it prints `MEMSOAK PASS
    // baseline=… final=…` and exits 0; on any failure it prints the reason
    // and parks forever (it never exits), so the run times out fail-loud
    // with the numbers in the transcript. The guest audit sink arms on the
    // fixture's audited `exit` (`sc=exit`, `comm=memsoak`) and reports PASS
    // on the next audited `exit` — the shell's, typed only after the
    // `MEMSOAK PASS` marker appeared — so the numeric verdict provably
    // reached the transcript before the run ended (the session-ceiling
    // arm-then-exit discipline). A 300-second budget covers boot + bounded
    // PBKDF2 + the 36-cycle soak on QEMU TCG (each cycle is a full
    // spawn/reap plus two sysinfod round trips, on top of the
    // session-ceiling verticals' 120-second boot-and-dialogue baseline);
    // single CPU like the other full-boot verticals.
    QemuTest {
        package: "tairix-test-memsoak-qemu-aarch64",
        binary: "tairix-test-memsoak-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(300),
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
    // `Pressure:` token on the transcript witnesses the gated
    // `MEMORY_PRESSURE` figures rendered on the first frame; `r` then
    // drives an immediate refresh over the raw console, and the
    // `reclaimable` token (the detail-panel header naming the
    // `RECLAIM_STATS` ledger table) witnesses the panel render. `q` quits,
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
            // The first frame's pressure line rendered its gated figures.
            ("Pressure:", Duration::ZERO, "r"),
            // The refresh key was accepted (raw-mode input works); the
            // reclaim ledger panel header rendered.
            ("reclaimable", Duration::ZERO, "q"),
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
    // prompt to accept `sysmon`, observes its `Pressure:` frame, refreshes to
    // the `reclaimable` panel, and quits back to the shell while ten CPU-bound
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
            ("Pressure:", Duration::ZERO, "r"),
            // Raw input and a fresh sysinfo round trip remain live under load.
            ("reclaimable", Duration::ZERO, "q"),
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
    // prompt — `os.loginType` defaults to text, so login drops to the
    // account's shell — and then the `desktop` command word, which the
    // shell resolves in the system app store and spawns: the desktop is
    // started exactly the way a user starts it from the command line.
    //
    // AW3 (`plans/APPWIN.md`) grows the presented desktop into the full
    // click-through: the display service's one-shot `FIRST_PRESENT`
    // witness keys the first screendump (the dark composited desktop) and
    // the whole start-menu → "Files" click sequence (the guest applies
    // injected events strictly in device order, so the menu clicks need no
    // extra gate); the spawned files bundle creates its window over the
    // reserved window rendezvous, and the endpoint's first *reply* on
    // serial gates the in-window click. From there every stage is keyed on
    // the kernel/ipc `MessageDelivered` records the desktop's app-ward
    // event deliveries emit — the shared interaction contract in the test
    // crate's lib target: delivery 2 (Focus + Pressed from the window
    // click) keys the second screendump (the served window on the dark
    // desktop), the reopen-menu + appearance-toggle + window clicks follow,
    // and delivery 4 (a handshake click processed in a wake strictly after
    // the re-themed frame presented) keys the third screendump (the
    // light-theme desktop, window still composited).
    //
    // AW4 then takes the run into the windowed terminal: held behind the
    // verified light dump, the script reopens the menu and clicks its
    // "Terminal" row (spawning the terminal bundle, which spawns the
    // user's shell over pipes and serves its window at the second cascade
    // slot); the window endpoint's fourth reply (the terminal's create +
    // first present) gates the terminal-window click (deliveries 5–7:
    // the files window's unfocus, the terminal's focus, and the press),
    // after which the runner types `true` + Enter at the seat keyboard.
    // The guest PASS gate latches a kernel `ProcessSpawned` record
    // observed once the delivery count has reached the Enter press — the
    // only spawn possible at that point is the shell executing the typed
    // command, so PASS proves the whole keyboard → session → terminal →
    // pipe → shell → spawn round trip and can neither fire under a
    // pending dump nor without the served windows; the runner fails any
    // run whose script or dumps did not complete.
    // Every click coordinate is computed from the production shell's own
    // layout code (`autoload_desktop_pointer_script`), and the pin move
    // also delivers the `kind=pointer` witness, so the pointer decode path
    // stays separately proven. A 240-second budget covers the boot +
    // bounded PBKDF2 + autoload + driver bring-up + the ~4 s passphrase +
    // ~1 s login typing + session bring-up + the paced click script +
    // both app spawns + the typed command on QEMU TCG.
    QemuTest {
        package: "tairix-test-autoload-input-qemu-aarch64",
        binary: "tairix-test-autoload-input-qemu-aarch64",
        target: "aarch64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
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
            // The AW4 terminal stage: once the terminal-window click's
            // deliveries prove the terminal focused, type the shell
            // command — its spawn is the guest PASS gate's round-trip
            // witness.
            (
                AUTOLOAD_WINDOW_EVENT_MARKER,
                AUTOLOAD_TERMINAL_TYPE_OCCURRENCES,
                AUTOLOAD_TERMINAL_COMMAND,
            ),
        ],
        screendumps: &[
            ScreendumpPlan {
                marker: AUTOLOAD_FIRST_PRESENT_MARKER,
                occurrences: 1,
                suffix: "desktop",
                assert: assert_dark_desktop_screendump,
            },
            ScreendumpPlan {
                marker: AUTOLOAD_WINDOW_EVENT_MARKER,
                occurrences: AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
                suffix: "window",
                assert: assert_files_window_dark_screendump,
            },
            ScreendumpPlan {
                marker: AUTOLOAD_WINDOW_EVENT_MARKER,
                occurrences: AUTOLOAD_APPEARANCE_DUMP_OCCURRENCES,
                suffix: "light",
                assert: assert_files_window_light_screendump,
            },
        ],
        pointer_script: Some(autoload_desktop_pointer_script),
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
    // `INBOUND_ECHO_SERVED` — the last gating exit so the guest stays
    // alive until a frame has crossed the two-process boundary and been
    // answered; the peer's own v6 echo verdict is required too, so neither
    // side can pass alone. Booted as a **display world** (`ramfb`): the
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
    // `netstack`'s `DRIVER_BOUND`, and `netstack`'s `INBOUND_ECHO_SERVED` — the
    // last gating exit so the guest stays alive until a frame has crossed the
    // two-process boundary and been answered; the peer's own v6 echo verdict is
    // required too, so neither side can pass alone. Single CPU (PID 1, the
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
    // `INBOUND_ECHO_SERVED` — the last gating exit so the guest stays alive
    // until a frame has crossed the two-process boundary and been answered; the
    // peer's own v6 echo verdict is required too, so neither side can pass
    // alone. Single CPU, with the same 240-second budget as its siblings
    // covering boot + autoload + service bring-up + the bind + the paced echo
    // campaign on QEMU TCG.
    QemuTest {
        package: "tairix-test-netstack-autoload-qemu-x86-64",
        binary: "tairix-test-netstack-autoload-qemu-x86-64",
        target: "x86_64-unknown-none",
        cpus: 1,
        timeout: Duration::from_secs(240),
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
        ctx.run(&label, cmd)?;
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
/// A uniprocessor guest consumes its vCPU plus one unit for QEMU's emulator/I/O
/// work. An SMP TCG guest consumes the entire budget and therefore runs alone:
/// its mutually synchronising vCPU threads need simultaneous host progress,
/// and co-scheduling it with other CPU-bound emulators can starve the whole
/// guest even when the aggregate thread count fits the nominal host capacity.
#[must_use]
pub(crate) fn qemu_job_weight(cpus: u32, host_budget: usize) -> usize {
    let budget = host_budget.max(1);
    if cpus > 1 {
        budget
    } else {
        2.min(budget)
    }
}

/// QEMU matrix capacity derived from effective logical host parallelism.
///
/// TCG vCPU threads are sustained compute workloads, so SMT siblings do not
/// provide independent wall-clock capacity and QEMU's emulator/I/O threads
/// still compete with cargo and the host. Use at most one quarter of reported
/// logical capacity; the weighted runner still lets a heavier guest run alone
/// when its weight exceeds this budget. The additional headroom keeps the
/// four-vCPU stress and migration guests' solo-reachable deadlines reachable
/// when the complete matrix is active.
#[must_use]
pub(crate) fn qemu_host_budget_for(logical_cpus: usize) -> usize {
    logical_cpus.max(1).div_ceil(4)
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
/// weighted-concurrency runner ([`super::parallel`]): one-vCPU guests reserve
/// one emulator/I/O unit beyond their vCPU, while an SMP guest reserves the
/// complete budget and runs alone. The budget is one quarter of the host's
/// effective logical-CPU count, so QEMU's non-vCPU work, cargo, and the host
/// retain capacity without treating SMT siblings as full independent TCG
/// cores.
/// That keeps every guest's wall-clock deadline as reachable as it is for a
/// solo run (no TCG starvation), so co-scheduling does not make a test flaky.
/// On a single-core host the budget collapses to one and the matrix runs
/// strictly sequentially.
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
            Ok(Job::closure(label, weight, move || {
                run_one(&target_dir, t, stores)
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
    pub(crate) fn run(&self, target_dir: &Path, stores: StoreSet) -> Result<(), String> {
        run_one(target_dir, self.test, stores)
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
    super::image_apps::with_plant_refs(apps, |files| {
        tairix_test_encrypted_root_image::build_image_with_apps(files)
    })
    .map_err(|e| {
        format!(
            "test --qemu ({}): build encrypted-root image: {e:?}",
            t.package
        )
    })
}

/// The composed `/System`-store bundle sets one enrolment plants on its
/// backing image, each resolved for the enrolment's own target arch and only
/// when its `fs_disk` actually plants it (empty otherwise, so no arch pays a
/// cross-compile it never uses). Copy `'static` slices, so a job closure
/// captures the set by value without borrowing the build context.
#[derive(Copy, Clone)]
pub(crate) struct StoreSet {
    /// The application/service bundles the encrypted-root plants lay on the
    /// read-only `/System` volume.
    apps: &'static [AppStoreFile],
    /// The memsoak-augmented application set the memory-stability vertical
    /// plants.
    apps_with_memsoak: &'static [AppStoreFile],
    /// The signed autoload driver bundles the `-M virt` autoload verticals
    /// plant in the `/System/Drivers/` store.
    autoload_drivers: &'static [AppStoreFile],
    /// The application/service bundles the stream vertical plants: the shared
    /// set plus the test-only `tcpecho` fixture bundle.
    apps_with_tcpecho: &'static [AppStoreFile],
    /// The application/service bundles the listener vertical plants: the
    /// shared set plus the test-only `tcpserve` server fixture bundle.
    apps_with_tcpserve: &'static [AppStoreFile],
    /// The signed driver bundles the stream/listener verticals plant: the
    /// virtio-net driver alone (no display/input driver, to keep the UART
    /// console the serial script drives).
    net_only_drivers: &'static [AppStoreFile],
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
    let apps = match t.fs_disk {
        FsDisk::EncryptedRootDisk | FsDisk::AutoloadRootDisk => {
            super::image_apps::app_store_files(ctx, arch)?
        }
        _ => EMPTY,
    };
    let apps_with_memsoak = match t.fs_disk {
        FsDisk::MemsoakRootDisk => super::image_apps::memsoak_store_files(ctx, arch)?,
        _ => EMPTY,
    };
    let autoload_drivers = match t.fs_disk {
        FsDisk::AutoloadRootDisk => super::image_drivers::autoload_driver_store_files(ctx, arch)?,
        _ => EMPTY,
    };
    let apps_with_tcpecho = match t.fs_disk {
        FsDisk::StreamRootDisk => super::image_apps::tcpecho_store_files(ctx, arch)?,
        _ => EMPTY,
    };
    let apps_with_tcpserve = match t.fs_disk {
        FsDisk::ListenRootDisk => super::image_apps::tcpserve_store_files(ctx, arch)?,
        _ => EMPTY,
    };
    let net_only_drivers = match t.fs_disk {
        FsDisk::StreamRootDisk | FsDisk::ListenRootDisk => {
            super::image_drivers::net_driver_store_files(ctx, arch)?
        }
        _ => EMPTY,
    };
    Ok(StoreSet {
        apps,
        apps_with_memsoak,
        autoload_drivers,
        apps_with_tcpecho,
        apps_with_tcpserve,
        net_only_drivers,
    })
}

fn run_one(target_dir: &Path, t: &QemuTest, stores: StoreSet) -> Result<(), String> {
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
    // One budget everywhere: the enrolment's own reachable wall-clock ceiling,
    // enforced identically on a developer machine and a CI runner. There is no
    // developer-only clamp — a budget that is reachable running solo but missed
    // under the parallel matrix would be a load-dependent (flaky) timeout, and
    // the charter forbids that. Concurrency, not the budget, is what bounds
    // local run time: the weighted-concurrency runner (`super::parallel`) caps
    // the sum of concurrently-running guest vCPUs at the host's logical-CPU
    // count, so no guest is starved of TCG time and every enrolled budget stays
    // as reachable co-scheduled as it is solo.
    let mut spec = base.with_cpus(t.cpus).with_timeout(t.timeout);

    // Attach a planted raw backing image for storage tests. Sector 0
    // carries the deterministic `byte[i] = i mod 256` pattern the
    // kernel-side test reads back and verifies; every other sector
    // reads as zero, so the test's write+read-back of sector 1 cannot
    // pass on stale data.
    if let Some(sectors) = t.disk_sectors {
        let image = kernel.with_extension("blk.img");
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
    if let Some(fs) = fs_disk_image(
        t,
        stores.apps,
        stores.apps_with_memsoak,
        stores.autoload_drivers,
        stores.apps_with_tcpecho,
        stores.apps_with_tcpserve,
        stores.net_only_drivers,
    )? {
        let image = kernel.with_extension(fs.extension);
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

    finish_run(t, &kernel, spec)
}

/// Decode a dumped scan-out and assert it is dominated by `theme`'s own
/// desktop colour (the compositor's background), whatever the taskbar,
/// cursor, window chrome, or menu overlays. The expected colour is read
/// from `tairix_theme` — the one definition the desktop session itself
/// renders with — never a literal, so a theme change cannot silently
/// diverge the test from the product.
fn assert_desktop_screendump(
    t: &QemuTest,
    path: &Path,
    theme: &tairix_theme::Theme,
) -> Result<(), String> {
    // The desktop background covers everything but the taskbar, cursor,
    // and any window, so a genuinely presented frame is far above this
    // floor; a boot console left on screen (text on its own background)
    // is far below it.
    const MIN_SHARE: f64 = 0.5;
    let image = read_screendump(t, path)?;
    let desktop = theme.palette().desktop;
    let expected = (desktop.r, desktop.g, desktop.b);
    let (dominant, share) = image.dominant_color();
    if dominant != expected || share < MIN_SHARE {
        return Err(format!(
            "test --qemu ({}): screendump {} is not the composited desktop: dominant colour \
             {dominant:?} at share {share:.3} (expected {expected:?} at >= {MIN_SHARE})",
            t.package,
            path.display(),
        ));
    }
    Ok(())
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

/// [`ScreendumpPlan`] assertion: the served files window on the
/// light-theme desktop — taken after the start menu's appearance toggle
/// was clicked, so the dumped frame must render the *other* palette with
/// the window still composited.
fn assert_files_window_light_screendump(t: &QemuTest, path: &Path) -> Result<(), String> {
    assert_files_window_screendump(t, path, &tairix_theme::Theme::light())
}

/// The served files window is on the desktop rendered with `theme`. The
/// theme's desktop colour still dominates the frame (the window is far
/// smaller than the screen), and the region where the session places the
/// first served window — the cascade origin, sized by the files app's
/// own window constants, inset to stay clear of the anti-aliased rounded
/// corners and chrome — is overwhelmingly *not* the desktop colour: a
/// composited window frame covers it.
fn assert_files_window_screendump(
    t: &QemuTest,
    path: &Path,
    theme: &tairix_theme::Theme,
) -> Result<(), String> {
    // Inside the window body — inset from every edge — effectively every
    // pixel belongs to the window's frame; a sliver of tolerance covers
    // the cursor and anti-aliasing if they straddle the inset boundary.
    const MIN_WINDOW_SHARE: f64 = 0.95;
    /// Pixels shaved off each window edge: clear of the rounded-corner
    /// radius and any chrome the compositor draws at the boundary.
    const INSET_PX: u32 = 16;
    assert_desktop_screendump(t, path, theme)?;
    let image = read_screendump(t, path)?;
    let desktop = theme.palette().desktop;
    let background = (desktop.r, desktop.g, desktop.b);
    let origin = tairix_desktop_session::windows::CASCADE_ORIGIN;
    #[allow(clippy::cast_sign_loss)] // The cascade origin is a positive screen offset.
    let (left, top) = (origin as u32 + INSET_PX, origin as u32 + INSET_PX);
    let right = left + tairix_browse::WIN_WIDTH - 2 * INSET_PX;
    let bottom = top + tairix_browse::WIN_HEIGHT - 2 * INSET_PX;
    let mut total = 0u64;
    let mut covered = 0u64;
    for y in top..bottom {
        for x in left..right {
            let pixel = image.pixel(x, y).map_err(|e| {
                format!(
                    "test --qemu ({}): screendump {} lacks the served window region: {e}",
                    t.package,
                    path.display(),
                )
            })?;
            total += 1;
            if pixel != background {
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
            "test --qemu ({}): screendump {} shows no served window at the cascade origin: \
             only {share:.3} of the window body differs from the desktop colour \
             (expected >= {MIN_WINDOW_SHARE})",
            t.package,
            path.display(),
        ));
    }
    Ok(())
}

/// Build the AW3+AW4 desktop click script: pin the pointer to the
/// top-left corner, click the taskbar's start button (the menu opens),
/// click the menu's "Files" row (spawning the file manager), click the
/// served window's body (delivering `Focus` + `Pressed` app-ward — the
/// kernel-attested `MessageDelivered` witnesses the second screendump
/// keys on), reopen the menu and click the appearance-toggle row, click
/// the window once more (the third delivery), and land the handshake
/// click keying the light-theme screendump; then the AW4 terminal stage:
/// reopen the menu, click its "Terminal" row (spawning the terminal
/// bundle), and click the terminal's served window at the second cascade
/// slot — the deliveries the typed shell command keys on. Every
/// coordinate is computed by reconstructing the production desktop shell
/// — the same `TaskbarConfig`, launcher registration, and layout code the
/// guest session runs over the shared ramfb console geometry — so the
/// script and the rendered desktop cannot drift.
///
/// Step gating: the guest processes injected events strictly in device
/// order and the menu model updates synchronously on the press, so the
/// whole start-menu → Files sequence keys on the display service's
/// `FIRST_PRESENT` witness alone (the runner already held it back until
/// the first dump verified). The in-window click waits for the reserved
/// window endpoint's first *reply* (the create round-trip completed, so
/// the window exists in the compositor and was presented by that wake).
/// The reopen/toggle/handshake steps key on the first click's deliveries
/// (and are additionally held while the second dump is pending), the
/// terminal-stage menu steps on the handshake's delivery (held behind the
/// pending light dump), and the terminal-window click on the endpoint's
/// fourth reply (the terminal's create + first present) — so each dump
/// captures exactly the staged frame and every stage is provably
/// established before its step fires.
#[allow(clippy::too_many_lines)] // One linear, ordered click-through script; splitting it would obscure the staging.
fn autoload_desktop_pointer_script() -> Result<Vec<tairix_qemu::PointerStep>, String> {
    use tairix_desktop_session::windows::cascade_origin_for;
    use tairix_desktop_session::{
        DesktopShell, APPEARANCE_LABEL, FILES_LABEL, FILES_LAUNCHER, TERMINAL_LABEL,
        TERMINAL_LAUNCHER, VIEWER_LABEL, VIEWER_LAUNCHER,
    };
    use tairix_geometry::{Point, Rect, Scale};
    use tairix_qemu::{MouseButton, PointerAction, PointerStep};
    use tairix_taskbar::TaskbarConfig;

    let width = tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX;
    let height = tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX;
    let mut shell = DesktopShell::new(TaskbarConfig::bottom_bar(width, height), APPEARANCE_LABEL);
    // Registered in the same order the production session registers them
    // (`userland/gui/session/src/run.rs`), so the reconstructed menu rows
    // sit exactly where the guest draws them.
    let _ = shell
        .session_mut()
        .taskbar_mut()
        .start_menu_mut()
        .add_launcher(FILES_LAUNCHER, FILES_LABEL);
    let _ = shell
        .session_mut()
        .taskbar_mut()
        .start_menu_mut()
        .add_launcher(TERMINAL_LAUNCHER, TERMINAL_LABEL);
    let _ = shell
        .session_mut()
        .taskbar_mut()
        .start_menu_mut()
        .add_launcher(VIEWER_LAUNCHER, VIEWER_LABEL);

    let centre = |rect: Rect, what: &str| -> Result<Point, String> {
        if rect.is_empty() {
            return Err(format!("desktop pointer script: {what} region is empty"));
        }
        #[allow(clippy::cast_possible_wrap)] // Screen extents are far below i32::MAX.
        Ok(Point::new(
            rect.left() + (rect.width / 2) as i32,
            rect.top() + (rect.height / 2) as i32,
        ))
    };
    let taskbar = shell.session().taskbar();
    let start = centre(taskbar.layout(Scale::ONE).start_button, "start button")?;
    let row = |label: &str| -> Result<Point, String> {
        let index = taskbar
            .start_menu()
            .entries()
            .iter()
            .position(|entry| entry.label() == label)
            .ok_or_else(|| format!("desktop pointer script: no menu entry labelled {label:?}"))?;
        let rect = *taskbar
            .menu_layout(Scale::ONE)
            .entries
            .get(index)
            .ok_or_else(|| format!("desktop pointer script: no layout row for {label:?}"))?;
        centre(rect, label)
    };
    let files_row = row(FILES_LABEL)?;
    let terminal_row = row(TERMINAL_LABEL)?;
    let toggle_row = row(APPEARANCE_LABEL)?;
    // The centre of each served window: the session cascades them in
    // open order through the one shared placement rule, each sized by
    // its app's own constants — the same values the dump assertion
    // measures.
    let files_origin = cascade_origin_for(0);
    // The files-window "focus" clicks below target the breadcrumb path bar,
    // not the item area. At the root the path bar's only crumb is the inert
    // current-directory crumb, and the click column sits off it, so the click
    // focuses the window (delivering `Focus` + `Pressed` app-ward) without
    // selecting a listing row or navigating. The files app's frame therefore
    // never changes — its single startup present is the "sole present" the
    // window-reply gate downstream counts on, independent of how many entries
    // the root lists. (A click on a row would select it and repaint — correct
    // app behaviour, but it would add presents the fixed count gate must not
    // see.) A few pixels below the toolbar strip lands squarely in the path
    // bar row (its height is far larger than this inset for the UI font).
    let path_bar_y =
        tairix_browse::render::toolbar_height(shell.session().active_theme()).saturating_add(4);
    #[allow(clippy::cast_possible_wrap)] // Screen extents are far below i32::MAX.
    let window = Point::new(
        files_origin.x + (tairix_browse::WIN_WIDTH / 2) as i32,
        files_origin.y + path_bar_y as i32,
    );
    let terminal_origin = cascade_origin_for(1);
    let terminal_window = centre(
        Rect::new(
            terminal_origin.x,
            terminal_origin.y,
            tairix_terminal::WIN_WIDTH,
            tairix_terminal::WIN_HEIGHT,
        ),
        "terminal window",
    )?;

    // The reserved window endpoint's first reply on serial: the create
    // round-trip completed, so the served window exists in the compositor
    // (and the wake that created it presented the frame carrying it).
    // Built from the kernel/ipc vocabulary and the shared endpoint id +
    // hex renderer, never a literal.
    let mut endpoint_hex = [0u8; 16];
    let created = format!(
        "{} endpoint={}",
        tairix_kernel_ipc::AuditEvent::CallReplied.message(),
        tairix_util::fmt::format_hex_u64(
            tairix_abi::window_ipc::WINDOW_ENDPOINT,
            &mut endpoint_hex
        ),
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
    let press = PointerAction::Press(MouseButton::Primary);
    let release = PointerAction::Release(MouseButton::Primary);
    let step = |marker: &str, occurrences: u32, action: PointerAction| PointerStep {
        ready_marker: marker.to_owned(),
        ready_occurrences: occurrences,
        action,
    };
    Ok(vec![
        // Pin, then click the start button (the menu opens) and the
        // menu's "Files" row (spawns the file manager and closes the
        // menu). This first motion is also the run's `kind=pointer`
        // delivery witness; the row click needs no extra gate — the
        // guest applies the injected events strictly in order and the
        // menu model updates synchronously on the press.
        step(AUTOLOAD_FIRST_PRESENT_MARKER, 1, pin),
        step(
            AUTOLOAD_FIRST_PRESENT_MARKER,
            1,
            move_by(Point::ORIGIN, start),
        ),
        step(AUTOLOAD_FIRST_PRESENT_MARKER, 1, press),
        step(AUTOLOAD_FIRST_PRESENT_MARKER, 1, release),
        step(AUTOLOAD_FIRST_PRESENT_MARKER, 1, move_by(start, files_row)),
        step(AUTOLOAD_FIRST_PRESENT_MARKER, 1, press),
        step(AUTOLOAD_FIRST_PRESENT_MARKER, 1, release),
        // The spawned app's window create has been replied: click the
        // window body — the session delivers `Focus` + `Pressed` app-ward
        // (`MessageDelivered` × 2, the second dump's key).
        step(&created, 1, move_by(files_row, window)),
        step(&created, 1, press),
        step(&created, 1, release),
        // Reopen the menu and click the appearance toggle (the light
        // theme presents), then click the window once more — the third
        // delivery, the light dump's key. These steps additionally wait
        // behind the pending second dump, so it captures the dark frame.
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
            move_by(window, start),
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
            press,
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
            release,
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
            move_by(start, toggle_row),
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
            press,
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
            release,
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
            move_by(toggle_row, window),
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
            press,
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_WINDOW_DUMP_OCCURRENCES,
            release,
        ),
        // The handshake click, keyed on the post-toggle click's own
        // delivery: it is injected only after that delivery appeared on
        // serial, so the guest processes it in a later wake — strictly
        // after the light-theme frame was presented — and its delivery
        // keys the light dump.
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_TOGGLE_CLICK_OCCURRENCES,
            press,
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_TOGGLE_CLICK_OCCURRENCES,
            release,
        ),
        // --- The AW4 terminal stage. Keyed on the handshake click's own
        // delivery (the light dump's key) and additionally held while
        // that dump is pending — the runner holds pointer steps behind
        // unverified dumps — so the terminal never enters the frame the
        // light dump asserts, and the guest (whose PASS needs the typed
        // command's spawn, far below) can never exit under a pending
        // dump. Reopen the menu and click its "Terminal" row, spawning
        // the terminal bundle from the on-disk store.
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_APPEARANCE_DUMP_OCCURRENCES,
            move_by(window, start),
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_APPEARANCE_DUMP_OCCURRENCES,
            press,
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_APPEARANCE_DUMP_OCCURRENCES,
            release,
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_APPEARANCE_DUMP_OCCURRENCES,
            move_by(start, terminal_row),
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_APPEARANCE_DUMP_OCCURRENCES,
            press,
        ),
        step(
            AUTOLOAD_WINDOW_EVENT_MARKER,
            AUTOLOAD_APPEARANCE_DUMP_OCCURRENCES,
            release,
        ),
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
            move_by(terminal_row, terminal_window),
        ),
        step(
            AUTOLOAD_WINDOW_MAP_MARKER,
            AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES,
            press,
        ),
        step(
            AUTOLOAD_WINDOW_MAP_MARKER,
            AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES,
            release,
        ),
    ])
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
fn fs_disk_image(
    t: &QemuTest,
    apps: &[super::image_apps::AppStoreFile],
    apps_with_memsoak: &[super::image_apps::AppStoreFile],
    autoload_drivers: &[super::image_apps::AppStoreFile],
    apps_with_tcpecho: &[super::image_apps::AppStoreFile],
    apps_with_tcpserve: &[super::image_apps::AppStoreFile],
    net_only_drivers: &[super::image_apps::AppStoreFile],
) -> Result<Option<FsImage>, String> {
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
        FsDisk::AutoloadRootDisk => {
            // The whole-disk encrypted-root image with the autoload driver
            // bundles planted in its read-only `/System/Drivers/` store
            // alongside the app/service bundles. The driver bundles are the
            // ones the `image_drivers` pipeline cross-compiled and signed
            // (`autoload_driver_store_files`), each paired with its store
            // path; the generic encrypted-root fixture plants both stores
            // (`AGENTS.md` §2.2 — one whole-disk author, no per-fixture copy).
            let bytes = super::image_apps::with_plant_refs(autoload_drivers, |driver_files| {
                super::image_apps::with_plant_refs(apps, |app_files| {
                    tairix_test_encrypted_root_image::build_image_with_contents(
                        driver_files,
                        app_files,
                        tairix_test_encrypted_root_image::PASSPHRASE,
                    )
                })
            })
            .map_err(|e| {
                format!(
                    "test --qemu ({}): build autoload-root image: {e:?}",
                    t.package
                )
            })?;
            let total_sectors = image_total_sectors(&bytes);
            Some(FsImage {
                extension: "autoload-root.img",
                bytes,
                total_sectors,
            })
        }
        // The encrypted-root layout with the memsoak-augmented bundle set:
        // the same builder, planting the same store plus the one test-only
        // fixture bundle.
        FsDisk::MemsoakRootDisk => {
            let bytes = encrypted_root_disk_bytes(t, apps_with_memsoak)?;
            let total_sectors = image_total_sectors(&bytes);
            Some(FsImage {
                extension: "memsoak-root.img",
                bytes,
                total_sectors,
            })
        }
        // The two-process TCP verticals: the same whole-disk author and the
        // same net-only driver set, differing only in the app set (the
        // `tcpecho` client vs. the `tcpserve` server) — one builder, no copy.
        FsDisk::StreamRootDisk => Some(net_root_image(
            t,
            net_only_drivers,
            apps_with_tcpecho,
            "stream-root.img",
            "stream-root",
        )?),
        FsDisk::ListenRootDisk => Some(net_root_image(
            t,
            net_only_drivers,
            apps_with_tcpserve,
            "listen-root.img",
            "listen-root",
        )?),
    })
}

/// Build the whole-disk encrypted-root image a two-process TCP vertical
/// plants: the shared net-only driver set in `/System/Drivers/` plus the
/// vertical's own app set (the `tcpecho` client or the `tcpserve` server).
/// The one builder both TCP verticals use, never a per-vertical copy;
/// `extension` names the
/// backing file and `label` names the vertical in a build error.
fn net_root_image(
    t: &QemuTest,
    drivers: &[super::image_apps::AppStoreFile],
    apps: &[super::image_apps::AppStoreFile],
    extension: &'static str,
    label: &str,
) -> Result<FsImage, String> {
    let bytes = super::image_apps::with_plant_refs(drivers, |driver_files| {
        super::image_apps::with_plant_refs(apps, |app_files| {
            tairix_test_encrypted_root_image::build_image_with_contents(
                driver_files,
                app_files,
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

/// Attach `t`'s remaining devices (network capture, display, input, the
/// scripted serial dialogue) to `spec` and drive the guest to its outcome.
/// `kernel` is the enrolment's binary path, which names the sibling capture
/// file.
fn finish_run(t: &QemuTest, kernel: &Path, mut spec: Spec) -> Result<(), String> {
    // Attach a virtio-net interface over a `dgram` unix-socket netdev and
    // start the harness-side `netpeer` link peer on its other end, dumping
    // every frame to a `<binary>.pcap` capture beside the kernel image so
    // a failing run leaves the on-wire exchange to inspect. The socket
    // paths live in the temp dir: unix datagram paths are length-bounded
    // (108 bytes) and the target dir can exceed that; the per-binary +
    // per-process name keeps concurrent runs on private wires.
    let mut peer = None;
    if t.netstack_peer != NetPeerMode::None {
        let pcap = kernel.with_extension("pcap");
        let sock_base = std::env::temp_dir().join(format!("{}-{}", t.binary, std::process::id()));
        let qemu_sock = sock_base.with_extension("qemu.sock");
        let peer_sock = sock_base.with_extension("peer.sock");
        let started = match t.netstack_peer {
            // Handled by the guard above; unreachable here.
            NetPeerMode::None => unreachable!("peer mode None is filtered above"),
            NetPeerMode::V6LinkLocal => super::netpeer::NetPeer::spawn(&qemu_sock, &peer_sock),
            NetPeerMode::V6TcpEcho => {
                super::netpeer::NetPeer::spawn_tcp_echo(&qemu_sock, &peer_sock)
            }
            NetPeerMode::V6TcpConnect => {
                super::netpeer::NetPeer::spawn_tcp_connect(&qemu_sock, &peer_sock)
            }
        };
        peer = Some(started.map_err(|e| format!("test --qemu ({}): {e}", t.package))?);
        // The guest derives its link-local from the device MAC, so the MAC
        // is pinned to the wire constant both sides agree on.
        spec = spec.with_virtio_net_dgram_mac(
            &qemu_sock,
            &peer_sock,
            &pcap,
            tairix_test_netstack_wire::GUEST_MAC_STR,
        );
    }

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

    let serial_log = kernel.with_extension("serial.log");
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
    match run? {
        Outcome::Pass => {
            for (path, assert) in &screendump_paths {
                assert(t, path)?;
            }
            if let Some(Err(e)) = peer_verdict {
                return Err(format!("test --qemu ({}): {e}", t.package));
            }
            Ok(())
        }
        Outcome::Fail { status, serial } => {
            persist_failure_serial(t.package, &serial_log, &serial)?;
            Err(format!(
                "test --qemu ({}) FAILED (qemu status {status}; full serial: {})\n--- serial ---\n{serial}\n--- end ---",
                t.package,
                serial_log.display()
            ))
        }
        Outcome::Timeout { budget, serial } => {
            persist_failure_serial(t.package, &serial_log, &serial)?;
            Err(format!(
                "test --qemu ({}) TIMEOUT after {budget:?} (no retries per AGENTS.md §7; full serial: {})\n--- serial ---\n{serial}\n--- end ---",
                t.package,
                serial_log.display()
            ))
        }
    }
}

/// Persist a failed guest's complete serial transcript beside its kernel.
///
/// The command-line report also includes the transcript, but build output can
/// exceed a terminal or CI log's display limit. The sidecar keeps the original
/// bytes available for diagnosis without changing the guest or rerunning a
/// failed workload.
fn persist_failure_serial(package: &str, path: &Path, serial: &str) -> Result<(), String> {
    std::fs::write(path, serial).map_err(|e| {
        format!(
            "test --qemu ({}): persist failure serial {}: {e}",
            package,
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_targets, persist_failure_serial, qemu_host_budget_for, qemu_job_weight, PrimePlan,
        MEMSOAK_PASS_PREFIX, TCPECHO_PASS_PREFIX, TCPSERVE_PASS_PREFIX, TESTS,
    };
    use std::time::Duration;

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
    fn terminal_window_click_gates_on_window_creation_not_repaint_count() {
        // Regression guard for the D10 flaky-repaint deadlock: the
        // terminal-window focus click (the script's final move + press +
        // release) must key on window *creation* — one shared-frame
        // `shm_map` per window — never on the window-endpoint `CallReplied`
        // count. A `CallReplied` gate counts window *presents* too, so a
        // files-window click that happened to repaint would inflate the
        // count and fire this click onto the empty desktop before the
        // terminal window existed, wedging the session (guest goes idle,
        // run times out). Counting creations is immune to repaints.
        let script = super::autoload_desktop_pointer_script().expect("build the pointer script");
        assert!(
            script.len() >= 3,
            "the script ends with the terminal-window click's move/press/release"
        );
        let terminal_click = &script[script.len() - 3..];

        // The present-inclusive marker the fragile gate used, reconstructed
        // exactly as the script builds it, so this test fails if the gate
        // is ever reverted to a `CallReplied`/present count.
        let mut endpoint_hex = [0u8; 16];
        let call_replied = format!(
            "{} endpoint={}",
            tairix_kernel_ipc::AuditEvent::CallReplied.message(),
            tairix_util::fmt::format_hex_u64(
                tairix_abi::window_ipc::WINDOW_ENDPOINT,
                &mut endpoint_hex,
            ),
        );

        for step in terminal_click {
            assert_eq!(
                step.ready_marker,
                super::AUTOLOAD_WINDOW_MAP_MARKER,
                "the terminal-window click must gate on the window-creation \
                 (shm_map) marker, not {:?}",
                step.ready_marker
            );
            assert_eq!(
                step.ready_occurrences,
                super::AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES
            );
            assert_ne!(
                step.ready_marker, call_replied,
                "the terminal-window click must not gate on the \
                 present-inclusive CallReplied count"
            );
        }

        // The creation-based contract: the marker is the shared `sc=<name>`
        // syscall trace, and exactly three frame maps precede the terminal
        // window — the boot framebuffer scan-out, the files window, and the
        // terminal window itself.
        assert_eq!(super::AUTOLOAD_WINDOW_MAP_MARKER, "sc=shm_map");
        assert_eq!(
            super::AUTOLOAD_TERMINAL_WINDOW_MAP_OCCURRENCES,
            tairix_test_autoload_input_qemu_aarch64::TERMINAL_WINDOW_FRAME_MAPS
        );
        assert_eq!(
            tairix_test_autoload_input_qemu_aarch64::TERMINAL_WINDOW_FRAME_MAPS,
            3
        );
    }

    #[test]
    fn failed_guest_serial_is_persisted_verbatim() {
        let path = std::env::temp_dir().join(format!(
            "tairix-xtask-serial-{}-{}.log",
            std::process::id(),
            line!()
        ));
        let transcript = "boot\0serial\nfinal marker\n";
        persist_failure_serial("fixture", &path, transcript).expect("persist transcript");
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
    fn qemu_weight_reserves_emulator_capacity_and_isolates_smp() {
        assert_eq!(
            qemu_job_weight(1, 16),
            2,
            "one-vCPU guest needs process headroom"
        );
        assert_eq!(
            qemu_job_weight(4, 16),
            16,
            "SMP guest must reserve the complete host budget"
        );
        assert_eq!(
            qemu_job_weight(0, 16),
            2,
            "invalid zero still fails safe to one vCPU"
        );
        assert_eq!(qemu_job_weight(1, 1), 1);
        assert_eq!(qemu_job_weight(4, 0), 1);
    }

    #[test]
    fn qemu_budget_reserves_smt_headroom() {
        assert_eq!(qemu_host_budget_for(0), 1);
        assert_eq!(qemu_host_budget_for(1), 1);
        assert_eq!(qemu_host_budget_for(2), 1);
        assert_eq!(qemu_host_budget_for(4), 1);
        assert_eq!(qemu_host_budget_for(8), 2);
        assert_eq!(qemu_host_budget_for(9), 3);
        assert_eq!(qemu_host_budget_for(64), 16);
    }

    /// The smallest wall-clock budget any enrolment may carry.
    ///
    /// Every enrolled QEMU test is a boot-then-do-fixed-work vertical whose
    /// budget is sized to be reachable when the guest runs co-scheduled with
    /// the rest of the matrix (the weighted-concurrency runner gives SMP
    /// guests exclusive admission), not merely when it runs solo. This
    /// floor is the reachable minimum the guard below enforces; the runner
    /// applies each enrolment's own [`super::QemuTest::timeout`] verbatim on
    /// both a developer machine and a CI runner, with no split that could
    /// shorten it.
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

    /// Regression guard for the removed developer-only timeout clamp. Every
    /// enrolment must carry a budget at least [`MIN_REACHABLE_BUDGET`], and
    /// that budget is what the runner enforces verbatim — there is no
    /// developer-vs-CI split that could shorten it. A previous 30 s
    /// developer cap halved these budgets locally and turned a guest that was
    /// merely slow under the parallel matrix into a load-dependent (flaky)
    /// timeout; nothing may re-introduce a budget, or a clamp, below this
    /// floor.
    #[test]
    fn every_enrolment_budget_is_at_least_the_reachable_floor() {
        for t in TESTS {
            assert!(
                t.timeout >= MIN_REACHABLE_BUDGET,
                "enrolment {} budget {:?} is below the reachable floor {:?}; a \
                 budget reachable solo but missed under load is a flaky timeout",
                t.package,
                t.timeout,
                MIN_REACHABLE_BUDGET,
            );
        }
    }
}
