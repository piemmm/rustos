//! The one text spelling of a network address every surface prints.
//!
//! An address reaches a reader from several places — the `info:`/`state:`
//! resolver reads, the Switchboard's interface section, the Settings DNS
//! pane — and a desktop that spelled the same address two ways would be
//! asking its reader to decide which is the machine's. So the rendering
//! lives here, once, and every caller reads through it.
//!
//! IPv6 is RFC 5952 canonical text — lowercase, leading zeros suppressed,
//! the leftmost longest run of two or more zero groups compressed to `::` —
//! because that is the spelling the standard fixes and the one every
//! command-line surface already prints.

use alloc::string::{String, ToString};

use tairix_abi::net_ipc::{NetAddrFamily, NetAddrState, NetIfAddr, NetServerAddr};

/// One address of `family`, held in the sixteen-byte slot both wire
/// records use (a V4 address occupies the first four bytes).
#[must_use]
pub fn render_ip(family: NetAddrFamily, addr: &[u8; 16]) -> String {
    match family {
        NetAddrFamily::V4 => render_ipv4(addr),
        NetAddrFamily::V6 => render_ipv6(addr),
    }
}

/// One configured server's address.
#[must_use]
pub fn render_server(server: &NetServerAddr) -> String {
    render_ip(server.family, &server.addr)
}

/// One bound interface address as `addr/prefix`, suffixed with its
/// DAD/SLAAC state where that state is anything but the ordinary
/// preferred one.
#[must_use]
pub fn render_if_addr(entry: &NetIfAddr) -> String {
    let mut out = render_ip(entry.family, &entry.addr);
    out.push('/');
    out.push_str(&entry.prefix.to_string());
    out.push_str(match entry.state {
        NetAddrState::Preferred => "",
        NetAddrState::Tentative => " (tentative)",
        NetAddrState::Deprecated => " (deprecated)",
    });
    out
}

/// The first four bytes of an address slot as dotted-quad text.
fn render_ipv4(addr: &[u8; 16]) -> String {
    let mut out = String::new();
    for (index, byte) in addr[..4].iter().enumerate() {
        if index > 0 {
            out.push('.');
        }
        out.push_str(&byte.to_string());
    }
    out
}

/// An address slot in RFC 5952 canonical text.
fn render_ipv6(octets: &[u8; 16]) -> String {
    let mut groups = [0u16; 8];
    for (index, group) in groups.iter_mut().enumerate() {
        *group = u16::from_be_bytes([octets[index * 2], octets[index * 2 + 1]]);
    }
    let (best_start, best_len) = longest_zero_run(&groups);
    let mut out = String::new();
    let mut index = 0;
    while index < groups.len() {
        if best_len >= 2 && index == best_start {
            out.push_str("::");
            index += best_len;
            continue;
        }
        if !out.is_empty() && !out.ends_with(':') {
            out.push(':');
        }
        push_u16_hex(&mut out, groups[index]);
        index += 1;
    }
    if out.is_empty() {
        out.push_str("::");
    }
    out
}

/// The leftmost longest run of zero groups, as `(start, len)`.
///
/// RFC 5952 compresses only a run of two or more and only the leftmost of
/// equal-length runs, so the caller applies both thresholds against this.
fn longest_zero_run(groups: &[u16; 8]) -> (usize, usize) {
    let (mut best_start, mut best_len) = (0usize, 0usize);
    let mut index = 0;
    while index < groups.len() {
        if groups[index] != 0 {
            index += 1;
            continue;
        }
        let start = index;
        while index < groups.len() && groups[index] == 0 {
            index += 1;
        }
        let len = index - start;
        if len >= 2 && len > best_len {
            best_start = start;
            best_len = len;
        }
    }
    (best_start, best_len)
}

/// Append `value` as minimal lowercase hex (no leading zeros).
fn push_u16_hex(out: &mut String, value: u16) {
    let mut started = false;
    for shift in [12u32, 8, 4, 0] {
        let nibble = (value >> shift) & 0xF;
        if nibble != 0 || started || shift == 0 {
            started = true;
            out.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
    }
}

#[cfg(test)]
#[path = "netaddr_tests.rs"]
mod tests;
