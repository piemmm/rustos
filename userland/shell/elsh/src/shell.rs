//! The interpreter that ties parsing, expansion, builtins, and job control
//! together.
//!
//! [`Shell`] owns the [`Environment`] and [`JobTable`] and borrows the
//! injected [`ProcessHost`] and [`Console`] seams. [`Shell::run_line`] is the
//! one entry point: it reports any finished background jobs, parses the line,
//! and runs each pipeline honouring the `;`/`&&`/`||` connectors and the `&`
//! background flag.
//!
//! Failure handling follows: a line that does not parse or
//! expand runs nothing and returns a [`ParseError`]; a command the host
//! cannot launch is reported and becomes a non-zero `$?`, never a panic and
//! never a line abort, so the remaining connectors behave as POSIX requires.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::builtin::{self, is_builtin, BuiltinContext};
use crate::env::{split_prefix_assignments, Environment};
use crate::error::ParseError;
use crate::host::{
    classify_redirect_target, Console, Elevator, LaunchError, LaunchSpec, LimitStore, ProcessHost,
    RedirAction, RedirTarget, ResolvedCommand, ResolvedRedirection, NULL_ELEVATOR,
    NULL_LIMIT_STORE,
};
use crate::job::{ExitStatus, JobState, JobTable, WaitOutcome};
use crate::lexer::{FdSpec, Segment, Word};
use crate::parser::{
    parse, Command, CommandList, ListEntry, OpenMode, Pipeline, Redirection, RunCondition,
};

/// Standard output's descriptor — the target of a combined `&>` open.
const STDOUT_FD: u32 = 1;
/// Standard error's descriptor — duplicated onto stdout by a combined `&>`.
const STDERR_FD: u32 = 2;

/// Status used when a command cannot be resolved anywhere on the search
/// path (mirrors the POSIX "command not found" convention).
const NOT_FOUND_STATUS: i32 = 127;

/// Status used when a command resolved but cannot be run — a permission or
/// capability denial, a malformed image, or a launch feature the host cannot
/// express (mirrors the POSIX "command not executable" convention).
const NOT_EXECUTABLE_STATUS: i32 = 126;

/// The first descriptor a `{var}` dynamic redirection may allocate. Numbers
/// below it are refused: 0–3 are the reserved standard streams and 4–9 stay
/// free for explicit script use, matching zsh's allocation floor.
const FIRST_DYN_FD: u32 = 10;

/// The interactive shell interpreter.
pub struct Shell<'a> {
    env: Environment,
    jobs: JobTable,
    host: &'a dyn ProcessHost,
    console: &'a dyn Console,
    limits: &'a dyn LimitStore,
    elevator: &'a dyn Elevator,
    exit: Option<i32>,
    next_dyn_fd: u32,
}

impl<'a> Shell<'a> {
    /// Create a shell with a fresh [`Environment`].
    #[must_use]
    pub fn new(host: &'a dyn ProcessHost, console: &'a dyn Console) -> Self {
        Self::with_environment(host, console, Environment::new())
    }

    /// Create a shell over a pre-populated [`Environment`] (e.g. one seeded
    /// by `login` with `HOME`, `PATH`, and the user identity).
    #[must_use]
    pub fn with_environment(
        host: &'a dyn ProcessHost,
        console: &'a dyn Console,
        env: Environment,
    ) -> Self {
        Self {
            env,
            jobs: JobTable::new(),
            host,
            console,
            limits: &NULL_LIMIT_STORE,
            elevator: &NULL_ELEVATOR,
            exit: None,
            next_dyn_fd: FIRST_DYN_FD,
        }
    }

    /// Install the resource-limit seam the `ulimit` builtin drives.
    ///
    /// A shell built without one uses a fail-closed default, so `ulimit`
    /// reports [`tairix_abi::Errno::NotImplemented`] rather than pretending a
    /// get or set landed. This mirrors the `with_*`
    /// builder seams the kernel boot path uses.
    #[must_use]
    pub fn with_limits(mut self, limits: &'a dyn LimitStore) -> Self {
        self.limits = limits;
        self
    }

    /// Install the elevation seam the `elevate` builtin drives.
    ///
    /// A shell built without one uses a fail-closed default, so `elevate`
    /// reports [`tairix_abi::Errno::NotImplemented`] rather than pretending
    /// a command ran. This mirrors [`Shell::with_limits`].
    #[must_use]
    pub fn with_elevator(mut self, elevator: &'a dyn Elevator) -> Self {
        self.elevator = elevator;
        self
    }

    /// The interactive prompt string for the current environment and working
    /// directory, rendered from `ELSH_PROMPT` (see
    /// [`Environment::render_prompt`]). Recomputed each time so the prompt
    /// tracks `cd` and any prompt-format change.
    #[must_use]
    pub fn render_prompt(&self) -> String {
        self.env.render_prompt()
    }

    /// The shell's environment.
    #[must_use]
    pub fn environment(&self) -> &Environment {
        &self.env
    }

    /// The shell's job table.
    #[must_use]
    pub fn jobs(&self) -> &JobTable {
        &self.jobs
    }

    /// The exit code requested by the `exit` builtin, if any. A read-eval
    /// loop stops when this becomes `Some`.
    #[must_use]
    pub fn exit_request(&self) -> Option<i32> {
        self.exit
    }

    /// Parse and run one command line.
    ///
    /// A line whose here-documents cannot be satisfied from a single string
    /// fails closed with [`ParseError::UnterminatedHereDoc`]: collecting the
    /// body lines is the caller's job ([`Shell::parse_line`] +
    /// [`CommandList::feed_here_doc_line`] + [`Shell::run_list`]), as the
    /// REPL does.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if the line cannot be lexed, parsed, or
    /// expanded; in that case the error is reported, `$?` becomes 2, and
    /// nothing from the line runs.
    pub fn run_line(&mut self, line: &str) -> Result<(), ParseError> {
        let list = self.parse_line(line)?;
        self.run_list(&list)
    }

