use std::net::SocketAddr;

use futures::{SinkExt, StreamExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::error::Error;
use crate::frame::FrameCodec;
use crate::proto::{
    caps, ClientMsg, ErrorCode, HandshakeMsg, HsId, InviteToken, MailboxId, PeerId, ServerMsg,
    SessionId, StoredEnvelope, PROTOCOL_VERSION,
};

/// Client-side view of `ServerMsg`. `from` fields are the relay-stamped
/// CLAIMED peer-ids — routing metadata only, never a trust input (§6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    ServerHello { version: u16, now_unix: u64 },
    InviteCreated { token: InviteToken, expires_at: u64 },
    Handshake {
        from: PeerId,
        hs_id: HsId,
        body: HandshakeMsg,
    },
    Transport {
        from: PeerId,
        session_id: SessionId,
        body: Vec<u8>,
    },
    MailboxOpened {
        mailbox_id: MailboxId,
        expires_at: u64,
    },
    MailboxDeposited { msg_id: u64 },
    MailboxMessages {
        msgs: Vec<StoredEnvelope>,
        more: bool,
    },
    Ok,
    PeerStatus { peer: PeerId, online: bool },
    Pong { nonce: u64 },
    Error { code: ErrorCode, detail: String },
}

impl From<ServerMsg> for ClientEvent {
    fn from(m: ServerMsg) -> Self {
        match m {
            ServerMsg::ServerHello { version, now_unix } => {
                ClientEvent::ServerHello { version, now_unix }
            }
            ServerMsg::InviteCreated { token, expires_at } => {
                ClientEvent::InviteCreated { token, expires_at }
            }
            ServerMsg::Handshake { from, hs_id, body } => {
                ClientEvent::Handshake { from, hs_id, body }
            }
            ServerMsg::Transport {
                from,
                session_id,
                body,
            } => ClientEvent::Transport {
                from,
                session_id,
                body,
            },
            ServerMsg::MailboxOpened {
                mailbox_id,
                expires_at,
            } => ClientEvent::MailboxOpened {
                mailbox_id,
                expires_at,
            },
            ServerMsg::MailboxDeposited { msg_id } => ClientEvent::MailboxDeposited { msg_id },
            ServerMsg::MailboxMessages { msgs, more } => {
                ClientEvent::MailboxMessages { msgs, more }
            }
            ServerMsg::Ok => ClientEvent::Ok,
            ServerMsg::PeerStatus { peer, online } => ClientEvent::PeerStatus { peer, online },
            ServerMsg::Pong { nonce } => ClientEvent::Pong { nonce },
            ServerMsg::Error { code, detail } => ClientEvent::Error { code, detail },
        }
    }
}

/// The session/flow layer's view of a relay connection. `RelayTransport`
/// implements it over TCP; tests implement it over in-memory channels.
#[allow(async_fn_in_trait)]
pub trait Link {
    async fn send_handshake(
        &mut self,
        to: PeerId,
        hs_id: HsId,
        body: HandshakeMsg,
    ) -> Result<(), Error>;
    async fn send_invite_accept(
        &mut self,
        token: InviteToken,
        hs_id: HsId,
        msg1: Vec<u8>,
    ) -> Result<(), Error>;
    async fn send_transport(
        &mut self,
        to: PeerId,
        session_id: SessionId,
        body: Vec<u8>,
    ) -> Result<(), Error>;
    async fn recv(&mut self) -> Result<ClientEvent, Error>;
}

/// Relay client: TCP → FrameCodec → Hello → typed sends + `recv()`.
/// `connection_id` is regenerated per connect (CSPRNG) — a Noise
/// `TransportState` never lives here and is never serialized (§11).
pub struct RelayTransport {
    writer: FramedWrite<OwnedWriteHalf, FrameCodec<ClientMsg>>,
    reader: FramedRead<OwnedReadHalf, FrameCodec<ServerMsg>>,
    peer_id: PeerId,
    connection_id: SessionId,
}

impl RelayTransport {
    pub async fn connect(addr: SocketAddr, peer_id: PeerId) -> Result<Self, Error> {
        let stream = TcpStream::connect(addr).await?;
        let (rh, wh) = stream.into_split();
        let mut writer = FramedWrite::new(wh, FrameCodec::<ClientMsg>::pre_hello());
        let mut reader = FramedRead::new(rh, FrameCodec::<ServerMsg>::pre_hello());
        writer
            .send(ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                peer_id,
            })
            .await?;
        match reader.next().await {
            Some(Ok(ServerMsg::ServerHello { version, .. })) => {
                if version != PROTOCOL_VERSION {
                    return Err(Error::Protocol(format!(
                        "relay protocol version {version}, expected {PROTOCOL_VERSION}"
                    )));
                }
            }
            Some(Ok(ServerMsg::Error { code, detail })) => {
                return Err(Error::Transport(format!(
                    "relay rejected hello: {code:?}: {detail}"
                )));
            }
            Some(Ok(_)) => {
                return Err(Error::Protocol("expected ServerHello".into()));
            }
            Some(Err(e)) => return Err(e.into()),
            None => {
                return Err(Error::Transport(
                    "connection closed during hello".into(),
                ));
            }
        }
        reader
            .decoder_mut()
            .set_max_frame(caps::MAX_FRAME_TRANSPORT as u32);
        writer
            .encoder_mut()
            .set_max_frame(caps::MAX_FRAME_TRANSPORT as u32);
        Ok(RelayTransport {
            writer,
            reader,
            peer_id,
            connection_id: SessionId::random(),
        })
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    pub fn connection_id(&self) -> SessionId {
        self.connection_id
    }

    pub async fn send(&mut self, msg: ClientMsg) -> Result<(), Error> {
        self.writer.send(msg).await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<ClientEvent, Error> {
        match self.reader.next().await {
            Some(Ok(m)) => Ok(m.into()),
            Some(Err(e)) => Err(e.into()),
            None => Err(Error::Transport("relay connection closed".into())),
        }
    }
}

impl Link for RelayTransport {
    async fn send_handshake(
        &mut self,
        to: PeerId,
        hs_id: HsId,
        body: HandshakeMsg,
    ) -> Result<(), Error> {
        self.send(ClientMsg::Handshake { to, hs_id, body }).await
    }

    async fn send_invite_accept(
        &mut self,
        token: InviteToken,
        hs_id: HsId,
        msg1: Vec<u8>,
    ) -> Result<(), Error> {
        self.send(ClientMsg::InviteAccept { token, hs_id, msg1 }).await
    }

    async fn send_transport(
        &mut self,
        to: PeerId,
        session_id: SessionId,
        body: Vec<u8>,
    ) -> Result<(), Error> {
        self.send(ClientMsg::Transport {
            to,
            session_id,
            body,
        })
        .await
    }

    async fn recv(&mut self) -> Result<ClientEvent, Error> {
        RelayTransport::recv(self).await
    }
}
