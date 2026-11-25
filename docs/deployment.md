# Deployment

How to build, run, configure and upgrade molcajete. Managing users, groups, connections, single sign-on and recordings from the web UI is in [administration.md](administration.md); logs, metrics and profiles are in [observability.md](observability.md).

## Components

```text
browser
  -> reverse proxy (HTTPS)               deploy/nas-nginx.conf, optional
  -> gateway:8080                        web UI, /api, /websocket-tunnel, sign-in
  -> molcajeted:4822 or guacd:4822       Guacamole protocol
  -> remote desktops                     VNC, RDP, SSH (molcajeted); any guacd protocol (guacd)
```

- **molcajete-gateway** serves the web UI and the wasm client, keeps users, groups, connections, folders and recordings in a database, signs people in (passwords or OpenID Connect), and relays each session between a WebSocket and a Guacamole daemon. It writes session recordings itself.
- **molcajeted** is the Rust Guacamole daemon. It speaks VNC, RDP and SSH. The gateway can also use a stock `guacd` (Apache Guacamole 1.x) for any connection, for example for telnet or Kubernetes, and each connection can name its own daemon.

| Container | Image target | Ports |
| --- | --- | --- |
| gateway | `gateway` | `8080` HTTP, `9100` telemetry |
| molcajeted | `daemon` (includes ffmpeg) | `4822` Guacamole protocol, `9100` telemetry (`9101` in the Kubernetes example, where both containers share a pod) |

The Guacamole protocol has no authentication: keep port 4822 on loopback or on a private network that only the gateway reaches. The telemetry ports are separate from the service ports, so the reverse proxy never exposes them; only an in-cluster collector needs them.

## Building the images

```bash
docker build --target gateway -t molcajete-gateway:<tag> .
docker build --target daemon  -t molcajeted:<tag> .
```

- The default Cargo profile is `profiling`: the release build plus `debug = "line-tables-only"`, so CPU profiles resolve inlined functions to source lines. `--build-arg CARGO_PROFILE=release` builds slightly smaller images.
- The gateway image builds the wasm client and the React UI with `scripts/build-web.sh` and serves them from `/usr/share/molcajete/web`.
- The daemon image installs Debian's ffmpeg, which molcajeted runs as a child process for the libsvtav1, libx265 and libx264 encoders.
- Both images run as uid 10001 with `tini` as PID 1, set `MOLCAJETE_LOG_FORMAT=json`, and stop cleanly on SIGTERM.
- Building molcajeted needs a C compiler (for `ring` and the bundled libzstd). `--features openh264` adds a built-in H.264 encoder and needs a C++ compiler.

To run the images on a cluster, push them to a registry its nodes pull from:

```bash
docker tag molcajete-gateway:<tag> registry.example.com/molcajete-gateway:<tag>
docker push registry.example.com/molcajete-gateway:<tag>
docker tag molcajeted:<tag> registry.example.com/molcajeted:<tag>
docker push registry.example.com/molcajeted:<tag>
```

## Running

### Docker Compose (single host)

`deploy/compose.yml` runs the gateway and molcajeted as two services, with SQLite on a named volume and telemetry, profiling and browser reports turned on.

```bash
cp deploy/gateway.toml deploy/gateway.local.toml   # set the admin password and connections
docker compose -f deploy/compose.yml up -d --build
open http://127.0.0.1:8080
curl -s http://127.0.0.1:9100/metrics | head      # gateway
curl -s http://127.0.0.1:9101/metrics | head      # molcajeted
```