    /// Parse one command line, first reporting any finished background jobs
    /// (exactly as a shell prints `[1] Done cmd` before its next prompt).
    ///
    /// The returned list may still be awaiting here-document bodies
    /// ([`CommandList::pending_here_doc`]); feed them with
    /// [`CommandList::feed_here_doc_line`] before [`Shell::run_list`].
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if the line cannot be lexed or parsed; the
    /// error is reported and `$?` becomes 2.
    pub fn parse_line(&mut self, line: &str) -> Result<CommandList, ParseError> {
        self.report_finished_jobs();
        parse(line).map_err(|err| self.fail_line(err))
    }

    /// Run a parsed command list, honouring the `;`/`&&`/`||` connectors and
    /// the `&` background flag.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if a word cannot be expanded, a redirection
    /// target is malformed, or a here-document body is missing or was
    /// discarded as over-length; the error is reported, `$?` becomes 2, and
    /// nothing further from the line runs.
    pub fn run_list(&mut self, list: &CommandList) -> Result<(), ParseError> {
        for entry in &list.entries {
            if self.should_run(entry.run_if) {
                if let Err(err) = self.run_entry(entry) {
                    return Err(self.fail_line(err));
                }
            }
        }
        Ok(())
    }

    /// Report a line-aborting error on standard error and set `$?` to 2 (the
    /// shell's parse/usage status), returning the error for propagation. One
    /// definition, so parse-stage and run-stage aborts report identically.
    fn fail_line(&mut self, err: ParseError) -> ParseError {
        self.console.write_stderr(&format!("shell: {err}\n"));
        self.env.set_last_status(2);
        err
    }

    fn should_run(&self, condition: RunCondition) -> bool {
        match condition {
            RunCondition::Always => true,
            RunCondition::OnSuccess => self.env.last_status() == 0,
            RunCondition::OnFailure => self.env.last_status() != 0,
        }
    }

    fn run_entry(&mut self, entry: &ListEntry) -> Result<(), ParseError> {
        if !entry.background {
            if let Some(status) = self.try_run_special(&entry.pipeline)? {
                self.env
                    .set_last_status(negate_if(entry.pipeline.negated, status));
                return Ok(());
            }
        }
        let commands = self.resolve_pipeline(&entry.pipeline)?;
        if entry.background {
            // `!` negates a pipeline's *exit status*; a background launch
            // reports 0 either way, so negation only applies foreground.
            self.launch_background(&commands);
        } else {
            self.launch_foreground(&commands);
            let status = negate_if(entry.pipeline.negated, self.env.last_status());
            self.env.set_last_status(status);
        }
        Ok(())
    }

    /// Handle the two cases that must run inside the shell process: an
    /// assignment-only command and a standalone builtin (either may carry
    /// `NAME=VALUE` prefix assignments). Returns the status if handled, or
    /// `None` if the pipeline is an ordinary external one.
    fn try_run_special(&mut self, pipeline: &Pipeline) -> Result<Option<i32>, ParseError> {
        if pipeline.commands.len() != 1 {
            return Ok(None);
        }
        let command = &pipeline.commands[0];
        let (assignments, rest) = split_prefix_assignments(&command.words);
        if rest.is_empty() {
            // An assignment-only command mutates the shell's own variables —
            // but only the plain form: with redirections attached it is left
            // to the ordinary launch path, exactly as before the split.
            if !command.redirections.is_empty() {
                return Ok(None);
            }
            let assignments = self.expand_assignments(assignments)?;
            for (name, value) in assignments {
                self.env.set(name, value);
            }
            return Ok(Some(0));
        }
        let name = self.env.expand_word(&rest[0])?;
        if !is_builtin(&name) {
            return Ok(None);
        }
        // Builtins write through the Console seam; a redirection on one
        // cannot be applied yet. Refusing loudly beats silently sending a
        // stream the user redirected to the terminal instead.
        if !command.redirections.is_empty() {
            self.console
                .write_stderr("shell: redirections on builtins are not supported\n");
            return Ok(Some(1));
        }
        let mut argv = Vec::with_capacity(rest.len());
        argv.push(name);
        for word in &rest[1..] {
            argv.push(self.env.expand_word(word)?);
        }
        // Prefix assignments bind for the builtin's duration only — they are
        // the command's environment, not the shell's. Argv and values were
        // expanded above, against the *pre-assignment* environment, as POSIX
        // orders expansion before assignment.
        let assignments = self.expand_assignments(assignments)?;
        let saved: Vec<(String, Option<String>)> = assignments
            .iter()
            .map(|(name, _)| (name.clone(), self.env.get(name).map(String::from)))
            .collect();
        for (name, value) in assignments {
            self.env.set(name, value);
        }
        let status = self.run_builtin(&argv);
        for (name, old) in saved.into_iter().rev() {
            match old {
                Some(value) => self.env.set(name, value),
                None => {
                    self.env.unset(&name);
                }
            }
        }
        Ok(Some(status))
    }

    /// Expand the value word of each pending `NAME=VALUE` assignment against
    /// the current (pre-assignment) environment.
    fn expand_assignments(
        &self,
        assignments: Vec<(String, Word)>,
    ) -> Result<Vec<(String, String)>, ParseError> {
        let mut expanded = Vec::with_capacity(assignments.len());
        for (name, value_word) in assignments {
            expanded.push((name, self.env.expand_word(&value_word)?));
        }
        Ok(expanded)
    }

    fn run_builtin(&mut self, argv: &[String]) -> i32 {
        let mut ctx = BuiltinContext {
            env: &mut self.env,
            jobs: &mut self.jobs,
            host: self.host,
            console: self.console,
            limits: self.limits,
            elevator: self.elevator,
            exit: &mut self.exit,
        };
        builtin::dispatch(&mut ctx, argv).unwrap_or(NOT_FOUND_STATUS)
    }

    fn resolve_pipeline(
        &mut self,
        pipeline: &Pipeline,
    ) -> Result<Vec<ResolvedCommand>, ParseError> {
        let mut commands = Vec::with_capacity(pipeline.commands.len());
        for command in &pipeline.commands {
            commands.push(self.resolve_command(command)?);
        }
        Ok(commands)
    }

