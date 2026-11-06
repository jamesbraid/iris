//! The guest's view of the host network: the NAT engine's controls, the
//! addresses it hands the guest and the count of frames it has processed.
//! The GUI's network indicator and the CI socket read these through
//! `Workstation`; the machines build one from `[network]` and attach the link
//! it returns to their Ethernet adapter.

use std::net::Ipv4Addr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crate::config::{MachineConfig, NetMode};
use crate::net::{GatewayConfig, Link, NatControl};

pub struct GuestNet {
    nat: Arc<NatControl>,
    /// Client, gateway, netmask: what the guest's interface should be
    /// configured as.
    addrs: (Ipv4Addr, Ipv4Addr, Ipv4Addr),
    /// None only where the source has none (the SEEQ's `frames()` is an Option).
    frames: Option<Arc<AtomicU64>>,
}

impl GuestNet {
    /// Open `[network]`. `None` for `mode = "off"`, an adapter left
    /// unplugged. Prints the "Ethernet on ..." line naming `who`; the caller
    /// attaches the returned link to its adapter.
    pub fn open(cfg: &MachineConfig, who: &str) -> Result<Option<(GuestNet, Link)>, String> {
        let net = GatewayConfig::from_network(&cfg.network());
        let Some(link) = Link::with_gateway(net.clone())? else { return Ok(None) };
        if net.mode == NetMode::Stream {
            eprintln!("iris: {who} Ethernet on {}", link.describe());
        } else {
            eprintln!("iris: {who} Ethernet on NAT: gateway {}, guest {} (DHCP)", net.gateway_ip, net.client_ip);
        }
        let addrs = (net.client_ip, net.gateway_ip, net.netmask);
        Ok(Some((GuestNet { nat: link.nat(), addrs, frames: Some(link.frames()) }, link)))
    }

    /// A GuestNet over a link someone else owns: the Indy-family's SEEQ keeps
    /// its own, and always yields one.
    pub fn from_parts(nat: Arc<NatControl>, addrs: (Ipv4Addr, Ipv4Addr, Ipv4Addr), frames: Option<Arc<AtomicU64>>) -> GuestNet {
        GuestNet { nat, addrs, frames }
    }

    pub fn nat(&self) -> Arc<NatControl> {
        self.nat.clone()
    }

    pub fn addrs(&self) -> (Ipv4Addr, Ipv4Addr, Ipv4Addr) {
        self.addrs
    }

    pub fn frames(&self) -> Option<Arc<AtomicU64>> {
        self.frames.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: NetMode) -> MachineConfig {
        let mut cfg = MachineConfig::default();
        cfg.network.mode = mode;
        cfg
    }

    #[test]
    fn open_returns_none_when_the_link_is_off() {
        assert!(GuestNet::open(&cfg(NetMode::Off), "Test").unwrap().is_none());
    }

    #[test]
    fn open_gives_the_gateways_addresses_in_client_gateway_netmask_order() {
        let c = cfg(NetMode::Nat);
        let want = GatewayConfig::from_network(&c.network());
        let (net, _link) = GuestNet::open(&c, "Test").unwrap().expect("a link");
        assert_eq!(net.addrs(), (want.client_ip, want.gateway_ip, want.netmask));
        assert!(net.frames().is_some());
    }

    #[test]
    fn from_parts_keeps_what_it_is_given() {
        let nat = NatControl::new();
        let frames = Arc::new(AtomicU64::new(7));
        let addrs = (Ipv4Addr::new(10, 0, 0, 2), Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(255, 255, 255, 0));
        let net = GuestNet::from_parts(nat.clone(), addrs, Some(frames.clone()));
        assert!(Arc::ptr_eq(&net.nat(), &nat));
        assert_eq!(net.addrs(), addrs);
        assert!(Arc::ptr_eq(&net.frames().unwrap(), &frames));
        assert!(GuestNet::from_parts(nat, addrs, None).frames().is_none());
    }
}
