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

### molcajeted on macOS (hardware video encoding)

Containers on a Mac (OrbStack, Docker Desktop) run in a Linux VM without access to the Mac's media engine, so molcajeted in a container encodes video in software. Run natively on macOS, molcajeted uses VideoToolbox for H.265 and H.264 (there is no AV1 hardware encoder; AV1 still uses SVT-AV1 when Homebrew's ffmpeg has it). The gateway can stay in the cluster:

```text
gateway (pod) -> host.orb.internal:4822 -> molcajeted (launchd agent, 127.0.0.1:4822 on the Mac)
```

1. Build and install the binary (the `profiling` profile keeps line tables for CPU profiles):

   ```bash
   cargo build --profile profiling -p molcajete-daemon
   install -m 755 target/profiling/molcajeted ~/.local/bin/molcajeted
   brew install ffmpeg   # software AV1 / fallback encoders
   ```

2. Copy `deploy/macos/io.molcajete.molcajeted.plist` to `~/Library/LaunchAgents/`, replace `/Users/USER`, and start it:

   ```bash
   mkdir -p ~/Library/Logs/molcajete
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.molcajete.molcajeted.plist
   grep '"video encoders"' ~/Library/Logs/molcajete/molcajeted.log | tail -1
   # H.265: VideoToolbox (hardware), then ffmpeg libx265; H.264: VideoToolbox (hardware), ...
   ```

   After replacing the binary: `launchctl kickstart -k gui/$(id -u)/io.molcajete.molcajeted`.

   The plist sets `MOLCAJETED_VIDEO=h265,h264,av1`: with the default order, areas small enough for software AV1 would be encoded on the CPU instead of by VideoToolbox.

3. Point the gateway at the Mac: `[guacd] host = "host.orb.internal"` and remove the molcajeted container from the pod.

- molcajeted listens on loopback only, because the Guacamole protocol has no authentication. OrbStack forwards `host.orb.internal` to the Mac's loopback, so pods still reach it, and so does an in-cluster collector for the telemetry port (`host.orb.internal:9101`).
- A launchd agent runs only while the user is logged in. So does OrbStack, so the cluster and the daemon start and stop together.
- Remote desktop addresses are now resolved on the Mac, not in the pod: cluster DNS names no longer work, while LAN and Tailnet addresses do.
- launchd does not rotate `StandardOutPath`. The log only grows by a few lines per session, but truncate it now and then or add an entry in `/etc/newsyslog.d/`.

### Reverse proxy

`deploy/nas-nginx.conf` and `deploy/nas-nginx.upstream.inc` are written for an nginx (here on a NAS) in front of a Tailnet-only ingress. Copy both into nginx's `conf.d/` and fill in the domain, the Tailnet IP and the host name.

- `/websocket-tunnel` passes the WebSocket upgrade headers, disables buffering and allows 1 hour of idleness.
- The client offers two WebSocket subprotocols (`Sec-WebSocket-Protocol: molcajete-binary-v1, guacamole`) and the gateway answers with one. A proxy must pass that header in both directions, as nginx does by default; one that drops it from the request leaves the tunnel on the text protocol, one that drops it from the response makes the browser refuse the connection. With `molcajete-binary-v1` most messages are binary WebSocket frames, which nginx forwards like text ones. Do not let a proxy negotiate `permessage-deflate` on this path: the payload is already compressed images and video.
- `X-Real-IP` and `X-Forwarded-For` are overwritten with `$remote_addr`, not appended to. The Tailscale ingress in between replaces `X-Forwarded-For` with its own peer address, so the gateway logs `X-Real-IP` first.
- If nginx runs in Docker with published ports, `$remote_addr` may be the Docker bridge gateway for every client; use host networking to see real client addresses.
- Authentication is the gateway's login page: users and their Argon2id password hashes are in the database, sessions are `HttpOnly`, `SameSite=Strict` cookies (`Secure` behind `X-Forwarded-Proto: https`). Once public, the remote desktops depend on those passwords: use long ones, and restrict client addresses at the proxy or sign in through your identity provider instead ([single sign-on](#single-sign-on-openid-connect)).
- The proxy must pass `Host` (or `X-Forwarded-Host`) through unchanged: the gateway refuses state-changing requests and tunnels whose `Origin` does not match it.

