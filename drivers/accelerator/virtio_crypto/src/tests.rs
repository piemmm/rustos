//! virtio-crypto unit tests against the in-process [`MockTransport`].
//!
//! The peer installed here is a protocol-faithful *virtio-crypto* device: it
//! decodes the control and data frames at the spec's offsets, keeps a session
//! table, and refuses a frame the driver got wrong. What it does **not** do is
//! AES — its payload transform is a deliberately trivial invertible mix whose
//! only job is to prove the key, the initialisation vector, the direction and
//! the payload all reached the device and came back. Whether the silicon
//! computes real AES-CBC is the device's claim, and the QEMU vertical checks
//! it against the NIST SP 800-38A known-answer vectors.

use super::*;
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_virtio::{ChainView, MockHost, MockTransport, MockWait, MAX_COMPLETION_WAKES};

/// Device-configuration window length: enough to hold `max_size` at offset
/// 48.
const CONFIG_LEN: usize = 56;

/// What the peer device recorded about one live session.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Session {
    algo: u32,
    op: u32,
    key: Vec<u8>,
}

/// Everything the peer device recorded, shared with the test body.
#[derive(Default)]
struct DeviceLog {
    /// Live sessions, by the id the device issued.
    sessions: Vec<(u64, Session)>,
    /// The next session id to issue.
    next_id: u64,
    /// Control frames the device decoded, in order, as
    /// `(opcode, session_id)`.
    control: Vec<(u32, u64)>,
    /// Data frames the device decoded, in order, as
    /// `(opcode, algo, session_id, iv_len, src_len, dst_len, op_type)`.
    data: Vec<(u32, u32, u64, u32, u32, u32, u32)>,
    /// Status the device should answer the next data request with.
    data_status: u8,
    /// Destroys still to refuse, keeping their sessions.
    refuse_destroys: u32,
    /// Complete requests without writing their replies.
    unanswering: bool,
    /// Answer jobs while reporting only the status written.
    short_output: bool,
}

impl DeviceLog {
    fn session(&self, id: u64) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, s)| s)
    }
}

/// The peer's non-cryptographic, invertible payload mix.
///
/// Sensitive to the key, the initialisation vector and the direction, so a
/// driver that dropped any of them, or bound the wrong direction into its
/// session, produces bytes this cannot invert.
fn mix(byte: u8, key: u8, iv: u8, encrypt: bool) -> u8 {
    if encrypt {
        (byte ^ key ^ iv).rotate_left(1)
    } else {
        byte.rotate_right(1) ^ key ^ iv
    }
}

/// A device-configuration window: ready, offering `services` with
/// `cipher_algos`, `data_queues` data queues and a `max_size` ceiling.
fn config(services: u32, cipher_algos: u32, data_queues: u32, max_size: u64) -> [u8; CONFIG_LEN] {
    let mut out = [0u8; CONFIG_LEN];
    out[wire::config::STATUS..][..4].copy_from_slice(&wire::S_HW_READY.to_le_bytes());
    out[wire::config::MAX_DATAQUEUES..][..4].copy_from_slice(&data_queues.to_le_bytes());
    out[wire::config::CRYPTO_SERVICES..][..4].copy_from_slice(&services.to_le_bytes());
    out[wire::config::CIPHER_ALGO_L..][..4].copy_from_slice(&cipher_algos.to_le_bytes());
    out[wire::config::MAX_SIZE..][..8].copy_from_slice(&max_size.to_le_bytes());
    out
}

/// The ordinary device this driver is written for: ready, one data queue,
/// AES-CBC, a 4 KiB ceiling.
fn healthy_config() -> [u8; CONFIG_LEN] {
    config(wire::SERVICE_CIPHER, wire::CIPHER_AES_CBC_BIT, 1, 4096)
}

/// Build a `MockTransport` speaking virtio-crypto over the given
/// configuration window, with the peer device installed on both queues.
///
/// The returned log is shared with the peer, so a test can read back exactly
/// what the device decoded.
fn build_device(config: [u8; CONFIG_LEN]) -> (MockTransport, Rc<RefCell<DeviceLog>>) {
    build_device_with_queue_max(config, 8)
}

