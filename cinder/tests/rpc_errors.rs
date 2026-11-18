mod common;

use common::rpc::codec::{is_blank, LineRead, LineReader, MAX_LINE_BYTES};
use common::rpc::dispatch::{serve_connection, Engine, EngineConfig, Transport};
use common::rpc::protocol::RpcError;
use common::*;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

fn assert_error_frame<'a>(f: &'a Value, ctx: &str) -> &'a Value {
    let e = &f["error"];
    assert!(e.is_object(), "{ctx}: no error object in {f}");
    assert!(e["code"].is_i64(), "{ctx}: code missing in {f}");
    assert!(e["message"].is_string(), "{ctx}: message missing in {f}");
    let d = &e["data"];
    assert!(d["kind"].is_string(), "{ctx}: data.kind missing in {f}");
    assert!(d["retryable"].is_boolean(), "{ctx}: data.retryable missing in {f}");
    assert!(d["hint"].is_string(), "{ctx}: data.hint missing in {f}");
    e
}

// P3 — every line-parseable garbage produces exactly one error frame.
#[tokio::test]
async fn p3_garbage_always_errors() {
    let mut h = harness();
    let cases = [
        "{",                                        // -32700
        "[]",                                       // -32600 batch
        "[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.hello\"}]", // batch array
        "{\"method\":null}",                        // -32600
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":5}", // -32600 non-string method
        "42",                                       // -32600 non-object
        "\"hi\"",                                   // -32600 non-object
        "{\"jsonrpc\":\"1.0\",\"id\":9,\"method\":\"session.hello\"}", // wrong version -32600
        "{\"id\":9,\"method\":\"session.hello\"}",  // missing jsonrpc -32600
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}", // a Response, not Request -32600
    ];
    for (i, case) in cases.iter().enumerate() {
        line(&mut h, case).await;
        let f = frame(&mut h.rx).await;
        let e = assert_error_frame(&f, &format!("case {i}: {case}"));
        assert_eq!(f["id"], Value::Null, "case {i}: error id should be null for {case}");
        let code = e["code"].as_i64().unwrap();
        assert!((-32700..=-32600).contains(&code), "case {i}: bad code {code} for {case}");
    }

    // unknown method with a real id echoes the id and carries a hint
    line(&mut h, &req(77, "no.such", json!({}))).await;
    let f = frame(&mut h.rx).await;
    let e = assert_error_frame(&f, "unknown method");
    assert_eq!(e["code"], -32601);
    assert_eq!(e["data"]["kind"], "unknown_method");
    assert_eq!(f["id"], 77);

    // array params -> -32602
    line(&mut h, &"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"chat.list\",\"params\":[1,2]}".to_string()).await;
    let f = frame(&mut h.rx).await;
    let e = assert_error_frame(&f, "positional params");
    assert_eq!(e["code"], -32602);
    assert_eq!(e["data"]["kind"], "invalid_params");

    // string id echoes byte-for-byte
    line(&mut h, &"{\"jsonrpc\":\"2.0\",\"id\":\"abc-01\",\"method\":\"no.such\"}".to_string()).await;
    let f = frame(&mut h.rx).await;
    assert_eq!(f["id"], "abc-01");

    // whitespace-only lines are the single legal silent drop
    line(&mut h, "   ").await;
    line(&mut h, "\t \r").await;
    assert!(maybe_frame(&mut h.rx, 150).await.is_none(), "blank lines must be ignored");
}

// Notifications produce no frames — but still execute.
#[tokio::test]
async fn notifications_silent_but_processed() {
    let mut h = harness();
    hello(&mut h).await;
    drain(&mut h.rx).await;

    line(&mut h, "{\"jsonrpc\":\"2.0\",\"method\":\"msg.send\",\"params\":{\"to\":\"contact:9f3b12\",\"body\":\"fire-and-forget\",\"client_msg_id\":\"notif-1\"}}").await;
    assert!(maybe_frame(&mut h.rx, 150).await.is_none(), "notification must not answer");
    assert_eq!(h.core.sends.lock().unwrap().len(), 1, "notification still executes");

    // unknown-method notification: logged, never answered
    line(&mut h, "{\"jsonrpc\":\"2.0\",\"method\":\"no.such\"}").await;
    assert!(maybe_frame(&mut h.rx, 150).await.is_none());
}

