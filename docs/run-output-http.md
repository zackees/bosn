# Task run output over HTTP

The daemon binds an owner-authenticated HTTP listener on `127.0.0.1` at an OS-selected port. Discover it with `bosn daemon url --state-dir STATE_DIR` and read its per-daemon secret with `bosn daemon token --state-dir STATE_DIR`. The token changes when the daemon restarts. Both files live in the owner-private state directory; on Unix they have mode `0600`.

Send `Authorization: Bearer TOKEN` with each request. `?token=TOKEN` is also accepted; keep token-bearing URLs out of browser history and logs. Every request also needs a matching `Host`; an `Origin`, when present, must name the listener. The daemon accepts GET only.

```sh
state_dir=/path/to/bosn-state
curl -H "Authorization: Bearer $(bosn daemon token --state-dir "$state_dir")" -H 'Accept: application/x-ndjson' "$(bosn daemon url --state-dir "$state_dir")/v1/runs/RUN_ID/stream?from_seq=0"

curl -N -H "Authorization: Bearer $(bosn daemon token --state-dir "$state_dir")" -H 'Accept: text/event-stream' "$(bosn daemon url --state-dir "$state_dir")/v1/runs/RUN_ID/stream?from_seq=0"
```

* `GET /v1/runs` lists run IDs, daemon job IDs, task names, and creation timestamps.
  New runs also include `state` (`running`, `success`, `failure`, `cancelled`, or `interrupted`) and an `ended_unix_ms` timestamp after completion. On daemon restart, unfinished runs are marked `interrupted`.
* `GET /v1/runs/{id}/stream?from_seq=N&streams=stdout,stderr` with `Accept: application/x-ndjson` returns up to 256 committed chunks after sequence `N`. Each row includes the globally ordered `seq`, an RFC 3339 `ts`, `stream`, `data_b64`, and nullable `job_id` and `step_id`. The `X-Next-Seq` response header is the cursor for the next page, including when a channel filter leaves the body empty. Resume works after a daemon restart. `streams` may select one channel.

* Set `Accept: application/vnd.bosn.stream` on the stream route for a binary page. Each frame has a 16-byte header: one stream byte (`1` stdout, `2` stderr), three reserved zero bytes, a big-endian `u32` payload length, and a big-endian `u64` sequence, followed by the exact payload bytes. Use `X-Next-Seq` to request the next page.
* Set `Accept: text/event-stream` for live output plus backlog. Each output event uses the sequence as native SSE `id:`, its channel as `event: stdout` or `event: stderr`, and the JSON record as `data:`. The terminal `event: end` carries the final state, end time, and an exit code when known, with the next sequence ID; then the response closes. `Last-Event-ID: N` resumes at sequences greater than `N`, including after daemon restart. `from_seq=N` is also accepted; when both are present, the greater cursor wins. A channel filter advances the shared cursor even for omitted output. Idle streams send SSE keepalives. Connections have a one-hour maximum lifetime; reconnect with the last event ID. SSE events are capped at 1 MiB; use binary or JSONL replay for larger chunks.
  The exit code is `0` for a successful task and `null` when cancellation, interruption, or a failure does not expose an exact process code.
* `GET /v1/runs/{id}/logs/stdout?offset=B` and the matching `stderr` route return up to 4 MiB of exact bytes. `Range: bytes=B-` is also accepted. The response is `206` with `Content-Range` when bytes remain; repeat at the next offset.

The byte files and index are the replay source. The bounded text job log is a display view and may replace invalid UTF-8. A run's output directory is `STATE_DIR/runs/{id}` and contains owner-only `stdout.log`, `stderr.log`, `index.jsonl`, `run.json`, and a durable `end.json` after completion.
Cross-pipe ordering reflects when the host read each pipe; it does not prove the task's real-time write order.

Act job/step status events and the port allocator remain tracked in Bosn #306 and #303.