/// [`build_device`] whose queues hold at most `queue_max` descriptors.
fn build_device_with_queue_max(
    config: [u8; CONFIG_LEN],
    queue_max: u16,
) -> (MockTransport, Rc<RefCell<DeviceLog>>) {
    // Data queue 0 plus the control queue at index 1 (one data queue).
    let mut t = MockTransport::new(2, queue_max, wire::VIRTIO_F_VERSION_1, CONFIG_LEN);
    t.set_config(0, &config);
    let log = Rc::new(RefCell::new(DeviceLog {
        next_id: 0x4200,
        data_status: wire::STATUS_OK,
        ..DeviceLog::default()
    }));

    install_control_shim(&mut t, &log);
    install_data_shim(&mut t, &log);
    (t, log)
}

/// Install the control queue's peer: the session table a create/destroy pair
/// acts on, refusing a frame that does not describe a plain cipher of the
/// length its own key descriptor carries.
fn install_control_shim(t: &mut MockTransport, log: &Rc<RefCell<DeviceLog>>) {
    let control_log = Rc::clone(log);
    t.install_shim(
        1,
        Box::new(move |chain: &mut ChainView<'_>| {
            let frame = *chain.device_read.first().ok_or(VirtioError::DeviceFault)?;
            if frame.len() != wire::REQ_LEN {
                return Err(VirtioError::DeviceFault);
            }
            let opcode = read_u32(frame, 0);
            let mut log = control_log.borrow_mut();
            match opcode {
                wire::CIPHER_CREATE_SESSION if log.unanswering => {
                    log.control.push((opcode, 0));
                    Ok(u32::try_from(wire::SESSION_INPUT_LEN).unwrap_or(0))
                }
                wire::CIPHER_CREATE_SESSION => {
                    let key = *chain.device_read.get(1).ok_or(VirtioError::DeviceFault)?;
                    let algo = read_u32(frame, wire::CTRL_CIPHER_PARA);
                    let keylen = read_u32(frame, wire::CTRL_CIPHER_PARA + 4);
                    let op = read_u32(frame, wire::CTRL_CIPHER_PARA + 8);
                    let op_type = read_u32(frame, wire::CTRL_SYM_OP_TYPE);
                    let reply = chain
                        .device_write
                        .first_mut()
                        .ok_or(VirtioError::DeviceFault)?;
                    if reply.len() != wire::SESSION_INPUT_LEN {
                        return Err(VirtioError::DeviceFault);
                    }
                    // The device refuses a frame that does not describe a
                    // plain cipher of the length its own key descriptor
                    // carries, exactly as the silicon would.
                    let well_formed = op_type == wire::SYM_OP_CIPHER
                        && keylen as usize == key.len()
                        && matches!(op, wire::OP_ENCRYPT | wire::OP_DECRYPT)
                        && algo == wire::CIPHER_AES_CBC;
                    if !well_formed {
                        reply[8..12].copy_from_slice(&u32::from(wire::STATUS_BADMSG).to_le_bytes());
                        log.control.push((opcode, 0));
                        return Ok(u32::try_from(reply.len()).unwrap_or(0));
                    }
                    let id = log.next_id;
                    log.next_id += 1;
                    log.sessions.push((
                        id,
                        Session {
                            algo,
                            op,
                            key: key.to_vec(),
                        },
                    ));
                    log.control.push((opcode, id));
                    reply[..8].copy_from_slice(&id.to_le_bytes());
                    reply[8..12].copy_from_slice(&u32::from(wire::STATUS_OK).to_le_bytes());
                    Ok(u32::try_from(reply.len()).unwrap_or(0))
                }
                wire::CIPHER_DESTROY_SESSION => {
                    let id = read_u64(frame, wire::CTRL_DESTROY_SESSION_ID);
                    log.control.push((opcode, id));
                    let reply = chain
                        .device_write
                        .first_mut()
                        .ok_or(VirtioError::DeviceFault)?;
                    if log.refuse_destroys > 0 {
                        log.refuse_destroys -= 1;
                        reply[0] = wire::STATUS_ERR;
                        return Ok(1);
                    }
                    let known = log.session(id).is_some();
                    log.sessions.retain(|(sid, _)| *sid != id);
                    reply[0] = if known {
                        wire::STATUS_OK
                    } else {
                        wire::STATUS_INVSESS
                    };
                    Ok(1)
                }
                _ => Err(VirtioError::DeviceFault),
            }
        }),
    );
}

