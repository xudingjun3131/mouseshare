//! LAN transport. Length-prefixed JSON frames over TCP.
//!
//! Topology: the **primary** runs a TCP listener and accepts secondaries. The primary keeps a
//! map of `peer name -> writer`. Clipboard messages are relayed to every other peer; input
//! messages are routed to a specific target peer.

use crate::layout::Layout;
use crate::protocol::{InputEvent, Message};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `(peer name, message)` delivered from any connection's reader thread to the app.
pub type Incoming = Sender<(String, Message)>;

/// How many queued messages the writer will coalesce into one `write` before flushing.
/// Mouse motion arrives at 120+ Hz on a trackpad; merging a burst into a single syscall is
/// what keeps the remote cursor smooth instead of stuttering on per-event write overhead.
const WRITE_BATCH: usize = 64;

/// Compose one length-prefixed frame into `out` (reused across calls to avoid re-allocating).
fn encode_into(out: &mut Vec<u8>, msg: &Message) {
    let start = out.len();
    out.extend_from_slice(&[0u8; 4]);
    serde_json::to_writer(&mut *out, msg).expect("serialize Message");
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Read one length-prefixed frame.
fn read_msg(stream: &mut impl Read) -> std::io::Result<Message> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {} bytes", len),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Upper bound on a single frame. File chunks are 256 KB (≈350 KB base64), so this leaves
/// plenty of headroom while still rejecting a corrupted length prefix that would otherwise
/// make us allocate gigabytes.
const MAX_FRAME: usize = 8 << 20;

/// Drain `rx` and write every frame out immediately.
///
/// Frames are **never coalesced**: a burst of 20 mouse moves goes out as 20 `MouseMotion` frames,
/// not one summed frame. Collapsing them (which this used to do) keeps the total displacement
/// identical but cuts the number of cursor updates the receiving machine performs, and that is
/// exactly what a user perceives as a stuttery remote cursor — the delta arrives in fewer, larger
/// jumps instead of tracking the hand smoothly.
///
/// What *is* batched is only the syscall: everything already queued is encoded into one buffer and
/// handed to a single `write`, so a busy trackpad does not cost one syscall per event. The wire
/// still carries each frame separately.
fn pump_writes(mut ws: TcpStream, rx: Receiver<Message>) {
    let mut pending: Vec<Message> = Vec::with_capacity(WRITE_BATCH);
    let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
    while let Ok(first) = rx.recv() {
        pending.clear();
        pending.push(first);
        // Take only what is *already* queued — never wait for more, or a single move would sit
        // in the buffer until the next one arrived.
        while pending.len() < WRITE_BATCH {
            match rx.try_recv() {
                Ok(m) => pending.push(m),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out.clear();
        for m in &pending {
            encode_into(&mut out, m);
        }
        if ws.write_all(&out).is_err() {
            break;
        }
    }
}

/// Disable Nagle on a freshly opened stream.
///
/// Every forwarded mouse move is its own ~40 byte frame. With Nagle enabled the kernel holds
/// a small frame until the previous one is ACKed (up to ~40 ms of extra latency on a busy
/// LAN), which is exactly what makes a remote cursor feel "steppy" instead of smooth.
fn tune(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
}

/// Shared network handle, used from both the capture thread and the clipboard thread.
pub enum Net {
    Primary {
        peers: Arc<Mutex<HashMap<String, Sender<Message>>>>,
    },
    Secondary {
        tx: Sender<Message>,
    },
    /// No live connection — networking failed at startup (port already in use, or the primary
    /// was unreachable). This variant exists so the app can still open its GUI and explain the
    /// problem, instead of exiting silently (which, when launched from Finder, looks exactly
    /// like "I clicked it and nothing happened").
    Idle,
}

impl Net {
    /// A do-nothing handle used when startup failed, so the GUI can still open.
    pub fn idle() -> Arc<Mutex<Net>> {
        Arc::new(Mutex::new(Net::Idle))
    }

    pub fn send_input(&self, target: &str, ev: InputEvent) {
        match self {
            Net::Primary { peers } => {
                if let Some(tx) = peers.lock().unwrap().get(target) {
                    let _ = tx.send(Message::Input(ev));
                }
            }
            Net::Secondary { .. } | Net::Idle => { /* never originate input */ }
        }
    }

    /// Broadcast a clipboard update. `except` avoids echoing back to the sender.
    pub fn broadcast_clipboard(&self, text: &str, except: Option<&str>) {
        let msg = Message::Clipboard {
            text: text.to_string(),
        };
        match self {
            Net::Primary { peers } => {
                for (name, tx) in peers.lock().unwrap().iter() {
                    if Some(name.as_str()) == except {
                        continue;
                    }
                    let _ = tx.send(msg.clone());
                }
            }
            Net::Secondary { tx } => {
                let _ = tx.send(msg);
            }
            Net::Idle => { /* not connected: nothing to broadcast */ }
        }
    }

    /// Send to every peer (primary) or to the primary (secondary). Used for whole-fabric
    /// messages such as clipboard and file transfers that are not addressed to one machine.
    pub fn broadcast_all(&self, msg: Message) {
        match self {
            Net::Primary { peers } => {
                for (_, tx) in peers.lock().unwrap().iter() {
                    let _ = tx.send(msg.clone());
                }
            }
            Net::Secondary { tx } => {
                let _ = tx.send(msg);
            }
            Net::Idle => { /* not connected */ }
        }
    }

    /// [`broadcast_all`] but skipping one peer — used by the hub to relay a copy that arrived
    /// from `except` to every *other* machine (the sender already has it).
    pub fn broadcast_all_except(&self, msg: Message, except: &str) {
        match self {
            Net::Primary { peers } => {
                for (name, tx) in peers.lock().unwrap().iter() {
                    if name == except {
                        continue;
                    }
                    let _ = tx.send(msg.clone());
                }
            }
            Net::Secondary { .. } | Net::Idle => { /* nothing to relay */ }
        }
    }

    /// Send an arbitrary message to the primary (used by secondaries, e.g. the hotkey). The
    /// primary never calls this (it originates input itself); a `Primary`/`Idle` handle ignores it.
    pub fn send_message(&self, msg: Message) {
        if let Net::Secondary { tx } = self {
            let _ = tx.send(msg);
        }
    }

    /// Send a targeted (non-input) message to a specific peer. Used by the control plane to tell a
    /// secondary that the cursor is entering (`EnterScreen`) or leaving (`LeaveScreen`) its screen.
    /// No-op unless this is a `Primary` hub (only the primary routes to named peers).
    pub fn send_to(&self, target: &str, msg: Message) {
        if let Net::Primary { peers } = self {
            if let Some(tx) = peers.lock().unwrap().get(target) {
                let _ = tx.send(msg);
            }
        }
    }

    pub fn peer_count(&self) -> usize {        match self {
            Net::Primary { peers } => peers.lock().unwrap().len(),
            Net::Secondary { .. } => 1,
            Net::Idle => 0,
        }
    }

    /// Whether the primary currently has a live connection to `name`. Used by the control plane to
    /// detect a secondary that dropped mid-hand-off, so it can return control instead of forwarding
    /// input into a dead socket forever.
    pub fn has_peer(&self, name: &str) -> bool {
        match self {
            Net::Primary { peers } => peers.lock().unwrap().contains_key(name),
            Net::Secondary { .. } | Net::Idle => false,
        }
    }

    /// Push the full layout to every connected secondary. Cheap (one small JSON frame per
    /// peer) and idempotent — the primary calls this periodically so new peers and any screen
    /// repositioning show up on every machine's canvas.
    pub fn broadcast_layout(&self, layout: &Layout) {
        if let Net::Primary { peers } = self {
            let msg = Message::Layout {
                layout: layout.clone(),
            };
            for (_, tx) in peers.lock().unwrap().iter() {
                let _ = tx.send(msg.clone());
            }
        }
    }
}

/// Start the primary hub. Spawns a listener thread that accepts secondaries.
pub fn start_hub(
    port: u16,
    incoming: Incoming,
    layout: Arc<Mutex<Layout>>,
) -> anyhow::Result<Arc<Mutex<Net>>> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    log::info!("primary hub listening on :{}", port);
    let peers: Arc<Mutex<HashMap<String, Sender<Message>>>> = Arc::new(Mutex::new(HashMap::new()));
    let net = Arc::new(Mutex::new(Net::Primary { peers: peers.clone() }));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => handle_primary_conn(s, peers.clone(), incoming.clone(), layout.clone()),
                Err(e) => log::warn!("accept error: {}", e),
            }
        }
    });
    Ok(net)
}

