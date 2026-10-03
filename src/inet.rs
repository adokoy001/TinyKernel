//! Internet addresses (IPv4 and IPv6), their text forms, and the Internet
//! checksum. Pure logic without hardware access, shared by the kernel and
//! host tests.

use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpAddr {
    V4([u8; 4]),
    V6([u8; 16]),
}

impl IpAddr {
    pub fn is_unspecified(&self) -> bool {
        match self {
            IpAddr::V4(a) => *a == [0; 4],
            IpAddr::V6(a) => *a == [0; 16],
        }
    }

    pub fn is_multicast(&self) -> bool {
        match self {
            IpAddr::V4(a) => a[0] >> 4 == 0xe,
            IpAddr::V6(a) => a[0] == 0xff,
        }
    }
}

/// Parse dotted-quad IPv4 or RFC 4291 IPv6 text (with `::`, without zone
/// identifiers or an embedded IPv4 tail).
pub fn parse(text: &str) -> Option<IpAddr> {
    parse_v4(text).map(IpAddr::V4).or_else(|| parse_v6(text).map(IpAddr::V6))
}

pub fn parse_v4(text: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = text.split('.');
    for byte in out.iter_mut() {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *byte = part.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

pub fn parse_v6(text: &str) -> Option<[u8; 16]> {
    fn groups(part: &str, out: &mut [u16; 8]) -> Option<usize> {
        if part.is_empty() {
            return Some(0);
        }
        let mut count = 0;
        for group in part.split(':') {
            if group.is_empty() || group.len() > 4 || count == 8 || !group.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out[count] = u16::from_str_radix(group, 16).ok()?;
            count += 1;
        }
        Some(count)
    }
    let (mut head, mut tail) = ([0u16; 8], [0u16; 8]);
    let words: [u16; 8] = match text.split_once("::") {
        Some((left, right)) => {
            if right.contains("::") {
                return None;
            }
            let (left_count, right_count) = (groups(left, &mut head)?, groups(right, &mut tail)?);
            if left_count + right_count > 7 {
                return None;
            }
            let mut words = [0u16; 8];
            words[..left_count].copy_from_slice(&head[..left_count]);
            words[8 - right_count..].copy_from_slice(&tail[..right_count]);
            words
        }
        None => {
            if groups(text, &mut head)? != 8 {
                return None;
            }
            head
        }
    };
    let mut out = [0u8; 16];
    for (index, word) in words.iter().enumerate() {
        out[index * 2..index * 2 + 2].copy_from_slice(&word.to_be_bytes());
    }
    Some(out)
}

impl fmt::Display for IpAddr {
    /// IPv6 in the RFC 5952 canonical form: lower case, the longest run of
    /// two or more zero groups (the first, on a tie) written as `::`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpAddr::V4(a) => write!(f, "{}.{}.{}.{}", a[0], a[1], a[2], a[3]),
            IpAddr::V6(a) => {
                let words: [u16; 8] = core::array::from_fn(|i| u16::from_be_bytes([a[i * 2], a[i * 2 + 1]]));
                let (mut best, mut best_len, mut run, mut run_len) = (8, 0, 0, 0);
                for (index, &word) in words.iter().enumerate() {
                    if word == 0 {
                        if run_len == 0 {
                            run = index;
                        }
                        run_len += 1;
                        if run_len > best_len {
                            best = run;
                            best_len = run_len;
                        }
                    } else {
                        run_len = 0;
                    }
                }
                if best_len < 2 {
                    best = 8;
                }
                let mut index = 0;
                while index < 8 {
                    if index == best {
                        f.write_str("::")?;
                        index += best_len;
                        continue;
                    }
                    if index > 0 && index != best + best_len {
                        f.write_str(":")?;
                    }
                    write!(f, "{:x}", words[index])?;
                    index += 1;
                }
                Ok(())
            }
        }
    }
}

pub struct Mac(pub [u8; 6]);

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

/// The modified EUI-64 interface identifier (RFC 4291, appendix A).
pub fn interface_id(mac: [u8; 6]) -> [u8; 8] {
    [mac[0] ^ 0x02, mac[1], mac[2], 0xff, 0xfe, mac[3], mac[4], mac[5]]
}

pub fn with_prefix(prefix: [u8; 8], mac: [u8; 6]) -> [u8; 16] {
    let mut address = [0u8; 16];
    address[..8].copy_from_slice(&prefix);
    address[8..].copy_from_slice(&interface_id(mac));
    address
}

pub fn link_local(mac: [u8; 6]) -> [u8; 16] {
    with_prefix([0xfe, 0x80, 0, 0, 0, 0, 0, 0], mac)
}

