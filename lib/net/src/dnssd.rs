//! The naming vocabulary of DNS-based service discovery (RFC 6763),
//! `plans/ZEROCONF.md` Z2.
//!
//! A service instance is named by a triple — an instance label, a service
//! type, and a domain — spelled as one DNS name
//! (`Hall Printer._ipp._tcp.local`), and described by the key/value
//! attributes of a `TXT` record. This module is that grammar and nothing
//! else: it parses and builds, owns no state, and allocates nothing.
//!
//! [`crate::mdns`] carries the records and this reads them, because the two
//! layers judge differently: `mdns` holds validated wire octets whatever
//! they spell, and the key/value reading of a `TXT` record is a consumer's
//! interpretation that must not decide whether the record was well formed.
//!
//! # Security
//!
//! Every name and every `TXT` octet here was authored by an unauthenticated
//! peer that chooses its own arrival rate, so parsing is total, bounded, and
//! allocation-free, and costs time linear in the record rather than
//! quadratic — an attacker picking our CPU cost is a denial-of-service
//! channel, not a performance question. A string this grammar cannot read is
//! dropped whole, never guessed at.
//!
//! What is validated is *structure*: the RFC's length, character, and
//! encoding rules. This is deliberately not a display filter. An instance
//! name is free-form UTF-8, and making one safe to draw is one shared policy
//! above this crate rather than a second sanitiser here — which is why
//! [`InstanceName`] will not render itself.

use core::fmt::{self, Write};

use crate::dns::{DnsError, Name, MAX_LABEL_LEN, MAX_NAME_LEN};
use crate::mdns::{TxtError, TxtRecord, TxtStrings, MAX_TXT_LEN};

/// The longest service name (RFC 6335 §5.1), the portion of `_ipp._tcp`
/// between the underscore and the transport.
///
/// A fixed validation bound: the registry's own limit, so a name past it
/// could never have been assigned. The discovery channel carries service
/// types, so it is defined there and taken here.
pub const MAX_SERVICE_NAME_LEN: usize = tairix_abi::discovery_ipc::SERVICE_NAME_MAX;

/// The longest one `TXT` string (RFC 1035 §3.3.14), whose length is carried
/// in a single octet. [`crate::mdns::MAX_TXT_LEN`] bounds the whole record.
pub const MAX_TXT_STRING_LEN: usize = 255;

/// Why a name or one of its parts was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DnsSdError {
    /// The name was not shaped like a service name: too few labels, or a
    /// service label without its leading underscore.
    NotAServiceName,
    /// The name carried no domain beneath the service type.
    DomainMissing,
    /// The instance label was empty or past [`MAX_LABEL_LEN`].
    InstanceLength,
    /// The instance label was not valid UTF-8 (RFC 6763 §4.1.1 requires
    /// Net-Unicode).
    InstanceEncoding,
    /// The instance label held an ASCII control byte.
    InstanceControl,
    /// The service name was empty or past [`MAX_SERVICE_NAME_LEN`].
    ServiceNameLength,
    /// The service name held a byte outside `A-Za-z0-9-`.
    ServiceNameCharacter,
    /// The service name had a leading, trailing, or doubled hyphen.
    ServiceNameHyphen,
    /// The service name was all digits and hyphens; RFC 6335 §5.1 requires
    /// at least one letter.
    ServiceNameNoLetter,
    /// The transport label was neither `_tcp` nor `_udp`.
    Transport,
    /// The parts were individually valid but will not spell one DNS name.
    Dns(DnsError),
}

/// The transport a service type runs over (RFC 6763 §4.1.2), defined once
/// with the channel that carries it.
pub use tairix_abi::discovery_ipc::Transport;

/// Both transport labels are four octets, which sizes [`ASSEMBLY_LEN`].
const TRANSPORT_LABEL_LEN: usize = Transport::Tcp.label().len();
const _: () = assert!(Transport::Udp.label().len() == TRANSPORT_LABEL_LEN);

