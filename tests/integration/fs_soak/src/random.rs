//! Randomized, model-checked filesystem soak.
//!
//! Where [`crate::exercise()`] drives one *fixed* operation sequence per
//! iteration (only the content bytes vary by seed), this body drives a
//! genuinely **randomized** operation mix: each step the RNG picks one of
//! create-file / create-dir / write / append / extend / truncate-grow /
//! truncate-shrink / remove / logical-move / read-verify, and every
//! choice (which file, which directory, the offset, the length, the
//! bytes) is drawn from the run's seed. The run-to-run *path* therefore
//! differs whenever the start seed differs (the runner draws it from
//! platform entropy, [`crate::registry`]), so the filesystem is exercised
//! "in a different manner" on every launch rather than replaying one PRNG
//! trace.
//!
//! Correctness is checked against an in-memory **oracle model**: a map
//! from path to the exact bytes the file should hold, plus the set of
//! directories that should exist. Every mutation is mirrored into the
//! model and the filesystem's result is asserted against it; the soak
//! periodically remounts and re-verifies that *every* file's bytes,
//! every file's size, and every directory's listing match the model
//! byte-for-byte. A mismatch — or an unexpected driver error — fails the
//! soak, tagged with the reproducing seed. The body is
//! generic over [`SoakFs`] so it adds no parallel re-implementation of
//! any filesystem semantics.

use std::collections::{BTreeMap, BTreeSet};

use rustos_abi::driver::filesystem::{FilesystemRead, NodeId, NodeKind};
use rustos_abi::DriverError;

use crate::{RamBlock, SoakFs};

/// Largest a single soak file is grown to, in bytes (64 KiB). Bounding
/// file size keeps the byte-exact oracle's memory modest while still
/// crossing the drivers' multi-block / extent paths many times over.
const MAX_FILE_BYTES: usize = 64 * 1024;

/// Most files the model keeps live at once. New `create` choices stop
/// once the population reaches this bound, so the run churns the same
/// working set (create/remove/move) rather than only ever growing.
const MAX_FILES: usize = 48;

/// Most directories (excluding the root) the model keeps live at once.
const MAX_DIRS: usize = 24;

/// Mutating operations performed per [`random_exercise`] call before it
/// returns. The runner repeats the call with a fresh seed until the
/// wall-clock budget elapses, so this only bounds one iteration.
const OPS_PER_ITERATION: u32 = 3000;

/// Run a full remount + whole-volume re-verify every this many
/// operations, proving committed state survives a fresh `open()`.
const REMOUNT_EVERY: u32 = 500;

/// A small, deterministic `SplitMix64` PRNG. Given the same seed it
/// reproduces the same run, so a failure replays from its tagged seed;
/// the *start* seed is what the runner randomizes per launch.
struct Rng {
    state: u64,
}

