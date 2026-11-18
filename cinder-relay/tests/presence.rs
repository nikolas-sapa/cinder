mod common;

use cinder_core::proto::{ClientMsg, PeerId, ServerMsg};
use cinder_relay::server::RelayConfig;
use common::*;

#[tokio::test]
async fn presence_subscribe_snapshot_and_events() {
    let addr = spawn_relay(RelayConfig::default()).await;
    let peer_b = PeerId::random();
    let peer_d = PeerId::random();

    let _b = TestClient::hello(addr, peer_b).await;
    let mut c = TestClient::hello(addr, PeerId::random()).await;

    c.send(ClientMsg::PresenceSubscribe {
        peers: vec![peer_b, peer_d],
    })
    .await;
    assert!(matches!(c.recv().await, ServerMsg::Ok));
    let mut seen = Vec::new();
    for _ in 0..2 {
        match c.recv().await {
            ServerMsg::PeerStatus { peer, online } => seen.push((peer, online)),
            other => panic!("expected PeerStatus, got {other:?}"),
        }
    }
    assert!(seen.contains(&(peer_b, true)));
    assert!(seen.contains(&(peer_d, false)));

    // D connects → C is notified.
    let d = TestClient::hello(addr, peer_d).await;
    match c.recv().await {
        ServerMsg::PeerStatus { peer, online } => {
            assert_eq!(peer, peer_d);
            assert!(online);
        }
        other => panic!("expected PeerStatus, got {other:?}"),
    }

    // D disconnects → C is notified.
    drop(d);
    match c.recv().await {
        ServerMsg::PeerStatus { peer, online } => {
            assert_eq!(peer, peer_d);
            assert!(!online);
        }
        other => panic!("expected PeerStatus, got {other:?}"),
    }
}
