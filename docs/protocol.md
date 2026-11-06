# Protocol: how games talk to Adsum

Adsum plays the part of the local Discord client. Games connect to it the
way they would connect to Discord, publish Rich Presence, and get
officially-shaped replies. Two transports are served:

- **IPC** — local sockets named `discord-ipc-0` … `discord-ipc-9`. On Linux
  the real socket is preferably bound in
  `$XDG_RUNTIME_DIR/app/com.discordapp.Discord/`, then in the conventional
  bases `XDG_RUNTIME_DIR`, `TMPDIR`, `TMP`, `TEMP`, and finally a private
  `.adsum-ipc-<euid>` directory under `/tmp` or `/var/tmp`; marked symlinks
  in every existing client layout let games find it wherever they probe (see
  [Linux socket placement](#linux-socket-placement)). On Windows the same
  names are **named pipes** (`\\.\pipe\discord-ipc-0` … `-9`), with no
  directory involved at all; the pipe carries an explicit DACL (full control
  to SYSTEM, Administrators and the owner; read to Everyone and Anonymous),
  matching what clients get from `lpSecurityAttributes = NULL`.
- **WebSocket** — loopback only, on the first free port in
  `6463`–`6472` (`--ws-port-start` / `--ws-port-end`).

Web clients (browsers, Vencord) do not use these transports; they connect
to the bridge instead — see [web-client.md](web-client.md).

## Ports at a glance

| What | Default | Flag |
|---|---|---|
| Game IPC sockets | `discord-ipc-0` … `discord-ipc-9` | — |
| Game websocket | 6463–6472 (first free) | `--ws-port-start` / `--ws-port-end` |
| Bridge (JSON) | 1337–1347 (first free) | `--bridge-port` / `--bridge-port-end` |
| Bridge (MessagePack) | 1338 (scans 1338–1348, first free) | `--msgpack-port` |

## Linux socket placement

Adsum separates where it creates the real socket from where it publishes
compatibility links:

- **Bind directory** — where the real `discord-ipc-N` `UnixListener` is
  created.
- **Alias directory** — an existing directory where Adsum may publish a
  symlink named `discord-ipc-N` that points at the bound socket.

Adsum never binds directly in `/tmp` or `/var/tmp`. It may publish aliases
there, and it may bind inside the private fallback directory described
below.

### Bind order

Candidates are normalized, deduplicated, and validated in order:

1. `$XDG_RUNTIME_DIR/app/com.discordapp.Discord`
2. `$XDG_RUNTIME_DIR`
3. `$TMPDIR`
4. `$TMP`
5. `$TEMP`
6. `<approved-temp>/.adsum-ipc-<euid>`

Shared temp roots (`/tmp`, `/var/tmp`) are filtered out of the bind list,
so a candidate that resolves to either is skipped. `<approved-temp>` is
`/tmp`, then `/var/tmp`, and the first root where the fallback can be
created or validated wins; the fallback directory is created with mode
`0700`. The preferred Flatpak directory is tried first. `$XDG_RUNTIME_DIR`
must already exist as a private real directory; only `app/` and
`app/com.discordapp.Discord/` are created as real directories without
following symlinks. A missing or invalid runtime base skips the preferred
bind, and the next candidate is tried. If no bind directory validates,
startup fails with `IpcBind`; a directory is never deleted or repaired to
make it usable.

Before binding each index, Adsum probes the entry with `lstat` (never
following symlinks) and a connect bounded to one second. A successful
connect is `Active` — a listener that never replies still counts as active
— `ECONNREFUSED` is `Stale`, and any other error or timeout is `Unknown`.
A symlink, regular file, directory, or other non-socket is `NotSocket`.
Only `Stale` may be reclaimed: a new probe immediately before unlink must
still report `Stale`, and the entry must still be a socket with the same
`(dev, ino)`. `Active`, `Unknown`, and `NotSocket` keep the path, and Adsum
tries the next index. A `Missing` path that becomes `AddrInUse` between
probe and bind is retried once, without another probe.

### Directory policies

Every directory Adsum writes into must satisfy exactly one policy:

- **Private directory** — absolute, without a `..` component, with every
  component an existing real directory (no symlinks). Every component must
  be owned by root or the effective UID and not writable by group or others
  (`mode & 0o022 == 0`); the final directory must be owned by the effective
  UID.
- **Approved shared-temp directory** — canonical `/tmp` or `/var/tmp`,
  owned by root or the effective UID, with the sticky bit set and writable
  by the effective UID.

The shared-temp policy is the only exception that allows group or other
write permission. `/tmp`, `/var/tmp`, and the canonical `/private/tmp` on
macOS may appear as chain components when owned by root or the effective
UID and carrying the sticky bit; that ancestor check is metadata-only. The
exact temp root still requires write access by the effective UID. A nested
directory below `/tmp` or `/var/tmp` still has to pass the private policy.
Write access is proven by `probe_temp_write`: an exclusive probe file is
created and removed, and up to eight distinct names are tried on
`AlreadyExists`, while every other error propagates. The probe names are
predictable: the retry covers collisions and residual state, not a
determined attacker, so occupying all eight names can still deny
validation (availability limitation). Relative, empty, root (`/`),
symlinked, non-directory, foreign-owned, and non-sticky world-writable
paths are rejected, and rejection never authorizes deleting the path.

#### User-namespace overflow UID

In a user namespace (including the implicit `PrivateUsers` of a systemd user
unit), host UIDs without a mapping inside appear as the overflow UID from
`/proc/sys/kernel/overflowuid` (normally `65534`). Adsum accepts that UID for
an ancestor component only when all of these hold:

- `/proc/self/uid_map` parses and does not map parent UID 0;
- the overflow UID is not one of the namespace's own mapped UIDs;
- Linux `statx` reports `STATX_MNT_ID` for `/` (kernel 5.8 or newer);
- the component is a mount point or a canonical shared temp root.

The final bind directory is never excused: it must be owned by the effective
UID, and its permission bits must be exactly `0700` (`mode & 0o777 == 0o700`).
Alias directories keep the private policy (effective-UID owner,
`mode & 0o022 == 0`) and may therefore be `0755`. A shared temp root is
validated as a whole canonical chain, so in a sandbox where `/var` is not a
separate mount, `/var/tmp` can be rejected because `/var` appears as the
overflow UID; the private fallback under `/tmp` remains the practical choice.
If a namespace probe fails (`/proc` reads or `statx`/`STATX_MNT_ID`), the
exception is off and these components are rejected as before; a failed write
probe only rejects that temp root.

The kernel cannot distinguish host root from any other unmapped UID. The
residual risk is therefore bounded by the anchor: the exception applies only
to mount points and canonical temp roots, never to the final directory, and
every other owner check is unchanged.

### Socket permissions

Immediately after a successful bind, Adsum sets the real socket to mode
`0600` before publishing any alias or marker. On Linux it opens the
temporary socket with `O_PATH | O_NOFOLLOW | O_CLOEXEC`, checks with
`fstat` that the handle is a socket, and takes `(dev, ino)` from the
handle instead of the path. The mode is applied through the handle with
`fchmodat(..., AT_EMPTY_PATH)`, falling back to `/proc/self/fd/<fd>`, and
the final name is installed with `linkat(..., AT_EMPTY_PATH)`, falling
back to the `/proc/self/fd/<fd>` path with `AT_SYMLINK_FOLLOW`. Replacing
the temporary name after the handle is open therefore cannot redirect the
chmod or the link, and the replacement is left in place. The residual
window is between `bind` and opening the handle; a same-euid substitution
inside that window is what the handle sees. If the mode cannot be set,
the final link is not attempted, and the attempt is cleaned up with the
same identity-checked removal as other owned-socket cleanup before the
next index is tried. After linking, the final `(dev, ino)` is checked; an
entry that does not match is never adopted, and only a final link that
still matches is removed. The temporary name is removed only while it
still names the captured socket. If the captured inode can no longer be
linked, the attempt is abandoned rather than falling back to the temporary
path. Linux never falls back to pathname chmod/link: if neither
`AT_EMPTY_PATH` nor `/proc/self/fd` works for an operation, the attempt
fails closed. Non-Linux Unix keeps the path-based chmod/hard-link fallback
and its residual race.

Processes under the same effective UID are one trust boundary. These
checks protect against other UIDs and accidental or concurrent
replacement, not against a malicious process running as the same user.

### Alias layouts

Alias bases keep the conventional order: `$XDG_RUNTIME_DIR`, `$TMPDIR`,
`$TMP`, `$TEMP`, `/tmp`. Under each base, only layouts that already exist
as real directories are used (symlinked layouts are skipped, never
followed):

- `<base>/discord-ipc-N` — conventional probe
- `<base>/app/com.discordapp.Discord/discord-ipc-N` — official Flatpak Discord
- `<base>/app/com.discordapp.DiscordCanary/discord-ipc-N` — Flatpak Canary
- `<base>/app/com.discordapp.DiscordPTB/discord-ipc-N` — Flatpak PTB
- `<base>/app/dev.vencord.Vesktop/discord-ipc-N` — Vesktop Flatpak
- `<base>/app/org.equicord.equibop/discord-ipc-N` — Equibop Flatpak
- `<base>/.flatpak/com.discordapp.Discord/xdg-run/discord-ipc-N` — legacy Flatpak
- `<base>/.flatpak/com.discordapp.DiscordCanary/xdg-run/discord-ipc-N` — legacy Canary
- `<base>/.flatpak/com.discordapp.DiscordPTB/xdg-run/discord-ipc-N` — legacy PTB
- `<base>/.flatpak/dev.vencord.Vesktop/xdg-run/discord-ipc-N` — legacy Vesktop
- `<base>/.flatpak/org.equicord.equibop/xdg-run/discord-ipc-N` — legacy Equibop
- `<base>/snap.discord/discord-ipc-N` — Discord Snap
- `<base>/snap.discord-canary/discord-ipc-N` — Canary Snap
- `<base>/snap.discord-ptb/discord-ipc-N` — PTB Snap

Alias directories are never created, except for the preferred Flatpak bind
directory. A missing layout is skipped. A path that already exists is
preserved: Adsum never replaces a symlink, socket, regular file, or
directory, even when the existing symlink points at Adsum's bound socket.
A Flatpak wrapper or another service may legitimately own such a link, so
there is no "dangling ours-shaped link" reclaim heuristic.

### Alias ownership markers

Every alias Adsum creates has one marker in the same directory:

```text
<alias-dir>/.adsum-ipc-owner-<euid>-<index>
```

The marker is a `0600` regular file owned by the effective UID, at most
4096 bytes, holding versioned JSON:

```json
{
  "version": 1,
  "owner_uid": 1000,
  "alias": "/tmp/discord-ipc-0",
  "target": "/run/user/1000/app/com.discordapp.Discord/discord-ipc-0",
  "target_dev": 123,
  "target_ino": 456
}
```

`target_dev` and `target_ino` identify the bound socket when the marker was
published. A marker is valid only when it is a regular file (not a
symlink), is owned by the effective UID, has mode `0600`, is at most 4096
bytes, parses as the version 1 schema, has `owner_uid` equal to the
effective UID, names the expected alias, and has an absolute target ending
in `discord-ipc-<index>`; the opened marker's `(dev, ino)` is recorded and
rechecked against the path after the content is read.

Markers are published atomically and never replace an existing entry: a
uniquely named temporary file (`.adsum-ipc-owner-<euid>-<index>.tmp-<pid>-<counter>`)
is written and synced, then hard-linked to the final name with `link()`.
`link()` fails when the final name already exists, so a marker is never
overwritten, even by the same UID. The temporary name is unlinked
afterwards; a leftover temporary file is inert, is not treated as a
marker, and never authorizes cleanup. `rename` is not used because it can
replace another process's marker. The marker is published before the alias
symlink: if the process dies between the two steps, the next start finds a
recoverable orphan marker instead of an unowned alias.

### Recovery, conflicts, and shutdown

Before any bind attempt, Adsum scans each validated alias directory for the
exact marker names `.adsum-ipc-owner-<euid>-<0..9>` and validates every
marker, the alias entry, and the recorded target. In that startup scan,
only two cases remove anything:

- An orphan marker with no alias is removed when its target probes as
  `Missing` or as `Stale` with the recorded `(dev, ino)`.
- An alias symlink whose `readlink` target equals the marker's `target` is
  removed together with its marker under the same dead-target rule, after
  the symlink and marker identities are revalidated.

Everything else is preserved. A live or `Unknown` target keeps the alias
and marker for the next run; a `NotSocket` target and a `Stale` target with
a different identity are preserved as foreign or unproven entries. A
symlink pointing anywhere else, a non-symlink entry, and an invalid or
foreign-owned marker are conflicts and are left untouched. Removing an
orphan alias requires the symlink's `(dev, ino)` to stay unchanged between
the two `lstat` calls around `readlink`, and a recheck before unlink must
still report the target as `Missing` or as `Stale` with the recorded
`(dev, ino)`. A marker whose target is `Stale` with a divergent identity is
not adopted automatically and may remain until the target becomes `Missing`
or until manual reconciliation. Recovery runs before bind so a freshly
bound socket cannot be mistaken for the target recorded in an old marker.
This marker-driven recovery is the only automatic alias/marker cleanup at
startup; it is separate from the per-index stale-socket reclamation that
happens during binding. Neither is an unconditional sweep of `/tmp` or
`$XDG_RUNTIME_DIR`.

On graceful shutdown (Ctrl+C or `SIGTERM`) and on `Drop`, Adsum removes
only aliases recorded as owned by that transport: it revalidates the
marker's identity and content, requires the alias to still be a symlink to
the exact owned target with the recorded `(dev, ino)`, unlinks the alias,
and only then removes the marker. If the alias cannot be removed, the
marker remains for the next startup's recovery. The bound socket is
removed separately, and only when its identity still matches and it is
still a socket; a replaced path is left untouched.

### Flatpak compatibility

`$XDG_RUNTIME_DIR/app/com.discordapp.Discord` is the only path guaranteed
to match the PrismLauncher Flatpak default permission
(`xdg-run/app/com.discordapp.Discord:create`). When Adsum cannot bind there
but conventional IPC still works elsewhere, startup continues and logs
exactly:

```text
[ipc] Flatpak-compatible socket path unavailable; compatibility aliases may not be reachable from sandboxed clients
```

The warning does not fail startup, and it does not mean Flatpak
compatibility is enabled. A conventional alias may still exist in the
official directory when that directory already exists and validates, but a
sandboxed client may not be able to resolve a symlink whose target is
outside its approved mount. Adsum cannot grant a Flatpak or Snap package
access to a host path.

### State snapshot aliases

With `--state-file`, Adsum also publishes its presence snapshot into the
same client runtime directories it publishes IPC aliases into. The alias
name always matches the canonical snapshot file (e.g. `/tmp/arrpc-state-0`
with `ADSUM_STATE_PREFIX=arrpc-state-`), so a client that scans its own
temp directory for `arrpc-state-N` — Equibop Flatpak scans
`$XDG_RUNTIME_DIR/app/org.equicord.equibop/` — finds it without any client
change.

These aliases differ from the IPC ones in one deliberate way: the snapshot
is rewritten atomically (temp + rename), so the target's inode changes on
every heartbeat. The ownership marker therefore records the symlink
identity and the target *path*; the target is validated as a regular,
owner-only, parseable snapshot file and is never inode-compared. While the
bridge runs, a reconcile pass re-publishes missing aliases (a client
runtime dir that appears later is picked up) and removes pairs whose
target went missing or stale beyond the consumer budget (~60s, the same
window status panels use). On shutdown only Adsum-owned aliases and their
markers are removed; foreign entries are preserved, and the canonical
snapshot file itself is left to go stale.

### Official Discord discovery conflict

If the official Discord client is running at the same official path and a
client probes index `0` first, that client may connect to official Discord
instead of Adsum. Adsum does not promise coexistence with official Discord
RPC and does not claim to win client discovery.

### Residual connection-exhaustion risk

The IPC transport allows at most 16 concurrent pumps and a 1 MiB payload
limit. These are resource bounds, not DoS protection: a local client can
open 16 connections and keep them idle or incomplete, occupying every pump
and preventing legitimate clients from being served. Handshake, frame-read,
and idle-connection timeouts are explicitly deferred.

## IPC handshake

Frames are `[4-byte opcode][4-byte length][JSON payload]`, little-endian.
Opcodes: `0` handshake, `1` frame, `2` close, `3` ping, `4` pong.

The first frame must be `{"v": 1, "client_id": "..."}`:

| Problem | Reply |
|---|---|
| `v` is not `1` | close `4004` — "Invalid version" |
| `client_id` is empty | close `4000` — "Invalid client_id" |
| Payload larger than 1 MiB | close `1003` — "Payload too large" |
| Unknown opcode | close `1003` — "Unsupported packet type" |
| Frame is not valid JSON | error `4005` — "Invalid encoding" |

On any connection loss, whatever that connection had published is cleared,
so a crashed game never leaves a card on screen.

## WebSocket handshake

Connect to `ws://127.0.0.1:<port>/?v=1&encoding=json` (add
`&client_id=<app id>` as a fallback for commands that carry no application
id):

- `v` must be `1` and `encoding` must be `json`; anything else is closed
  with the normal close code before any reply.
- Browser origins must be Discord (`discord.com`, `canary.discord.com`,
  `ptb.discord.com`); a missing origin (native clients) passes. Anything
  else is closed.
- A valid client immediately receives `DISPATCH` / `READY`:

```json
{
  "cmd": "DISPATCH",
  "evt": "READY",
  "data": {
    "v": 1,
    "user": { "id": "1045800378228281345", "username": "arRPC", "...": "..." },
    "config": { "api_endpoint": "//discord.com/api", "cdn_host": "cdn.discordapp.com", "environment": "production" }
  },
  "nonce": null
}
```

The `user` object is Adsum's identity — see
[web-client.md](web-client.md) for how to change it.

## Commands

Both transports share one reply vocabulary (see
`crates/adsum-protocol/src/commands.rs`), but they do **not** route
identically. Where a row below differs, the difference is called out — the
most important ones are `DEEP_LINK` and a malformed `SET_ACTIVITY`.

| Command | What happens |
|---|---|
| `SET_ACTIVITY` | Forwarded to the bridge; the game gets an arRPC-shaped confirmation echoing its own activity (plus `application_id`). Clears (`activity: null`) work the same way. |
| `SUBSCRIBE`, `UNSUBSCRIBE` | Acknowledged locally — there is no voice/guild backend to subscribe to. |
| `GET_USER` | Returns the connection's authenticated user after `AUTHENTICATE` when the `identify` scope was granted; otherwise returns the configured fallback identity or `null` as applicable. |
| `GET_GUILDS` | Experimental: when the connection holds a session with the `guilds` scope, proxies `GET /users/@me/guilds` with a 30-second cache (a cache miss is fetched off the pump thread). An unauthenticated connection gets `4006` ("Not authenticated; call AUTHENTICATE first"); an authenticated connection without the `guilds` scope gets `4006` ("Missing required scope: guilds"). |
| `AUTHORIZE` | Experimental: opens browser OAuth, validates a random `state` at the loopback callback, and returns `{code}`. Accepts optional PKCE S256 challenge fields. Requires `ADSUM_OAUTH_REDIRECT_URI`. |
| `AUTHENTICATE` | Experimental: validates `args.access_token` through Discord `/oauth2/@me`, checks the application ID against the handshake, and stores a per-connection session containing user/scopes/expiry. The bearer token stays in memory for that connection only and is never logged or written to disk. |
| `INVITE_BROWSER`, `GUILD_TEMPLATE_BROWSER`, `GIFT_CODE_BROWSER` | Forwarded to bridge clients, then ACKed. The websocket transport validates the code first (`4011` invalid invite, `4017` invalid guild template, `4016` invalid gift code). |
| `DEEP_LINK` | **IPC forwards it, then ACKs. The websocket transport only ACKs** — it builds the acknowledgement and returns, never forwarding. A deep link over the websocket reaches bridge clients as nothing. |
| `CONNECTIONS_CALLBACK` | Refused with an error (code `1000`, "CONNECTIONS_CALLBACK is not supported"). |

A `SET_ACTIVITY` with no `args` at all is treated as malformed input, not a
clear. Over **IPC** it gets error `4005` ("Missing activity args") and
changes nothing. Over the **websocket** there is no error at all: no reply
is built, the connection is reported as alive, and the bridge drops the
command in silence.

A `SET_ACTIVITY` that carries `args` but omits the `activity` field is also
malformed, not a clear: only an explicit `activity: null` clears presence.
With `SET_ACTIVITY` enabled, both transports answer error `4005` ("Missing
activity field") and neither forwards nor tracks the command; with the
feature disabled the websocket transport stays silent for every
`SET_ACTIVITY`, as it does for the other malformed shape above.

### Identity: what Adsum knows, and when

Adsum never talks to the Discord desktop client. Every identity it reports
comes from one of two sources, and neither follows the desktop client
automatically:

1. **The fallback identity** — the user presented in `READY`, in
   unauthenticated `GET_USER` replies, and in the bridge snapshots. It is
   configured at startup with `ADSUM_USER_*` variables (defaulting to the
   arRPC placeholder). No authentication backs it.
2. **The OAuth grant owner** — the user bound to an authenticated
   connection. This is the only identity Adsum can prove, because Discord
   returned it for that exact token.

#### Discord account switches

Nothing in the local RPC protocol announces that the desktop client changed
accounts. Adsum cannot detect a switch on its own, and it does not guess.
After a switch:

- **Presence is unaffected.** `SET_ACTIVITY` is published under the
  application ID, and the Discord client that consumes the bridge shows it
  for whichever account is logged in. Presence carries no identity.
- **An authenticated session keeps reporting the old account.** The OAuth
  grant belongs to the user who approved it in the browser, so `GET_USER`
  and `GET_GUILDS` keep answering for that user until the grant is replaced
  or expires. A client-side account switch does not revoke the token.
- **The fallback identity stays stale** until something updates it.

#### Recovery paths after a switch

| Path | Who acts | Effect |
|---|---|---|
| `SET_USER` control message | The bridge client. Equibop and similar clients run inside the Discord client, so they know the active account. | Patches the shared fallback identity. New connections present it in `READY`, and unauthenticated `GET_USER` reflects it right away. Bridge consumers receive a `CURRENT_USER_UPDATE`. Repeating the same patch does nothing; `RESET_USER` restores the startup identity. Connections that are already open keep the `READY` they received. |
| Fresh `AUTHORIZE` + `AUTHENTICATE` | The game or its user | Re-binds the connection's session to the account currently logged in at `discord.com` in the browser. The per-connection session is replaced; other connections are untouched. |
| Re-`AUTHENTICATE` with a new `access_token` | The game's backend | Same as above, without the browser step. |
| Delete the stored grant | The operator | `adsum oauth logout <client id>` (or `rm ~/.cache/adsum/oauth/<client id>.json`) removes the persisted refresh token. The next `AUTHENTICATE` cannot silently restore the old account, and a browser authorization is required again. |

`SET_USER` never touches OAuth sessions, and OAuth sessions never patch the
fallback identity. One is a label the operator controls; the other is a
credential owned by a grant. Letting them mix would let either side
impersonate the other.

### OAuth prototype constraints

- `AUTHORIZE` accepts `client_id`, `scopes`, and optional PKCE S256 fields in
  `args`. Its ID must match the IPC handshake or WebSocket `client_id` query.
- Set `ADSUM_OAUTH_REDIRECT_URI` to an exact registered URI such as
  `http://127.0.0.1:48152/callback`. The port must be unprivileged (1024 or
  above) and must not collide with Adsum's own listeners (6463-6472, 1337,
  1338). Only one browser authorization can run at a time; the wait is
  bounded by `ADSUM_OAUTH_TIMEOUT` (default 180 seconds). The authorization
  code is returned to the RPC caller.
- `AUTHENTICATE` validates `args.access_token` using `/oauth2/@me`, checks the
  application ID, and stores user/scopes/expiry for that one connection. The
  access token is retained in memory for that session only and is never
  logged or written to disk.
- `AUTHENTICATE` may also be called with `args.refresh_token`, or with no
  token at all, when `ADSUM_OAUTH_CLIENT_SECRET` is set: Adsum then exchanges
  the supplied or previously stored refresh token for a fresh access token
  without a browser flow. The rotated refresh token is stored under
  `~/.cache/adsum/oauth/<client_id>.json` with mode 0600.
- `GET_USER` returns the authenticated user when the token granted `identify`.
  Expired sessions are cleared when used.
- `GET_GUILDS` proxies `GET /users/@me/guilds` while the connection holds a
  session with the `guilds` scope. Responses are cached for 30 seconds per
  connection and the cache is cleared on re-authentication.
- `ADSUM_REQUIRE_RPC_AUTH=1` optionally requires a valid session for all
  commands except `AUTHORIZE`/`AUTHENTICATE`; default off preserves current
  unauthenticated Rich Presence compatibility.
- `rpc_token`, provisional-account `username`, token revocation, and
  the native authenticated Social SDK service connection remain unsupported.
- OAuth commands are rate limited to one per 250 ms per connection by
  default; `ADSUM_OAUTH_RATE_LIMIT_MS` tunes or disables the limit.

See [OAuth prototype](oauth-social-sdk-prototype.md),
[OAuth RPC testing](oauth-rpc-social-sdk-testing.md), and
[Social SDK compatibility](social-sdk-compatibility.md).

### Limits that change what a client observes

- **16 tracked pids per connection.** Beyond the cap, new pids are refused
  before being forwarded — they are never published, so they never ghost
  either. The cap bounds memory against pathological publishers; a client
  multiplexing more than 16 distinct pids over one connection will see the
  extras silently dropped.
- **5-second handshake deadline.** A connection that does not complete its
  handshake within 5 seconds of connecting is closed and its slot freed.
  Real clients send the handshake immediately, so this only affects probes
  that open the endpoint and go silent (on Windows, a filesystem `exists()`
  on a named pipe opens the pipe).
- **Flood guard.** An identical republish of the same `(application_id,
  pid)` within 5 seconds is dropped before it reaches the broadcast. A
  client that counts frames will count fewer than it published.

### Commands Adsum cannot back

These answer with official-shaped errors instead of hanging:

| Commands | Code | Message |
|---|---|---|
| Activity invites (`SEND_ACTIVITY_JOIN_INVITE`, `CLOSE_ACTIVITY_REQUEST`, `ACCEPT_ACTIVITY_INVITE`, `ACTIVITY_INVITE_USER`) | `5006` | No eligible activity: invites require the real Discord client |
| Voice, guild detail, channels, overlay, store, capture (e.g. `GET_GUILD`, `GET_CHANNELS`, `GET_CHANNEL`, `SELECT_VOICE_CHANNEL`, `SET_VOICE_SETTINGS`, `OVERLAY`, …) | `1000` | requires the real Discord client |
| Anything else unknown | `1000` | Unknown command |

OAuth is handled by the experimental loopback adapter. Its browser flow,
PKCE exchange test and limitations are documented in
[oauth-rpc-social-sdk-testing.md](oauth-rpc-social-sdk-testing.md). The adapter
does not emulate `rpc_token`, provisional accounts, token revocation,
or the authenticated Social SDK service connection. `GET_GUILD`,
`GET_CHANNELS` and `GET_CHANNEL` need live client state that a user OAuth
token cannot reach (Discord rejects those REST endpoints for user tokens),
so they stay in the unsupported table above.

## Events

Only three events are ever dispatched to clients:

- `READY` — on connect (handshake above).
- `ERROR` — for refused commands.
- `CURRENT_USER_UPDATE` — when the identity changes (bridge `SET_USER` /
  `RESET_USER`); game clients see it on their next handshake.

## The bridge side

Everything a game publishes is forwarded to bridge clients (JSON on
1337–1347, MessagePack on 1338+), keyed by `socketId` — the pid for
game-SDK frames, the app id for process-detected cards. Details, including
control messages and delivery behavior, are in
[web-client.md](web-client.md).

## See also

- [process-detection.md](process-detection.md) — games that never connect
  at all (process scanning and the IPC-wins handoff).
- [cli-options.md](cli-options.md) — port and transport flags.