/// Install the data queue's peer: the cipher itself, refusing a frame whose
/// declared lengths disagree with the descriptors carrying them or whose
/// direction is not the one its session was created for.
fn install_data_shim(t: &mut MockTransport, log: &Rc<RefCell<DeviceLog>>) {
    let data_log = Rc::clone(log);
    t.install_shim(
        0,
        Box::new(move |chain: &mut ChainView<'_>| {
            let frame = *chain.device_read.first().ok_or(VirtioError::DeviceFault)?;
            let iv = *chain.device_read.get(1).ok_or(VirtioError::DeviceFault)?;
            let src = *chain.device_read.get(2).ok_or(VirtioError::DeviceFault)?;
            if frame.len() != wire::REQ_LEN || chain.device_write.len() < 2 {
                return Err(VirtioError::DeviceFault);
            }
            let opcode = read_u32(frame, 0);
            let algo = read_u32(frame, 4);
            let id = read_u64(frame, 8);
            let iv_len = read_u32(frame, wire::DATA_CIPHER_PARA);
            let src_len = read_u32(frame, wire::DATA_CIPHER_PARA + 4);
            let dst_len = read_u32(frame, wire::DATA_CIPHER_PARA + 8);
            let op_type = read_u32(frame, wire::DATA_SYM_OP_TYPE);
            let mut log = data_log.borrow_mut();
            log.data
                .push((opcode, algo, id, iv_len, src_len, dst_len, op_type));
            if log.unanswering {
                chain.device_write[0].fill(0xEE);
                return Ok(1);
            }
            let planted = log.data_status;
            let session = log.session(id).cloned();
            let (Some(session), true) = (session, planted == wire::STATUS_OK) else {
                chain.device_write[1][0] = if planted == wire::STATUS_OK {
                    wire::STATUS_INVSESS
                } else {
                    planted
                };
                return Ok(1);
            };
            // The device refuses a frame whose declared lengths disagree with
            // the descriptors carrying them, or whose direction is not the
            // one its session was created for.
            let encrypt = opcode == wire::CIPHER_ENCRYPT;
            let directions_agree = (encrypt && session.op == wire::OP_ENCRYPT)
                || (!encrypt && session.op == wire::OP_DECRYPT);
            let lengths_agree = iv_len as usize == iv.len()
                && src_len as usize == src.len()
                && dst_len as usize == chain.device_write[0].len();
            if !directions_agree
                || !lengths_agree
                || op_type != wire::SYM_OP_CIPHER
                || algo != session.algo
            {
                chain.device_write[1][0] = wire::STATUS_BADMSG;
                return Ok(1);
            }
            for (index, byte) in src.iter().enumerate() {
                let key = session.key[index % session.key.len()];
                chain.device_write[0][index] = mix(*byte, key, iv[index % iv.len()], encrypt);
            }
            chain.device_write[1][0] = wire::STATUS_OK;
            if log.short_output {
                return Ok(1);
            }
            Ok(u32::try_from(src.len()).unwrap_or(0) + 1)
        }),
    );
}

/// The mock device, shared by the driver under test and the host playing it.
type Device = Rc<RefCell<MockTransport>>;

type Crypto = Box<VirtioCrypto<'static, Device>>;

/// Open a driver on `t`, whose waits `host` answers by playing the device.
fn open_played_by(t: MockTransport, host: MockHost) -> (Crypto, Device, &'static MockHost) {
    let host: &'static MockHost = Box::leak(Box::new(host));
    let device = t.into_shared();
    host.attach(&device);
    let driver = Box::new(VirtioCrypto::open(Rc::clone(&device), host).expect("open"));
    (driver, device, host)
}

fn auto_host() -> &'static MockHost {
    Box::leak(Box::new(MockHost::new()))
}

fn open_healthy() -> (Crypto, Rc<RefCell<DeviceLog>>, &'static MockHost, Device) {
    let (t, log) = build_device(healthy_config());
    let (driver, device, host) = open_played_by(t, MockHost::new());
    (driver, log, host, device)
}

/// Let the wait `answered` waits from now time out without the device
/// answering, abandoning that request to be answered later.
fn go_silent_after(host: &MockHost, answered: usize) {
    host.script_waits(core::iter::repeat_n(MockWait::Answer, answered).chain([MockWait::Silent]));
}

const KEY: [u8; 16] = [
    0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c,
];
const IV: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
];

fn cipher_job<'a>(
    direction: CipherDirection,
    input: &'a [u8],
    output: &'a mut [u8],
) -> CipherJob<'a> {
    CipherJob {
        algorithm: CipherAlgorithm::AesCbc,
        direction,
        key: &KEY,
        iv: &IV,
        input,
        output,
    }
}

