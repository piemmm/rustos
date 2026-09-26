//! The `settings.app` bundle's `Run` entry point: the windowed Settings
//! application (`plans/NEW-DESKTOP-SETTINGS.md`).
//!
//! # It browses pictures it may not read
//!
//! The Wallpaper pane offers the shipped store, and this application holds
//! no filesystem capability and no sandbox to decode a picture with. Both
//! are *served*: the desktop session lists the store once and answers a
//! catalog page, and renders one candidate at a time into a shared-memory
//! region this program created and granted — which is the one thing its
//! `CAP_SHM` already allows. So there is one sandboxed decode path on the
//! desktop rather than two, and no untrusted picture is ever decoded in the
//! address space of the application that browses them.
//!
//! Everything with behaviour worth testing lives in the host-tested shell
//! (`tairix_settings`); this binary only composes it over the live window
//! channel, exactly as `userland/apps/widgets` composes its gallery:
//!
//! * one `shm_create`d frame region granted to the reserved window endpoint;
//! * one `port_bind`-bound event mailbox the app **parks** on through its
//!   wait-set — never a poll loop — accepting only events whose
//!   kernel-attested sender is the desktop session the create reply named;
//! * the `WindowClient` calls and the `WindowEvents` typed wait over it.
//!
//! The window is resizable: a `Resized` event re-maps the frame region and
//! the shell lays itself out to the new client, shedding the strip where it
//! no longer fits. Every bring-up refusal exits fail-loud with a reserved
//! code and a stated reason on `stderr`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    extern crate alloc;

    use alloc::string::String;
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::cell::Cell;

    use tairix_abi::driver::display::{DamageRect, DisplayMode};
    use tairix_abi::elevate::{ElevateArgv, ElevateReply, ElevateRequest, ELEVATE_MAX_REPLY};
    use tairix_abi::input::KeyInput;
    use tairix_abi::latency::DEFAULT_FRAME_BUDGET_NS;
    use tairix_abi::net_ipc::{NetServerAddr, MAX_RESOLVER_SERVERS};
    use tairix_abi::pinboard_ipc::PinboardDocument;
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_abi::sysinfo::{SysinfoQueryId, SystemIdentity, Uptime};
    use tairix_abi::window_ipc::{PointerAction, WindowEvent};
    use tairix_abi::{Errno, ProcId, WaitSetOp, WaitSourceKind};
    use tairix_appdata::RtHost;
    use tairix_geometry::{Point, Rect, Region, Scale};
    use tairix_icon::{
        artwork_cache, ArtworkCache, IconArtworkSource, InlineArtwork, NoArtworkSeam,
    };
    use tairix_input::InputEvent;
    use tairix_procinfo::{for_each_mount, IpcTransport, WalkStep};
    use tairix_rt::io::{Stderr, Write};
    use tairix_settings::{
        win_sizing, AccountFacts, ElevateRefusal, Elevated, Elevation, MachineFacts, OwnAccount,
        Pane, Roster, RunMode, Shell, ShellOutcome, VolumeReading, WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_sysconfig::SystemConfig;
    use tairix_theme::{CursorSetId, Theme, ThemeRegistry};
    use tairix_users::{Salt, SALT_LEN};
    use tairix_wallpaper::{ApplyOutcome, CatalogItem, DesktopSettings, PINBOARD_PUBLISHER};
    use tairix_window::app::{self, AppWindow, ShellError, Wake, EXIT_CHANNEL_LOST};
    use tairix_window::{
        key_input_event, pointer_input_events, pointer_point, present_damage, Desktop, EventDrain,
        EventError, EventMailbox, EventSource, Parked, Repaint, Target, WindowEvents,
    };

    /// The wait-set token of the applier's wake pipe: readable exactly when
    /// the desktop session has answered an apply, so the rows are brought up
    /// to date through the park the loop is already in rather than by
    /// waiting for it.
    const APPLY_TOKEN: u64 = app::FIRST_APP_TOKEN;

    /// The wait-set token of the mount walk's wake: readable exactly when a
    /// fresh mount table has landed, so the Storage pane is brought up to
    /// date through the park the loop is already in.
    ///
    /// Its own desk rather than the applier's: the two carry different work
    /// and a shared latest-wins desk would let one evict the other.
    const MOUNTS_TOKEN: u64 = app::FIRST_APP_TOKEN + 1;

    /// The wait-set token of the machine-reading desk's wake: readable
    /// exactly when a fresh reading of the machine's configuration and
    /// facts has landed.
    const MACHINE_TOKEN: u64 = app::FIRST_APP_TOKEN + 2;

    /// The wait-set token of the elevated-run desk's wake: readable exactly
    /// when the console's broker has answered an offered account.
    ///
    /// Its own desk because it is the one round trip that blocks for as
    /// long as another program takes to run — a window that waited on it
    /// would stop answering for the whole of an authentication and a store
    /// write.
    const ELEVATE_TOKEN: u64 = app::FIRST_APP_TOKEN + 3;

    /// The wait-set token of the network-reading desk's wake: readable
    /// exactly when a fresh reading of the stack's resolver set has landed.
    ///
    /// Its own desk rather than the machine desk's: the two are wanted by
    /// different panes, and folding them together would spend a network
    /// round trip every time the About pane came on show.
    const NETWORK_TOKEN: u64 = app::FIRST_APP_TOKEN + 4;

    /// The wait-set token of the account-reading desk's wake: readable
    /// exactly when a fresh reading of the caller's own record, the two
    /// public directories and a salt has landed.
    ///
    /// Its own desk for the same reason the network one is: it is wanted by
    /// one pane, and folding it into another's would spend three round
    /// trips every time that other pane came on show.
    const ACCOUNTS_TOKEN: u64 = app::FIRST_APP_TOKEN + 5;

    /// The desktop settings in effect for the launching user, so every
    /// composed row opens on what the desktop is actually drawn with.
    ///
    /// Read from the desktop session's **published** app-data scope, which
    /// is the sanctioned channel one application reaches another's values
    /// through: this program names the publisher and nothing else, so the
    /// request shape it sends cannot ask for the session's private
    /// settings. It never writes them — an application publishes only its
    /// own scope — so a change is a request the session decides on.
    ///
    /// A desktop that has published **nothing** means the documented
    /// defaults and is not an error: a fresh account has never applied a
    /// setting. Anything else that stops the document being used says so on
    /// `stderr` rather than showing values the user cannot account for.
    fn settings_in_effect() -> DesktopSettings {
        let document = match tairix_appdata::read_published(&mut RtHost, PINBOARD_PUBLISHER) {
            Ok(document) => document,
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "settings: the desktop's settings could not be read ({err:?}); showing the \
                     defaults"
                );
                return DesktopSettings::default();
            }
        };
        let (settings, refused) = DesktopSettings::load(&document);
        for key in refused {
            let _ = writeln!(
                Stderr,
                "settings: the desktop publishes a `{key}` this build does not accept; showing \
                 its default"
            );
        }
        settings
    }

    /// The applier: the session round trip an apply costs, carried out on a
    /// worker thread.
    ///
    /// The session answers only once its own publisher has written the
    /// store, so making the choice wait for it would freeze this window for
    /// a disk commit — and freeze it again on every further choice. The loop
    /// encodes the document (in memory, and refusable on the spot), submits,
    /// and adopts the answer on the wake it nudges.
    type Applier = tairix_rt::work::Worker<(), PinboardDocument, ApplyOutcome>;

    /// The worker's body: the shared apply client's one round trip.
    fn send_apply(_: &mut (), document: &mut PinboardDocument) -> ApplyOutcome {
        tairix_wallpaper::apply(*document)
    }

    /// The mount walk: the system mount table, read through the one shared
    /// client every surface reads it through.
    ///
    /// Carried out on a worker thread because it is a paged IPC round trip,
    /// and a window that waited on it would stop answering for as long as
    /// the service took. A walk that fails part way keeps what arrived —
    /// listing the volumes it did learn about is better than listing none —
    /// and states the reason rather than showing a shorter table silently.
    type Mounts = tairix_rt::work::Worker<(), (), Vec<VolumeReading>>;

    /// The machine readings: the boot-time configuration store, and the
    /// facts the About and Date & Time panes state.
    ///
    /// One desk for all of them because they are one refresh: a pane that
    /// comes on show wants the reading that backs it, and the queries are
    /// each a single ungated round trip. A reading that is refused is left
    /// absent, so its row states that rather than showing a value the
    /// reader could not account for.
    type Machine = tairix_rt::work::Worker<(), (), (Option<SystemConfig>, MachineFacts)>;

    /// The machine readings' body.
    fn read_machine(_: &mut (), (): &mut ()) -> (Option<SystemConfig>, MachineFacts) {
        let config = match tairix_procinfo::system_config(&IpcTransport) {
            Ok(config) => config.or_else(|| Some(SystemConfig::default())),
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "settings: the machine's configuration could not be read ({err:?}); its rows \
                     show no value"
                );
                None
            }
        };
        (config, machine_facts())
    }

    /// The network readings: the resolver set the stack is actually using.
    ///
    /// Carried out on a worker thread because it is a paged IPC round trip
    /// like the mount walk, and a window that waited on it would stop
    /// answering for as long as the service took.
    type Network = tairix_rt::work::Worker<(), (), Option<Vec<NetServerAddr>>>;

    /// The network readings' body.
    ///
    /// A refused or undecodable walk leaves the set absent rather than
    /// empty: "no server answered the question" and "the stack holds no
    /// server" are different facts, and the pane states which it has.
    fn read_network(_: &mut (), (): &mut ()) -> Option<Vec<NetServerAddr>> {
        let mut resolvers = Vec::with_capacity(MAX_RESOLVER_SERVERS);
        match tairix_procinfo::for_each_resolver_server(&IpcTransport, |record| {
            resolvers.push(*record);
            // The stack bounds its own set, so a reply that keeps going
            // past the bound is a service this window will not grow a heap
            // for. Stopping is an ordinary success, not a failure.
            if resolvers.len() >= MAX_RESOLVER_SERVERS {
                return Ok(tairix_procinfo::WalkStep::Stop);
            }
            Ok(tairix_procinfo::WalkStep::Continue)
        }) {
            Ok(()) => Some(resolvers),
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "settings: the name servers could not be read ({err:?}); the pane says so"
                );
                None
            }
        }
    }

    /// The account readings: the caller's own record, the two ungated
    /// directories, and a fresh salt for hashing a password with.
    ///
    /// One worker job rather than four round trips from the loop, for the
    /// same reason the machine desk bundles its readings: the pane wants
    /// them together and a window that waited on any of them would stop
    /// answering.
    type Accounts = tairix_rt::work::Worker<(), (), (AccountFacts, Option<Salt>)>;

    /// The account readings' body.
    ///
    /// Each directory is absent rather than empty when its walk failed:
    /// "the directory could not be read" and "the machine holds no
    /// account" are different facts, and the pane states which it has. The
    /// listing half is left `Unasked` — only an authenticated run answers
    /// it, and this desk holds no authority at all.
    fn read_accounts(_: &mut (), (): &mut ()) -> (AccountFacts, Option<Salt>) {
        let facts = AccountFacts {
            own: own_account(),
            users: directory(|sink| {
                tairix_procinfo::for_each_user(&IpcTransport, |record| {
                    sink((
                        record.uid,
                        tairix_procinfo::field_lossy(record.name_bytes()),
                    ));
                    Ok(tairix_procinfo::WalkStep::Continue)
                })
            }),
            groups: directory(|sink| {
                tairix_procinfo::for_each_group(&IpcTransport, |record| {
                    sink((
                        record.gid,
                        tairix_procinfo::field_lossy(record.name_bytes()),
                    ));
                    Ok(tairix_procinfo::WalkStep::Continue)
                })
            }),
            roster: Roster::Unasked,
        };
        (facts, draw_salt())
    }

    /// The caller's own account record, read ungated against the uid the
    /// kernel attested.
    ///
    /// Three answers, told apart: a record, a uid no database holds, and a
    /// read that could not be taken. A display that collapsed the last two
    /// would report the machine's state for its own failure.
    fn own_account() -> OwnAccount {
        match tairix_procinfo::self_account(&IpcTransport) {
            Ok(Some(record)) => OwnAccount::Known(alloc::boxed::Box::new(record)),
            Ok(None) => OwnAccount::Unknown,
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "settings: this session's own account could not be read ({err:?}); the pane                      says so"
                );
                OwnAccount::Unmeasured
            }
        }
    }

    /// Collect one id-and-name directory, or `None` where the walk failed.
    fn directory(
        walk: impl FnOnce(&mut dyn FnMut((u32, String))) -> Result<(), tairix_procinfo::ListError>,
    ) -> Option<Vec<(u32, String)>> {
        let mut listed = Vec::new();
        let outcome = walk(&mut |entry| listed.push(entry));
        match outcome {
            Ok(()) => Some(listed),
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "settings: a directory could not be read ({err:?}); the pane says so"
                );
                None
            }
        }
    }

    /// One fresh salt from the kernel CSPRNG through the unprivileged
    /// `sys:random` resource.
    ///
    /// Refuses, never guesses: a failed draw leaves the pane with no salt,
    /// which refuses a password apply rather than hashing under something
    /// predictable.
    fn draw_salt() -> Option<Salt> {
        let fd = u32::try_from(tairix_rt::resource_open(
            b"sys:random",
            tairix_abi::OpenFlags::READ,
        ))
        .ok()?;
        let mut salt = [0u8; SALT_LEN];
        let outcome = tairix_rt::fs_read(fd, 0, &mut salt);
        let _ = tairix_rt::fs_close(fd);
        match outcome {
            Ok(read) if read == SALT_LEN => Some(salt),
            _ => {
                let _ = writeln!(
                    Stderr,
                    "settings: no randomness was available; a new password cannot be hashed here"
                );
                None
            }
        }
    }

    /// The machine facts the About and Date & Time panes state, each taken
    /// on its own so one refusal costs one reading rather than all of them.
    fn machine_facts() -> MachineFacts {
        MachineFacts {
            identity: read_scalar(SysinfoQueryId::SYSTEM_IDENTITY)
                .and_then(|reply| SystemIdentity::from_bytes(&reply).ok()),
            uptime: read_scalar(SysinfoQueryId::UPTIME)
                .and_then(|reply| Uptime::from_bytes(&reply).ok()),
            cpus: tairix_procinfo::cpu_info(&IpcTransport).unwrap_or_default(),
            memory_bytes: tairix_procinfo::memory_total_bytes(&IpcTransport).ok(),
            clock: wall_clock(),
        }
    }

    /// One scalar System Information API reading.
    fn read_scalar(query: SysinfoQueryId) -> Option<Vec<u8>> {
        tairix_procinfo::call(&IpcTransport, query, &[]).ok()
    }

    /// The machine's wall clock, or `None` where it could not be read.
    fn wall_clock() -> Option<tairix_abi::time::WallClockReading> {
        tairix_rt::wall_time().ok()
    }

    /// The elevated run: the console broker's round trip, carried out on a
    /// worker thread.
    ///
    /// The broker re-authenticates the offered account, runs the program as
    /// it, and answers only once that program has exited — which is exactly
    /// as long as an authentication and a store write take. The loop
    /// submits and collects the answer on the wake it nudges, so the window
    /// keeps drawing throughout.
    type Elevator = tairix_rt::work::Worker<(), Elevation, Elevated>;

    /// The elevated run's body: one posted request, one verdict.
    ///
    /// The offered password is wiped as soon as the exchange resolves,
    /// whichever way it went: the request it was encoded into is erased by
    /// the runtime's own wiped buffer, and the copy this desk was handed is
    /// erased here.
    fn send_elevate(_: &mut (), asked: &mut Elevation) -> Elevated {
        let argv: Vec<&str> = asked.argv.iter().map(String::as_str).collect();
        let verdict = elevate_once(asked, &argv);
        // Before the desk's own drop, because it keeps the job it was
        // handed until the next one replaces it.
        asked.erase();
        verdict
    }

    /// Post one elevation request and turn the reply into a verdict.
    fn elevate_once(asked: &Elevation, argv: &[&str]) -> Elevated {
        // A secret that is not text was never one the broker could check,
        // so it is refused here rather than put on the wire.
        let Ok(password) = core::str::from_utf8(&asked.password) else {
            return Elevated::Refused(ElevateRefusal::Credentials);
        };
        let Ok(carried) = ElevateArgv::new(argv) else {
            return Elevated::Refused(ElevateRefusal::NotRun(String::from(
                "That is more than one command can carry.",
            )));
        };
        let request = match asked.mode {
            RunMode::Wait => ElevateRequest::Run {
                username: &asked.account,
                password,
                program: asked.program,
                argv: carried,
            },
            RunMode::Capture => ElevateRequest::Capture {
                username: &asked.account,
                password,
                program: asked.program,
                argv: carried,
            },
            RunMode::Leave => ElevateRequest::Launch {
                username: &asked.account,
                password,
                program: asked.program,
            },
        };
        let mut reply = [0u8; ELEVATE_MAX_REPLY];
        match tairix_rt::elevate(&request, &mut reply) {
            Ok(ElevateReply::Completed { exit_code }) => Elevated::Finished(exit_code),
            Ok(ElevateReply::Captured { exit_code, output }) => {
                Elevated::Printed(exit_code, output.to_vec())
            }
            Ok(ElevateReply::Overran { .. }) => Elevated::Overran,
            // A launch is started and left running; there is no exit code
            // to wait for and none is invented.
            Ok(ElevateReply::Launched { .. }) => Elevated::Finished(0),
            Ok(ElevateReply::Refused(Errno::PermissionDenied)) => {
                Elevated::Refused(ElevateRefusal::Credentials)
            }
            Ok(ElevateReply::Refused(err)) => Elevated::Refused(ElevateRefusal::NotRun(
                alloc::format!("The account was accepted, but nothing ran ({err})."),
            )),
            // A reply to a request this program did not send: nothing ran
            // on an answer it does not understand.
            Ok(ElevateReply::Verified) | Err(_) => Elevated::Refused(ElevateRefusal::NotRun(
                String::from("This console has no way to ask for an account."),
            )),
        }
    }

    /// The mount walk's body.
    fn read_mounts(_: &mut (), (): &mut ()) -> Vec<VolumeReading> {
        let mut volumes = Vec::new();
        if let Err(err) = for_each_mount(&IpcTransport, |record| {
            volumes.push(VolumeReading::of(record));
            Ok(WalkStep::Continue)
        }) {
            let _ = writeln!(
                Stderr,
                "settings: the mount table could not be read ({err:?}); the storage pane shows \
                 what arrived"
            );
        }
        volumes
    }

    /// The mount walk's client half: the desk it is submitted to, and
    /// whether one is outstanding.
    ///
    /// One walk at a time, because the pane only ever wants the latest
    /// answer and re-asking while one is in flight would spend a round trip
    /// on a table it is about to be told anyway.
    struct MountWalk<'a> {
        worker: &'a Mounts,
        pending: bool,
    }

    impl MountWalk<'_> {
        /// Ask for a fresh mount table if the pane wants one and none is
        /// outstanding, answering whether the pane changed.
        ///
        /// Submitted, never awaited: the answer arrives as an ordinary wake.
        fn request(&mut self, shell: &mut Shell) -> bool {
            if self.pending || !shell.volumes_wanted() {
                return false;
            }
            // With no worker the walk was made on this thread and its
            // answer is already on the desk.
            if self.worker.submit(()) {
                return self.settle(shell);
            }
            self.pending = true;
            false
        }

        /// Adopt a landed mount table, answering whether the pane changed.
        fn settle(&mut self, shell: &mut Shell) -> bool {
            let Some(volumes) = self.worker.collect() else {
                return false;
            };
            self.pending = false;
            shell.adopt_volumes(volumes);
            true
        }
    }

    /// The machine-reading desk's client half.
    ///
    /// One reading at a time, because a pane only ever wants the latest
    /// and re-asking while one is in flight would spend a round trip on
    /// readings it is about to be told anyway.
    struct MachineRead<'a> {
        worker: &'a Machine,
        pending: bool,
    }

    impl MachineRead<'_> {
        /// Ask for a fresh reading if a pane wants one and none is
        /// outstanding, answering whether anything on screen changed.
        fn request(&mut self, shell: &mut Shell) -> bool {
            if self.pending || !shell.config_wanted() {
                return false;
            }
            if self.worker.submit(()) {
                return self.settle(shell);
            }
            self.pending = true;
            false
        }

        /// Adopt a landed reading, answering whether anything changed.
        fn settle(&mut self, shell: &mut Shell) -> bool {
            let Some((config, facts)) = self.worker.collect() else {
                return false;
            };
            self.pending = false;
            shell.adopt_config(config);
            shell.adopt_machine(facts);
            true
        }
    }

    /// The network-reading desk's client half.
    ///
    /// One reading at a time, for the same reason the machine desk holds
    /// one: a pane only ever wants the latest.
    struct NetworkRead<'a> {
        worker: &'a Network,
        pending: bool,
    }

    impl NetworkRead<'_> {
        /// Ask for a fresh reading if a pane wants one and none is
        /// outstanding, answering whether anything on screen changed.
        fn request(&mut self, shell: &mut Shell) -> bool {
            if self.pending || !shell.network_wanted() {
                return false;
            }
            if self.worker.submit(()) {
                return self.settle(shell);
            }
            self.pending = true;
            false
        }

        /// Adopt a landed reading, answering whether anything changed.
        fn settle(&mut self, shell: &mut Shell) -> bool {
            let Some(facts) = self.worker.collect() else {
                return false;
            };
            self.pending = false;
            shell.adopt_resolvers(facts);
            true
        }
    }

    /// The account-reading desk's client half.
    ///
    /// One reading at a time, for the same reason every other desk holds
    /// one: the pane only ever wants the latest.
    struct AccountRead<'a> {
        worker: &'a Accounts,
        pending: bool,
    }

    impl AccountRead<'_> {
        /// Ask for a fresh reading if the pane wants one, or wants a salt
        /// it has spent, and none is outstanding.
        fn request(&mut self, shell: &mut Shell) -> bool {
            if self.pending || !(shell.accounts_wanted() || shell.salt_wanted()) {
                return false;
            }
            if self.worker.submit(()) {
                return self.settle(shell);
            }
            self.pending = true;
            false
        }

        /// Adopt a landed reading, answering whether anything changed.
        fn settle(&mut self, shell: &mut Shell) -> bool {
            let Some((facts, salt)) = self.worker.collect() else {
                return false;
            };
            self.pending = false;
            shell.adopt_accounts(facts);
            shell.adopt_salt(salt);
            true
        }
    }

    /// Ask the desktop session to adopt `document`, off the event loop.
    ///
    /// A document this program cannot even encode is refused here, where it
    /// costs nothing; everything else goes to the worker and is answered on
    /// a later wake.
    fn submit_apply(applier: &Applier, document: &str) -> Option<ApplyOutcome> {
        match PinboardDocument::new(document) {
            // With no worker the call was made on this thread and its answer
            // is already on the desk.
            Ok(document) if applier.submit(document) => applier.collect(),
            Ok(_) => None,
            Err(_) => Some(ApplyOutcome::Refused(String::from(
                "settings document out of range",
            ))),
        }
    }

    /// Ask the console's broker to run what an offered account authorises,
    /// off the event loop.
    fn submit_elevate(elevator: &Elevator, asked: Elevation) -> Option<Elevated> {
        // With no worker the call was made on this thread and its answer is
        // already on the desk.
        if elevator.submit(asked) {
            return elevator.collect();
        }
        None
    }

    /// Adopt what the desktop answered: state a refusal, then re-read what
    /// the desktop actually holds into `shell`.
    ///
    /// Persist-then-adopt. The rows showed the reader's choice at once; the
    /// durable value is whatever the session answers with, so a refusal puts
    /// the row back rather than leaving a value on screen the next login
    /// would not restore.
    fn adopt_apply(shell: &mut Shell, outcome: ApplyOutcome) {
        match outcome {
            ApplyOutcome::Applied | ApplyOutcome::Applying => {}
            ApplyOutcome::Refused(reason) => {
                let _ = writeln!(Stderr, "settings: the desktop refused the change: {reason}");
            }
            ApplyOutcome::NoDesktop => {
                let _ = writeln!(
                    Stderr,
                    "settings: no desktop session answered; nothing was changed"
                );
            }
        }
        shell.adopt_settings(settings_in_effect());
    }

    /// The shipped picture catalog the desktop offers, read once.
    ///
    /// The store is on the read-only `/System` volume, so the session
    /// listed it at bring-up and answers from memory: this costs a round
    /// trip per page and no I/O either side, which is why it is done here
    /// rather than deferred. A page that cannot be read leaves the gallery
    /// with whatever arrived — an empty gallery still offers "no picture"
    /// — and states the reason once.
    fn fetch_catalog(
        client: &mut tairix_window::WindowClient<app::RtWindowTransport>,
    ) -> Vec<CatalogItem> {
        let mut page = [0u8; tairix_abi::window_ipc::WINDOW_WALLPAPERS_REPLY_MAX];
        let mut catalog: Vec<CatalogItem> = Vec::new();
        loop {
            let Ok(from) = u16::try_from(catalog.len()) else {
                return catalog;
            };
            let answered = match client.wallpapers(from, &mut page) {
                Ok(answered) => answered,
                Err(err) => {
                    let _ = writeln!(
                        Stderr,
                        "settings: the desktop's picture catalog could not be read ({err}); the \
                         gallery shows what arrived"
                    );
                    return catalog;
                }
            };
            let total = answered.total;
            // An empty page with entries still outstanding is a session
            // that cannot answer them, not a queue to keep asking: stop
            // rather than turn the read into a spin.
            if answered.is_empty() {
                return catalog;
            }
            for entry in answered.entries() {
                let (Ok(category), Ok(file)) = (
                    core::str::from_utf8(entry.category),
                    core::str::from_utf8(entry.file),
                ) else {
                    continue;
                };
                catalog.push(CatalogItem {
                    category: String::from(category),
                    file: String::from(file),
                });
            }
            if catalog.len() >= usize::from(total) {
                return catalog;
            }
        }
    }

    /// The cursor sets the desktop offers, read once.
    ///
    /// The store is on the read-only `/System` volume, so the session
    /// listed it at bring-up and answers from memory: this costs one round
    /// trip and no I/O either side. An answer that cannot be read leaves
    /// the pointer-set row offering the built-in set alone — which is
    /// honest, since that is the one set every desktop has — and states
    /// the reason once.
    fn fetch_cursor_sets(
        client: &mut tairix_window::WindowClient<app::RtWindowTransport>,
    ) -> Vec<CursorSetId> {
        let mut frame = [0u8; tairix_abi::window_ipc::WINDOW_CURSOR_SETS_REPLY_MAX];
        let answered = match client.cursor_sets(&mut frame) {
            Ok(answered) => answered,
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "settings: the desktop's cursor sets could not be read ({err}); the \
                     pointer row offers the built-in set alone"
                );
                return Vec::new();
            }
        };
        // A name this build would not accept as a set is dropped rather
        // than offered: choosing it would post a document the desktop
        // refuses whole.
        answered
            .names()
            .filter_map(|name| core::str::from_utf8(name).ok())
            .filter_map(CursorSetId::new)
            .collect()
    }

    /// The picture gallery's client half: the region the desktop renders
    /// into, and which render is outstanding.
    ///
    /// One region, created once at the side the tiles are drawn at and
    /// re-created when that side moves, because a grant is cheap only if it
    /// is not taken per tile. One render outstanding, because the desktop
    /// serves one at a time.
    struct Pictures {
        region: Option<tairix_rt::shm::SharedRegion>,
        /// The square side `region` was sized for.
        side: u16,
        /// The catalog position awaiting its answer.
        pending: Option<u16>,
    }

    impl Pictures {
        const fn new() -> Self {
            Self {
                region: None,
                side: 0,
                pending: None,
            }
        }

        /// Ask the desktop for the next picture the shell wants, if any.
        ///
        /// Requested, never awaited: the answer arrives as an ordinary
        /// window event. A refusal is stated once and the tile is marked
        /// refused, so a picture the desktop will not render never becomes
        /// a request loop.
        fn request(
            &mut self,
            shell: &mut Shell,
            surface: &mut SettingsWindow,
            theme: &Theme,
            scale: Scale,
        ) {
            if self.pending.is_some() {
                return;
            }
            let viewport = surface.viewport();
            let Some(wanted) = shell.next_picture_wanted(viewport, scale, theme) else {
                return;
            };
            let Some(window_id) = surface.window.window_id() else {
                return;
            };
            let Some(grant) = self.grant(wanted.side) else {
                let _ = writeln!(
                    Stderr,
                    "settings: no shared region for a picture preview; the gallery shows its \
                     placeholders"
                );
                shell.mark_picture_refused(wanted.index);
                return;
            };
            match surface.window.client().render_wallpaper(
                window_id,
                grant,
                wanted.index,
                wanted.side,
            ) {
                Ok(()) => self.pending = Some(wanted.index),
                Err(err) => {
                    let _ = writeln!(
                        Stderr,
                        "settings: the desktop refused a picture preview ({err}); it is not \
                         shown"
                    );
                    shell.mark_picture_refused(wanted.index);
                }
            }
        }

        /// The grant handle of a region big enough for a `side` square,
        /// creating one when the side has moved.
        fn grant(&mut self, side: u16) -> Option<u64> {
            let want = usize::from(side)
                .checked_mul(usize::from(side))?
                .checked_mul(4)?;
            if self.side != side || self.region.is_none() {
                // Dropped before the new one is mapped, so a gallery that
                // re-renders at a new scale holds one region, not two.
                self.region = None;
                self.region = tairix_rt::shm::SharedRegion::create(want);
                self.side = side;
            }
            let region = self.region.as_ref()?;
            let handle = tairix_rt::shm_grant(region.id(), tairix_abi::window_ipc::WINDOW_ENDPOINT);
            u64::try_from(handle).ok().filter(|grant| *grant >= 1)
        }

        /// Adopt the conclusion of a render, answering whether the gallery
        /// changed.
        ///
        /// An answer for a position this program is not waiting on, or at a
        /// side its region is not, is dropped: the tile keeps waiting rather
        /// than drawing pixels of the wrong shape.
        fn settle(&mut self, shell: &mut Shell, index: u16, side: u16, rendered: bool) -> bool {
            if self.pending != Some(index) || self.side != side {
                return false;
            }
            self.pending = None;
            if !rendered {
                return shell.mark_picture_refused(index);
            }
            let Some(region) = self.region.as_mut() else {
                return shell.mark_picture_refused(index);
            };
            let pixels = region.bytes_mut();
            shell.set_picture(index, side, pixels)
        }

        /// Forget the outstanding render, because the desktop it was asked
        /// of has moved and every tile is being asked for again.
        const fn restart(&mut self) {
            self.pending = None;
        }
    }

    /// State the abnormal-exit reason on `stderr` (fail loud) and hand back
    /// `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "settings: {reason}");
        code
    }

    /// State a shared-shell bring-up refusal and hand its reserved code back.
    fn fail_shell(err: ShellError) -> i32 {
        let _ = writeln!(Stderr, "settings: {err}");
        err.code()
    }

    /// The production [`EventSource`]: drain the app's own event mailbox,
    /// parking on the wait-set whenever it is empty.
    struct RtEventSource<'a> {
        mailbox: EventMailbox,
        set: u64,
        /// The applier's wake, drained on an [`APPLY_TOKEN`] wake. Its
        /// readiness is a level peek, so leaving it undrained would report
        /// ready for ever and turn the park into a spin.
        applier: &'a Applier,
        /// The mount walk's wake, drained on a [`MOUNTS_TOKEN`] wake for
        /// the same reason.
        mounts: &'a Mounts,
        /// The machine readings' wake, drained on a [`MACHINE_TOKEN`] wake.
        machine: &'a Machine,
        /// The network readings' wake, drained on a [`NETWORK_TOKEN`] wake.
        network: &'a Network,
        /// The account readings' wake, drained on an [`ACCOUNTS_TOKEN`]
        /// wake.
        accounts: &'a Accounts,
        /// The elevated run's wake, drained on an [`ELEVATE_TOKEN`] wake.
        elevator: &'a Elevator,
        /// Set when the park woke for a desktop change, cleared when the loop
        /// adopts it.
        desktop_moved: &'a Cell<bool>,
        /// Set when the memory-pressure band moved, cleared when the loop has
        /// trimmed the window's icon cache to it.
        pressure_moved: &'a Cell<bool>,
    }

    impl EventDrain for RtEventSource<'_> {
        fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
            self.mailbox.try_next(event)
        }
    }

    impl EventSource for RtEventSource<'_> {
        fn park(&mut self) -> Result<Parked, Errno> {
            match app::park(self.set)? {
                // The session answered an apply. Draining is the whole of
                // noticing it, and the answer is the loop's to adopt, so the
                // wait ends here rather than parking again on a ready source.
                Wake::App(APPLY_TOKEN) => {
                    self.applier.wake().drain();
                    Ok(Parked::Interrupted)
                }
                // The mount table landed. Draining is the whole of noticing
                // it, and the answer is the loop's to adopt.
                Wake::App(MOUNTS_TOKEN) => {
                    self.mounts.wake().drain();
                    Ok(Parked::Interrupted)
                }
                // The machine readings landed.
                Wake::App(MACHINE_TOKEN) => {
                    self.machine.wake().drain();
                    Ok(Parked::Interrupted)
                }
                // The network readings landed.
                Wake::App(NETWORK_TOKEN) => {
                    self.network.wake().drain();
                    Ok(Parked::Interrupted)
                }
                // The account readings landed.
                Wake::App(ACCOUNTS_TOKEN) => {
                    self.accounts.wake().drain();
                    Ok(Parked::Interrupted)
                }
                // The broker answered an offered account.
                Wake::App(ELEVATE_TOKEN) => {
                    self.elevator.wake().drain();
                    Ok(Parked::Interrupted)
                }
                Wake::PressureChanged => {
                    tairix_font::trim_glyph_cache();
                    self.pressure_moved.set(true);
                    Ok(Parked::Interrupted)
                }
                Wake::DesktopChanged => {
                    self.desktop_moved.set(true);
                    Ok(Parked::Interrupted)
                }
                Wake::Event | Wake::PressureUnchanged | Wake::App(_) => Ok(Parked::Served),
            }
        }
    }

    /// The app's channel to the desktop, the window it may or may not have
    /// open, and the client extent that window is currently showing.
    struct SettingsWindow {
        window: AppWindow,
        mode: DisplayMode,
        /// The title the session is carrying for the window.
        title: &'static str,
        /// The built-in pictures the shell draws — the categories' badges —
        /// rasterised once per side rather than once per frame.
        artwork: ArtworkCache,
    }

    impl SettingsWindow {
        /// Open a window at the current mode and present the shell's first
        /// frame, answering the session's [`ProcId`] or the reserved exit code
        /// for the refusal.
        fn open(
            &mut self,
            event_endpoint: u64,
            shell: &Shell,
            theme: &Theme,
            scale: Scale,
        ) -> Result<ProcId, i32> {
            self.title = shell.title();
            let server = self
                .window
                .open(event_endpoint, &self.mode, self.title, win_sizing(scale))
                .map_err(fail_shell)?;
            if self
                .present(shell, theme, scale, DamageRect::full(&self.mode))
                .is_err()
            {
                self.close();
                return Err(fail(EXIT_CHANNEL_LOST, "present refused"));
            }
            Ok(server)
        }

        /// Close the open window, leaving the app on the icon bar.
        fn close(&mut self) {
            let _ = self.window.close();
        }

        /// The client rectangle the window is showing.
        fn viewport(&self) -> Rect {
            Rect::new(0, 0, self.mode.width_px, self.mode.height_px)
        }

        /// Draw the shell and present `damage`, then retitle the window if
        /// the pane on show has changed.
        ///
        /// The retitle follows the present so the title bar never names a pane
        /// the frame beneath it does not show yet. A refused retitle keeps the
        /// remembered title, so the next present asks again.
        fn present(
            &mut self,
            shell: &Shell,
            theme: &Theme,
            scale: Scale,
            damage: DamageRect,
        ) -> Result<(), Errno> {
            let viewport = self.viewport();
            let artwork = &mut self.artwork;
            // Nothing this window draws is read from a file: every picture is
            // compiled in, so the resolver refuses every asset tier and the
            // cache answers from its built-in one.
            let mut resolver = InlineArtwork::new(NoArtworkSeam, NoArtworkSeam);
            self.window.present(damage, |surface| {
                shell.render(
                    surface,
                    viewport,
                    scale,
                    theme,
                    &mut IconArtworkSource::new(artwork, &mut resolver),
                );
            })?;
            let wanted = shell.title();
            if wanted != self.title {
                if let Some(id) = self.window.window_id() {
                    if self.window.client().set_title(id, wanted).is_ok() {
                        self.title = wanted;
                    }
                }
            }
            Ok(())
        }
    }

    /// What one delivered event concluded.
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Acted {
        /// Nothing on screen changed.
        Idle,
        /// The shell changed and must be re-presented.
        Changed,
        /// The whole client changed and no report could describe it.
        Whole,
        /// The reader chose a setting: ask the desktop to adopt it, then
        /// re-read what it holds.
        Apply(String),
        /// The reader offered an account: ask the console's broker to run
        /// what it authorises, then adopt the verdict.
        Elevate(Elevation),
        /// The desktop queued a target: drain the queue and show what it
        /// named.
        Opened,
        /// A picture the gallery asked for is in the shared region, or was
        /// refused.
        Rendered {
            /// The catalog position that was asked for.
            index: u16,
            /// The square side it was rendered at.
            side: u16,
            /// Whether the region holds the picture.
            rendered: bool,
        },
        /// The reader asked for the screen to be locked: ask the desktop.
        LockScreen,
        /// End the program.
        Quit,
    }

    /// Apply one delivered event to the shell.
    fn apply_event(
        surface: &mut SettingsWindow,
        shell: &mut Shell,
        theme: &Theme,
        scale: Scale,
        event: &WindowEvent,
        damage: &mut Region,
    ) -> Acted {
        let viewport = surface.viewport();
        let concluded = |outcome: ShellOutcome| match outcome {
            ShellOutcome::Idle => Acted::Idle,
            ShellOutcome::Changed => Acted::Changed,
            ShellOutcome::Apply(document) => Acted::Apply(document),
            ShellOutcome::Elevate(asked) => Acted::Elevate(asked),
            ShellOutcome::LockScreen => Acted::LockScreen,
        };
        match event {
            WindowEvent::CloseRequested { .. } => Acted::Quit,
            // The desktop resized the client: re-map the frame region to the
            // new extent, then lay the shell out to it.
            WindowEvent::Resized {
                width_px,
                height_px,
                ..
            } => {
                let mode = app::mode_for(*width_px, *height_px);
                if surface.window.resize(mode) {
                    surface.mode = mode;
                } else {
                    // A refused resize leaves the old geometry standing and
                    // still drawable, so the window keeps the size it had.
                    let _ = writeln!(
                        Stderr,
                        "settings: the desktop refused a resize; the window keeps its size"
                    );
                }
                // The reported client extent is what the shell lays out to
                // either way, so the whole window is redrawn regardless.
                shell.lay_out(surface.viewport(), scale, theme);
                Acted::Whole
            }
            WindowEvent::Key {
                key: pressed @ KeyInput::Pressed { .. },
                ..
            } => match key_input_event(*pressed) {
                InputEvent::KeyPressed { key, modifiers } => {
                    concluded(shell.on_key(key, modifiers, viewport, scale, theme, damage))
                }
                _ => Acted::Idle,
            },
            WindowEvent::Pointer { x, y, action, .. } => concluded(apply_pointer(
                shell,
                pointer_point(*x, *y),
                *action,
                viewport,
                scale,
                theme,
                damage,
            )),
            WindowEvent::Scrolled { dx, dy, .. } => {
                let scroll = InputEvent::PointerScrolled { dx: *dx, dy: *dy };
                concluded(shell.on_pointer(&scroll, viewport, scale, theme, damage))
            }
            // The desktop queued at least one target for this instance.
            WindowEvent::OpenRequested => Acted::Opened,
            // A picture the gallery asked for. Answered by the loop, which
            // holds the region it was rendered into.
            WindowEvent::WallpaperRendered {
                index,
                side,
                rendered,
                ..
            } => Acted::Rendered {
                index: *index,
                side: *side,
                rendered: *rendered,
            },
            // A redraw needs nothing here: the client library re-presents the
            // last frame and the shell it drew has not changed. The rest are
            // events this surface does not act on — it opens no chain of the
            // desktop's, declares no file association, and owns no desktop
            // layer — and `ContentReleased` is the caller's, which owns the
            // region it lets go of.
            // Settings is part of the desktop rather than an application
            // the user manages: its signed manifest presents no icon-bar
            // slot and it declares none, so neither icon-bar event can
            // reach it. A secondary press on Close asks to leave what the
            // window shows, and this window shows only itself.
            WindowEvent::AlternateCloseRequested { .. }
            | WindowEvent::AppBarDefault
            | WindowEvent::AppBarMenu { .. }
            | WindowEvent::MenuClosed { .. }
            | WindowEvent::TerrainChanged { .. }
            | WindowEvent::LayerPointer { .. }
            | WindowEvent::Key { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::RedrawRequested { .. }
            | WindowEvent::ContentReleased { .. }
            | WindowEvent::FilePicked { .. }
            | WindowEvent::PickCancelled { .. } => Acted::Idle,
        }
    }

    /// Drain every target the desktop queued for this instance and show the
    /// last pane it named, answering whether the window moved.
    ///
    /// One event may cover several targets, and another may arrive while
    /// this drain is running, so it loops until the queue answers empty. A
    /// target that is not a pane is not one this application can act on —
    /// it declares no file association and holds no filesystem capability —
    /// and is stated rather than silently dropped. A pane it does not carry
    /// leaves the window where it is, which is the whole point of a target
    /// that confers nothing.
    fn drain_open_targets(
        shell: &mut Shell,
        surface: &mut SettingsWindow,
        theme: &Theme,
        scale: Scale,
    ) -> bool {
        let viewport = surface.viewport();
        let mut moved = false;
        loop {
            match surface.window.client().take_open_target() {
                Ok(Some(Target::Pane(pane))) => {
                    let mut sink = tairix_controls::damage::sink();
                    if shell.go_to_pane(&pane, viewport, scale, theme, &mut sink) {
                        moved = true;
                    } else {
                        let _ = writeln!(
                            Stderr,
                            "settings: there is no `{pane}` here; the window stays where it is"
                        );
                    }
                }
                Ok(Some(Target::Path(path))) => {
                    let _ = writeln!(
                        Stderr,
                        "settings: {path} was handed over, but this window shows settings, not \
                         files"
                    );
                }
                Ok(Some(Target::Document { name, .. })) => {
                    let _ = writeln!(
                        Stderr,
                        "settings: {name} was handed over as a document, which this window has \
                         nowhere to show"
                    );
                }
                Ok(None) => return moved,
                Err(err) => {
                    let _ = writeln!(Stderr, "settings: cannot take an open target: {err}");
                    return moved;
                }
            }
        }
    }

    /// Route one wire pointer event: a move to `at` to sync the pointer, then
    /// the press or release the action names.
    ///
    /// A press and its release are two inputs of one gesture, so the
    /// stronger of what they concluded is the gesture's: an apply the
    /// release asked for is not lost behind the press's bare repaint.
    fn apply_pointer(
        shell: &mut Shell,
        at: Point,
        action: PointerAction,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        let mut concluded = ShellOutcome::Idle;
        for input in pointer_input_events(action, at) {
            let acted = shell.on_pointer(&input, viewport, scale, theme, damage);
            // A conclusion the caller must act on outranks a repaint: a
            // press and its release are two events, and the one that asked
            // for something must not be lost to the one that did not.
            let asked = matches!(acted, ShellOutcome::Apply(_) | ShellOutcome::Elevate(_));
            let held = matches!(concluded, ShellOutcome::Apply(_) | ShellOutcome::Elevate(_));
            if asked || (acted.changed() && !held) {
                concluded = acted;
            }
        }
        concluded
    }

    /// Adopt the desktop the session published, if the park said it moved.
    fn adopt_desktop(
        desktop: &mut Desktop,
        themes: &mut ThemeRegistry,
        moved: &Cell<bool>,
    ) -> bool {
        if !moved.replace(false) {
            return false;
        }
        match app::adopt_desktop(desktop, themes) {
            Ok(changed) => changed,
            Err(err) => {
                let _ = writeln!(Stderr, "settings: desktop change refused: {err}");
                false
            }
        }
    }

    /// Ask the gallery for every picture again, because the desktop's
    /// scale or theme moved and a rendered picture is square at one side
    /// only.
    ///
    /// The outstanding render is forgotten rather than waited for: its
    /// answer would be at the old side and the settle drops it, so the
    /// gallery asks afresh instead of stalling behind an answer it cannot
    /// use.
    fn restart_pictures(
        shell: &mut Shell,
        surface: &mut SettingsWindow,
        pictures: &mut Pictures,
        theme: &Theme,
        scale: Scale,
    ) {
        shell.invalidate_pictures();
        pictures.restart();
        pictures.request(shell, surface, theme, scale);
    }

    /// Present the whole client, answering whether the session took it.
    fn present_whole(
        surface: &mut SettingsWindow,
        shell: &Shell,
        themes: &ThemeRegistry,
        desktop: &Desktop,
    ) -> bool {
        let whole = DamageRect::full(&surface.mode);
        surface
            .present(shell, themes.active(), desktop.scale(), whole)
            .is_ok()
    }

    /// Adopt a desktop change the park reported, answering whether the
    /// window is still presentable.
    ///
    /// A new density or theme re-measures every length the layout is
    /// derived from, and stales every rendered picture — a picture is
    /// square at one side only.
    fn adopt_desktop_round(
        surface: &mut SettingsWindow,
        shell: &mut Shell,
        themes: &mut ThemeRegistry,
        desktop: &mut Desktop,
        moved: &Cell<bool>,
        pictures: &mut Pictures,
    ) -> bool {
        if !adopt_desktop(desktop, themes, moved) {
            return true;
        }
        shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
        restart_pictures(shell, surface, pictures, themes.active(), desktop.scale());
        present_whole(surface, shell, themes, desktop)
    }

    /// The event loop: park, apply, repaint.
    /// The long-lived state one turn of the event loop reads and writes.
    ///
    /// Grouped because every path through the loop needs all of it: the
    /// window it presents to, the desktop and theme it draws with, the
    /// shell it routes into, and the channels it answers on.
    struct Session<'a> {
        surface: &'a mut SettingsWindow,
        desktop: &'a mut Desktop,
        themes: &'a mut ThemeRegistry,
        shell: &'a mut Shell,
        desktop_moved: &'a Cell<bool>,
        pressure_moved: &'a Cell<bool>,
        pictures: &'a mut Pictures,
        desks: Desks<'a>,
    }

    /// The four worker desks the loop submits to and collects from.
    ///
    /// One group, because every path through the loop reaches all of them
    /// and each is the same shape: submit, carry on drawing, adopt the
    /// answer on the wake it nudges.
    struct Desks<'a> {
        /// The desktop apply an immediate row posts.
        applier: &'a Applier,
        /// The mount-table walk the Storage pane lists.
        mounts: MountWalk<'a>,
        /// The machine's configuration and facts the General panes state.
        machine: MachineRead<'a>,
        /// The network readings the DNS pane states.
        network: NetworkRead<'a>,
        /// The account readings the Users pane states, and the salt a new
        /// password is hashed under.
        accounts: AccountRead<'a>,
        /// The broker round trip an offered account costs.
        elevator: &'a Elevator,
    }

    /// Adopt whatever a worker answered while the loop was parked, and
    /// redraw if anything did, answering whether the window is still
    /// presentable.
    ///
    /// Both desks land a whole new set of rows, so the client is redrawn
    /// rather than the one row a choice reported.
    fn adopt_answers(
        surface: &mut SettingsWindow,
        shell: &mut Shell,
        themes: &ThemeRegistry,
        desktop: &Desktop,
        desks: &mut Desks<'_>,
    ) -> bool {
        let mut landed = false;
        if let Some(outcome) = desks.applier.collect() {
            adopt_apply(shell, outcome);
            landed = true;
        }
        landed |= desks.mounts.settle(shell);
        landed |= desks.machine.settle(shell);
        landed |= desks.network.settle(shell);
        landed |= desks.accounts.settle(shell);
        if let Some(verdict) = desks.elevator.collect() {
            shell.adopt_elevation(verdict);
            landed = true;
        }
        if !landed {
            return true;
        }
        shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
        present_whole(surface, shell, themes, desktop)
    }

    /// Carry out what one event concluded, answering whether the program
    /// is to end.
    fn act(
        acted: &Acted,
        surface: &mut SettingsWindow,
        shell: &mut Shell,
        themes: &ThemeRegistry,
        desktop: &Desktop,
        desks: &Desks<'_>,
        pictures: &mut Pictures,
    ) -> bool {
        match acted {
            Acted::Quit => {
                surface.close();
                return true;
            }
            Acted::Elevate(asked) => {
                // Submitted, not awaited: the broker answers only once the
                // program it started has exited, and a window that waited
                // would stop drawing for the whole of it.
                if let Some(verdict) = submit_elevate(desks.elevator, asked.clone()) {
                    shell.adopt_elevation(verdict);
                    shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
                }
            }
            Acted::Apply(document) => {
                // Submitted, not awaited: the answer arrives on the wake
                // the worker nudges. With no worker to serve it the call
                // was made here and its answer is already in hand.
                if let Some(outcome) = submit_apply(desks.applier, document) {
                    adopt_apply(shell, outcome);
                    shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
                }
            }
            Acted::LockScreen => {
                // The desktop answers from memory and puts its own lock up
                // once it has, so the round trip costs no I/O either side.
                let answer = surface.window.client().lock_screen();
                if let Err(err) = answer {
                    let _ = writeln!(
                        Stderr,
                        "settings: the desktop would not lock the screen ({err})"
                    );
                }
                shell.adopt_lock_answer(answer);
                shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
            }
            Acted::Opened => {
                if drain_open_targets(shell, surface, themes.active(), desktop.scale()) {
                    shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
                }
            }
            Acted::Rendered {
                index,
                side,
                rendered,
            } => {
                pictures.settle(shell, *index, *side, *rendered);
            }
            Acted::Idle | Acted::Changed | Acted::Whole => {}
        }
        false
    }

    fn run_event_loop(session: Session<'_>, mut events: WindowEvents<RtEventSource<'_>>) -> i32 {
        let Session {
            surface,
            desktop,
            themes,
            shell,
            desktop_moved,
            pressure_moved,
            pictures,
            mut desks,
        } = session;
        loop {
            // Memory goes back when the machine asks for it, not at whatever
            // later frame happens to draw an icon.
            if pressure_moved.take() {
                surface.artwork.trim();
            }
            // An answer the park drained is the loop's to adopt, whether or
            // not an event came with it.
            if !adopt_answers(surface, shell, themes, desktop, &mut desks) {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
            let event = match events.wait(surface.window.client()) {
                Ok(Some(event)) => event,
                // A wait that ended without an event is the desktop notice; a
                // malformed frame from the authenticated session is refused
                // rather than guessed at. Either way the round is the
                // re-theme alone.
                Ok(None) | Err(EventError::Undecodable(_)) => {
                    if !adopt_desktop_round(
                        surface,
                        shell,
                        themes,
                        desktop,
                        desktop_moved,
                        pictures,
                    ) {
                        return fail(EXIT_CHANNEL_LOST, "present refused");
                    }
                    continue;
                }
                Err(EventError::Mailbox(_)) => {
                    return fail(EXIT_CHANNEL_LOST, "event channel lost")
                }
            };

            let redraw = adopt_desktop(desktop, themes, desktop_moved);
            if redraw {
                shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
                restart_pictures(shell, surface, pictures, themes.active(), desktop.scale());
            }
            let mut damage = tairix_controls::damage::sink();
            let acted = apply_event(
                surface,
                shell,
                themes.active(),
                desktop.scale(),
                &event,
                &mut damage,
            );
            if act(&acted, surface, shell, themes, desktop, &desks, pictures) {
                return 0;
            }
            // Ask for the next picture the gallery wants, whatever this
            // round was: a navigation, a resize and an answered render all
            // change what it is waiting for.
            pictures.request(shell, surface, themes.active(), desktop.scale());
            // And for the mount table, if this round put the storage pane on
            // show. With no worker to serve it the walk was made here and
            // the pane already holds its answer.
            let volumes_landed = desks.mounts.request(shell);
            // And for the machine's own readings, if this round put a pane
            // that states them on show.
            let machine_landed = desks.machine.request(shell);
            // And for the stack's resolver set, if this round put the pane
            // that states it on show.
            let network_landed = desks.network.request(shell);
            // And for the account readings, if this round put the pane that
            // states them on show or spent the salt it held.
            let accounts_landed = desks.accounts.request(shell);
            // And for the sources that have notified, which the session
            // answers from memory, if this round put that pane on show.
            let sources_landed = settle_notify_sources(shell, surface.window.client());
            if volumes_landed
                || machine_landed
                || network_landed
                || accounts_landed
                || sources_landed
            {
                shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
            }
            if matches!(event, WindowEvent::ContentReleased { .. }) {
                surface.window.release_frames();
                continue;
            }
            // An apply re-read the desktop, so every row may have moved:
            // the whole client is redrawn rather than the one row the
            // choice reported.
            let whole = redraw
                || machine_landed
                || volumes_landed
                || accounts_landed
                || sources_landed
                || matches!(
                    acted,
                    Acted::Whole
                        | Acted::Apply(_)
                        | Acted::LockScreen
                        | Acted::Opened
                        | Acted::Rendered { .. }
                );
            let repaint = match (whole, matches!(acted, Acted::Changed)) {
                (true, _) => Repaint::Whole,
                (false, true) => Repaint::Reported,
                (false, false) => Repaint::Nothing,
            };
            let Some(area) = present_damage(&surface.mode, repaint, &damage) else {
                continue;
            };
            if surface
                .present(shell, themes.active(), desktop.scale(), area)
                .is_err()
            {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
        }
    }

    /// Read everything the window's first frame needs, before it opens.
    ///
    /// Each of these is answered from memory or from one ungated round
    /// trip, so taking them here costs a moment at start-up and spares the
    /// reader a first frame that states nothing it could have stated.
    fn seat_first_frame(
        shell: &mut Shell,
        surface: &mut SettingsWindow,
        desktop: &Desktop,
        themes: &ThemeRegistry,
    ) {
        // The session lists the read-only picture store once at its own
        // bring-up and answers this from memory.
        shell.adopt_catalog(fetch_catalog(surface.window.client()));
        // And the pointer rows their choice space, for the same reason.
        shell.adopt_cursor_sets(fetch_cursor_sets(surface.window.client()));
        // A pane the launch named, if it named one: a fresh process is
        // given it as its one operand, exactly as a running instance is
        // handed it over the channel. `args` has already dropped the
        // program's own name, so the operand is the first of them.
        if let Some(argv) = tairix_rt::args().filter(|argv| !argv.is_empty()) {
            let mut sink = tairix_controls::damage::sink();
            let viewport = surface.viewport();
            let named = Pane::launched(&argv).is_some_and(|pane| {
                shell.go_to(pane, viewport, desktop.scale(), themes.active(), &mut sink)
            });
            if !named {
                let _ = writeln!(
                    Stderr,
                    "settings: that is not a pane here; the window opens where it always does"
                );
            }
        }
        // After the launch target, so a window opened *at* the storage pane
        // has its volumes on its first frame rather than on a later wake.
        // The pane asks for them again each time it comes on show, because
        // unlike the picture store the mount table moves.
        shell.adopt_volumes(read_mounts(&mut (), &mut ()));
        // And the machine's own readings, for the same reason: the window
        // opens on General, whose panes state them.
        let (config, facts) = read_machine(&mut (), &mut ());
        shell.adopt_config(config);
        shell.adopt_machine(facts);
        // And the sources that have notified, should the launch have named
        // the pane that lists them.
        settle_notify_sources(shell, surface.window.client());
        shell.lay_out(surface.viewport(), desktop.scale(), themes.active());
    }

    /// Ask the desktop which sources have notified, if the pane that lists
    /// them has come on show, answering whether an answer landed.
    ///
    /// The session answers from memory, so the round trip costs no I/O on
    /// either side. A name this build would not accept as a bundle identity
    /// is dropped rather than offered: a policy for it could never be kept.
    fn settle_notify_sources(
        shell: &mut Shell,
        client: &mut tairix_window::WindowClient<app::RtWindowTransport>,
    ) -> bool {
        if !shell.notify_sources_wanted() {
            return false;
        }
        let mut frame = [0u8; tairix_abi::window_ipc::WINDOW_NOTIFY_SOURCES_REPLY_MAX];
        let sources = match client.notify_sources(&mut frame) {
            Ok(answered) => Some(
                answered
                    .names()
                    .filter_map(|name| core::str::from_utf8(name).ok())
                    .filter(|name| tairix_abi::validate_bundle_id(name).is_ok())
                    .filter_map(|name| tairix_abi::BundleId::new(name).ok())
                    .collect(),
            ),
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "settings: the desktop would not say which programs have notified ({err})"
                );
                None
            }
        };
        shell.adopt_notify_sources(sources);
        true
    }

    /// Put `worker` on a desk of its own and start it.
    /// Every worker desk this window runs, started and owned together.
    ///
    /// One desk per kind of work rather than one shared desk: the six
    /// carry different jobs and a latest-wins desk would let any of them
    /// evict another's answer. Each would otherwise stall the window for a
    /// round trip.
    struct Workers {
        applier: Arc<Applier>,
        mounts: Arc<Mounts>,
        machine: Arc<Machine>,
        network: Arc<Network>,
        accounts: Arc<Accounts>,
        elevator: Arc<Elevator>,
    }

    impl Workers {
        /// Start every desk. A machine that grants no thread runs the work
        /// on the event loop instead, which each start states for itself.
        fn started() -> Self {
            Self {
                applier: started(
                    Applier::new(send_apply, (), tairix_rt::sync::WorkerWake::create()),
                    "apply",
                ),
                mounts: started(
                    Mounts::new(read_mounts, (), tairix_rt::sync::WorkerWake::create()),
                    "mount-table",
                ),
                machine: started(
                    Machine::new(read_machine, (), tairix_rt::sync::WorkerWake::create()),
                    "machine-readings",
                ),
                network: started(
                    Network::new(read_network, (), tairix_rt::sync::WorkerWake::create()),
                    "network-readings",
                ),
                accounts: started(
                    Accounts::new(read_accounts, (), tairix_rt::sync::WorkerWake::create()),
                    "account-readings",
                ),
                elevator: started(
                    Elevator::new(send_elevate, (), tairix_rt::sync::WorkerWake::create()),
                    "elevated-run",
                ),
            }
        }

        /// Put every desk's wake on `set`, so a landed answer ends the park
        /// the loop is already in.
        fn watched(&self, set: u64) -> Result<(), i32> {
            watch_wakes(
                set,
                &[
                    (self.applier.wake(), APPLY_TOKEN, "apply wake refused"),
                    (self.mounts.wake(), MOUNTS_TOKEN, "mount-table wake refused"),
                    (
                        self.machine.wake(),
                        MACHINE_TOKEN,
                        "machine-readings wake refused",
                    ),
                    (
                        self.network.wake(),
                        NETWORK_TOKEN,
                        "network-readings wake refused",
                    ),
                    (
                        self.accounts.wake(),
                        ACCOUNTS_TOKEN,
                        "account-readings wake refused",
                    ),
                    (
                        self.elevator.wake(),
                        ELEVATE_TOKEN,
                        "elevated-run wake refused",
                    ),
                ],
            )
        }
    }

    impl Drop for Workers {
        /// Ask every desk to leave. What the per-worker guards did
        /// individually, in the one place that owns them all.
        fn drop(&mut self) {
            self.applier.stop();
            self.mounts.stop();
            self.machine.stop();
            self.network.stop();
            self.accounts.stop();
            self.elevator.stop();
        }
    }

    fn started<S: Send + 'static, Req: Send + 'static, Ans: Send + 'static>(
        worker: tairix_rt::work::Worker<S, Req, Ans>,
        what: &str,
    ) -> Arc<tairix_rt::work::Worker<S, Req, Ans>> {
        let worker = Arc::new(worker);
        start_worker(&worker, what);
        worker
    }

    /// Start a worker, stating in this program's own words why a machine
    /// that grants none will do its work on the event loop instead.
    ///
    /// Not a failure: a program with no thread is exactly as correct and
    /// only as responsive as it was before there was a worker at all.
    fn start_worker<S: Send + 'static, Req: Send + 'static, Ans: Send + 'static>(
        worker: &Arc<tairix_rt::work::Worker<S, Req, Ans>>,
        what: &str,
    ) {
        if let Err(reason) = tairix_rt::work::Worker::start(worker) {
            let _ = writeln!(
                Stderr,
                "settings: no {what} worker ({reason:?}); it is done on the event loop"
            );
        }
    }

    /// Add each worker's wake to the loop's wait-set.
    ///
    /// A refused add is fatal rather than tolerated: an answer nobody
    /// collects would leave every row showing a value the desktop may never
    /// have adopted, or a storage pane waiting for ever on a table that has
    /// already landed.
    fn watch_wakes(
        set: u64,
        wakes: &[(&tairix_rt::sync::WorkerWake, u64, &str)],
    ) -> Result<(), i32> {
        for (wake, token, refusal) in wakes {
            let Some(read) = wake.read_end() else {
                continue;
            };
            if tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Stream,
                u64::from(read),
                *token,
            ) != 0
            {
                return Err(fail(app::EXIT_NO_EVENTS, refusal));
            }
        }
        Ok(())
    }

    /// The window's icon cache, budgeted from the `frame_bytes` of the
    /// window it draws into, and registered with the process's cache report.
    fn icon_cache(frame_bytes: usize) -> ArtworkCache {
        // The reclaim bookkeeping's audit sink. The shared constructor takes
        // a `'static` borrow, and the runtime sink owns nothing.
        static LOG_SINK: tairix_rt::LogSink = tairix_rt::LogSink;
        let cache = artwork_cache(
            "settings.icon-artwork",
            SEAT_PRIMARY,
            frame_bytes,
            tairix_rt::pressure::gauge(),
            &LOG_SINK,
        );
        if let Some(ledger) = cache.ledger() {
            tairix_rt::cachereport::register(ledger);
        }
        cache
    }

    /// Program entry point.
    fn main() -> i32 {
        let _ = tairix_rt::latency_watch(DEFAULT_FRAME_BUDGET_NS);

        let mut window = AppWindow::new();
        let (mut desktop, mut themes) = match app::bring_up_desktop(window.client()) {
            Ok(pair) => pair,
            Err(err) => return fail_shell(err),
        };
        let (initial_w, initial_h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mode = app::mode_for(initial_w, initial_h);
        // A frame region that cannot be sized is a window that can never open,
        // so it is stated here rather than as a refused create later.
        let Some(frame_bytes) = app::region_bytes(&mode, app::FRAME_COUNT) else {
            return fail(
                app::EXIT_NO_FRAMES,
                "window frame larger than the address width",
            );
        };
        let mut surface = SettingsWindow {
            window,
            mode,
            title: "",
            artwork: icon_cache(frame_bytes),
        };

        let binding = match app::bind_event_mailbox() {
            Ok(binding) => binding,
            Err(err) => return fail_shell(err),
        };
        let event_endpoint = binding.endpoint();

        // An empty registry would leave the window nothing to show at all, so
        // it ends fail-loud rather than opening a blank frame.
        let Some(mut shell) = Shell::new(settings_in_effect()) else {
            return fail(
                EXIT_CHANNEL_LOST,
                "the settings registry holds no categories",
            );
        };
        let workers = Workers::started();
        if let Err(code) = workers.watched(binding.set()) {
            return code;
        }
        let Workers {
            applier,
            mounts,
            machine,
            network,
            accounts,
            elevator,
        } = &workers;

        seat_first_frame(&mut shell, &mut surface, &desktop, &themes);

        let server = match surface.open(event_endpoint, &shell, themes.active(), desktop.scale()) {
            Ok(server) => server,
            Err(code) => return code,
        };

        let desktop_moved = Cell::new(false);
        let pressure_moved = Cell::new(false);
        let events = WindowEvents::new(RtEventSource {
            mailbox: EventMailbox::new(event_endpoint, server),
            set: binding.set(),
            applier,
            mounts,
            machine,
            network,
            accounts,
            elevator,
            desktop_moved: &desktop_moved,
            pressure_moved: &pressure_moved,
        });
        run_event_loop(
            Session {
                surface: &mut surface,
                desktop: &mut desktop,
                themes: &mut themes,
                shell: &mut shell,
                desktop_moved: &desktop_moved,
                pressure_moved: &pressure_moved,
                pictures: &mut Pictures::new(),
                desks: Desks {
                    applier,
                    mounts: MountWalk {
                        worker: mounts,
                        pending: false,
                    },
                    machine: MachineRead {
                        worker: machine,
                        pending: false,
                    },
                    network: NetworkRead {
                        worker: network,
                        pending: false,
                    },
                    accounts: AccountRead {
                        worker: accounts,
                        pending: false,
                    },
                    elevator,
                },
            },
            events,
        )
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
