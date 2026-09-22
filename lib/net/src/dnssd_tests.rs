//! Unit tests for the DNS-SD naming and attribute grammar.

use super::*;

fn name(dotted: &str) -> Name {
    Name::encode(dotted).expect("a test name encodes")
}

fn service(name: &[u8], transport: Transport) -> ServiceType {
    ServiceType::new(name, transport).expect("a test service type is well formed")
}

fn instance(label: &[u8]) -> InstanceName {
    InstanceName::new(label).expect("a test instance name is well formed")
}

/// A `TXT` record built from whole strings, so a test never has to count
/// length prefixes by hand.
fn txt(strings: &[&[u8]]) -> TxtRecord {
    let mut octets = alloc::vec::Vec::new();
    for string in strings {
        octets.push(u8::try_from(string.len()).expect("a test string fits one length octet"));
        octets.extend_from_slice(string);
    }
    TxtRecord::new(&octets).expect("a test TXT is well formed")
}

// -- the service-name grammar (RFC 6335 §5.1) ----------------------------

#[test]
fn a_service_name_takes_letters_digits_and_interior_hyphens() {
    for accepted in [&b"ipp"[..], b"a", b"x-1", b"dns-sd", b"http", b"afpovertcp"] {
        assert!(
            ServiceType::new(accepted, Transport::Tcp).is_ok(),
            "{accepted:?} is a registrable service name"
        );
    }
}

#[test]
fn a_service_name_outside_its_length_bound_is_refused() {
    assert_eq!(
        ServiceType::new(b"", Transport::Tcp),
        Err(DnsSdError::ServiceNameLength)
    );
    let oversize = alloc::vec![b'a'; MAX_SERVICE_NAME_LEN + 1];
    assert_eq!(
        ServiceType::new(&oversize, Transport::Tcp),
        Err(DnsSdError::ServiceNameLength)
    );
    let widest = alloc::vec![b'a'; MAX_SERVICE_NAME_LEN];
    assert!(ServiceType::new(&widest, Transport::Tcp).is_ok());
}

#[test]
fn a_service_name_outside_the_registered_character_set_is_refused() {
    for refused in [&b"ip p"[..], b"ipp_x", b"ipp.tcp", b"_ipp", b"ipp\x01"] {
        assert_eq!(
            ServiceType::new(refused, Transport::Tcp),
            Err(DnsSdError::ServiceNameCharacter),
            "{refused:?}"
        );
    }
}

#[test]
fn a_leading_trailing_or_doubled_hyphen_is_refused() {
    for refused in [&b"-ipp"[..], b"ipp-", b"ip--p", b"-", b"--"] {
        assert_eq!(
            ServiceType::new(refused, Transport::Tcp),
            Err(DnsSdError::ServiceNameHyphen),
            "{refused:?}"
        );
    }
}

#[test]
fn a_service_name_with_no_letter_is_refused() {
    for refused in [&b"123"[..], b"1-2", b"9"] {
        assert_eq!(
            ServiceType::new(refused, Transport::Tcp),
            Err(DnsSdError::ServiceNameNoLetter),
            "{refused:?}"
        );
    }
}

#[test]
fn a_service_type_compares_without_ascii_case_but_keeps_its_spelling() {
    let upper = service(b"IPP", Transport::Tcp);
    assert_eq!(upper, service(b"ipp", Transport::Tcp));
    assert_eq!(upper.name(), b"IPP");
    assert_ne!(upper, service(b"ipp", Transport::Udp));
    assert_ne!(upper, service(b"ipps", Transport::Tcp));
}

#[test]
fn a_transport_label_is_read_without_ascii_case_and_never_guessed_at() {
    assert_eq!(Transport::from_label(b"_TcP"), Some(Transport::Tcp));
    assert_eq!(Transport::from_label(b"_udp"), Some(Transport::Udp));
    assert_eq!(Transport::from_label(b"_sctp"), None);
    assert_eq!(Transport::from_label(b"tcp"), None);
}

#[test]
fn a_service_type_renders_as_its_two_labels() {
    assert_eq!(
        alloc::format!("{}", service(b"ipp", Transport::Tcp)),
        "_ipp._tcp"
    );
    assert_eq!(
        alloc::format!("{:?}", service(b"http", Transport::Udp)),
        "ServiceType(_http._udp)"
    );
}

