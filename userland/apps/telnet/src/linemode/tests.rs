//! Host tests for RFC 1184: the `MODE` acknowledgement discipline, every SLC
//! level and acknowledgement transition, and `FORWARDMASK`.

use alloc::vec::Vec;

use super::{
    mode, slc, slc_flag, slc_function, slc_name, sub, ForwardMask, Linemode, SlcTable, SLC_MAX,
    SLC_NOVALUE,
};
use crate::nvt::{NvtEvent, Parser, DO, DONT, IAC, SE, WILL, WONT};
use crate::option;

/// Decode `bytes` back through the real receive parser, returning every
/// LINEMODE subnegotiation payload it holds. Asserting an encoder against the
/// parser that must read it is what keeps the two from drifting.
fn payloads(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut parser = Parser::new();
    let mut out = Vec::new();
    parser.feed(bytes, |event| {
        if let NvtEvent::Subnegotiation { option, params } = event {
            assert_eq!(option, option::LINEMODE);
            out.push(params.to_vec());
        }
    });
    out
}

/// The one payload `bytes` holds.
fn payload(bytes: &[u8]) -> Vec<u8> {
    let mut all = payloads(bytes);
    assert_eq!(all.len(), 1, "expected exactly one subnegotiation: {all:?}");
    all.remove(0)
}

// --- MODE -------------------------------------------------------------------

#[test]
fn a_server_stated_mode_is_adopted_and_acknowledged_exactly_once() {
    let mut lm = Linemode::new();
    let outcome = lm.fold(&[sub::MODE, mode::EDIT | mode::TRAPSIG]);
    assert_eq!(outcome.mode_changed, Some(mode::EDIT | mode::TRAPSIG));
    assert!(lm.edit() && lm.trapsig());
    assert_eq!(
        payload(&outcome.reply),
        alloc::vec![sub::MODE, mode::EDIT | mode::TRAPSIG | mode::MODE_ACK]
    );

    // Restating the same mask needs no second acknowledgement: that is what
    // stops a server which repeats itself from cycling.
    let repeat = lm.fold(&[sub::MODE, mode::EDIT | mode::TRAPSIG]);
    assert_eq!(repeat.mode_changed, None);
    assert!(repeat.reply.is_empty());
}

#[test]
fn our_requested_mode_takes_effect_only_when_the_server_acknowledges_it() {
    let mut lm = Linemode::new();
    let mut wire = Vec::new();
    lm.request_mode(mode::EDIT | mode::SOFT_TAB, &mut wire);
    assert_eq!(
        payload(&wire),
        alloc::vec![sub::MODE, mode::EDIT | mode::SOFT_TAB]
    );
    assert_eq!(
        lm.mask(),
        0,
        "the request is not adopted optimistically — the two ends would be \
         editing on different rules"
    );

    let outcome = lm.fold(&[sub::MODE, mode::EDIT | mode::SOFT_TAB | mode::MODE_ACK]);
    assert_eq!(outcome.mode_changed, Some(mode::EDIT | mode::SOFT_TAB));
    assert!(lm.edit() && lm.soft_tab());
    assert!(
        outcome.reply.is_empty(),
        "an acknowledgement is never itself acknowledged"
    );
}

#[test]
fn an_acknowledgement_of_something_else_is_ignored() {
    let mut lm = Linemode::new();
    let mut wire = Vec::new();
    lm.request_mode(mode::EDIT, &mut wire);
    let outcome = lm.fold(&[sub::MODE, mode::LIT_ECHO | mode::MODE_ACK]);
    assert_eq!(outcome.mode_changed, None);
    assert_eq!(lm.mask(), 0, "we asked for EDIT, not LIT_ECHO");
    assert!(outcome.reply.is_empty());
}

#[test]
fn the_mode_ack_bit_is_never_stored_as_a_mode() {
    let mut lm = Linemode::new();
    lm.fold(&[sub::MODE, mode::EDIT | mode::MODE_ACK]);
    assert_eq!(lm.mask() & mode::MODE_ACK, 0);
}

#[test]
fn a_malformed_mode_payload_is_refused_whole() {
    for payload in [
        alloc::vec![sub::MODE],
        alloc::vec![sub::MODE, mode::EDIT, mode::EDIT],
    ] {
        let mut lm = Linemode::new();
        let outcome = lm.fold(&payload);
        assert!(outcome.refused, "{payload:?}");
        assert_eq!(lm.mask(), 0);
        assert!(outcome.reply.is_empty());
    }
}

