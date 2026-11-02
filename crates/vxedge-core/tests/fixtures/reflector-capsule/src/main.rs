//! Echoes every datagram on :9800, the same reflector contract as the `linux-probe` bundle.

use std::net::UdpSocket;

fn main() {
    let node = std::env::var("VX_NODE").unwrap_or_default();
    let socket = UdpSocket::bind("0.0.0.0:9800").expect("bind :9800");
    println!("capsule {node} reflecting on :9800");
    let mut buf = [0u8; 2048];
    loop {
        let (n, peer) = socket.recv_from(&mut buf).expect("receive");
        // Best effort: a lost echo is measured as loss by the sender.
        let _ = socket.send_to(&buf[..n], peer);
    }
}