All ports are published on 127.0.0.1 only. Put a reverse proxy with TLS in front before exposing the gateway (see [Reverse proxy](#reverse-proxy)).

### Kubernetes

`deploy/k8s/` is a kustomize template to copy into a GitOps repository. Both containers run in one pod: the gateway reaches molcajeted on `127.0.0.1:4822`, and only the gateway's HTTP port leaves the pod through the Service.

1. Write the configuration from `deploy/gateway.toml`, with `[guacd] host = "127.0.0.1"`.
2. The configuration holds passwords, so store it as a Secret named `molcajete-config` with the key `gateway.toml`, for example as a SealedSecret:

   ```bash
   kubectl create secret generic molcajete-config -n molcajete \
     --from-file=gateway.toml=./gateway.toml --dry-run=client -o yaml \
     | kubeseal -o yaml > sealed-secret.yaml
   ```

3. Uncomment `sealed-secret.yaml` in `kustomization.yaml` and set the image names and tags in `deployment.yaml`.
4. Choose where state lives. Either set `MOLCAJETE_DATABASE_URL` to PostgreSQL or MySQL (keep the URL in a Secret), or mount a PersistentVolume at `/var/lib/molcajete` for the SQLite file. Session recordings also need a PersistentVolume (`[recordings] directory`, by default `/var/lib/molcajete/recordings`). The template mounts neither, so without one everything created in the web UI, and every recording, is lost when the pod restarts.
5. Give the gateway its [secret key](#secrets) for stored connection credentials: `[secrets] key = "…"` (from `openssl rand -base64 32`) in the same Secret as the configuration, or `MOLCAJETE_SECRET_KEY` from a Secret of its own. Keep a copy outside the cluster with the database backups. Without a key the gateway generates `/var/lib/molcajete/secret.key`, which only survives with a PersistentVolume there.
6. Pick an Ingress. `ingress.yaml` is a Tailscale ingress (`ingressClassName: tailscale`) that the reverse proxy example connects to; replace it with your own if needed. With Argo CD, add an Application and allow the `molcajete` namespace in its AppProject.

Notes on the template:

- The Deployment uses the `Recreate` strategy, so an old and a new gateway never share the database during an upgrade (see [Upgrading](#upgrading)).
- Root filesystems are read-only; a disk-backed `emptyDir` at `/tmp` holds ffmpeg's temporary files and the CPU profiler's samples.
- molcajeted listens on `127.0.0.1` (`MOLCAJETED_BIND_HOST`), so its probe uses the telemetry listener's `/healthz`; the gateway is probed on `/healthz` of its HTTP port.
- The memory limit of the gateway (512Mi) leaves room for the about 150 MiB that symbolizing a CPU profile takes; without profiling the gateway needs a few MiB. molcajeted's CPU limit (4 cores) bounds image and software video encoding; raise it if profiles show throttling.
- With `[oidc]`, the gateway calls the identity provider's discovery, token, keys and userinfo endpoints: the pod needs outbound HTTPS to the provider.
- If the remote desktops are on the LAN, check first that pods can reach LAN addresses.

### molcajeted natively on macOS (VideoToolbox)

Containers on a Mac (OrbStack, Docker Desktop) run in a Linux VM without access to the Mac's media engine, so molcajeted in a container encodes video in software. Run natively on macOS, molcajeted encodes H.265 and H.264 with VideoToolbox. There is no AV1 hardware encoder; AV1 still uses SVT-AV1 when Homebrew's ffmpeg has it. The gateway can stay in the cluster:

```text
gateway (pod) -> host.orb.internal:4822 -> molcajeted (launchd agent, 127.0.0.1:4822 on the Mac)
```

1. Build and install the binary:

   ```bash
   cargo build --profile profiling -p molcajete-daemon
   install -m 755 target/profiling/molcajeted ~/.local/bin/molcajeted
   brew install ffmpeg   # software AV1 and fallback encoders
   ```

2. Copy `deploy/macos/io.molcajete.molcajeted.plist` to `~/Library/LaunchAgents/`, replace `/Users/USER`, and start it:

   ```bash
   mkdir -p ~/Library/Logs/molcajete
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.molcajete.molcajeted.plist
   grep '"video encoders"' ~/Library/Logs/molcajete/molcajeted.log | tail -1
   # H.265: VideoToolbox (hardware), then ffmpeg libx265; H.264: VideoToolbox (hardware), ...
   ```

   After replacing the binary: `launchctl kickstart -k gui/$(id -u)/io.molcajete.molcajeted`.

3. Point the gateway at the Mac (`[guacd] host = "host.orb.internal"`) and remove the molcajeted container from the pod.

- The plist sets `MOLCAJETED_VIDEO=h265,h264,av1`. With the default order (`av1,h265,h264`), areas small enough for software AV1 would be encoded on the CPU instead of by VideoToolbox. It also sets `MOLCAJETED_FFMPEG=/opt/homebrew/bin/ffmpeg`, because launchd's `PATH` does not include Homebrew.
- molcajeted listens on loopback only. OrbStack forwards `host.orb.internal` to the Mac's loopback, so pods still reach it, and so does an in-cluster collector for the telemetry port (`host.orb.internal:9101`).
- A launchd agent runs only while the user is logged in. So does OrbStack, so the cluster and the daemon start and stop together.
- Remote desktop addresses are resolved on the Mac, not in the pod: cluster DNS names do not work, while LAN and Tailnet addresses do. On OrbStack, `host.orb.internal` is the Mac itself, and its port 5900 is macOS Screen Sharing.
- launchd does not rotate `StandardOutPath`. The log grows by a few lines per session; truncate it now and then or add an entry in `/etc/newsyslog.d/`.

### From source

For development, `cargo run -p molcajete-gateway -- --config gateway.toml` and `cargo run -p molcajete-daemon -- -l 4823` run both processes from the checkout; the README's quick start builds the web client first. `docker/compose.yml` starts test targets (stock guacd, VNC, SSH, xrdp) that `gateway.example.toml` points at, and `docker/keycloak/` a Keycloak test realm with a matching `gateway.keycloak.toml`.

## Reverse proxy

`deploy/nas-nginx.conf` and `deploy/nas-nginx.upstream.inc` are an nginx example in front of a Tailnet-only ingress. Copy both into nginx's `conf.d/` and fill in the domain, the upstream address and the TLS server name. What any proxy must do:

- **WebSocket.** `/websocket-tunnel` needs the `Upgrade` and `Connection` headers passed through, buffering off, and a long idle timeout (the example allows one hour; the client pings every few seconds, so this only ends dead tunnels).
- **Subprotocol.** The client offers two WebSocket subprotocols (`Sec-WebSocket-Protocol: molcajete-binary-v1, guacamole`) and the gateway answers with one. Pass that header in both directions, as nginx does by default. A proxy that drops it from the request leaves the tunnel on the text protocol; one that drops it from the response makes the browser refuse the connection. With `molcajete-binary-v1` most messages are binary WebSocket frames.
- **No compression on the tunnel.** Do not let a proxy negotiate `permessage-deflate` on `/websocket-tunnel`: the payload is already compressed images and video.
- **Host and Origin.** Pass `Host` (or `X-Forwarded-Host`) through unchanged. The gateway refuses state-changing API requests and tunnels whose `Origin` does not match it.
- **HTTPS.** Send `X-Forwarded-Proto: https`; the gateway then marks its session cookie `Secure`. Cookies are `HttpOnly` and `SameSite=Strict`.
- **Client addresses.** List the proxy that connects to the gateway in [`[server] trusted_proxies`](#server). The gateway takes the client's address from `X-Real-IP`, otherwise from `X-Forwarded-For`, **only when the TCP peer is one of those proxies**; from anyone else the headers are ignored, since a client could put any address there. The address goes into logs and the audit log (`203.0.113.9 (via 10.0.0.2)`) and is what [failed sign-ins are counted by](administration.md#repeated-failed-sign-ins). **Without `trusted_proxies`, every client behind the proxy has the proxy's address**: they share one limit of failed sign-ins, so a few typos in one place can make the gateway refuse everyone for a few minutes, and logs show only the proxy.
  - Set both headers at the edge proxy to `$remote_addr`, overwriting rather than appending, so clients cannot spoof them. Proxies in between (such as the Tailscale ingress) replace `X-Forwarded-For` but pass `X-Real-IP` through, and `X-Real-IP` wins.
  - `trusted_proxies` names the hop the gateway sees, not the edge: in Kubernetes that is the ingress controller's (or the Tailscale ingress proxy's) pod address, so list the cluster's pod range (`kubectl get pods -A -o wide` shows the addresses). From `X-Forwarded-For` the gateway takes the rightmost address that is not a trusted proxy, so list any proxies in between as well.
  - Anyone who can reach the gateway from a trusted range can claim any address; keep those ranges to the proxies and the cluster.
  - If nginx runs in Docker with published ports, `$remote_addr` may be the Docker bridge for every client; use host networking to see real addresses.
- **Single sign-on.** The `redirect_url` and `post_logout_redirect_url` in `[oidc]` are the browser-visible URLs behind the proxy.

Once the gateway is public, the remote desktops depend on its sign-in. Use long passwords, restrict client addresses at the proxy, or sign in through your identity provider and turn the password form off ([`[oidc]`](#oidc)). The example also rate-limits requests and connections per client address.

## Configuration reference

The gateway reads one TOML file (`--config`, default `gateway.toml`). Unknown sections and keys are errors, so a typo stops the gateway at startup instead of being ignored. `gateway.example.toml` (development, with the `docker/compose.yml` targets) and `deploy/gateway.toml` (containers) are commented starting points.

```toml
[server]
listen = "127.0.0.1:8080"

[guacd]
host = "molcajeted"
port = 4822

[database]
url = "postgres://molcajete:secret@db/molcajete"

[recordings]
max_age_days = 90

[audit]
retention_days = 365

[[users]]
username = "admin"
password = "change-me"

[[connections]]
id = "desktop"
name = "Desktop (VNC)"
protocol = "vnc"
[connections.parameters]
hostname = "192.168.0.10"
port = "5900"
password = "change-me"
```

### Gateway command line and environment

| Flag | Environment variable | Default | |
| --- | --- | --- | --- |
| `-c`, `--config PATH` | `MOLCAJETE_CONFIG` | `gateway.toml` | The configuration file. |
| `--listen ADDR` | `MOLCAJETE_LISTEN` | | Overrides `server.listen`. |
| `--static-dir DIR` | `MOLCAJETE_STATIC_DIR` | | Overrides `server.static_dir`. |
| `--data-dir DIR` | `MOLCAJETE_DATA_DIR` | | Overrides `server.data_dir`. |
| `--database-url URL` | `MOLCAJETE_DATABASE_URL` | | Overrides `database.url`; an empty value is ignored. |
| `--client-reports` | `MOLCAJETE_CLIENT_REPORTS` | off | Accept browser performance reports ([observability.md](observability.md#browser-reports)). |
| `--log-format text\|json` | `MOLCAJETE_LOG_FORMAT` | `text` | See [observability.md](observability.md#options). |
| `--telemetry-listen ADDR` | `MOLCAJETE_TELEMETRY_LISTEN` | off | `/metrics`, `/healthz` and profiles on a separate port. |
| `--profiling` | `MOLCAJETE_PROFILING` | off | CPU profiles on the telemetry listener. |
| | `MOLCAJETE_OIDC_CLIENT_SECRET` | | The `[oidc]` client secret; wins over the file. |
| | `MOLCAJETE_SECRET_KEY` | | The [secret key](#secrets) for stored connection credentials; wins over `[secrets]`. |
| | `RUST_LOG` | `info,tower_http=warn,pprof=warn` | Log filter. |

The gateway image sets `MOLCAJETE_CONFIG=/etc/molcajete/gateway.toml`, `MOLCAJETE_LISTEN=0.0.0.0:8080`, `MOLCAJETE_STATIC_DIR=/usr/share/molcajete/web` and `MOLCAJETE_DATA_DIR=/var/lib/molcajete`, so the file need not repeat them.

Two subcommands work on the database directly, without a running gateway. They open it from the same configuration (and apply pending migrations), so run them with the same `--database-url` or `--data-dir` as the server, for example with `kubectl exec` in the gateway container.

- `set-password <username>` recovers access. It reads the password (at least 8 characters) from stdin, and makes the account an enabled administrator, creating it if needed:

  ```bash
  echo 'new-password' | molcajete-gateway --config gateway.toml set-password admin
  ```

- `rotate-key` re-encrypts every stored connection credential with the current secret key ([Rotating the key](#rotating-the-key)).

### `[server]`

| Key | Default | |
| --- | --- | --- |
| `listen` | `127.0.0.1:8080` | Address of the HTTP listener (web UI, `/api`, `/websocket-tunnel`, `/healthz`). |
| `static_dir` | `web/dist` | The built web client. A warning is logged when it has no `index.html`. |
| `data_dir` | `.` | Holds the default SQLite database (`molcajete.db`) and the default recordings directory. Created if missing. |
| `binary_tunnel` | `true` | Offer the `molcajete-binary-v1` WebSocket subprotocol, which carries image, video and audio data as raw bytes. `false` keeps every tunnel on the Guacamole text protocol. |
| `trusted_proxies` | `[]` | Reverse proxies whose `X-Real-IP` and `X-Forwarded-For` name the client: addresses or CIDR ranges, such as `["10.42.0.0/16", "127.0.0.1"]`. Requests from other peers are attributed to the peer. See [Reverse proxy](#reverse-proxy). |

### `[guacd]`

The daemon the gateway connects to, molcajeted or stock guacd. A connection can override it (see [`[[connections]]`](#connections)).

| Key | Default | |
| --- | --- | --- |
| `host` | `127.0.0.1` | Host name or address. |
| `port` | `4822` | |
| `handshake_timeout` | `15` | Seconds to wait for the handshake, which includes the daemon connecting to the remote desktop. |

### `[database]`

| Key | Default | |
| --- | --- | --- |
| `url` | none | `postgres://` (or `postgresql://`), `mysql://` (or `mariadb://`) or `sqlite://`. Without one, SQLite at `<data_dir>/molcajete.db`. |
| `max_connections` | `10` | Size of the connection pool. |

The schema is created and migrated when the gateway starts ([Upgrading](#upgrading)). Access checks use a recursive query (`WITH RECURSIVE`), which needs PostgreSQL, SQLite 3.8.3+, MySQL 8.0+ or MariaDB 10.2+. Keep the URL, which holds a password, in `MOLCAJETE_DATABASE_URL` from a Secret rather than in the file if the file is shared.

### `[auth]`

| Key | Default | |
| --- | --- | --- |
| `session_ttl_hours` | `12` | Lifetime of a sign-in session (the `molcajete_session` cookie), at least 1. |

`[auth.throttle]` limits failed password sign-ins; how it behaves is in [administration.md](administration.md#repeated-failed-sign-ins). A limit of `0` turns it off.

| Key | Default | |
| --- | --- | --- |
| `address_failures` | `10` | Failed sign-ins from one client address (any username; an IPv6 address counts as its /64) within `address_window_secs`. The next attempt from there gets `429`. |
| `address_window_secs` | `300` | |
| `username_failures` | `20` | Failed sign-ins for one username, from any address, within `username_window_secs`. The next attempt for that username gets `429`, except from addresses the user signed in from recently. |
| `username_window_secs` | `900` | |
| `known_address_days` | `30` | How long an address the user signed in from stays exempt from the username limit. |
| `max_concurrent_verifications` | `0` | Password checks (Argon2) running at once; `0` is the number of CPUs. |
| `verification_wait_secs` | `10` | How long a sign-in waits for a free check before it gets `503`. |

The counters are kept in memory: each gateway replica counts on its own, and a restart forgets them. Client addresses are only right behind a proxy listed in [`[server] trusted_proxies`](#server).

```toml
[server]
trusted_proxies = ["10.42.0.0/16"]

[auth.throttle]
address_failures = 10
username_failures = 20
```

### `[secrets]`

The key that [stored connection credentials](administration.md#stored-credentials) (passwords, private keys, passphrases) are encrypted with in the database. A key is 32 random bytes written as base64; make one with `openssl rand -base64 32`.

| Key | Default | |
| --- | --- | --- |
| `key_file` | none | File holding the key. A warning is logged when other users can read it. |
| `key` | none | The key itself; set `key` or `key_file`, not both. |
| `previous_key_files`, `previous_keys` | `[]` | Retired keys that still decrypt values encrypted before a [rotation](#rotating-the-key). |

```toml
[secrets]
key_file = "/run/secrets/molcajete-secret-key"
```

The gateway takes the key from `MOLCAJETE_SECRET_KEY` first, then `key_file`, then `key`. **Without any, it uses `<data_dir>/secret.key`**, and on the first start generates that file (mode 0600) and logs `generated a new secret key …; back it up together with the database`. That suits the single-host setup, where the key sits next to the SQLite file; with PostgreSQL or MySQL, or in Kubernetes, configure the key explicitly.

**Back the key up along with the database, but store it separately** (a password manager or secrets store): a database restored without its key has lost its stored credentials, and a key kept in the same backup as the database protects nothing. Losing the key loses the stored credentials, not the connections: they have to be typed again.

Starting without the right key fails with a message that names the key the stored values need, rather than sending wrong credentials to remote servers:

- `the database holds encrypted connection credentials but no secret key is configured and <data_dir>/secret.key does not exist`: the database was restored or moved without its key. Restore the key file, or configure the key.
- `a stored credential is encrypted with key <id>, which is not configured`: a different key is configured. The id is derived from the key and shown at every start (`secret key for stored credentials loaded key=<id>`).

#### Rotating the key

1. Make a new key and configure it as the key, and the old one as a previous key:

   ```toml
   [secrets]
   key_file = "/run/secrets/molcajete-secret-key-2"
   previous_key_files = ["/run/secrets/molcajete-secret-key"]
   ```

2. Restart the gateway (every replica). It now encrypts new values with the new key and still reads old ones.
3. Run `molcajete-gateway rotate-key` with that configuration. It re-encrypts every stored credential with the new key, in one transaction, and prints how many it changed.
4. Remove the previous key from the configuration and restart. Back up the new key with the database.

With the generated `<data_dir>/secret.key`, rename it (to `secret.key.old`, say), write the new key to `secret.key` and list the old file in `previous_key_files`.

### `[oidc]`

Sign-in through an OpenID Connect provider (authorization code flow with PKCE). Without this section only passwords are used. How sign-in, account linking and role and group sync behave, and notes for Keycloak, Authentik and Entra ID, are in [administration.md](administration.md).

```toml
[oidc]
issuer = "https://sso.example.com/realms/example"
client_id = "molcajete"
client_secret_file = "/run/secrets/oidc-client-secret"
redirect_url = "https://molcajete.example.com/api/auth/oidc/callback"
display_name = "Example SSO"
admin_groups = ["molcajete-admins"]
post_logout_redirect_url = "https://molcajete.example.com/"
```

| Key | Default | |
| --- | --- | --- |
| `issuer` | required | Discovery reads `<issuer>/.well-known/openid-configuration`. Must match the ID token's `iss` exactly, trailing slash included. |
| `client_id` | required | |
| `client_secret` | none | For confidential clients. |
| `client_secret_file` | none | Reads the secret from a file (trimmed). Set this or `client_secret`, not both. `MOLCAJETE_OIDC_CLIENT_SECRET` wins over both. Without any secret the gateway is a public client (PKCE only). |
| `redirect_url` | required | The browser-visible URL of `/api/auth/oidc/callback`; register it with the provider. A warning is logged when it does not end with that path. |
| `scopes` | `["profile", "email"]` | Requested besides `openid`. Add the scope that releases groups if the provider needs one. |
| `username_claim` | `preferred_username` | Claim holding the molcajete username; a dotted path reads a nested claim. Must not be empty. |
| `groups_claim` | `groups` | Claim holding a list of group names; dotted paths work (`realm_access.roles`). |
| `display_name` | `single sign-on` | The button reads *Sign in with &lt;display_name&gt;*. |
| `admin_groups` | `[]` | Members of any of these provider groups get the `admin` role. |
| `connection_manager_groups` | `[]` | Members of any of these, and of none of `admin_groups`, get the `connection_manager` role. |
| `sync_role` | `true` | With either list, set the user's own role from them on every sign-in, down to `user` for someone who left them. `false` only ever raises it. |
| `sync_groups` | `false` | Make the user a member of exactly the molcajete user groups named in `groups_claim` on every sign-in; other memberships are removed. |
| `create_groups` | `false` | With `sync_groups`, create a group (role `user`, no grants) for a provider group that has none; otherwise such names are skipped. |
| `group_prefix` | none | With `sync_groups`, sync only provider groups whose names start with this. Names are kept as they are. |
| `auto_create_users` | `true` | Create users on their first sign-in. `false` admits only users an administrator created beforehand, matched by username. |
| `password_login` | `true` | `false` hides the password form and refuses `POST /api/auth/login`. To get back in when the provider is unavailable, set it to `true`, restart, and use `set-password`. |
| `post_logout_redirect_url` | none | Where the provider sends the browser after signing out there; register it with the provider. |

Discovery happens at the first sign-in, not at startup, so a provider that is down does not stop the gateway. Sign-ins in progress are kept in the database, so several gateway replicas that share a database can share them. Cookies set during single sign-on are `Secure` when `redirect_url` is HTTPS.

### `[recordings]`

Where and how long the sessions of connections with *Record sessions* on are kept. Turning recording on for a connection, and playing recordings back, is in [administration.md](administration.md).

```toml
[recordings]
directory = "/var/lib/molcajete/recordings"
max_age_days = 90
max_total_size = "200 GiB"
```

| Key | Default | |
| --- | --- | --- |
| `enabled` | `true` | `false` records nothing, whatever the connections say. The web UI warns about it in the connection form. |
| `directory` | `<data_dir>/recordings` | Where files are written, as `YYYYMMDD-HHMMSS_<connection>_<id>.guac` (UTC start time). |
| `max_age_days` | `0` | Delete recordings that ended more than this many days ago. `0` keeps them. |
| `max_total_size` | `0` | When all recordings together exceed this, delete the oldest. Bytes, or a string with a unit: `B`, `KB`/`KiB`, `MB`/`MiB`, `GB`/`GiB`, `TB`/`TiB` (`"500 MB"`, `"1.5 TiB"`). `0` sets no limit. |

- **Retention** runs at startup and every ten minutes. Sessions still being recorded are never deleted. With neither limit set, nothing is deleted.
- **Storage.** In Kubernetes, put the directory on a PersistentVolume; without one, recordings are lost when the pod restarts while the database still lists them. Several gateway replicas that share a database must share the directory too (ReadWriteMany). At startup a gateway completes the index entries of recordings that were still open, which assumes no other replica is recording at that moment.
- **Failures.** A session whose recording file cannot be created is refused (Guacamole status 512) rather than run unrecorded, and counted in `molcajete_gateway_recording_failures_total`. A write error during a session is logged and the rest of that session is not recorded.
- **Format.** Files are in Guacamole's recording format, whichever daemon served the session. `guacenc` (from guacamole-server built with `--enable-guacenc`; it is not in the `guacamole/guacd` image) and the stock player play them, unless the connection keeps molcajete's image and video formats.
- **guacd's own recording.** Connections with guacd's `recording-path` parameters keep working: those parameters go to stock guacd unchanged and guacd writes its own files on its host. molcajeted does not implement them.

### `[audit]`

How long the [audit log](administration.md#audit-log) of sign-ins and administrative changes is kept.

| Key | Default | |
| --- | --- | --- |
| `retention_days` | `365` | Delete entries older than this many days, at startup and every hour. `0` keeps them forever. |

The audit log is in the database, so it is backed up with it. Each entry is also a log line with the target `audit` ([observability.md](observability.md#sign-ins-and-changes)), for keeping a copy elsewhere or for longer.

### `[[users]]`

Administrators created at startup when no user with that username exists yet. Existing users and their passwords are left alone, so changing a password here later has no effect; use the web UI or `set-password`.

| Key | |
| --- | --- |
| `username` | required |
| `password` | required |

With no user in the database at all, the gateway creates `admin` with a random password and logs it (`created the first administrator; sign in and change this password`).

### `[[connections]]`

Connections imported at startup when no connection with that id exists yet; afterwards, edit them in the web UI. Ids must be unique in the file.

| Key | |
| --- | --- |
| `id` | required. Used in URLs, metrics labels and recording file names. |
| `name` | Shown in the connection list; defaults to `id`. |
| `protocol` | required. The daemon's protocol: `vnc`, `rdp`, `ssh` (molcajeted and guacd), `telnet`, `kubernetes` (guacd). |
| `guacd` | This connection's daemon, `{ host = "…", port = … }`; otherwise `[guacd]`. The handshake timeout is always `[guacd] handshake_timeout`. |
| `parameters` | Protocol parameters sent to the daemon in `connect`, as strings (Guacamole's parameter names: `hostname`, `port`, `password`, `security`, `resize-method`, …). Credentials are encrypted when imported, but stay readable in this file: remove them once imported, or leave them out. |
| `attributes` | molcajete's own settings, never sent to the daemon. Flags are on when the value is `"true"`: `record-sessions`, `record-keys`, `record-exclude-mouse`, `record-molcajete-formats`. |

```toml
[[connections]]
id = "windows"
name = "Windows (RDP)"
protocol = "rdp"
guacd = { host = "127.0.0.1", port = 4823 }
attributes = { record-sessions = "true" }
[connections.parameters]
hostname = "192.168.0.20"
port = "3389"
username = "user"
password = "change-me"
security = "any"
ignore-cert = "true"
resize-method = "display-update"
```

### Client settings

The web client asks `GET /api/settings` which optional features are on: `clientReports` (`--client-reports`) and `recordings` (`[recordings] enabled`). A few URL parameters on a session page change what one browser does, which helps when comparing behaviour or isolating a problem:

| Parameter | Effect |
| --- | --- |
| `?main` | Run the client on the page's main thread instead of a worker. |
| `?text` | Keep the tunnel on the Guacamole text protocol (base64). |
| `?png` | Receive lossless updates as PNG instead of zstd images. |

### molcajeted

| Flag | Environment variable | Default | |
| --- | --- | --- | --- |
| `-b`, `--bind-host ADDR` | `MOLCAJETED_BIND_HOST` | `127.0.0.1` | Listen address. The daemon image sets `0.0.0.0`. |
| `-l`, `--bind-port PORT` | `MOLCAJETED_BIND_PORT` | `4822` | |
| `--video LIST` | `MOLCAJETED_VIDEO` | `av1,h265,h264` | Codecs for busy screen areas, most preferred first among those the client can decode (`hevc` and `avc` are accepted too). `none` always sends images. |
| `--ffmpeg PATH` | `MOLCAJETED_FFMPEG` | `ffmpeg` from `PATH` | ffmpeg for the video encoders (hardware encoders when a trial encode works, otherwise libsvtav1, libx265, libx264). On macOS, H.265 and H.264 use VideoToolbox directly. |
| `--image-zstd true\|false` | `MOLCAJETED_IMAGE_ZSTD` | `true` | Send lossless updates as zstd images to clients that announce the type; `false` keeps every client on PNG. |
| `--encode-threads N` | `MOLCAJETED_ENCODE_THREADS` | `0` | Threads that encode the images of a frame side by side, shared by all sessions. `0` uses half the available cores, at most 8; `1` encodes on each session's own thread. |
| `--log-format`, `--telemetry-listen`, `--profiling` | `MOLCAJETE_LOG_FORMAT`, `MOLCAJETE_TELEMETRY_LISTEN`, `MOLCAJETE_PROFILING` | | As for the gateway; see [observability.md](observability.md#options). |
| | `RUST_LOG` | `info,pprof=warn` | Log filter. |

At startup molcajeted logs the encoders it found (`video encoders`). After three encoder failures in a session it stops using video for that session.

## Upgrading

The web client ships inside the gateway image, so deploying the gateway also upgrades the client. Browser tabs opened before keep running the old client until they are reloaded. The gateway and molcajeted can be upgraded in either order ([Mixed versions](#mixed-versions)).

### Database migrations

The gateway applies pending migrations when it starts (and when `set-password` runs), for SQLite, PostgreSQL and MySQL alike. There are no down migrations: **back up the database before upgrading if you may need to roll back**, and roll back by restoring that backup together with the older image.

| Migration | Changes |
| --- | --- |
| `20261125000000_init` | Users, sessions, connections, parameters and per-user grants. |
| `20261126000000_groups_roles_folders` | `molcajete_user.is_admin` becomes `molcajete_user.role` (administrators get `admin`, everyone else `user`) and **the `is_admin` column is dropped**. Adds groups, members, folders (`molcajete_connection_group`, `molcajete_connection.parent_id`) and grants of folders to users and of connections and folders to groups. Existing grants are kept; existing connections sit at the top level. |
| `20261126000100_recordings` | `molcajete_connection_attribute` (molcajete's own connection settings) and `molcajete_recording` (the recordings index). |
| `20261126000200_oidc` | `molcajete_user_identity` (provider `iss` and `sub` per user), `molcajete_oidc_login` (sign-ins in progress) and `molcajete_session.id_token`. |
| `20261127000000_audit_log` | `molcajete_audit_log` (the audit log). It starts empty: changes made before the upgrade are only in old process logs. |

- A gateway that predates `20261126000000_groups_roles_folders` cannot start on a migrated database, because the column it reads is gone. Do not run old and new gateways against one database: stop the old one first (the Kubernetes template's `Recreate` strategy does this).
- The JSON API keeps `isAdmin` on users, derived from the role, and accepts it in user requests, so scripts that used it keep working.
- `[[users]]` from the configuration file and `set-password` still create administrators.

### Encrypted credentials

Gateways from this change on keep [connection credentials](administration.md#stored-credentials) encrypted. The schema does not change; the data does:

- **Before upgrading, configure the [secret key](#secrets)** wherever `<data_dir>` is not on persistent storage (PostgreSQL or MySQL without a data volume, Kubernetes): otherwise the gateway generates a key that is lost with the pod, or fails to start when `<data_dir>` is read-only. Back the key up.
- **On every start the gateway encrypts credentials still stored in plain text**, in one transaction, and logs `encrypted stored connection credentials that were in plain text count=<n>`. The first start after the upgrade encrypts all existing ones; later starts find none, unless a newer gateway adds a parameter name to the list of credentials.
- **Rolling back needs the database backup taken before the upgrade.** An older gateway would send the encrypted values to remote servers as passwords. Restoring the backup also undoes connection changes made since.
- The connection API no longer returns credentials ([JSON API](administration.md#json-api)). Scripts that read a connection and write it back keep its credentials, since a credential left out of a `PUT` is kept.

### Configuration files

The configuration is strict in both directions: a newer gateway rejects keys it does not know, and an older one rejects sections it does not know (`[oidc]`, `[recordings]`, `[audit]`, `[secrets]`). When rolling back, roll back the configuration file too. No released key has been renamed or removed so far, and new sections and keys are optional with defaults that keep the previous behaviour, with one exception: since `[server] trusted_proxies` was added, `X-Real-IP` and `X-Forwarded-For` are read only from the proxies it lists. A gateway behind a proxy needs it set, or every client has the proxy's address in logs, in the audit log and for [failed sign-in limits](administration.md#repeated-failed-sign-ins).

## Mixed versions

Old and new versions of the web client, the gateway and molcajeted work together: each extension is announced or negotiated per session and falls back to stock Guacamole behaviour, and the gateway relays instructions and arguments it does not know. Stock guacd and stock Guacamole clients see no difference.

| Feature | Needs | With an older or stock component | Check, or turn off |
| --- | --- | --- | --- |
| Binary tunnel: image, video and audio data as raw bytes, about 25% fewer bytes | Negotiated per leg: client and gateway (WebSocket subprotocol), gateway and molcajeted (handshake) | Gateway and client new, daemon older or stock guacd: binary to the browser, the gateway decodes the daemon's base64 (`framing="transcoded"`). Gateway older, or molcajeted alone upgraded: text, as before. All three new: `framing="binary"`. | `framing` in the gateway's `tunnel established`; `binary=true` in molcajeted's `connection established`. `[server] binary_tunnel = false` for every tunnel, `?text` for one session. With `transcoded`, `molcajete_gateway_relayed_bytes_total{direction="to_browser"}` counts what the gateway read, and the browser receives about a quarter less. |
| zstd images: a lossless image type the client decodes in wasm, instead of PNG | Web client and molcajeted; the client announces `image/x-molcajete-zstd-v1` in the handshake. Any gateway relays it, over either tunnel. | Daemon older or stock guacd: PNG (it ignores the unknown mimetype). Client older, stock guacamole-common-js or the stock web application: PNG (it does not announce the type). Photographs, images with transparency and the cursor are PNG in every case. | `image` in molcajeted's `connection established`; `molcajeted_image_*{format="zstd"}`. `--image-zstd false` for a daemon, `?png` for one session. |
| Remote display size and scale (RDP): resize the desktop with the window, choose the DPI | Web client, gateway (passes the browser's `param.dpi` and `param.resize-method`) and molcajeted (applies them during the session) | Stock guacd: choices apply at the next connection, and the menu rows stay enabled for servers without Display Control. Gateway older: it ignores `param.*`, so a remembered choice applies only once changed in a session. Client older with new molcajeted: the desktop resizes only with `resize-method=display-update`. Existing RDP connections without `resize-method` do not follow the window until it is turned on in the connection form or the session menu. | molcajeted logs `Display Control availability`, `client changed the resize method` and `client changed the DPI`; the gateway logs the browser's overrides in `opening tunnel`. |
| Frame pacing by apply time: the frame interval follows how long the client takes to apply a frame, not the whole round trip | Web client and molcajeted (an extra `sync` argument each way); any gateway relays it | Daemon older or stock guacd: neither side asks, and pacing is as before. Client older or stock: it ignores the extra argument and molcajeted paces by the smoothed round trip. | `molcajeted_client_apply_seconds` stays empty for clients that do not report. `molcajeted_unacked_frames` stays within the window (3 frames for RDP and VNC, 2 for SSH). |
| Terminal copy, paste and links | Shortcuts and toasts in the web client; links and clearing the selection on click in molcajeted (SSH) | Stock guacd (SSH, telnet, Kubernetes): shortcuts and toasts but no links, and after a click clears the selection the copy shortcut still copies the previous one. Client older with new molcajeted: links are underlined but cannot be opened. | |
| Session recordings | Gateway only (it records and indexes; the player ships with it) | Any daemon, stock guacd included. Recorded sessions offer the daemon only stock formats, so molcajeted sends PNG and JPEG instead of zstd images and video, unless the connection has `record-molcajete-formats`; those recordings play only in molcajete's player. | `record` in the gateway's `opening tunnel`; `recording the session` and `recording complete` in its log. `[recordings] enabled = false` for every connection. |
| SSH jump hosts: reach the server through bastions | molcajeted only (the `jump-hosts` and `jump-host-*` parameters); the form section is in the web UI, and any gateway passes the parameters and the `required` prompts through | Stock guacd and older molcajeted do not ask for these parameters, so they are never sent; the daemon connects to the server directly, which usually fails because only the bastion can reach it. Web UI older: the parameters can still be set under *Advanced*. | `connected through jump hosts` in molcajeted's log; remove `jump-hosts` from the connection to connect directly. |

- molcajeted serves the stock Guacamole web application and guacamole-common-js as well; those clients use none of the extensions above.
- molcajeted implements VNC, RDP and SSH. Connections with other protocols, or with parameters only guacd implements (such as `recording-path`), need stock guacd.