#[test]
fn an_empty_linemode_payload_is_refused() {
    let mut lm = Linemode::new();
    assert!(lm.fold(&[]).refused);
}

// --- SLC --------------------------------------------------------------------

#[test]
fn a_value_the_server_names_is_accepted_and_acknowledged() {
    let mut table = SlcTable::new();
    let reply = table.fold(&[slc::EC, slc_flag::VALUE, 0x08]);
    assert_eq!(table.char_for(slc::EC), Some(0x08));
    assert_eq!(
        reply,
        alloc::vec![slc::EC, slc_flag::VALUE | slc_flag::ACK, 0x08]
    );
}

#[test]
fn an_acknowledgement_matching_our_value_settles_the_exchange() {
    let mut table = SlcTable::new();
    let ours = table.char_for(slc::EL).expect("a default kill character");
    let reply = table.fold(&[slc::EL, slc_flag::VALUE | slc_flag::ACK, ours]);
    assert!(
        reply.is_empty(),
        "an acknowledgement is never replied to, which is what ends the exchange"
    );
    assert_eq!(table.char_for(slc::EL), Some(ours));
}

#[test]
fn an_acknowledgement_of_a_value_we_do_not_hold_changes_nothing() {
    let mut table = SlcTable::new();
    let ours = table.char_for(slc::EL).expect("a default kill character");
    let reply = table.fold(&[slc::EL, slc_flag::VALUE | slc_flag::ACK, 0x7B]);
    assert!(reply.is_empty());
    assert_eq!(table.char_for(slc::EL), Some(ours));
}

#[test]
fn the_default_level_makes_us_state_our_own_value() {
    let mut table = SlcTable::new();
    table.fold(&[slc::EC, slc_flag::VALUE, 0x08]);
    let reply = table.fold(&[slc::EC, slc_flag::DEFAULT, SLC_NOVALUE]);
    assert_eq!(
        reply,
        alloc::vec![slc::EC, slc_flag::VALUE, 0x7F],
        "our own default is restored and stated"
    );
    assert_eq!(table.char_for(slc::EC), Some(0x7F));
}

#[test]
fn nosupport_from_the_server_disables_the_function() {
    let mut table = SlcTable::new();
    let reply = table.fold(&[slc::EW, slc_flag::NOSUPPORT, SLC_NOVALUE]);
    assert_eq!(table.char_for(slc::EW), None);
    assert_eq!(
        reply,
        alloc::vec![slc::EW, slc_flag::NOSUPPORT | slc_flag::ACK, SLC_NOVALUE],
        "a function only one end performs is no function at all"
    );
}

#[test]
fn cantchange_pins_the_character_and_a_later_set_is_refused() {
    let mut table = SlcTable::new();
    let reply = table.fold(&[slc::IP, slc_flag::CANTCHANGE, 0x03]);
    assert_eq!(
        reply,
        alloc::vec![slc::IP, slc_flag::CANTCHANGE | slc_flag::ACK, 0x03]
    );
    assert!(
        !table.set_local(slc::IP, 0x01),
        "the operator is told rather than disagreeing silently with the server"
    );
    assert!(!table.unset_local(slc::IP));
    assert_eq!(table.char_for(slc::IP), Some(0x03));
}

#[test]
fn a_value_we_cannot_change_is_answered_with_what_we_hold() {
    let mut table = SlcTable::new();
    table.fold(&[slc::IP, slc_flag::CANTCHANGE, 0x03]);
    let reply = table.fold(&[slc::IP, slc_flag::VALUE, 0x1B]);
    assert_eq!(
        reply,
        alloc::vec![slc::IP, slc_flag::CANTCHANGE, 0x03],
        "we restate what we hold rather than pretending to change it"
    );
    assert_eq!(table.char_for(slc::IP), Some(0x03));
}

#[test]
fn an_undefined_function_is_answered_nosupport_and_never_stored() {
    let mut table = SlcTable::new();
    for function in [0u8, SLC_MAX + 1, 200, 255] {
        let reply = table.fold(&[function, slc_flag::VALUE, b'x']);
        assert_eq!(
            reply,
            alloc::vec![function, slc_flag::NOSUPPORT, SLC_NOVALUE],
            "function {function}"
        );
        assert_eq!(table.get(function), None);
    }
}

#[test]
fn a_truncated_trailing_triplet_does_not_discard_the_valid_ones() {
    let mut table = SlcTable::new();
    let reply = table.fold(&[
        slc::EC,
        slc_flag::VALUE,
        0x08,
        // One stray byte: a whole valid triplet must still be honoured.
        slc::EL,
    ]);
    assert_eq!(table.char_for(slc::EC), Some(0x08));
    assert_eq!(
        reply,
        alloc::vec![slc::EC, slc_flag::VALUE | slc_flag::ACK, 0x08]
    );
}

