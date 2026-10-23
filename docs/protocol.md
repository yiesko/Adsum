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
  directory involved at all.
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
Relative, empty, root (`/`), symlinked, non-directory, foreign-owned, and
non-sticky world-writable paths are rejected, and rejection never
authorizes deleting the path.

### Socket permissions

Immediately after a successful bind, Adsum sets the real socket to mode
`0600` and verifies the mode before publishing any alias or marker. If the
mode cannot be set or verified, that bind attempt is cleaned up with the
same identity-checked removal as other owned-socket cleanup, and the next
index is tried.

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
| `GET_USER` | Returns the current identity, or `null` when the id names somebody else (a missing id resolves to self). |
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

### Limits that change what a client observes

- **16 tracked pids per connection.** Beyond the cap, new pids are refused
  before being forwarded — they are never published, so they never ghost
  either. The cap bounds memory against pathological publishers; a client
  multiplexing more than 16 distinct pids over one connection will see the
  extras silently dropped.
- **Flood guard.** An identical republish of the same `(application_id,
  pid)` within 5 seconds is dropped before it reaches the broadcast. A
  client that counts frames will count fewer than it published.

### Commands Adsum cannot back

These answer with official-shaped errors instead of hanging:

| Commands | Code | Message |
|---|---|---|
| `AUTHORIZE`, `AUTHENTICATE` | `5000` | Authorization requires the real Discord client |
| Activity invites (`SEND_ACTIVITY_JOIN_INVITE`, `CLOSE_ACTIVITY_REQUEST`, `ACCEPT_ACTIVITY_INVITE`, `ACTIVITY_INVITE_USER`) | `5006` | No eligible activity: invites require the real Discord client |
| Voice, guilds, channels, overlay, store, capture (e.g. `GET_GUILD`, `SELECT_VOICE_CHANNEL`, `SET_VOICE_SETTINGS`, `OVERLAY`, …) | `1000` | requires the real Discord client |
| Anything else unknown | `1000` | Unknown command |

Why: OAuth needs the real client's modal and the app's `client_secret`,
and the other commands need live client state that only Discord itself
has. The README's known-limitations section explains what that means for
games that try to log in over RPC.

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
