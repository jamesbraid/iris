//! An adapter's connection to its network backend, made from `[network]`:
//! the two frame rings, their wake-ups and the backend at the far end. The
//! adapter takes one through `attach` and knows nothing of NAT or PCAP;
//! a new backend is one `NetBackend` implementation and a line in `select`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::config::{NetMode, NetworkConfig};

use super::{GatewayConfig, NatControl, NatEngine, NetBackend, Wake};

pub struct Link {
    /// Frames the guest sends.
    pub to_net: rtrb::Producer<Vec<u8>>,
    /// Frames for the guest.
    pub from_net: rtrb::Consumer<Vec<u8>>,
    pub tx_wake: Arc<Wake>,
    pub rx_wake: Arc<Wake>,
    /// Guest frames processed, whichever the backend: the GUI's indicator.
    pub frames: Arc<AtomicU64>,
    pub nat: Arc<NatControl>,
    /// Which backend, for the adapter's log line.
    pub describe: String,
    /// Clear it to stop the backend.
    pub running: Arc<AtomicBool>,
    pub backend: Option<Box<dyn NetBackend>>,
    config: GatewayConfig,
}

impl Link {
    /// `None` for `mode = "off"`: an adapter left unplugged.
    pub fn open(net: &NetworkConfig) -> Result<Option<Link>, String> {
        Self::with_gateway(GatewayConfig::from_network(net))
    }

    /// As `open`, for a gateway of the caller's own (a DaynaPort's subnet).
    pub fn with_gateway(config: GatewayConfig) -> Result<Option<Link>, String> {
        if config.mode == NetMode::Off {
            return Ok(None);
        }
        let frames = Arc::new(AtomicU64::new(0));
        Self::build(config, frames.clone(), NatControl::counting(frames), Wake::new(), Wake::new()).map(Some)
    }

    /// A link to the same backend with fresh rings and a backend that has not
    /// run, sharing the wake-ups and NAT controls: what a stopped adapter
    /// runs on its next start.
    pub fn renew(&self) -> Result<Link, String> {
        Self::build(self.config.clone(), self.frames.clone(), self.nat.clone(), self.tx_wake.clone(), self.rx_wake.clone())
    }

    fn build(config: GatewayConfig, frames: Arc<AtomicU64>, nat: Arc<NatControl>, tx_wake: Arc<Wake>, rx_wake: Arc<Wake>) -> Result<Link, String> {
        let (to_net, tx_cons) = rtrb::RingBuffer::new(256);
        let (rx_prod, from_net) = rtrb::RingBuffer::new(256);
        let running = Arc::new(AtomicBool::new(false));
        let (c, rw, tw, r, n) = (config.clone(), rx_wake.clone(), tx_wake.clone(), running.clone(), nat.clone());
        let (backend, describe) = select(c, tx_cons, rx_prod, rw, tw, r, n)?;
        Ok(Link { to_net, from_net, tx_wake, rx_wake, frames, nat, describe, running, backend: Some(backend), config })
    }

    /// Run the backend on a thread called `name` until `running` clears; the
    /// thread hands it back when it ends.
    pub fn spawn(&mut self, name: &str) -> Option<JoinHandle<Box<dyn NetBackend>>> {
        let mut backend = self.backend.take()?;
        self.running.store(true, Ordering::SeqCst);
        let run = move || {
            backend.run();
            backend
        };
        Some(std::thread::Builder::new().name(name.into()).spawn(run).expect("network backend thread"))
    }

    /// Rings to nowhere, and their far ends, for adapter tests.
    #[cfg(test)]
    pub fn loopback() -> (Link, rtrb::Consumer<Vec<u8>>, rtrb::Producer<Vec<u8>>) {
        let (to_net, tx_cons) = rtrb::RingBuffer::new(256);
        let (rx_prod, from_net) = rtrb::RingBuffer::new(256);
        let link = Link {
            to_net, from_net, tx_wake: Wake::new(), rx_wake: Wake::new(), frames: Arc::default(), nat: NatControl::new(),
            describe: String::new(), running: Arc::new(AtomicBool::new(false)), backend: None,
            config: GatewayConfig::default(),
        };
        (link, tx_cons, rx_prod)
    }
}

/// The backend `config.mode` names: the NAT gateway or a PCAP bridge, which
/// needs `--features pcap` (without it the NAT runs instead).
fn select(
    config: GatewayConfig,
    tx_cons: rtrb::Consumer<Vec<u8>>,
    rx_prod: rtrb::Producer<Vec<u8>>,
    rx_wake: Arc<Wake>,
    tx_wake: Arc<Wake>,
    running: Arc<AtomicBool>,
    ctl: Arc<NatControl>,
) -> Result<(Box<dyn NetBackend>, String), String> {
    #[cfg(feature = "pcap")]
    if config.mode == NetMode::Pcap {
        let b = Lazy::new(move || super::net_pcap::PcapEngine::new(config, tx_cons, rx_prod, rx_wake, tx_wake, running, ctl));
        return Ok((Box::new(b), "PCAP (bridged)".into()));
    }
    let nat = if cfg!(not(feature = "pcap")) && config.mode == NetMode::Pcap {
        "NAT (software gateway): mode = \"pcap\" needs a build with --features pcap"
    } else {
        "NAT (software gateway)"
    };
    let b = Lazy::new(move || NatEngine::new(config, tx_cons, rx_prod, rx_wake, tx_wake, running, ctl));
    Ok((Box::new(b), nat.into()))
}

/// A backend built when first opened or run, not when the link is made: the
/// NAT binds its host listeners then, and keeps its state across runs.
struct Lazy<B, F: FnOnce() -> B> {
    make: Option<F>,
    backend: Option<B>,
}

impl<B, F: FnOnce() -> B> Lazy<B, F> {
    fn new(make: F) -> Self {
        Lazy { make: Some(make), backend: None }
    }
}

impl<B: NetBackend, F: FnOnce() -> B + Send> NetBackend for Lazy<B, F> {
    fn open(&mut self) {
        if let Some(make) = self.make.take() {
            self.backend = Some(make());
        }
    }

    fn run(&mut self) {
        self.open();
        if let Some(b) = self.backend.as_mut() {
            b.run();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A renewed link has fresh rings and a backend that has not run, and
    /// shares the wake-ups and controls.
    #[test]
    fn a_renewed_link_is_fresh() {
        let first = Link::open(&NetworkConfig::default()).unwrap().unwrap();
        let next = first.renew().unwrap();
        assert!(Arc::ptr_eq(&first.nat, &next.nat) && Arc::ptr_eq(&first.tx_wake, &next.tx_wake));
        assert!(next.backend.is_some() && !Arc::ptr_eq(&first.running, &next.running));
    }
}