// -- the instance grammar (RFC 6763 §4.1.1) ------------------------------

#[test]
fn an_instance_name_is_free_form_utf8_within_the_label_bound() {
    assert_eq!(
        instance("Hall Printer".as_bytes()).as_bytes(),
        b"Hall Printer"
    );
    assert_eq!(
        instance("Café (2)".as_bytes()).as_bytes(),
        "Café (2)".as_bytes()
    );
    let widest = alloc::vec![b'i'; MAX_LABEL_LEN];
    assert!(InstanceName::new(&widest).is_ok());
}

#[test]
fn an_instance_name_outside_the_label_bound_is_refused() {
    assert_eq!(InstanceName::new(b""), Err(DnsSdError::InstanceLength));
    let oversize = alloc::vec![b'i'; MAX_LABEL_LEN + 1];
    assert_eq!(
        InstanceName::new(&oversize),
        Err(DnsSdError::InstanceLength)
    );
}

#[test]
fn an_instance_name_that_is_not_utf8_is_refused() {
    assert_eq!(
        InstanceName::new(&[0xFF, 0xFE]),
        Err(DnsSdError::InstanceEncoding)
    );
    // A truncated multi-byte sequence is the same refusal.
    assert_eq!(
        InstanceName::new(&[0xE2, 0x82]),
        Err(DnsSdError::InstanceEncoding)
    );
}

#[test]
fn an_instance_name_holding_an_ascii_control_is_refused() {
    for refused in [
        &b"bad\x00name"[..],
        b"bad\x01name",
        b"bad\x1fname",
        b"bad\x7fname",
    ] {
        assert_eq!(
            InstanceName::new(refused),
            Err(DnsSdError::InstanceControl),
            "{refused:?}"
        );
    }
}

#[test]
fn an_instance_name_compares_without_ascii_case_but_keeps_its_spelling() {
    let spelled = instance(b"Hall Printer");
    assert_eq!(spelled, instance(b"hall printer"));
    assert_eq!(spelled.as_bytes(), b"Hall Printer");
    assert_ne!(spelled, instance(b"Hall  Printer"));
}

#[test]
fn an_instance_name_debug_does_not_print_peer_authored_text() {
    let rendered = alloc::format!("{:?}", instance(b"Evil Twin"));
    assert!(!rendered.contains("Evil"), "{rendered}");
    assert_eq!(rendered, "InstanceName(9 octets)");
}

// -- the instance / type / domain triple (RFC 6763 §4.1) -----------------

#[test]
fn a_service_instance_round_trips_through_its_dns_name() {
    let triple = ServiceInstance::new(
        instance("Hall Printer".as_bytes()),
        service(b"ipp", Transport::Tcp),
        name("local"),
    )
    .expect("a triple under a real domain");
    let spelled = triple.to_name().expect("the triple fits one name");
    assert_eq!(
        spelled,
        Name::from_labels(&[b"Hall Printer", b"_ipp", b"_tcp", b"local"]).expect("the same labels")
    );
    assert_eq!(
        ServiceInstance::from_name(&spelled).expect("it parses back"),
        triple
    );
}

#[test]
fn a_multi_label_domain_survives_the_round_trip() {
    let triple = ServiceInstance::new(
        instance(b"Desk"),
        service(b"http", Transport::Udp),
        name("sub.example.com"),
    )
    .expect("a triple under a deep domain");
    let spelled = triple.to_name().expect("the triple fits one name");
    let parsed = ServiceInstance::from_name(&spelled).expect("it parses back");
    assert_eq!(parsed.domain(), &name("sub.example.com"));
    assert_eq!(parsed.service(), &service(b"http", Transport::Udp));
    assert_eq!(parsed.instance().as_bytes(), b"Desk");
}

