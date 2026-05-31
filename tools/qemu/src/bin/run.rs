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
//!     --kernel path/to/kernel.elf [--arch x86_64|riscv64] [--cpus N] \
//!     [--timeout-secs S] [--virtio-blk path/to/disk.img ...] \
//!     [--virtio-net] [--virtio-net-pcap path/to/capture.pcap] [--ramfb]
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
    let mut virtio_net = false;
    let mut net_pcap: Option<PathBuf> = None;
    let mut arch = Arch::X86_64;
    let mut ramfb = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--kernel" => kernel = args.next().map(PathBuf::from),
            "--arch" => {
                arch = match args.next().as_deref() {
                    Some("x86_64") => Arch::X86_64,
                    Some("riscv64") => Arch::Riscv64,
                    _ => return usage_err("--arch must be x86_64 or riscv64"),
                };
            }
            "--ramfb" => ramfb = true,
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
            "--virtio-net" => virtio_net = true,
            "--virtio-net-pcap" => {
                let Some(v) = args.next() else {
                    return usage_err("--virtio-net-pcap needs a path");
                };
                net_pcap = Some(PathBuf::from(v));
                virtio_net = true;
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

    let base = match arch {
        Arch::X86_64 => Spec::for_x86_64_kernel(kernel),
        Arch::Riscv64 => Spec::for_riscv64_kernel(kernel),
    };
    let mut spec = base.with_cpus(cpus).with_timeout(timeout);
    for image in block_images {
        spec = spec.with_virtio_blk(image);
    }
    match net_pcap {
        Some(pcap) => spec = spec.with_virtio_net_pcap(pcap),
        None if virtio_net => spec = spec.with_virtio_net(),
        None => {}
    }
    if ramfb {
        spec = spec.with_ramfb();
    }

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
    "usage: rustos-qemu-run --kernel <path> [--arch x86_64|riscv64] [--cpus N] \
[--timeout-secs S] [--virtio-blk <image> ...] [--virtio-net] \
[--virtio-net-pcap <path>] [--ramfb]"
}

fn usage_err(msg: &str) -> ExitCode {
    eprintln!("rustos-qemu-run: {msg}\n{}", usage());
    ExitCode::from(2)
}
