//! Host unit tests for the shared tty line discipline.
//!
//! These pin the exact cooking behaviour both the kernel console and the
//! pseudo-terminal depend on: `ONLCR` output translation with POSIX
//! short-write accounting ([`write_cooked`]), the local-echo state machine
//! with bounded rub-out and split Delete-sequence handling
//! ([`EchoLine::echo`]), and the `^C`/`^Z` classifier ([`job_control_signal`]).

use alloc::vec::Vec;

use tairix_abi::{Errno, Signal};

use super::{job_control_signal, write_cooked, EchoLine, INTERRUPT_BYTE, STOP_BYTE};

#[test]
fn job_control_signal_maps_only_ctrl_c_and_ctrl_z() {
    assert_eq!(job_control_signal(INTERRUPT_BYTE), Some(Signal::Interrupt));
    assert_eq!(job_control_signal(STOP_BYTE), Some(Signal::Stop));
    assert_eq!(job_control_signal(b'a'), None);
    assert_eq!(job_control_signal(b'\r'), None);
    assert_eq!(job_control_signal(b'\n'), None);
    assert_eq!(job_control_signal(0x08), None);
}

#[test]
fn write_cooked_translates_a_bare_lf_to_crlf() {
    let mut out = Vec::new();
    let consumed = write_cooked(b"ab\ncd", |run| {
        out.extend_from_slice(run);
        Ok(run.len())
    })
    .expect("sink accepts everything");
    // Five input bytes consumed, but the LF rendered as CR LF on the device.
    assert_eq!(consumed, 5);
    assert_eq!(out, b"ab\r\ncd");
}

#[test]
fn write_cooked_passes_a_bare_cr_through_unchanged() {
    let mut out = Vec::new();
    let consumed = write_cooked(b"a\rb", |run| {
        out.extend_from_slice(run);
        Ok(run.len())
    })
    .expect("sink accepts everything");
    assert_eq!(consumed, 3);
    assert_eq!(out, b"a\rb");
}

#[test]
fn write_cooked_cooks_a_leading_and_only_lf() {
    let mut out = Vec::new();
    let consumed = write_cooked(b"\n", |run| {
        out.extend_from_slice(run);
        Ok(run.len())
    })
    .expect("sink accepts everything");
    assert_eq!(consumed, 1);
    assert_eq!(out, b"\r\n");
}

#[test]
fn write_cooked_cooks_every_lf_even_after_a_cr() {
    let mut out = Vec::new();
    let consumed = write_cooked(b"\r\n", |run| {
        out.extend_from_slice(run);
        Ok(run.len())
    })
    .expect("sink accepts everything");
    // The CR passes through, and the following LF still expands to CR LF.
    assert_eq!(consumed, 2);
    assert_eq!(out, b"\r\r\n");
}

#[test]
fn write_cooked_on_an_inert_sink_fails_closed() {
    let err = write_cooked(b"abc", |_run| Err(Errno::NotImplemented))
        .expect_err("an inert sink surfaces its error before any byte is consumed");
    assert_eq!(err, Errno::NotImplemented);
}

#[test]
fn write_cooked_reports_a_short_run_write_as_a_short_consume() {
    // The sink accepts only the first byte of the leading run, then would
    // accept no more: the reported consume count is exactly the accepted run
    // prefix, so the caller loops on the remainder.
    let mut first = true;
    let consumed = write_cooked(b"abc\n", |_run| {
        if first {
            first = false;
            Ok(1)
        } else {
            Ok(0)
        }
    })
    .expect("first write consumed a byte");
    assert_eq!(consumed, 1);
}

#[test]
fn write_cooked_maps_a_short_crlf_write_back_to_one_input_byte() {
    // The device accepts the run, then only the CR of the expanded pair on
    // the first offer and the LF on the retry: the single input LF still
    // counts once, never twice.
    let mut out = Vec::new();
    let mut crlf_first = true;
    let consumed = write_cooked(b"a\n", |run| {
        if run == b"\r\n" && crlf_first {
            crlf_first = false;
            out.extend_from_slice(b"\r");
            Ok(1)
        } else {
            out.extend_from_slice(run);
            Ok(run.len())
        }
    })
    .expect("sink makes progress");
    assert_eq!(consumed, 2);
    assert_eq!(out, b"a\r\n");
}

