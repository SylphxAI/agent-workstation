//! Takeover against a real headless sway + wayvnc. Runs only with
//! `AGENT_WORKSTATION_HEADFUL=1` and `sway` and `wayvnc` on `PATH`.

use std::path::Path;
use std::time::Duration;

use agent_workstation::compositor::Compositor;
use agent_workstation::gate::ControlGate;
use agent_workstation::takeover::{Takeover, TakeoverConfig};
use agent_workstation::vnc::Vnc;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

/// Reads exact byte counts across WebSocket message boundaries.
struct Rfb<S> {
    ws: S,
    buf: Vec<u8>,
}

impl<S> Rfb<S>
where
    S: futures::Stream<Item = tokio_tungstenite::tungstenite::Result<Message>>
        + futures::Sink<Message>
        + Unpin,
    <S as futures::Sink<Message>>::Error: std::fmt::Debug,
{
    async fn read(&mut self, n: usize) -> Vec<u8> {
        while self.buf.len() < n {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("RFB read timed out");
            match msg.expect("stream ended").expect("websocket error") {
                Message::Binary(b) => self.buf.extend_from_slice(&b),
                other => panic!("unexpected message {other:?}"),
            }
        }
        self.buf.drain(..n).collect()
    }

    async fn send(&mut self, bytes: &[u8]) {
        self.ws
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .unwrap();
    }
}

async fn status(addr: std::net::SocketAddr, path: &str) -> String {
    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    tcp.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    tcp.read_to_string(&mut out).await.unwrap();
    out.lines().next().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn takeover_streams_a_headless_sway_desktop() {
    if std::env::var("AGENT_WORKSTATION_HEADFUL").as_deref() != Ok("1") {
        eprintln!("skipped: set AGENT_WORKSTATION_HEADFUL=1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let compositor = Compositor::start(Path::new("sway"), dir.path(), 1280, 800)
        .await
        .expect("compositor");
    let wayvnc = Vnc::start(Path::new("wayvnc"), &compositor.client_env(), dir.path())
        .await
        .expect("wayvnc");
    let vnc = wayvnc.socket().to_path_buf();

    let gate = ControlGate::new();
    let takeover = Takeover::start(TakeoverConfig::default(), vnc, gate)
        .await
        .unwrap();
    let addr = takeover.local_addr();
    let url = takeover.url();
    let token = url
        .query_pairs()
        .find(|(k, _)| k == "token")
        .unwrap()
        .1
        .into_owned();
    assert!(token.len() >= 43, "generated token carries 32 random bytes");

    assert!(status(addr, "/state").await.contains(" 401 "));
    assert!(status(addr, "/ws?token=wrong").await.contains(" 403 "));
    assert!(
        status(addr, &format!("/?token={token}"))
            .await
            .contains(" 200 ")
    );
    assert!(
        tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .is_err()
    );

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?token={token}"))
        .await
        .unwrap();
    let mut rfb = Rfb {
        ws,
        buf: Vec::new(),
    };
    assert_eq!(rfb.read(12).await, b"RFB 003.008\n");
    rfb.send(b"RFB 003.008\n").await;
    let count = rfb.read(1).await[0] as usize;
    assert!(
        rfb.read(count).await.contains(&1),
        "VNC server offers security type None"
    );
    rfb.send(&[1]).await;
    assert_eq!(rfb.read(4).await, [0, 0, 0, 0], "SecurityResult OK");
    rfb.send(&[1]).await; // ClientInit, shared
    let init = rfb.read(24).await;
    let (width, height) = (
        u16::from_be_bytes([init[0], init[1]]),
        u16::from_be_bytes([init[2], init[3]]),
    );
    assert_eq!((width, height), (1280, 800));
    let name_len = u32::from_be_bytes([init[20], init[21], init[22], init[23]]) as usize;
    rfb.read(name_len).await;
    rfb.send(&[2, 0, 0, 1, 0, 0, 0, 0]).await; // SetEncodings: Raw
    let [w0, w1] = width.to_be_bytes();
    let [h0, h1] = height.to_be_bytes();
    rfb.send(&[3, 0, 0, 0, 0, 0, w0, w1, h0, h1]).await;
    // Keyboard input while the agent holds control is dropped, not fatal.
    rfb.send(&[4, 1, 0, 0, 0, 0, 0, 0x61]).await;
    let header = rfb.read(4).await;
    assert_eq!(header[0], 0, "FramebufferUpdate");
    assert!(
        u16::from_be_bytes([header[2], header[3]]) >= 1,
        "at least one rectangle"
    );
    takeover.shutdown().await;
    wayvnc.shutdown().await;
    compositor.shutdown().await;
}