#[test]
fn register_requires_the_load_capability() {
    struct Host(bool);
    impl DriverHost for Host {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            self.0 && cap == CapabilityId::DRV_LOAD
        }

        fn kind(&self) -> tairix_abi::DriverKind {
            tairix_abi::DriverKind::UserSpace
        }
    }
    assert!(register(&Host(true)).is_ok());
    assert_eq!(register(&Host(false)), Err(DriverError::PermissionDenied));
}

#[test]
fn bind_table_names_the_virtio_crypto_device_type_and_nothing_else() {
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(VIRTIO_CRYPTO_DEVICE_ID, 20);
    let key = BIND_KEYS[0].key;
    assert!(key.matches(&HwMatchKey::virtio(VIRTIO_CRYPTO_DEVICE_ID)));
    assert!(!key.matches(&HwMatchKey::virtio(2)));
}

#[test]
fn open_negotiates_version_1_and_reports_what_the_device_offered() {
    let (driver, _log, _host, device) = open_healthy();
    assert!(device.borrow().status().contains(Status::DRIVER_OK));
    assert_eq!(
        device.borrow().negotiated_driver_features(),
        wire::VIRTIO_F_VERSION_1
    );
    let report = driver.device_report();
    assert!(report.ciphers.contains(CipherAlgorithm::AesCbc));
    assert_eq!(report.ciphers.len(), 1);
    assert_eq!(report.max_job_bytes, 4096);
    // A virtio-crypto device owns no memory of its own; reporting a figure
    // here would be inventing one.
    assert_eq!(report.mem_total_bytes, 0);
    assert_eq!(report.mem_resident_bytes, 0);
}

