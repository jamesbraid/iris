//! ICMP: echo to the gateway answered locally, echo to anywhere else relayed
//! through a host ICMP socket per (destination, identifier).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use super::engine::NatEngine;
use super::packet::{self, Icmp, MacAddr};
use crate::devlog::LogModule;

const ICMP_IDLE: Duration = Duration::from_secs(30);

pub(super) struct NatIcmpEntry {
    pub(super) sock: Option<Socket>, // None if socket creation failed (e.g. not admin on Windows)
    client_mac: MacAddr,
    client_ip: Ipv4Addr,
    pub(super) last_use: Instant,
}

/// Linux: unprivileged SOCK_DGRAM+ICMPV4 works (kernel ≥3.11) but Time Exceeded
///   replies are not delivered, so traceroute sees * * * for intermediate hops.
/// macOS: unprivileged SOCK_DGRAM works for any user and, unlike Linux, recv
///   yields the full IP header and delivers Time Exceeded; SOCK_RAW (root) is
///   the fallback.
/// Windows: SOCK_RAW+ICMPV4 needs admin; Time Exceeded is delivered on recv.
fn icmp_socket() -> std::io::Result<Socket> {
    let new = |t| Socket::new(Domain::IPV4, t, Some(Protocol::ICMPV4));
    #[cfg(target_os = "linux")]
    { new(Type::DGRAM) }
    #[cfg(target_os = "macos")]
    { new(Type::DGRAM).or_else(|_| new(Type::RAW)) }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { new(Type::RAW) }
}

impl NatEngine {
    pub(super) fn handle_icmp(&mut self, src_mac: MacAddr, src_ip: Ipv4Addr, dst_ip: Ipv4Addr,
                              ttl: u8, icmp: &Icmp) {
        if icmp.ty() != packet::ICMP_ECHO_REQUEST {
            return;
        }
        let (ident, seq) = (icmp.ident(), icmp.seq());

        if dst_ip == self.config.gateway_ip {
            dlog_dev!(LogModule::Net, "NAT ICMP {} → {} ident={} seq={} (local reply)", src_ip, dst_ip, ident, seq);
            let id = self.next_ip_id();
            for f in self.route(src_mac, dst_ip, src_ip)
                .icmp_fragments(id, packet::ICMP_ECHO_REPLY, 0, icmp.rest(), icmp.body()) {
                self.send(f);
            }
            return;
        }

        let key = (u32::from(dst_ip), ident);
        dlog_dev!(LogModule::Net, "NAT ICMP {} → {} ident={} seq={} ttl={}{}", src_ip, dst_ip, ident, seq, ttl,
            if self.icmp_nat.contains_key(&key) { "" } else { " [new]" });
        if self.icmp_unavailable {
            return;
        }
        let entry = self.icmp_nat.entry(key).or_insert_with(|| {
            let sock = match icmp_socket() {
                Ok(s) => { let _ = s.set_nonblocking(true); Some(s) }
                Err(e) => {
                    #[cfg(windows)]
                    eprintln!("iris: ICMP unavailable ({}); ping will time out. \
                        Run as Administrator to enable raw ICMP.", e);
                    #[cfg(not(windows))]
                    eprintln!("iris: ICMP unavailable ({}); ping will time out.", e);
                    None
                }
            };
            NatIcmpEntry { sock, client_mac: src_mac, client_ip: src_ip, last_use: Instant::now() }
        });
        let Some(sock) = entry.sock.as_ref() else {
            self.icmp_unavailable = true;
            return;
        };
        entry.last_use = Instant::now();
        // Keep the guest's TTL so routers answer Time Exceeded at the right hop.
        let _ = sock.set_ttl_v4(ttl as u32);
        let _ = sock.send_to(icmp.bytes(), &SocketAddr::new(IpAddr::V4(dst_ip), 0).into());
    }