    fn resolve_command(&mut self, command: &Command) -> Result<ResolvedCommand, ParseError> {
        let (assignments, rest) = split_prefix_assignments(&command.words);
        // A command that is *all* assignments (e.g. `FOO=1 >file`, or the
        // background `FOO=1 &`) launches verbatim, as it always did — prefix
        // assignments only bind when a command word follows them.
        let (env_overrides, argv_words) = if rest.is_empty() {
            (Vec::new(), &command.words[..])
        } else {
            (self.expand_assignments(assignments)?, rest)
        };
        let mut argv = Vec::with_capacity(argv_words.len());
        for word in argv_words {
            argv.push(self.env.expand_word(word)?);
        }
        let mut redirections = Vec::with_capacity(command.redirections.len());
        for redirection in &command.redirections {
            self.lower_redirection(redirection, &mut redirections)?;
        }
        let redirections = merge_multios(redirections)?;
        Ok(ResolvedCommand {
            argv,
            env_overrides,
            redirections,
        })
    }

    /// Lower one parsed [`Redirection`] into the primitive descriptor actions
    /// the host applies, expanding any target word.
    ///
    /// A combined `&>` open lowers to two actions — open the target on stdout,
    /// then duplicate stdout onto stderr — so the host never re-derives the
    /// combined-redirection meaning (one definition of what `&>` does).
    fn lower_redirection(
        &mut self,
        redirection: &Redirection,
        out: &mut Vec<ResolvedRedirection>,
    ) -> Result<(), ParseError> {
        match redirection {
            Redirection::File { fd, mode, target } => {
                let fd = self.resolve_bound_fd(fd)?;
                out.push(ResolvedRedirection {
                    fd,
                    action: RedirAction::Open {
                        mode: *mode,
                        target: classify_redirect_target(self.env.expand_word(target)?)?,
                    },
                });
            }
            Redirection::Combined {
                append,
                clobber,
                target,
            } => {
                let mode = if *append {
                    OpenMode::Append { clobber: *clobber }
                } else {
                    OpenMode::Write { clobber: *clobber }
                };
                out.push(ResolvedRedirection {
                    fd: STDOUT_FD,
                    action: RedirAction::Open {
                        mode,
                        target: classify_redirect_target(self.env.expand_word(target)?)?,
                    },
                });
                out.push(ResolvedRedirection {
                    fd: STDERR_FD,
                    action: RedirAction::Dup { source: STDOUT_FD },
                });
            }
            Redirection::Dup { fd, source } => {
                let fd = self.resolve_bound_fd(fd)?;
                out.push(ResolvedRedirection {
                    fd,
                    action: RedirAction::Dup { source: *source },
                });
            }
            Redirection::Close { fd } => {
                let fd = self.resolve_close_fd(fd)?;
                out.push(ResolvedRedirection {
                    fd,
                    action: RedirAction::Close,
                });
            }
            Redirection::HereString { fd, content } => {
                // A here-string feeds the expanded word followed by a single
                // newline — the one definition of its shape, so the host reads
                // the bytes verbatim without re-appending the terminator.
                let fd = self.resolve_bound_fd(fd)?;
                let mut bytes = self.env.expand_word(content)?;
                bytes.push('\n');
                out.push(ResolvedRedirection {
                    fd,
                    action: RedirAction::HereString { content: bytes },
                });
            }
            Redirection::HereDoc(doc) => {
                // The collected body is already newline-terminated per line;
                // it fails closed here when unterminated or discarded as
                // over-length. A quoted delimiter makes the body literal;
                // otherwise it undergoes the same `$` expansion as a word.
                let body = doc.body()?;
                let content = if doc.is_quoted() {
                    String::from(body)
                } else {
                    self.env
                        .expand_word(&alloc::vec![Segment::Expandable(String::from(body))])?
                };
                let fd = self.resolve_bound_fd(doc.fd())?;
                out.push(ResolvedRedirection {
                    fd,
                    action: RedirAction::HereString { content },
                });
            }
        }
        Ok(())
    }

    /// Resolve the descriptor an opening/duplicating redirection binds. A
    /// `{var}` spec allocates the next dynamic descriptor — always ≥
    /// [`FIRST_DYN_FD`], never a standard stream — and binds its number to
    /// the shell parameter `var`.
    ///
    /// # Errors
    ///
    /// [`ParseError::BadDynamicFd`] if the allocator is exhausted (the
    /// counter would overflow), failing closed rather than re-issuing a
    /// descriptor.
    fn resolve_bound_fd(&mut self, spec: &FdSpec) -> Result<u32, ParseError> {
        match spec {
            FdSpec::Fd(n) => Ok(*n),
            FdSpec::Var(name) => {
                let fd = self.next_dyn_fd;
                self.next_dyn_fd = fd.checked_add(1).ok_or(ParseError::BadDynamicFd)?;
                self.env.set(name.clone(), fd.to_string());
                Ok(fd)
            }
        }
    }

