//! The worker role's program half (`plans/STRESSTEST.md` §7.2): the
//! syscall-backed [`Scratch`] and the per-kind load loops.
//!
//! A worker runs its bounded unit in an endless loop until the controller
//! ends it (`Terminate`, then `Kill` after the grace deadline — the
//! default disposition, no intake opt-in: the *controller* is the run's
//! signal observer). The load loops are deliberately tight — the workers
//! **are** the load — and they are ordinary preemptible user tasks. A
//! typed refusal is reported once on stderr and exits
//! [`REFUSED_EXIT`]; any other failure states its reason and exits 1
//! (fail loud, never silent).

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::{Errno, OpenFlags};
use rustos_rt::io::write_stderr_line;
use rustos_stress::{
    cache_unit, cpu_unit, hdd_unit, io_unit, scratch_file, vm_unit, Scratch, ScratchError,
    UnitOutcome, WorkerKind, WorkerSpec, REFUSED_EXIT,
};

extern crate alloc;

/// Iterations of one CPU unit: large enough that the loop is genuinely
/// CPU-bound between unit boundaries, small enough that a unit is
/// milliseconds.
const CPU_UNIT_ITERATIONS: u64 = 4_000_000;

/// Run the worker described by `spec` until terminated. Returns the
/// worker's exit code on a refusal or failure (the load loops themselves
/// never end on their own).
pub fn run(spec: &WorkerSpec) -> i32 {
    match spec.kind {
        WorkerKind::Cpu => {
            let mut seed = u64::from(spec.index).wrapping_add(1);
            loop {
                seed = core::hint::black_box(cpu_unit(seed, CPU_UNIT_ITERATIONS));
            }
        }
        WorkerKind::Vm => {
            let mut buf = Vec::new();
            let mut pass = 0u64;
            loop {
                match vm_unit(&mut buf, spec.bytes, pass) {
                    UnitOutcome::Done => pass = pass.wrapping_add(1),
                    UnitOutcome::Refused(reason) => return refuse(spec, reason),
                    UnitOutcome::Failed(reason) => return fail(spec, reason),
                }
            }
        }
        WorkerKind::Io | WorkerKind::Hdd | WorkerKind::Cache => {
            let mut fs = RtScratch::new();
            let path = scratch_file(&spec.scratch, spec.kind, spec.index);
            loop {
                let outcome = match spec.kind {
                    WorkerKind::Io => io_unit(&mut fs, &path, spec.bytes),
                    WorkerKind::Hdd => hdd_unit(&mut fs, &path, spec.bytes),
                    _ => cache_unit(&mut fs, &spec.scratch, spec.index, spec.bytes),
                };
                match outcome {
                    UnitOutcome::Done => {}
                    UnitOutcome::Refused(reason) => return refuse(spec, reason),
                    UnitOutcome::Failed(reason) => return fail(spec, reason),
                }
            }
        }
    }
}

/// Report a typed refusal once and choose the refusal exit status.
fn refuse(spec: &WorkerSpec, reason: &str) -> i32 {
    write_stderr_line(&alloc::format!(
        "stress: {} worker {}: refused: {reason}",
        spec.kind.as_str(),
        spec.index
    ));
    REFUSED_EXIT
}

/// Report a genuine failure once and fail the worker.
fn fail(spec: &WorkerSpec, reason: &str) -> i32 {
    write_stderr_line(&alloc::format!(
        "stress: {} worker {}: failed: {reason}",
        spec.kind.as_str(),
        spec.index
    ));
    1
}

/// The syscall-backed [`Scratch`]: opens each file once and reuses the
/// descriptor across the unit's operations (a one-slot cache — every unit
/// works one file at a time), classifying each refused syscall into the
/// typed refusal/failure split.
struct RtScratch {
    open: Option<(String, u32)>,
}

impl RtScratch {
    const fn new() -> Self {
        Self { open: None }
    }

    /// The open descriptor for `path`, opening (and caching) it with
    /// `flags` on a miss.
    fn fd(&mut self, path: &str, flags: OpenFlags) -> Result<u32, ScratchError> {
        if let Some((cached, fd)) = &self.open {
            if cached == path {
                return Ok(*fd);
            }
            let (_, stale) = self.open.take().unwrap_or((String::new(), 0));
            let _ = rustos_rt::fs_close(stale);
        }
        let ret = rustos_rt::fs_open(path.as_bytes(), flags);
        if ret < 0 {
            return Err(classify(ret));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // A non-negative open result is a descriptor the kernel bounded to u32.
        let fd = ret as u32;
        self.open = Some((String::from(path), fd));
        Ok(fd)
    }

    /// Drop the cached descriptor (before an unlink of its file).
    fn close_cached(&mut self, path: &str) {
        if let Some((cached, fd)) = &self.open {
            if cached == path {
                let fd = *fd;
                self.open = None;
                let _ = rustos_rt::fs_close(fd);
            }
        }
    }
}

impl Scratch for RtScratch {
    fn create(&mut self, path: &str) -> Result<(), ScratchError> {
        self.close_cached(path);
        let flags = OpenFlags::READ
            .union(OpenFlags::WRITE)
            .union(OpenFlags::CREATE)
            .union(OpenFlags::TRUNCATE);
        self.fd(path, flags).map(|_| ())
    }

    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), ScratchError> {
        let fd = self.fd(path, OpenFlags::READ.union(OpenFlags::WRITE))?;
        let mut written = 0;
        while written < data.len() {
            match rustos_rt::fs_write(fd, offset + written as u64, &data[written..]) {
                Ok(0) => return Err(ScratchError::Failed("write made no progress")),
                Ok(n) => written += n,
                Err(ret) => return Err(classify(ret)),
            }
        }
        Ok(())
    }

    fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<(), ScratchError> {
        let fd = self.fd(path, OpenFlags::READ.union(OpenFlags::WRITE))?;
        let mut done = 0;
        while done < buf.len() {
            match rustos_rt::fs_read(fd, offset + done as u64, &mut buf[done..]) {
                Ok(0) => return Err(ScratchError::Failed("short read")),
                Ok(n) => done += n,
                Err(ret) => return Err(classify(ret)),
            }
        }
        Ok(())
    }

    fn sync(&mut self, path: &str) -> Result<(), ScratchError> {
        let fd = self.fd(path, OpenFlags::READ.union(OpenFlags::WRITE))?;
        let ret = rustos_rt::fs_sync(fd);
        if ret < 0 {
            return Err(classify(ret));
        }
        Ok(())
    }

    fn remove(&mut self, path: &str) -> Result<(), ScratchError> {
        self.close_cached(path);
        let ret = rustos_rt::fs_unlink(path.as_bytes(), rustos_abi::UnlinkFlags::empty());
        if ret < 0 {
            return Err(classify(ret));
        }
        Ok(())
    }
}

/// Classify a negative syscall result: the resource refusals every load
/// test expects under pressure are typed [`ScratchError::Refused`];
/// anything else is a genuine failure that fails the run.
fn classify(ret: i64) -> ScratchError {
    #[allow(clippy::cast_possible_truncation)] // The kernel encodes -errno in i32 range.
    let errno = -(ret as i32);
    if errno == Errno::NoSpace.as_i32() {
        ScratchError::Refused("no space left on the scratch volume")
    } else if errno == Errno::PermissionDenied.as_i32() {
        ScratchError::Refused("permission denied on the scratch path")
    } else if errno == Errno::OutOfRange.as_i32() {
        ScratchError::Refused("a resource limit was reached")
    } else {
        ScratchError::Failed("scratch I/O failed")
    }
}