#[test]
fn an_slc_exchange_terminates() {
    // A server that keeps restating a value gets one acknowledgement per
    // statement and no cascade: the reply is bounded by the triplets received.
    let mut table = SlcTable::new();
    for _ in 0..100 {
        let reply = table.fold(&[slc::EC, slc_flag::VALUE, 0x08]);
        assert_eq!(reply.len(), 3);
        let ack = table.fold(&[slc::EC, slc_flag::VALUE | slc_flag::ACK, 0x08]);
        assert!(ack.is_empty());
    }
}

#[test]
fn a_hostile_table_cannot_make_the_reply_unbounded() {
    // Every one of 512 triplets is answered by exactly one, so the reply is
    // linear in the input and the input is already bounded by the parser.
    let mut table = SlcTable::new();
    let mut params = Vec::new();
    for _ in 0..512 {
        params.extend_from_slice(&[slc::EC, slc_flag::VALUE, 0x08]);
    }
    assert_eq!(table.fold(&params).len(), 512 * 3);
}

#[test]
fn the_exported_table_round_trips_through_the_parser() {
    let table = SlcTable::new();
    let mut out = Vec::new();
    table.push_export(&mut out);
    let params = payload(&out);
    assert_eq!(params[0], sub::SLC);
    assert_eq!(
        params.len(),
        1 + usize::from(SLC_MAX) * 3,
        "one triplet per defined function"
    );
    for (index, triplet) in params[1..].as_chunks::<3>().0.iter().enumerate() {
        let function = u8::try_from(index + 1).expect("SLC_MAX fits u8");
        assert_eq!(triplet[0], function);
        let entry = table.get(function).expect("a defined function");
        assert_eq!((triplet[1], triplet[2]), (entry.flags, entry.value));
    }
}

#[test]
fn a_locally_set_character_is_found_both_ways() {
    let mut table = SlcTable::new();
    assert!(table.set_local(slc::EC, b'#'));
    assert_eq!(table.char_for(slc::EC), Some(b'#'));
    assert_eq!(table.function_for(b'#'), Some(slc::EC));
    assert!(table.unset_local(slc::EC));
    assert_eq!(table.char_for(slc::EC), None);
    assert_eq!(table.function_for(b'#'), None);
}

#[test]
fn a_duplicate_binding_resolves_in_function_order() {
    let mut table = SlcTable::new();
    assert!(table.set_local(slc::EW, b'@'));
    assert!(table.set_local(slc::RP, b'@'));
    assert_eq!(
        table.function_for(b'@'),
        Some(slc::EW),
        "deterministic when a server binds one character twice"
    );
}

#[test]
fn the_no_value_byte_is_never_reported_as_a_binding() {
    let table = SlcTable::new();
    assert_eq!(table.function_for(SLC_NOVALUE), None);
}

#[test]
fn set_and_unset_refuse_an_out_of_range_function() {
    let mut table = SlcTable::new();
    for function in [0u8, SLC_MAX + 1, 255] {
        assert!(!table.set_local(function, b'x'), "{function}");
        assert!(!table.unset_local(function), "{function}");
    }
}

#[test]
fn every_function_has_a_name_and_the_names_resolve_back() {
    for function in 1..=SLC_MAX {
        let name = slc_name(function).expect("every defined function is named");
        assert_eq!(slc_function(name), Some(function), "{name}");
    }
    assert_eq!(slc_name(0), None);
    assert_eq!(slc_name(SLC_MAX + 1), None);
    assert_eq!(slc_function("escape"), None, "not an SLC function");
}

// --- FORWARDMASK ------------------------------------------------------------

#[test]
fn a_forwardmask_request_is_accepted_and_the_named_characters_forward() {
    let mut lm = Linemode::new();
    // Bit 7 of octet 0 is code 0; the `;` at 0x3B is octet 7, bit 4.
    let mut mask = alloc::vec![0u8; ForwardMask::LEN];
    mask[7] = 1 << (7 - (0x3B & 0x07));
    let mut request = alloc::vec![DO, sub::FORWARDMASK];
    request.extend_from_slice(&mask);
    let outcome = lm.fold(&request);
    assert!(!outcome.refused);
    assert_eq!(payload(&outcome.reply), alloc::vec![WILL, sub::FORWARDMASK]);
    assert!(lm.forwards(b';'));
    assert!(!lm.forwards(b':'));
}