/// A service type: the registered service name and its transport, spelled
/// `_ipp._tcp` (RFC 6763 §4.1.2).
///
/// The stored name omits the leading underscore, which is grammar rather
/// than part of the name RFC 6335 §5.1 governs. Case is preserved and
/// compared without, as a DNS label is.
#[derive(Clone, Copy)]
pub struct ServiceType {
    octets: [u8; MAX_SERVICE_NAME_LEN],
    len: u8,
    transport: Transport,
}

impl ServiceType {
    /// Validate `name` against RFC 6335 §5.1 and pair it with `transport`.
    pub fn new(name: &[u8], transport: Transport) -> Result<Self, DnsSdError> {
        if name.is_empty() || name.len() > MAX_SERVICE_NAME_LEN {
            return Err(DnsSdError::ServiceNameLength);
        }
        let mut has_letter = false;
        let mut after_hyphen = false;
        for (index, &byte) in name.iter().enumerate() {
            match byte {
                b'-' => {
                    if index == 0 || index + 1 == name.len() || after_hyphen {
                        return Err(DnsSdError::ServiceNameHyphen);
                    }
                    after_hyphen = true;
                }
                letter_or_digit if letter_or_digit.is_ascii_alphanumeric() => {
                    has_letter |= letter_or_digit.is_ascii_alphabetic();
                    after_hyphen = false;
                }
                _ => return Err(DnsSdError::ServiceNameCharacter),
            }
        }
        if !has_letter {
            return Err(DnsSdError::ServiceNameNoLetter);
        }
        let mut octets = [0u8; MAX_SERVICE_NAME_LEN];
        octets
            .get_mut(..name.len())
            .ok_or(DnsSdError::ServiceNameLength)?
            .copy_from_slice(name);
        Ok(Self {
            octets,
            len: u8::try_from(name.len()).map_err(|_| DnsSdError::ServiceNameLength)?,
            transport,
        })
    }

    /// The registered service name, without its leading underscore.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        let end = usize::from(self.len);
        self.octets.get(..end).unwrap_or(&[])
    }

    /// The transport the type runs over.
    #[must_use]
    pub const fn transport(&self) -> Transport {
        self.transport
    }

    /// Split a service-type name — `_ipp._tcp.local` — into the type and
    /// the domain beneath it.
    ///
    /// This is the name a browse `PTR` query asks about.
    pub fn from_name(name: &Name) -> Result<(Self, Name), DnsSdError> {
        let mut labels = name.labels();
        let service_label = labels.next().ok_or(DnsSdError::NotAServiceName)?;
        let transport_label = labels.next().ok_or(DnsSdError::NotAServiceName)?;
        let domain = suffix(name, 2 + service_label.len() + transport_label.len())?;
        check_domain(&domain)?;
        Ok((Self::from_labels(service_label, transport_label)?, domain))
    }

    /// Spell the type under `domain` as one DNS name.
    pub fn to_name(&self, domain: &Name) -> Result<Name, DnsSdError> {
        check_domain(domain)?;
        assemble(None, self, domain)
    }

    /// The type a `_<name>` label and a transport label spell.
    fn from_labels(service_label: &[u8], transport_label: &[u8]) -> Result<Self, DnsSdError> {
        let transport = Transport::from_label(transport_label).ok_or(DnsSdError::Transport)?;
        let name = service_label
            .strip_prefix(b"_")
            .ok_or(DnsSdError::NotAServiceName)?;
        Self::new(name, transport)
    }
}

/// Case-insensitive, as every DNS label comparison is (RFC 4343).
impl PartialEq for ServiceType {
    fn eq(&self, other: &Self) -> bool {
        self.transport == other.transport && self.name().eq_ignore_ascii_case(other.name())
    }
}

impl Eq for ServiceType {}