// Core-originated domain errors surface as -320xx kinds.
#[tokio::test]
async fn domain_errors_propagate() {
    let mut h = harness();
    hello(&mut h).await;
    h.core.fail("msg.send", RpcError::new(common::rpc::protocol::ErrorKind::PeerOffline, "peer unreachable and no mailbox path; wait + retry"));
    let e = call_err(&mut h, 2, "msg.send", json!({"to": "contact:x", "body": "hi", "client_msg_id": "d1"})).await;
    assert_eq!(e["code"], -32011);
    assert_eq!(e["data"]["kind"], "peer_offline");
    assert_eq!(e["data"]["retryable"], true);
    assert!(e["data"]["hint"].is_string());
}

// Param-shape violations per method -> -32602.
#[tokio::test]
async fn param_validation() {
    let mut h = harness();
    hello(&mut h).await;

    let cases: &[(&str, Value)] = &[
        ("msg.send", json!({"to": "nope:x", "body": "b", "client_msg_id": "k"})), // bad prefix
        ("msg.send", json!({"to": "chat:x", "body": "b"})),                       // missing client_msg_id
        ("msg.send", json!({"to": "chat:x", "body": "b", "client_msg_id": ""})),  // empty key
        ("msg.edit", json!({"msg_id": "contact:x", "body": "b"})),                // wrong prefix
        ("msg.delete", json!({"msg_id": "msg:x", "scope": "bogus"})),             // bad enum
        ("chat.history", json!({"chat_id": "chat:x", "limit": 0})),               // limit bounds
        ("chat.history", json!({"chat_id": "chat:x", "limit": 501})),             // over max
        ("chat.history", json!({"chat_id": "chat:x", "before": "notcursor"})),    // bad cursor prefix
        ("contact.add", json!({"link": "http://x"})),                             // bad scheme
        ("invite.accept", json!({"link": "cinder://abc@host"})),                  // no rendezvous
        ("invite.create", json!({"ttl_sec": 5})),                                 // under min
        ("ident.verify", json!({"contact_id": "contact:x", "sas": ""})),          // empty sas
        ("events.subscribe", json!({"kinds": ["bogus.event"]})),                  // unknown kind
        ("events.subscribe", json!({"since": "yesterday"})),                      // wrong type
        ("contact.list", json!({"state": "weird"})),                              // bad enum
        ("msg.react", json!({"msg_id": "msg:x"})),                                // emoji required (null ok)
    ];
    for (i, (method, params)) in cases.iter().enumerate() {
        let e = call_err(&mut h, 100 + i as u64, method, params.clone()).await;
        assert_eq!(e["code"], -32602, "{method} {params}");
        assert_eq!(e["data"]["kind"], "invalid_params", "{method} {params}");
        assert_eq!(e["data"]["retryable"], false, "{method} {params}");
    }

    // unknown subscription / stale resume point -> -32017 not_found
    let e = call_err(&mut h, 200, "events.unsubscribe", json!({"subscription_id": "sub:999"})).await;
    assert_eq!(e["code"], -32017);
    assert_eq!(e["data"]["kind"], "not_found");
    let e = call_err(&mut h, 201, "events.subscribe", json!({"since": 99999})).await;
    assert_eq!(e["code"], -32017);
    assert_eq!(e["data"]["kind"], "not_found");
    let e = call_err(&mut h, 202, "events.subscribe", json!({"since": -1})).await;
    assert_eq!(e["code"], -32017);
    assert_eq!(e["data"]["kind"], "not_found");
}

