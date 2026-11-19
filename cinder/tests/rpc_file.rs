//! RPC-layer file transfer coverage: param validation (FT5/FT11), inflight
//! error propagation (FT6), accept/decline/cancel states (FT13),
//! client_xfer_id dedup (FT17), info/list/thumb surface (FT18), and
//! describe limits (FT20).
mod common;

use common::*;
use serde_json::json;

fn send_params() -> serde_json::Value {
    json!({
        "to": "contact:peer1",
        "data": data_encoding::BASE64.encode(b"hello file"),
        "name": "note.txt",
        "client_xfer_id": "cx-1",
    })
}

// ---------------------------------------------------------------------------
// FT5 + FT11: file.send validation.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn ft5_file_send_param_validation() {
    let mut h = harness();
    hello(&mut h).await;

    // Neither path nor data → -32602.
    let e = call_err(
        &mut h,
        1,
        "file.send",
        json!({"to": "contact:p", "name": "x", "client_xfer_id": "a"}),
    )
    .await;
    assert_eq!(e["code"], -32602);

    // Both path and data → -32602.
    let mut p = send_params();
    p["path"] = json!("/tmp/x");
    let e = call_err(&mut h, 2, "file.send", p).await;
    assert_eq!(e["code"], -32602);

    // Oversize name / caption / mime → -32602.
    for (k, v) in [
        ("name", json!("n".repeat(256))),
        ("caption", json!("c".repeat(1025))),
        ("mime", json!("m".repeat(65))),
    ] {
        let mut p = send_params();
        p[k] = v;
        p["client_xfer_id"] = json!(format!("cx-{k}"));
        let e = call_err(&mut h, 3, "file.send", p).await;
        assert_eq!(e["code"], -32602, "{k} over limit must fail");
    }

    // Empty client_xfer_id → -32602.
    let mut p = send_params();
    p["client_xfer_id"] = json!("");
    let e = call_err(&mut h, 4, "file.send", p).await;
    assert_eq!(e["code"], -32602);

    // kind:voice without audio metadata → -32602; with it → ok (FT11).
    let mut p = send_params();
    p["kind"] = json!("voice");
    p["client_xfer_id"] = json!("cx-v1");
    let e = call_err(&mut h, 5, "file.send", p.clone()).await;
    assert_eq!(e["code"], -32602);
    p["audio"] = json!({"duration_ms": 1500, "codec": "opus"});
    p["client_xfer_id"] = json!("cx-v2");
    let r = call(&mut h, 6, "file.send", p).await;
    assert!(r["file_id"].as_str().unwrap().starts_with("file:"));

    // Unknown kind → -32602.
    let mut p = send_params();
    p["kind"] = json!("movie");
    p["client_xfer_id"] = json!("cx-k");
    let e = call_err(&mut h, 7, "file.send", p).await;
    assert_eq!(e["code"], -32602);

    // data > 1 MiB decoded (b64 length bound) → -32602.
    let mut p = send_params();
    p["data"] = json!("A".repeat(2 * 1024 * 1024));
    p["client_xfer_id"] = json!("cx-big");
    let e = call_err(&mut h, 8, "file.send", p).await;
    assert_eq!(e["code"], -32602);
}

// ---------------------------------------------------------------------------
// FT6 (surface half): a core inflight cap surfaces as -32015, not a hang.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn ft6_inflight_cap_surfaces() {
    let mut h = harness();
    hello(&mut h).await;
    h.core.fail(
        "file.send",
        common::rpc::protocol::RpcError::new(
            common::rpc::protocol::ErrorKind::RateLimited,
            "too many inflight transfers to this peer",
        ),
    );
    let e = call_err(&mut h, 1, "file.send", send_params()).await;
    assert_eq!(e["code"], -32015);
}

// ---------------------------------------------------------------------------
// FT13: accept/decline/cancel reach the expected wire-facing states and
// file_id params are prefix-validated.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn ft13_accept_decline_cancel_states() {
    let mut h = harness();
    hello(&mut h).await;

    for (method, want) in [
        ("file.accept", "transferring"),
        ("file.decline", "declined"),
        ("file.cancel", "cancelled"),
    ] {
        let r = call(&mut h, 1, method, json!({"file_id": "file:svc0001"})).await;
        assert_eq!(r["file_id"], "file:svc0001");
        assert_eq!(r["state"], want, "{method} must return {want}");
    }

    // Prefix validation: msg: ids are not file ids.
    for method in ["file.accept", "file.decline", "file.cancel", "file.info"] {
        let e = call_err(&mut h, 2, method, json!({"file_id": "msg:abc"})).await;
        assert_eq!(e["code"], -32602, "{method} prefix check");
    }
}

