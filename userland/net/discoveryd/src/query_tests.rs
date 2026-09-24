//! Unit tests for the query planner.

use super::{plan, Needs};
use crate::wire::Form;
use tairix_abi::discovery_ipc::{Families, Query, ServiceTypeField, Transport};
use tairix_abi::Errno;
use tairix_net::dns::Name;
use tairix_net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn ipp() -> ServiceTypeField<'static> {
    ServiceTypeField {
        name: b"ipp",
        transport: Transport::Tcp,
    }
}

fn name(dotted: &str) -> Name {
    Name::encode(dotted).unwrap()
}

#[test]
fn a_browse_asks_for_the_types_instances_and_needs_that_type() {
    let planned = plan(&Query::Browse { service: ipp() }).unwrap();
    assert!(matches!(planned.needs, Needs::Type(ty) if ty.name() == b"ipp"));
    assert_eq!(planned.asks.len(), 1);
    assert_eq!(planned.asks[0].form, Form::Instance);
    assert_eq!(planned.asks[0].name, name("_ipp._tcp.local"));
}

#[test]
fn a_resolve_asks_for_where_and_what() {
    let planned = plan(&Query::Resolve {
        instance: b"Hall Printer",
        service: ipp(),
    })
    .unwrap();
    let asks: [Form; 2] = [planned.asks[0].form, planned.asks[1].form];
    assert_eq!(asks, [Form::Service, Form::Text]);
    assert_eq!(
        planned.asks[0].name,
        Name::from_labels(&[b"Hall Printer", b"_ipp", b"_tcp", b"local"]).unwrap()
    );
    assert!(matches!(planned.needs, Needs::Type(_)));
}

#[test]
fn a_host_lookup_is_one_name_under_local() {
    let host = name("printer.local");
    let planned = plan(&Query::Host {
        name: host.as_wire(),
        families: Families::BOTH,
    })
    .unwrap();
    assert_eq!(planned.needs, Needs::Nothing);
    assert_eq!(planned.asks.len(), 2);
    assert_eq!(planned.asks[0].form, Form::AddressV4);
    assert_eq!(planned.asks[1].form, Form::AddressV6);
    let nested = name("a.b.LOCAL");
    let planned = plan(&Query::Host {
        name: nested.as_wire(),
        families: Families {
            v4: false,
            v6: true,
        },
    })
    .unwrap();
    assert_eq!(planned.asks.len(), 1);
    for refused in ["local", "printer.example.com", "printer.localhost"] {
        assert_eq!(
            plan(&Query::Host {
                name: name(refused).as_wire(),
                families: Families::BOTH,
            })
            .map(|_| ()),
            Err(Errno::OutOfRange),
            "{refused}"
        );
    }
}

#[test]
fn only_a_link_local_address_is_resolved_in_reverse() {
    let v4 = IpAddr::V4(Ipv4Addr::new(169, 254, 7, 9));
    let planned = plan(&Query::Reverse { address: v4 }).unwrap();
    assert_eq!(planned.asks[0].name, Name::reverse(v4));
    assert_eq!(planned.asks[0].form, Form::Pointer);
    let v6 = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
    assert!(plan(&Query::Reverse { address: v6 }).is_ok());
    for refused in [
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
    ] {
        assert_eq!(
            plan(&Query::Reverse { address: refused }).map(|_| ()),
            Err(Errno::OutOfRange)
        );
    }
}

#[test]
fn the_type_enumeration_needs_everything() {
    let planned = plan(&Query::Types).unwrap();
    assert_eq!(planned.needs, Needs::Everything);
    assert_eq!(planned.asks[0].name, name("_services._dns-sd._udp.local"));
    assert_eq!(planned.asks[0].form, Form::Type);
}

#[test]
fn a_service_or_instance_outside_its_grammar_is_refused() {
    for service in [&b"-ipp"[..], b"i p p", b"123"] {
        assert_eq!(
            plan(&Query::Browse {
                service: ServiceTypeField {
                    name: service,
                    transport: Transport::Tcp,
                },
            })
            .map(|_| ()),
            Err(Errno::OutOfRange)
        );
    }
    assert_eq!(
        plan(&Query::Resolve {
            instance: b"bell\x07",
            service: ipp(),
        })
        .map(|_| ()),
        Err(Errno::OutOfRange)
    );
}