/// Renders `_ipp._tcp`. Every octet is alphanumeric or a hyphen, so unlike
/// an instance name this is safe to draw as it stands.
impl fmt::Display for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('_')?;
        for &byte in self.name() {
            f.write_char(char::from(byte))?;
        }
        f.write_char('.')?;
        f.write_str(self.transport.label())
    }
}

impl fmt::Debug for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServiceType({self})")
    }
}

/// A service instance's own label: the user-visible name of one instance
/// (RFC 6763 §4.1.1).
///
/// Free-form Net-Unicode UTF-8 rather than a host name, bounded by the DNS
/// label ceiling and refusing the ASCII controls the RFC forbids. The octets
/// are validated UTF-8, so `core::str::from_utf8` on them cannot fail; there
/// is deliberately no infallible `as_str`, because one would need an
/// unreachable unwrap and would make an unsanitised display path look like
/// the obvious one.
///
/// Normalisation is *not* applied: RFC 5198 asks for NFC and the tables that
/// needs do not belong in a `no_std` wire crate, so two spellings that
/// normalise alike remain two names here, exactly as they are on the wire.
#[derive(Clone, Copy)]
pub struct InstanceName {
    octets: [u8; MAX_LABEL_LEN],
    len: u8,
}

impl InstanceName {
    /// Validate `octets` as an instance label.
    pub fn new(octets: &[u8]) -> Result<Self, DnsSdError> {
        if octets.is_empty() || octets.len() > MAX_LABEL_LEN {
            return Err(DnsSdError::InstanceLength);
        }
        if core::str::from_utf8(octets).is_err() {
            return Err(DnsSdError::InstanceEncoding);
        }
        // Every control byte is single-byte in UTF-8, so scanning octets is
        // exactly scanning characters.
        if octets.iter().any(|&byte| byte < 0x20 || byte == 0x7F) {
            return Err(DnsSdError::InstanceControl);
        }
        let mut held = [0u8; MAX_LABEL_LEN];
        held.get_mut(..octets.len())
            .ok_or(DnsSdError::InstanceLength)?
            .copy_from_slice(octets);
        Ok(Self {
            octets: held,
            len: u8::try_from(octets.len()).map_err(|_| DnsSdError::InstanceLength)?,
        })
    }

    /// The validated label octets.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        let end = usize::from(self.len);
        self.octets.get(..end).unwrap_or(&[])
    }
}

/// Case-insensitive, as every DNS label comparison is (RFC 4343): two
/// instance names differing only in ASCII case are one name, and so
/// conflict.
impl PartialEq for InstanceName {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes().eq_ignore_ascii_case(other.as_bytes())
    }
}

impl Eq for InstanceName {}

/// Prints the octet count rather than the octets: an instance name is
/// peer-authored text and a debug line is not a display-safe surface.
impl fmt::Debug for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InstanceName({} octets)", self.len)
    }
}

/// One named service instance: `<instance>._<type>._<transport>.<domain>`
/// (RFC 6763 §4.1).
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ServiceInstance {
    instance: InstanceName,
    service: ServiceType,
    domain: Name,
}

impl ServiceInstance {
    /// Pair an instance with the type and domain it is published under.
    pub fn new(
        instance: InstanceName,
        service: ServiceType,
        domain: Name,
    ) -> Result<Self, DnsSdError> {
        check_domain(&domain)?;
        Ok(Self {
            instance,
            service,
            domain,
        })
    }

    /// The instance's own label.
    #[must_use]
    pub const fn instance(&self) -> &InstanceName {
        &self.instance
    }

    /// The service type it is published under.
    #[must_use]
    pub const fn service(&self) -> &ServiceType {
        &self.service
    }

    /// The domain it is published in.
    #[must_use]
    pub const fn domain(&self) -> &Name {
        &self.domain
    }

    /// Split a service instance name into its three parts.
    pub fn from_name(name: &Name) -> Result<Self, DnsSdError> {
        let instance_label = name.labels().next().ok_or(DnsSdError::NotAServiceName)?;
        // What follows the instance label is an ordinary service-type name,
        // so the type/domain split has one implementation.
        let rest = suffix(name, 1 + instance_label.len())?;
        let (service, domain) = ServiceType::from_name(&rest)?;
        Self::new(InstanceName::new(instance_label)?, service, domain)
    }