#[test]
fn parsing_a_name_preserves_the_case_the_publisher_spelled() {
    let spelled =
        Name::from_labels(&[b"Hall Printer", b"_IPP", b"_TCP", b"Local"]).expect("labels encode");
    let parsed = ServiceInstance::from_name(&spelled).expect("it parses");
    assert_eq!(parsed.instance().as_bytes(), b"Hall Printer");
    assert_eq!(parsed.service().name(), b"IPP");
    assert_eq!(parsed.service().transport(), Transport::Tcp);
}

#[test]
fn a_service_type_name_splits_into_the_type_and_its_domain() {
    let (parsed, domain) = ServiceType::from_name(&name("_ipp._tcp.local")).expect("a browse name");
    assert_eq!(parsed, service(b"ipp", Transport::Tcp));
    assert_eq!(domain, name("local"));
    assert_eq!(
        parsed.to_name(&domain).expect("it spells back"),
        name("_ipp._tcp.local")
    );
}

#[test]
fn the_service_enumeration_name_needs_no_special_case() {
    // RFC 6763 §9 spells the meta-query as an instance-shaped name, so the
    // ordinary triple reads it and a browse never grows a second parser.
    let parsed = ServiceInstance::from_name(&name("_services._dns-sd._udp.local"))
        .expect("the meta-query name is an ordinary triple");
    assert_eq!(parsed.instance().as_bytes(), b"_services");
    assert_eq!(parsed.service(), &service(b"dns-sd", Transport::Udp));
    assert_eq!(parsed.domain(), &name("local"));
}

#[test]
fn a_name_with_no_domain_beneath_the_type_is_refused() {
    assert_eq!(
        ServiceInstance::new(
            instance(b"Desk"),
            service(b"ipp", Transport::Tcp),
            Name::root()
        ),
        Err(DnsSdError::DomainMissing)
    );
    assert_eq!(
        ServiceInstance::from_name(
            &Name::from_labels(&[b"Desk", b"_ipp", b"_tcp"]).expect("labels encode")
        ),
        Err(DnsSdError::DomainMissing)
    );
    assert_eq!(
        ServiceType::from_name(&Name::from_labels(&[b"_ipp", b"_tcp"]).expect("labels encode")),
        Err(DnsSdError::DomainMissing)
    );
    assert_eq!(
        service(b"ipp", Transport::Tcp).to_name(&Name::root()),
        Err(DnsSdError::DomainMissing)
    );
}

#[test]
fn a_name_that_is_not_shaped_like_a_service_name_is_refused() {
    assert_eq!(
        ServiceInstance::from_name(&name("printer.local")),
        Err(DnsSdError::NotAServiceName)
    );
    assert_eq!(
        ServiceInstance::from_name(&Name::root()),
        Err(DnsSdError::NotAServiceName)
    );
    // A service label without its leading underscore is not one.
    assert_eq!(
        ServiceInstance::from_name(
            &Name::from_labels(&[b"Desk", b"ipp", b"_tcp", b"local"]).expect("labels encode")
        ),
        Err(DnsSdError::NotAServiceName)
    );
}

#[test]
fn an_unknown_transport_is_refused_rather_than_assumed() {
    assert_eq!(
        ServiceInstance::from_name(
            &Name::from_labels(&[b"Desk", b"_ipp", b"_sctp", b"local"]).expect("labels encode")
        ),
        Err(DnsSdError::Transport)
    );
}

#[test]
fn parts_that_are_each_valid_but_jointly_too_long_are_refused() {
    let label = [b'd'; 60];
    let domain = Name::from_labels(&[&label, &label, &label, &label]).expect("a deep domain");
    let widest = alloc::vec![b'i'; MAX_LABEL_LEN];
    let triple = ServiceInstance::new(instance(&widest), service(b"ipp", Transport::Tcp), domain)
        .expect("the parts are individually valid");
    assert_eq!(
        triple.to_name(),
        Err(DnsSdError::Dns(DnsError::NameTooLong))
    );
}

#[test]
fn a_service_instance_debug_names_its_parts_without_drawing_the_instance() {
    let triple = ServiceInstance::new(
        instance(b"Evil Twin"),
        service(b"ipp", Transport::Tcp),
        name("local"),
    )
    .expect("a triple");
    let rendered = alloc::format!("{triple:?}");
    assert!(!rendered.contains("Evil"), "{rendered}");
    assert_eq!(
        rendered,
        "ServiceInstance(InstanceName(9 octets), _ipp._tcp, local)"
    );
}

