//! Unit tests for command authentication and decode.

use tairix_abi::switchboard_ipc::{CommandSection, SwitchboardCommand};
use tairix_abi::{
    CapabilitySummary, Errno, Origin, ProcId, TrustDomain, ORIGIN_WIRE_LEN, PROC_ID_LEN,
};

use super::authenticate_command;

fn origin_bytes(proc_id: ProcId) -> [u8; ORIGIN_WIRE_LEN] {
    Origin::new(
        TrustDomain::User,
        1000,
        1000,
        7,
        proc_id,
        CapabilitySummary::EMPTY,
        0,
    )
    .to_le_bytes()
}

#[test]
fn a_command_from_the_session_decodes() {
    let session = ProcId::from_raw([9; PROC_ID_LEN]);
    let sender = origin_bytes(session);
    let frame = SwitchboardCommand::OpenPanel {
        section: CommandSection::Overview,
    }
    .to_le_bytes();
    let command = authenticate_command(&frame, &sender, session).expect("authenticated");
    assert_eq!(
        command,
        SwitchboardCommand::OpenPanel {
            section: CommandSection::Overview
        }
    );
}

#[test]
fn a_command_from_a_non_session_origin_is_dropped() {
    let session = ProcId::from_raw([9; PROC_ID_LEN]);
    let stranger = ProcId::from_raw([2; PROC_ID_LEN]);
    let sender = origin_bytes(stranger);
    let frame = SwitchboardCommand::OpenPanel {
        section: CommandSection::Tasks,
    }
    .to_le_bytes();
    assert_eq!(
        authenticate_command(&frame, &sender, session),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn a_malformed_sender_record_is_dropped() {
    let session = ProcId::from_raw([9; PROC_ID_LEN]);
    let sender = [0u8; ORIGIN_WIRE_LEN];
    let frame = SwitchboardCommand::OpenPanel {
        section: CommandSection::Tasks,
    }
    .to_le_bytes();
    // The all-zero sender decodes as the kernel sentinel, never the session.
    assert_eq!(
        authenticate_command(&frame, &sender, session),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn a_malformed_command_frame_is_refused_even_from_the_session() {
    let session = ProcId::from_raw([9; PROC_ID_LEN]);
    let sender = origin_bytes(session);
    let bad = [0xffu8; SwitchboardCommand::WIRE_LEN];
    assert_eq!(
        authenticate_command(&bad, &sender, session),
        Err(Errno::BadMagic)
    );
}

#[test]
fn a_too_short_frame_is_refused() {
    let session = ProcId::from_raw([9; PROC_ID_LEN]);
    let sender = origin_bytes(session);
    assert_eq!(
        authenticate_command(&[], &sender, session),
        Err(Errno::BufferTooSmall)
    );
}