    /// Spell the instance as one DNS name.
    ///
    /// Fails with [`DnsError::NameTooLong`] when the three parts are each
    /// valid but jointly outgrow the 255-octet name bound.
    pub fn to_name(&self) -> Result<Name, DnsSdError> {
        assemble(Some(&self.instance), &self.service, &self.domain)
    }
}

/// Prints the instance as an octet count, because it is peer-authored text,
/// and the domain through the escaping presentation form a [`Name`] renders
/// in. Deriving this would dump the name's whole 255-octet backing array.
impl fmt::Debug for ServiceInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ServiceInstance({:?}, {}, {})",
            self.instance, self.service, self.domain
        )
    }
}

/// A DNS-SD name always sits under a domain; the root is not one.
fn check_domain(domain: &Name) -> Result<(), DnsSdError> {
    if domain.labels().next().is_none() {
        return Err(DnsSdError::DomainMissing);
    }
    Ok(())
}

/// The sub-name starting at `offset`, which must be a label boundary of
/// `name`'s own wire.
///
/// A label length never has the two high bits a compression pointer is
/// spelled with, so reading a name's own octets can only walk labels.
fn suffix(name: &Name, offset: usize) -> Result<Name, DnsSdError> {
    Name::read(name.as_wire(), offset)
        .map(|(suffix, _)| suffix)
        .ok_or(DnsSdError::NotAServiceName)
}

/// The widest concatenation [`assemble`] lays out before the name bound is
/// applied: an instance label, the two service-type labels, and a whole
/// domain.
const ASSEMBLY_LEN: usize =
    (1 + MAX_LABEL_LEN) + (1 + 1 + MAX_SERVICE_NAME_LEN) + (1 + TRANSPORT_LABEL_LEN) + MAX_NAME_LEN;

/// Spell a service type — with an instance label before it, or without —
/// under `domain`.
///
/// Built as wire octets and read back rather than assembled from a label
/// list, so a deep domain costs no arbitrary label ceiling.
fn assemble(
    instance: Option<&InstanceName>,
    service: &ServiceType,
    domain: &Name,
) -> Result<Name, DnsSdError> {
    let too_long = DnsSdError::Dns(DnsError::NameTooLong);
    let name = service.name();
    let mut service_label = [0u8; 1 + MAX_SERVICE_NAME_LEN];
    service_label[0] = b'_';
    service_label
        .get_mut(1..=name.len())
        .ok_or(too_long)?
        .copy_from_slice(name);
    let service_label = service_label.get(..=name.len()).ok_or(too_long)?;

    let mut wire = [0u8; ASSEMBLY_LEN];
    let mut len = 0usize;
    if let Some(instance) = instance {
        len = write_label(&mut wire, len, instance.as_bytes()).ok_or(too_long)?;
    }
    len = write_label(&mut wire, len, service_label).ok_or(too_long)?;
    len = write_label(&mut wire, len, service.transport.label().as_bytes()).ok_or(too_long)?;

    let domain_wire = domain.as_wire();
    let end = len.checked_add(domain_wire.len()).ok_or(too_long)?;
    wire.get_mut(len..end)
        .ok_or(too_long)?
        .copy_from_slice(domain_wire);
    // Every label written above is inside its own bound, so the 255-octet
    // whole-name ceiling is the only rejection the reader can reach.
    Name::read(wire.get(..end).ok_or(too_long)?, 0)
        .map(|(assembled, _)| assembled)
        .ok_or(too_long)
}

