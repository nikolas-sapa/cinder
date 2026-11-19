use std::time::Duration;

use crate::error::Error;
use crate::frame::wire_serialize;
use crate::identity::Identity;
use crate::proto::{caps, HandshakeMsg, HsId, InviteToken, PeerId};
use crate::session::Session;
use crate::transport::{ClientEvent, Link};

/// Initiator XX driver (§6/§7): Msg1 — inside `InviteAccept` when a rendezvous
/// token exists (folded, saves an RTT) else a plain `Handshake` frame — then
/// wait Msg2 → PIN → Msg3 → Trusted. Pinning happens after Msg2 and before
/// Msg3; a mismatch kills the session and Msg3 is never sent.
pub async fn initiator(
    identity: &Identity,
    link: &mut impl Link,
    to: PeerId,
    expected: &PeerId,
    invite: Option<InviteToken>,
) -> Result<Session, Error> {
    let mut session = Session::initiator(identity)?;
    let hs_id = HsId::random();
    let msg1 = session.produce_handshake()?;
    match invite {
        Some(token) => {
            // Wire field is the serialized HandshakeMsg::Msg1 (§7); the relay
            // re-wraps it as a Handshake frame to the slot owner.
            link.send_invite_accept(token, hs_id, wire_serialize(&msg1)?)
                .await?;
        }
        None => link.send_handshake(to, hs_id, msg1).await?,
    }
    let msg2 = wait_handshake(link, to, Some(hs_id), caps::HANDSHAKE_TIMEOUT).await?;
    session.accept_handshake(msg2)?;
    session.verify_peer_id(expected)?;
    let msg3 = session.produce_handshake()?;
    link.send_handshake(to, hs_id, msg3).await?;
    session.finish_handshake()?;
    Ok(session)
}

/// Responder XX driver (§6): wait Msg1 → Msg2 → wait Msg3 → PIN → Trusted.
/// `expected` receives the peer-id derived from Msg3's static and returns the
/// pin for a known contact; `None` means new contact → TOFU pin (still via
/// `verify_peer_id` internally). `from` on frames is claimed routing metadata
/// only — it is never used for the trust decision.
pub async fn responder(
    identity: &Identity,
    link: &mut impl Link,
    expected: impl FnOnce(&PeerId) -> Option<PeerId>,
) -> Result<Session, Error> {
    let mut session = Session::responder(identity)?;
    let (from, hs_id, msg1) = wait_first_handshake(link, caps::HANDSHAKE_TIMEOUT).await?;
    session.accept_handshake(msg1)?;
    let msg2 = session.produce_handshake()?;
    link.send_handshake(from, hs_id, msg2).await?;
    let msg3 = wait_handshake(link, from, Some(hs_id), caps::HANDSHAKE_TIMEOUT).await?;
    session.accept_handshake(msg3)?;
    let learned = session
        .remote_peer_id()
        .ok_or_else(|| Error::Protocol("handshake complete without remote static".into()))?;
    match expected(&learned) {
        Some(pin) => session.verify_peer_id(&pin)?,
        None => {
            session.accept_learned_peer()?;
        }
    }
    session.finish_handshake()?;
    Ok(session)
}

/// Wait for a `Handshake` frame FROM `from`, optionally filtered by hs_id.
/// Mismatched hs_ids are dropped per §6 — and so are frames from any other
/// peer: hs_ids collide across concurrent handshakes, and only matching
/// both fields proves the frame continues THIS exchange. A relay `Error`
/// frame aborts the wait.
async fn wait_handshake(
    link: &mut impl Link,
    from: PeerId,
    want: Option<HsId>,
    within: Duration,
) -> Result<HandshakeMsg, Error> {
    let fut = async {
        loop {
            match link.recv().await? {
                ClientEvent::Handshake {
                    from: got,
                    hs_id,
                    body,
                } if got == from && want.is_none_or(|w| w == hs_id) => {
                    return Ok(body);
                }
                ClientEvent::Error { code, detail } => {
                    return Err(Error::Transport(format!("relay error {code:?}: {detail}")));
                }
                _ => {}
            }
        }
    };
    match tokio::time::timeout(within, fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::HandshakeTimeout),
    }
}

async fn wait_first_handshake(
    link: &mut impl Link,
    within: Duration,
) -> Result<(PeerId, HsId, HandshakeMsg), Error> {
    let fut = async {
        loop {
            match link.recv().await? {
                ClientEvent::Handshake { from, hs_id, body } => return Ok((from, hs_id, body)),
                ClientEvent::Error { code, detail } => {
                    return Err(Error::Transport(format!("relay error {code:?}: {detail}")));
                }
                _ => {}
            }
        }
    };
    match tokio::time::timeout(within, fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::HandshakeTimeout),
    }
}