fn handle_primary_conn(
    stream: TcpStream,
    peers: Arc<Mutex<HashMap<String, Sender<Message>>>>,
    incoming: Incoming,
    layout: Arc<Mutex<Layout>>,
) {
    tune(&stream);
    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("clone error: {}", e);
            return;
        }
    };
    // The first frame must be Hello so we learn the peer's name.
    let mut rs = BufReader::new(read_stream);
    let hello = read_msg(&mut rs).ok();
    let (name, width, height, scale) = match hello {
        Some(Message::Hello {
            name,
            width,
            height,
            scale,
        }) => (name, width, height, scale),
        _ => {
            log::warn!("peer did not send Hello; dropping");
            return;
        }
    };
    log::info!("secondary connected: {} (scale {})", name, scale);

    // Register the peer's screen (idempotent) so the layout we push already includes it.
    layout
        .lock()
        .unwrap()
        .ensure_screen(&name, width, height, false, scale);

    let (tx, rx) = channel::<Message>();
    peers.lock().unwrap().insert(name.clone(), tx.clone());

    // Send the current layout to the new peer immediately, so the secondary's canvas shows the
    // primary's screen (and every other machine) the instant it connects.
    let snapshot = layout.lock().unwrap().clone();
    let _ = tx.send(Message::Layout { layout: snapshot });

    // Writer thread: drains the per-peer channel into the socket (batched + coalesced).
    std::thread::spawn(move || pump_writes(stream, rx));

    // Reader thread: forwards everything the peer sends to the app.
    let peers2 = peers.clone();
    std::thread::spawn(move || {
        loop {
            match read_msg(&mut rs) {
                Ok(msg) => {
                    if incoming.send((name.clone(), msg)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        peers2.lock().unwrap().remove(&name);
        log::info!("secondary disconnected: {}", name);
    });
}

/// Resolve `host:port` (hostname or IP) and try each candidate with a short timeout.
///
/// A plain `TcpStream::connect` to an unreachable LAN address can block for a minute or more —
/// during which no window is shown at all, which reads as "the app does nothing".
fn connect_with_timeout(addr: &str, timeout: Duration) -> std::io::Result<TcpStream> {
    let addrs: Vec<std::net::SocketAddr> = addr.to_socket_addrs()?.collect();
    if addrs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("could not resolve address {}", addr),
        ));
    }
    let mut last: Option<std::io::Error> = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            format!("could not connect to {}", addr),
        )
    }))
}

