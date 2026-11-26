# Observability

How to see what molcajete is doing: logs from both processes, Prometheus metrics for every stage a frame goes through, CPU profiles, and performance reports from the browser. With them you can find where a slow session spends its time, after the fact. Installing and configuring molcajete is in [deployment.md](deployment.md).

## Options

Everything is off by default and costs nothing unless enabled: metrics are recorded through the `metrics` facade, whose calls do nothing until a recorder is installed.

| Flag | Environment variable | Applies to | |
| --- | --- | --- | --- |
| `--log-format json` | `MOLCAJETE_LOG_FORMAT` | both | One JSON object per line, with the current span's fields (`tunnel_id`, `connection`, `id`, ...). The default is `text`; the images set `json`. |
| `--telemetry-listen ADDR` | `MOLCAJETE_TELEMETRY_LISTEN` | both | A separate HTTP listener for `/metrics` (Prometheus text format) and `/healthz`. Without it no metrics are collected. |
| `--profiling` | `MOLCAJETE_PROFILING` | both | Adds `/debug/pprof/profile` and `/debug/pprof/flamegraph` to the telemetry listener. Has no effect without `--telemetry-listen`. |
| `--client-reports` | `MOLCAJETE_CLIENT_REPORTS` | gateway | Accept browser performance reports (`POST /api/telemetry/client`), logged and recorded as metrics. |
| | `RUST_LOG` | both | Log filter. Defaults: `info,tower_http=warn,pprof=warn` (gateway), `info,pprof=warn` (molcajeted). |

The telemetry listener is separate from the service port so that the reverse proxy never exposes it. `deploy/compose.yml` publishes it on `127.0.0.1:9100` (gateway) and `127.0.0.1:9101` (molcajeted); the Kubernetes template names it `telemetry` on 9100 and 9101 so an in-cluster collector finds it.

## Profiling

`--profiling` serves CPU profiles sampled with pprof-rs (SIGPROF), only while a request is in progress:

| Endpoint | Query | Returns |
| --- | --- | --- |
| `/debug/pprof/profile` | `seconds` (default 10, at most 300), `frequency` in Hz (default 99, at most 1000) | pprof protobuf, for `go tool pprof` and Pyroscope |
| `/debug/pprof/flamegraph` | the same | SVG flamegraph |

```bash
# a flamegraph to look at directly
curl -o gw.svg 'http://127.0.0.1:9100/debug/pprof/flamegraph?seconds=30'
# pprof format
curl -o daemon.pb 'http://127.0.0.1:9101/debug/pprof/profile?seconds=30&frequency=199'
go tool pprof -http=:0 daemon.pb
```