/// Write one length-prefixed label at `len`, or `None` when it will not fit.
fn write_label(wire: &mut [u8], len: usize, label: &[u8]) -> Option<usize> {
    let prefix = u8::try_from(label.len()).ok()?;
    let end = len.checked_add(1)?.checked_add(label.len())?;
    let (head, body) = wire.get_mut(len..end)?.split_first_mut()?;
    *head = prefix;
    body.copy_from_slice(label);
    Some(end)
}

/// What one `TXT` attribute carries (RFC 6763 §6.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TxtValue<'a> {
    /// The string held no `=`: the attribute is present with no value, a
    /// boolean.
    Flag,
    /// Everything after the first `=`, which may be empty and may be any
    /// binary data.
    Value(&'a [u8]),
}

/// One key/value attribute of a `TXT` record (RFC 6763 §6.3, §6.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TxtAttribute<'a> {
    /// The key as the publisher spelled it; compare it with
    /// [`Self::has_key`], never with `==`, because keys are
    /// case-insensitive.
    pub key: &'a [u8],
    /// What the attribute carries.
    pub value: TxtValue<'a>,
}

impl TxtAttribute<'_> {
    /// Whether this is the attribute named `key`, ignoring case.
    #[must_use]
    pub fn has_key(&self, key: &[u8]) -> bool {
        self.key.eq_ignore_ascii_case(key)
    }
}

/// The attributes of a `TXT` record, in the order they were published.
///
/// Strings with no usable key are dropped as RFC 6763 §6.3 requires: an
/// empty one, one beginning with `=`, and one whose key holds a byte outside
/// printable US-ASCII.
///
/// Duplicates are **not** removed, because doing so would cost time
/// quadratic in a record a peer authored. RFC 6763 §6.5 says a reader takes
/// the first occurrence of a repeated key, which is what [`Self::get`] does;
/// a consumer that iterates instead must apply that rule itself.
#[derive(Clone, Debug)]
pub struct TxtAttributes<'a> {
    octets: &'a [u8],
    strings: TxtStrings<'a>,
}

impl<'a> TxtAttributes<'a> {
    /// Read `record`'s attributes.
    #[must_use]
    pub fn new(record: &'a TxtRecord) -> Self {
        Self::over(record.as_octets())
    }

    /// The first value published under `key`, ignoring case — the
    /// occurrence RFC 6763 §6.5 says a reader takes.
    ///
    /// Always from the start of the record, never from wherever iteration
    /// has reached: a lookup relative to a cursor would answer with a
    /// duplicate the RFC says to ignore.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<TxtValue<'a>> {
        Self::over(self.octets)
            .find(|attribute| attribute.has_key(key))
            .map(|attribute| attribute.value)
    }

    /// Over rdata octets already validated as a run of length-prefixed
    /// strings: what [`TxtBuilder`] reads back before it has a record.
    fn over(octets: &'a [u8]) -> Self {
        Self {
            octets,
            strings: TxtStrings::over(octets),
        }
    }
}

impl<'a> Iterator for TxtAttributes<'a> {
    type Item = TxtAttribute<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let string = self.strings.next()?;
            if let Some(attribute) = read_attribute(string) {
                return Some(attribute);
            }
        }
    }
}

impl core::iter::FusedIterator for TxtAttributes<'_> {}

/// Split one `TXT` string into its key and value, or `None` for one RFC
/// 6763 §6.3 says a reader silently ignores.
fn read_attribute(string: &[u8]) -> Option<TxtAttribute<'_>> {
    let (key, value) = match string.iter().position(|&byte| byte == b'=') {
        Some(split) => (
            string.get(..split)?,
            TxtValue::Value(string.get(split + 1..)?),
        ),
        None => (string, TxtValue::Flag),
    };
    if key.is_empty() || !key.iter().all(|&byte| is_key_byte(byte)) {
        return None;
    }
    Some(TxtAttribute { key, value })
}

/// Whether `byte` may appear in a key: printable US-ASCII, and `=` only
/// because it is what ended the key (RFC 6763 §6.3).
fn is_key_byte(byte: u8) -> bool {
    (0x20..=0x7E).contains(&byte)
}

