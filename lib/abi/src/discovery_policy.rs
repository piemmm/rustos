//! The link-local discovery grant store (`plans/ZEROCONF.md` Z4): which
//! service types each application may browse for.
//!
//! One line per grant, keyed on the kernel-attested application identity:
//!
//! ```text
//! browse <bundle-id> <publisher> _<service>._<tcp|udp>
//! ```
//!
//! `publisher` is the [`PublisherId`] in lowercase hex. Blank lines and lines
//! beginning `#` are ignored; any other line that is not exactly this shape
//! refuses the whole store, so a damaged store grants nothing rather than
//! whatever of it happens to parse. The shape is judged here; the RFC 6335
//! grammar of the service name is `tairix_net::dnssd`'s, which the service
//! applies to every grant it loads.

use crate::appinfo::{validate_bundle_id, PublisherId, PUBLISHER_ID_LEN};
use crate::discovery_ipc::{ServiceTypeField, Transport, SERVICE_NAME_MAX};
use crate::Errno;

/// Where the store lives: on the read-only `/System` volume, which no
/// projection shadows, so its only writer is the image builder.
pub const DISCOVERY_POLICY_PATH: &str = "/System/Security/Policy/Discovery";

/// The largest store a reader accepts — a fixed bound on what it will read
/// and parse, far past every grant the shipped bundles can declare.
pub const DISCOVERY_POLICY_MAX: usize = 256 * 1024;

const BROWSE: &str = "browse";

/// One application's grant to browse for one service type.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Grant<'a> {
    /// The application's signed bundle identifier.
    pub bundle_id: &'a str,
    /// The developer identity its bundle is published under.
    pub publisher: PublisherId,
    /// The type it may browse for.
    pub service: ServiceTypeField<'a>,
}

impl Grant<'_> {
    /// The grant's line, newline included, into the front of `out`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `out` cannot hold it, and whatever
    /// [`validate_bundle_id`] refuses or [`Errno::LengthOutOfRange`] for a
    /// service name out of bounds, so no line is written that a reader would
    /// refuse.
    pub fn write_line(&self, out: &mut [u8]) -> Result<usize, Errno> {
        validate_bundle_id(self.bundle_id)?;
        let name = self.service.name;
        if name.is_empty() || name.len() > SERVICE_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut publisher = [0u8; PUBLISHER_ID_LEN * 2];
        let publisher = crate::hex::encode(self.publisher.as_bytes(), &mut publisher);
        let parts: [&[u8]; 10] = [
            BROWSE.as_bytes(),
            b" ",
            self.bundle_id.as_bytes(),
            b" ",
            publisher.as_bytes(),
            b" _",
            name,
            b".",
            self.service.transport.label().as_bytes(),
            b"\n",
        ];
        let len = parts.iter().map(|part| part.len()).sum();
        let line = out.get_mut(..len).ok_or(Errno::BufferTooSmall)?;
        let mut at = 0;
        for part in parts {
            line[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        Ok(len)
    }
}

/// Hand every grant in `store` to `each`, in order, after the whole store has
/// been read and found well-formed.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] for a store past [`DISCOVERY_POLICY_MAX`],
/// [`Errno::OutOfRange`] for text that is not UTF-8 or a line that is not a
/// grant, and whatever `each` returns.
pub fn read_grants(
    store: &[u8],
    each: &mut dyn FnMut(Grant<'_>) -> Result<(), Errno>,
) -> Result<(), Errno> {
    if store.len() > DISCOVERY_POLICY_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let text = core::str::from_utf8(store).map_err(|_| Errno::OutOfRange)?;
    for line in grant_lines(text) {
        parse_grant(line)?;
    }
    for line in grant_lines(text) {
        each(parse_grant(line)?)?;
    }
    Ok(())
}

fn grant_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

fn parse_grant(line: &str) -> Result<Grant<'_>, Errno> {
    let mut fields = line.split(' ');
    let (Some(BROWSE), Some(bundle_id), Some(publisher), Some(service), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return Err(Errno::OutOfRange);
    };
    validate_bundle_id(bundle_id)?;
    let publisher = PublisherId::from_raw(
        crate::hex::decode::<PUBLISHER_ID_LEN>(publisher.as_bytes()).ok_or(Errno::OutOfRange)?,
    );
    if publisher.is_none() {
        return Err(Errno::OutOfRange);
    }
    Ok(Grant {
        bundle_id,
        publisher,
        service: parse_service(service)?,
    })
}

/// `_<name>._<tcp|udp>`, spelled in lowercase as the image builder writes it.
fn parse_service(text: &str) -> Result<ServiceTypeField<'_>, Errno> {
    let (name, transport) = text
        .strip_prefix('_')
        .and_then(|rest| rest.rsplit_once('.'))
        .ok_or(Errno::OutOfRange)?;
    let transport = [Transport::Tcp, Transport::Udp]
        .into_iter()
        .find(|known| known.label() == transport)
        .ok_or(Errno::OutOfRange)?;
    if name.is_empty() || name.len() > SERVICE_NAME_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(Errno::OutOfRange);
    }
    Ok(ServiceTypeField {
        name: name.as_bytes(),
        transport,
    })
}

#[cfg(test)]
#[path = "discovery_policy_tests.rs"]
mod tests;
