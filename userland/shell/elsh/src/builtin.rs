//! The shell's small set of built-in commands.
//!
//! A builtin runs *inside* the shell process rather than being launched
//! through the [`ProcessHost`], because it must read or
//! mutate shell state the host cannot see: the working directory (`cd`,
//! `pwd`), the variable table (`export`, `unset`), the job table (`jobs`,
//! `fg`, `bg`), or the request to leave the shell (`exit`).
//!
//! Each builtin returns an exit status; the caller stores it as `$?`. The
//! recognised set is intentionally minimal (no bloat):
//! `cd`, `pwd`, `exit`, `export`, `unset`, `echo`, `jobs`, `fg`, `bg`, and
//! `help`. A name outside this set is not a builtin and is launched as an
//! external program instead.

use alloc::format;
use alloc::string::{String, ToString};

use crate::env::{is_valid_name, Environment};
use crate::host::{Console, LimitStore, ProcessHost};
use crate::job::{JobId, JobState, JobTable, Signal};
use crate::ulimit;

/// Status returned by a builtin that completed without error.
const OK: i32 = 0;
/// Status returned by a builtin used incorrectly (bad argument, missing job).
const USAGE_ERROR: i32 = 1;

/// The mutable shell state a builtin may touch, gathered into one borrow so
/// builtins live in their own module without the `Shell` exposing its
/// internals.
pub(crate) struct BuiltinContext<'a> {
    pub env: &'a mut Environment,
    pub jobs: &'a mut JobTable,
    pub host: &'a dyn ProcessHost,
    pub console: &'a dyn Console,
    pub limits: &'a dyn LimitStore,
    /// Set to `Some(code)` by `exit` to ask the read-eval loop to stop.
    pub exit: &'a mut Option<i32>,
}

/// `true` if `name` is a shell builtin.
pub(crate) fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "cd" | "pwd"
            | "exit"
            | "export"
            | "unset"
            | "echo"
            | "jobs"
            | "fg"
            | "bg"
            | "ulimit"
            | "help"
    )
}

/// Run the builtin named by `argv[0]` with `argv[1..]` as arguments.
///
/// `argv` is non-empty. Returns the builtin's exit status, or `None` if
/// `argv[0]` is not a builtin (the caller then launches it externally).
pub(crate) fn dispatch(ctx: &mut BuiltinContext<'_>, argv: &[String]) -> Option<i32> {
    let (name, operands) = argv.split_first()?;
    let status = match name.as_str() {
        "cd" => cd(ctx, operands),
        "pwd" => pwd(ctx),
        "exit" => exit(ctx, operands),
        "export" => export(ctx, operands),
        "unset" => unset(ctx, operands),
        "echo" => echo(ctx, operands),
        "jobs" => jobs(ctx),
        "fg" => fg(ctx, operands),
        "bg" => bg(ctx, operands),
        "ulimit" => ulimit::ulimit(ctx, operands),
        "help" => help(ctx),
        _ => return None,
    };
    Some(status)
}

fn cd(ctx: &mut BuiltinContext<'_>, args: &[String]) -> i32 {
    let target = if let Some(path) = args.first() {
        path.clone()
    } else {
        let Some(home) = ctx.env.get("HOME") else {
            ctx.console.write_stderr("cd: HOME not set\n");
            return USAGE_ERROR;
        };
        home.to_string()
    };
    match ctx.host.change_directory(&target) {
        Ok(resolved) => {
            ctx.env.set_cwd(resolved);
            OK
        }
        Err(err) => {
            ctx.console.write_stderr(&format!("cd: {target}: {err}\n"));
            USAGE_ERROR
        }
    }
}

fn pwd(ctx: &mut BuiltinContext<'_>) -> i32 {
    ctx.console.write_stdout(&format!("{}\n", ctx.env.cwd()));
    OK
}