    /// Resolve the descriptor a closing redirection acts on. A `{var}` spec
    /// reads back a number a previous `{var}` redirection allocated; a
    /// variable that does not hold such a number fails closed — the shell
    /// never closes a standard stream on a stale or mistyped variable.
    ///
    /// # Errors
    ///
    /// [`ParseError::BadDynamicFd`] if `var` does not hold an allocated
    /// descriptor number.
    fn resolve_close_fd(&self, spec: &FdSpec) -> Result<u32, ParseError> {
        match spec {
            FdSpec::Fd(n) => Ok(*n),
            FdSpec::Var(name) => self
                .env
                .get(name)
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|fd| *fd >= FIRST_DYN_FD)
                .ok_or(ParseError::BadDynamicFd),
        }
    }

    fn launch_foreground(&mut self, commands: &[ResolvedCommand]) {
        let env = self.env.exported_vars();
        let spec = LaunchSpec {
            commands,
            env: &env,
            background: false,
        };
        let pid = match self.host.launch(&spec) {
            Ok(pid) => pid,
            Err(err) => {
                let (status, reason) = launch_failure(err);
                self.console
                    .write_stderr(&format!("shell: {}: {reason}\n", program_name(commands)));
                self.env.set_last_status(status);
                return;
            }
        };
        match self.host.wait(pid) {
            Ok(WaitOutcome::Stopped(signal)) => {
                let id = self
                    .jobs
                    .add(pid, command_text(commands), JobState::Stopped);
                self.console.write_stdout(&format!(
                    "[{}] Stopped {}\n",
                    id.as_u32(),
                    command_text(commands)
                ));
                self.env.set_last_status(128 + signal);
            }
            Ok(outcome) => {
                let status = outcome.terminal().map_or(0, ExitStatus::code);
                // A child admitted by `spawn` but then refused by its own
                // asynchronous image load exits with a reserved `LOAD_*`
                // status: turn that into a loud, named diagnosis instead of a
                // silent, opaque `$?`.
                if let Some((reason, shell_status)) = async_load_failure(status) {
                    self.console
                        .write_stderr(&format!("shell: {}: {reason}\n", program_name(commands)));
                    self.env.set_last_status(shell_status);
                } else {
                    // A child the kernel killed for an unresolvable memory
                    // fault exits `139`: state it loudly on stderr rather
                    // than leaving the user only an opaque `$?` (fail loud).
                    // The status stays `139` for scripts to test.
                    if let Some(reason) = fault_kill_reason(status) {
                        self.console.write_stderr(&format!(
                            "shell: {}: {reason}\n",
                            program_name(commands)
                        ));
                    }
                    self.env.set_last_status(status);
                }
            }
            Err(err) => {
                self.console.write_stderr(&format!("shell: {err}\n"));
                self.env.set_last_status(NOT_FOUND_STATUS);
            }
        }
    }

    fn launch_background(&mut self, commands: &[ResolvedCommand]) {
        let env = self.env.exported_vars();
        let spec = LaunchSpec {
            commands,
            env: &env,
            background: true,
        };
        match self.host.launch(&spec) {
            Ok(pid) => {
                let id = self
                    .jobs
                    .add(pid, command_text(commands), JobState::Running);
                self.console
                    .write_stdout(&format!("[{}] {}\n", id.as_u32(), pid.as_u64()));
                self.env.set_last_status(0);
            }
            Err(err) => {
                let (status, reason) = launch_failure(err);
                self.console
                    .write_stderr(&format!("shell: {}: {reason}\n", program_name(commands)));
                self.env.set_last_status(status);
            }
        }
    }

    /// Drain the host's background state changes into the job table and
    /// report any that finished, exactly as a shell prints `[1] Done cmd`
    /// before its next prompt.
    fn report_finished_jobs(&mut self) {
        while let Some((pid, outcome)) = self.host.poll() {
            if let Some(status) = outcome.terminal() {
                self.jobs.set_state(pid, JobState::Done(status));
            } else if let WaitOutcome::Stopped(_) = outcome {
                self.jobs.set_state(pid, JobState::Stopped);
            }
        }
        for job in self.jobs.drain_done() {
            self.console
                .write_stdout(&format!("[{}] Done {}\n", job.id.as_u32(), job.command));
            // A background child whose asynchronous load was refused finished
            // with a reserved `LOAD_*` status; state its reason on stderr so
            // the failure is never silent (fail loud).
            if let JobState::Done(ExitStatus::Exited(code)) = job.state {
                if let Some((reason, _)) = async_load_failure(code) {
                    self.console
                        .write_stderr(&format!("shell: {}: {reason}\n", job.command));
                } else if let Some(reason) = fault_kill_reason(code) {
                    // A background child killed by a memory fault: state it
                    // on stderr so the failure is never silent (fail loud).
                    self.console
                        .write_stderr(&format!("shell: {}: {reason}\n", job.command));
                }
            }
        }
    }
}

/// Map a launch refusal onto its POSIX-style exit status and report text.
///
/// Only a *spawn* that exhausted every search candidate is `127`,
/// "command not found"; a redirection refusal names the redirection instead,
/// because blaming the program for a missing input file sends the reader
/// looking in the wrong place.
fn launch_failure(err: LaunchError) -> (i32, String) {
    match err {
        // A redirection failure is not the program's fault, so it is never
        // worded as a missing command however it failed.
        LaunchError::Redirection(errno) => (NOT_EXECUTABLE_STATUS, format!("redirection: {errno}")),
        LaunchError::Spawn(tairix_abi::Errno::NotFound) => {
            (NOT_FOUND_STATUS, String::from("command not found"))
        }
        LaunchError::Spawn(errno) => (NOT_EXECUTABLE_STATUS, errno.to_string()),
    }
}

/// If `code` is one of the reserved asynchronous-load-failure exit statuses
/// (a child that `spawn` admitted but that then refused to load its own image
/// — the async-launch semantics of `plans/FIX-DESKTOP.md` DESK-1), returns
/// the terse human reason to print and the coreutils-conventional `$?` for
/// it: a missing or unreadable program is "command not found" (127); every
/// other load refusal (verification, malformed image, out of memory) is
/// "found but not executable" (126). Returns `None` for an ordinary exit,
/// which is reported by its own code. The reason text is the single shared
/// [`tairix_abi::load_failure_reason`] mapping, so the shell and every other
/// launcher word a cause identically.
fn async_load_failure(code: i32) -> Option<(&'static str, i32)> {
    tairix_abi::load_failure_reason(code).map(|reason| {
        let status = if code == tairix_abi::LOAD_NOT_FOUND {
            NOT_FOUND_STATUS
        } else {
            NOT_EXECUTABLE_STATUS
        };
        (reason, status)
    })
}

/// The shell exit status a task killed by an unresolvable memory fault
/// carries: `128 + SIGSEGV (11)`, the conventional "segmentation fault"
/// code. The kernel records it for every user-fault kill.
const FAULT_KILL_STATUS: i32 = 139;

/// If `code` is the fault-kill status, the terse "why" to state on the
/// crashed command's `stderr` so a segfault is never a silent, opaque `$?`
/// (fail loud). The breadcrumb states only the class every user understands;
/// the precise cause (read vs write, near-null vs wild) and the backtrace
/// live in the capability-gated crash record, never on the terminal.
fn fault_kill_reason(code: i32) -> Option<&'static str> {
    (code == FAULT_KILL_STATUS).then_some("killed by fault (segmentation fault)")
}

/// Negate an exit status when `negated` (the `!` pipeline prefix): 0 becomes
/// 1, any failure becomes 0.
fn negate_if(negated: bool, status: i32) -> i32 {
    if negated {
        i32::from(status == 0)
    } else {
        status
    }
}

