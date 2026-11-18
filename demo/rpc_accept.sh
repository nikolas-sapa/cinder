#!/usr/bin/env bash
# demo helper: pull the pending_in contact id out of clist.json, then
# accept it and replay events (since:0) so the verification_needed event
# with sas_hint lands in accept.json.
set -u
CID=$(grep -oE 'contact:[a-z2-7]{20,}' /tmp/cinder-demo/clist.json | tail -n1)
( echo '{"jsonrpc":"2.0","id":1,"method":"session.hello","params":{"protocol_version":[1]}}'
  sleep 0.5
  echo "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"contact.accept\",\"params\":{\"contact_id\":\"$CID\"}}"
  sleep 1.5
  echo '{"jsonrpc":"2.0","id":4,"method":"events.subscribe","params":{"since":0}}'
  sleep 1.5
  echo '{"jsonrpc":"2.0","id":9,"method":"shutdown.request","params":{"grace_ms":500}}'
) | cinder rpc | tee /tmp/cinder-demo/accept.json