    pub(super) fn poll_icmp(&mut self) {
        self.icmp_nat.retain(|_, e| e.last_use.elapsed() <= ICMP_IDLE);
        // (icmp message, outer source, key)
        let mut replies: Vec<(Vec<u8>, Ipv4Addr, (u32, u16))> = Vec::new();
        for (&key, entry) in &mut self.icmp_nat {
            let Some(sock) = &mut entry.sock else { continue };
            // Room for the largest datagram: the host reassembles a big echo
            // reply before the socket sees it. Left uninitialised, so the
            // size costs nothing per poll.
            let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 65535];
            while let Ok(n) = sock.recv(&mut buf) {
                // SAFETY: recv initialised the first n bytes.
                let raw: Vec<u8> = buf[..n].iter().map(|b| unsafe { b.assume_init() }).collect();
                // Linux (SOCK_DGRAM) delivers only the ICMP message; Windows
                // (SOCK_RAW) and macOS prepend the outer IP header.
                #[cfg(not(target_os = "linux"))]
                let Some(ip) = packet::Ipv4::parse(&raw) else { continue };
                #[cfg(not(target_os = "linux"))]
                let (outer_src, icmp) = (ip.src(), ip.payload().to_vec());
                #[cfg(target_os = "linux")]
                let (outer_src, icmp) = (Ipv4Addr::from(key.0), raw);
                replies.push((icmp, outer_src, key));
            }
        }
        for (icmp, outer_src, key) in replies {
            self.reply_from_host(&icmp, outer_src, key);
        }
    }

    /// Hand the guest an ICMP message a host socket received for the flow
    /// `key`, from `outer_src`.
    fn reply_from_host(&mut self, icmp: &[u8], outer_src: Ipv4Addr, key: (u32, u16)) {
        let Some(msg) = Icmp::parse(icmp) else { return };
        // Time Exceeded (non-Linux only): find the flow from the probe
        // embedded in it, rewrite its source back to the guest.
        #[cfg(not(target_os = "linux"))]
        if msg.ty() == packet::ICMP_TIME_EXCEEDED {
            let Some(emb) = packet::Ipv4::parse(msg.body()) else { return };
            let Some(probe) = Icmp::parse(&msg.body()[emb.header_len()..]) else { return };
            let Some(entry) = self.icmp_nat.get(&(u32::from(emb.dst()), probe.ident())) else { return };
            let (client_mac, client_ip) = (entry.client_mac, entry.client_ip);
            let mut body = msg.body().to_vec();
            body[12..16].copy_from_slice(&client_ip.octets());
            dlog_dev!(LogModule::Net, "NAT ICMP TimeExceeded {} → {} ident={}", outer_src, client_ip, probe.ident());
            let id = self.next_ip_id();
            let frame = self.route(client_mac, outer_src, client_ip)
                .icmp(id, msg.ty(), msg.code(), msg.rest(), &body);
            self.send(frame);
            return;
        }
        // Echo reply: restore the guest's identifier (the host kernel may
        // have rewritten it).
        let Some(entry) = self.icmp_nat.get(&key) else { return };
        let (client_mac, client_ip) = (entry.client_mac, entry.client_ip);
        let mut rest = msg.rest();
        rest[..2].copy_from_slice(&key.1.to_be_bytes());
        dlog_dev!(LogModule::Net, "NAT ICMP reply {} → {} ident={} seq={}", outer_src, client_ip, key.1, msg.seq());
        let id = self.next_ip_id();
        for f in self.route(client_mac, outer_src, client_ip).icmp_fragments(id, msg.ty(), msg.code(), rest, msg.body()) {
            self.send(f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::packet::{Ether, Ipv4Route};
    use crate::net::testnet::{echo_message, fragment_by_hand, pattern, TestNet, GUEST_MAC};

    /// An echo reply from an outside host, as the host socket hands it over
    /// (reassembled, identifier rewritten by the host kernel), reaches the
    /// guest with the guest's identifier, fragmented to the MTU.
    fn host_reply_with(size: usize) {
        let mut net = TestNet::default();
        let (guest, gw_mac) = (net.guest_ip, net.gw_mac);
        let outside = Ipv4Addr::new(198, 51, 100, 7);
        let key = (u32::from(outside), 0xbeef);
        net.e.icmp_nat.insert(key, NatIcmpEntry { sock: None, client_mac: GUEST_MAC, client_ip: guest,
                                                  last_use: Instant::now() });
        let body = pattern(size);
        let from_host = echo_message(packet::ICMP_ECHO_REPLY, 0x1234, 7, &body);
        net.e.reply_from_host(&from_host, outside, key);
        net.pump();
        let got: Vec<Vec<u8>> = net.inbox.drain(..).collect();
        assert!(!got.is_empty(), "{size}-byte reply: nothing sent");
        let id = Ether::parse(&got[0]).unwrap().ipv4().unwrap().id();
        let reply = echo_message(packet::ICMP_ECHO_REPLY, 0xbeef, 7, &body);
        let want = fragment_by_hand(Ipv4Route::new(GUEST_MAC, gw_mac, outside, guest), packet::PROTO_ICMP, id, &reply);
        assert_eq!(got.len(), want.len(), "{size}-byte reply: frames");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert!(g == w, "{size}-byte reply: frame {i} differs:\n got {}\nwant {}",
                    packet::summary(g), packet::summary(w));
        }
    }

    #[test]
    fn big_echo_reply_from_outside_is_fragmented() {
        host_reply_with(1473);
        host_reply_with(8000);
    }

    #[test]
    fn echo_reply_from_outside_that_fits_is_one_frame() {
        host_reply_with(56);
        host_reply_with(1472);
    }
}