/// The stream direction an [`OpenMode`] opens — `Some(true)` writes,
/// `Some(false)` reads, `None` for the bidirectional `<>` (which multios
/// never merges).
fn open_direction(mode: OpenMode) -> Option<bool> {
    match mode {
        OpenMode::Read => Some(false),
        OpenMode::Write { .. } | OpenMode::Append { .. } => Some(true),
        OpenMode::ReadWrite => None,
    }
}

/// Merge repeated opens on one descriptor into a single
/// [`RedirAction::Multi`] — zsh multios: repeated output redirections fan
/// out, repeated input redirections concatenate in order.
///
/// # Errors
///
/// [`ParseError::AmbiguousRedirection`] when one descriptor mixes reading and
/// writing opens (or involves the bidirectional `<>`): such a line has no one
/// meaning, so it runs nothing.
fn merge_multios(
    redirections: Vec<ResolvedRedirection>,
) -> Result<Vec<ResolvedRedirection>, ParseError> {
    let mut out: Vec<ResolvedRedirection> = Vec::with_capacity(redirections.len());
    for redirection in redirections {
        let ResolvedRedirection {
            fd,
            action: RedirAction::Open { mode, target },
        } = redirection
        else {
            out.push(redirection);
            continue;
        };
        match out.iter_mut().find(|prior| {
            prior.fd == fd
                && matches!(
                    prior.action,
                    RedirAction::Open { .. } | RedirAction::Multi { .. }
                )
        }) {
            None => out.push(ResolvedRedirection {
                fd,
                action: RedirAction::Open { mode, target },
            }),
            Some(prior) => {
                let previous = core::mem::replace(&mut prior.action, RedirAction::Close);
                prior.action = merge_open(previous, mode, target)?;
            }
        }
    }
    Ok(out)
}

/// Fold one more open (`mode`, `target`) into an existing open or multios on
/// the same descriptor. See [`merge_multios`] for the direction rule.
fn merge_open(
    prior: RedirAction,
    mode: OpenMode,
    target: RedirTarget,
) -> Result<RedirAction, ParseError> {
    let mut targets = match prior {
        RedirAction::Open {
            mode: first_mode,
            target: first_target,
        } => alloc::vec![(first_mode, first_target)],
        RedirAction::Multi { targets } => targets,
        // Unreachable by `merge_multios`'s filter; failing closed keeps the
        // merge total without ever panicking or dropping an open.
        _ => return Err(ParseError::AmbiguousRedirection),
    };
    let direction = open_direction(targets[0].0);
    if direction.is_none() || open_direction(mode) != direction {
        return Err(ParseError::AmbiguousRedirection);
    }
    targets.push((mode, target));
    Ok(RedirAction::Multi { targets })
}

fn program_name(commands: &[ResolvedCommand]) -> String {
    commands
        .first()
        .and_then(|c| c.argv.first())
        .cloned()
        .unwrap_or_default()
}