// -- the TXT key/value grammar (RFC 6763 §6) -----------------------------

#[test]
fn an_attribute_is_absent_a_flag_an_empty_value_or_a_value() {
    let record = txt(&[b"paper", b"pdl=", b"rp=queue"]);
    let attributes = TxtAttributes::new(&record);
    assert_eq!(attributes.get(b"absent"), None);
    assert_eq!(attributes.get(b"paper"), Some(TxtValue::Flag));
    assert_eq!(attributes.get(b"pdl"), Some(TxtValue::Value(&[])));
    assert_eq!(attributes.get(b"rp"), Some(TxtValue::Value(b"queue")));
}

#[test]
fn a_value_may_hold_any_binary_including_further_equals_signs() {
    let record = txt(&[b"k=a=b", b"raw=\x00\xff"]);
    let attributes = TxtAttributes::new(&record);
    assert_eq!(attributes.get(b"k"), Some(TxtValue::Value(b"a=b")));
    assert_eq!(attributes.get(b"raw"), Some(TxtValue::Value(&[0x00, 0xFF])));
}

#[test]
fn a_string_with_no_usable_key_is_silently_ignored() {
    // An empty string, one whose key is missing, and one whose key holds a
    // byte outside printable US-ASCII.
    let record = txt(&[b"", b"=orphan", b"a\x01b=v", b"ok=1"]);
    let keys: alloc::vec::Vec<&[u8]> = TxtAttributes::new(&record)
        .map(|attribute| attribute.key)
        .collect();
    assert_eq!(keys, alloc::vec![&b"ok"[..]]);
}

#[test]
fn the_empty_attribute_set_yields_nothing() {
    assert_eq!(TxtAttributes::new(&TxtRecord::empty()).count(), 0);
}

#[test]
fn a_repeated_key_reads_as_its_first_occurrence() {
    let record = txt(&[b"paper=a4", b"other=x", b"PAPER=letter"]);
    let attributes = TxtAttributes::new(&record);
    assert_eq!(attributes.get(b"paper"), Some(TxtValue::Value(b"a4")));
    // The iterator does not deduplicate — removing a duplicate would cost
    // time quadratic in a record a peer authored.
    let keys: alloc::vec::Vec<&[u8]> = TxtAttributes::new(&record)
        .map(|attribute| attribute.key)
        .collect();
    assert_eq!(
        keys,
        alloc::vec![&b"paper"[..], &b"other"[..], &b"PAPER"[..]]
    );
}

#[test]
fn a_lookup_is_first_wins_even_on_a_partly_read_record() {
    let record = txt(&[b"paper=a4", b"PAPER=letter"]);
    let mut attributes = TxtAttributes::new(&record);
    attributes.next();
    // A lookup is not relative to where iteration has reached; otherwise it
    // would answer with the duplicate the RFC says to ignore.
    assert_eq!(attributes.get(b"paper"), Some(TxtValue::Value(b"a4")));
}

#[test]
fn a_key_is_looked_up_without_ascii_case() {
    let record = txt(&[b"TxtVers=1"]);
    let attributes = TxtAttributes::new(&record);
    assert_eq!(attributes.get(b"txtvers"), Some(TxtValue::Value(b"1")));
    assert_eq!(attributes.get(b"TXTVERS"), Some(TxtValue::Value(b"1")));
    // The key is surfaced as the publisher spelled it.
    let first = TxtAttributes::new(&record).next().expect("one attribute");
    assert_eq!(first.key, b"TxtVers");
    assert!(first.has_key(b"txtvers"));
}

#[test]
fn a_space_in_a_key_is_significant() {
    let record = txt(&[b"a b=1"]);
    let attributes = TxtAttributes::new(&record);
    assert_eq!(attributes.get(b"a b"), Some(TxtValue::Value(b"1")));
    assert_eq!(attributes.get(b"ab"), None);
}

// -- building a TXT record -----------------------------------------------