/// ff02::1:ffXX:XXXX, the group a Neighbor Solicitation for `address` goes to.
pub fn solicited_node(address: [u8; 16]) -> [u8; 16] {
    let mut group = [0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0xff, 0, 0, 0];
    group[13..].copy_from_slice(&address[13..]);
    group
}

/// The Ethernet address an IPv6 multicast group maps to: 33:33 + low 32 bits.
pub fn multicast_mac(group: [u8; 16]) -> [u8; 6] {
    [0x33, 0x33, group[12], group[13], group[14], group[15]]
}

pub fn is_link_local(address: [u8; 16]) -> bool {
    address[0] == 0xfe && address[1] & 0xc0 == 0x80
}

/// Internet checksum (RFC 1071) over several pieces. Every piece but the
/// last must have an even length.
pub fn checksum(pieces: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    for piece in pieces {
        let mut chunks = piece.chunks_exact(2);
        for pair in &mut chunks {
            sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
        }
        if let [last] = chunks.remainder() {
            sum += (*last as u32) << 8;
        }
        sum = (sum & 0xffff) + (sum >> 16);
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v6(text: &str) -> [u8; 16] {
        parse_v6(text).unwrap_or_else(|| panic!("{text}"))
    }

    #[test]
    fn ipv4_text() {
        assert_eq!(parse("10.0.2.15"), Some(IpAddr::V4([10, 0, 2, 15])));
        assert_eq!(IpAddr::V4([255, 0, 7, 1]).to_string(), "255.0.7.1");
        for bad in ["10.0.2", "10.0.2.15.1", "256.0.0.1", "1..2.3", "+1.2.3.4", "1.2.3.4 ", "0001.2.3.4", ""] {
            assert_eq!(parse_v4(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn ipv6_text_round_trips_in_canonical_form() {
        for (text, canonical) in [
            ("::", "::"),
            ("::1", "::1"),
            ("fe80::5054:ff:fe12:3456", "fe80::5054:ff:fe12:3456"),
            ("2001:DB8:0:0:0:0:0:1", "2001:db8::1"),
            ("2001:db8:0:0:1:0:0:1", "2001:db8::1:0:0:1"),
            ("2001:db8:0:1:1:1:1:1", "2001:db8:0:1:1:1:1:1"),
            ("ff02::1:ff12:3456", "ff02::1:ff12:3456"),
            ("1::", "1::"),
            ("0:0:1:0:0:0:1:0", "0:0:1::1:0"),
        ] {
            let address = IpAddr::V6(v6(text));
            assert_eq!(address.to_string(), canonical, "{text}");
            assert_eq!(parse(canonical), Some(address));
        }
        for bad in [":::", "1:::2", "1::2::3", "1:2:3:4:5:6:7", "1:2:3:4:5:6:7:8:9", "12345::", "g::",
                    "1:2:3:4:5:6:7::8", ":1::", "fe80::1%eth0", "::1.2.3.4", ""] {
            assert_eq!(parse_v6(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn derived_ipv6_addresses() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        assert_eq!(link_local(mac), v6("fe80::5054:ff:fe12:3456"));
        assert_eq!(solicited_node(v6("2001:db8::5054:ff:fe12:3456")), v6("ff02::1:ff12:3456"));
        assert_eq!(multicast_mac(v6("ff02::1:ff12:3456")), [0x33, 0x33, 0xff, 0x12, 0x34, 0x56]);
        assert!(is_link_local(v6("fe80::1")) && is_link_local(v6("febf::1")) && !is_link_local(v6("fec0::1")));
        assert!(IpAddr::V6(v6("ff02::1")).is_multicast() && IpAddr::V4([224, 0, 0, 1]).is_multicast());
        assert_eq!(Mac(mac).to_string(), "52:54:00:12:34:56");
    }

    #[test]
    fn internet_checksum() {
        // RFC 1071 example bytes: sum 0xddf2, checksum 0x220d.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&[&data]), 0x220d);
        assert_eq!(checksum(&[&data[..4], &data[4..]]), 0x220d);
        // Odd length pads with a zero byte; a correct packet sums to zero.
        assert_eq!(checksum(&[&[0x01]]), !0x0100);
        let header = [0x45, 0, 0, 0x1c, 0, 0, 0, 0, 64, 1, 0, 0, 10, 0, 2, 15, 10, 0, 2, 2];
        let sum = checksum(&[&header]);
        let mut with_sum = header;
        with_sum[10..12].copy_from_slice(&sum.to_be_bytes());
        assert_eq!(checksum(&[&with_sum]), 0);
    }
}