impl Rng {
    /// Seed the generator.
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next 64-bit value (`SplitMix64`).
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n` (returns `0` when `n == 0`).
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        // `% n` keeps the result < n, so the narrowing to `usize` is safe;
        // `try_from` avoids a lint-tripping `as` cast.
        usize::try_from(self.next_u64() % (n as u64)).unwrap_or(0)
    }

    /// `true` with probability `1 / n` (and never when `n == 0`).
    fn one_in(&mut self, n: usize) -> bool {
        n != 0 && self.below(n) == 0
    }

    /// Fill `buf` with pseudo-random bytes.
    fn fill(&mut self, buf: &mut [u8]) {
        for byte in buf.iter_mut() {
            *byte = self.next_u64().to_le_bytes()[0];
        }
    }
}

/// The oracle: what the filesystem *should* contain. `dirs` always holds
/// the root (the empty path); `files` maps each file's path to the exact
/// bytes it must read back as. Paths are `/`-joined component strings
/// relative to the root.
struct Model {
    dirs: BTreeSet<String>,
    files: BTreeMap<String, Vec<u8>>,
}

impl Model {
    /// A fresh model holding only the (empty) root directory.
    fn new() -> Self {
        let mut dirs = BTreeSet::new();
        dirs.insert(String::new());
        Self {
            dirs,
            files: BTreeMap::new(),
        }
    }

    /// Children (file and sub-directory leaf names) the model records
    /// directly under directory `dir`.
    fn children_of(&self, dir: &str) -> Vec<String> {
        let mut out = Vec::new();
        for path in self.dirs.iter().chain(self.files.keys()) {
            if path.is_empty() {
                continue;
            }
            let (parent, name) = split_parent(path);
            if parent == dir {
                out.push(name.to_string());
            }
        }
        out
    }

    /// `true` if `dir` has no children recorded (so its `rmdir` should
    /// succeed rather than report `Busy`).
    fn dir_is_empty(&self, dir: &str) -> bool {
        self.children_of(dir).is_empty()
    }
}

/// Split a non-empty path into `(parent_dir, leaf_name)`. The root's
/// children have an empty parent.
fn split_parent(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Join a directory path and a leaf name into a child path.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Map a driver error into a seed-tagged soak failure.
fn ck<T>(r: Result<T, DriverError>, what: &str, seed: u64) -> Result<T, String> {
    r.map_err(|e| format!("seed {seed:#x}: {what}: unexpected {e:?}"))
}

/// Assert an operation failed with exactly `want`.
fn want_err(
    got: Option<DriverError>,
    want: DriverError,
    what: &str,
    seed: u64,
) -> Result<(), String> {
    match got {
        Some(e) if e == want => Ok(()),
        Some(e) => Err(format!(
            "seed {seed:#x}: {what}: expected {want:?}, got {e:?}"
        )),
        None => Err(format!("seed {seed:#x}: {what}: expected {want:?}, got Ok")),
    }
}

/// Resolve a directory path to its live [`NodeId`] by walking `lookup`
/// from the root.
fn resolve_dir<F: FilesystemRead>(fs: &mut F, path: &str, seed: u64) -> Result<NodeId, String> {
    let mut node = fs.root();
    if path.is_empty() {
        return Ok(node);
    }
    for comp in path.split('/') {
        node = ck(
            fs.lookup(node, comp.as_bytes()),
            "resolve dir component",
            seed,
        )?;
    }
    Ok(node)
}

/// Read the whole of file `node` (known `len`) into a buffer.
fn read_all<F: FilesystemRead>(
    fs: &mut F,
    node: NodeId,
    len: usize,
    what: &str,
    seed: u64,
) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; len];
    let mut done = 0usize;
    while done < len {
        let n = ck(fs.read_at(node, done as u64, &mut out[done..]), what, seed)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    out.truncate(done);
    Ok(out)
}

/// Collect the entry names a directory lists, skipping the `.`/`..`
/// self/parent links a driver may surface.
fn list_names<F: FilesystemRead>(
    fs: &mut F,
    dir: NodeId,
    seed: u64,
) -> Result<Vec<Vec<u8>>, String> {
    let mut names = Vec::new();
    let mut index = 0u64;
    let mut buf = [0u8; 256];
    while let Some(entry) = ck(fs.read_dir(dir, index, &mut buf), "read_dir", seed)? {
        let name = &buf[..entry.name_len];
        if name != b"." && name != b".." {
            names.push(name.to_vec());
        }
        index += 1;
        if index > 10_000_000 {
            return Err(format!("seed {seed:#x}: read_dir did not terminate"));
        }
    }
    Ok(names)
}

/// A printable, model-unique leaf name from `[a-z0-9_]`. Uniqueness is
/// the caller's job (it retries on collision).
fn random_name(rng: &mut Rng) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let len = 1 + rng.below(12);
    let mut name = String::with_capacity(len);
    for _ in 0..len {
        let idx = rng.below(ALPHABET.len());
        name.push(char::from(ALPHABET[idx]));
    }
    // Never collide with the self/parent links.
    if name == "." || name == ".." {
        name.push('_');
    }
    name
}

/// Pick a fresh leaf name for a new child of `dir` that no model entry
/// already uses, or `None` after a few attempts (the directory is busy).
fn fresh_name(rng: &mut Rng, model: &Model, dir: &str) -> Option<String> {
    for _ in 0..8 {
        let name = random_name(rng);
        let path = join(dir, &name);
        if !model.files.contains_key(&path) && !model.dirs.contains(&path) {
            return Some(name);
        }
    }
    None
}

/// Choose a random existing directory path (including the root).
fn pick_dir(rng: &mut Rng, model: &Model) -> String {
    let n = model.dirs.len();
    let idx = rng.below(n);
    model.dirs.iter().nth(idx).cloned().unwrap_or_default()
}

/// Choose a random existing file path, or `None` when none exist.
fn pick_file(rng: &mut Rng, model: &Model) -> Option<String> {
    let n = model.files.len();
    if n == 0 {
        return None;
    }
    let idx = rng.below(n);
    model.files.keys().nth(idx).cloned()
}

/// Non-root directory paths the model records, optionally filtered to the
/// empty ones (whose `rmdir` should succeed).
fn dirs_matching(model: &Model, want_empty: bool) -> Vec<String> {
    model
        .dirs
        .iter()
        .filter(|d| !d.is_empty() && model.dir_is_empty(d) == want_empty)
        .cloned()
        .collect()
}

/// Create a fresh empty file in a random directory (bounded by
/// [`MAX_FILES`]).
fn op_create_file<F: SoakFs>(
    fs: &mut F,
    model: &mut Model,
    rng: &mut Rng,
    seed: u64,
) -> Result<(), String> {
    if model.files.len() >= MAX_FILES {
        return Ok(());
    }
    let dir = pick_dir(rng, model);
    let Some(name) = fresh_name(rng, model, &dir) else {
        return Ok(());
    };
    let node = resolve_dir(fs, &dir, seed)?;
    ck(
        fs.create(node, name.as_bytes(), NodeKind::RegularFile),
        "create file",
        seed,
    )?;
    model.files.insert(join(&dir, &name), Vec::new());
    Ok(())
}

/// Create a fresh sub-directory in a random directory (bounded by
/// [`MAX_DIRS`]).
fn op_create_dir<F: SoakFs>(
    fs: &mut F,
    model: &mut Model,
    rng: &mut Rng,
    seed: u64,
) -> Result<(), String> {
    // `dirs` includes the root, so the live sub-directory count is one
    // less than its length.
    if model.dirs.len().saturating_sub(1) >= MAX_DIRS {
        return Ok(());
    }
    let dir = pick_dir(rng, model);
    let Some(name) = fresh_name(rng, model, &dir) else {
        return Ok(());
    };
    let node = resolve_dir(fs, &dir, seed)?;
    ck(
        fs.create(node, name.as_bytes(), NodeKind::Directory),
        "create dir",
        seed,
    )?;
    model.dirs.insert(join(&dir, &name));
    Ok(())
}

/// Write at a random offset — overwriting, appending, or (past the end)
/// extending with a zero-filled gap — and mirror the bytes into the
/// model.
fn op_write<F: SoakFs>(
    fs: &mut F,
    model: &mut Model,
    rng: &mut Rng,
    seed: u64,
) -> Result<(), String> {
    let Some(path) = pick_file(rng, model) else {
        return Ok(());
    };
    let len = model.files.get(&path).map_or(0, Vec::len);

    let offset = match rng.below(10) {
        0..=5 => rng.below(len + 1), // overwrite within / at end
        6 | 7 => len,                // append exactly at the end
        _ => len + rng.below(4096),  // extend, leaving a zero gap
    };
    if offset >= MAX_FILE_BYTES {
        return Ok(());
    }
    let room = MAX_FILE_BYTES - offset;
    let data_len = 1 + rng.below(room.min(8192));
    let mut data = vec![0u8; data_len];
    rng.fill(&mut data);

    let (dir, name) = split_parent(&path);
    let node = resolve_dir(fs, dir, seed)?;
    let n = ck(
        fs.write_at(node, name.as_bytes(), offset as u64, &data),
        "write_at",
        seed,
    )?;
    if n != data_len {
        return Err(format!(
            "seed {seed:#x}: write_at short write: {n} of {data_len}"
        ));
    }

    let content = model
        .files
        .get_mut(&path)
        .ok_or_else(|| format!("seed {seed:#x}: model lost file {path}"))?;
    let new_len = content.len().max(offset + data_len);
    content.resize(new_len, 0);
    content[offset..offset + data_len].copy_from_slice(&data);
    Ok(())
}

/// Grow (zero-extend) or shrink a random file via `truncate`, mirroring
/// the new length into the model.
fn op_truncate<F: SoakFs>(
    fs: &mut F,
    model: &mut Model,
    rng: &mut Rng,
    seed: u64,
) -> Result<(), String> {
    let Some(path) = pick_file(rng, model) else {
        return Ok(());
    };
    let len = model.files.get(&path).map_or(0, Vec::len);
    let new_size = if rng.one_in(2) {
        len + rng.below(MAX_FILE_BYTES - len + 1) // grow (zero-extend)
    } else {
        rng.below(len + 1) // shrink
    };

    let (dir, name) = split_parent(&path);
    let node = resolve_dir(fs, dir, seed)?;
    ck(
        fs.truncate(node, name.as_bytes(), new_size as u64),
        "truncate",
        seed,
    )?;
    if let Some(content) = model.files.get_mut(&path) {
        content.resize(new_size, 0);
    }
    Ok(())
}

/// Remove a random file and confirm it then resolves as `NotFound`.
fn op_remove_file<F: SoakFs>(
    fs: &mut F,
    model: &mut Model,
    rng: &mut Rng,
    seed: u64,
) -> Result<(), String> {
    let Some(path) = pick_file(rng, model) else {
        return Ok(());
    };
    let (dir, name) = split_parent(&path);
    let node = resolve_dir(fs, dir, seed)?;
    ck(fs.remove(node, name.as_bytes()), "remove file", seed)?;
    model.files.remove(&path);
    want_err(
        fs.lookup(node, name.as_bytes()).err(),
        DriverError::NotFound,
        "lookup after remove",
        seed,
    )
}

/// Remove a random *empty* sub-directory and confirm it then resolves as
/// `NotFound`.
fn op_remove_dir<F: SoakFs>(
    fs: &mut F,
    model: &mut Model,
    rng: &mut Rng,
    seed: u64,
) -> Result<(), String> {
    let candidates = dirs_matching(model, true);
    if candidates.is_empty() {
        return Ok(());
    }
    let path = candidates[rng.below(candidates.len())].clone();
    let (dir, name) = split_parent(&path);
    let node = resolve_dir(fs, dir, seed)?;
    ck(fs.remove(node, name.as_bytes()), "remove dir", seed)?;
    model.dirs.remove(&path);
    want_err(
        fs.lookup(node, name.as_bytes()).err(),
        DriverError::NotFound,
        "lookup after rmdir",
        seed,
    )
}

/// Logically move a random file to a fresh name in a random directory by
/// copying its bytes through the read/write ABI (there is no native
/// rename in `abi-v1`), then unlinking the source. The bytes must survive
/// the move unchanged.
fn op_move<F: SoakFs>(
    fs: &mut F,
    model: &mut Model,
    rng: &mut Rng,
    seed: u64,
) -> Result<(), String> {
    let Some(src) = pick_file(rng, model) else {
        return Ok(());
    };
    let dst_dir = pick_dir(rng, model);
    let Some(dst_name) = fresh_name(rng, model, &dst_dir) else {
        return Ok(());
    };
    let dst = join(&dst_dir, &dst_name);

    let (src_parent, src_leaf) = split_parent(&src);
    let src_parent_node = resolve_dir(fs, src_parent, seed)?;
    let src_node = ck(
        fs.lookup(src_parent_node, src_leaf.as_bytes()),
        "lookup move src",
        seed,
    )?;
    let expected = model
        .files
        .get(&src)
        .cloned()
        .ok_or_else(|| format!("seed {seed:#x}: model lost move src {src}"))?;
    let bytes = read_all(fs, src_node, expected.len(), "read move src", seed)?;
    if bytes != expected {
        return Err(format!(
            "seed {seed:#x}: move src content mismatch for {src}"
        ));
    }

    let dst_parent_node = resolve_dir(fs, &dst_dir, seed)?;
    ck(
        fs.create(dst_parent_node, dst_name.as_bytes(), NodeKind::RegularFile),
        "create move dst",
        seed,
    )?;
    if !bytes.is_empty() {
        let n = ck(
            fs.write_at(dst_parent_node, dst_name.as_bytes(), 0, &bytes),
            "write move dst",
            seed,
        )?;
        if n != bytes.len() {
            return Err(format!(
                "seed {seed:#x}: move dst short write: {n} of {}",
                bytes.len()
            ));
        }
    }
    ck(
        fs.remove(src_parent_node, src_leaf.as_bytes()),
        "remove move src",
        seed,
    )?;

    model.files.remove(&src);
    model.files.insert(dst, bytes);
    Ok(())
}

/// Read a random file back and assert it matches the model byte-for-byte.
fn op_read_verify<F: SoakFs>(
    fs: &mut F,
    model: &mut Model,
    rng: &mut Rng,
    seed: u64,
) -> Result<(), String> {
    let Some(path) = pick_file(rng, model) else {
        return Ok(());
    };
    let expected = model.files.get(&path).cloned().unwrap_or_default();
    let (dir, name) = split_parent(&path);
    let node = resolve_dir(fs, dir, seed)?;
    let file = ck(fs.lookup(node, name.as_bytes()), "lookup verify", seed)?;
    let info = ck(fs.node_info(file), "node_info verify", seed)?;
    if info.size != expected.len() as u64 {
        return Err(format!(
            "seed {seed:#x}: {path}: size {} != model {}",
            info.size,
            expected.len()
        ));
    }
    let back = read_all(fs, file, expected.len(), "read verify", seed)?;
    if back != expected {
        return Err(format!("seed {seed:#x}: {path}: content mismatch"));
    }
    Ok(())
}

/// Exercise a fail-closed extreme that must *not* mutate state: a
/// duplicate create, a bad name, a missing target, or a non-empty
/// `rmdir`. The model is left untouched.
fn op_negative<F: SoakFs>(
    fs: &mut F,
    model: &Model,
    rng: &mut Rng,
    seed: u64,
) -> Result<(), String> {
    let root = fs.root();
    match rng.below(5) {
        0 => {
            // Duplicate create over an existing file → Busy.
            let Some(path) = pick_file(rng, model) else {
                return Ok(());
            };
            let (dir, name) = split_parent(&path);
            let node = resolve_dir(fs, dir, seed)?;
            want_err(
                fs.create(node, name.as_bytes(), NodeKind::RegularFile)
                    .err(),
                DriverError::Busy,
                "duplicate create",
                seed,
            )
        }
        1 => want_err(
            fs.create(root, b"", NodeKind::RegularFile).err(),
            DriverError::LengthOutOfRange,
            "empty name",
            seed,
        ),
        2 => {
            // 256 bytes — one past the 255-byte component limit.
            let oversize = vec![b'x'; 256];
            want_err(
                fs.create(root, &oversize, NodeKind::RegularFile).err(),
                DriverError::LengthOutOfRange,
                "oversize name",
                seed,
            )
        }
        3 => want_err(
            fs.remove(root, b"__definitely_absent_entry__").err(),
            DriverError::NotFound,
            "remove absent",
            seed,
        ),
        _ => {
            // Non-empty rmdir → Busy.
            let candidates = dirs_matching(model, false);
            if candidates.is_empty() {
                return Ok(());
            }
            let path = &candidates[rng.below(candidates.len())];
            let (dir, name) = split_parent(path);
            let node = resolve_dir(fs, dir, seed)?;
            want_err(
                fs.remove(node, name.as_bytes()).err(),
                DriverError::Busy,
                "remove non-empty dir",
                seed,
            )
        }
    }
}

/// Re-verify the *entire* volume against the model: every directory
/// exists with exactly the children the model records, and every file
/// reads back at the right size with the right bytes.
fn verify_all<F: SoakFs>(fs: &mut F, model: &Model, seed: u64) -> Result<(), String> {
    for dir in &model.dirs {
        let node = resolve_dir(fs, dir, seed)?;
        let info = ck(fs.node_info(node), "node_info dir", seed)?;
        if info.kind != NodeKind::Directory {
            return Err(format!(
                "seed {seed:#x}: '{dir}' is not a directory ({:?})",
                info.kind
            ));
        }
        let mut listed: BTreeSet<Vec<u8>> = list_names(fs, node, seed)?.into_iter().collect();
        let mut expected: BTreeSet<Vec<u8>> = model
            .children_of(dir)
            .into_iter()
            .map(String::into_bytes)
            .collect();
        if listed != expected {
            // Symmetric difference makes the discrepancy legible.
            let only_fs: Vec<_> = listed
                .difference(&expected)
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect();
            let only_model: Vec<_> = expected
                .difference(&listed)
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect();
            listed.clear();
            expected.clear();
            return Err(format!(
                "seed {seed:#x}: '{dir}' listing mismatch: only-on-fs {only_fs:?}, only-in-model {only_model:?}"
            ));
        }
    }
    for (path, expected) in &model.files {
        let (dir, name) = split_parent(path);
        let node = resolve_dir(fs, dir, seed)?;
        let file = ck(fs.lookup(node, name.as_bytes()), "lookup verify-all", seed)?;
        let info = ck(fs.node_info(file), "node_info verify-all", seed)?;
        if info.size != expected.len() as u64 {
            return Err(format!(
                "seed {seed:#x}: {path}: size {} != model {} after remount",
                info.size,
                expected.len()
            ));
        }
        let back = read_all(fs, file, expected.len(), "read verify-all", seed)?;
        if &back != expected {
            return Err(format!(
                "seed {seed:#x}: {path}: content mismatch after remount"
            ));
        }
    }
    Ok(())
}

/// Run one randomized soak iteration on a fresh `device_bytes` volume,
/// driven entirely by `seed`.
///
/// The body formats the volume, then performs `OPS_PER_ITERATION`
/// randomly-chosen mutations (interspersed with read-back checks and
/// fail-closed negative probes), remounting and re-verifying the whole
/// volume every `REMOUNT_EVERY` operations and once more at the end.
///
/// # Errors
/// Returns a descriptive, seed-tagged error on any driver failure or any
/// divergence from the oracle model.
pub fn random_exercise<F: SoakFs>(device_bytes: u64, seed: u64) -> Result<(), String> {
    let block = RamBlock::new(device_bytes);
    let mut fs = ck(F::format_volume(block), "format", seed)?;
    let mut model = Model::new();
    let mut rng = Rng::new(seed);

    for step in 0..OPS_PER_ITERATION {
        match rng.below(100) {
            0..=14 => op_create_file(&mut fs, &mut model, &mut rng, seed)?,
            15..=20 => op_create_dir(&mut fs, &mut model, &mut rng, seed)?,
            21..=50 => op_write(&mut fs, &mut model, &mut rng, seed)?,
            51..=63 => op_truncate(&mut fs, &mut model, &mut rng, seed)?,
            64..=73 => op_remove_file(&mut fs, &mut model, &mut rng, seed)?,
            74..=77 => op_remove_dir(&mut fs, &mut model, &mut rng, seed)?,
            78..=85 => op_move(&mut fs, &mut model, &mut rng, seed)?,
            _ => op_read_verify(&mut fs, &mut model, &mut rng, seed)?,
        }

        // Sprinkle in a fail-closed negative probe.
        if rng.one_in(9) {
            op_negative(&mut fs, &model, &mut rng, seed)?;
        }

        // Periodically flush, remount, and re-verify the whole volume.
        if step != 0 && step % REMOUNT_EVERY == 0 {
            ck(fs.flush(), "flush before remount", seed)?;
            fs = ck(fs.remount(), "remount", seed)?;
            verify_all(&mut fs, &model, seed)?;
        }
    }

    ck(fs.flush(), "final flush", seed)?;
    let mut fs = ck(fs.remount(), "final remount", seed)?;
    verify_all(&mut fs, &model, seed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{fresh_name, join, random_name, split_parent, Model, Rng};

    #[test]
    fn rng_is_deterministic_from_its_seed() {
        let mut a = Rng::new(0x1234_5678);
        let mut b = Rng::new(0x1234_5678);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        // Overwhelmingly likely to differ in the first few draws.
        let diverged = (0..8).any(|_| a.next_u64() != b.next_u64());
        assert!(diverged);
    }

    #[test]
    fn below_stays_in_range_and_handles_zero() {
        let mut rng = Rng::new(99);
        assert_eq!(rng.below(0), 0);
        for _ in 0..1000 {
            assert!(rng.below(7) < 7);
        }
    }

    #[test]
    fn fill_writes_every_byte() {
        let mut rng = Rng::new(7);
        let mut buf = [0u8; 64];
        rng.fill(&mut buf);
        // Not a strict requirement, but a 64-byte all-zero fill from this
        // PRNG would signal a broken generator.
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn split_parent_separates_leaf_from_directory() {
        assert_eq!(split_parent("alpha"), ("", "alpha"));
        assert_eq!(split_parent("a/b"), ("a", "b"));
        assert_eq!(split_parent("a/b/c"), ("a/b", "c"));
    }

    #[test]
    fn join_round_trips_through_split() {
        for (dir, name) in [("", "x"), ("a", "y"), ("a/b", "z")] {
            let path = join(dir, name);
            let (p, n) = split_parent(&path);
            assert_eq!((p, n), (dir, name));
        }
    }

    #[test]
    fn random_name_is_non_empty_bounded_and_not_a_dotlink() {
        let mut rng = Rng::new(0xDEAD_BEEF);
        for _ in 0..2000 {
            let name = random_name(&mut rng);
            assert!(!name.is_empty());
            assert!(name.len() <= 13);
            assert_ne!(name, ".");
            assert_ne!(name, "..");
            assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        }
    }

    #[test]
    fn model_tracks_children_and_emptiness() {
        let mut model = Model::new();
        assert!(model.dir_is_empty(""));
        model.dirs.insert("sub".to_string());
        model.files.insert("sub/file".to_string(), vec![1, 2, 3]);
        model.files.insert("root_file".to_string(), Vec::new());

        let mut root_children = model.children_of("");
        root_children.sort();
        assert_eq!(root_children, vec!["root_file", "sub"]);
        assert_eq!(model.children_of("sub"), vec!["file"]);

        assert!(!model.dir_is_empty(""));
        assert!(!model.dir_is_empty("sub"));
    }

    #[test]
    fn fresh_name_avoids_existing_children() {
        let mut model = Model::new();
        let mut rng = Rng::new(5);
        for _ in 0..40 {
            let name = fresh_name(&mut rng, &model, "").expect("a free name exists");
            let path = join("", &name);
            assert!(!model.files.contains_key(&path));
            assert!(!model.dirs.contains(&path));
            model.files.insert(path, Vec::new());
        }
    }
}
