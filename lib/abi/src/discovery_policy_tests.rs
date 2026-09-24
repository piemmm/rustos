//! Unit tests for the discovery grant store.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;

use super::*;

const PUBLISHER: &str = "0101010101010101010101010101010101010101010101010101010101010101";

fn ipp() -> ServiceTypeField<'static> {
    ServiceTypeField {
        name: b"ipp",
        transport: Transport::Tcp,
    }
}

/// The first four grants read, as (whose, the type's first three octets).
type Seen<'a> = [Option<(&'a str, [u8; 3])>; 4];

fn read(store: &str) -> Result<(Seen<'_>, usize), Errno> {
    let mut seen = [None; 4];
    let mut count = 0;
    read_grants(store.as_bytes(), &mut |grant| {
        let mut name = [0u8; 3];
        let len = grant.service.name.len().min(3);
        name[..len].copy_from_slice(&grant.service.name[..len]);
        let id = if grant.bundle_id == "os.tairix.dns-sd" {
            "os.tairix.dns-sd"
        } else {
            "other"
        };
        if let Some(slot) = seen.get_mut(count) {
            *slot = Some((id, name));
        }
        count += 1;
        Ok(())
    })?;
    Ok((seen, count))
}

#[test]
fn a_written_line_reads_back_as_the_grant_it_was() {
    let grant = Grant {
        bundle_id: "os.tairix.dns-sd",
        publisher: PublisherId::from_raw([1; PUBLISHER_ID_LEN]),
        service: ipp(),
    };
    let mut out = [0u8; 160];
    let len = grant.write_line(&mut out).unwrap();
    let line = core::str::from_utf8(&out[..len]).unwrap();
    assert_eq!(
        line,
        "browse os.tairix.dns-sd 0101010101010101010101010101010101010101010101010101010101010101 _ipp._tcp\n"
    );
    let mut back = None;
    read_grants(line.as_bytes(), &mut |read| {
        back = Some((
            read.publisher,
            read.service.transport,
            read.service.name.len(),
        ));
        assert_eq!(read.bundle_id, "os.tairix.dns-sd");
        Ok(())
    })
    .unwrap();
    assert_eq!(back, Some((grant.publisher, Transport::Tcp, 3)));
    assert_eq!(grant.write_line(&mut [0u8; 40]), Err(Errno::BufferTooSmall));
}

#[test]
fn comments_and_blank_lines_are_ignored() {
    let (seen, count) = read(store()).unwrap();
    assert_eq!(count, 2);
    assert_eq!(seen[0], Some(("os.tairix.dns-sd", *b"ipp")));
    assert_eq!(seen[1], Some(("other", *b"ssh")));
}

fn store() -> &'static str {
    concat!(
        "# written by the image builder\n",
        "\n",
        "browse os.tairix.dns-sd 0101010101010101010101010101010101010101010101010101010101010101 _ipp._tcp\n",
        "browse os.example.term 0101010101010101010101010101010101010101010101010101010101010101 _ssh._tcp\n",
    )
}

#[test]
fn one_malformed_line_refuses_the_whole_store_before_any_grant_is_taken() {
    for bad in [
        "grant os.tairix.x P _ipp._tcp",
        "browse os.tairix.x P _ipp._tcp extra",
        "browse os.tairix.x P ipp._tcp",
        "browse os.tairix.x P _ipp._sctp",
        "browse os.tairix.x P _IPP._tcp",
        "browse os.tairix.x P _ip.p._tcp",
        "browse os.tairix.x P _._tcp",
        "browse os.tairix.x P _sixteen-octets-x._tcp",
        "browse Not/An/Id P _ipp._tcp",
        "browse os.tairix.x  P _ipp._tcp",
    ] {
        let line = bad.replace('P', PUBLISHER);
        let mut store = String::from(store());
        store.push_str(&line);
        store.push('\n');
        let mut taken = 0;
        let result = read_grants(store.as_bytes(), &mut |_| {
            taken += 1;
            Ok(())
        });
        assert!(result.is_err(), "{bad}");
        assert_eq!(taken, 0, "{bad}: nothing is granted from a damaged store");
    }
}

#[test]
fn a_publisher_must_be_real_and_spelled_in_lowercase_hex() {
    for publisher in [
        "0000000000000000000000000000000000000000000000000000000000000000",
        "01010101010101010101010101010101010101010101010101010101010101",
        "0101010101010101010101010101010101010101010101010101010101010101ab",
        "0A01010101010101010101010101010101010101010101010101010101010101",
    ] {
        let line = format!("browse os.tairix.x {publisher} _ipp._tcp\n");
        assert_eq!(
            read_grants(line.as_bytes(), &mut |_| Ok(())),
            Err(Errno::OutOfRange),
            "{publisher}"
        );
    }
}

#[test]
fn a_store_past_its_bound_or_not_utf8_is_refused() {
    let big = vec![b'#'; DISCOVERY_POLICY_MAX + 1];
    assert_eq!(
        read_grants(&big, &mut |_| Ok(())),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        read_grants(&[0xFF, b'\n'], &mut |_| Ok(())),
        Err(Errno::OutOfRange)
    );
}
