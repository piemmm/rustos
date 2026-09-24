//! The `Run` entry-point binary of the `dns-sd` tool.
//!
//! `main` parses the argument vector, reads the `LANG` locale preference, and
//! runs the parsed command against the production seams: one
//! `tairix_discovery::RtDiscovery` session parked on its doorbell between
//! answers, the bundle's own help, and the inherited standard streams.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::format;

    use tairix_abi::discovery_ipc::{Entry, Query};
    use tairix_abi::Errno;
    use tairix_discovery::{DiscoveryError, RtDiscovery, Waited};
    use tairix_dns_sd::{parse, run, Command, Discovery, Output, USAGE};
    use tairix_help::BundleHelp;
    use tairix_rt::io::{write_stderr_line, Stderr, Stdout, Write};

    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stdout.write_all(bytes).map_err(|_| Errno::BrokenPipe)
        }
    }

    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::BrokenPipe)
        }
    }

    /// The discovery service, reached lazily: help needs no session.
    struct RtService(Option<RtDiscovery>);

    impl Discovery for RtService {
        fn start(&mut self, query: &Query<'_>) -> Result<u32, DiscoveryError> {
            let discovery = match &mut self.0 {
                Some(discovery) => discovery,
                none => none.insert(RtDiscovery::open()?),
            };
            discovery.session().start(query)
        }

        fn wait(
            &mut self,
            until: Option<u64>,
            each: &mut dyn FnMut(&Entry<'_>),
        ) -> Result<Waited, DiscoveryError> {
            self.0
                .as_mut()
                .ok_or(DiscoveryError::Unavailable)?
                .wait(until, each)
        }

        fn now(&self) -> u64 {
            tairix_rt::clock_get()
        }
    }

    /// Program entry point. Exit codes: `0` when the command ran its course
    /// or its help was written, `1` when the discovery service refused the
    /// request or is not running, and `2` on a usage error or an output
    /// failure.
    fn main() -> i32 {
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("dns-sd: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let help = BundleHelp::new("dns-sd");
        let asks = matches!(command, Command::Ask { .. });
        let mut service = RtService(None);
        match run(command, locale, &mut service, &help, &RtOutput, &RtErrors) {
            Ok(status) => status,
            Err(err) => {
                if asks {
                    write_stderr_line(&format!("dns-sd: cannot write output: {err}"));
                }
                2
            }
        }
    }

    tairix_rt::entry!(main);
}

#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
