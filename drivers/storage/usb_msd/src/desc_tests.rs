//! Host tests for the configuration-descriptor reader: transport
//! classification (BOT / CBI / UAS), endpoint and pipe derivation, and
//! the fail-closed handling of hostile descriptor streams.

use super::*;
use alloc::vec::Vec;

/// Build a configuration stream from a header and raw descriptors,
/// patching `wTotalLength` to the real total.
fn stream(descriptors: &[&[u8]]) -> Vec<u8> {
    let mut out = alloc::vec![
        9,
        DESC_TYPE_CONFIGURATION,
        0,
        0,
        1,    // bNumInterfaces
        1,    // bConfigurationValue
        0,    // iConfiguration
        0x80, // bmAttributes
        50,   // bMaxPower
    ];
    for descriptor in descriptors {
        out.extend_from_slice(descriptor);
    }
    let total = u16::try_from(out.len()).expect("test stream fits u16");
    out[2..4].copy_from_slice(&total.to_le_bytes());
    out
}

fn interface(number: u8, class: [u8; 3]) -> [u8; 9] {
    [
        9,
        DESC_TYPE_INTERFACE,
        number,
        0, // bAlternateSetting
        2, // bNumEndpoints
        class[0],
        class[1],
        class[2],
        0, // iInterface
    ]
}

fn endpoint(address: u8, attributes: u8) -> [u8; 7] {
    [7, DESC_TYPE_ENDPOINT, address, attributes, 0, 2, 0]
}

fn pipe_usage(id: u8) -> [u8; 4] {
    [4, DESC_TYPE_PIPE_USAGE, id, 0]
}

const BOT_SCSI: [u8; 3] = [0x08, SUBCLASS_SCSI, PROTOCOL_BOT];
const BOT_UFI: [u8; 3] = [0x08, SUBCLASS_UFI, PROTOCOL_BOT];
const CBI_UFI: [u8; 3] = [0x08, SUBCLASS_UFI, PROTOCOL_CBI];
const UAS: [u8; 3] = [0x08, SUBCLASS_SCSI, PROTOCOL_UAS];

#[test]
fn finds_the_bulk_pair_of_a_bot_interface() {
    let bytes = stream(&[
        &interface(0, BOT_SCSI),
        &endpoint(0x83, 0x02), // bulk-IN, EP3
        &endpoint(0x04, 0x02), // bulk-OUT, EP4
    ]);
    assert_eq!(
        find_storage_interface(&bytes),
        Ok(StorageInterface {
            interface_number: 0,
            command_set: CommandSet::Transparent,
            protocol: StorageProtocol::Bot {
                bulk_in: 3,
                bulk_out: 4,
            },
        })
    );
}

#[test]
fn a_ufi_bot_floppy_reports_the_ufi_command_set() {
    let bytes = stream(&[
        &interface(0, BOT_UFI),
        &endpoint(0x81, 0x02),
        &endpoint(0x02, 0x02),
    ]);
    let found = find_storage_interface(&bytes).expect("servable");
    assert_eq!(found.command_set, CommandSet::Ufi);
    assert_eq!(
        found.protocol,
        StorageProtocol::Bot {
            bulk_in: 1,
            bulk_out: 2,
        }
    );
}

#[test]
fn a_cbi_floppy_needs_its_interrupt_endpoint() {
    // Without the interrupt endpoint the interface never completes.
    let bytes = stream(&[
        &interface(0, CBI_UFI),
        &endpoint(0x81, 0x02),
        &endpoint(0x02, 0x02),
    ]);
    assert_eq!(find_storage_interface(&bytes), Err(Errno::NotFound));

    // With it, all three endpoints are derived.
    let bytes = stream(&[
        &interface(0, CBI_UFI),
        &endpoint(0x81, 0x02),
        &endpoint(0x02, 0x02),
        &endpoint(0x83, 0x03), // interrupt-IN, EP3
    ]);
    assert_eq!(
        find_storage_interface(&bytes),
        Ok(StorageInterface {
            interface_number: 0,
            command_set: CommandSet::Ufi,
            protocol: StorageProtocol::Cbi {
                bulk_in: 1,
                bulk_out: 2,
                interrupt_in: 3,
            },
        })
    );
}

