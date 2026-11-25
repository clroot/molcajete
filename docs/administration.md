# Administration

Who can sign in, what they can open, what is recorded, and who changed what. Everything here is done in the gateway's web UI (*Connections*, *Users*, *Groups*, *Recordings*, *Audit log*) or its JSON API, and stored in the gateway's database. The settings in `gateway.toml` that these features read are listed in the [configuration reference](deployment.md#configuration-reference).

## Users and passwords

Like Guacamole's JDBC authentication, the gateway keeps users, groups, connections, folders and grants in a database (PostgreSQL, MySQL/MariaDB or SQLite; see [`[database]`](deployment.md#database)). The schema is created and migrated when the gateway starts.

### The first administrator

- With no user in the database, the gateway creates an `admin` account with a random password and writes the password to its log (`created the first administrator; sign in and change this password`).
- `[[users]]` entries in `gateway.toml` are created as administrators when no user of that name exists yet. Afterwards the file is not read for them again: passwords and roles changed in the web UI stay as they are.

```toml
[[users]]
username = "admin"
password = "change-me"
```

### Managing users

Administrators manage users under *Users*: username, password, role, groups, direct grants (see [connections and who can open them](#connections-and-who-can-open-them)) and *Disabled*.

- Passwords need at least 8 characters. They are stored as Argon2id hashes.
- Setting a new password, or disabling a user, signs that user out of every browser. A disabled user cannot sign in, whichever way they try.
- Everyone can change their own password with *Change password* in the side menu; this signs them out everywhere too.
- Administrators cannot remove their own administrator role, disable themselves or delete their own account, and at least one enabled administrator always remains: a change that would leave none is refused.
- With [single sign-on](#single-sign-on-openid-connect) configured, a user can be created without a password; such a user signs in only through the identity provider.

A login session lasts 12 hours (`[auth] session_ttl_hours`). The session cookie is `HttpOnly` and `SameSite=Strict`, and `Secure` when the browser reached the gateway over HTTPS.

### Repeated failed sign-ins

The gateway counts failed password sign-ins and, past a limit, refuses further attempts for a while with `429 Too Many Requests` and a `Retry-After` header. The login page shows *Too many failed sign-ins. Try again in 5 minutes.* Refused attempts are turned away before the password is checked, and do not count as failures themselves.

- **Per address.** 10 failures from one client address within 5 minutes, whatever usernames were tried, and that address is refused for any username until the oldest of them is 5 minutes old. Other addresses are not affected. An IPv6 address counts as its /64.
- **Per username.** 20 failures for one username within 15 minutes, from any number of addresses, and that username is refused, which stops a guess spread over many addresses. Unknown usernames are counted and answered in the same way, so the answer does not tell whether a user exists.
- **The user's own addresses stay open.** An address the user signed in from within the last 30 days is exempt from the username limit (not from the address limit), so someone guessing a user's password from elsewhere cannot lock them out of their usual places. From a new address the user waits like anyone else, or signs in through [single sign-on](#single-sign-on-openid-connect), which is not throttled: the identity provider checks the password and throttles its own form.
- **A successful sign-in** forgets that address's failures for that username only. Signing in to one's own account does not clear guesses at another account from the same address.
- **Password checks run a few at a time** (as many as the server has CPUs), so a burst of sign-ins cannot take the CPU from open sessions. A sign-in that waits more than 10 seconds for its turn gets `503` and can be retried.

The counters are kept in memory, per gateway replica, and a restart clears them, which is also the way to lift a lockout early. Each lockout is recorded once in the audit log as `login.throttled` and counted in `molcajete_gateway_logins_throttled_total` ([observability.md](observability.md#gateway)); the limits are set in [`[auth.throttle]`](deployment.md#auth).

Behind a reverse proxy, the client's address is only known when the proxy is listed in [`[server] trusted_proxies`](deployment.md#reverse-proxy). Otherwise every user has the proxy's address and they share the per-address limit: a few typing mistakes anywhere would lock everyone out for five minutes.

### Recovering access

`set-password` sets a user's password from standard input and makes the account an enabled administrator, creating it if it does not exist. It works directly on the database, so the gateway does not need to be running:

```sh
echo 'new-password' | molcajete-gateway --config gateway.toml set-password admin
```

In a container, run it in the gateway container with the same configuration (and `MOLCAJETE_DATABASE_URL` or `MOLCAJETE_DATA_DIR`, if set).

## Roles

Each user and each user group has one of three roles. A user's effective role, the one the gateway checks, is the highest of their own and their groups' roles.

| Role | In the API | Can |
| --- | --- | --- |
| User | `user` | Open the connections granted to them. |
| Connection manager | `connection_manager` | Also open every connection; create, edit, move and delete connections and folders; grant them to users and groups. Cannot manage users, groups, roles or recordings. |
| Administrator | `admin` | Everything, including users, groups, roles and recordings. |

Connection managers see the parameters of every connection except [stored credentials](#stored-credentials), which nobody can read back, administrators included. They can turn recording on or off for a connection, but only administrators can open the recordings.

## User groups

Administrators manage groups under *Groups*. A group has a name, a role, members, and grants of its own.

- Members share the group's grants: grant a folder to a group once instead of to each person.
- Members get the group's role when it is higher than their own. A group with the `admin` role makes all its members administrators.
- Removing someone from a group takes away what the group gave them and leaves their own grants. Deleting a group does the same for every member.
- With single sign-on, group memberships can follow the identity provider's groups ([`sync_groups`](#roles-and-groups-from-the-provider)).

## Connections and who can open them

### Connections

Connection managers and administrators create and edit connections under *Connections*. A connection has:

- an **id**, used in the session's address (`#/connect/<id>`); 1–64 letters, digits, `-`, `_` or `.`, fixed once created;
- a **name** shown in the list;
- a **protocol**: VNC, RDP, SSH, Telnet or Kubernetes;
- the **parameters** sent to the daemon in Guacamole's `connect` instruction. The form has fields for the common ones (host name, port, credentials; for RDP also the domain, security mode, keyboard layout, resizing the desktop to the window and display scale; for SSH a private key and font size; for Kubernetes namespace, pod and container). Any other Guacamole parameter can be added under *Advanced* as a name and value. Empty values are not stored;
- optionally a **daemon** of its own (*Advanced → guacd host / port*) instead of the gateway's default `[guacd]`: this is how a connection is served by molcajeted while others use stock guacd, or the other way round. molcajeted serves VNC, SSH and RDP; Telnet and Kubernetes need stock guacd;
- optionally **SSH jump hosts** to reach the server through ([below](#ssh-jump-hosts));
- **recording** settings ([session recordings](#session-recordings));
- the **folder** it is in.

Deleting a connection takes it away from everyone. Sessions that are open keep running until they disconnect; recordings of the connection are kept.

Connections can also be written in `gateway.toml`. They are imported once, when no connection with that id exists yet; afterwards the web UI is where they are changed:

```toml
[[connections]]
id = "office-pc"
name = "Office PC"
protocol = "rdp"
guacd = { host = "127.0.0.1", port = 4823 }    # optional: this connection's daemon
attributes = { record-sessions = "true" }      # optional: molcajete's own settings
[connections.parameters]
hostname = "10.0.0.5"
port = "3389"
security = "any"
ignore-cert = "true"
```

The attributes are the recording switches below: `record-sessions`, `record-exclude-mouse`, `record-keys` and `record-molcajete-formats`, each `"true"` when on. Unlike parameters they are never sent to the daemon.

A password written in `[connections.parameters]` is encrypted in the database like one typed in the form, but it stays readable in `gateway.toml`. Remove it from the file once the connection is imported, or leave it out and type it in the form.

### SSH jump hosts

A server that only a bastion can reach is opened through SSH jump hosts, set in the *SSH jump hosts* section of the connection form for any protocol. Only molcajeted connects through them: it logs in to the first jump host, from there to the next one, and has the last open the connection to the server. Stock guacd ignores these parameters and connects directly.

- **Jump hosts** (`jump-hosts`): one per line (or separated by commas), in the order they are passed through, as `user@host` or `user@host:port`; the port defaults to 22 and IPv6 addresses go in brackets (`[2001:db8::1]:22`). Up to four.
- **Username, password, private key, key passphrase** (`jump-host-username`, `jump-host-password`, `jump-host-private-key`, `jump-host-passphrase`): used for every jump host. The username is for hosts listed without one.
- **Host key** (`jump-host-host-key`): the jump hosts must present this key, or the connection is refused. It takes a known_hosts line, a public key (`ssh-ed25519 AAAA…`) or a SHA-256 fingerprint (`SHA256:…`, as `ssh-keygen -lf` prints it), one per line. Without it any key is accepted. `ssh-keyscan -t ed25519 bastion.example.com` prints a host's key; check it against the host itself before pinning it.
- **One jump host differs from the others**: add its own values under *Advanced → Other parameters*, named after its position in the list: `jump-host-2-password`, `jump-host-2-private-key`, `jump-host-2-passphrase`, `jump-host-2-host-key` for the second. Its own password or key replaces the shared password and key.

The jump hosts must allow TCP forwarding (OpenSSH: `AllowTcpForwarding yes`, the default). When a password is left out, the browser asks for it when the session starts (*Jump host password*, or *Jump host 2 password* with several hops). Errors name the jump host they come from, for example `jump host 1 (admin@bastion:22) presented a host key that does not match its configured host key`.

In `gateway.toml` the parameters go with the others:

```toml
[connections.parameters]
hostname = "10.20.0.5"        # as the last jump host sees it
jump-hosts = "admin@bastion.example.com"
jump-host-private-key = """
-----BEGIN OPENSSH PRIVATE KEY-----
...
-----END OPENSSH PRIVATE KEY-----
"""
jump-host-host-key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF8z..."
```

The host name and port of the connection are resolved and reached by the last jump host, not by molcajeted. Each session opens its own SSH connections to the jump hosts.

### Stored credentials

The credentials of a connection are the parameters whose name contains `password`, `passphrase`, `private-key` or `secret` (`password`, `sftp-password`, `gateway-password` for an RDP gateway, `jump-host-2-private-key`, …), and `client-key` (Kubernetes) and `token`, whether typed in a form field or added under *Advanced*. Patterns such as telnet's `password-regex` are not credentials, nor are host keys (`jump-host-host-key`), which are public.

- **They are write-only.** Once saved, nobody can read them back, administrators included: the form shows a stored one as an empty field saying *Saved — type to replace*. Typing a new value replaces it, saving without touching it keeps it, and the trash button next to it removes it when you save.
- **They are encrypted in the database** with a key the database does not hold, and decrypted only to open a session. Someone with a copy of the database, or a backup of it, does not get them without the key. Keeping that key and backing it up is described in [deployment](deployment.md#secrets).
- **They don't have to be stored at all.** Leave the password (or username) empty and the remote server asks for it: the browser shows a *Credentials required* dialog when the session starts, and what is typed there goes to the daemon for that session only. SSH and Telnet ask in the terminal instead.

### Folders

Connections live in a tree of folders.

- A grant on a folder covers every connection below it, in subfolders too.
- Moving a connection or folder (*Move to…*) into or out of a folder therefore changes who can open it: users granted the new folder gain it, grants on the folders it left stop applying. A folder moves with everything in it.
- Deleting a folder moves what it holds, connections and folders, up one level, and removes the grants on it, so people may lose access to the connections that were in it.

Users who are not connection managers see only the folders that lead to a connection they can open.

### Grants

A user can open the union of:

- the connections granted to them,
- the connections granted to any of their groups,
- every connection below a folder granted to them or to any of their groups.

Connection managers and administrators can open every connection. The tunnel refuses any other connection as unknown.

Grants are set in two places: from a user's or group's drawer (*Folders*, *Connections*), or from a connection's or folder's *Access…* action (*Users*, *Groups*).

## Single sign-on (OpenID Connect)

With an `[oidc]` section in `gateway.toml`, the login page offers *Sign in with &lt;display_name&gt;* next to the password form, or instead of it. The gateway runs the authorization code flow with PKCE against any OpenID Connect provider (Keycloak, Authentik, Entra ID, Okta, Google Workspace, …) and then opens the same kind of login session as a password sign-in. Every key and its default is in the [configuration reference](deployment.md#oidc).

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

### Setting it up

1. Register a client at the provider with the standard (authorization code) flow. Its redirect URL is the browser-visible address of `/api/auth/oidc/callback` on the gateway, the same as `redirect_url`. Register `post_logout_redirect_url` too if you set it.
2. Give the gateway the client secret: `client_secret`, `client_secret_file` or the `MOLCAJETE_OIDC_CLIENT_SECRET` environment variable, which wins. Without any, the gateway is a public client and relies on PKCE alone.
3. Set `issuer` to exactly the `iss` of the provider's ID tokens (trailing slashes matter); the gateway reads `<issuer>/.well-known/openid-configuration`.
4. If roles or groups should follow the provider, make the provider put group names in the ID token or userinfo (claim `groups` by default, `groups_claim` to change it, dotted paths such as `realm_access.roles` for nested claims) and add the scope that releases them to `scopes` if the provider needs one.
5. Restart the gateway and sign in with the button on the login page.

### Users

- On their first sign-in, people are created as users with the `user` role, no password and no connections (`auto_create_users = false` lets in only users an administrator created first, matched by username).
- An existing user with the same username is linked to the identity on its first sign-in, unless that user is already linked to someone else at the provider. Use a username claim (`username_claim`, `preferred_username` by default) that people cannot change themselves at the provider: whoever arrives with the name `admin` is linked to the local `admin` account.
- Users are then recognised by the provider's issuer and subject, not by name. A user renamed at the provider keeps their account, and the local username follows when the new one is free.
- Users created on sign-in cannot use the password form or *Change password*; an administrator may still set them a password.
- Disabled users are refused, whichever way they sign in.
- Signing out of a session opened through the provider also signs out there, when the provider supports RP-initiated logout (`end_session_endpoint`).

### Roles and groups from the provider

On every sign-in:

- **Role.** `admin_groups` and `connection_manager_groups` list provider groups. Members of any of `admin_groups` get the `admin` role, members of any of `connection_manager_groups` (and none of `admin_groups`) get `connection_manager`, everyone else `user`. With `sync_role = true` (the default) that is the user's own role on every sign-in, so leaving the groups lowers it again; with `false` it only ever raises it. With neither list, roles are left to the administrators.
- **Groups.** With `sync_groups = true`, the user is made a member of exactly the molcajete user groups whose names appear in the groups claim (names compared exactly). Memberships in other groups are removed, including ones an administrator added by hand: the provider decides every membership. `group_prefix` limits this to provider groups whose names start with it (the names are kept as they are). A name with no molcajete group is skipped, or, with `create_groups = true`, a group of that name is created with the `user` role and no grants. Give the groups their role, folders and connections in *Groups*.
- A user's effective role is still the highest of their own and their groups' roles, so with `sync_groups` a molcajete group with the `admin` role is another way to make administrators.
- Neither may leave no enabled administrator. When a sign-in would, that change is skipped with a warning in the log and the user still signs in. Demote the last administrator by hand after making someone else one.

### Turning the password form off

`password_login = false` hides the password form and refuses password sign-in (`POST /api/auth/login`), so everyone signs in through the provider. Keep a way back in:

**Break-glass.** If the provider is unavailable or nobody can sign in as an administrator: set `password_login = true` (or remove the line), restart the gateway, run [`set-password`](#recovering-access) for an account and sign in with that password. Turn it off again afterwards.

### When sign-in fails

The browser returns to the login page with a short message (`?login_error=<code>`); the reason is in the gateway log as `single sign-on failed`:

| Code | Meaning |
| --- | --- |
| `expired` | The sign-in took more than ten minutes, or was finished in another browser than the one that started it. |
| `cancelled` | The user cancelled at the provider. |
| `unavailable` | The gateway cannot reach the provider. |
| `no_username` | The ID token and userinfo have no usable `username_claim`. |
| `disabled` | The account is disabled. |
| `not_allowed` | No such user and `auto_create_users` is off. |
| `username_taken` | The username belongs to another identity at the provider. |
| `failed` | Anything else (token checks, provider errors, database errors). |

How the flow is secured: the ID token's signature, issuer, audience, expiry, nonce and access token hash are checked, and claims missing from it are read from the userinfo endpoint. A sign-in in progress is kept in the database for at most ten minutes and used once, and its `state` also in a cookie, so a callback is accepted once and only in the browser that started it. Several gateway replicas can share the work as long as they share the database. Discovery and signing keys are cached for ten minutes and fetched again when a signature does not verify.

### Provider notes

- **Keycloak.** Issuer `https://<host>/realms/<realm>`. Create a confidential OpenID Connect client with the standard flow, the redirect URL and *Valid post logout redirect URIs*. For groups add a *Group Membership* mapper with token claim name `groups`, *Full group path* off and *Add to ID token* on. `docker/keycloak/compose.yml` starts a test realm with this set up (users `alice`, in `molcajete-admins`, and `bob`); `docker/keycloak/gateway.keycloak.toml` runs a gateway against it on `http://localhost:18083`.
- **Authentik.** Create an OAuth2/OpenID provider and an application. The issuer is `https://<host>/application/o/<application-slug>/`, including the trailing slash. The default `profile` scope mapping includes `groups` and `preferred_username`.
- **Entra ID.** Register an application with a web redirect URI and a client secret. The issuer is `https://login.microsoftonline.com/<tenant-id>/v2.0`, and `preferred_username` is the user principal name (`user@domain`). For groups, add a groups claim under *Token configuration*. Entra sends group object ids unless the groups come from on-premises AD, so list those ids in `admin_groups` and `connection_manager_groups` (and expect them as group names with `sync_groups`).

## Session recordings

The gateway can write every session of a connection to a file, whichever daemon serves it, stock guacd or molcajeted. Administrators play the recordings back in the browser, download them, or turn them into video with Guacamole's `guacenc`.

### Recording a connection

Turn on *Record sessions* in the connection's *Recording* settings. Recording must also be enabled in the gateway (`[recordings] enabled`, on by default); when it is off, the form says so and nothing is recorded. The directory, the retention limits and the rest are in the [configuration reference](deployment.md#recordings).

Below *Record sessions*:

- **Record the mouse** (on by default) records where the pointer was and what was clicked.
- **Record key presses** (off by default) records every key typed. That includes passwords typed during the session, which every administrator can then read from the recording. The form warns about it, and the recordings list marks such recordings *Includes key presses*.
- **Keep molcajete's image and video formats** (off by default): see [compatibility](#compatibility-with-guacamoles-tools).

A session that is to be recorded never runs unrecorded: if its file cannot be created, the session is refused with a server error. A write error during a session is logged, and the rest of that session is not recorded.

Connections that use guacd's own recording parameters (`recording-path` and the like) keep working: those parameters go to stock guacd unchanged and guacd writes its own files on its host. molcajeted does not implement them; use *Record sessions* instead.

### Compatibility with Guacamole's tools

A recording is what guacd writes for `recording-path`: the instructions the browser received, as Guacamole protocol text, with the user's mouse and, if chosen, keys in between. The stock player (`guacamole-common-js`'s `SessionRecording`) and `guacenc` read it, as does molcajete's player:

```sh
guacenc -s 1280x800 -f 20261125-093012_office-pc_<id>.guac   # writes an .m4v next to it
```

`guacenc` is not in the `guacamole/guacd` image; build guacamole-server with `--enable-guacenc`.

molcajeted's zstd images and video streams are unknown to those tools; a session recorded with them is mostly black there. So a recorded session does not use them: molcajeted sends PNG and JPEG images, as it does to the stock client. That costs the bandwidth and browser time those formats save, for recorded connections only. Stock guacd never sends them, so nothing changes for it.

*Keep molcajete's image and video formats* lifts that for one connection. Its sessions get the same formats as unrecorded ones, and its recordings are smaller and seek faster, but only molcajete's player can play them.

### Playing recordings back

*Recordings* lists every recorded session, newest first, with the connection, the user, the protocol, the length (or *Recording* while the session is still going), the size and whether key presses are included. Each one can be played, downloaded as a `.guac` file, or deleted once the session has ended. Connection and user names are copied into the recording when it starts, so a recording outlives a renamed or deleted connection or user.

The player plays in the browser at 1×, 2×, 4× or 8×, pauses, and seeks anywhere with the position slider. Audio is not played back. Seeking backwards replays from the start. Seeking to the end of an hour of a screen that changed 20 times a second for the whole hour took 2.8 s with molcajete's formats, 5.0 s with the PNG images of an ordinary recorded molcajeted session and 16.2 s for a stock guacd recording; ordinary desktop sessions change much less and seek proportionally faster ([architecture](architecture.md) has the measurements).

Only administrators can list, play, download or delete recordings; every other request gets 403. Downloads and deletions go into the [audit log](#audit-log).

### Retention

Recordings are kept until an administrator deletes them, unless `[recordings]` sets limits:

- `max_age_days` deletes recordings that ended longer ago than that;
- `max_total_size` deletes the oldest recordings once all of them together take more than that.

Retention runs when the gateway starts and every ten minutes. Sessions still being recorded are never deleted. Files are named `YYYYMMDD-HHMMSS_<connection>_<id>.guac` (UTC start time) in the recordings directory; keep that directory on persistent storage, see the [configuration reference](deployment.md#recordings).

## Audit log

The gateway keeps a record of every administrative change and every sign-in in its database (`molcajete_audit_log`): who did it, from which address, what they did, to what, and which fields changed. It answers questions such as "who changed the password of connection X last week" or "who made user Y an administrator". Administrators read it under *Audit log*.

### What an entry holds

| Field | |
| --- | --- |
| Time | When it happened. |
| Actor | The user who did it: their id and their username at the time. A failed sign-in has the username that was typed, and no id when no such user exists. Changes made from the configuration file, at first start or with `set-password` have no actor; the page shows them as *Gateway*. |
| Source address | The browser's address as the gateway logs it: behind a [trusted proxy](deployment.md#reverse-proxy) the address the proxy reports, followed by the proxy (`203.0.113.9 (via 10.0.0.2)`), otherwise the TCP peer; `configuration file`, `first start` or `command line` for changes not made in a browser. |
| Action | What happened; see below. |
| Target | The user, group, connection, folder or recording, with its id and its name at the time. |
| Changes | Each changed field with its value before and after; lists (groups, members, grants) as what was added and removed. Details such as why a sign-in failed are here too. |

**Secrets are never stored.** Passwords, private keys, passphrases and secrets show only that they were set or changed, as `[redacted]`, never their value. This covers the user's password and the connection parameters `password`, `passphrase`, `private-key`, `client-key` and their variants (`sftp-password`, `gateway-password`, `sftp-private-key`, ...). Grants, members and folders are shown by name rather than id, as they were named at the time.

### Actions

| Action | When |
| --- | --- |
| `login.success`, `login.failure` | Password sign-in. Failures carry `reason`: `unknown_user`, `wrong_password`, `disabled` or `no_password` (a single sign-on account). |
| `login.throttled` | Too many failed sign-ins: attempts are refused for a while ([Repeated failed sign-ins](#repeated-failed-sign-ins)). Recorded once when the lockout starts, with the username typed in that attempt, `reason` (`address` or `username`) and `retryAfterSecs`. |
| `logout` | Signing out. Sessions that expire are not recorded. |
| `user.password` | A user changed their own password. |
| `oidc.login.success`, `oidc.login.failure` | Single sign-on. Failures carry the `reason` the browser sees (`expired`, `cancelled`, `not_allowed`, ...) and the `error` the gateway logged. |
| `oidc.user.create`, `oidc.user.link`, `oidc.user.rename`, `oidc.user.groups`, `oidc.user.role` | What the provider changed at sign-in: a user created on their first sign-in or linked to their identity, renamed after the provider, groups or role set from the provider's groups. The actor is the user signing in. |
| `user.create`, `user.update`, `user.delete` | Users, including their role, disabled state, groups and grants. A password set by an administrator shows as a changed `password`. |
| `group.create`, `group.update`, `group.delete` | Groups, including their role, members and grants. |
| `connection.create`, `connection.update`, `connection.delete`, `connection.move` | Connections and their parameters and settings. A [stored credential](#stored-credentials) shows only as set, replaced (any value typed again, even the same one) or removed, never its value. |
| `connection.access`, `folder.access` | Who is granted a connection or folder (users and groups added and removed). |
| `folder.create`, `folder.rename`, `folder.move`, `folder.delete` | Folders. |
| `recording.download`, `recording.delete` | Recordings downloaded (playing one back in the page is not recorded) or deleted. |
| `secrets.encrypt`, `secrets.rotate` | Stored credentials encrypted at start because they were in plain text (`count`), and re-encrypted by `rotate-key` (`count`, `keyId`). No actor. |

The names are stable, so filters and alert rules can rely on them; `GET /api/audit/actions` lists them.

### Reading it

*Audit log* lists entries newest first. Filter by who did it (username or user id), by action (one action, or any action of a kind such as every `user` action), by target (name or id) and by dates; open an entry to see what changed. *Export CSV* downloads every entry that matches the filters (up to 100 000) with the columns `time` (UTC), `action`, `actor`, `actor_id`, `source_address`, `target_type`, `target_id`, `target_name` and `changes` (JSON). Values a spreadsheet would read as a formula start with `'`.

Only administrators read the audit log; connection managers' changes are in it, but they cannot see it.

Each entry is also written to the gateway's log as one line with the target `audit`, for a SIEM or log store ([observability.md](observability.md#sign-ins-and-changes)). The entry is stored right after the change succeeds. If storing it fails, the change still stands, the log line is still written, and the gateway logs an error and counts it in `molcajete_gateway_audit_write_failures_total`.

### Retention

Entries older than `[audit] retention_days` (365 by default) are deleted when the gateway starts and every hour after that; `0` keeps them forever. See the [configuration reference](deployment.md#audit).

## JSON API

The web UI uses a JSON API under `/api`, which scripts can use as well. Sign in with `POST /api/auth/login` and send the `molcajete_session` cookie with the following requests. Request bodies are JSON (`Content-Type: application/json`), and a request that changes something is refused when it carries an `Origin` header that does not match the gateway's host.

```sh
curl -c cookies -H 'Content-Type: application/json' \
  -d '{"username": "admin", "password": "…"}' https://molcajete.example.com/api/auth/login
curl -b cookies https://molcajete.example.com/api/recordings
curl -b cookies -OJ 'https://molcajete.example.com/api/recordings/<id>/file?download=true'
```

| Route | Methods | Who |
| --- | --- | --- |
| `/api/auth/login`, `/api/auth/logout` | `POST` | anyone |
| `/api/auth/me` | `GET` | signed-in users |
| `/api/auth/password` | `PUT` (`{"currentPassword": …, "newPassword": …}`) | signed-in users |
| `/api/connections`, `/api/folders` | `GET` | signed-in users (what they can open) |
| `/api/connections`, `/api/connections/{id}` | `POST`, `GET`, `PUT`, `DELETE` | connection managers |
| `/api/connections/{id}/parent`, `/api/folders/{id}/parent` | `PUT` (move; `{"parentId": null}` for the top level) | connection managers |
| `/api/connections/{id}/access`, `/api/folders/{id}/access` | `GET`, `PUT` (`{"users": […], "groups": […]}`) | connection managers |
| `/api/folders`, `/api/folders/{id}` | `POST`, `PUT` (rename), `DELETE` | connection managers |
| `/api/users`, `/api/groups` | `GET` (lists) | connection managers |
| `/api/users`, `/api/users/{id}`, `/api/groups`, `/api/groups/{id}` | `POST`, `GET`, `PUT`, `DELETE` | administrators |
| `/api/recordings`, `/api/recordings/{id}` | `GET`, `DELETE` | administrators |
| `/api/recordings/{id}/file` | `GET` (range requests; `?download=true` to save) | administrators |
| `/api/audit` | `GET` (filters below; `?format=csv` for a CSV download) | administrators |
| `/api/audit/actions` | `GET` (every action name) | administrators |

`GET /api/audit` returns `{"entries": […], "hasMore": true|false}`, newest first. Its query parameters, all optional: `actor` (username or user id), `action` (an action, or a prefix of whole parts such as `user` or `oidc.user`), `targetType` (`user`, `group`, `connection`, `folder`, `recording`), `target` (id or name), `from` and `to` (Unix milliseconds; `to` excluded), `limit` (default 100, at most 1000) and `offset`.

```sh
curl -b cookies 'https://molcajete.example.com/api/audit?action=login.failure&limit=20'
curl -b cookies -OJ 'https://molcajete.example.com/api/audit?target=office-pc&format=csv'
```

A connection read from `/api/connections/{id}` leaves its [credentials](#stored-credentials) out of `parameters` and names the stored ones in `storedSecrets`. Sending a connection back without a credential keeps the stored value, so a script can read, change and write a connection without knowing its password; a new value in `parameters` replaces it, and `"clearSecrets": ["password"]` removes it.

Users carry `role` and, for scripts written before roles, `isAdmin` (true for an effective role of administrator). Requests that create or change a user accept either; `role` wins when both are given. Leaving a field out of a `PUT` keeps its current value, including lists of members, groups and grants.