#[test]
fn open_refuses_a_device_that_is_not_ready() {
    let (mut t, _log) = build_device(healthy_config());
    t.set_config(wire::config::STATUS, &0u32.to_le_bytes());
    assert_eq!(
        VirtioCrypto::open(t, auto_host()).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_refuses_a_device_offering_no_cipher_service() {
    let (t, _log) = build_device(config(0, wire::CIPHER_AES_CBC_BIT, 1, 4096));
    assert_eq!(
        VirtioCrypto::open(t, auto_host()).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_refuses_a_device_whose_algorithms_this_driver_cannot_encode() {
    // Service offered, but the only algorithm bit set is one this driver does
    // not implement: binding would accept jobs it could only fail.
    let (t, _log) = build_device(config(wire::SERVICE_CIPHER, 1 << 13, 1, 4096));
    assert_eq!(
        VirtioCrypto::open(t, auto_host()).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn an_algorithm_bit_this_driver_cannot_encode_is_ignored_not_offered() {
    // The device offers AES-CBC alongside two algorithms this driver has no
    // request encoding for. Offering them would have jobs accepted here and
    // then failed by the device; ignoring them keeps the refusal at the
    // driver, where the caller can act on it.
    let offered = wire::CIPHER_AES_CBC_BIT | (1 << 13) | (1 << 1);
    let (t, _log) = build_device(config(wire::SERVICE_CIPHER, offered, 1, 4096));
    let driver = VirtioCrypto::open(t, auto_host()).expect("open");
    let report = driver.device_report();
    assert!(report.ciphers.contains(CipherAlgorithm::AesCbc));
    assert_eq!(report.ciphers.len(), 1);
}

#[test]
fn open_refuses_a_device_that_advertises_no_data_queue() {
    let (t, _log) = build_device(config(
        wire::SERVICE_CIPHER,
        wire::CIPHER_AES_CBC_BIT,
        0,
        4096,
    ));
    assert_eq!(
        VirtioCrypto::open(t, auto_host()).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn the_staged_ceiling_is_the_smaller_of_the_devices_and_the_drivers() {
    // A device claiming a preposterous ceiling cannot make the driver
    // allocate for it.
    let (t, _log) = build_device(config(
        wire::SERVICE_CIPHER,
        wire::CIPHER_AES_CBC_BIT,
        1,
        u64::MAX,
    ));
    let driver = VirtioCrypto::open(t, auto_host()).expect("open");
    assert_eq!(driver.device_report().max_job_bytes, MAX_STAGED_JOB_BYTES);
    // A device declaring no ceiling of its own gets the driver's.
    let (t, _log) = build_device(config(wire::SERVICE_CIPHER, wire::CIPHER_AES_CBC_BIT, 1, 0));
    let driver = VirtioCrypto::open(t, auto_host()).expect("open");
    assert_eq!(driver.device_report().max_job_bytes, MAX_STAGED_JOB_BYTES);
}

#[test]
fn a_cipher_job_round_trips_through_the_device() {
    let (mut driver, _log, _host, _device) = open_healthy();
    let plain = [0x11u8; 32];
    let mut encrypted = [0u8; 32];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &plain, &mut encrypted))
        .expect("encrypt");
    assert_ne!(encrypted, plain, "the device transformed the payload");
    let mut back = [0u8; 32];
    driver
        .cipher(cipher_job(CipherDirection::Decrypt, &encrypted, &mut back))
        .expect("decrypt");
    assert_eq!(back, plain);
}

#[test]
fn the_control_frames_carry_the_layout_the_device_decodes() {
    let (mut driver, log, _host, _device) = open_healthy();
    let mut out = [0u8; 16];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out))
        .expect("encrypt");
    let log = log.borrow();
    // Create then destroy, and the destroy names the id the create issued —
    // which is only true if the driver read the session reply at the right
    // offset and wrote it back at the right one.
    assert_eq!(log.control.len(), 2);
    assert_eq!(log.control[0].0, wire::CIPHER_CREATE_SESSION);
    assert_eq!(log.control[1].0, wire::CIPHER_DESTROY_SESSION);
    assert_eq!(log.control[0].1, log.control[1].1);
    assert_ne!(log.control[0].1, 0);
}

#[test]
fn the_data_frame_carries_the_session_the_algorithm_and_every_length() {
    let (mut driver, log, _host, _device) = open_healthy();
    let plain = [0u8; 48];
    let mut out = [0u8; 48];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &plain, &mut out))
        .expect("encrypt");
    let log = log.borrow();
    let (opcode, algo, id, iv_len, src_len, dst_len, op_type) = log.data[0];
    assert_eq!(opcode, wire::CIPHER_ENCRYPT);
    assert_eq!(algo, wire::CIPHER_AES_CBC);
    assert_eq!(id, log.control[0].1);
    assert_eq!(usize::try_from(iv_len).expect("fits"), IV.len());
    assert_eq!(usize::try_from(src_len).expect("fits"), plain.len());
    assert_eq!(usize::try_from(dst_len).expect("fits"), plain.len());
    assert_eq!(op_type, wire::SYM_OP_CIPHER);
}

#[test]
fn a_decrypt_job_binds_the_decrypt_direction_into_its_session() {
    // The peer refuses a data request whose direction is not the one its
    // session was created for, so a driver that hard-coded the session's `op`
    // could not complete this.
    let (mut driver, log, _host, _device) = open_healthy();
    let mut out = [0u8; 16];
    driver
        .cipher(cipher_job(CipherDirection::Decrypt, &[0u8; 16], &mut out))
        .expect("decrypt");
    assert_eq!(log.borrow().data[0].0, wire::CIPHER_DECRYPT);
}

#[test]
fn the_session_is_destroyed_even_when_the_job_fails_and_the_jobs_error_wins() {
    let (mut driver, log, _host, _device) = open_healthy();
    log.borrow_mut().data_status = wire::STATUS_ERR;
    let mut out = [0xAAu8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out)),
        Err(DriverError::DeviceFault)
    );
    let log = log.borrow();
    // Nothing left behind holding the caller's key schedule.
    assert!(log.sessions.is_empty());
    assert_eq!(log.control[1].0, wire::CIPHER_DESTROY_SESSION);
    // The caller's buffer is as it was: a refused job transforms nothing.
    assert_eq!(out, [0xAAu8; 16]);
}

#[test]
fn a_refused_key_and_an_unsupported_request_are_request_level_refusals() {
    let (mut driver, log, _host, _device) = open_healthy();
    let mut out = [0u8; 16];
    log.borrow_mut().data_status = wire::STATUS_KEY_REJECTED;
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out)),
        Err(DriverError::PermissionDenied)
    );
    log.borrow_mut().data_status = wire::STATUS_NOTSUPP;
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out)),
        Err(DriverError::Unsupported)
    );
    log.borrow_mut().data_status = wire::STATUS_NOSPC;
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out)),
        Err(DriverError::Busy)
    );
}

