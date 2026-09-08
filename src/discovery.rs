//! LAN peer discovery over UDP.
//!
//! The previous builds required the user to type the primary's `host:port` into every secondary —
//! which is exactly why a fresh Windows box "couldn't see" the Mac: it had no address to connect
//! to. This module fixes that by letting the primary periodically broadcast a tiny beacon, and
//! secondaries listen and learn the primary's IP + port automatically.
//!
//! Transport: a single JSON `Beacon { port, name }` datagram, sent to the directed-broadcast
//! address `255.255.255.255` (and the link-local multicast group as a fallback) on `DISCOVERY_PORT`.
//! A secondary binds `DISCOVERY_PORT`, receives beacons, and auto-connects when it is not already
//! linked. Pure UDP, no new dependencies, works on macOS / Windows / Linux.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// UDP port the discovery beacons travel on. Deliberately distinct from the TCP hub port so the
/// two never collide and a machine can be both a listener and (elsewhere) a sender.
pub const DISCOVERY_PORT: u16 = 49153;

/// Broadcast cadence. Two seconds is responsive enough to "just appeared" without flooding the
/// subnet; the primary keeps sending forever so a secondary that joins late still finds it.
const BEACON_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Beacon {
    /// TCP hub port the primary is listening on.
    port: u16,
    /// Primary's machine name (shown in the "discovered" list).
    name: String,
}

/// A primary seen on the LAN.
#[derive(Debug, Clone)]
pub struct Discovered {
    pub ip: String,
    pub port: u16,
    pub name: String,
}

impl Discovered {
    /// `host:port` suitable for `connect_client`.
    pub fn addr(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }
}

/// Start the primary's beacon sender. Runs forever on its own thread; never returns an error to
/// the caller (logging instead) so a discovery failure can never take the app down.
pub fn start_beacon(port: u16, name: String) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                log::warn!("discovery beacon: bind failed: {}", e);
                return;
            }
        };
        if let Err(e) = sock.set_broadcast(true) {
            log::warn!("discovery beacon: set_broadcast failed: {}", e);
        }
        let payload = match serde_json::to_vec(&Beacon { port, name }) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("discovery beacon: serialize failed: {}", e);
                return;
            }
        };
        loop {
            // Directed broadcast reaches every host on the local subnet.
            let _ = sock.send_to(&payload, (std::net::Ipv4Addr::BROADCAST, DISCOVERY_PORT));
            // Link-local multicast as a fallback for networks that drop directed broadcasts.
            let _ = sock.send_to(&payload, (std::net::Ipv4Addr::new(239, 255, 255, 250), DISCOVERY_PORT));
            std::thread::sleep(BEACON_INTERVAL);
        }
    })
}

/// Start the secondary's listener. `on_discover` is invoked for every primary beacon received
/// (it may fire repeatedly for the same primary — dedupe in the callback if needed).
pub fn start_listener(
    on_discover: impl Fn(Discovered) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let sock = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("discovery listener: bind failed: {}", e);
                return;
            }
        };
        // Short timeout so the loop yields and the thread stays responsive to shutdown.
        let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
        let mut buf = [0u8; 1024];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if let Ok(beacon) = serde_json::from_slice::<Beacon>(&buf[..n]) {
                        let ip = match from.ip() {
                            IpAddr::V4(v4) => v4.to_string(),
                            IpAddr::V6(v6) => v6.to_string(),
                        };
                        on_discover(Discovered {
                            ip,
                            port: beacon.port,
                            name: beacon.name,
                        });
                    }
                }
                Err(_) => continue,
            }
        }
    })
}

/// Shared, GUI-readable list of primaries currently seen on the LAN. The listener appends to it;
/// the UI reads it to populate the "discovered devices" card.
pub type DiscoveredList = Arc<Mutex<Vec<Discovered>>>;

/// Build an empty discovered list (call once at startup, share with both the listener and the GUI).
pub fn new_list() -> DiscoveredList {
    Arc::new(Mutex::new(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_roundtrips() {
        let b = Beacon {
            port: 49152,
            name: "Mac".into(),
        };
        let j = serde_json::to_vec(&b).unwrap();
        let back: Beacon = serde_json::from_slice(&j).unwrap();
        assert_eq!(back.port, 49152);
        assert_eq!(back.name, "Mac");
    }

    #[test]
    fn discovered_addr_formats() {
        let d = Discovered {
            ip: "10.0.0.5".into(),
            port: 49152,
            name: "Mac".into(),
        };
        assert_eq!(d.addr(), "10.0.0.5:49152");
    }
}
