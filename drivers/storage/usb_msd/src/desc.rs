//! Fail-closed configuration-descriptor reader: which interface and bulk
//! endpoint pair this class driver drives.
//!
//! The interface node the host-controller driver emits carries the device's
//! `vid:pid:class` identity but not its endpoint numbers, and the URB
//! transport's bulk server refuses any endpoint that is not the interface's
//! *configured* bulk endpoint in that direction. The class driver therefore
//! reads the device's own configuration descriptor over control-IN (the
//! standard `GET_DESCRIPTOR` every USB device serves) and derives the same
//! facts the host-controller driver derived at enumeration — the bulk-only
//! mass-storage interface's number (the `wIndex` of the class requests) and
//! its first bulk-IN + bulk-OUT endpoints — from the descriptor stream,
//! never assumed (USB 2.0 §9.6).
//!
//! The descriptor bytes come from the device and are hostile input: every
//! length is validated, the walk is bounded by the buffer, and a stream that
//! cannot be parsed refuses the whole device rather than guessing.

use rustos_abi::Errno;

/// `bDescriptorType` of a configuration descriptor (USB 2.0 table 9-5).
const DESC_TYPE_CONFIGURATION: u8 = 2;
/// `bDescriptorType` of an interface descriptor.
const DESC_TYPE_INTERFACE: u8 = 4;
/// `bDescriptorType` of an endpoint descriptor.
const DESC_TYPE_ENDPOINT: u8 = 5;

/// Length of a configuration-descriptor header (USB 2.0 §9.6.3).
pub const CONFIGURATION_HEADER_LEN: usize = 9;
/// Length of an interface descriptor (USB 2.0 §9.6.5).
const INTERFACE_DESC_LEN: usize = 9;
/// Minimum length of an endpoint descriptor (USB 2.0 §9.6.6).
const ENDPOINT_DESC_LEN: usize = 7;

/// `bmAttributes` transfer-type mask and the bulk value (USB 2.0 §9.6.6).
const ATTR_TRANSFER_TYPE_MASK: u8 = 0x03;
const ATTR_TRANSFER_TYPE_BULK: u8 = 0x02;

/// `bEndpointAddress` direction bit: set for IN endpoints.
const ENDPOINT_ADDRESS_IN: u8 = 0x80;
/// `bEndpointAddress` endpoint-number mask.
const ENDPOINT_ADDRESS_NUMBER_MASK: u8 = 0x0F;

/// The facts the class driver derives from the configuration descriptor:
/// the bulk-only mass-storage interface and its bulk endpoint pair.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MsdInterface {
    /// `bInterfaceNumber` — the `wIndex` of the class requests
    /// (`GET MAX LUN`, Bulk-Only Mass Storage Reset).
    pub interface_number: u8,
    /// Endpoint number of the interface's first bulk-IN endpoint.
    pub bulk_in: u8,
    /// Endpoint number of the interface's first bulk-OUT endpoint.
    pub bulk_out: u8,
}

/// Total length (`wTotalLength`) the configuration-descriptor header
/// announces, so the caller can fetch the full stream.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] if `header` is shorter than the 9-byte
///   configuration header, or the announced total is shorter than the
///   header itself (a stream that cannot contain its own header).
/// * [`Errno::BadMagic`] if the descriptor type is not a configuration
///   descriptor (fail closed — never walk a stream of the wrong shape).
pub fn configuration_total_length(header: &[u8]) -> Result<usize, Errno> {
    if header.len() < CONFIGURATION_HEADER_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    if header[1] != DESC_TYPE_CONFIGURATION {
        return Err(Errno::BadMagic);
    }
    let total = usize::from(u16::from_le_bytes([header[2], header[3]]));
    if total < CONFIGURATION_HEADER_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(total)
}