/// Builds a `TXT` record from key/value attributes.
///
/// The publication side of [`TxtAttributes`]: it refuses a key the RFC's
/// grammar does not admit and a key already written, so a record this emits
/// reads back as the attribute set it was given. The duplicate check rescans
/// what is already written, which is quadratic in the attribute count — a
/// service builds its record once, off any hot path.
#[derive(Clone)]
pub struct TxtBuilder {
    octets: [u8; MAX_TXT_LEN],
    len: u16,
}

impl Default for TxtBuilder {
    fn default() -> Self {
        Self {
            octets: [0u8; MAX_TXT_LEN],
            len: 0,
        }
    }
}

impl TxtBuilder {
    /// An empty attribute set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an attribute.
    ///
    /// # Errors
    ///
    /// [`TxtBuildError`] for an unusable or repeated key, a string past
    /// [`MAX_TXT_STRING_LEN`], or a record that would pass
    /// [`crate::mdns::MAX_TXT_LEN`].
    pub fn push(&mut self, key: &[u8], value: TxtValue<'_>) -> Result<(), TxtBuildError> {
        if key.is_empty() {
            return Err(TxtBuildError::KeyEmpty);
        }
        if !key.iter().all(|&byte| is_key_byte(byte) && byte != b'=') {
            return Err(TxtBuildError::KeyCharacter);
        }
        if TxtAttributes::over(self.written()).any(|attribute| attribute.has_key(key)) {
            return Err(TxtBuildError::DuplicateKey);
        }
        let value_len = match value {
            TxtValue::Flag => 0,
            TxtValue::Value(bytes) => 1 + bytes.len(),
        };
        let string_len = key.len() + value_len;
        if string_len > MAX_TXT_STRING_LEN {
            return Err(TxtBuildError::StringTooLong);
        }
        let start = usize::from(self.len);
        let end = start + 1 + string_len;
        let slot = self.octets.get_mut(start..end).ok_or(TxtBuildError::Full)?;
        let (prefix, body) = slot.split_first_mut().ok_or(TxtBuildError::Full)?;
        *prefix = u8::try_from(string_len).map_err(|_| TxtBuildError::StringTooLong)?;
        let (key_slot, rest) = body.split_at_mut(key.len());
        key_slot.copy_from_slice(key);
        if let TxtValue::Value(bytes) = value {
            let (equals, value_slot) = rest.split_first_mut().ok_or(TxtBuildError::Full)?;
            *equals = b'=';
            value_slot.copy_from_slice(bytes);
        }
        self.len = u16::try_from(end).map_err(|_| TxtBuildError::Full)?;
        Ok(())
    }

    /// The record built so far.
    ///
    /// # Errors
    ///
    /// Delegates to the one `TXT` validator rather than asserting the
    /// invariant here; a builder that accepted its pushes cannot fail it.
    pub fn build(&self) -> Result<TxtRecord, TxtError> {
        TxtRecord::new(self.written())
    }

    /// The rdata octets written so far.
    fn written(&self) -> &[u8] {
        let end = usize::from(self.len);
        self.octets.get(..end).unwrap_or(&[])
    }
}

/// Prints the octet count rather than the octets, as a `TXT` record does.
impl fmt::Debug for TxtBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TxtBuilder({} octets)", self.len)
    }
}

/// Why [`TxtBuilder::push`] refused an attribute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TxtBuildError {
    /// The key was empty; RFC 6763 §6.3 requires at least one character.
    KeyEmpty,
    /// The key held a byte outside printable US-ASCII, or an `=`.
    KeyCharacter,
    /// The key was already written, and a reader would ignore this one.
    DuplicateKey,
    /// The key and value together passed [`MAX_TXT_STRING_LEN`].
    StringTooLong,
    /// The record would pass [`crate::mdns::MAX_TXT_LEN`].
    Full,
}

#[cfg(test)]
#[path = "dnssd_tests.rs"]
mod tests;
