# Deployment and observability

How to run molcajete in containers, and how to find bottlenecks from the logs, metrics and CPU profiles collected during real use.

## Layout

```text
browser
  -> https://molcajete.example.com      public HTTPS (reverse proxy, deploy/nas-nginx.conf)
  -> https://molcajete.<tailnet>.ts.net Tailnet-only ingress (deploy/k8s/ingress.yaml)
  -> gateway:8080                       web UI, /api, /websocket-tunnel, login sessions
  -> molcajeted:4822 (same pod)         VNC / RDP / SSH -> remote desktops
```

| Container | Image target | Ports |
| --- | --- | --- |
| gateway | `gateway` | `8080` HTTP, `9100` telemetry |
| molcajeted | `daemon` (includes ffmpeg) | `4822` Guacamole protocol, `9100` (`9101` in Kubernetes) telemetry |

The telemetry ports are separate from the service ports, so they are never exposed through the public reverse proxy; only an in-cluster collector (Grafana Alloy) reaches them.

## Images

```bash
docker build --target gateway -t molcajete-gateway:<tag> .
docker build --target daemon  -t molcajeted:<tag> .
```

- The default build profile is `profiling` (release + `debug = "line-tables-only"`), so CPU profiles resolve even inlined functions to source lines. Use `--build-arg CARGO_PROFILE=release` to change it.
- The wasm client is built with `scripts/build-web.sh` while building the gateway image and lands in `/usr/share/molcajete/web`.
- Both images run as non-root (uid 10001) with `tini` as PID 1, and both processes shut down cleanly on SIGTERM.

To run them on a cluster, push them to the registry its nodes pull from:

```bash
docker tag molcajete-gateway:<tag> registry.example.com/molcajete-gateway:<tag>
docker push registry.example.com/molcajete-gateway:<tag>
docker tag molcajeted:<tag> registry.example.com/molcajeted:<tag>
docker push registry.example.com/molcajeted:<tag>
```

## Running

### Docker Compose (single host)

```bash
cp deploy/gateway.toml deploy/gateway.local.toml   # edit users and connections
docker compose -f deploy/compose.yml up -d --build
open http://127.0.0.1:8080
curl -s http://127.0.0.1:9100/metrics | head      # gateway
curl -s http://127.0.0.1:9101/metrics | head      # molcajeted
```

### Kubernetes

`deploy/k8s/` is a kustomize template to copy into a GitOps repository.

1. Write the real configuration from `deploy/gateway.toml`, with `[guacd] host = "127.0.0.1"` (same pod).
2. The configuration holds passwords, so store it as a Secret, for example a SealedSecret:

   ```bash
   kubectl create secret generic molcajete-config -n molcajete \
     --from-file=gateway.toml=./gateway.toml --dry-run=client -o yaml \
     | kubeseal -o yaml > sealed-secret.yaml
   ```

3. Uncomment `sealed-secret.yaml` in `kustomization.yaml` and set the image names and tags.
4. With Argo CD, add the Application and allow the `molcajete` namespace in its AppProject.
5. Users and connections live in a database. Either set `MOLCAJETE_DATABASE_URL` (PostgreSQL or MySQL; keep the URL in a Secret) or mount a PersistentVolume at `/var/lib/molcajete` for the default SQLite file. Without either, everything created in the web UI is lost when the pod restarts.

If the remote desktops are on the LAN, check first that pods can reach LAN addresses. On a single-node cluster on a Mac (OrbStack), `host.orb.internal` is the Mac itself, and its port 5900 is macOS Screen Sharing, not a test VNC server.

### Reverse proxy

`deploy/nas-nginx.conf` and `deploy/nas-nginx.upstream.inc` are written for an nginx (here on a NAS) in front of a Tailnet-only ingress. Copy both into nginx's `conf.d/` and fill in the domain, the Tailnet IP and the host name.

- `/websocket-tunnel` passes the WebSocket upgrade headers, disables buffering and allows 1 hour of idleness.
- `X-Real-IP` and `X-Forwarded-For` are overwritten with `$remote_addr`, not appended to. The Tailscale ingress in between replaces `X-Forwarded-For` with its own peer address, so the gateway logs `X-Real-IP` first.
- If nginx runs in Docker with published ports, `$remote_addr` may be the Docker bridge gateway for every client; use host networking to see real client addresses.
- Authentication is the gateway's login page: users and their Argon2id password hashes are in the database, sessions are `HttpOnly`, `SameSite=Strict` cookies (`Secure` behind `X-Forwarded-Proto: https`). Once public, the remote desktops depend on those passwords: use long ones, and restrict client addresses at the proxy or put single sign-on in front.
- The proxy must pass `Host` (or `X-Forwarded-Host`) through unchanged: the gateway refuses state-changing requests and tunnels whose `Origin` does not match it.