#[test]
fn uas_pipes_are_derived_from_the_pipe_usage_descriptors() {
    let bytes = stream(&[
        &interface(0, UAS),
        &endpoint(0x01, 0x02), // bulk-OUT EP1
        &pipe_usage(0x01),     // command
        &endpoint(0x82, 0x02), // bulk-IN EP2
        &pipe_usage(0x02),     // status
        &endpoint(0x83, 0x02), // bulk-IN EP3
        &pipe_usage(0x03),     // data-in
        &endpoint(0x04, 0x02), // bulk-OUT EP4
        &pipe_usage(0x04),     // data-out
    ]);
    assert_eq!(
        find_storage_interface(&bytes),
        Ok(StorageInterface {
            interface_number: 0,
            command_set: CommandSet::Transparent,
            protocol: StorageProtocol::Uas(UasEndpoints {
                command: 1,
                status: 2,
                data_in: 3,
                data_out: 4,
            }),
        })
    );
}

#[test]
fn uas_pipe_order_follows_the_descriptors_not_an_assumption() {
    // Devices may order the pipes differently; the ids decide.
    let bytes = stream(&[
        &interface(0, UAS),
        &endpoint(0x85, 0x02),
        &pipe_usage(0x03), // data-in on EP5
        &endpoint(0x06, 0x02),
        &pipe_usage(0x04), // data-out on EP6
        &endpoint(0x87, 0x02),
        &pipe_usage(0x02), // status on EP7
        &endpoint(0x08, 0x02),
        &pipe_usage(0x01), // command on EP8
    ]);
    let found = find_storage_interface(&bytes).expect("servable");
    assert_eq!(
        found.protocol,
        StorageProtocol::Uas(UasEndpoints {
            command: 8,
            status: 7,
            data_in: 5,
            data_out: 6,
        })
    );
}

#[test]
fn a_uas_pipe_on_the_wrong_direction_poisons_the_interface() {
    // The command pipe must ride an OUT endpoint; a later well-formed
    // sibling still serves.
    let bytes = stream(&[
        &interface(0, UAS),
        &endpoint(0x81, 0x02),
        &pipe_usage(0x01), // command on an IN endpoint: malformed
        &interface(1, BOT_SCSI),
        &endpoint(0x82, 0x02),
        &endpoint(0x03, 0x02),
    ]);
    let found = find_storage_interface(&bytes).expect("sibling serves");
    assert_eq!(found.interface_number, 1);
}

#[test]
fn a_duplicate_uas_pipe_id_poisons_the_interface() {
    let bytes = stream(&[
        &interface(0, UAS),
        &endpoint(0x01, 0x02),
        &pipe_usage(0x01),
        &endpoint(0x02, 0x02),
        &pipe_usage(0x01), // command named twice
        &endpoint(0x83, 0x02),
        &pipe_usage(0x02),
        &endpoint(0x84, 0x02),
        &pipe_usage(0x03),
    ]);
    assert_eq!(find_storage_interface(&bytes), Err(Errno::NotFound));
}

#[test]
fn unsupported_class_combinations_are_left_unserved() {
    // The interrupt-less CB variant (protocol 0x01) is not implemented.
    let bytes = stream(&[
        &interface(0, [0x08, SUBCLASS_UFI, 0x01]),
        &endpoint(0x81, 0x02),
        &endpoint(0x02, 0x02),
    ]);
    assert_eq!(find_storage_interface(&bytes), Err(Errno::NotFound));

    // An ATAPI sub-class over BOT is not implemented either.
    let bytes = stream(&[
        &interface(0, [0x08, 0x02, PROTOCOL_BOT]),
        &endpoint(0x81, 0x02),
        &endpoint(0x02, 0x02),
    ]);
    assert_eq!(find_storage_interface(&bytes), Err(Errno::NotFound));
}

#[test]
fn skips_foreign_interfaces_and_their_endpoints() {
    let bytes = stream(&[
        &interface(0, [0x03, 0x01, 0x01]), // HID boot keyboard
        &endpoint(0x81, 0x03),             // its interrupt-IN
        &interface(1, BOT_SCSI),
        &endpoint(0x02, 0x02), // bulk-OUT first
        &endpoint(0x85, 0x02), // bulk-IN second
    ]);
    assert_eq!(
        find_storage_interface(&bytes),
        Ok(StorageInterface {
            interface_number: 1,
            command_set: CommandSet::Transparent,
            protocol: StorageProtocol::Bot {
                bulk_in: 5,
                bulk_out: 2,
            },
        })
    );
}