#[test]
fn status_decoding_fails_an_undefined_value_closed() {
    assert_eq!(status_to_result(wire::STATUS_OK), Ok(()));
    assert_eq!(
        status_to_result(wire::STATUS_ERR),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        status_to_result(wire::STATUS_BADMSG),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        status_to_result(wire::STATUS_INVSESS),
        Err(DriverError::DeviceFault)
    );
    for undefined in [7u8, 42, u8::MAX] {
        assert_eq!(
            status_to_result(undefined),
            Err(DriverError::DeviceFault),
            "status {undefined} must not read as success"
        );
    }
}

#[test]
fn a_job_the_device_cannot_do_is_refused_before_any_of_it_is_submitted() {
    let (mut driver, log, _host, _device) = open_healthy();
    let mut out = [0u8; 16];
    // A key length the algorithm does not accept.
    let job = CipherJob {
        key: &[0u8; 20],
        ..cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out)
    };
    assert_eq!(driver.cipher(job), Err(DriverError::OutOfRange));
    let mut out = [0u8; 16];
    // An initialisation vector of the wrong length.
    let job = CipherJob {
        iv: &[0u8; 8],
        ..cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out)
    };
    assert_eq!(driver.cipher(job), Err(DriverError::OutOfRange));
    // A partial block, and a mismatched output.
    let mut out = [0u8; 8];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 8], &mut out)),
        Err(DriverError::BufferTooSmall)
    );
    let mut out = [0u8; 32];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out)),
        Err(DriverError::BufferTooSmall)
    );
    // Not one of them reached the device: no session was ever created.
    assert!(log.borrow().control.is_empty());
    assert!(log.borrow().data.is_empty());
}

#[test]
fn a_job_above_the_devices_own_ceiling_is_refused_rather_than_split() {
    let (mut driver, log, _host, _device) = open_healthy();
    let ceiling = usize::try_from(driver.device_report().max_job_bytes).expect("fits");
    let plain = vec![0u8; ceiling + 16];
    let mut out = vec![0u8; ceiling + 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &plain, &mut out)),
        Err(DriverError::LengthOutOfRange)
    );
    assert!(log.borrow().control.is_empty());
    // Exactly the ceiling is admitted.
    let plain = vec![0u8; ceiling];
    let mut out = vec![0u8; ceiling];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &plain, &mut out))
        .expect("a job of exactly the ceiling runs");
}

#[test]
fn a_silent_device_releases_the_caller_rather_than_parking_it_for_ever() {
    let (t, _log) = build_device(healthy_config());
    let (mut driver, _device, _host) = open_played_by(t, MockHost::silent());
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out)),
        Err(DriverError::DeviceOffline)
    );
}

#[test]
fn a_wake_storm_with_no_completion_fails_the_job_closed() {
    let (t, _log) = build_device(healthy_config());
    let host = MockHost::new();
    let wakes = usize::try_from(MAX_COMPLETION_WAKES).unwrap_or(usize::MAX) + 1;
    host.script_waits(core::iter::repeat_n(
        MockWait::Spurious { after_ns: 0 },
        wakes,
    ));
    let (mut driver, _device, _host) = open_played_by(t, host);
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out)),
        Err(DriverError::DeviceFault)
    );
}

#[test]
fn the_job_path_reuses_its_staging_rather_than_re_granting_dma() {
    let (mut driver, _log, host, _device) = open_healthy();
    let after_open = host.bytes_allocated();
    let mut out = [0u8; 32];
    for _ in 0..4 {
        driver
            .cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 32], &mut out))
            .expect("encrypt");
    }
    assert_eq!(
        host.bytes_allocated(),
        after_open,
        "the job path must not re-enter the DMA allocator"
    );
}

#[test]
fn the_device_interrupt_is_acknowledged_once_per_published_chain() {
    let (mut driver, _log, _host, device) = open_healthy();
    let before = device.borrow().ack_interrupts;
    let mut out = [0u8; 16];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &[0u8; 16], &mut out))
        .expect("encrypt");
    // Three chains: create session, run the job, destroy session.
    assert_eq!(device.borrow().ack_interrupts, before + 3);
}

#[test]
fn bring_up_declares_the_device_quiesced_once_its_reset_confirms() {
    let (t, _log) = build_device(healthy_config());
    let host = MockHost::new();
    let _driver = VirtioCrypto::open(t, &host).expect("open");
    assert_eq!(host.quiesced_calls(), 1);
}