fn command_text(commands: &[ResolvedCommand]) -> String {
    let rendered: Vec<String> = commands.iter().map(|c| c.argv.join(" ")).collect();
    rendered.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::Shell;
    use crate::job::{JobState, Pid, WaitOutcome};
    use crate::test_support::{RecordingConsole, ScriptedHost};

    #[test]
    fn foreground_launch_is_recorded_and_status_comes_from_wait() {
        let host = ScriptedHost::new();
        host.set_wait(Pid::new(100), WaitOutcome::Exited(5));
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("ls -l").unwrap();

        let launches = host.launches();
        assert_eq!(launches.len(), 1);
        assert!(!launches[0].background);
        assert_eq!(launches[0].commands[0].argv, ["ls", "-l"]);
        assert_eq!(shell.environment().last_status(), 5);
    }

    #[test]
    fn unlaunchable_command_reports_and_sets_127() {
        let host = ScriptedHost::new();
        host.fail_launch_of("nope");
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("nope arg").unwrap();

        assert_eq!(shell.environment().last_status(), 127);
        assert!(console.stderr().contains("shell: nope: command not found"));
        assert!(host.launches().is_empty());
    }

    /// A redirection target that cannot be opened is reported against the
    /// *redirection*, never as a missing command. Blaming the program for a
    /// missing input file sends the reader looking in the wrong place, and
    /// `127` would claim the command does not exist when it does.
    #[test]
    fn unopenable_redirection_target_is_not_a_missing_command() {
        for errno in [
            tairix_abi::Errno::NotFound,
            tairix_abi::Errno::PermissionDenied,
            tairix_abi::Errno::NotSupported,
        ] {
            let host = ScriptedHost::new();
            host.fail_redirection_with(errno);
            let console = RecordingConsole::new();
            let mut shell = Shell::new(&host, &console);

            shell.run_line("cat < missing.txt").unwrap();

            assert_eq!(shell.environment().last_status(), 126, "{errno}");
            let stderr = console.stderr();
            assert!(
                stderr.contains("redirection") && stderr.contains(&alloc::format!("{errno}")),
                "{errno} reported as {stderr:?}"
            );
            assert!(
                !stderr.contains("command not found"),
                "{errno} blamed the command: {stderr:?}"
            );
        }
    }

    #[test]
    fn non_executable_command_reports_and_sets_126() {
        let host = ScriptedHost::new();
        host.fail_launch_with("secret-tool", tairix_abi::Errno::PermissionDenied);
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        // A command that resolved but is refused (a permission or capability
        // denial) is the POSIX 126 "found but not executable" case, distinct
        // from 127 "not found".
        shell.run_line("secret-tool").unwrap();

        assert_eq!(shell.environment().last_status(), 126);
        assert!(console.stderr().contains("shell: secret-tool"));
        assert!(host.launches().is_empty());

        // The background launch path reports through the same mapping.
        shell.run_line("secret-tool &").unwrap();
        assert_eq!(shell.environment().last_status(), 126);
    }

    #[test]
    fn foreground_async_load_failure_is_reported_with_its_reason() {
        // A child that `spawn` admits but whose own asynchronous image load is
        // then refused (signature/hash mismatch) exits with a reserved
        // `LOAD_*` status. The shell must turn that into a loud, named
        // diagnosis, never a silent, opaque `$?`.
        let host = ScriptedHost::new();
        host.set_wait(
            Pid::new(100),
            WaitOutcome::Exited(tairix_abi::LOAD_UNVERIFIED),
        );
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("badapp").unwrap();

        assert!(console
            .stderr()
            .contains("shell: badapp: signature or hash verification failed"));
        // A verification/build refusal is the POSIX 126 "found but not
        // executable" case.
        assert_eq!(shell.environment().last_status(), 126);
    }

    #[test]
    fn foreground_missing_program_load_failure_maps_to_127() {
        // A missing/unreadable bundle surfaces (asynchronously) as
        // `LOAD_NOT_FOUND`, the POSIX 127 "command not found" case.
        let host = ScriptedHost::new();
        host.set_wait(
            Pid::new(100),
            WaitOutcome::Exited(tairix_abi::LOAD_NOT_FOUND),
        );
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("ghost").unwrap();

        assert!(console
            .stderr()
            .contains("shell: ghost: program not found or not readable"));
        assert_eq!(shell.environment().last_status(), 127);
    }

    #[test]
    fn ordinary_nonzero_exit_is_not_treated_as_a_load_failure() {
        // A program that runs and exits non-zero on its own is reported by its
        // own code with nothing on stderr — only the reserved `LOAD_*` band is
        // diagnosed as a load failure.
        let host = ScriptedHost::new();
        host.set_wait(Pid::new(100), WaitOutcome::Exited(3));
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("tool").unwrap();

        assert_eq!(shell.environment().last_status(), 3);
        assert!(console.stderr().is_empty());
    }

    #[test]
    fn foreground_fault_kill_is_reported_loudly_and_keeps_status_139() {
        // A child the kernel kills for an unresolvable memory fault exits
        // 139 (128 + SIGSEGV). The shell states it loudly on stderr, never
        // leaving the user only an opaque `$?`, and keeps 139 for scripts.
        let host = ScriptedHost::new();
        host.set_wait(Pid::new(100), WaitOutcome::Exited(139));
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("crasher").unwrap();

        assert!(console
            .stderr()
            .contains("shell: crasher: killed by fault (segmentation fault)"));
        assert_eq!(shell.environment().last_status(), 139);
        // The breadcrumb never carries an address, register, or secret.
        assert!(!console.stderr().contains("0x"));
    }

    #[test]
    fn background_fault_kill_is_reported_loudly() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("crasher &").unwrap();
        assert_eq!(shell.jobs().len(), 1);
        console.clear();

        host.queue_poll(Pid::new(100), WaitOutcome::Exited(139));
        shell.run_line("echo hi").unwrap();

        assert!(console.stdout().contains("[1] Done crasher\n"));
        assert!(console
            .stderr()
            .contains("shell: crasher: killed by fault (segmentation fault)"));
        assert!(shell.jobs().is_empty());
    }

    #[test]
    fn background_async_load_failure_is_reported_loudly() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("badapp &").unwrap();
        assert_eq!(shell.jobs().len(), 1);
        console.clear();

        // The background child finished with a reserved load-failure status;
        // the next line drains it, prints its `[1] Done` line, and states the
        // reason on stderr so the failure is never silent.
        host.queue_poll(
            Pid::new(100),
            WaitOutcome::Exited(tairix_abi::LOAD_MALFORMED),
        );
        shell.run_line("echo hi").unwrap();

        assert!(console.stdout().contains("[1] Done badapp\n"));
        assert!(console
            .stderr()
            .contains("shell: badapp: executable is malformed or incompatible"));
        assert!(shell.jobs().is_empty());
    }

    #[test]
    fn failed_command_short_circuits_and_with_or() {
        let host = ScriptedHost::new();
        host.fail_launch_of("a");
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        // `a` fails (127); `&&` skips `b`; `|| c` then runs because $? != 0.
        shell.run_line("a && b || c").unwrap();

        let launched: alloc::vec::Vec<_> = host
            .launches()
            .into_iter()
            .map(|r| r.commands[0].argv[0].clone())
            .collect();
        assert_eq!(launched, ["c"]);
        assert_eq!(shell.environment().last_status(), 0);
    }

    #[test]
    fn background_command_is_tracked_as_a_running_job() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("sleep 10 &").unwrap();

        assert_eq!(shell.environment().last_status(), 0);
        assert_eq!(shell.jobs().len(), 1);
        let job = &shell.jobs().all()[0];
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.command, "sleep 10");
        assert!(console.stdout().starts_with("[1] "));
    }

    #[test]
    fn finished_background_job_is_reported_before_the_next_line() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("sleep 10 &").unwrap();
        assert_eq!(shell.jobs().len(), 1);
        console.clear();

        // The host reports the background pid has exited; the next line drains
        // it, prints `[1] Done ...`, and prunes the job.
        host.queue_poll(Pid::new(100), WaitOutcome::Exited(0));
        shell.run_line("echo hi").unwrap();

        assert!(console.stdout().contains("[1] Done sleep 10\n"));
        assert!(shell.jobs().is_empty());
    }

    #[test]
    fn combined_redirection_lowers_to_open_then_dup() {
        use crate::host::{RedirAction, RedirTarget};
        use crate::parser::OpenMode;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("run &>both").unwrap();

        let launches = host.launches();
        assert_eq!(launches.len(), 1);
        let redirs = &launches[0].commands[0].redirections;
        // `&>both` lowers to: open `both` on stdout, then dup stdout onto stderr.
        assert_eq!(
            redirs,
            &[
                super::ResolvedRedirection {
                    fd: 1,
                    action: RedirAction::Open {
                        mode: OpenMode::Write { clobber: false },
                        target: RedirTarget::Path("both".into()),
                    },
                },
                super::ResolvedRedirection {
                    fd: 2,
                    action: RedirAction::Dup { source: 1 },
                },
            ]
        );
    }

    #[test]
    fn resource_reference_target_reaches_the_host_as_a_resource() {
        use crate::host::{RedirAction, RedirTarget};
        use alloc::string::ToString;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("run >sys:null").unwrap();

        let launches = host.launches();
        let action = &launches[0].commands[0].redirections[0].action;
        match action {
            RedirAction::Open {
                target: RedirTarget::Resource(reference),
                ..
            } => assert_eq!(reference.to_string(), "sys:null"),
            other => panic!("expected a resource target, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_filename_target_reaches_the_host_as_a_path() {
        use crate::host::{RedirAction, RedirTarget};
        use crate::parser::OpenMode;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("run >mylisting.txt").unwrap();

        let launches = host.launches();
        let action = &launches[0].commands[0].redirections[0].action;
        assert_eq!(
            action,
            &RedirAction::Open {
                mode: OpenMode::Write { clobber: false },
                target: RedirTarget::Path("mylisting.txt".into()),
            }
        );
    }

    #[test]
    fn malformed_resource_target_runs_nothing() {
        use crate::error::ParseError;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        // A registered namespace with a broken reference aborts the line and
        // launches nothing — never a fallback file open.
        assert_eq!(
            shell.run_line("run >sys:null@"),
            Err(ParseError::InvalidResourceTarget)
        );
        assert!(host.launches().is_empty());
    }

    #[test]
    fn numbered_dup_reaches_the_host_verbatim() {
        use crate::host::RedirAction;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("run 2>&1").unwrap();

        let launches = host.launches();
        assert_eq!(
            launches[0].commands[0].redirections,
            [super::ResolvedRedirection {
                fd: 2,
                action: RedirAction::Dup { source: 1 },
            }]
        );
    }

    #[test]
    fn here_string_lowers_to_its_content_plus_a_newline() {
        use crate::host::RedirAction;
        use alloc::string::ToString;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        // The here-string feeds the expanded word plus one trailing newline as
        // the input of fd 0.
        shell.run_line("run <<<hello").unwrap();
        let launches = host.launches();
        assert_eq!(
            launches[0].commands[0].redirections,
            [super::ResolvedRedirection {
                fd: 0,
                action: RedirAction::HereString {
                    content: "hello\n".to_string(),
                },
            }]
        );

        // An explicit IO number binds the here-string's descriptor.
        shell.run_line("run 4<<< body").unwrap();
        let launches = host.launches();
        assert_eq!(
            launches[1].commands[0].redirections,
            [super::ResolvedRedirection {
                fd: 4,
                action: RedirAction::HereString {
                    content: "body\n".to_string(),
                },
            }]
        );
    }

    #[test]
    fn here_string_expands_its_content() {
        use crate::env::Environment;
        use crate::host::RedirAction;
        use alloc::string::ToString;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut env = Environment::new();
        env.set("WHO", "world");
        let mut shell = Shell::with_environment(&host, &console, env);

        shell.run_line("run <<<$WHO").unwrap();
        let launches = host.launches();
        assert_eq!(
            launches[0].commands[0].redirections,
            [super::ResolvedRedirection {
                fd: 0,
                action: RedirAction::HereString {
                    content: "world\n".to_string(),
                },
            }]
        );
    }

    #[test]
    fn parse_error_runs_nothing_and_sets_status_2() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        assert!(shell.run_line("ls |").is_err());
        assert_eq!(shell.environment().last_status(), 2);
        assert!(host.launches().is_empty());
    }

    #[test]
    fn expansion_error_is_reported_and_sets_status_2() {
        use crate::error::ParseError;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        // Expansion happens after parsing; its failure must be reported and
        // must set `$?` exactly like a parse failure (this once escaped
        // unreported, leaving `$?` untouched).
        assert_eq!(
            shell.run_line("echo ${OOPS"),
            Err(ParseError::UnterminatedExpansion)
        );
        assert_eq!(shell.environment().last_status(), 2);
        assert!(console.stderr().contains("unterminated ${...} expansion"));
        assert!(host.launches().is_empty());
    }

    #[test]
    fn here_document_body_is_collected_and_lowered() {
        use crate::host::RedirAction;
        use alloc::string::ToString;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        let mut list = shell.parse_line("run <<EOF").unwrap();
        list.feed_here_doc_line("line one");
        list.feed_here_doc_line("line two");
        list.feed_here_doc_line("EOF");
        shell.run_list(&list).unwrap();

        let launches = host.launches();
        assert_eq!(
            launches[0].commands[0].redirections,
            [super::ResolvedRedirection {
                fd: 0,
                action: RedirAction::HereString {
                    content: "line one\nline two\n".to_string(),
                },
            }]
        );
    }

    #[test]
    fn here_document_body_expands_unless_the_delimiter_was_quoted() {
        use crate::env::Environment;
        use crate::host::RedirAction;
        use alloc::string::ToString;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut env = Environment::new();
        env.set("WHO", "world");
        let mut shell = Shell::with_environment(&host, &console, env);

        // Unquoted delimiter: the body undergoes `$` expansion.
        let mut list = shell.parse_line("run <<EOF").unwrap();
        list.feed_here_doc_line("hello $WHO");
        list.feed_here_doc_line("EOF");
        shell.run_list(&list).unwrap();

        // Quoted delimiter: the body is literal.
        let mut list = shell.parse_line("run <<'EOF'").unwrap();
        list.feed_here_doc_line("hello $WHO");
        list.feed_here_doc_line("EOF");
        shell.run_list(&list).unwrap();

        let launches = host.launches();
        assert_eq!(
            launches[0].commands[0].redirections[0].action,
            RedirAction::HereString {
                content: "hello world\n".to_string(),
            }
        );
        assert_eq!(
            launches[1].commands[0].redirections[0].action,
            RedirAction::HereString {
                content: "hello $WHO\n".to_string(),
            }
        );
    }

    #[test]
    fn unterminated_here_document_runs_nothing() {
        use crate::error::ParseError;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        // `run_line` cannot collect a body from a single string, so it fails
        // closed rather than running with empty input.
        assert_eq!(
            shell.run_line("run <<EOF"),
            Err(ParseError::UnterminatedHereDoc)
        );
        assert_eq!(shell.environment().last_status(), 2);
        assert!(console.stderr().contains("missing its terminator"));
        assert!(host.launches().is_empty());
    }

    #[test]
    fn over_length_here_document_runs_nothing() {
        use crate::error::ParseError;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        let mut list = shell.parse_line("run <<EOF").unwrap();
        list.feed_here_doc_line("kept");
        list.poison_pending_here_doc();
        list.feed_here_doc_line("EOF");
        assert_eq!(shell.run_list(&list), Err(ParseError::HereDocTooLarge));
        assert_eq!(shell.environment().last_status(), 2);
        assert!(console.stderr().contains("here-document too large"));
        assert!(host.launches().is_empty());
    }

    #[test]
    fn bang_negates_the_foreground_status() {
        let host = ScriptedHost::new();
        host.fail_launch_of("nope");
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        // A succeeding external command (default wait outcome is Exited(0)).
        shell.run_line("! run").unwrap();
        assert_eq!(shell.environment().last_status(), 1);

        // A failing launch (127) negates to success.
        shell.run_line("! nope").unwrap();
        assert_eq!(shell.environment().last_status(), 0);

        // A builtin negates through the same path.
        shell.run_line("! echo hi").unwrap();
        assert_eq!(shell.environment().last_status(), 1);
    }

    #[test]
    fn prefix_assignment_reaches_the_child_not_the_shell() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("FOO=bar run arg").unwrap();

        let launches = host.launches();
        let command = &launches[0].commands[0];
        assert_eq!(command.argv, ["run", "arg"]);
        assert_eq!(command.env_overrides, [("FOO".into(), "bar".into())]);
        // The shell's own environment is untouched.
        assert_eq!(shell.environment().get("FOO"), None);
    }

    #[test]
    fn prefix_assignment_on_a_builtin_is_temporary() {
        use crate::env::Environment;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut env = Environment::new();
        env.set("FOO", "old");
        let mut shell = Shell::with_environment(&host, &console, env);

        // Expansion happens before the assignment binds, so `$FOO` is still
        // "old" — and afterwards the shell variable is restored.
        shell.run_line("FOO=new echo $FOO").unwrap();
        assert_eq!(console.stdout(), "old\n");
        assert_eq!(shell.environment().get("FOO"), Some("old"));

        // A variable that was unset before is unset again afterwards.
        shell.run_line("BAR=x echo hi").unwrap();
        assert_eq!(shell.environment().get("BAR"), None);
    }

    #[test]
    fn builtin_with_a_redirection_fails_closed() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("echo hi >out").unwrap();

        assert_eq!(shell.environment().last_status(), 1);
        assert!(console
            .stderr()
            .contains("redirections on builtins are not supported"));
        // Nothing was echoed and nothing launched: the stream the user
        // redirected must never silently land on the terminal instead.
        assert_eq!(console.stdout(), "");
        assert!(host.launches().is_empty());
    }

    #[test]
    fn repeated_output_redirections_fan_out() {
        use crate::host::{RedirAction, RedirTarget};
        use crate::parser::OpenMode;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("run >a >>b").unwrap();

        let launches = host.launches();
        assert_eq!(
            launches[0].commands[0].redirections,
            [super::ResolvedRedirection {
                fd: 1,
                action: RedirAction::Multi {
                    targets: alloc::vec![
                        (
                            OpenMode::Write { clobber: false },
                            RedirTarget::Path("a".into())
                        ),
                        (
                            OpenMode::Append { clobber: false },
                            RedirTarget::Path("b".into())
                        ),
                    ],
                },
            }]
        );
    }

    #[test]
    fn repeated_input_redirections_concatenate() {
        use crate::host::{RedirAction, RedirTarget};
        use crate::parser::OpenMode;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("run <a <b").unwrap();

        let launches = host.launches();
        assert_eq!(
            launches[0].commands[0].redirections,
            [super::ResolvedRedirection {
                fd: 0,
                action: RedirAction::Multi {
                    targets: alloc::vec![
                        (OpenMode::Read, RedirTarget::Path("a".into())),
                        (OpenMode::Read, RedirTarget::Path("b".into())),
                    ],
                },
            }]
        );
    }

    #[test]
    fn multios_may_mix_paths_and_resources() {
        use crate::host::{RedirAction, RedirTarget};

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("run >log >sys:null").unwrap();

        let launches = host.launches();
        let action = &launches[0].commands[0].redirections[0].action;
        let RedirAction::Multi { targets } = action else {
            panic!("expected a multios action, got {action:?}");
        };
        assert!(matches!(targets[0].1, RedirTarget::Path(ref p) if p == "log"));
        assert!(matches!(targets[1].1, RedirTarget::Resource(_)));
    }

    #[test]
    fn mixed_direction_opens_on_one_fd_fail_closed() {
        use crate::error::ParseError;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        // Reading and writing opens on the same descriptor have no one
        // meaning; the line runs nothing.
        assert_eq!(
            shell.run_line("run 1<a >b"),
            Err(ParseError::AmbiguousRedirection)
        );
        // The bidirectional `<>` never merges.
        assert_eq!(
            shell.run_line("run <>a <>b"),
            Err(ParseError::AmbiguousRedirection)
        );
        assert_eq!(shell.environment().last_status(), 2);
        assert!(host.launches().is_empty());
    }

    #[test]
    fn dynamic_fd_allocates_from_ten_and_binds_the_variable() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("run {fd}>out {log}>log").unwrap();

        let launches = host.launches();
        let redirs = &launches[0].commands[0].redirections;
        // Never a standard stream: allocation starts at 10 and advances.
        assert_eq!(redirs[0].fd, 10);
        assert_eq!(redirs[1].fd, 11);
        assert_eq!(shell.environment().get("fd"), Some("10"));
        assert_eq!(shell.environment().get("log"), Some("11"));
    }

    #[test]
    fn dynamic_fd_close_reuses_the_bound_number() {
        use crate::host::RedirAction;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        shell.run_line("run {fd}>out").unwrap();
        shell.run_line("run {fd}>&-").unwrap();

        let launches = host.launches();
        assert_eq!(
            launches[1].commands[0].redirections,
            [super::ResolvedRedirection {
                fd: 10,
                action: RedirAction::Close,
            }]
        );
    }

    #[test]
    fn dynamic_fd_close_without_an_allocation_fails_closed() {
        use crate::error::ParseError;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);

        // No allocation bound `fd`, so there is no number to close.
        assert_eq!(shell.run_line("run {fd}>&-"), Err(ParseError::BadDynamicFd));

        // A variable holding a standard-stream number is refused too: the
        // shell never closes fd 0–3 (or 4–9) off a stale or mistyped value.
        shell.run_line("fd=1").unwrap();
        assert_eq!(shell.run_line("run {fd}>&-"), Err(ParseError::BadDynamicFd));
        assert_eq!(shell.environment().last_status(), 2);
        assert!(host.launches().is_empty());
    }
}