## Observability options

Everything is off by default and costs nothing unless enabled (metrics calls are no-ops without a recorder).

| Option | Environment variable | Applies to | Description |
| --- | --- | --- | --- |
| `--log-format json` | `MOLCAJETE_LOG_FORMAT` | both | One JSON object per line, including the current span's fields (`tunnel_id`, `connection`, ...) |
| `--telemetry-listen ADDR` | `MOLCAJETE_TELEMETRY_LISTEN` | both | Separate listener for `/metrics` and `/healthz` |
| `--profiling` | `MOLCAJETE_PROFILING` | both | Adds `/debug/pprof/profile` and `/debug/pprof/flamegraph` |
| `--client-reports` | `MOLCAJETE_CLIENT_REPORTS` | gateway | Accepts browser performance reports (`POST /api/telemetry/client`) |
| `RUST_LOG` | | both | Log level filter (default `info`) |

The images default to `MOLCAJETE_LOG_FORMAT=json`. The gateway also reads `MOLCAJETE_CONFIG`, `MOLCAJETE_LISTEN` and `MOLCAJETE_STATIC_DIR`; molcajeted reads `MOLCAJETED_BIND_HOST`, `MOLCAJETED_BIND_PORT`, `MOLCAJETED_VIDEO` and `MOLCAJETED_FFMPEG`.

### Profiling

pprof-rs samples with SIGPROF (99 Hz by default), only while a request is in progress, and serves one profile at a time (concurrent requests get 409).
The first profile loads the binary's line tables to symbolize stacks, which adds about 150 MiB to the process (the gateway otherwise needs a few MiB). Samples are buffered in a temporary file, so `/tmp` must be writable. An idle process produces empty profiles, which Pyroscope does not store.

```bash
# flamegraph to look at directly
curl -o gw.svg 'http://127.0.0.1:9100/debug/pprof/flamegraph?seconds=30'
# pprof format (go tool pprof, Pyroscope)
curl -o daemon.pb 'http://127.0.0.1:9101/debug/pprof/profile?seconds=30&frequency=199'
go tool pprof -http=:0 daemon.pb
```

For continuous profiling, Alloy's `pyroscope.scrape` calls the endpoint above every 15 seconds and pushes the result to Pyroscope (`deploy/observability/molcajete.alloy`). Profiles accumulate while the system is in use, so you can pick a time range, such as the ten minutes when last night's RDP session was slow, and compare flamegraphs.

## What is measured

A frame goes through the following path, and every stage has metrics:

```text
remote server -> molcajeted: diff / scroll and move detection / PNG, JPEG, video encoding -> socket write
              -> gateway relay -> WebSocket -> browser: parsing / decoding / GPU drawing -> sync ack
```

### molcajeted

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `molcajeted_frame_flush_seconds` | histogram | protocol | CPU time to diff and encode one frame |
| `molcajeted_flush_phase_seconds` | histogram | protocol, phase | Time per flush phase: `video`, `scroll`, `moves`, `pixels` (includes image encoding), `refine` |
| `molcajeted_image_encode_seconds` | histogram | protocol, format | Encoding one image (`png`, `jpeg`) |
| `molcajeted_image_bytes_total`, `molcajeted_image_pixels_total` | counter | protocol, format | Encoded size and pixels processed (compression efficiency) |
| `molcajeted_video_encode_seconds` | histogram | codec, key | Encoding one video frame (including the ffmpeg round trip) |
| `molcajeted_video_frame_bytes` | histogram | codec, key | Video frame size |
| `molcajeted_video_streams_total`, `molcajeted_video_failures_total` | counter | codec | Video mode starts and encoder failures |
| `molcajeted_frames_total` | counter | protocol | Frames (`sync`) sent |
| `molcajeted_frame_encoded_bytes` | histogram | protocol | Encoded image bytes per frame |
| `molcajeted_client_ack_seconds` | histogram | protocol | From sending a frame to the client's ack: network round trip + browser processing |
| `molcajeted_unacked_frames` | histogram | protocol | Frames still unacknowledged when a frame is sent |
| `molcajeted_socket_write_seconds` | histogram | protocol | Time blocked in `write_all`; grows when the gateway or browser cannot keep up (backpressure) |
| `molcajeted_sent_bytes_total` | counter | protocol | Bytes sent to the gateway |
| `molcajeted_connections_active`, `molcajeted_connections_total` | gauge, counter | protocol, result | Connections and how they ended |
| `molcajeted_session_duration_seconds` | histogram | protocol | Session length |

