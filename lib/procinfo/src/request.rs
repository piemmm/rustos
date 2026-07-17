//! Framing a `sysinfo-v1` request and issuing it through the [`Transport`]
//! seam.
//!
//! Both the `sysinfo` and `ps` tools build the same [`SysinfoRequestHeader`]
//! envelope and map a capability denial onto the same distinguished error,
//! so that logic lives here rather than being copied.

use alloc::vec::Vec;

use tairix_abi::sysinfo::{
    SysinfoQueryId, SysinfoRequestHeader, SYSINFO_REQUEST_MAGIC, SYSINFO_VERSION_CURRENT,
};
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

/// Frame `query` and its already-encoded `payload` into a `sysinfo-v1`
/// request: the [`SysinfoRequestHeader`] envelope followed by the payload
/// bytes.
///
/// The header's `payload_len` is the payload's byte length. Every payload a
/// process-list client builds is at most a handful of bytes, so the
/// `usize → u32` conversion is always exact; the saturating fallback is
/// unreachable and never silently corrupts a real request.
#[must_use]
pub fn encode_request(query: SysinfoQueryId, payload: &[u8]) -> Vec<u8> {
    let header = SysinfoRequestHeader {
        magic: SYSINFO_REQUEST_MAGIC,
        version: SYSINFO_VERSION_CURRENT,
        flags: 0,
        query,
        reserved: 0,
        payload_len: u32::try_from(payload.len()).unwrap_or(u32::MAX),
        request_id: 0,
    };
    let mut bytes = Vec::with_capacity(SysinfoRequestHeader::WIRE_LEN + payload.len());
    bytes.extend_from_slice(&header.to_le_bytes());
    bytes.extend_from_slice(payload);
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