#[test]
fn a_short_forwardmask_names_only_the_low_codes() {
    let mut lm = Linemode::new();
    // One octet covers codes 0..=7; bit 7 is code 0, so 0x08 names code 4.
    let outcome = lm.fold(&[DO, sub::FORWARDMASK, 0x08]);
    assert!(!outcome.refused);
    assert!(lm.forwards(4));
    assert!(!lm.forwards(b';'), "the unstated octets are zero");
}

#[test]
fn an_over_long_forwardmask_is_refused_and_nothing_forwards() {
    let mut lm = Linemode::new();
    let mut request = alloc::vec![DO, sub::FORWARDMASK];
    request.extend(core::iter::repeat_n(0xFF, ForwardMask::LEN + 1));
    let outcome = lm.fold(&request);
    assert!(outcome.refused);
    assert_eq!(payload(&outcome.reply), alloc::vec![WONT, sub::FORWARDMASK]);
    assert!(
        !lm.forwards(b'x'),
        "a refused mask leaves forwarding off, not partly applied"
    );
}

#[test]
fn dont_forwardmask_clears_the_mask_and_answers_wont() {
    let mut lm = Linemode::new();
    lm.fold(&[DO, sub::FORWARDMASK, 0xFF]);
    assert!(lm.forwards(0));
    let outcome = lm.fold(&[DONT, sub::FORWARDMASK]);
    assert_eq!(payload(&outcome.reply), alloc::vec![WONT, sub::FORWARDMASK]);
    assert!(!lm.forwards(0));
}

#[test]
fn a_server_offering_to_forward_is_refused() {
    // The client is the forwarder; a server that offers to do it has nothing a
    // client can act on.
    for verb in [WILL, WONT] {
        let mut lm = Linemode::new();
        let outcome = lm.fold(&[verb, sub::FORWARDMASK]);
        assert_eq!(
            payload(&outcome.reply),
            alloc::vec![DONT, sub::FORWARDMASK],
            "verb {verb}"
        );
    }
}

#[test]
fn forwarding_is_off_until_a_mask_is_agreed() {
    let lm = Linemode::new();
    assert!(!lm.forwards(0));
    assert!(!lm.forwards(0xFF));
    assert!(ForwardMask::empty().is_empty());
}

#[test]
fn a_full_mask_names_every_code() {
    let mask = ForwardMask::parse(&[0xFF; ForwardMask::LEN]).expect("a full mask");
    assert!(!mask.is_empty());
    for byte in 0u16..=255 {
        let byte = u8::try_from(byte).expect("0..=255 fits u8");
        assert!(mask.contains(byte), "code {byte}");
    }
}

#[test]
fn an_unknown_linemode_sub_option_is_refused() {
    let mut lm = Linemode::new();
    let outcome = lm.fold(&[99, 1, 2]);
    assert!(outcome.refused);
    assert!(outcome.reply.is_empty());
}

#[test]
fn reset_clears_every_negotiated_fact() {
    let mut lm = Linemode::new();
    lm.fold(&[sub::MODE, mode::EDIT | mode::LIT_ECHO]);
    lm.fold(&[DO, sub::FORWARDMASK, 0xFF]);
    lm.slc_mut().set_local(slc::EC, b'#');
    lm.reset();
    assert_eq!(lm.mask(), 0);
    assert!(!lm.forwards(0));
    assert_eq!(
        lm.slc().char_for(slc::EC),
        Some(0x7F),
        "back to our default"
    );
}

#[test]
fn an_slc_subnegotiation_reply_is_framed_as_one_linemode_message() {
    let mut lm = Linemode::new();
    let outcome = lm.fold(&[sub::SLC, slc::EC, slc_flag::VALUE, 0x08]);
    let params = payload(&outcome.reply);
    assert_eq!(params[0], sub::SLC);
    assert_eq!(
        &params[1..],
        &[slc::EC, slc_flag::VALUE | slc_flag::ACK, 0x08]
    );
    assert_eq!(outcome.reply[0], IAC);
    assert_eq!(&outcome.reply[outcome.reply.len() - 2..], &[IAC, SE]);
}

#[test]
fn an_slc_message_of_pure_acknowledgements_sends_nothing() {
    let mut lm = Linemode::new();
    let ours = lm.slc().char_for(slc::EL).expect("a default");
    let outcome = lm.fold(&[sub::SLC, slc::EL, slc_flag::VALUE | slc_flag::ACK, ours]);
    assert!(outcome.reply.is_empty());
    assert!(!outcome.refused);
}