#[test]
fn echo_writes_printable_bytes_verbatim() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(b"hello", |b| out.extend_from_slice(b));
    assert_eq!(out, b"hello");
}

#[test]
fn echo_translates_cr_and_lf_to_crlf() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(b"a\rb\nc", |b| out.extend_from_slice(b));
    assert_eq!(out, b"a\r\nb\r\nc");
}

#[test]
fn echo_rubs_out_the_previous_character_on_backspace() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(b"ab", |b| out.extend_from_slice(b));
    out.clear();
    line.echo(&[0x08], |b| out.extend_from_slice(b));
    assert_eq!(out, b"\x08 \x08");
}

#[test]
fn echo_accepts_del_as_an_erase_too() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(b"a", |b| out.extend_from_slice(b));
    out.clear();
    line.echo(&[0x7f], |b| out.extend_from_slice(b));
    assert_eq!(out, b"\x08 \x08");
}

#[test]
fn echo_erase_at_line_start_is_a_no_op() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(&[0x08], |b| out.extend_from_slice(b));
    assert!(
        out.is_empty(),
        "erase with nothing to rub out draws nothing"
    );
}

#[test]
fn echo_column_persists_across_calls() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(b"x", |b| out.extend_from_slice(b));
    line.echo(b"y", |b| out.extend_from_slice(b));
    out.clear();
    // Two characters were typed across two calls; two erases rub both out and
    // the third is a no-op at the line start.
    line.echo(&[0x08, 0x08, 0x08], |b| out.extend_from_slice(b));
    assert_eq!(out, b"\x08 \x08\x08 \x08");
}

#[test]
fn echo_line_terminator_resets_the_erase_bound() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(b"ab\n", |b| out.extend_from_slice(b));
    out.clear();
    // After the newline a fresh line starts at column zero: an erase rubs out
    // nothing rather than walking back into the finished line.
    line.echo(&[0x08], |b| out.extend_from_slice(b));
    assert!(out.is_empty());
}

#[test]
fn echo_reset_starts_a_fresh_line() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(b"abc", |b| out.extend_from_slice(b));
    line.reset();
    out.clear();
    line.echo(&[0x08], |b| out.extend_from_slice(b));
    assert!(out.is_empty(), "reset zeroes the rub-out bound");
}

#[test]
fn echo_rubs_out_on_the_delete_key_sequence() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(b"a", |b| out.extend_from_slice(b));
    out.clear();
    // The Delete key's `CSI 3 ~` erases one character and never paints the
    // raw escape glyphs.
    line.echo(b"\x1b[3~", |b| out.extend_from_slice(b));
    assert_eq!(out, b"\x08 \x08");
}

#[test]
fn echo_delete_sequence_survives_split_reads() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    line.echo(b"a", |b| out.extend_from_slice(b));
    out.clear();
    // The escape sequence arrives across three reads; only the completing
    // read rubs out, and no escape glyph is drawn on the way.
    line.echo(b"\x1b", |b| out.extend_from_slice(b));
    line.echo(b"[3", |b| out.extend_from_slice(b));
    assert!(out.is_empty(), "an incomplete Delete prefix draws nothing");
    line.echo(b"~", |b| out.extend_from_slice(b));
    assert_eq!(out, b"\x08 \x08");
}

#[test]
fn echo_a_broken_delete_prefix_echoes_literally() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    // `ESC [` then a non-`3` byte: the held prefix is released as literal
    // input rather than silently dropped.
    line.echo(b"\x1b[x", |b| out.extend_from_slice(b));
    assert_eq!(out, b"\x1b[x");
}

#[test]
fn echo_batches_a_long_printable_run() {
    let mut out = Vec::new();
    let mut line = EchoLine::new();
    // Longer than one internal run buffer: every byte still reaches the sink
    // in order, exactly once.
    let input: Vec<u8> = (0..200u32).map(|i| b'a' + (i % 26) as u8).collect();
    line.echo(&input, |b| out.extend_from_slice(b));
    assert_eq!(out, input);
}