- One profile at a time per process; a concurrent request gets 409.
- Build with line tables (the `profiling` Cargo profile, the images' default) so stacks resolve to source lines, inlined functions included. The `profiling` feature of `molcajete-telemetry` is on by default; without it `--profiling` fails at startup.
- The first profile loads the binary's line tables to symbolize stacks, which adds about 150 MiB to the process. Samples are buffered in a temporary file, so `/tmp` must be writable.
- An idle process produces empty profiles, which Pyroscope does not store.

For continuous profiling, Grafana Alloy's `pyroscope.scrape` fetches a profile every 15 seconds and pushes it to Pyroscope (`deploy/observability/molcajete.alloy`). Profiles accumulate while the system is in use, so you can pick a time range, such as the ten minutes last night when an RDP session was slow, and compare flamegraphs.

## What is measured

A frame goes through the following path, and every stage has metrics:

```text
remote server -> molcajeted: receive and decode / diff / scroll and move detection / image or video encoding -> socket write
              -> gateway relay -> WebSocket -> browser: receive and parse / decode / apply / draw -> sync ack
```

Latency histograms use buckets from 0.25 ms to 10 s; metrics ending in `_duration_seconds` use buckets up to 8 hours, `_bytes` up to 16 MiB, `_fps` up to 144, and `_ops`, `_frames` and `_images` small counts up to 1024. Both processes also export `molcajete_build_info{service, version}` (always 1), to tell which version produced a series.

### molcajeted

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `molcajeted_connections_active` | gauge | protocol | Open connections |
| `molcajeted_connections_total` | counter | protocol, result | Connections by how they ended: `ok`, `error`, `handshake_failed`, `unsupported` (protocol `unknown` for the last two when the handshake did not get that far) |
| `molcajeted_session_duration_seconds` | histogram | protocol | Session length |
| `molcajeted_joined_users_active` | gauge | protocol | Users who joined a connection (share links, administrators watching); each also counts in the frame and image metrics, since it gets frames of its own |
| `molcajeted_remote_received_bytes_total` | counter | protocol | Bytes read from the remote server's socket (`rdp`: TLS records, handshake included; `vnc`) |
| `molcajeted_remote_frames_total` | counter | protocol | `rdp` only: frames the server marked the end of with Frame Marker surface commands (graphics pipeline frames are counted by `molcajeted_rdp_gfx_frames_total`). Zero for a server that sends classic bitmap updates; molcajeted then cannot keep its updates whole |
| `molcajeted_remote_decode_seconds` | histogram | protocol | `rdp` only: decoding one PDU from the server (IronRDP, bitmap decompression and the graphics pipeline's codecs included) and copying its graphics updates into the display |
| `molcajeted_rdp_gfx_frames_total` | counter | | Frames the RDP server ended on the graphics pipeline (and molcajeted acknowledged). Zero while `molcajeted_frames_total{protocol="rdp"}` grows: the sessions use bitmap updates |
| `molcajeted_rdp_gfx_frame_decode_seconds` | histogram | | Decoding one graphics pipeline frame: all its messages, H.264 and colour conversion included. The server gets its acknowledgement after this time, so it bounds the frame rate the server can reach |
| `molcajeted_rdp_gfx_pixels_total` | counter | codec | Pixels decoded on the graphics pipeline by codec: `avc420`, `avc444`, `progressive`, `remotefx`, `clearcodec`, `planar`, `uncompressed`, `alpha`. Shows which codecs a server really uses |
| `molcajeted_frame_flush_seconds` | histogram | protocol | CPU time to diff and encode one frame |
| `molcajeted_flush_phase_seconds` | histogram | protocol, phase | Time per flush phase: `video`, `scroll`, `moves`, `pixels` (includes image encoding), `refine` |
| `molcajeted_copies_total` | counter | protocol, phase | Copies found by `scroll` and by `moves` detection. Compare with that phase's share of `molcajeted_flush_phase_seconds` to see what detection costs per copy |
| `molcajeted_image_encode_seconds` | histogram | protocol, format | Encoding one image: `png`, `jpeg`, `zstd`. Images of a frame are encoded in parallel, so these add up to more than the `pixels` phase |
| `molcajeted_image_encoded_bytes` | histogram | protocol, format | Size of one encoded image; many small images cost the client a decode each |
| `molcajeted_image_bytes_total`, `molcajeted_image_pixels_total` | counter | protocol, format | Encoded size and pixels processed (compression efficiency) |
| `molcajeted_video_streams_total` | counter | codec | Video mode starts (`av1`, `h265`, `h264`) |
| `molcajeted_video_failures_total` | counter | | Video encoder failures (after three in a session, video is off for it) |
| `molcajeted_video_encode_seconds` | histogram | codec, key | Encoding one video frame, including the round trip to ffmpeg |
| `molcajeted_video_frame_bytes` | histogram | codec, key | Video frame size |
| `molcajeted_frames_total` | counter | protocol | Frames (`sync`) sent |
| `molcajeted_frame_encoded_bytes` | histogram | protocol | Encoded image bytes per frame |
| `molcajeted_frame_images` | histogram | protocol | Images per frame (fills and copies not counted): the client decodes each, and the daemon encodes them side by side |
| `molcajeted_client_ack_seconds` | histogram | protocol | From sending a frame to the client's ack arriving: network round trip, plus the time the frame's data takes to arrive (`molcajete_client_frame_receive_seconds`), plus browser processing |
| `molcajeted_client_ack_wait_seconds` | histogram | protocol | How long an ack then waited for the session loop (busy building a frame or handling server data); not part of `client_ack`. The time an SSH session spends connecting to the remote host, in which it reads no input, is left out |
| `molcajeted_client_apply_seconds` | histogram | protocol | How long the client says it took to apply a frame (decoding and drawing, without waiting behind earlier frames). Only molcajete's client reports it, and for those clients it sets the frame interval (median of the last 8 reports, forgotten after 1 s) |
| `molcajeted_unacked_frames` | histogram | protocol | Frames still unacknowledged when a frame is sent; at most 3 for RDP and VNC, 2 for SSH |
| `molcajeted_socket_write_seconds` | histogram | protocol | Time blocked writing to the gateway; grows when the gateway or browser cannot keep up (backpressure) |
| `molcajeted_sent_bytes_total` | counter | protocol | Bytes sent to the gateway |

### Gateway

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `molcajete_gateway_handshake_seconds` | histogram | connection, protocol | Handshake with molcajeted or guacd, including its connection to the remote server |
| `molcajete_gateway_handshake_failures_total` | counter | connection, protocol | Failed handshakes |
| `molcajete_gateway_tunnels_rejected_total` | counter | reason | Tunnels refused before a handshake: `unknown_connection` (no such connection, or the user may not open it), `join` (an unknown, expired or revoked share link, or watching refused) |
| `molcajete_gateway_recording_failures_total` | counter | connection, protocol | Sessions refused because their recording could not be started |
| `molcajete_gateway_tunnels_active` | gauge | connection, protocol | Open tunnels |
| `molcajete_gateway_tunnels_total` | counter | connection, protocol, status | Closed tunnels by Guacamole status code |
| `molcajete_gateway_tunnel_duration_seconds` | histogram | connection, protocol | Tunnel length |
| `molcajete_gateway_relayed_bytes_total` | counter | connection, protocol, direction | Bytes relayed: `to_browser` (as read from the daemon), `to_guacd` |
| `molcajete_gateway_logins_throttled_total` | counter | reason | Password sign-ins refused before the password was checked: `address` or `username` (too many failures, `429`), `busy` (no free password check within `verification_wait_secs`, `503`). See [Repeated failed sign-ins](administration.md#repeated-failed-sign-ins) |
| `molcajete_gateway_audit_write_failures_total` | counter | | Audit log entries that could not be stored in the database (the change itself went through and the `audit` log line was written) |
| `molcajete_gateway_sessions_ended_total` | counter | reason | Sessions ended by the gateway: `admin` (an administrator ended them), `user_disabled`, `user_deleted`, `access_revoked`, and for sessions that joined another `owner_left`, `link_revoked` |
| `molcajete_gateway_history_write_failures_total` | counter | | Closed sessions that could not be stored in the connection history |

### Browser reports

With `--client-reports`, the web client batches its once-per-second statistics and posts them to `POST /api/telemetry/client` every 10 seconds (or after 60 samples, and once more when the session ends). Only signed-in users can post, for a connection that exists, with at most 120 samples per report; the page stops reporting when the gateway answers 404, which it does while reports are off.

Every browser metric has four labels with a handful of values each, so the number of series stays bounded:

| Label | Values |
| --- | --- |
| `connection` | The connection id |
| `mode` | `worker` or `main`: where the client runs (`unknown` for anything else) |
| `browser` | The engine, from the report's `User-Agent`: `chromium` (Chrome, Edge and other Chromium browsers), `firefox`, `safari` (Safari, and every browser on iOS and iPadOS), `other` |
| `device` | `desktop`, `tablet` or `mobile`, from the `User-Agent`. An iPad, whose browsers claim to be desktop Safari, is told from a Mac by its touch screen |

Engine and device decide how fast a page decodes and draws, and they stay the same for the whole session; the display size would split a session into several series whenever the window is resized. Two clients of the same kind share their series; tell them apart in the logs, where each `client report` names its tunnel ([Logs](#logs)).

All are histograms with one sample per second of the report:

| Metric | Meaning |
| --- | --- |
| `molcajete_client_frame_lag_seconds` | From receiving a frame's `sync` to applying it and acknowledging: waiting for its images to decode and for the frames before it (average per second). The time the page was hidden, in which the browser may suspend it, is left out |
| `molcajete_client_frame_receive_seconds` | From a frame's first instruction that draws on the screen (or into a buffer the frame then uses) to its `sync`: how long its data took to arrive and be parsed (average per second; no sample for a second without frames). A cursor change sent between frames does not start a frame |
| `molcajete_client_frame_present_seconds` | From applying a frame to having drawn the canvas with it: the wait for the next animation frame plus the composite (average per second; no sample for a second without a drawn frame). The browser shows the canvas at a later display refresh, which is not included |
| `molcajete_client_image_decode_seconds` | Decoding one image (average per second). For PNG, JPEG and WebP: from handing the data to the browser's decoder to the decoded image, waiting included. For zstd images: the decode itself, which runs while the frame is drawn |
| `molcajete_client_images_ops` | Images decoded per second |
| `molcajete_client_rtt_seconds` | Tunnel ping round trip |
| `molcajete_client_frames_fps` | Frames applied per second |
| `molcajete_client_queued_ops` | Operations waiting to be drawn |
| `molcajete_client_receive_rate_bytes` | Bytes received per second |

## Logs

Both processes log to stdout. With `--log-format json`, every line is one JSON object with the event's fields, its message under `message`, and the fields of the current span under `span` (the gateway's tunnel, molcajeted's connection). In Loki, extract the fields with `| json`; nested span fields become `span_<name>`.

### Sessions

| `message` | Source | Main fields |
| --- | --- | --- |
| `opening tunnel` | gateway | `connection`, `protocol`, `width`, `height`, `overrides` (display parameters the browser asked for), `record` |
| `tunnel established` | gateway | `guacd_id`, `version`, `framing` (`text`, `transcoded`, `binary`), `handshake_ms` |
| `tunnel closed` | gateway | `status`, `duration_s`, `bytes_to_browser`, `bytes_to_guacd`, and for a session the gateway ended `ended_for` (`admin`, `user_disabled`, `user_deleted`, `access_revoked`) and `ended_by` |
| `ending session` | gateway | `session` (tunnel id), `user`, `connection`, `reason`, `by` |
| `client report` | gateway | `session`, `tunnel_id`, `guacd_id`, `connection`, `mode`, `browser`, `device`, `samples`, `fps_avg`, `fps_min`, `frame_lag_ms_avg`, `frame_lag_ms_max`, `frame_receive_ms_max`, `images_per_s`, `image_decode_ms_max`, `frame_present_ms_avg`, `frame_present_ms_max`, `rtt_ms_avg`, `queued_ops_max`, `recv_bytes_per_s`, `send_buffered_max`, `display_width`, `display_height` |
| `guacd handshake failed`, `tunnel requested for unknown or forbidden connection` | gateway | `error`, `connection` |
| `connection established` | molcajeted | `width`, `height`, `dpi`, `image` (mimetypes the client announced), `video`, `binary` |
| `connection closed` | molcajeted | `duration_s` |
| `display summary` | molcajeted | Whole-session `frames`, `fps_avg`, `encoded_bytes`, `flush_ms_avg/max`, `scroll_ms_avg`, `moves_ms_avg`, `pixels_ms_avg`, `frame_images_max`, `scroll_copies`, `move_copies`, `client_ack_ms_avg/max`, `client_ack_wait_ms_avg/max`, `png_*`, `jpeg_*`, `zstd_*` (`images`, `bytes`, `ms_avg`, `ms_max`), `video_frames`, `video_bytes`, `video_encode_ms_avg/max` |
| `video mode started` | molcajeted | `codec`, `x`, `y`, `width`, `height`, `kbps` |
| `video encoder failed` | molcajeted | `error`, `failures` |
| `video encoders` | molcajeted | At startup: the encoders found per codec |
| `RDP session active` | molcajeted | `width`, `height`, `fastpath_input`, `graphics_pipeline_offered` (false with `disable-gfx`, `force-lossless` or a colour depth below 32) |
| `RDP graphics pipeline in use` | molcajeted | Once, when the server took up the graphics pipeline: `version`, `avc420`, `avc444`, `h264_decoder`. A session without this line gets bitmap updates |
| `RDP graphics pipeline summary` | molcajeted | Whole-session `frames`, `decode_ms_avg/max`, `dropped_updates` (updates a codec could not decode), `<codec>_pixels` |

Every gateway line of a tunnel carries the span fields `tunnel_id`, `connection`, `session`, `peer` and `user`. Every molcajeted line of a connection carries `id`, `protocol` and `peer`.

### Following one session

```text
browser session (client report.session)
  = gateway tunnel span.session  ->  gateway tunnel_id, guacd_id
  = molcajeted connection span.id (= guacd_id)
```

A `client report` carries the `tunnel_id` and `guacd_id` of the tunnel its page opened. They are looked up by `session`, and are absent for a page that reported before its tunnel was established or more than two minutes after it closed. Every molcajeted line of a connection carries the id as `span.id`, so a client report and its session's `display summary` share `guacd_id` = `span_id`:

```logql
{namespace="molcajete", container="gateway"} | json | message="client report" | guacd_id="$abcd..."
{namespace="molcajete", container="molcajeted"} | json | message="display summary" | span_id="$abcd..."
```

### Sign-ins and changes

Every entry of the [audit log](administration.md#audit-log) (sign-ins and their failures, sign-outs, changes to users, groups, connections, folders and grants, recordings downloaded or deleted) is also one log line with the target `audit`. Its message is the action, such as `user.update`; with `--log-format json` the line carries `target: "audit"` and these fields, empty when they do not apply:

| Field | |
| --- | --- |
| `entry` | The entry's id in `molcajete_audit_log` |
| `action` | `login.failure`, `connection.update`, ... ([the list](administration.md#actions)) |
| `actor`, `actor_id` | Username at the time (or the name a failed sign-in gave) and user id |
| `source` | The browser's address, or `configuration file`, `first start`, `command line` |
| `target_type`, `target_id`, `target_name` | What the entry is about |
| `changes` | The changed fields as a JSON object serialized into a string, secrets replaced by `[redacted]` |

The line is written before the entry is stored, so a SIEM gets it even when the database write fails (`cannot store an audit log entry`, `molcajete_gateway_audit_write_failures_total`). In Loki:

```logql
{namespace="molcajete", container="gateway"} | json | target="audit" | action="login.failure"
```

Other lines about sign-ins and recordings:

| `message` | Fields |
| --- | --- |
| `single sign-on failed` | `code` and `error` (the browser only sees `?login_error=<code>`) |
| `too many failed sign-ins; refusing more for a while` | `reason` (`address`, `username`), `username`, `source`, `retry_after` (seconds); once per lockout, like `login.throttled` |
| `recording the session`, `recording complete`, `deleted an old recording`, `writing the recording failed; the rest of the session is not recorded`, `cannot start the recording; refusing the session` | `recording`, `file`, `bytes`, `error` |
| `created the first administrator; sign in and change this password` | `username`, `password` (only for an empty database) |

## Finding bottlenecks

### Queries

```promql
# p95 time to produce one frame (server CPU)
histogram_quantile(0.95, sum by (le, protocol) (rate(molcajeted_frame_flush_seconds_bucket[5m])))

# which flush phase uses the CPU (fraction of one core)
sum by (phase) (rate(molcajeted_flush_phase_seconds_sum[5m]))

# image encoding throughput (Mpx/s) and compression (bytes/pixel)
sum by (format) (rate(molcajeted_image_pixels_total[5m])) / 1e6
sum by (format) (rate(molcajeted_image_bytes_total[5m])) / sum by (format) (rate(molcajeted_image_pixels_total[5m]))

# p95 from send to ack against its parts: tunnel round trip, the frame's
# data arriving (a large frame takes longer than the ping), browser processing
histogram_quantile(0.95, sum by (le) (rate(molcajeted_client_ack_seconds_bucket[5m])))
histogram_quantile(0.95, sum by (le) (rate(molcajete_client_rtt_seconds_bucket[5m])))
histogram_quantile(0.95, sum by (le) (rate(molcajete_client_frame_receive_seconds_bucket[5m])))
histogram_quantile(0.95, sum by (le) (rate(molcajete_client_frame_lag_seconds_bucket[5m])))

# the same per kind of client: a slow tablet and a fast desktop separately
histogram_quantile(0.95, sum by (le, browser, device) (rate(molcajete_client_frame_lag_seconds_bucket[5m])))

# what taking pixels in costs: bytes received from the remote server,
# and decoding time as a fraction of one core
sum by (protocol) (rate(molcajeted_remote_received_bytes_total[5m]))
sum by (protocol) (rate(molcajeted_remote_decode_seconds_sum[5m]))

# socket write waits (backpressure)
histogram_quantile(0.99, sum by (le) (rate(molcajeted_socket_write_seconds_bucket[5m])))

# tunnels that did not get going
sum by (connection) (rate(molcajete_gateway_handshake_failures_total[5m]))
sum by (reason) (rate(molcajete_gateway_tunnels_rejected_total[5m]))
```

```logql
# slow sessions
{namespace="molcajete", container="molcajeted"} | json | message="display summary" | flush_ms_max > 100

# periods of high browser frame lag
{namespace="molcajete", container="gateway"} | json | message="client report" | frame_lag_ms_max > 50
```

### Reading them

| Symptom | Suspect | Next step |
| --- | --- | --- |
| High `frame_flush` p95 | Server CPU | Narrow it down with `flush_phase`, then look at the molcajeted CPU flamegraph for that time range |
| High `phase="pixels"` and `image_encode{format="png"}` | PNG encoding | Compare bytes per pixel with encoding time. molcajete's client should get `format="zstd"` for nearly all lossless images; if it gets PNG, check `image` in `connection established`, `--image-zstd`, and whether the connection is recorded (recorded sessions get stock formats) |
| `video_encode` above the frame interval (16 ms at best for RDP and VNC) | Video encoder | Compare codecs, and check in `video encoders` whether a hardware encoder was found |
| High `remote_decode` share of a core | Taking in the remote desktop (RDP) | Compare with `remote_received_bytes`; a busy desktop sends more bitmap updates |
| High `client_ack` but low `client_frame_lag` | Network or proxy | Check `client_rtt`, `client_frame_receive`, proxy buffering and `socket_write` waits |
| High `client_frame_lag` and `client_queued_ops` | Browser decoding or GPU | Compare `mode="main"` with `worker` and the kinds of client, and profile with the browser's DevTools Performance panel |
| Long `socket_write` waits | Gateway or beyond | Look at the gateway flamegraph and the rate of `relayed_bytes` |

## Grafana stack

### Requirements

- **Logs**: nothing extra when a collector already ships pod stdout to Loki.
- **Metrics**: a Prometheus-compatible store (Prometheus with the remote-write receiver enabled, or Mimir) and a Grafana datasource.
- **Profiles**: Pyroscope and a Grafana datasource.

### Alloy

Add `deploy/observability/molcajete.alloy` to an existing Grafana Alloy configuration. It reuses `discovery.kubernetes "pods"`, keeps pods labelled `app.kubernetes.io/name=molcajete` and container ports named `telemetry`, and adds the labels `namespace`, `pod`, `container` and `service_name` (`molcajete/gateway`, `molcajete/molcajeted`, which Pyroscope groups profiles by). It scrapes metrics every 15 seconds and CPU profiles (`profile.process_cpu`, with the Go-specific profile types turned off) at the same interval. Set the two write endpoints to your Prometheus and Pyroscope.

For molcajeted running natively on a Mac, add a static target for `host.orb.internal:9101` (or wherever its telemetry listener is) with the same labels.

### Dashboard

`deploy/observability/grafana-dashboard.json` (*Molcajete Performance*) expects datasources with the uids `prometheus`, `loki` and `pyroscope`. It narrows a bottleneck down in order, one row each:

1. **Overview**: open tunnels, active sessions, frames sent, frame build p95, ack p95, browser lag p95.
2. **Server frame pipeline (molcajeted)**: frame build time, CPU per flush phase, image and video encode times, encoding throughput and compression, bytes and images per frame, socket write waits, bytes received from the remote server and their decoding time, and the copies scroll and move detection found.
3. **Client and network**: the server's ack against round trip, frame arrival and browser lag; browser fps and backlog; browser lag and frame arrival by kind of client; the apply times clients report to molcajeted; and *Clients over the time range*.
4. **Gateway**: relayed traffic, handshake time, closed tunnels by status.
5. **CPU profiles (Pyroscope)**: molcajeted and gateway flamegraphs.
6. **Session logs (Loki)**: `display summary` and `client report` lines, warnings and errors, and a *Sessions* table that puts each tunnel's browser lag and frame arrival from its client reports next to the ack from molcajeted's `display summary` (logged when the session ends).

The `Protocol`, `Browser` and `Device` variables filter the panels. `Browser` and `Device` apply to the browser metrics only; molcajeted's own metrics, such as the ack, cover every client together.
