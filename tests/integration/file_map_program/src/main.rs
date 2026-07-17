//! M1 demand-paged file-mapping fixture: a minimal, separately-linked
//! pure-Rust EL0 program built once and driven in four argv-selected roles.
//!
//! The consuming verticals (`tests/integration/file_map_qemu_aarch64` /
//! `…_riscv64`) register this one `rxe` under four registry paths, each with
//! a distinct role argument (`tairix_rt::arg(1)`), and spawn the **parent**
//! role through the production spawn path
//! (`docs/src/architecture/memory.md` §7o):
//!
//! * **`parent`** — spawns the three child roles below through the
//!   production `spawn` syscall and reaps each through the production
//!   blocking `wait`, asserting `verify` exits `0` and `wild` / `store`
//!   are fault-killed with exit code 139 (`128 + SIGSEGV`). Exits `0` only
//!   when all three children behaved.
//! * **`verify`** — opens the fixture file, `file_map`s it whole, closes the
//!   descriptor (the mapping's identity snapshot must survive the close),
//!   demand-faults the first byte, an interior page, and the end-of-file
//!   straddle page (asserting the file bytes and the zero fill past end of
//!   file), passes an **untouched** mapped page as a syscall buffer (the
//!   `fs_open` path string — the copy-path fault-resolution proof), unmaps,
//!   and exits `0`. Every failure exits with a distinct diagnostic code.
//! * **`wild`** — maps and unmaps the fixture, then reads the unmapped base:
//!   the access must fault-kill the task (exit 139 observed by the parent).
//!   Surviving the access is a distinct non-zero exit.
//! * **`store`** — maps the fixture, demand-faults the first page resident,
//!   then stores to it: file mappings are read-only, so the write fault must
//!   kill the task (exit 139), never spin retrying a resident-page write.
//!
//! The consuming vertical's build script is the single source of truth for
//! the fixture geometry: it computes the fixture bytes once and pins the
//! file length, path, probe offsets, and expected probe bytes into this
//! build through the `TAIRIX_FM_*` environment variables, so the program
//! and the kernel-side filesystem double can never disagree.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt` (`_start`, stack canary, panic handler, syscall wrappers),
//! never the C ABI. Built position-independent and converted to an `rxe`
//! blob by the consuming test's build script. On the host it is an inert
//! stub so `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::OpenFlags;

    /// Page size shared by every Tier-1 MMU target this fixture runs on.
    const PAGE: u64 = 4096;

    /// Exit code the kernel records for a fault-killed task
    /// (`128 + SIGSEGV`), which the parent expects from `wild` and `store`.
    const FAULT_EXIT_CODE: i32 = 139;

    /// Fixture file length in bytes, pinned by the consuming build script.
    const FILE_LEN: u64 = parse_u64(req_env(option_env!("TAIRIX_FM_FILE_LEN")));

    /// Absolute path the filesystem double serves the fixture file under.
    const FILE_PATH: &[u8] = req_env(option_env!("TAIRIX_FM_PATH"));

    /// Byte offset of the copy of [`FILE_PATH`] inside the fixture content
    /// (page-aligned, so the whole string lives in one untouched page).
    const PATH_OFFSET: u64 = parse_u64(req_env(option_env!("TAIRIX_FM_PATH_OFFSET")));

    /// Interior-page probe offset (inside a page that is neither the first
    /// nor the end-of-file straddle page).
    const INTERIOR_OFFSET: u64 = parse_u64(req_env(option_env!("TAIRIX_FM_INTERIOR_OFFSET")));

    /// Expected fixture byte at offset `0`.
    const BYTE_FIRST: u8 = parse_u64(req_env(option_env!("TAIRIX_FM_BYTE_FIRST"))) as u8;

    /// Expected fixture byte at [`INTERIOR_OFFSET`].
    const BYTE_INTERIOR: u8 = parse_u64(req_env(option_env!("TAIRIX_FM_BYTE_INTERIOR"))) as u8;

    /// Expected fixture byte at `FILE_LEN - 1` (the straddle page).
    const BYTE_LAST: u8 = parse_u64(req_env(option_env!("TAIRIX_FM_BYTE_LAST"))) as u8;

    /// The pinned environment variable's bytes. An absent variable is a
    /// build wiring defect — the consuming vertical's build script is the
    /// single source of the fixture geometry — so fail the build loudly
    /// rather than bake a default that could silently diverge from the
    /// kernel-side fixture.
    const fn req_env(value: Option<&'static str>) -> &'static [u8] {
        match value {
            Some(s) => s.as_bytes(),
            None => panic!("TAIRIX_FM_* geometry must be pinned by the consuming build script"),
        }
    }

    /// Parse `bytes` as a non-negative decimal `u64` at compile time. Any
    /// non-digit byte, empty string, or overflow is a build wiring defect;
    /// fail the build loudly rather than bake a wrong geometry.
    const fn parse_u64(bytes: &[u8]) -> u64 {
        assert!(!bytes.is_empty(), "TAIRIX_FM_* value must be non-empty");
        let mut acc: u64 = 0;
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            assert!(b >= b'0' && b <= b'9', "TAIRIX_FM_* value must be decimal");
            acc = match acc.checked_mul(10) {
                Some(v) => match v.checked_add((b - b'0') as u64) {
                    Some(v) => v,
                    None => panic!("TAIRIX_FM_* value overflows u64"),
                },
                None => panic!("TAIRIX_FM_* value overflows u64"),
            };
            i += 1;
        }
        acc
    }

    /// Read one mapped byte through a volatile load, so the demand fault the
    /// probe exists to trigger is never optimised away.
    ///
    /// # Safety
    ///
    /// `va` must lie inside a region `file_map` returned and that has not
    /// been unmapped (the `wild` role violates this deliberately — that read
    /// is the point of the role and is expected to fault-kill the task).
    unsafe fn probe(va: u64) -> u8 {
        // SAFETY: the caller upholds the contract above.
        unsafe { (va as *const u8).read_volatile() }
    }

    /// The `verify` role body. Returns `0` on success or a distinct
    /// diagnostic exit code per failure site.
    fn verify() -> i32 {
        let fd = tairix_rt::fs_open(FILE_PATH, OpenFlags::READ);
        if fd < 0 {
            return 20;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let fd = fd as u32;
        let base = tairix_rt::file_map(fd, 0, FILE_LEN);
        if base <= 0 {
            return 21;
        }
        #[allow(clippy::cast_sign_loss)]
        let base = base as u64;
        // The mapping's identity snapshot must survive the descriptor: close
        // it before any page is faulted in.
        if tairix_rt::fs_close(fd) != 0 {
            return 22;
        }
        // First byte (the region's first page).
        // SAFETY: `base` maps `FILE_LEN` bytes and offset 0 is inside it.
        if unsafe { probe(base) } != BYTE_FIRST {
            return 23;
        }
        // An interior page.
        // SAFETY: `INTERIOR_OFFSET < FILE_LEN` (pinned by the build script).
        if unsafe { probe(base + INTERIOR_OFFSET) } != BYTE_INTERIOR {
            return 24;
        }
        // The end-of-file straddle page: the last file byte, and the zero
        // fill past end of file inside the same mapped page.
        // SAFETY: `FILE_LEN - 1` is the last mapped file byte.
        if unsafe { probe(base + FILE_LEN - 1) } != BYTE_LAST {
            return 25;
        }
        let straddle_pages = FILE_LEN.div_ceil(PAGE);
        let past_eof = FILE_LEN; // First byte past end of file.
        if past_eof < straddle_pages * PAGE {
            // SAFETY: `past_eof` is inside the last mapped page (the page is
            // whole even though the file ends inside it).
            if unsafe { probe(base + past_eof) } != 0 {
                return 26;
            }
        }
        // Copy-path proof: hand the kernel an **untouched** mapped page as a
        // syscall buffer. The fixture carries a copy of its own path at
        // `PATH_OFFSET` (page-aligned, not probed above), so `fs_open` must
        // resolve the copy-in fault against the live file region and read
        // the path out of the demand-paged bytes.
        let path_in_map =
            // SAFETY: `[PATH_OFFSET, PATH_OFFSET + FILE_PATH.len())` lies
            // wholly inside the mapping (pinned by the build script) and the
            // page has not been touched, so the kernel copy-in takes the
            // fault this probe exists to prove resolvable.
            unsafe { core::slice::from_raw_parts((base + PATH_OFFSET) as *const u8, FILE_PATH.len()) };
        let fd2 = tairix_rt::fs_open(path_in_map, OpenFlags::READ);
        if fd2 < 0 {
            return 27;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        if tairix_rt::fs_close(fd2 as u32) != 0 {
            return 28;
        }
        if tairix_rt::file_unmap(base, FILE_LEN) != 0 {
            return 29;
        }
        0
    }

    /// The `wild` role body: map, unmap, then read the torn-down base. The
    /// read must fault-kill the task (exit 139); every return here is a
    /// distinct failure the parent will surface.
    fn wild() -> i32 {
        let fd = tairix_rt::fs_open(FILE_PATH, OpenFlags::READ);
        if fd < 0 {
            return 40;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let base = tairix_rt::file_map(fd as u32, 0, FILE_LEN);
        if base <= 0 {
            return 41;
        }
        #[allow(clippy::cast_sign_loss)]
        let base = base as u64;
        if tairix_rt::file_unmap(base, FILE_LEN) != 0 {
            return 42;
        }
        // SAFETY contract deliberately violated: the region was just
        // unmapped, so this read must take an unresolvable fault and the
        // kernel must kill the task. Surviving it is the defect this role
        // exists to catch.
        let _ = unsafe { probe(base) };
        43
    }

    /// The `store` role body: map, fault the first page resident, then store
    /// to it. File mappings are read-only, so the write must fault-kill the
    /// task (exit 139) — never resolve, and never spin retrying.
    fn store() -> i32 {
        let fd = tairix_rt::fs_open(FILE_PATH, OpenFlags::READ);
        if fd < 0 {
            return 50;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let base = tairix_rt::file_map(fd as u32, 0, FILE_LEN);
        if base <= 0 {
            return 51;
        }
        #[allow(clippy::cast_sign_loss)]
        let base = base as u64;
        // Fault the page resident first, so the store below exercises the
        // resident-page write-fault gate, not the not-yet-backed path.
        // SAFETY: offset 0 is inside the live mapping.
        if unsafe { probe(base) } != BYTE_FIRST {
            return 52;
        }
        // SAFETY contract deliberately violated: the mapping is read-only,
        // so this store must take a write fault the kernel refuses to
        // resolve, killing the task. Surviving it is the defect this role
        // exists to catch.
        unsafe { (base as *mut u8).write_volatile(0xEE) };
        53
    }

    /// Spawn the child registered at `path` and reap it, asserting it exited
    /// with `expected`. Returns `0` on success or `fail_code` on any
    /// mismatch or syscall failure.
    fn run_child(path: &[u8], expected: i32, fail_code: i32) -> i32 {
        let pid = tairix_rt::spawn(path);
        if pid <= 0 {
            return fail_code;
        }
        #[allow(clippy::cast_possible_truncation)]
        let pid = pid as i32;
        let mut code = 0i32;
        if tairix_rt::wait_exit(pid, &mut code) < 0 {
            return fail_code + 1;
        }
        if code != expected {
            return fail_code + 2;
        }
        0
    }

    /// The `parent` role body: drive the three children through the
    /// production spawn + wait path and assert their exit codes.
    fn parent() -> i32 {
        let failed = run_child(b"/bin/fm-verify", 0, 10);
        if failed != 0 {
            return failed;
        }
        let failed = run_child(b"/bin/fm-wild", FAULT_EXIT_CODE, 13);
        if failed != 0 {
            return failed;
        }
        let failed = run_child(b"/bin/fm-store", FAULT_EXIT_CODE, 16);
        if failed != 0 {
            return failed;
        }
        0
    }

    /// Program entry point: dispatch on the role argument the registry entry
    /// pinned (`arg(1)`). An absent or unknown role is a wiring defect and a
    /// distinct failure code (fail closed, never a default role).
    fn main() -> i32 {
        match tairix_rt::arg(1) {
            Some(b"parent") => parent(),
            Some(b"verify") => verify(),
            Some(b"wild") => wild(),
            Some(b"store") => store(),
            _ => 5,
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the
// crate building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