fn exit(ctx: &mut BuiltinContext<'_>, args: &[String]) -> i32 {
    let code = match args.first() {
        Some(text) => text.parse::<i32>().unwrap_or_else(|_| {
            ctx.console
                .write_stderr(&format!("exit: {text}: numeric argument required\n"));
            255
        }),
        None => ctx.env.last_status(),
    };
    *ctx.exit = Some(code);
    code
}

fn export(ctx: &mut BuiltinContext<'_>, args: &[String]) -> i32 {
    if args.is_empty() {
        for (name, value) in ctx.env.exported_vars() {
            ctx.console
                .write_stdout(&format!("export {name}={value}\n"));
        }
        return OK;
    }
    let mut status = OK;
    for arg in args {
        if let Some(eq) = arg.find('=') {
            let name = &arg[..eq];
            if is_valid_name(name) {
                ctx.env.export(name, &arg[eq + 1..]);
            } else {
                ctx.console
                    .write_stderr(&format!("export: {arg}: not a valid identifier\n"));
                status = USAGE_ERROR;
            }
        } else if is_valid_name(arg) {
            if !ctx.env.mark_exported(arg) {
                ctx.env.export(arg, "");
            }
        } else {
            ctx.console
                .write_stderr(&format!("export: {arg}: not a valid identifier\n"));
            status = USAGE_ERROR;
        }
    }
    status
}

fn unset(ctx: &mut BuiltinContext<'_>, args: &[String]) -> i32 {
    let mut status = OK;
    for arg in args {
        if is_valid_name(arg) {
            ctx.env.unset(arg);
        } else {
            ctx.console
                .write_stderr(&format!("unset: {arg}: not a valid identifier\n"));
            status = USAGE_ERROR;
        }
    }
    status
}

fn echo(ctx: &mut BuiltinContext<'_>, args: &[String]) -> i32 {
    let (suppress_newline, words) = match args.first() {
        Some(flag) if flag == "-n" => (true, &args[1..]),
        _ => (false, args),
    };
    let mut line = words.join(" ");
    if !suppress_newline {
        line.push('\n');
    }
    ctx.console.write_stdout(&line);
    OK
}

fn jobs(ctx: &mut BuiltinContext<'_>) -> i32 {
    for job in ctx.jobs.all() {
        let state = match job.state {
            JobState::Running => "Running",
            JobState::Stopped => "Stopped",
            JobState::Done(_) => "Done",
        };
        ctx.console.write_stdout(&format!(
            "[{}] {} {}\n",
            job.id.as_u32(),
            state,
            job.command
        ));
    }
    OK
}

/// Resolve a `%N` / `N` job argument, defaulting to the current job.
fn resolve_job(ctx: &BuiltinContext<'_>, args: &[String]) -> Option<JobId> {
    match args.first() {
        Some(spec) => {
            let digits = spec.strip_prefix('%').unwrap_or(spec);
            let id = digits.parse::<u32>().ok()?;
            ctx.jobs
                .all()
                .iter()
                .find(|j| j.id.as_u32() == id)
                .map(|j| j.id)
        }
        None => ctx.jobs.current(),
    }
}

fn fg(ctx: &mut BuiltinContext<'_>, args: &[String]) -> i32 {
    let Some(id) = resolve_job(ctx, args) else {
        ctx.console.write_stderr("fg: no such job\n");
        return USAGE_ERROR;
    };
    let Some(job) = ctx.jobs.by_id(id) else {
        ctx.console.write_stderr("fg: no such job\n");
        return USAGE_ERROR;
    };
    let pid = job.pid;
    let command = job.command.clone();
    if ctx.host.signal(pid, Signal::Continue).is_err() {
        ctx.console.write_stderr("fg: cannot resume job\n");
        return USAGE_ERROR;
    }
    ctx.jobs.set_state(pid, JobState::Running);
    ctx.console.write_stdout(&format!("{command}\n"));
    match ctx.host.wait(pid) {
        Ok(outcome) => {
            if let Some(status) = outcome.terminal() {
                ctx.jobs.remove(id);
                status.code()
            } else {
                ctx.jobs.set_state(pid, JobState::Stopped);
                ctx.console
                    .write_stdout(&format!("[{}] Stopped {command}\n", id.as_u32()));
                USAGE_ERROR
            }
        }
        Err(err) => {
            ctx.console.write_stderr(&format!("fg: {err}\n"));
            USAGE_ERROR
        }
    }
}

