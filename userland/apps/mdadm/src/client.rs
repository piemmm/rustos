//! The `mdadm` engine: dispatch a parsed [`Command`] to the reads, the
//! mutations, and the rendered output.
//!
//! Every syscall is behind the [`Reader`] / [`Controller`] / [`Output`] seams
//! (`io`), so the whole engine — reads, name resolution, control-frame build,
//! reply decode, rendering, and the fd-3 advisories — is host-tested against
//! in-memory fixtures. The authority is the composer's: it checks the caller's
//! capability against the kernel-attested origin and the engine reports what it
//! answers; the engine holds no ambient authority and re-checks nothing.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::raid::ArrayHealth;
use tairix_abi::raid_admin::{
    decode_create_reply, RaidControlOp, RAID_CONTROL_MAX_REQUEST, RAID_CREATE_REPLY_LEN,
};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};

use crate::command::{Command, CreateArgs};
use crate::error::MdadmError;
use crate::io::{Controller, Output, Reader};
use crate::render::{
    format_identity, render_array_detail, render_detail, render_examine, render_version,
};
use crate::resolve::{resolve_array, resolve_device, resolve_members};

/// The one-line usage banner, printed on a usage error and as the fallback
/// when the bundled help document is unavailable.
pub const USAGE: &str = "usage: mdadm {--create --level=<L> --raid-devices=<n> [--chunk=<blocks>] \
     <device>... | --detail [<array>] | --examine | --add <array> <device> | \
     --remove <array> <device> | --stop <array>}";

/// The command word this bundle is named by, for the own-help lookup and the
/// `stdinfo` `producer`.
const OWN_WORD: &str = "mdadm";

/// Run a parsed `mdadm` command against the injected seams.
///
/// # Errors
///
/// An [`MdadmError`]: a denied capability, a name that did not resolve, a
/// service or composer refusal, or an output failure.
pub fn run(
    command: Command,
    locale: Option<&str>,
    reader: &dyn Reader,
    controller: &dyn Controller,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), MdadmError> {
    match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(MdadmError::Output)
        }
        Command::Version => write_line(out, &render_version()),
        Command::Detail { array } => run_detail(array.as_deref(), reader, out),
        Command::Examine => run_examine(reader, out),
        Command::Create(args) => run_create(&args, controller, out),
        Command::Add { array, device } => {
            run_manage(&array, &device, reader, controller, out, Manage::Add)
        }
        Command::Remove { array, device } => {
            run_manage(&array, &device, reader, controller, out, Manage::Remove)
        }
        Command::Stop { array } => run_stop(&array, reader, controller, out),
    }
}

/// Which member mutation `run_manage` performs.
#[derive(Copy, Clone, Eq, PartialEq)]
enum Manage {
    Add,
    Remove,
}

/// `--detail`: report one array (a resolved identity) or every array.
fn run_detail(
    array: Option<&str>,
    reader: &dyn Reader,
    out: &dyn Output,
) -> Result<(), MdadmError> {
    let arrays = reader.arrays().map_err(read_error)?;
    if let Some(name) = array {
        let uuid = resolve_array(&arrays, name)?;
        let record = arrays
            .iter()
            .find(|candidate| candidate.array() == uuid)
            .ok_or(MdadmError::Service(Errno::NotFound))?;
        write_lines(out, &render_array_detail(record))?;
        if record.health() != ArrayHealth::Optimal {
            emit_redundancy_summary(out, 1);
        }
        return Ok(());
    }

    write_lines(out, &render_detail(&arrays))?;
    if arrays.is_empty() {
        emit_context(out, "raid.no_arrays", "No RAID arrays are configured.");
    } else {
        let degraded = arrays
            .iter()
            .filter(|array| array.health() != ArrayHealth::Optimal)
            .count();
        if degraded > 0 {
            emit_redundancy_summary(out, degraded);
        }
    }
    // The array view omits the blank candidate devices; note how many exist so
    // a reader knows to look with `--examine`. Best-effort: fd 3 is advisory,
    // so a failed second read simply omits the note.
    if let Ok(members) = reader.members() {
        let blanks = members.iter().filter(|m| m.is_unaffiliated()).count();
        if blanks > 0 {
            emit_blank_omission(out, blanks);
        }
    }
    Ok(())
}

/// `--examine`: list every device the composer holds.
fn run_examine(reader: &dyn Reader, out: &dyn Output) -> Result<(), MdadmError> {
    let members = reader.members().map_err(read_error)?;
    write_lines(out, &render_examine(&members))?;
    if members.is_empty() {
        emit_context(
            out,
            "raid.no_devices",
            "No storage devices are held by the array composer.",
        );
    }
    Ok(())
}

/// `--create`: compose an array over the named devices and print the identity
/// the composer minted.
fn run_create(
    args: &CreateArgs,
    controller: &dyn Controller,
    out: &dyn Output,
) -> Result<(), MdadmError> {
    let members = resolve_members(&args.devices)?;
    let op = RaidControlOp::Create {
        level: args.level,
        chunk_blocks: args.chunk_blocks.unwrap_or(0),
        members,
    };
    let reply = submit(controller, &op)?;
    let uuid = decode_create_reply(&reply).map_err(reply_error)?;
    write_line(out, &format!("Created array {}", format_identity(&uuid)))
}

/// `--add` / `--remove`: admit or retire a device on a resolved array.
fn run_manage(
    array: &str,
    device: &str,
    reader: &dyn Reader,
    controller: &dyn Controller,
    out: &dyn Output,
    which: Manage,
) -> Result<(), MdadmError> {
    let arrays = reader.arrays().map_err(read_error)?;
    let uuid = resolve_array(&arrays, array)?;
    let node = resolve_device(device)?;
    let op = match which {
        Manage::Add => RaidControlOp::Add { array: uuid, node },
        Manage::Remove => RaidControlOp::Remove { array: uuid, node },
    };
    let reply = submit(controller, &op)?;
    decode_status_reply(&reply).map_err(reply_error)?;
    let verb = match which {
        Manage::Add => "Added",
        Manage::Remove => "Removed",
    };
    write_line(
        out,
        &format!("{verb} node:{node} on array {}", format_identity(&uuid)),
    )
}