#[test]
fn a_bot_interface_ignores_a_stray_interrupt_endpoint() {
    let bytes = stream(&[
        &interface(0, BOT_SCSI),
        &endpoint(0x81, 0x03), // interrupt-IN — not the data pair
        &endpoint(0x82, 0x02), // bulk-IN, EP2
        &endpoint(0x03, 0x02), // bulk-OUT, EP3
    ]);
    assert_eq!(
        find_storage_interface(&bytes),
        Ok(StorageInterface {
            interface_number: 0,
            command_set: CommandSet::Transparent,
            protocol: StorageProtocol::Bot {
                bulk_in: 2,
                bulk_out: 3,
            },
        })
    );
}

#[test]
fn refuses_a_stream_with_no_storage_interface() {
    let bytes = stream(&[&interface(0, [0x03, 0x01, 0x01]), &endpoint(0x81, 0x03)]);
    assert_eq!(find_storage_interface(&bytes), Err(Errno::NotFound));
}

#[test]
fn refuses_an_interface_missing_a_bulk_direction() {
    let bytes = stream(&[
        &interface(0, BOT_SCSI),
        &endpoint(0x81, 0x02), // bulk-IN only
    ]);
    assert_eq!(find_storage_interface(&bytes), Err(Errno::NotFound));
}

#[test]
fn refuses_a_zero_length_descriptor_rather_than_looping() {
    let mut bytes = stream(&[&interface(0, BOT_SCSI)]);
    // A hostile descriptor whose bLength is zero can never advance the
    // walk; append one and assert the refusal.
    bytes.extend_from_slice(&[0, DESC_TYPE_ENDPOINT]);
    let total = u16::try_from(bytes.len()).expect("fits");
    bytes[2..4].copy_from_slice(&total.to_le_bytes());
    assert_eq!(find_storage_interface(&bytes), Err(Errno::LengthOutOfRange));
}

#[test]
fn refuses_a_descriptor_running_past_the_stream() {
    let mut bytes = stream(&[&interface(0, BOT_SCSI)]);
    // An endpoint descriptor claiming more bytes than remain.
    bytes.extend_from_slice(&[9, DESC_TYPE_ENDPOINT, 0x81, 0x02]);
    let total = u16::try_from(bytes.len()).expect("fits");
    bytes[2..4].copy_from_slice(&total.to_le_bytes());
    assert_eq!(find_storage_interface(&bytes), Err(Errno::LengthOutOfRange));
}

#[test]
fn refuses_a_truncated_or_mistyped_header() {
    assert_eq!(
        configuration_total_length(&[9, DESC_TYPE_CONFIGURATION, 9, 0, 1, 1, 0, 0x80]),
        Err(Errno::LengthOutOfRange)
    );
    let mistyped = [9, DESC_TYPE_INTERFACE, 9, 0, 1, 1, 0, 0x80, 50];
    assert_eq!(configuration_total_length(&mistyped), Err(Errno::BadMagic));
    // A total shorter than the header itself cannot be a stream.
    let short_total = [9, DESC_TYPE_CONFIGURATION, 4, 0, 1, 1, 0, 0x80, 50];
    assert_eq!(
        configuration_total_length(&short_total),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn refuses_a_stream_shorter_than_its_announced_total() {
    let bytes = stream(&[&interface(0, BOT_SCSI)]);
    assert_eq!(
        find_storage_interface(&bytes[..bytes.len() - 1]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_second_matching_interface_serves_when_the_first_lacks_endpoints() {
    let bytes = stream(&[
        &interface(0, BOT_SCSI), // no endpoints follow
        &interface(1, BOT_SCSI),
        &endpoint(0x81, 0x02),
        &endpoint(0x02, 0x02),
    ]);
    assert_eq!(
        find_storage_interface(&bytes),
        Ok(StorageInterface {
            interface_number: 1,
            command_set: CommandSet::Transparent,
            protocol: StorageProtocol::Bot {
                bulk_in: 1,
                bulk_out: 2,
            },
        })
    );
}