#[test]
fn a_device_whose_reset_never_confirms_is_refused_before_it_is_given_memory() {
    let (mut t, _log) = build_device(healthy_config());
    t.refuse_resets_after(0);
    let host = MockHost::new();
    assert_eq!(
        VirtioCrypto::open(t, &host).err(),
        Some(DriverError::DeviceFault)
    );
    assert_eq!(host.quiesced_calls(), 0);
    assert_eq!(host.bytes_allocated(), 0);
}

#[test]
fn a_dropped_device_that_confirms_its_reset_releases_every_region() {
    let (t, _log) = build_device(healthy_config());
    let host = MockHost::new();
    drop(VirtioCrypto::open(t, &host).expect("open"));
    assert_eq!(host.slabs_outstanding(), 0);
}

#[test]
fn a_dropped_device_whose_reset_never_confirms_releases_nothing() {
    let (t, _log) = build_device(healthy_config());
    let (driver, device, host) = open_played_by(t, MockHost::new());
    let held = host.slabs_outstanding();
    assert!(held > 0);
    device.borrow_mut().refuse_resets_after(0);
    drop(driver);
    assert_eq!(host.slabs_outstanding(), held);
}

/// What a healthy device returns for `input`, run on a fresh one.
fn healthy_output(input: &[u8]) -> Vec<u8> {
    let (mut driver, _log, _host, _device) = open_healthy();
    let mut out = vec![0u8; input.len()];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, input, &mut out))
        .expect("encrypt");
    out
}

#[test]
fn a_late_jobs_output_is_never_handed_to_the_next_caller() {
    let (mut driver, log, host, device) = open_healthy();
    let first = [0x11u8; 16];
    let second = [0x22u8; 16];
    // The create answers; the job itself is answered only after its deadline.
    go_silent_after(host, 1);
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &first, &mut out)),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(device.borrow_mut().drain_queue(DATA_QUEUE), Ok(1));

    let mut out = [0u8; 16];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &second, &mut out))
        .expect("the device answers again");
    assert_eq!(out.as_slice(), healthy_output(&second), "its own output");
    assert!(
        log.borrow().sessions.is_empty(),
        "the late job's session was destroyed with it"
    );
}

#[test]
fn a_session_an_abandoned_create_made_is_destroyed_once_the_device_answers() {
    let (mut driver, log, host, device) = open_healthy();
    go_silent_after(host, 0);
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x33; 16], &mut out)),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(device.borrow_mut().drain_queue(1), Ok(1));
    assert_eq!(
        log.borrow().sessions.len(),
        1,
        "the device made the session"
    );

    let mut out = [0u8; 16];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &[0x44; 16], &mut out))
        .expect("the device answers again");
    assert!(
        log.borrow().sessions.is_empty(),
        "no key schedule outlives its job"
    );
}

#[test]
fn a_destroy_the_device_refuses_is_retried_before_the_next_job() {
    // A refused destroy leaves the device holding the caller's key schedule.
    let (mut driver, log, _host, _device) = open_healthy();
    log.borrow_mut().refuse_destroys = 1;
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x21; 16], &mut out)),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(log.borrow().sessions.len(), 1);
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &[0x22; 16], &mut out))
        .expect("the device answers again");
    assert!(log.borrow().sessions.is_empty());
}

#[test]
fn an_abandoned_destroy_the_device_then_refuses_is_retried_before_the_next_job() {
    let (mut driver, log, host, device) = open_healthy();
    // The create and the job are answered; the destroy's wait goes silent.
    go_silent_after(host, 2);
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x31; 16], &mut out)),
        Err(DriverError::DeviceOffline)
    );
    log.borrow_mut().refuse_destroys = 1;
    assert_eq!(device.borrow_mut().drain_queue(1), Ok(1), "refused late");
    assert_eq!(log.borrow().sessions.len(), 1);
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &[0x32; 16], &mut out))
        .expect("the device answers again");
    assert!(log.borrow().sessions.is_empty());
}

#[test]
fn a_job_the_device_completed_without_answering_hands_back_nothing() {
    // The status staging is reused, so without a fresh sentinel a job the
    // device never answered reads as the last one's OK.
    let (mut driver, log, _host, _device) = open_healthy();
    let mut out = [0u8; 16];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &[0x41; 16], &mut out))
        .expect("answered");
    log.borrow_mut().unanswering = true;
    out.fill(0);
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x42; 16], &mut out)),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(out, [0u8; 16]);
}