### Single sign-on (OpenID Connect)

With an `[oidc]` section the login page offers *Sign in with …* next to (or instead of) the password form. The gateway runs the authorization code flow with PKCE against any OpenID Connect provider (Keycloak, Authentik, Okta, Google Workspace, Entra ID) and then issues the same `molcajete_session` cookie as a password login.

```toml
[oidc]
issuer = "https://sso.example.com/realms/example"   # exactly the ID token's iss
client_id = "molcajete"
client_secret_file = "/run/secrets/oidc-client-secret"
redirect_url = "https://molcajete.example.com/api/auth/oidc/callback"
display_name = "Example SSO"
admin_groups = ["molcajete-admins"]
post_logout_redirect_url = "https://molcajete.example.com/"
```

| Key | Default | |
| --- | --- | --- |
| `issuer` | | Discovery reads `<issuer>/.well-known/openid-configuration`; the value must match the provider's `iss` exactly (mind trailing slashes). |
| `client_id` | | |
| `client_secret`, `client_secret_file` | none | Or the `MOLCAJETE_OIDC_CLIENT_SECRET` environment variable, which wins. Without any, the gateway is a public client (PKCE only). |
| `redirect_url` | | The browser-visible URL of `/api/auth/oidc/callback`; register it at the provider. |
| `scopes` | `["profile", "email"]` | Requested besides `openid`. Add the scope that releases groups if the provider needs one. |
| `username_claim` | `preferred_username` | Claim holding the molcajete username. A dotted path reads nested claims. |
| `groups_claim` | `groups` | Claim holding a list of group names (dotted paths work, e.g. `realm_access.roles`). |
| `display_name` | `single sign-on` | The button says *Sign in with &lt;display_name&gt;*. |
| `admin_groups` | `[]` | Members of any of these provider groups get the `admin` role. |
| `connection_manager_groups` | `[]` | Members of any of these (and none of `admin_groups`) get the `connection_manager` role. |
| `sync_role` | `true` | With either list, the user's own role is set from them on every sign-in, down to `user` for someone who left them. `false` only ever raises it. |
| `sync_groups` | `false` | Make the user a member of exactly the molcajete user groups named in `groups_claim` on every sign-in. |
| `create_groups` | `false` | With `sync_groups`, create a group (role `user`, no grants) for a provider group that has none here; otherwise such names are skipped. |
| `group_prefix` | none | With `sync_groups`, sync only provider groups whose names start with this (names are kept as they are). |
| `auto_create_users` | `true` | Create users on their first sign-in, with no connections. `false` lets in only users an administrator created first (matched by username). |
| `password_login` | `true` | `false` hides the password form and refuses `POST /api/auth/login`. |
| `post_logout_redirect_url` | none | Where the provider sends the browser after signing out there; register it at the provider. |

How it behaves:

- The ID token's signature, issuer, audience, expiry, nonce and access token hash are checked. Claims missing from the ID token are read from the userinfo endpoint. Discovery and signing keys are cached for ten minutes and fetched again when a signature does not verify.
- The `state`, `nonce` and PKCE verifier of a sign-in in progress are kept in the database (`molcajete_oidc_login`, ten minutes, used once) and the state also in an `HttpOnly`, `SameSite=Lax` cookie (`molcajete_oidc_state`), so a callback is accepted once and only in the browser that started it. Several gateway replicas can share the work as long as they share the database.
- Users are linked to the provider's `iss` and `sub` (`molcajete_user_identity`), so a user renamed at the provider keeps their account; the local username follows when the new one is free. On the first sign-in an existing user with the same username is linked, unless already linked to someone else at the provider. Use a username claim people cannot change themselves at the provider: whoever arrives with the name `admin` is linked to the local `admin` account.
- Users created on sign-in have no password and cannot use the password form; an administrator may still set one in *Users*. With single sign-on configured, administrators may also create users without a password. Disabled users are refused, whichever way they sign in.
- Signing out of a session opened through the provider also signs out there, when the provider advertises an `end_session_endpoint` (RP-initiated logout with `id_token_hint`).
- Failed sign-ins return to the login page with a short message (`?login_error=<code>`); the reason is in the gateway log (`single sign-on failed`).
- **Break-glass** with `password_login = false`: set `password_login = true` (or remove the line), restart, run `set-password <name>` (see the README) and sign in with that password; then turn it off again.
- Roles and groups from the provider, on every sign-in:
  - `admin_groups` and `connection_manager_groups` set the user's own role. A user's effective role is still the highest of their own and their groups' roles, so with `sync_groups` a molcajete group with the `admin` role is another way to make administrators.
  - `sync_groups` matches provider group names to molcajete user groups exactly and removes memberships in groups the claim does not name, including ones an administrator added by hand: with it on, the provider decides every membership. Give the groups (or the ones `create_groups` made) their connections, folders and role in *Groups*.
  - Neither may leave no enabled administrator. When a sign-in would, that change is skipped with a warning in the log and the user still signs in; demote the last administrator by hand after making someone else one.

Provider notes:

- **Keycloak**: issuer `https://<host>/realms/<realm>`. Create a confidential OpenID Connect client with the standard flow, the redirect URL and *Valid post logout redirect URIs*. For groups add a *Group Membership* mapper with token claim name `groups`, *Full group path* off and *Add to ID token* on. `docker/keycloak/compose.yml` is a ready test realm (gateway config in `docker/keycloak/gateway.keycloak.toml`).
- **Authentik**: create an OAuth2/OpenID provider and application; the issuer is `https://<host>/application/o/<application-slug>/`, including the trailing slash. The default `profile` scope mapping includes `groups` and `preferred_username`.
- **Entra ID**: register an application with a web redirect URI and a client secret; the issuer is `https://login.microsoftonline.com/<tenant-id>/v2.0`. `preferred_username` is the user principal name (`user@domain`). For groups, set *Token configuration → Add groups claim*; Entra sends group object ids unless the groups come from on-premises AD, so list those ids in `admin_groups` (and use them as group names with `sync_groups`).

### Binary tunnel and mixed versions

Our client, the gateway and molcajeted carry image, video and audio data as raw bytes instead of base64 when both ends of a leg support it ([architecture.md](architecture.md), section 2.6): about 25% fewer bytes between molcajeted, gateway and browser, and less work in the browser. Each leg is negotiated per tunnel and falls back to the Guacamole text protocol, so the three components can be upgraded in any order and old and new versions work together:

| Upgraded | Result |
| --- | --- |
| gateway only (older client and molcajeted) | text, as before |
| gateway and web client (they ship together), older molcajeted or stock guacd | binary to the browser; the gateway decodes the daemon's base64 (`framing="transcoded"`) |
| molcajeted only | text, as before |
| all three | binary end to end (`framing="binary"`) |

- The gateway logs `framing` in `tunnel established`; molcajeted logs `binary=true` in `connection established`.
- `[server] binary_tunnel = false` in the gateway configuration turns the extension off for every tunnel. A single session stays on text with `?text` in its URL.
- A browser tab opened before the upgrade keeps running the old client (text) until it is reloaded.
- `molcajete_gateway_relayed_bytes_total{direction="to_browser"}` counts what the gateway read from the daemon; with `framing="transcoded"` the browser receives about a quarter less than that.

### zstd images and mixed versions

Our web client decodes one more lossless image type itself, in WebAssembly, and molcajeted sends it instead of PNG to the clients that announce it ([architecture.md](architecture.md), section 2.7): `image/x-molcajete-zstd-v1` in the `image` instruction of the handshake, which the gateway passes on as it does every mimetype. Nothing else changes for anyone else:

| Web client | molcajeted | Lossless images |
| --- | --- | --- |
| new | new | zstd images; PNG for photographs, images with transparency and the cursor |
| new | older, or stock guacd | PNG (the daemon does not know the mimetype and ignores it) |
| older, or stock guacamole-common-js, or the stock web application | new | PNG (the client does not announce the mimetype) |

- The gateway needs no change and no new version for this: an older gateway relays the mimetype and the images, over the text and the binary tunnel alike. The web client ships with the gateway image, so in practice the client is upgraded by deploying the gateway, and a tab opened before that keeps PNG until it is reloaded.
- molcajeted logs the mimetypes a client announced as `image` in `connection established`, and counts the images under `format="zstd"`.
- `--image-zstd false` (`MOLCAJETED_IMAGE_ZSTD=false`) keeps every client of a daemon on PNG. A single session stays on PNG with `?png` in its URL.
- molcajeted now links libzstd, which the `zstd-sys` crate compiles from its bundled source: building needs a C compiler, as it already did for `ring`.

### Remote display changes and mixed versions

Resizing an RDP desktop to the window and choosing its scale ([architecture.md](architecture.md), section 2.8) touch all three components:

| Web client | Gateway | Daemon | Result |
| --- | --- | --- | --- |
| new | new | new molcajeted | everything: the form, the menu, choices remembered per browser, rows disabled for servers without Display Control |
| new | new | stock guacd | choices apply at the next connection (guacd does not apply `argv` for `dpi` and `resize-method` during a session); the rows stay enabled |
| new | older | any | the gateway ignores `param.*`, so a remembered choice applies only once it is changed in a session; the menu shows Automatic and off until a choice is made |
| older | any | new molcajeted | as before: the desktop resizes only with `resize-method=display-update` |

- Existing RDP connections keep what they have: without `resize-method` the desktop does not follow the window. Turn the switch on in the connection form, or in the session menu for one browser.
- molcajeted logs `Display Control availability` with `available=` once it knows, and the client's changes as `client changed the resize method` and `client changed the DPI`. The gateway logs the overrides a browser asked for in `opening tunnel`.

### Frame pacing by apply time and mixed versions

molcajeted asks in every `sync` how long the client took to apply the frame, and our web client answers it in its `sync` reply ([architecture.md](architecture.md), section 2.9). The interval between frames then follows that time instead of the whole round trip, which the window of unacknowledged frames already covers: a slow client, or one far away, gets more frames without its queue growing. Nothing is negotiated in the handshake and the gateway passes both directions through unchanged:

| Web client | Daemon | Frame pacing |
| --- | --- | --- |
| new | new molcajeted | by the reported apply time (median of the last 8 frames, forgotten after 1 s) |
| new | older molcajeted, or stock guacd | as before: neither asks, and the client sends the stock reply |
| older, or stock guacamole-common-js, or the stock web application | new molcajeted | as before, by the smoothed round trip: these clients ignore the extra `sync` argument and reply without a time |

- The gateway needs no change: an older gateway relays the extra arguments over the text and the binary tunnel alike. As with the other client changes, a tab opened before the gateway (and with it the web client) was upgraded keeps the old pacing until it is reloaded.
- `molcajeted_client_apply_seconds` records the reported times; it stays empty for sessions of clients that do not report them. `molcajeted_unacked_frames` must stay within the window (3 for RDP and VNC, 2 for SSH).

### Terminal copy, paste and links and mixed versions

The terminal shortcuts ([architecture.md](architecture.md), section 2.10) live in the web client; links and the cleared selection come from molcajeted.

| Web client | Daemon | Result |
| --- | --- | --- |
| new | new molcajeted (SSH) | everything: copy and paste shortcuts, toasts, links |
| new | stock guacd (SSH, telnet, Kubernetes) | copy and paste shortcuts and toasts; no links; after a click clears the selection, the copy shortcut still copies the previous selection |
| older | new molcajeted | as before; the underline and the hand pointer show, but links cannot be opened |

### Roles, groups and folders