#[test]
fn a_built_record_reads_back_as_the_attributes_it_was_given() {
    let mut builder = TxtBuilder::new();
    builder
        .push(b"txtvers", TxtValue::Value(b"1"))
        .expect("a first attribute");
    builder.push(b"paper", TxtValue::Flag).expect("a flag");
    builder
        .push(b"pdl", TxtValue::Value(&[]))
        .expect("an empty value");
    let record = builder.build().expect("a well-formed record");
    let attributes = TxtAttributes::new(&record);
    assert_eq!(attributes.get(b"txtvers"), Some(TxtValue::Value(b"1")));
    assert_eq!(attributes.get(b"paper"), Some(TxtValue::Flag));
    assert_eq!(attributes.get(b"pdl"), Some(TxtValue::Value(&[])));
    assert_eq!(record, txt(&[b"txtvers=1", b"paper", b"pdl="]));
}

#[test]
fn an_empty_builder_yields_the_empty_attribute_set() {
    assert_eq!(
        TxtBuilder::new().build().expect("the empty set"),
        TxtRecord::empty()
    );
}

#[test]
fn a_repeated_key_is_refused_at_build_time_rather_than_shadowed() {
    let mut builder = TxtBuilder::new();
    builder.push(b"paper", TxtValue::Flag).expect("a first key");
    assert_eq!(
        builder.push(b"PAPER", TxtValue::Value(b"a4")),
        Err(TxtBuildError::DuplicateKey)
    );
    assert_eq!(builder.build().expect("still one key"), txt(&[b"paper"]));
}

#[test]
fn a_key_the_grammar_does_not_admit_is_refused() {
    let mut builder = TxtBuilder::new();
    assert_eq!(
        builder.push(b"", TxtValue::Flag),
        Err(TxtBuildError::KeyEmpty)
    );
    for refused in [&b"a=b"[..], b"a\x01b", b"a\x7fb", b"caf\xc3\xa9"] {
        assert_eq!(
            builder.push(refused, TxtValue::Flag),
            Err(TxtBuildError::KeyCharacter),
            "{refused:?}"
        );
    }
    assert_eq!(
        builder.build().expect("nothing was written"),
        TxtRecord::empty()
    );
}

#[test]
fn a_string_past_its_length_octet_is_refused() {
    let mut builder = TxtBuilder::new();
    let key = alloc::vec![b'k'; MAX_TXT_STRING_LEN];
    // The key alone is the widest string there is; one more octet cannot be
    // spelled in a length prefix.
    assert!(builder.push(&key, TxtValue::Flag).is_ok());
    let mut builder = TxtBuilder::new();
    assert_eq!(
        builder.push(&key, TxtValue::Value(&[])),
        Err(TxtBuildError::StringTooLong)
    );
    let mut builder = TxtBuilder::new();
    let oversize = alloc::vec![b'k'; MAX_TXT_STRING_LEN + 1];
    assert_eq!(
        builder.push(&oversize, TxtValue::Flag),
        Err(TxtBuildError::StringTooLong)
    );
}

#[test]
fn a_record_past_its_fixed_bound_is_refused_rather_than_truncated() {
    let mut builder = TxtBuilder::new();
    let value = alloc::vec![b'v'; 200];
    let mut written = 0usize;
    let mut refused = false;
    for index in 0..MAX_TXT_LEN {
        let key = alloc::format!("k{index}");
        match builder.push(key.as_bytes(), TxtValue::Value(&value)) {
            Ok(()) => written += 1,
            Err(error) => {
                assert_eq!(error, TxtBuildError::Full);
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "the bound is reachable");
    assert!(written > 0, "attributes fit before the bound");
    let record = builder.build().expect("what was accepted is well formed");
    assert!(record.as_octets().len() <= MAX_TXT_LEN);
    assert_eq!(TxtAttributes::new(&record).count(), written);
}

#[test]
fn builder_debug_does_not_print_the_octets() {
    let mut builder = TxtBuilder::new();
    builder
        .push(b"key", TxtValue::Value(b"secret"))
        .expect("an attribute");
    let rendered = alloc::format!("{builder:?}");
    assert!(!rendered.contains("secret"), "{rendered}");
    assert_eq!(rendered, "TxtBuilder(11 octets)");
}