#[test]
fn a_create_the_device_completed_without_answering_runs_no_job() {
    // Read stale, the session reply would name the last, destroyed session.
    let (mut driver, log, _host, _device) = open_healthy();
    let mut out = [0u8; 16];
    driver
        .cipher(cipher_job(CipherDirection::Encrypt, &[0x51; 16], &mut out))
        .expect("answered");
    log.borrow_mut().unanswering = true;
    let jobs = log.borrow().data.len();
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x52; 16], &mut out)),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(log.borrow().data.len(), jobs, "no job was published");
}

#[test]
fn a_ring_too_shallow_for_a_job_is_refused_before_it_is_programmed() {
    // A device whose data queue holds four descriptors could run no job.
    let (t, _log) = build_device_with_queue_max(healthy_config(), 4);
    let device = t.into_shared();
    assert_eq!(
        VirtioCrypto::open(Rc::clone(&device), auto_host()).err(),
        Some(DriverError::Unsupported)
    );
    assert_eq!(
        device.borrow_mut().publish_raw_used(DATA_QUEUE, 0, 0),
        Err(VirtioError::DeviceFault),
        "the device was never given the ring"
    );
}

#[test]
fn a_control_queue_too_shallow_for_a_session_is_refused_at_open() {
    let (mut t, _log) = build_device(healthy_config());
    t.set_queue_max(1, 2);
    let device = t.into_shared();
    assert_eq!(
        VirtioCrypto::open(Rc::clone(&device), auto_host()).err(),
        Some(DriverError::Unsupported)
    );
    assert_eq!(
        device.borrow_mut().publish_raw_used(1, 0, 0),
        Err(VirtioError::DeviceFault),
        "the device was never given the ring"
    );
}

#[test]
fn a_job_whose_completion_does_not_cover_its_output_hands_back_nothing() {
    let (mut driver, log, _host, _device) = open_healthy();
    log.borrow_mut().short_output = true;
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x5D; 16], &mut out)),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(out, [0u8; 16]);
    assert!(
        log.borrow().sessions.is_empty(),
        "its session was destroyed"
    );
}

#[test]
fn a_key_the_device_held_is_scrubbed_when_the_driver_is_dropped() {
    let (mut driver, _log, host, _device) = open_healthy();
    go_silent_after(host, 0);
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x78; 16], &mut out)),
        Err(DriverError::DeviceOffline)
    );
    let (phys, len) = driver
        .key
        .as_ref()
        .map(|key| (key.phys(), key.len()))
        .expect("put back");
    drop(driver);
    // SAFETY: the mock host leaks every slab's storage, so these bytes
    // outlive the driver that freed them.
    let staging = unsafe { core::slice::from_raw_parts(phys as *const u8, len) };
    assert!(
        staging.iter().all(|b| *b == 0),
        "the confirmed reset took it back"
    );
}

#[test]
fn no_job_is_published_while_the_device_still_holds_an_abandoned_chain() {
    let (mut driver, _log, host, device) = open_healthy();
    go_silent_after(host, 1);
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x55; 16], &mut out)),
        Err(DriverError::DeviceOffline)
    );
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x66; 16], &mut out)),
        Err(DriverError::DeviceOffline),
        "the staging is the device's, so nothing is published over it"
    );
    assert_eq!(device.borrow_mut().drain_queue(1), Ok(0));
    assert_eq!(
        device.borrow_mut().drain_queue(DATA_QUEUE),
        Ok(1),
        "only the abandoned job was ever published"
    );
}

#[test]
fn a_key_the_device_held_is_scrubbed_when_it_comes_back() {
    let (mut driver, _log, host, device) = open_healthy();
    go_silent_after(host, 0);
    let mut out = [0u8; 16];
    assert_eq!(
        driver.cipher(cipher_job(CipherDirection::Encrypt, &[0x77; 16], &mut out)),
        Err(DriverError::DeviceOffline)
    );
    let key = driver.key.as_ref().expect("put back");
    assert_eq!(
        &key.as_bytes()[..KEY.len()],
        KEY.as_slice(),
        "left for the device, which may not have read it yet"
    );
    assert_eq!(device.borrow_mut().drain_queue(1), Ok(1));
    driver.settle().expect("the chain came back");
    let key = driver.key.as_ref().expect("the driver's again");
    assert!(key.as_bytes().iter().all(|b| *b == 0));
}