// P7 — line cap: 4 MiB exactly processes; +1 triggers one -32700 then close.
#[tokio::test]
async fn p7_line_cap() {
    let (mut cli, srv) = duplex(1024);
    let (srv_read, srv_write) = tokio::io::split(srv);

    let core = Arc::new(MockCore::basic());
    let engine = Engine::new(core.clone(), EngineConfig::default());
    core.attach(engine.events.clone());
    let task = tokio::spawn(serve_connection(engine, BufReader::new(srv_read), srv_write, Transport::Stdio));

    // exactly at cap: a huge whitespace-ish valid request padded with a big string id
    let body = "{\"jsonrpc\":\"2.0\",\"id\":\"";
    let tail = "\",\"method\":\"no.such\"}";
    let pad = MAX_LINE_BYTES - body.len() - tail.len();
    let big_line = format!("{body}{}{tail}", "x".repeat(pad));
    assert_eq!(big_line.len(), MAX_LINE_BYTES);
    cli.write_all(big_line.as_bytes()).await.unwrap();
    cli.write_all(b"\n").await.unwrap();

    let mut br = BufReader::new(cli);
    let mut buf = String::new();
    let n = br.read_line(&mut buf).await.unwrap();
    assert!(n > 0, "expected an error frame for the exactly-at-cap line");
    let f: Value = serde_json::from_str(buf.trim()).unwrap();
    assert_eq!(f["error"]["code"], -32601, "exactly-4MiB line must be processed: {buf}");

    // cap+1: one final -32700 then connection closes
    let mut over = "{\"jsonrpc\":\"2.0\",\"id\":\"y\",\"method\":\"no.such\",\"pad\":\"".to_string();
    over.push_str(&"x".repeat(MAX_LINE_BYTES));
    over.push_str("\"}");
    assert!(over.len() > MAX_LINE_BYTES);
    cli_write(&mut br, over.as_bytes()).await;
    cli_write(&mut br, b"\n").await;

    buf.clear();
    let n = br.read_line(&mut buf).await.unwrap();
    assert!(n > 0, "over-cap line must emit a final error frame");
    let f: Value = serde_json::from_str(buf.trim()).unwrap();
    assert_eq!(f["error"]["code"], -32700, "over-cap line must get -32700: {buf}");
    assert_eq!(f["id"], Value::Null);
    assert_eq!(f["error"]["data"]["kind"], "parse_error");

    buf.clear();
    let n = br.read_line(&mut buf).await.unwrap();
    assert_eq!(n, 0, "connection must close after the over-cap frame (AGENT-API §1.3)");
    let _ = task.await;
}

async fn cli_write(cli: &mut (impl tokio::io::AsyncWrite + Unpin), bytes: &[u8]) {
    cli.write_all(bytes).await.unwrap();
    cli.flush().await.unwrap();
}

// Codec unit-level checks independent of transport.
#[tokio::test]
async fn codec_oversized_and_eof() {
    // oversized drains to newline then keeps reading
    let (mut w, r) = duplex(1024);
    let mut lr = LineReader::with_cap(BufReader::new(r), 16);
    w.write_all(b"0123456789abcdefg\n").await.unwrap(); // 17 > 16
    w.write_all(b"ok\n").await.unwrap();
    assert_eq!(lr.read_line().await.unwrap(), Some(LineRead::Oversized));
    assert_eq!(lr.read_line().await.unwrap(), Some(LineRead::Line(b"ok".to_vec())));

    // oversized spanning many buffer fills, EOF-terminated (writer runs on a
    // task: the 16-byte socket buffer can't hold 10k, it needs a live reader)
    let (mut w, r) = duplex(16);
    let mut lr = LineReader::with_cap(BufReader::new(r), 16);
    let wr = tokio::spawn(async move {
        let _ = w.write_all(&vec![b'x'; 10_000]).await;
    });
    assert_eq!(lr.read_line().await.unwrap(), Some(LineRead::Oversized));
    assert_eq!(lr.read_line().await.unwrap(), None);
    wr.await.unwrap();

    // exactly at cap accepted, cap+1 rejected
    let (mut w, r) = duplex(1024);
    let mut lr = LineReader::with_cap(BufReader::new(r), 4);
    w.write_all(b"abcd\n").await.unwrap();
    w.write_all(b"abcde\n").await.unwrap();
    assert_eq!(lr.read_line().await.unwrap(), Some(LineRead::Line(b"abcd".to_vec())));
    assert_eq!(lr.read_line().await.unwrap(), Some(LineRead::Oversized));

    assert!(is_blank(b" \t\r"));
    assert!(!is_blank(b"{"));
}