/// `--stop`: retire a resolved array.
fn run_stop(
    array: &str,
    reader: &dyn Reader,
    controller: &dyn Controller,
    out: &dyn Output,
) -> Result<(), MdadmError> {
    let arrays = reader.arrays().map_err(read_error)?;
    let uuid = resolve_array(&arrays, array)?;
    let op = RaidControlOp::Stop { array: uuid };
    let reply = submit(controller, &op)?;
    decode_status_reply(&reply).map_err(reply_error)?;
    write_line(out, &format!("Stopped array {}", format_identity(&uuid)))
}

/// Encode `op`, post it to the control endpoint, and return the reply bytes.
fn submit(controller: &dyn Controller, op: &RaidControlOp) -> Result<Vec<u8>, MdadmError> {
    let mut request = [0u8; RAID_CONTROL_MAX_REQUEST];
    let len = op.encode(&mut request).map_err(MdadmError::Encode)?;
    let mut reply = [0u8; RAID_CREATE_REPLY_LEN];
    let written = controller
        .call(&request[..len], &mut reply)
        .map_err(call_error)?;
    Ok(reply[..written.min(reply.len())].to_vec())
}

/// Map a read query's [`Errno`]: a refused `CAP_SYSINFO_HW` is the denied
/// read; anything else is a service error.
fn read_error(errno: Errno) -> MdadmError {
    if errno == Errno::PermissionDenied {
        MdadmError::ReadDenied
    } else {
        MdadmError::Service(errno)
    }
}

/// Map a control transport [`Errno`]: a refused `CAP_STORAGE_ADMIN` is the
/// denied mutation; anything else (no composer, malformed) is a service error.
fn call_error(errno: Errno) -> MdadmError {
    if errno == Errno::PermissionDenied {
        MdadmError::AdminDenied
    } else {
        MdadmError::Service(errno)
    }
}

/// Map a decoded reply's [`Errno`]: a refused `CAP_STORAGE_ADMIN` is the
/// denied mutation; anything else is the composer's typed refusal.
fn reply_error(errno: Errno) -> MdadmError {
    if errno == Errno::PermissionDenied {
        MdadmError::AdminDenied
    } else {
        MdadmError::Refused(errno)
    }
}

/// Write each line, each terminated by a newline.
fn write_lines(out: &dyn Output, lines: &[String]) -> Result<(), MdadmError> {
    for line in lines {
        write_line(out, line)?;
    }
    Ok(())
}

/// Write one line terminated by a newline.
fn write_line(out: &dyn Output, line: &str) -> Result<(), MdadmError> {
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    out.write_all(buf.as_bytes()).map_err(MdadmError::Output)
}

/// Emit a bare `context` advisory (no suggestion, empty `ai`) on fd 3.
fn emit_context(out: &dyn Output, code: &str, message: &str) {
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Context,
        code,
        Severity::Info,
        Human::message(message),
    );
    emit(out, &record);
}

/// Emit the `raid.redundancy_reduced` summary: an aggregate count of arrays
/// whose redundancy is reduced (a next-action pointer, not a restatement of
/// each array's state, which is already on stdout).
fn emit_redundancy_summary(out: &dyn Output, count: usize) {
    let message = if count == 1 {
        String::from("1 array has reduced redundancy.")
    } else {
        format!("{count} arrays have reduced redundancy.")
    };
    let ai = format!(
        "{{\"subject\":\"raid_arrays\",\"degraded_count\":{count},\
         \"suggestion\":{{\"argv\":[\"mdadm\",\"--examine\"],\
         \"safe_to_autorun\":false,\"requires_confirmation\":true}}}}"
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Summary,
        "raid.redundancy_reduced",
        Severity::Info,
        Human::with_suggestion(&message, "Inspect the devices with `mdadm --examine`."),
    )
    .with_ai(&ai);
    emit(out, &record);
}

/// Emit the `raid.blank_devices_omitted` advisory: the array-focused detail
/// view does not list the unaffiliated candidate devices, so note how many
/// exist and where to see them.
fn emit_blank_omission(out: &dyn Output, count: usize) {
    let message = if count == 1 {
        String::from("1 blank device not shown.")
    } else {
        format!("{count} blank devices not shown.")
    };
    let ai = format!(
        "{{\"subject\":\"raid_arrays\",\
         \"omission\":{{\"reason\":\"not_in_array_view\",\"entry_class\":\"blank_device\",\
         \"omitted_count\":{count},\"stdout_is_exhaustive\":false}},\
         \"suggestion\":{{\"argv\":[\"mdadm\",\"--examine\"],\
         \"safe_to_autorun\":false,\"requires_confirmation\":true}}}}"
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Omission,
        "raid.blank_devices_omitted",
        Severity::Info,
        Human::with_suggestion(&message, "List every device with `mdadm --examine`."),
    )
    .with_ai(&ai);
    emit(out, &record);
}

/// Serialise a record to one JSONL line and hand it to the fd-3 writer,
/// best-effort: an over-long record (never expected from these fixed shapes)
/// is dropped rather than affecting the report.
fn emit(out: &dyn Output, record: &StdInfoRecord<'_>) {
    let mut buf = [0u8; 512];
    if let Ok(len) = record.write_jsonl(&mut buf) {
        out.info(&buf[..len]);
    }
}

#[cfg(test)]
mod tests;