/// Find the first SCSI-transparent bulk-only mass-storage interface
/// (class `08:06:50`, alternate setting 0) in the full configuration
/// descriptor stream and derive its bulk endpoint pair — the first bulk-IN
/// and the first bulk-OUT endpoint the interface declares, exactly the pair
/// the host-controller driver configured at enumeration.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] / [`Errno::BadMagic`] as
///   [`configuration_total_length`], or for a descriptor whose announced
///   length is shorter than its type requires or runs past the stream (a
///   zero `bLength` cannot advance the walk and is refused, never looped
///   on).
/// * [`Errno::NotFound`] if no such interface exists, or it declares no
///   bulk-IN + bulk-OUT pair, or an endpoint number is outside `1..=15`.
pub fn find_msd_interface(stream: &[u8]) -> Result<MsdInterface, Errno> {
    let total = configuration_total_length(stream)?;
    if stream.len() < total {
        return Err(Errno::LengthOutOfRange);
    }
    let stream = &stream[..total];

    // Walk the descriptor stream. `offset` only ever advances by a
    // validated, non-zero `bLength`, so the walk is bounded by the stream.
    let mut offset = CONFIGURATION_HEADER_LEN;
    // The matched interface, once seen; endpoint descriptors that follow it
    // (until the next interface descriptor) belong to it.
    let mut matched: Option<u8> = None;
    let mut bulk_in: Option<u8> = None;
    let mut bulk_out: Option<u8> = None;
    while offset < stream.len() {
        let remaining = &stream[offset..];
        if remaining.len() < 2 {
            return Err(Errno::LengthOutOfRange);
        }
        let length = usize::from(remaining[0]);
        if length < 2 || length > remaining.len() {
            return Err(Errno::LengthOutOfRange);
        }
        let descriptor = &remaining[..length];
        match descriptor[1] {
            DESC_TYPE_INTERFACE => {
                if length < INTERFACE_DESC_LEN {
                    return Err(Errno::LengthOutOfRange);
                }
                if matched.is_some() {
                    // The matched interface's endpoint list has ended
                    // without a full bulk pair; keep looking for a later
                    // matching interface.
                    matched = None;
                    bulk_in = None;
                    bulk_out = None;
                }
                let alternate_setting = descriptor[3];
                let class = descriptor[5];
                let sub_class = descriptor[6];
                let protocol = descriptor[7];
                if alternate_setting == 0
                    && u32::from_be_bytes([0, class, sub_class, protocol])
                        == crate::MSD_INTERFACE_CLASS
                {
                    matched = Some(descriptor[2]);
                }
            }
            DESC_TYPE_ENDPOINT => {
                if length < ENDPOINT_DESC_LEN {
                    return Err(Errno::LengthOutOfRange);
                }
                if let Some(interface_number) = matched {
                    let address = descriptor[2];
                    let attributes = descriptor[3];
                    let number = address & ENDPOINT_ADDRESS_NUMBER_MASK;
                    if attributes & ATTR_TRANSFER_TYPE_MASK == ATTR_TRANSFER_TYPE_BULK
                        && number != 0
                    {
                        if address & ENDPOINT_ADDRESS_IN != 0 {
                            if bulk_in.is_none() {
                                bulk_in = Some(number);
                            }
                        } else if bulk_out.is_none() {
                            bulk_out = Some(number);
                        }
                    }
                    if let (Some(bulk_in), Some(bulk_out)) = (bulk_in, bulk_out) {
                        return Ok(MsdInterface {
                            interface_number,
                            bulk_in,
                            bulk_out,
                        });
                    }
                }
            }
            _ => {}
        }
        offset += length;
    }
    Err(Errno::NotFound)
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn finds_the_bulk_pair_of_the_msd_interface() {
        let bytes = stream(&[
            &interface(0, [0x08, 0x06, 0x50]),
            &endpoint(0x83, 0x02), // bulk-IN, EP3
            &endpoint(0x04, 0x02), // bulk-OUT, EP4
        ]);
        assert_eq!(
            find_msd_interface(&bytes),
            Ok(MsdInterface {
                interface_number: 0,
                bulk_in: 3,
                bulk_out: 4,
            })
        );
    }

    #[test]
    fn skips_foreign_interfaces_and_their_endpoints() {
        let bytes = stream(&[
            &interface(0, [0x03, 0x01, 0x01]), // HID boot keyboard
            &endpoint(0x81, 0x03),             // its interrupt-IN
            &interface(1, [0x08, 0x06, 0x50]),
            &endpoint(0x02, 0x02), // bulk-OUT first
            &endpoint(0x85, 0x02), // bulk-IN second
        ]);
        assert_eq!(
            find_msd_interface(&bytes),
            Ok(MsdInterface {
                interface_number: 1,
                bulk_in: 5,
                bulk_out: 2,
            })
        );
    }

    #[test]
    fn ignores_non_bulk_endpoints_on_the_msd_interface() {
        let bytes = stream(&[
            &interface(0, [0x08, 0x06, 0x50]),
            &endpoint(0x81, 0x03), // interrupt-IN — not the data pair
            &endpoint(0x82, 0x02), // bulk-IN, EP2
            &endpoint(0x03, 0x02), // bulk-OUT, EP3
        ]);
        assert_eq!(
            find_msd_interface(&bytes),
            Ok(MsdInterface {
                interface_number: 0,
                bulk_in: 2,
                bulk_out: 3,
            })
        );
    }

    #[test]
    fn refuses_a_stream_with_no_msd_interface() {
        let bytes = stream(&[&interface(0, [0x03, 0x01, 0x01]), &endpoint(0x81, 0x03)]);
        assert_eq!(find_msd_interface(&bytes), Err(Errno::NotFound));
    }

    #[test]
    fn refuses_an_msd_interface_missing_a_bulk_direction() {
        let bytes = stream(&[
            &interface(0, [0x08, 0x06, 0x50]),
            &endpoint(0x81, 0x02), // bulk-IN only
        ]);
        assert_eq!(find_msd_interface(&bytes), Err(Errno::NotFound));
    }

    #[test]
    fn refuses_a_zero_length_descriptor_rather_than_looping() {
        let mut bytes = stream(&[&interface(0, [0x08, 0x06, 0x50])]);
        // A hostile descriptor whose bLength is zero can never advance the
        // walk; append one and assert the refusal.
        bytes.extend_from_slice(&[0, DESC_TYPE_ENDPOINT]);
        let total = u16::try_from(bytes.len()).expect("fits");
        bytes[2..4].copy_from_slice(&total.to_le_bytes());
        assert_eq!(find_msd_interface(&bytes), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn refuses_a_descriptor_running_past_the_stream() {
        let mut bytes = stream(&[&interface(0, [0x08, 0x06, 0x50])]);
        // An endpoint descriptor claiming more bytes than remain.
        bytes.extend_from_slice(&[9, DESC_TYPE_ENDPOINT, 0x81, 0x02]);
        let total = u16::try_from(bytes.len()).expect("fits");
        bytes[2..4].copy_from_slice(&total.to_le_bytes());
        assert_eq!(find_msd_interface(&bytes), Err(Errno::LengthOutOfRange));
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
        let bytes = stream(&[&interface(0, [0x08, 0x06, 0x50])]);
        assert_eq!(
            find_msd_interface(&bytes[..bytes.len() - 1]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn a_second_matching_interface_serves_when_the_first_lacks_endpoints() {
        let bytes = stream(&[
            &interface(0, [0x08, 0x06, 0x50]), // no endpoints follow
            &interface(1, [0x08, 0x06, 0x50]),
            &endpoint(0x81, 0x02),
            &endpoint(0x02, 0x02),
        ]);
        assert_eq!(
            find_msd_interface(&bytes),
            Ok(MsdInterface {
                interface_number: 1,
                bulk_in: 1,
                bulk_out: 2,
            })
        );
    }
}