// ---------------------------------------------------------------------------
// FT17: client_xfer_id dedup — same payload replays with deduped:true, a
// conflicting payload under the same key is rejected.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn ft17_client_xfer_id_dedup() {
    let mut h = harness();
    hello(&mut h).await;

    let r1 = call(&mut h, 1, "file.send", send_params()).await;
    assert_eq!(r1["deduped"], false);
    assert_eq!(r1["state"], "offered");
    assert!(r1["file_id"].as_str().unwrap().starts_with("file:"));
    assert!(r1["msg_id"].as_str().unwrap().starts_with("msg:"));
    assert!(r1["chat_id"].as_str().unwrap().starts_with("chat:"));

    // Replay: same key + same payload → deduped:true, same file_id.
    let r2 = call(&mut h, 2, "file.send", send_params()).await;
    assert_eq!(r2["deduped"], true);
    assert_eq!(r2["file_id"], r1["file_id"]);

    // Conflict: same key, different name → -32609 conflict class.
    let mut p = send_params();
    p["name"] = json!("other.txt");
    let e = call_err(&mut h, 3, "file.send", p).await;
    let code = e["code"].as_i64().unwrap();
    assert!(
        code == -32609 || (-32099..=-32000).contains(&code),
        "conflicting client_xfer_id reuse must fail, got {e}"
    );
    // Only ONE core call happened.
    assert_eq!(h.core.file_calls.lock().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// FT18: file.info / file.list / file.thumb surface rows without fetch_token.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn ft18_info_list_thumb_surface() {
    let mut h = harness();
    hello(&mut h).await;

    let info = call(&mut h, 1, "file.info", json!({"file_id": "file:svc0001"})).await;
    for f in [
        "file_id",
        "msg_id",
        "chat_id",
        "dir",
        "name",
        "mime",
        "size",
        "sha256",
        "kind",
        "state",
        "chunks_total",
        "chunks_done",
    ] {
        assert!(info.get(f).is_some(), "file.info missing {f}");
    }
    assert!(
        info.get("fetch_token").is_none(),
        "fetch_token must never leak over RPC"
    );

    let list = call(&mut h, 2, "file.list", json!({})).await;
    assert_eq!(list["files"].as_array().unwrap().len(), 1);
    let row = &list["files"][0];
    assert!(row.get("fetch_token").is_none());

    let filtered = call(&mut h, 3, "file.list", json!({"chat_id": "chat:nope000"})).await;
    assert_eq!(filtered["files"].as_array().unwrap().len(), 0);

    let thumb = call(&mut h, 4, "file.thumb", json!({"file_id": "file:svc0001"})).await;
    assert_eq!(thumb["file_id"], "file:svc0001");
    assert!(thumb.get("thumb_b64").is_some()); // null is a valid answer
}

// ---------------------------------------------------------------------------
// FT20: describe documents the file surface + normative limits.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn ft20_describe_limits_and_ids() {
    let mut h = harness();
    hello(&mut h).await;
    let d = call(&mut h, 1, "rpc.describe", json!({})).await;

    let limits = &d["limits"];
    for l in [
        "file_max_bytes",
        "file_chunk_bytes",
        "file_inline_max_bytes",
        "file_max_inflight",
        "file_max_inflight_peer",
        "file_voice_max_ms",
    ] {
        assert!(limits.get(l).is_some(), "limits.{l} missing");
    }
    assert_eq!(limits["file_max_bytes"], 64 * 1024 * 1024);
    assert_eq!(limits["file_inline_max_bytes"], 1024 * 1024);

    let methods = d["methods"].as_object().unwrap();
    assert_eq!(methods["file.save"]["scope"], "read");
    assert_eq!(methods["file.send"]["scope"], "send");
    assert_eq!(methods["file.thumb"]["scope"], "read");
    let ev = d["events"].as_object().unwrap();
    assert!(ev.contains_key("file_state"));
}
