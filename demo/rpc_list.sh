#!/usr/bin/env bash
# demo helper: one-shot rpc session (hello -> contact.list -> shutdown).
# stdout tee'd so the pane shows the frames AND we can parse the file.
set -u
( echo '{"jsonrpc":"2.0","id":1,"method":"session.hello","params":{"protocol_version":[1]}}'
  sleep 0.5
  echo '{"jsonrpc":"2.0","id":2,"method":"contact.list","params":{}}'
  sleep 0.5
  echo '{"jsonrpc":"2.0","id":9,"method":"shutdown.request","params":{"grace_ms":500}}'
) | cinder rpc | tee /tmp/cinder-demo/clist.json
