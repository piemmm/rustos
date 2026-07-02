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
use alloc::string::String;
use alloc::vec::Vec;

use crate::builtin::{self, is_builtin, BuiltinContext};
use crate::env::{assignment_split, Environment};
use crate::error::ParseError;
use crate::host::{
    classify_redirect_target, Console, LaunchSpec, LimitStore, ProcessHost, RedirAction,
    ResolvedCommand, ResolvedRedirection, NULL_LIMIT_STORE,
};
use crate::job::{ExitStatus, JobState, JobTable, WaitOutcome};
use crate::lexer::Segment;
use crate::parser::{
    parse, Command, CommandList, ListEntry, OpenMode, Pipeline, Redirection, RunCondition,
};

/// Standard output's descriptor — the target of a combined `&>` open.
const STDOUT_FD: u32 = 1;
/// Standard error's descriptor — duplicated onto stdout by a combined `&>`.
const STDERR_FD: u32 = 2;

/// Status used when a command cannot be launched (mirrors the POSIX
/// "command not found" convention).
const NOT_FOUND_STATUS: i32 = 127;

/// The interactive shell interpreter.
pub struct Shell<'a> {
    env: Environment,
    jobs: JobTable,
    host: &'a dyn ProcessHost,
    console: &'a dyn Console,
    limits: &'a dyn LimitStore,
    exit: Option<i32>,
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
            exit: None,
        }
    }

    /// Install the resource-limit seam the `ulimit` builtin drives.
    ///
    /// A shell built without one uses a fail-closed default, so `ulimit`
    /// reports [`rustos_abi::Errno::NotImplemented`] rather than pretending a
    /// get or set landed. This mirrors the `with_*`
    /// builder seams the kernel boot path uses.
    #[must_use]
    pub fn with_limits(mut self, limits: &'a dyn LimitStore) -> Self {
        self.limits = limits;
        self
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
                self.env.set_last_status(status);
                return Ok(());
            }
        }
        let commands = self.resolve_pipeline(&entry.pipeline)?;
        if entry.background {
            self.launch_background(&commands);
        } else {
            self.launch_foreground(&commands);
        }
        Ok(())
    }

    /// Handle the two cases that must run inside the shell process: an
    /// assignment-only command and a standalone builtin. Returns the status
    /// if handled, or `None` if the pipeline is an ordinary external one.
    fn try_run_special(&mut self, pipeline: &Pipeline) -> Result<Option<i32>, ParseError> {
        if pipeline.commands.len() != 1 {
            return Ok(None);
        }
        let command = &pipeline.commands[0];
        if command.redirections.is_empty() {
            if let Some(status) = self.try_assignments(command)? {
                return Ok(Some(status));
            }
        }
        let argv = self.expand_argv(command)?;
        match argv.first() {
            Some(name) if is_builtin(name) => Ok(Some(self.run_builtin(&argv))),
            _ => Ok(None),
        }
    }

    /// If every word of `command` is a `NAME=VALUE` assignment, apply them to
    /// the environment and return status 0; otherwise return `None`.
    fn try_assignments(&mut self, command: &Command) -> Result<Option<i32>, ParseError> {
        let mut pending = Vec::with_capacity(command.words.len());
        for word in &command.words {
            match assignment_split(word) {
                Some((name, value_word)) => {
                    let value = self.env.expand_word(&value_word)?;
                    pending.push((name, value));
                }
                None => return Ok(None),
            }
        }
        for (name, value) in pending {
            self.env.set(name, value);
        }
        Ok(Some(0))
    }

    fn run_builtin(&mut self, argv: &[String]) -> i32 {
        let mut ctx = BuiltinContext {
            env: &mut self.env,
            jobs: &mut self.jobs,
            host: self.host,
            console: self.console,
            limits: self.limits,
            exit: &mut self.exit,
        };
        builtin::dispatch(&mut ctx, argv).unwrap_or(NOT_FOUND_STATUS)
    }

    fn resolve_pipeline(&self, pipeline: &Pipeline) -> Result<Vec<ResolvedCommand>, ParseError> {
        let mut commands = Vec::with_capacity(pipeline.commands.len());
        for command in &pipeline.commands {
            commands.push(self.resolve_command(command)?);
        }
        Ok(commands)
    }

    fn resolve_command(&self, command: &Command) -> Result<ResolvedCommand, ParseError> {
        let argv = self.expand_argv(command)?;
        let mut redirections = Vec::with_capacity(command.redirections.len());
        for redirection in &command.redirections {
            self.lower_redirection(redirection, &mut redirections)?;
        }
        Ok(ResolvedCommand { argv, redirections })
    }

    /// Lower one parsed [`Redirection`] into the primitive descriptor actions
    /// the host applies, expanding any target word.
    ///
    /// A combined `&>` open lowers to two actions — open the target on stdout,
    /// then duplicate stdout onto stderr — so the host never re-derives the
    /// combined-redirection meaning (one definition of what `&>` does).
    fn lower_redirection(
        &self,
        redirection: &Redirection,
        out: &mut Vec<ResolvedRedirection>,
    ) -> Result<(), ParseError> {
        match redirection {
            Redirection::File { fd, mode, target } => out.push(ResolvedRedirection {
                fd: *fd,
                action: RedirAction::Open {
                    mode: *mode,
                    target: classify_redirect_target(self.env.expand_word(target)?)?,
                },
            }),
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
            Redirection::Dup { fd, source } => out.push(ResolvedRedirection {
                fd: *fd,
                action: RedirAction::Dup { source: *source },
            }),
            Redirection::Close { fd } => out.push(ResolvedRedirection {
                fd: *fd,
                action: RedirAction::Close,
            }),
            Redirection::HereString { fd, content } => {
                // A here-string feeds the expanded word followed by a single
                // newline — the one definition of its shape, so the host reads
                // the bytes verbatim without re-appending the terminator.
                let mut bytes = self.env.expand_word(content)?;
                bytes.push('\n');
                out.push(ResolvedRedirection {
                    fd: *fd,
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
                out.push(ResolvedRedirection {
                    fd: doc.fd(),
                    action: RedirAction::HereString { content },
                });
            }
        }
        Ok(())
    }

    fn expand_argv(&self, command: &Command) -> Result<Vec<String>, ParseError> {
        let mut argv = Vec::with_capacity(command.words.len());
        for word in &command.words {
            argv.push(self.env.expand_word(word)?);
        }
        Ok(argv)
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
                self.console
                    .write_stderr(&format!("shell: {}: {err}\n", program_name(commands)));
                self.env.set_last_status(NOT_FOUND_STATUS);
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
                self.env.set_last_status(status);
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
                self.console
                    .write_stderr(&format!("shell: {}: {err}\n", program_name(commands)));
                self.env.set_last_status(NOT_FOUND_STATUS);
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
        }
    }
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
        assert!(console.stderr().contains("shell: nope"));
        assert!(host.launches().is_empty());
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
}
