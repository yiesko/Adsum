# Protocol: how games talk to Adsum

Adsum plays the part of the local Discord client. Games connect to it the
way they would connect to Discord, publish Rich Presence, and get
officially-shaped replies. Two transports are served:

- **IPC** — local sockets named `discord-ipc-0` … `discord-ipc-9`. On Linux
  the real socket is preferably bound in
  `$XDG_RUNTIME_DIR/app/com.discordapp.Discord/`, then in the conventional
  bases `XDG_RUNTIME_DIR`, `TMPDIR`, `TMP`, `TEMP`, `/tmp`; symlinks in every
  existing client layout let games find it wherever they probe (see
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

Adsum binds one real `UnixListener` at the first free index `0..=9` in
`$XDG_RUNTIME_DIR/app/com.discordapp.Discord/` — the path the official
Discord Flatpak exposes to sandboxed clients. That directory is created only
when `XDG_RUNTIME_DIR` is an absolute path that normalizes to a real
directory other than `/`, `/tmp`, or `/var/tmp`; the runtime base, `app/`,
and the app directory must all be real directories, not symlinks. If it
cannot be used (or every index there is held by a live socket), Adsum falls
back to the conventional bases in order:

1. `$XDG_RUNTIME_DIR`
2. `$TMPDIR`
3. `$TMP`
4. `$TEMP`
5. `/tmp`

The bound index is then published as a symlink for every **already existing,
real** directory in the list below, under each base (symlinked layouts are
skipped):

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

Only the official Flatpak directory is ever created. Adsum never creates
`app/` for the other clients, `.flatpak/`, or `snap.*`: those aliases appear
only when the client's own directory already exists. Alias publishing never
replaces a real socket, regular file, or live symlink owned by another
service. A dangling ours-shaped link left by a crashed Adsum run is
reclaimed when the same index is published again. Shutdown removes the real
socket and the links for that index, and leaves directories in place.

**Sandbox limitation:** Adsum cannot grant a Flatpak or Snap package access
to a host path. A sandboxed client is served only when its own configuration
already exposes the directory it probes. The PrismLauncher Flatpak case is
covered because it maps the official
`$XDG_RUNTIME_DIR/app/com.discordapp.Discord` directory (`:create`), which is
exactly where Adsum binds first.

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
