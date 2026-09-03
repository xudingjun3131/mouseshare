//! LAN transport. Length-prefixed JSON frames over TCP.
//!
//! Topology: the **primary** runs a TCP listener and accepts secondaries. The primary keeps a
//! map of `peer name -> writer`. Clipboard messages are relayed to every other peer; input
//! messages are routed to a specific target peer.

use crate::protocol::{InputEvent, Message};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

/// `(peer name, message)` delivered from any connection's reader thread to the app.
pub type Incoming = Sender<(String, Message)>;

fn write_msg(stream: &mut TcpStream, msg: &Message) -> std::io::Result<()> {
    let buf = serde_json::to_vec(msg).expect("serialize Message");
    let len = buf.len() as u32;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(&buf)?;
    stream.flush()
}

fn read_msg(stream: &mut TcpStream) -> std::io::Result<Message> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Shared network handle, used from both the capture thread and the clipboard thread.
pub enum Net {
    Primary {
        peers: Arc<Mutex<HashMap<String, Sender<Message>>>>,
    },
    Secondary {
        tx: Sender<Message>,
    },
}

impl Net {
    pub fn send_input(&self, target: &str, ev: InputEvent) {
        match self {
            Net::Primary { peers } => {
                if let Some(tx) = peers.lock().unwrap().get(target) {
                    let _ = tx.send(Message::Input(ev));
                }
            }
            Net::Secondary { .. } => { /* secondaries never originate input */ }
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
        }
    }

    pub fn peer_count(&self) -> usize {
        match self {
            Net::Primary { peers } => peers.lock().unwrap().len(),
            Net::Secondary { .. } => 1,
        }
    }
}

/// Start the primary hub. Spawns a listener thread that accepts secondaries.
pub fn start_hub(port: u16, incoming: Incoming) -> anyhow::Result<Arc<Mutex<Net>>> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    log::info!("primary hub listening on :{}", port);
    let peers: Arc<Mutex<HashMap<String, Sender<Message>>>> = Arc::new(Mutex::new(HashMap::new()));
    let net = Arc::new(Mutex::new(Net::Primary { peers: peers.clone() }));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => handle_primary_conn(s, peers.clone(), incoming.clone()),
                Err(e) => log::warn!("accept error: {}", e),
            }
        }
    });
    Ok(net)
}

fn handle_primary_conn(stream: TcpStream, peers: Arc<Mutex<HashMap<String, Sender<Message>>>>, incoming: Incoming) {
    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("clone error: {}", e);
            return;
        }
    };
    // The first frame must be Hello so we learn the peer's name.
    let mut rs = read_stream;
    let hello = read_msg(&mut rs).ok();
    let name = match hello {
        Some(Message::Hello { name, .. }) => name,
        _ => {
            log::warn!("peer did not send Hello; dropping");
            return;
        }
    };
    log::info!("secondary connected: {}", name);

    let (tx, rx) = channel::<Message>();
    peers.lock().unwrap().insert(name.clone(), tx.clone());

    // Writer thread: drains the per-peer channel into the socket.
    std::thread::spawn(move || {
        let mut ws = stream;
        while let Ok(msg) = rx.recv() {
            if write_msg(&mut ws, &msg).is_err() {
                break;
            }
        }
    });

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

/// Connect as a secondary. Spawns reader/writer threads and returns the shared handle.
pub fn connect_client(addr: &str, incoming: Incoming) -> anyhow::Result<(Arc<Mutex<Net>>, Sender<Message>)> {
    let stream = TcpStream::connect(addr)?;
    log::info!("connected to primary at {}", addr);
    let read_stream = stream.try_clone()?;
    let (tx, rx) = channel::<Message>();

    std::thread::spawn(move || {
        let mut ws = stream;
        while let Ok(msg) = rx.recv() {
            if write_msg(&mut ws, &msg).is_err() {
                break;
            }
        }
    });

    std::thread::spawn(move || {
        let mut rs = read_stream;
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
    });

    let net = Arc::new(Mutex::new(Net::Secondary { tx: tx.clone() }));
    Ok((net, tx))
}
