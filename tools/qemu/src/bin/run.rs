//! Thin CLI wrapper around [`rustos_qemu::Runner`].
//!
//! Intended for manual debugging — `cargo xtask test --qemu` drives the
//! library directly and never shells out to this binary. Keeping the wrapper
//! in-tree means a developer chasing a flake can reproduce the *exact* QEMU
//! invocation the test runner used, without having to read xtask source.
//!
//! Usage:
//! ```text
//! cargo run -p rustos-qemu --bin rustos-qemu-run -- \
//!     --kernel path/to/kernel.elf [--cpus N] [--timeout-secs S] \
//!     [--virtio-blk path/to/disk.img ...]
//! ```

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use rustos_qemu::{Arch, Outcome, Runner, Spec};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let mut kernel: Option<PathBuf> = None;
    let mut cpus: u32 = 1;
    let mut timeout = Duration::from_secs(30);
    let mut block_images: Vec<PathBuf> = Vec::new();

    while let Some(a) = args.next() {
        match a.as_str() {
            "--kernel" => kernel = args.next().map(PathBuf::from),
            "--cpus" => {
                let Some(v) = args.next() else {
                    return usage_err("--cpus needs a value");
                };
                let Ok(n) = v.parse::<u32>() else {
                    return usage_err("--cpus must be a u32");
                };
                cpus = n.max(1);
            }
            "--timeout-secs" => {
                let Some(v) = args.next() else {
                    return usage_err("--timeout-secs needs a value");
                };
                let Ok(n) = v.parse::<u64>() else {
                    return usage_err("--timeout-secs must be a u64");
                };
                timeout = Duration::from_secs(n);
            }
            "--virtio-blk" => {
                let Some(v) = args.next() else {
                    return usage_err("--virtio-blk needs a path");
                };
                block_images.push(PathBuf::from(v));
            }
            "-h" | "--help" => {
                println!("{}", usage());
                return ExitCode::SUCCESS;
            }
            other => return usage_err(&format!("unknown argument `{other}`")),
        }
    }

    let Some(kernel) = kernel else {
        return usage_err("--kernel is required");
    };

    let mut spec = Spec::for_x86_64_kernel(kernel)
        .with_cpus(cpus)
        .with_timeout(timeout);
    for image in block_images {
        spec = spec.with_virtio_blk(image);
    }
    let _ = Arch::X86_64; // tie-down — the wrapper is currently x86_64-only

    match Runner::run(&spec) {
        Ok(Outcome::Pass) => ExitCode::SUCCESS,
        Ok(Outcome::Fail { status, serial }) => {
            eprintln!("rustos-qemu-run: FAIL (qemu status {status})");
            eprint!("{serial}");
            ExitCode::from(1)
        }
        Ok(Outcome::Timeout { budget, serial }) => {
            eprintln!("rustos-qemu-run: TIMEOUT after {budget:?}");
            eprint!("{serial}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("rustos-qemu-run: could not spawn QEMU: {e}");
            ExitCode::from(3)
        }
    }
}

fn usage() -> &'static str {
    "usage: rustos-qemu-run --kernel <path> [--cpus N] [--timeout-secs S] \
[--virtio-blk <image> ...]"
}

fn usage_err(msg: &str) -> ExitCode {
    eprintln!("rustos-qemu-run: {msg}\n{}", usage());
    ExitCode::from(2)
}
