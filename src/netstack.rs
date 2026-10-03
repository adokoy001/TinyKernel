//! Bounded, allocation-free Ethernet/ARP/ICMP and IPv6 neighbor discovery.
//! No DHCP, routing discovery, fragmentation, extension headers, TCP or UDP.
//! Packet input never grants authority: this engine only answers requests to
//! its own addresses and completes the one explicitly started ping.

use crate::inet::{self, IpAddr};

pub const MAX_FRAME: usize = 1514;
const PAYLOAD_LEN: usize = 16;
const NEIGHBOR_TTL: u64 = 3000;
const RETRY_TICKS: u64 = 20;
const MAX_PROBES: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub mac: [u8; 6],
    pub ipv4: [u8; 4],
    pub netmask4: [u8; 4],
    pub gateway4: [u8; 4],
    pub ipv6: [u8; 16],
    pub prefix6: u8,
    pub gateway6: [u8; 16],
}

impl Config {
    /// Requires QEMU's explicit `ipv6-net=fd00::/64` (its default is fec0::).
    pub const fn qemu(mac: [u8; 6]) -> Self {
        Self {
            mac, ipv4: [10, 0, 2, 15], netmask4: [255, 255, 255, 0], gateway4: [10, 0, 2, 2],
            ipv6: [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x15],
            prefix6: 64,
            gateway6: [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error { Truncated, Malformed, Checksum, Unsupported, BufferTooSmall, Busy, InvalidTarget, NoRoute }

impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Truncated => "truncated packet", Self::Malformed => "malformed packet",
            Self::Checksum => "invalid checksum", Self::Unsupported => "unsupported protocol",
            Self::BufferTooSmall => "packet buffer too small", Self::Busy => "ping already active",
            Self::InvalidTarget => "ping requires a remote unicast address", Self::NoRoute => "no route to target",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Neighbor { pub address: IpAddr, pub mac: [u8; 6], pub last_seen: u64 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub source: IpAddr, pub identifier: u16, pub sequence: u16,
    pub bytes: usize, pub ttl: u8, pub rtt_ticks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome { pub tx_len: usize, pub reply: Option<Reply> }
impl Outcome { const NONE: Self = Self { tx_len: 0, reply: None }; }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll { Idle, Send(usize), Timeout }

#[derive(Clone, Copy)]
struct Pending {
    target: IpAddr, next_hop: IpAddr, identifier: u16, sequence: u16,
    payload: [u8; PAYLOAD_LEN], started: u64, sent_at: Option<u64>,
    sent_mac: [u8; 6], last_probe: u64, probes: u8,
}

pub struct Stack {
    config: Config,
    neighbors: [Option<Neighbor>; 8],
    pending: Option<Pending>,
    generation: u64,
}

impl Stack {
    pub const fn new(config: Config) -> Self {
        Self { config, neighbors: [None; 8], pending: None, generation: 0 }
    }
    pub fn config(&self) -> &Config { &self.config }
    pub fn neighbors(&self) -> &[Option<Neighbor>; 8] { &self.neighbors }
    pub fn active(&self) -> bool { self.pending.is_some() }
    pub fn cancel_ping(&mut self) { self.pending = None; }

    pub fn start_ping(&mut self, target: IpAddr, identifier: u16, sequence: u16,
                      now: u64, out: &mut [u8]) -> Result<usize, Error> {
        if self.active() { return Err(Error::Busy); }
        let next_hop = self.next_hop(target)?;
        self.generation = self.generation.wrapping_add(1);
        let mut payload = [0; PAYLOAD_LEN];
        payload[..8].copy_from_slice(&self.generation.to_be_bytes());
        payload[8..].copy_from_slice(&now.to_be_bytes());
        let mut p = Pending { target, next_hop, identifier, sequence, payload, started: now,
            sent_at: None, sent_mac: [0; 6], last_probe: now, probes: 1 };
        let n = match self.lookup(next_hop, now) {
            Some(mac) => { p.sent_at = Some(now); p.sent_mac = mac; self.echo(p, mac, out)? }
            None => self.probe(p, out)?,
        };
        self.pending = Some(p);
        Ok(n)
    }

    /// PIT ticks are 100 Hz. Timeout covers resolution and echo together.
    pub fn poll(&mut self, now: u64, timeout_ticks: u64, out: &mut [u8]) -> Result<Poll, Error> {
        let Some(mut p) = self.pending else { return Ok(Poll::Idle); };
        if now.wrapping_sub(p.started) >= timeout_ticks {
            self.pending = None;
            return Ok(Poll::Timeout);
        }
        if p.sent_at.is_none() {
            if let Some(mac) = self.lookup(p.next_hop, now) {
                let n = self.echo(p, mac, out)?;
                p.sent_at = Some(now);
                p.sent_mac = mac;
                self.pending = Some(p);
                return Ok(Poll::Send(n));
            }
            if p.probes < MAX_PROBES && now.wrapping_sub(p.last_probe) >= RETRY_TICKS {
                let n = self.probe(p, out)?;
                p.last_probe = now;
                p.probes += 1;
                self.pending = Some(p);
                return Ok(Poll::Send(n));
            }
        }
        Ok(Poll::Idle)
    }

    pub fn ingest(&mut self, frame: &[u8], now: u64, out: &mut [u8]) -> Result<Outcome, Error> {
        if frame.len() < 14 { return Err(Error::Truncated); }
        if frame.len() > MAX_FRAME { return Err(Error::Malformed); }
        let dst = array::<6>(&frame[..6]);
        let src = array::<6>(&frame[6..12]);
        if !valid_mac(src) || src == self.config.mac { return Err(Error::Malformed); }
        match be16(&frame[12..14]) {
            0x0806 if dst == self.config.mac || dst == [255; 6] => self.arp(&frame[14..], src, now, out),
            0x0800 if dst == self.config.mac => self.ipv4(&frame[14..], src, now, out),
            0x86dd => {
                // Multicast Ethernet must match the accepted IPv6 destination.
                if frame.len() < 54 { return Err(Error::Truncated); }
                let ipdst = array::<16>(&frame[38..54]);
                if (ipdst[0] == 255 && dst != inet::multicast_mac(ipdst))
                    || (ipdst[0] != 255 && dst != self.config.mac) {
                    return Ok(Outcome::NONE);
                }
                self.ipv6(&frame[14..], src, now, out)
            }
            _ => Ok(Outcome::NONE),
        }
    }

    fn on_link4(&self, a: [u8; 4]) -> bool {
        (0..4).all(|i| a[i] & self.config.netmask4[i] == self.config.ipv4[i] & self.config.netmask4[i])
    }
    fn broadcast4(&self, a: [u8; 4]) -> bool {
        a == [255; 4] || (0..4).all(|i| a[i] == self.config.ipv4[i] | !self.config.netmask4[i])
    }
    fn network4(&self, a: [u8; 4]) -> bool {
        (0..4).all(|i| a[i] == self.config.ipv4[i] & self.config.netmask4[i])
    }
    fn on_link6(&self, a: [u8; 16]) -> bool {
        inet::is_link_local(a) || prefix_matches(a, self.config.ipv6, self.config.prefix6)
    }
    fn own6(&self, a: [u8; 16]) -> bool { a == self.config.ipv6 || a == inet::link_local(self.config.mac) }
    fn source6(&self, dst: [u8; 16]) -> [u8; 16] {
        if inet::is_link_local(dst) { inet::link_local(self.config.mac) } else { self.config.ipv6 }
    }
    fn next_hop(&self, target: IpAddr) -> Result<IpAddr, Error> {
        match target {
            IpAddr::V4(a) => {
                if !valid4(a) || self.broadcast4(a) || self.network4(a) || a == self.config.ipv4 { return Err(Error::InvalidTarget); }
                let hop = if self.on_link4(a) { a } else { self.config.gateway4 };
                if !valid4(hop) || !self.on_link4(hop) { return Err(Error::NoRoute); }
                Ok(IpAddr::V4(hop))
            }
            IpAddr::V6(a) => {
                if !valid6(a) || self.own6(a) { return Err(Error::InvalidTarget); }
                let hop = if self.on_link6(a) { a } else { self.config.gateway6 };
                if !valid6(hop) || !self.on_link6(hop) { return Err(Error::NoRoute); }
                Ok(IpAddr::V6(hop))
            }
        }
    }
    fn lookup(&self, address: IpAddr, now: u64) -> Option<[u8; 6]> {
        self.neighbors.iter().flatten().find(|n| n.address == address && now.wrapping_sub(n.last_seen) < NEIGHBOR_TTL).map(|n| n.mac)
    }
    fn learn(&mut self, address: IpAddr, mac: [u8; 6], now: u64) {
        let i = self.neighbors.iter().position(|n| n.is_none() || n.is_some_and(|n| n.address == address))
            .unwrap_or_else(|| self.neighbors.iter().enumerate().max_by_key(|(_, n)| now.wrapping_sub(n.unwrap().last_seen)).unwrap().0);
        self.neighbors[i] = Some(Neighbor { address, mac, last_seen: now });
    }
    fn pending_after_learning(&mut self, now: u64, out: &mut [u8]) -> Result<Outcome, Error> {
        let Some(mut p) = self.pending else { return Ok(Outcome::NONE); };
        if p.sent_at.is_some() { return Ok(Outcome::NONE); }
        let Some(mac) = self.lookup(p.next_hop, now) else { return Ok(Outcome::NONE); };
        let n = self.echo(p, mac, out)?;
        p.sent_at = Some(now);
        p.sent_mac = mac;
        self.pending = Some(p);
        Ok(Outcome { tx_len: n, reply: None })
    }

    fn arp(&mut self, p: &[u8], ethsrc: [u8; 6], now: u64, out: &mut [u8]) -> Result<Outcome, Error> {
        if p.len() < 28 { return Err(Error::Truncated); }
        if p[..6] != [0, 1, 8, 0, 6, 4] { return Err(Error::Unsupported); }
        let op = be16(&p[6..8]);
        let mac = array::<6>(&p[8..14]);
        let src = array::<4>(&p[14..18]);
        let dst = array::<4>(&p[24..28]);
        if mac != ethsrc || !valid_mac(mac) { return Err(Error::Malformed); }
        if dst != self.config.ipv4 { return Ok(Outcome::NONE); }
        if src != [0; 4] && (!valid4(src) || !self.on_link4(src) || self.broadcast4(src) || self.network4(src) || src == self.config.ipv4) {
            return Err(Error::Malformed);
        }
        match op {
            1 => {
                // ARP probes (sender 0.0.0.0) receive a reply but aren't cached.
                if src != [0; 4] { self.learn(IpAddr::V4(src), mac, now); }
                let n = arp_packet(self.config.mac, mac, 2, self.config.ipv4, src, mac, out)?;
                Ok(Outcome { tx_len: n, reply: None })
            }
            2 => {
                if p[18..24] != self.config.mac || src == [0; 4] { return Err(Error::Malformed); }
                if self.pending.is_some_and(|q| q.sent_at.is_none() && q.next_hop == IpAddr::V4(src)) {
                    self.learn(IpAddr::V4(src), mac, now);
                    self.pending_after_learning(now, out)
                } else { Ok(Outcome::NONE) }
            }
            _ => Err(Error::Unsupported),
        }
    }

    fn ipv4(&mut self, p: &[u8], ethsrc: [u8; 6], now: u64, out: &mut [u8]) -> Result<Outcome, Error> {
        if p.len() < 20 { return Err(Error::Truncated); }
        let header = usize::from(p[0] & 15) * 4;
        if p[0] >> 4 != 4 || header < 20 { return Err(Error::Malformed); }
        let total = usize::from(be16(&p[2..4]));
        if total < header + 8 || total > 1500 { return Err(Error::Malformed); }
        if total > p.len() { return Err(Error::Truncated); }
        if be16(&p[6..8]) & 0xbfff != 0 { return Err(Error::Unsupported); }
        if inet::checksum(&[&p[..header]]) != 0 { return Err(Error::Checksum); }
        if p[9] != 1 { return Err(Error::Unsupported); }
        let src = array::<4>(&p[12..16]);
        let dst = array::<4>(&p[16..20]);
        if dst != self.config.ipv4 { return Ok(Outcome::NONE); }
        if !valid4(src) || self.broadcast4(src) || self.network4(src) || src == self.config.ipv4 || p[8] == 0 { return Err(Error::Malformed); }
        let icmp = &p[header..total];
        if inet::checksum(&[icmp]) != 0 { return Err(Error::Checksum); }
        if icmp[1] != 0 { return Err(Error::Malformed); }
        match icmp[0] {
            8 => {
                let n = ipv4_packet(self.config.mac, ethsrc, self.config.ipv4, src, icmp.len(), out)?;
                out[34..34 + icmp.len()].copy_from_slice(icmp);
                out[34] = 0; out[36] = 0; out[37] = 0;
                let sum = inet::checksum(&[&out[34..n]]);
                put16(&mut out[36..38], sum);
                Ok(Outcome { tx_len: n, reply: None })
            }
            0 => Ok(Outcome { tx_len: 0, reply: self.match_reply(IpAddr::V4(src), ethsrc, icmp, p[8], now) }),
            _ => Err(Error::Unsupported),
        }
    }

    fn ipv6(&mut self, p: &[u8], ethsrc: [u8; 6], now: u64, out: &mut [u8]) -> Result<Outcome, Error> {
        if p.len() < 40 { return Err(Error::Truncated); }
        if p[0] >> 4 != 6 { return Err(Error::Malformed); }
        let len = usize::from(be16(&p[4..6]));
        if len < 8 || len > 1460 { return Err(Error::Malformed); }
        if p.len() < 40 + len { return Err(Error::Truncated); }
        if p[6] != 58 { return Err(Error::Unsupported); }
        let src = array::<16>(&p[8..24]);
        let dst = array::<16>(&p[24..40]);
        let icmp = &p[40..40 + len];
        if checksum6(src, dst, icmp) != 0 { return Err(Error::Checksum); }
        if icmp[1] != 0 || p[7] == 0 { return Err(Error::Malformed); }
        match icmp[0] {
            135 | 136 => self.nd(src, dst, ethsrc, p[7], icmp, now, out),
            128 | 129 => {
                if !self.own6(dst) { return Ok(Outcome::NONE); }
                if !valid6(src) || self.own6(src) { return Err(Error::Malformed); }
                if icmp[0] == 128 {
                    let n = ipv6_packet(self.config.mac, ethsrc, dst, src, 64, icmp.len(), out)?;
                    out[54..n].copy_from_slice(icmp);
                    out[54] = 129; out[56] = 0; out[57] = 0;
                    let sum = checksum6(dst, src, &out[54..n]);
                    put16(&mut out[56..58], sum);
                    Ok(Outcome { tx_len: n, reply: None })
                } else { Ok(Outcome { tx_len: 0, reply: self.match_reply(IpAddr::V6(src), ethsrc, icmp, p[7], now) }) }
            }
            _ => Err(Error::Unsupported),
        }
    }

    fn nd(&mut self, src: [u8; 16], dst: [u8; 16], ethsrc: [u8; 6], hop: u8,
          icmp: &[u8], now: u64, out: &mut [u8]) -> Result<Outcome, Error> {
        if icmp.len() < 24 { return Err(Error::Truncated); }
        let target = array::<16>(&icmp[8..24]);
        if hop != 255 || !valid6(target) { return Err(Error::Malformed); }
        let option = nd_option(&icmp[24..], if icmp[0] == 135 { 1 } else { 2 })?;
        if option.is_some_and(|m| m != ethsrc || !valid_mac(m)) { return Err(Error::Malformed); }
        if icmp[0] == 135 {
            if !self.own6(target) { return Ok(Outcome::NONE); }
            let group = inet::solicited_node(target);
            if dst != target && dst != group { return Err(Error::Malformed); }
            if src == [0; 16] {
                if dst != group || option.is_some() { return Err(Error::Malformed); }
                let all_nodes = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
                let n = neighbor_advertisement(self.config.mac, inet::multicast_mac(all_nodes), target, all_nodes, false, out)?;
                return Ok(Outcome { tx_len: n, reply: None });
            }
            if !valid6(src) || !self.on_link6(src) || self.own6(src) { return Err(Error::Malformed); }
            if let Some(mac) = option { self.learn(IpAddr::V6(src), mac, now); }
            let n = neighbor_advertisement(self.config.mac, ethsrc, target, src, true, out)?;
            Ok(Outcome { tx_len: n, reply: None })
        } else {
            if !valid6(src) || !self.on_link6(src) || self.own6(src) { return Err(Error::Malformed); }
            let solicited = icmp[4] & 0x40 != 0;
            let all_nodes = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
            if !self.own6(dst) && dst != all_nodes { return Ok(Outcome::NONE); }
            if dst[0] == 255 && solicited { return Err(Error::Malformed); }
            if solicited && self.pending.is_some_and(|q| q.sent_at.is_none() && q.next_hop == IpAddr::V6(target)) {
                let mac = option.ok_or(Error::Malformed)?;
                self.learn(IpAddr::V6(target), mac, now);
                self.pending_after_learning(now, out)
            } else { Ok(Outcome::NONE) }
        }
    }

    fn match_reply(&mut self, source: IpAddr, mac: [u8; 6], icmp: &[u8], ttl: u8, now: u64) -> Option<Reply> {
        let p = self.pending?;
        let sent = p.sent_at?;
        if p.target != source || p.sent_mac != mac || icmp.len() != 8 + PAYLOAD_LEN
            || be16(&icmp[4..6]) != p.identifier || be16(&icmp[6..8]) != p.sequence || icmp[8..] != p.payload { return None; }
        self.pending = None;
        Some(Reply { source, identifier: p.identifier, sequence: p.sequence, bytes: PAYLOAD_LEN, ttl, rtt_ticks: now.wrapping_sub(sent) })
    }

    fn probe(&self, p: Pending, out: &mut [u8]) -> Result<usize, Error> {
        match p.next_hop {
            IpAddr::V4(a) => arp_packet(self.config.mac, [255; 6], 1, self.config.ipv4, a, [0; 6], out),
            IpAddr::V6(a) => {
                let src = self.source6(a);
                let dst = inet::solicited_node(a);
                let n = ipv6_packet(self.config.mac, inet::multicast_mac(dst), src, dst, 255, 32, out)?;
                out[54] = 135;
                out[62..78].copy_from_slice(&a);
                out[78] = 1; out[79] = 1;
                out[80..86].copy_from_slice(&self.config.mac);
                let sum = checksum6(src, dst, &out[54..n]);
                put16(&mut out[56..58], sum);
                Ok(n)
            }
        }
    }
    fn echo(&self, p: Pending, dstmac: [u8; 6], out: &mut [u8]) -> Result<usize, Error> {
        let (offset, n) = match p.target {
            IpAddr::V4(a) => (34, ipv4_packet(self.config.mac, dstmac, self.config.ipv4, a, 8 + PAYLOAD_LEN, out)?),
            IpAddr::V6(a) => (54, ipv6_packet(self.config.mac, dstmac, self.source6(a), a, 64, 8 + PAYLOAD_LEN, out)?),
        };
        out[offset] = match p.target { IpAddr::V4(_) => 8, IpAddr::V6(_) => 128 };
        put16(&mut out[offset + 4..offset + 6], p.identifier);
        put16(&mut out[offset + 6..offset + 8], p.sequence);
        out[offset + 8..n].copy_from_slice(&p.payload);
        let sum = match p.target {
            IpAddr::V4(_) => inet::checksum(&[&out[offset..n]]),
            IpAddr::V6(a) => checksum6(self.source6(a), a, &out[offset..n]),
        };
        put16(&mut out[offset + 2..offset + 4], sum);
        Ok(n)
    }
}

fn valid_mac(a: [u8; 6]) -> bool { a != [0; 6] && a[0] & 1 == 0 }
fn valid4(a: [u8; 4]) -> bool { a[0] != 0 && a[0] != 127 && a[0] < 224 && a != [255; 4] }
fn valid6(a: [u8; 16]) -> bool { a != [0; 16] && a[0] != 255 && a != [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1] }
fn prefix_matches(a: [u8; 16], b: [u8; 16], bits: u8) -> bool {
    if bits > 128 { return false; }
    let whole = usize::from(bits / 8);
    let rest = bits % 8;
    a[..whole] == b[..whole] && (rest == 0 || a[whole] >> (8 - rest) == b[whole] >> (8 - rest))
}
fn array<const N: usize>(p: &[u8]) -> [u8; N] { let mut a = [0; N]; a.copy_from_slice(p); a }
fn be16(p: &[u8]) -> u16 { u16::from_be_bytes([p[0], p[1]]) }
fn put16(p: &mut [u8], n: u16) { p.copy_from_slice(&n.to_be_bytes()); }

fn ethernet(src: [u8; 6], dst: [u8; 6], protocol: u16, n: usize, out: &mut [u8]) -> Result<(), Error> {
    if n > MAX_FRAME || out.len() < n { return Err(Error::BufferTooSmall); }
    out[..n].fill(0);
    out[..6].copy_from_slice(&dst); out[6..12].copy_from_slice(&src);
    put16(&mut out[12..14], protocol);
    Ok(())
}
fn arp_packet(srcmac: [u8; 6], dstmac: [u8; 6], op: u16, src: [u8; 4], dst: [u8; 4], targetmac: [u8; 6], out: &mut [u8]) -> Result<usize, Error> {
    ethernet(srcmac, dstmac, 0x0806, 42, out)?;
    out[14..20].copy_from_slice(&[0, 1, 8, 0, 6, 4]);
    put16(&mut out[20..22], op);
    out[22..28].copy_from_slice(&srcmac); out[28..32].copy_from_slice(&src);
    out[32..38].copy_from_slice(&targetmac); out[38..42].copy_from_slice(&dst);
    Ok(42)
}
fn ipv4_packet(srcmac: [u8; 6], dstmac: [u8; 6], src: [u8; 4], dst: [u8; 4], payload: usize, out: &mut [u8]) -> Result<usize, Error> {
    let n = 34 + payload;
    ethernet(srcmac, dstmac, 0x0800, n, out)?;
    out[14] = 0x45; put16(&mut out[16..18], (20 + payload) as u16);
    out[20] = 0x40; out[22] = 64; out[23] = 1;
    out[26..30].copy_from_slice(&src); out[30..34].copy_from_slice(&dst);
    let sum = inet::checksum(&[&out[14..34]]);
    put16(&mut out[24..26], sum);
    Ok(n)
}
fn ipv6_packet(srcmac: [u8; 6], dstmac: [u8; 6], src: [u8; 16], dst: [u8; 16], hop: u8, payload: usize, out: &mut [u8]) -> Result<usize, Error> {
    let n = 54 + payload;
    ethernet(srcmac, dstmac, 0x86dd, n, out)?;
    out[14] = 0x60; put16(&mut out[18..20], payload as u16);
    out[20] = 58; out[21] = hop;
    out[22..38].copy_from_slice(&src); out[38..54].copy_from_slice(&dst);
    Ok(n)
}
fn checksum6(src: [u8; 16], dst: [u8; 16], icmp: &[u8]) -> u16 {
    inet::checksum(&[&src, &dst, &(icmp.len() as u32).to_be_bytes(), &[0, 0, 0, 58], icmp])
}
fn nd_option(mut p: &[u8], kind: u8) -> Result<Option<[u8; 6]>, Error> {
    let mut mac = None;
    while !p.is_empty() {
        if p.len() < 2 { return Err(Error::Truncated); }
        let n = usize::from(p[1]) * 8;
        if n == 0 { return Err(Error::Malformed); }
        if n > p.len() { return Err(Error::Truncated); }
        if p[0] == kind {
            if n != 8 || mac.is_some() { return Err(Error::Malformed); }
            mac = Some(array::<6>(&p[2..8]));
        }
        p = &p[n..];
    }
    Ok(mac)
}
fn neighbor_advertisement(srcmac: [u8; 6], dstmac: [u8; 6], target: [u8; 16], dst: [u8; 16], solicited: bool, out: &mut [u8]) -> Result<usize, Error> {
    let n = ipv6_packet(srcmac, dstmac, target, dst, 255, 32, out)?;
    out[54] = 136; out[58] = if solicited { 0x60 } else { 0x20 };
    out[62..78].copy_from_slice(&target);
    out[78] = 2; out[79] = 1; out[80..86].copy_from_slice(&srcmac);
    let sum = checksum6(target, dst, &out[54..n]);
    put16(&mut out[56..58], sum);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    const LOCAL: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
    const PEER: [u8; 6] = [0x52, 0x55, 10, 0, 2, 2];
    fn stack() -> Stack { Stack::new(Config::qemu(LOCAL)) }
    fn refresh4(frame: &mut [u8]) {
        frame[24..26].fill(0);
        let c = inet::checksum(&[&frame[14..34]]); put16(&mut frame[24..26], c);
        frame[36..38].fill(0);
        let c = inet::checksum(&[&frame[34..]]); put16(&mut frame[36..38], c);
    }
    fn refresh6(frame: &mut [u8]) {
        frame[56..58].fill(0);
        let c = checksum6(array::<16>(&frame[22..38]), array::<16>(&frame[38..54]), &frame[54..]);
        put16(&mut frame[56..58], c);
    }
    fn resolve4(s: &mut Stack, now: u64, out: &mut [u8]) -> usize {
        let mut reply = [0; MAX_FRAME];
        let n = arp_packet(PEER, LOCAL, 2, [10, 0, 2, 2], [10, 0, 2, 15], LOCAL, &mut reply).unwrap();
        s.ingest(&reply[..n], now, out).unwrap().tx_len
    }
    fn echo_reply(request: &[u8], v6: bool) -> Vec<u8> {
        let mut p = request.to_vec();
        p[..6].copy_from_slice(&LOCAL); p[6..12].copy_from_slice(&PEER);
        let (start, end, size, icmp) = if v6 { (22, 38, 16, 54) } else { (26, 30, 4, 34) };
        for i in 0..size { p.swap(start + i, end + i); }
        p[icmp] = if v6 { 129 } else { 0 };
        if v6 { refresh6(&mut p); } else { refresh4(&mut p); }
        p
    }

    #[test]
    fn ipv4_resolution_reply_matching_and_cache_reuse() {
        let mut s = stack(); let mut out = [0; MAX_FRAME];
        assert_eq!(s.start_ping(IpAddr::V4([10, 0, 2, 2]), 17, 9, 100, &mut out), Ok(42));
        assert_eq!(out[..6], [255; 6]); assert_eq!(be16(&out[20..22]), 1);
        let n = resolve4(&mut s, 104, &mut out); assert_eq!(n, 58);
        let request = out[..n].to_vec();
        let mut reply = echo_reply(&request, false);
        reply[41] ^= 1; refresh4(&mut reply);
        assert_eq!(s.ingest(&reply, 107, &mut out).unwrap().reply, None);
        reply = echo_reply(&request, false); reply[49] ^= 1; refresh4(&mut reply);
        assert_eq!(s.ingest(&reply, 107, &mut out).unwrap().reply, None);
        reply = echo_reply(&request, false);
        let r = s.ingest(&reply, 109, &mut out).unwrap().reply.unwrap();
        assert_eq!((r.rtt_ticks, r.identifier, r.sequence, r.bytes), (5, 17, 9, 16));
        assert!(!s.active());
        assert_eq!(s.start_ping(IpAddr::V4([1, 1, 1, 1]), 18, 10, 110, &mut out), Ok(58));
        assert_eq!(out[..6], PEER); assert_eq!(out[30..34], [1, 1, 1, 1]);
    }

    #[test]
    fn ipv6_resolution_and_echo() {
        let mut s = stack(); let mut out = [0; MAX_FRAME];
        let gateway = s.config.gateway6;
        let n = s.start_ping(IpAddr::V6(gateway), 7, 2, 50, &mut out).unwrap();
        assert_eq!(n, 86); assert_eq!(out[54], 135); assert_eq!(out[21], 255);
        assert_eq!(checksum6(s.config.ipv6, inet::solicited_node(gateway), &out[54..n]), 0);
        let mut na = [0; MAX_FRAME];
        let n = neighbor_advertisement(PEER, LOCAL, gateway, s.config.ipv6, true, &mut na).unwrap();
        let n = s.ingest(&na[..n], 52, &mut out).unwrap().tx_len;
        assert_eq!(n, 78); assert_eq!(out[54], 128);
        let reply = echo_reply(&out[..n], true);
        let r = s.ingest(&reply, 55, &mut out).unwrap().reply.unwrap();
        assert_eq!((r.source, r.rtt_ticks), (IpAddr::V6(gateway), 3));
    }

    #[test]
    fn only_solicited_resolution_learns_and_retries_are_bounded() {
        let mut s = stack(); let mut out = [0; MAX_FRAME]; let mut reply = [0; MAX_FRAME];
        let n = arp_packet(PEER, LOCAL, 2, [10, 0, 2, 2], [10, 0, 2, 15], LOCAL, &mut reply).unwrap();
        assert_eq!(s.ingest(&reply[..n], 1, &mut out).unwrap(), Outcome::NONE);
        assert!(s.neighbors.iter().all(Option::is_none));
        s.start_ping(IpAddr::V4([10, 0, 2, 2]), 1, 1, 10, &mut out).unwrap();
        assert_eq!(s.poll(29, 100, &mut out), Ok(Poll::Idle));
        assert_eq!(s.poll(30, 100, &mut out), Ok(Poll::Send(42)));
        assert_eq!(s.poll(50, 100, &mut out), Ok(Poll::Send(42)));
        assert_eq!(s.poll(90, 100, &mut out), Ok(Poll::Idle));
        assert_eq!(s.poll(110, 100, &mut out), Ok(Poll::Timeout));
        assert!(!s.active());
    }

    #[test]
    fn requests_to_own_addresses_are_answered() {
        let mut s = stack(); let mut frame = [0; MAX_FRAME]; let mut out = [0; MAX_FRAME];
        let n = arp_packet(PEER, [255; 6], 1, [10, 0, 2, 2], s.config.ipv4, [0; 6], &mut frame).unwrap();
        assert_eq!(s.ingest(&frame[..n], 1, &mut out).unwrap().tx_len, 42);
        assert_eq!(out[..6], PEER); assert_eq!(be16(&out[20..22]), 2);
        let n = ipv4_packet(PEER, LOCAL, [10, 0, 2, 2], s.config.ipv4, 11, &mut frame).unwrap();
        frame[34] = 8; frame[38..n].copy_from_slice(&[0, 1, 0, 2, 97, 98, 99]); refresh4(&mut frame[..n]);
        assert_eq!(s.ingest(&frame[..n], 2, &mut out).unwrap().tx_len, n);
        assert_eq!(&out[34..36], &[0, 0]); assert_eq!(inet::checksum(&[&out[34..n]]), 0);
        assert_eq!(&out[38..n], &frame[38..n]);
    }

    #[test]
    fn malformed_checksum_fragments_and_output_bounds() {
        let mut s = stack(); let mut frame = [0; MAX_FRAME]; let mut out = [0; MAX_FRAME];
        let n = ipv4_packet(PEER, LOCAL, [10, 0, 2, 2], s.config.ipv4, 8, &mut frame).unwrap();
        frame[34] = 8; refresh4(&mut frame[..n]);
        frame[41] ^= 1;
        assert_eq!(s.ingest(&frame[..n], 1, &mut out), Err(Error::Checksum));
        frame[41] ^= 1; frame[20] = 0x20; refresh4(&mut frame[..n]);
        assert_eq!(s.ingest(&frame[..n], 1, &mut out), Err(Error::Unsupported));
        frame[20] = 0x40; refresh4(&mut frame[..n]);
        assert_eq!(s.ingest(&frame[..n - 1], 1, &mut out), Err(Error::Truncated));
        assert_eq!(s.ingest(&frame[..n], 1, &mut out[..n - 1]), Err(Error::BufferTooSmall));
        assert_eq!(s.start_ping(IpAddr::V4([10, 0, 2, 2]), 1, 1, 0, &mut out[..10]), Err(Error::BufferTooSmall));
        assert!(!s.active());
        for a in [[0; 4], [127, 0, 0, 1], [224, 0, 0, 1], [10, 0, 2, 255], [10, 0, 2, 0], s.config.ipv4] {
            assert_eq!(s.start_ping(IpAddr::V4(a), 1, 1, 0, &mut out), Err(Error::InvalidTarget));
        }
    }

    #[test]
    fn neighbor_discovery_validation_and_dad() {
        let mut s = stack(); let mut out = [0; MAX_FRAME]; let mut frame = [0; MAX_FRAME];
        let gateway = s.config.gateway6;
        s.start_ping(IpAddr::V6(gateway), 1, 1, 1, &mut out).unwrap();
        let n = neighbor_advertisement(PEER, LOCAL, gateway, s.config.ipv6, true, &mut frame).unwrap();
        frame[21] = 254;
        assert_eq!(s.ingest(&frame[..n], 2, &mut out), Err(Error::Malformed));
        frame[21] = 255; frame[79] = 0; refresh6(&mut frame[..n]);
        assert_eq!(s.ingest(&frame[..n], 2, &mut out), Err(Error::Malformed));
        frame[79] = 2; refresh6(&mut frame[..n]);
        assert_eq!(s.ingest(&frame[..n], 2, &mut out), Err(Error::Truncated));
        frame[79] = 1; frame[80] ^= 2; refresh6(&mut frame[..n]);
        assert_eq!(s.ingest(&frame[..n], 2, &mut out), Err(Error::Malformed));
        assert!(s.neighbors.iter().all(Option::is_none));
        let dst = inet::solicited_node(s.config.ipv6);
        let n = ipv6_packet(PEER, inet::multicast_mac(dst), [0; 16], dst, 255, 24, &mut frame).unwrap();
        frame[54] = 135; frame[62..78].copy_from_slice(&s.config.ipv6); refresh6(&mut frame[..n]);
        assert_eq!(s.ingest(&frame[..n], 3, &mut out).unwrap().tx_len, 86);
        assert_eq!(out[58] & 0x40, 0); assert_eq!(out[53], 1); // all-nodes, unsolicited
    }

    #[test]
    fn busy_cancel_expiry_and_generation_prevent_stale_reply() {
        let mut s = stack(); let mut out = [0; MAX_FRAME]; let target = IpAddr::V4([10, 0, 2, 2]);
        s.start_ping(target, 1, 1, 1, &mut out).unwrap();
        assert_eq!(s.start_ping(target, 1, 1, 2, &mut out), Err(Error::Busy));
        let n = resolve4(&mut s, 2, &mut out); let old = echo_reply(&out[..n], false);
        s.cancel_ping(); s.start_ping(target, 1, 1, 3, &mut out).unwrap();
        assert!(s.ingest(&old, 4, &mut out).unwrap().reply.is_none());
        s.cancel_ping(); assert_eq!(s.start_ping(target, 1, 1, 3002, &mut out), Ok(42));
    }

    #[test]
    fn link_local_ns_and_inbound_ipv6_echo() {
        let mut s = stack(); let mut frame = [0; MAX_FRAME]; let mut out = [0; MAX_FRAME];
        let local = inet::link_local(LOCAL); let peer = inet::link_local(PEER);
        let group = inet::solicited_node(local);
        let n = ipv6_packet(PEER, inet::multicast_mac(group), peer, group, 255, 32, &mut frame).unwrap();
        frame[54] = 135; frame[62..78].copy_from_slice(&local);
        frame[78..86].copy_from_slice(&[1, 1, PEER[0], PEER[1], PEER[2], PEER[3], PEER[4], PEER[5]]);
        refresh6(&mut frame[..n]);
        assert_eq!(s.ingest(&frame[..n], 1, &mut out).unwrap().tx_len, 86);
        assert_eq!(out[54], 136); assert_eq!(out[58], 0x60);
        assert_eq!(&out[22..38], &local); assert_eq!(&out[38..54], &peer);
        assert_eq!(checksum6(local, peer, &out[54..86]), 0);
        let n = ipv6_packet(PEER, LOCAL, peer, local, 64, 11, &mut frame).unwrap();
        frame[54] = 128; frame[58..n].copy_from_slice(&[0, 1, 0, 2, 97, 98, 99]); refresh6(&mut frame[..n]);
        assert_eq!(s.ingest(&frame[..n], 2, &mut out).unwrap().tx_len, n);
        assert_eq!(out[54], 129); assert_eq!(&out[58..n], &frame[58..n]);
        assert_eq!(checksum6(local, peer, &out[54..n]), 0);
        let wrong = [0x33, 0x33, 0, 0, 0, 1];
        frame[..6].copy_from_slice(&wrong);
        assert_eq!(s.ingest(&frame[..n], 3, &mut out).unwrap(), Outcome::NONE);
    }

    #[test]
    fn neighbor_capacity_and_prefix_boundaries() {
        let mut s = stack(); let mut frame = [0; MAX_FRAME]; let mut out = [0; MAX_FRAME];
        for suffix in 30..=41 {
            let n = arp_packet(PEER, [255; 6], 1, [10, 0, 2, suffix], s.config.ipv4, [0; 6], &mut frame).unwrap();
            s.ingest(&frame[..n], suffix as u64, &mut out).unwrap();
        }
        assert_eq!(s.neighbors.iter().flatten().count(), 8);
        assert!(s.lookup(IpAddr::V4([10, 0, 2, 30]), 42).is_none());
        assert_eq!(s.lookup(IpAddr::V4([10, 0, 2, 41]), 42), Some(PEER));
        assert!(prefix_matches([0; 16], [1; 16], 0));
        assert!(prefix_matches([0x80; 16], [0xff; 16], 1));
        assert!(!prefix_matches([0; 16], [0x80; 16], 1));
        assert!(prefix_matches([7; 16], [7; 16], 128));
        assert!(!prefix_matches([7; 16], [7; 16], 129));
    }

    #[test]
    fn in_flight_reply_uses_the_transmitted_neighbor_even_after_cache_expiry() {
        let mut s = stack(); let mut out = [0; MAX_FRAME]; let target = IpAddr::V4([10, 0, 2, 2]);
        s.start_ping(target, 1, 1, 1, &mut out).unwrap();
        resolve4(&mut s, 2, &mut out); s.cancel_ping();
        let n = s.start_ping(target, 2, 2, 3000, &mut out).unwrap();
        assert_eq!(n, 58);
        let reply = echo_reply(&out[..n], false);
        assert!(s.lookup(target, 3003).is_none());
        assert_eq!(s.ingest(&reply, 3003, &mut out).unwrap().reply.unwrap().rtt_ticks, 3);
    }

    #[test]
    fn hostile_inputs_never_panic_or_overrun() {
        let mut s = stack(); let mut out = [0; MAX_FRAME]; let mut bytes = [0; MAX_FRAME];
        let mut state = 0x1234_5678u32;
        for len in 0..=MAX_FRAME {
            for b in &mut bytes[..len] { state ^= state << 13; state ^= state >> 17; state ^= state << 5; *b = state as u8; }
            // Exercise each parser as well as the random Ethernet envelope.
            if len >= 14 { bytes[..6].copy_from_slice(&LOCAL); bytes[6..12].copy_from_slice(&PEER);
                put16(&mut bytes[12..14], [0x0800, 0x0806, 0x86dd][len % 3]); }
            let _ = s.ingest(&bytes[..len], len as u64, &mut out);
        }
    }
}