fn bg(ctx: &mut BuiltinContext<'_>, args: &[String]) -> i32 {
    let Some(id) = resolve_job(ctx, args) else {
        ctx.console.write_stderr("bg: no such job\n");
        return USAGE_ERROR;
    };
    let Some(job) = ctx.jobs.by_id(id) else {
        ctx.console.write_stderr("bg: no such job\n");
        return USAGE_ERROR;
    };
    let pid = job.pid;
    let command = job.command.clone();
    if ctx.host.signal(pid, Signal::Continue).is_err() {
        ctx.console.write_stderr("bg: cannot resume job\n");
        return USAGE_ERROR;
    }
    ctx.jobs.set_state(pid, JobState::Running);
    ctx.console
        .write_stdout(&format!("[{}] {command} &\n", id.as_u32()));
    OK
}

fn help(ctx: &mut BuiltinContext<'_>) -> i32 {
    ctx.console
        .write_stdout("builtins: cd pwd exit export unset echo jobs fg bg ulimit help\n");
    OK
}

#[cfg(test)]
mod tests {
    use super::{is_builtin, BuiltinContext};
    use crate::env::Environment;
    use crate::job::{JobState, JobTable, Pid, Signal, WaitOutcome};
    use crate::test_support::{MemoryLimitStore, RecordingConsole, ScriptedHost};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    struct Fixture {
        env: Environment,
        jobs: JobTable,
        host: ScriptedHost,
        console: RecordingConsole,
        limits: MemoryLimitStore,
        exit: Option<i32>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                env: Environment::new(),
                jobs: JobTable::new(),
                host: ScriptedHost::new(),
                console: RecordingConsole::new(),
                limits: MemoryLimitStore::new(),
                exit: None,
            }
        }

        fn run(&mut self, words: &[&str]) -> Option<i32> {
            let mut ctx = BuiltinContext {
                env: &mut self.env,
                jobs: &mut self.jobs,
                host: &self.host,
                console: &self.console,
                limits: &self.limits,
                exit: &mut self.exit,
            };
            super::dispatch(&mut ctx, &argv(words))
        }
    }

    #[test]
    fn unknown_command_is_not_a_builtin() {
        assert!(!is_builtin("ls"));
        let mut fx = Fixture::new();
        assert_eq!(fx.run(&["ls", "-l"]), None);
    }

    #[test]
    fn echo_joins_args_and_honours_dash_n() {
        let mut fx = Fixture::new();
        assert_eq!(fx.run(&["echo", "hello", "world"]), Some(0));
        assert_eq!(fx.console.stdout(), "hello world\n");
        fx.console.clear();
        assert_eq!(fx.run(&["echo", "-n", "no-newline"]), Some(0));
        assert_eq!(fx.console.stdout(), "no-newline");
    }

    #[test]
    fn cd_updates_cwd_via_host_and_reports_errors() {
        let mut fx = Fixture::new();
        fx.host.set_directory("/Users/ada");
        assert_eq!(fx.run(&["cd", "/Users/ada"]), Some(0));
        assert_eq!(fx.env.cwd(), "/Users/ada");
        // An unknown directory fails closed without changing cwd.
        assert_eq!(fx.run(&["cd", "/nope"]), Some(1));
        assert_eq!(fx.env.cwd(), "/Users/ada");
        assert!(fx.console.stderr().contains("cd: /nope"));
    }

    #[test]
    fn pwd_prints_cwd() {
        let mut fx = Fixture::new();
        fx.env.set_cwd("/Apps");
        assert_eq!(fx.run(&["pwd"]), Some(0));
        assert_eq!(fx.console.stdout(), "/Apps\n");
    }

    #[test]
    fn export_sets_lists_and_validates() {
        let mut fx = Fixture::new();
        assert_eq!(fx.run(&["export", "PATH=/Apps"]), Some(0));
        assert_eq!(fx.env.get("PATH"), Some("/Apps"));
        assert_eq!(fx.env.exported_vars(), [("PATH", "/Apps")]);
        // Listing.
        fx.console.clear();
        assert_eq!(fx.run(&["export"]), Some(0));
        assert_eq!(fx.console.stdout(), "export PATH=/Apps\n");
        // Invalid identifier.
        assert_eq!(fx.run(&["export", "1BAD=x"]), Some(1));
    }

    #[test]
    fn unset_removes_and_validates() {
        let mut fx = Fixture::new();
        fx.env.set("X", "1");
        assert_eq!(fx.run(&["unset", "X"]), Some(0));
        assert_eq!(fx.env.get("X"), None);
        assert_eq!(fx.run(&["unset", "bad-name"]), Some(1));
    }

    #[test]
    fn exit_records_request_and_code() {
        let mut fx = Fixture::new();
        fx.env.set_last_status(7);
        assert_eq!(fx.run(&["exit"]), Some(7));
        assert_eq!(fx.exit, Some(7));
        assert_eq!(fx.run(&["exit", "3"]), Some(3));
        assert_eq!(fx.exit, Some(3));
        assert_eq!(fx.run(&["exit", "notnum"]), Some(255));
    }

    #[test]
    fn jobs_lists_tracked_jobs() {
        let mut fx = Fixture::new();
        fx.jobs.add(Pid::new(10), "sleep 1", JobState::Running);
        fx.jobs.add(Pid::new(11), "sleep 2", JobState::Stopped);
        assert_eq!(fx.run(&["jobs"]), Some(0));
        assert_eq!(
            fx.console.stdout(),
            "[1] Running sleep 1\n[2] Stopped sleep 2\n"
        );
    }

    #[test]
    fn bg_resumes_and_marks_running() {
        let mut fx = Fixture::new();
        let id = fx.jobs.add(Pid::new(20), "build", JobState::Stopped);
        assert_eq!(fx.run(&["bg", "%1"]), Some(0));
        assert_eq!(
            fx.host.last_signal(),
            Some((Pid::new(20), Signal::Continue))
        );
        assert_eq!(fx.jobs.by_id(id).map(|j| j.state), Some(JobState::Running));
        assert_eq!(fx.console.stdout(), "[1] build &\n");
    }

    #[test]
    fn fg_resumes_waits_and_reaps_on_exit() {
        let mut fx = Fixture::new();
        let id = fx.jobs.add(Pid::new(30), "make", JobState::Stopped);
        fx.host.set_wait(Pid::new(30), WaitOutcome::Exited(0));
        assert_eq!(fx.run(&["fg"]), Some(0));
        assert!(fx.jobs.by_id(id).is_none());
        assert!(fx.console.stdout().contains("make\n"));
    }

    #[test]
    fn fg_keeps_job_when_it_stops_again() {
        let mut fx = Fixture::new();
        let id = fx.jobs.add(Pid::new(31), "vi", JobState::Running);
        fx.host.set_wait(Pid::new(31), WaitOutcome::Stopped(19));
        let status = fx.run(&["fg", "1"]);
        assert_eq!(status, Some(1));
        assert_eq!(fx.jobs.by_id(id).map(|j| j.state), Some(JobState::Stopped));
    }

    #[test]
    fn fg_and_bg_reject_missing_jobs() {
        let mut fx = Fixture::new();
        assert_eq!(fx.run(&["fg", "%9"]), Some(1));
        assert_eq!(fx.run(&["bg"]), Some(1));
    }

    #[test]
    fn help_lists_builtins() {
        let mut fx = Fixture::new();
        assert_eq!(fx.run(&["help"]), Some(0));
        assert!(fx.console.stdout().contains("cd"));
        assert!(fx.console.stdout().contains("help"));
    }
}