/// Connect as a secondary. Spawns reader/writer threads and returns the shared handle.
///
/// `net` is the same `Arc<Mutex<Net>>` the GUI holds; when the link drops the reader thread
/// flips it back to `Net::Idle` so the status panel can show "disconnected" and the user can
/// hit "Connect" again without restarting the app.
pub fn connect_client(
    addr: &str,
    incoming: Incoming,
    net: Arc<Mutex<Net>>,
) -> anyhow::Result<(Arc<Mutex<Net>>, Sender<Message>)> {
    let stream = connect_with_timeout(addr, Duration::from_secs(3))?;
    tune(&stream);
    log::info!("connected to primary at {}", addr);
    let read_stream = stream.try_clone()?;
    let (tx, rx) = channel::<Message>();

    std::thread::spawn(move || pump_writes(stream, rx));

    let net_for_reader = net.clone();
    std::thread::spawn(move || {
        let mut rs = BufReader::new(read_stream);
        let server = "server".to_string();
        loop {
            match read_msg(&mut rs) {
                Ok(msg) => {
                    if incoming.send((server.clone(), msg)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        log::info!("lost connection to primary");
        *net_for_reader.lock().unwrap() = Net::Idle;
    });

    // Mark the *same* handle the caller passed in as Secondary, and return that exact Arc so the
    // reader thread's Idle-on-disconnect above flips the handle the GUI is actually reading.
    *net.lock().unwrap() = Net::Secondary { tx: tx.clone() };
    Ok((net.clone(), tx))
}
