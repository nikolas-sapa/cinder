#![allow(dead_code)]

use cinder_core::frame::FrameCodec;
use cinder_core::proto::{ClientMsg, ErrorCode, PeerId, ServerMsg};
use cinder_relay::server::{serve, RelayConfig};
use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::{FramedRead, FramedWrite};

pub type ClientReader = FramedRead<OwnedReadHalf, FrameCodec<ServerMsg>>;
pub type ClientWriter = FramedWrite<OwnedWriteHalf, FrameCodec<ClientMsg>>;

pub async fn spawn_relay(cfg: RelayConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(listener, cfg).await;
    });
    addr
}

pub async fn connect(addr: SocketAddr) -> (ClientReader, ClientWriter) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (rh, wh) = stream.into_split();
    (
        FramedRead::new(rh, FrameCodec::<ServerMsg>::transport()),
        FramedWrite::new(wh, FrameCodec::<ClientMsg>::transport()),
    )
}

pub struct TestClient {
    pub rd: ClientReader,
    pub wr: ClientWriter,
    pub peer_id: PeerId,
}

impl TestClient {
    pub async fn hello(addr: SocketAddr, peer_id: PeerId) -> Self {
        let (mut rd, mut wr) = connect(addr).await;
        wr.send(ClientMsg::Hello {
            version: 1,
            peer_id,
        })
        .await
        .unwrap();
        match rd.next().await {
            Some(Ok(ServerMsg::ServerHello { version, .. })) => assert_eq!(version, 1),
            other => panic!("expected ServerHello, got {other:?}"),
        }
        TestClient { rd, wr, peer_id }
    }

    pub async fn send(&mut self, msg: ClientMsg) {
        self.wr.send(msg).await.unwrap();
    }

    pub async fn recv(&mut self) -> ServerMsg {
        match tokio::time::timeout(Duration::from_secs(2), self.rd.next()).await {
            Ok(Some(Ok(msg))) => msg,
            other => panic!("expected ServerMsg, got {other:?}"),
        }
    }

    /// Next ServerMsg if it is Error, else panic. Returns (code, detail).
    pub async fn recv_error(&mut self) -> (ErrorCode, String) {
        match self.recv().await {
            ServerMsg::Error { code, detail } => (code, detail),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Read until the stream ends (EOF / decode error); collected messages.
    pub async fn drain(&mut self, within: Duration) -> Vec<ServerMsg> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.rd.next()).await {
                Ok(Some(Ok(msg))) => out.push(msg),
                Ok(Some(Err(_))) | Ok(None) => return out,
                Err(_) => panic!("stream still open after {within:?}; got {out:?}"),
            }
        }
    }
}
