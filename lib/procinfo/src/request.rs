//! Framing a `sysinfo-v1` request and issuing it through the [`Transport`]
//! seam.
//!
//! Both the `sysinfo` and `ps` tools build the same [`SysinfoRequestHeader`]
//! envelope and map a capability denial onto the same distinguished error,
//! so that logic lives here rather than being copied.

use alloc::vec::Vec;

use tairix_abi::sysinfo::{encode_request as frame_request, SysinfoQueryId, SysinfoRequestHeader};
use tairix_abi::Errno;

use crate::transport::Transport;

/// Why a single System Information API call did not return a reply.
///
/// [`PermissionDenied`](CallError::PermissionDenied) is distinguished from
/// [`Service`](CallError::Service) so a consuming tool can print the precise
/// "this query requires a capability you do not hold" diagnostic; everything
/// else leans on the frozen [`Errno`] for the wire-level cause and invents no
/// parallel error set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallError {
    /// The service refused the query because the caller lacks the capability
    /// the query declares.
    PermissionDenied,
    /// The transport failed for any other reason. Carries the underlying
    /// [`Errno`].
    Service(Errno),
}

/// Frame `query` and its already-encoded `payload` into an owned
/// `sysinfo-v1` request buffer.
///
/// The envelope itself is built by the one framer in `lib/abi`, beside the
/// reply framing it mirrors; this adds only the allocation a tool wants and
/// the runtime's cache reporter does not. Every payload a client builds is
/// at most a handful of bytes and the buffer is sized to fit exactly, so the
/// framer cannot refuse; an empty request on that unreachable path would be
/// refused by the service rather than misread.
#[must_use]
pub fn encode_request(query: SysinfoQueryId, payload: &[u8]) -> Vec<u8> {
    let mut bytes = alloc::vec![0u8; SysinfoRequestHeader::WIRE_LEN + payload.len()];
    match frame_request(query, payload, &mut bytes) {
        Ok(len) => bytes.truncate(len),
        Err(_) => bytes.clear(),
    }
    bytes
}

/// Issue `query` with `payload` through `transport`, mapping the transport
/// [`Errno`] onto the [`CallError`] vocabulary so a capability denial renders
/// precisely.
///
/// # Errors
///
/// * [`CallError::PermissionDenied`] — the service refused the query for want
///   of its declared capability.
/// * [`CallError::Service`] — any other transport or IPC failure.
pub fn call(
    transport: &dyn Transport,
    query: SysinfoQueryId,
    payload: &[u8],
) -> Result<Vec<u8>, CallError> {
    transport
        .query(&encode_request(query, payload))
        .map_err(|errno| match errno {
            Errno::PermissionDenied => CallError::PermissionDenied,
            other => CallError::Service(other),
        })
}

#[cfg(test)]
mod tests {
    use super::{call, encode_request, CallError};
    use crate::transport::Transport;
    use alloc::vec::Vec;
    use tairix_abi::sysinfo::{SysinfoQueryId, SysinfoRequestHeader};
    use tairix_abi::Errno;

    struct Echo {
        reply: Result<Vec<u8>, Errno>,
        seen: core::cell::RefCell<Vec<u8>>,
    }

    impl Transport for Echo {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            self.seen.borrow_mut().extend_from_slice(request);
            self.reply.clone()
        }
    }

    #[test]
    fn encode_request_frames_header_and_payload() {
        let payload = [1u8, 2, 3, 4];
        let bytes = encode_request(SysinfoQueryId::SELF_PROCESS_LIST, &payload);
        assert_eq!(bytes.len(), SysinfoRequestHeader::WIRE_LEN + payload.len());
        let header = SysinfoRequestHeader::from_bytes(&bytes).expect("decode");
        assert_eq!(header.query, SysinfoQueryId::SELF_PROCESS_LIST);
        assert_eq!(header.payload_len as usize, payload.len());
        assert_eq!(&bytes[SysinfoRequestHeader::WIRE_LEN..], &payload);
    }

    #[test]
    fn encode_request_handles_an_empty_payload() {
        let bytes = encode_request(SysinfoQueryId::UPTIME, &[]);
        let header = SysinfoRequestHeader::from_bytes(&bytes).expect("decode");
        assert_eq!(header.payload_len, 0);
        assert_eq!(bytes.len(), SysinfoRequestHeader::WIRE_LEN);
    }

    #[test]
    fn call_forwards_the_encoded_request_and_reply() {
        let echo = Echo {
            reply: Ok(alloc::vec![9, 9, 9]),
            seen: core::cell::RefCell::new(Vec::new()),
        };
        let reply = call(&echo, SysinfoQueryId::UPTIME, &[]).expect("ok");
        assert_eq!(reply, alloc::vec![9, 9, 9]);
        let header = SysinfoRequestHeader::from_bytes(&echo.seen.borrow()).expect("decode");
        assert_eq!(header.query, SysinfoQueryId::UPTIME);
    }

    #[test]
    fn call_maps_permission_denied() {
        let echo = Echo {
            reply: Err(Errno::PermissionDenied),
            seen: core::cell::RefCell::new(Vec::new()),
        };
        assert_eq!(
            call(&echo, SysinfoQueryId::GLOBAL_PROCESS_LIST, &[]),
            Err(CallError::PermissionDenied)
        );
    }

    #[test]
    fn call_maps_other_errno_to_service() {
        let echo = Echo {
            reply: Err(Errno::NotFound),
            seen: core::cell::RefCell::new(Vec::new()),
        };
        assert_eq!(
            call(&echo, SysinfoQueryId::UPTIME, &[]),
            Err(CallError::Service(Errno::NotFound))
        );
    }
}