### gateway

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `molcajete_gateway_handshake_seconds` | histogram | connection, protocol | guacd/molcajeted handshake (including connecting to the remote server) |
| `molcajete_gateway_handshake_failures_total` | counter | connection, protocol | Failed handshakes |
| `molcajete_gateway_tunnels_active` | gauge | connection, protocol | Open tunnels |
| `molcajete_gateway_tunnels_total` | counter | connection, protocol, status | Closed tunnels by Guacamole status code |
| `molcajete_gateway_tunnel_duration_seconds` | histogram | connection, protocol | Tunnel length |
| `molcajete_gateway_relayed_bytes_total` | counter | connection, protocol, direction | Bytes relayed (`to_browser`, `to_guacd`) |

### Browser (gateway `--client-reports`)

The page batches the client's once-per-second `stats` and posts them every 10 seconds. The only labels are the configured connection id and `mode` (`worker`/`main`), so cardinality stays bounded.

| Metric | Meaning |
| --- | --- |
| `molcajete_client_frame_lag_seconds` | From receiving a frame to drawing it (decoding + GPU) |
| `molcajete_client_rtt_seconds` | Tunnel ping round trip |
| `molcajete_client_frames_fps` | Frames applied per second |
| `molcajete_client_queued_ops` | Operations waiting to be drawn |
| `molcajete_client_receive_rate_bytes` | Bytes received per second |

## Logs

Main events of the JSON logs. In Loki, extract the fields with `| json`.

| message | Source | Main fields |
| --- | --- | --- |
| `tunnel established` / `tunnel closed` | gateway | `tunnel_id`, `connection`, `session`, `peer`, `guacd_id`, `handshake_ms`, `duration_s`, `bytes_to_browser`, `status` |
| `client report` | gateway | `session`, `fps_avg`, `fps_min`, `frame_lag_ms_avg/max`, `rtt_ms_avg`, `queued_ops_max`, `recv_bytes_per_s`, `display_width/height` |
| `connection established` / `connection closed` | molcajeted | `id`, `protocol`, `peer`, `width`, `height`, `image`, `video`, `duration_s` |
| `display summary` | molcajeted | Whole-session `frames`, `fps_avg`, `flush_ms_avg/max`, `client_ack_ms_avg/max`, `png_*`, `jpeg_*`, `video_*` |
| `video mode started` | molcajeted | `codec`, area, `kbps` |

Keys to follow one session end to end:

```text
browser session (client report.session)
  = gateway tunnel.session  ->  gateway tunnel_id, guacd_id
  = molcajeted connection.id (= guacd_id)
```

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

# p95 from send to ack vs the browser's own processing: the gap is roughly the network
histogram_quantile(0.95, sum by (le) (rate(molcajeted_client_ack_seconds_bucket[5m])))
histogram_quantile(0.95, sum by (le) (rate(molcajete_client_frame_lag_seconds_bucket[5m])))

# socket write waits (backpressure)
histogram_quantile(0.99, sum by (le) (rate(molcajeted_socket_write_seconds_bucket[5m])))
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
| High `frame_flush` p95 | Server CPU | Narrow down with `flush_phase`, then look at the molcajeted CPU flamegraph for that time range |
| High `phase="pixels"` and `image_encode{format="png"}` | PNG encoding | Compare bytes per pixel with encoding time and tune the compression level or the JPEG conditions |
| `video_encode` above the frame interval (33 ms) | Video encoder | Compare codecs, and check whether a hardware encoder is available |
| High `client_ack` but low `client_frame_lag` | Network or proxy | Check `client_rtt`, proxy buffering and `socket_write` waits |
| High `client_frame_lag` and `client_queued_ops` | Browser decoding or GPU | Compare `mode=main` with `worker`, and profile with the browser's DevTools Performance panel |
| Long `socket_write` waits | Gateway or beyond | Look at the gateway flamegraph and the rate of `relayed_bytes` |

## Grafana stack requirements

- **Logs**: nothing extra when a collector already ships pod stdout to Loki.
- **Metrics**: a Prometheus-compatible store (Prometheus with the remote-write receiver enabled, or Mimir) and a Grafana datasource.
- **Profiles**: Pyroscope and a Grafana datasource.
- Add `deploy/observability/molcajete.alloy` to the existing Alloy configuration. It finds targets by the pod label `app.kubernetes.io/name=molcajete` and ports named `telemetry`.
- Dashboard: `deploy/observability/grafana-dashboard.json` (`Molcajete Performance`; datasource uids `prometheus`, `loki`, `pyroscope`). It narrows bottlenecks down in order: overview → server frame pipeline → client and network → gateway → CPU flamegraphs → session logs.
