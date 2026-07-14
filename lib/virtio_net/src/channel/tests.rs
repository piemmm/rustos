//! Host tests for [`NetChannelServer`](super::NetChannelServer): the
//! detached/attached state machine, the geometry validation, and the
//! fail-closed service path, driven against an in-process loopback [`Net`].

extern crate alloc;

use super::NetChannelServer;
use rustos_abi::driver::net::{
    DeviceFacts, LinkState, MacAddress, Net, NetOffloads, ETHERNET_HEADER_LEN,
};
use rustos_abi::driver::net_channel::{decode_facts_reply, decode_service_reply, AttachParams};
use rustos_abi::driver::net_ring::{FrameRings, RingGeometry, ServiceReport};
use rustos_abi::driver::BufferClass;
use rustos_abi::reply::decode_status_reply;
use rustos_abi::DriverError;
use rustos_abi::{Errno, PortName};

/// The mock device's MTU (standard Ethernet).
const MTU: u32 = 1500;

/// A loopback [`Net`]: frames pushed to the TX ring come straight back on
/// the RX ring, so one service call round-trips a frame end to end.
struct LoopbackNet {
    facts_fault: bool,
}

impl LoopbackNet {
    fn new() -> Self {
        Self { facts_fault: false }
    }
}

impl Net for LoopbackNet {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        if self.facts_fault {
            return Err(DriverError::DeviceFault);
        }
        Ok(DeviceFacts {
            mac: MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]),
            mtu: MTU,
            link: LinkState::Up,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
        })
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let mut report = ServiceReport::default();
        let mut frame = [0u8; 2048];
        loop {
            match rings.tx.pop(&mut frame) {
                Ok(Some(len)) => {
                    report.transmitted += 1;
                    match rings.rx.push(&frame[..len]) {
                        Ok(()) => report.received += 1,
                        Err(Errno::NoSpace) => {
                            report.rx_ring_full = true;
                            break;
                        }
                        Err(_) => return Err(DriverError::BadMagic),
                    }
                }
                Ok(None) => break,
                Err(Errno::LengthOutOfRange) => {}
                Err(_) => return Err(DriverError::BadMagic),
            }
        }
        Ok(report)
    }
}

/// A geometry sized exactly to the device's largest frame, as the stack
/// derives it from the facts reply.
fn geometry() -> RingGeometry {
    RingGeometry::new(8, MTU + ETHERNET_HEADER_LEN).expect("valid geometry")
}

fn attach_params(geometry: RingGeometry) -> AttachParams {
    AttachParams {
        geometry,
        region_grant: 0x1234,
        class: BufferClass::NonSensitive,
        notify_port: PortName::from_ascii(b"netstack.nic0").expect("port"),
    }
}

#[test]
fn facts_reply_reports_the_device() {
    let server = NetChannelServer::new(LoopbackNet::new());
    let facts = decode_facts_reply(&server.facts_reply()).expect("facts");
    assert_eq!(facts.mtu, MTU);
    assert_eq!(facts.rx_queues, 1);
}

#[test]
fn facts_reply_carries_a_device_fault() {
    let mut net = LoopbackNet::new();
    net.facts_fault = true;
    let server = NetChannelServer::new(net);
    assert_eq!(
        decode_facts_reply(&server.facts_reply()),
        Err(Errno::DeviceFault)
    );
}

#[test]
fn service_before_attach_fails_closed() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    assert!(!server.is_attached());
    let mut region = alloc::vec![0u8; geometry().region_len()];
    assert_eq!(
        decode_service_reply(&server.service_reply(&mut region)),
        Err(Errno::NotConnected)
    );
}

#[test]
fn attach_stores_state_and_service_round_trips_a_frame() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    let geom = geometry();
    assert!(decode_status_reply(&server.attach(attach_params(geom))).is_ok());
    assert!(server.is_attached());
    assert_eq!(server.geometry(), Some(geom));
    assert_eq!(
        server.notify_port(),
        Some(PortName::from_ascii(b"netstack.nic0").expect("port"))
    );

    // Queue a frame in TX from the stack side, then doorbell.
    let mut region = alloc::vec![0u8; geom.region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geom, BufferClass::NonSensitive).expect("bind");
        rings.tx.push(&[0xAB; 64]).expect("queue tx");
    }
    let report = decode_service_reply(&server.service_reply(&mut region)).expect("service");
    assert_eq!(report.transmitted, 1);
    assert_eq!(report.received, 1);

    // The frame looped back onto RX.
    let mut rings = FrameRings::bind(&mut region, geom, BufferClass::NonSensitive).expect("bind");
    let mut out = [0u8; 2048];
    assert_eq!(rings.rx.pop(&mut out), Ok(Some(64)));
    assert_eq!(&out[..64], &[0xAB; 64]);
}

#[test]
fn attach_rejects_a_geometry_too_small_for_the_device() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    // A ring whose slots cannot carry the device's full frame (MTU+header).
    let small = RingGeometry::new(8, MTU).expect("valid but too small");
    assert_eq!(
        decode_status_reply(&server.attach(attach_params(small))),
        Err(Errno::OutOfRange)
    );
    assert!(!server.is_attached());
}

#[test]
fn service_rejects_a_wrong_length_region() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    let geom = geometry();
    let _ = server.attach(attach_params(geom));
    // A region a byte short of the agreed geometry is refused whole.
    let mut region = alloc::vec![0u8; geom.region_len() - 1];
    assert_eq!(
        decode_service_reply(&server.service_reply(&mut region)),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn detach_returns_to_the_detached_state() {
    let mut server = NetChannelServer::new(LoopbackNet::new());
    let _ = server.attach(attach_params(geometry()));
    assert!(server.is_attached());
    assert!(decode_status_reply(&server.detach()).is_ok());
    assert!(!server.is_attached());
    assert_eq!(server.notify_port(), None);
    assert_eq!(server.geometry(), None);
}