Permissions are roles, user groups and connection folders ([README](../README.md#users-and-connections)). The gateway migrates the database on startup (`20261126000000_groups_roles_folders`):

- `molcajete_user.is_admin` becomes `molcajete_user.role`: administrators get `admin`, everyone else `user`. The `is_admin` column is dropped, so an older gateway cannot start on the migrated database; back up the database before upgrading if you may need to go back.
- Existing per-user grants (`molcajete_connection_permission`) are kept as they are; existing connections sit at the top level, outside any folder.
- New tables: `molcajete_group`, `molcajete_group_member`, `molcajete_connection_group` (folders, with `parent_id`), and grants of folders to users (`molcajete_connection_group_permission`) and of connections and folders to groups (`molcajete_group_connection_permission`, `molcajete_group_connection_group_permission`). `molcajete_connection.parent_id` puts a connection in a folder.

Finding a user's connections uses a recursive query (`WITH RECURSIVE`): PostgreSQL, SQLite 3.8.3+, MySQL 8.0+ or MariaDB 10.2+.

The JSON API keeps `isAdmin` on users (now derived from the role) and accepts it in user requests; scripts that use it keep working. `/api/users` and `/api/groups` (lists only) are open to connection managers, who need them to grant connections.

## Observability options

Everything is off by default and costs nothing unless enabled (metrics calls are no-ops without a recorder).

| Option | Environment variable | Applies to | Description |
| --- | --- | --- | --- |
| `--log-format json` | `MOLCAJETE_LOG_FORMAT` | both | One JSON object per line, including the current span's fields (`tunnel_id`, `connection`, ...) |
| `--telemetry-listen ADDR` | `MOLCAJETE_TELEMETRY_LISTEN` | both | Separate listener for `/metrics` and `/healthz` |
| `--profiling` | `MOLCAJETE_PROFILING` | both | Adds `/debug/pprof/profile` and `/debug/pprof/flamegraph` |
| `--client-reports` | `MOLCAJETE_CLIENT_REPORTS` | gateway | Accepts browser performance reports (`POST /api/telemetry/client`) |
| `RUST_LOG` | | both | Log level filter (default `info`) |

The images default to `MOLCAJETE_LOG_FORMAT=json`. The gateway also reads `MOLCAJETE_CONFIG`, `MOLCAJETE_LISTEN` and `MOLCAJETE_STATIC_DIR`; molcajeted reads `MOLCAJETED_BIND_HOST`, `MOLCAJETED_BIND_PORT`, `MOLCAJETED_VIDEO`, `MOLCAJETED_FFMPEG`, `MOLCAJETED_IMAGE_ZSTD` and `MOLCAJETED_ENCODE_THREADS` (threads that encode the images of a frame side by side, shared by all sessions: `0`, the default, for half the available cores and at most 8; `1` encodes on each session's own thread).

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
| `molcajeted_copies_total` | counter | protocol, phase | Copies found by `scroll` and by `moves` detection; compare with that phase's share of `molcajeted_flush_phase_seconds` to see what the detection costs per copy |
| `molcajeted_image_encode_seconds` | histogram | protocol, format | Encoding one image (`png`, `jpeg`, `zstd`: the zstd image type, see below). Images of a frame are encoded in parallel, so these add up to more than the `pixels` phase |
| `molcajeted_image_encoded_bytes` | histogram | protocol, format | Size of one encoded image: many small images cost the client a decode each |
| `molcajeted_image_bytes_total`, `molcajeted_image_pixels_total` | counter | protocol, format | Encoded size and pixels processed (compression efficiency) |
| `molcajeted_video_encode_seconds` | histogram | codec, key | Encoding one video frame (including the ffmpeg round trip) |
| `molcajeted_video_frame_bytes` | histogram | codec, key | Video frame size |
| `molcajeted_video_streams_total`, `molcajeted_video_failures_total` | counter | codec | Video mode starts and encoder failures |
| `molcajeted_frames_total` | counter | protocol | Frames (`sync`) sent |
| `molcajeted_frame_encoded_bytes` | histogram | protocol | Encoded image bytes per frame |
| `molcajeted_frame_images` | histogram | protocol | Images per frame (fills and copies not counted): the client decodes each, and the daemon encodes them side by side |
| `molcajeted_client_ack_seconds` | histogram | protocol | From sending a frame to the client's ack arriving: network round trip + the time the frame's data takes to arrive (`molcajete_client_frame_receive_seconds`) + browser processing |
| `molcajeted_client_ack_wait_seconds` | histogram | protocol | How long an ack then waited for the session loop (busy building a frame or handling server data); not part of `client_ack` |
| `molcajeted_client_apply_seconds` | histogram | protocol | How long the client says it took to apply a frame (decoding and drawing, without waiting behind earlier frames); only molcajete clients report it, and it sets the frame interval for them |
| `molcajeted_unacked_frames` | histogram | protocol | Frames still unacknowledged when a frame is sent |
| `molcajeted_socket_write_seconds` | histogram | protocol | Time blocked in `write_all`; grows when the gateway or browser cannot keep up (backpressure) |
| `molcajeted_sent_bytes_total` | counter | protocol | Bytes sent to the gateway |
| `molcajeted_remote_received_bytes_total` | counter | protocol | Bytes read from the remote server's socket (`rdp`: TLS records, handshake included; `vnc`) |
| `molcajeted_remote_decode_seconds` | histogram | protocol | `rdp` only: decoding one PDU from the server (IronRDP, bitmap decompression included) and copying its graphics updates into the display |
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

The page batches the client's once-per-second `stats` and posts them every 10 seconds. Every metric has four labels with a handful of values each, so cardinality stays bounded:

| Label | Values |
| --- | --- |
| `connection` | The configured connection id |
| `mode` | `worker` or `main`: where the client runs |
| `browser` | The browser engine, from the report's `User-Agent`: `chromium` (Chrome, Edge and other Chromium browsers), `firefox`, `safari` (Safari, and every browser on iOS and iPadOS), `other` |
| `device` | `desktop`, `tablet` or `mobile`, from the `User-Agent`; an iPad, whose browsers claim to be desktop Safari, is told from a Mac by its touch screen |

Engine and device are what decide how fast a page decodes and draws, and they stay the same for the whole session; the display size would split a session into several series whenever the window is resized. Two clients of the same kind still share their series: tell them apart in the logs, where each `client report` names its tunnel (below).

| Metric | Meaning |
| --- | --- |
| `molcajete_client_frame_lag_seconds` | From receiving a frame's `sync` to applying it and acknowledging: waiting for its images to decode and for the frames before it (average per second) |
| `molcajete_client_frame_receive_seconds` | From a frame's first instruction that draws on the screen (or into a buffer the frame then uses) to its `sync`: how long its data took to arrive and be parsed (average per second). A cursor change sent between frames does not start a frame |
| `molcajete_client_frame_present_seconds` | From applying a frame to having drawn the canvas with it: the wait for the next animation frame plus the composite (average per second; no sample for a second without a drawn frame). The browser shows the canvas at a later display refresh, which is not included |
| `molcajete_client_image_decode_seconds` | Decoding one image (average per second). For PNG, JPEG and WebP: from handing the data to the browser's decoder to the decoded image, waiting included. For zstd images: the decode itself, which runs while the frame is drawn |
| `molcajete_client_images_ops` | Images decoded per second |
| `molcajete_client_rtt_seconds` | Tunnel ping round trip |
| `molcajete_client_frames_fps` | Frames applied per second |
| `molcajete_client_queued_ops` | Operations waiting to be drawn |
| `molcajete_client_receive_rate_bytes` | Bytes received per second |

## Logs

Main events of the JSON logs. In Loki, extract the fields with `| json`.

| message | Source | Main fields |
| --- | --- | --- |
| `tunnel established` / `tunnel closed` | gateway | `tunnel_id`, `connection`, `session`, `peer`, `guacd_id`, `handshake_ms`, `duration_s`, `bytes_to_browser`, `status` |
| `client report` | gateway | `session`, `tunnel_id`, `guacd_id`, `browser`, `device`, `fps_avg`, `fps_min`, `frame_lag_ms_avg/max`, `frame_present_ms_avg/max`, `rtt_ms_avg`, `queued_ops_max`, `recv_bytes_per_s`, `display_width/height` |
| `connection established` / `connection closed` | molcajeted | `id`, `protocol`, `peer`, `width`, `height`, `image`, `video`, `duration_s` |
| `display summary` | molcajeted | `span.id` (the connection id, `span_id` after `\| json`), whole-session `frames`, `fps_avg`, `flush_ms_avg/max`, `frame_images_max`, `scroll_copies`, `move_copies`, `client_ack_ms_avg/max`, `png_*`, `jpeg_*`, `zstd_*`, `video_*` |
| `video mode started` | molcajeted | `codec`, area, `kbps` |

Keys to follow one session end to end:

```text
browser session (client report.session)
  = gateway tunnel.session  ->  gateway tunnel_id, guacd_id
  = molcajeted connection.id (= guacd_id)
```

A `client report` carries the `tunnel_id` and `guacd_id` of the tunnel its page opened (looked up by `session`; absent for a page that reported before the tunnel was established, or more than two minutes after it closed). Every molcajeted log line of a connection carries its id as `span.id`, so a client report and its session's `display summary` share `guacd_id` = `span_id`:

```logql
{namespace="molcajete", container="gateway"} | json | message="client report" | guacd_id="$abcd..."
{namespace="molcajete", container="molcajeted"} | json | message="display summary" | span_id="$abcd..."
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
| High `phase="pixels"` and `image_encode{format="png"}` | PNG encoding | Compare bytes per pixel with encoding time and tune the compression level or the JPEG conditions. A molcajete client should get `format="zstd"` for nearly all lossless images; if it gets PNG, check `image=` in `connection established` and `--image-zstd` |
| `video_encode` above the frame interval (16 ms at best for RDP and VNC) | Video encoder | Compare codecs, and check whether a hardware encoder is available |
| High `client_ack` but low `client_frame_lag` | Network or proxy | Check `client_rtt`, proxy buffering and `socket_write` waits |
| High `client_frame_lag` and `client_queued_ops` | Browser decoding or GPU | Compare `mode=main` with `worker`, and profile with the browser's DevTools Performance panel |
| Long `socket_write` waits | Gateway or beyond | Look at the gateway flamegraph and the rate of `relayed_bytes` |

## Grafana stack requirements

- **Logs**: nothing extra when a collector already ships pod stdout to Loki.
- **Metrics**: a Prometheus-compatible store (Prometheus with the remote-write receiver enabled, or Mimir) and a Grafana datasource.
- **Profiles**: Pyroscope and a Grafana datasource.
- Add `deploy/observability/molcajete.alloy` to the existing Alloy configuration. It finds targets by the pod label `app.kubernetes.io/name=molcajete` and ports named `telemetry`.
- Dashboard: `deploy/observability/grafana-dashboard.json` (`Molcajete Performance`; datasource uids `prometheus`, `loki`, `pyroscope`). It narrows bottlenecks down in order: overview → server frame pipeline → client and network → gateway → CPU flamegraphs → session logs. The `Browser` and `Device` variables select the kinds of client the browser panels show (molcajeted's own metrics, such as the ack, are for every client together); "Browser lag p95 by client" and "Clients over the time range" show each kind separately, and the "Sessions" table shows each tunnel's browser lag and arrival from its client reports next to the ack from molcajeted's `display summary` (logged when the session ends). The server pipeline also shows the bytes received from the remote server and their decoding time (`molcajeted_remote_received_bytes_total`, `molcajeted_remote_decode_seconds`), the copies scroll and move detection found (`molcajeted_copies_total`) and the images per frame (`molcajeted_frame_images`); the client row shows the apply times clients report (`molcajeted_client_apply_seconds`).
