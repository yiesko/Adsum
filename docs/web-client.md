# Web client (browser, Vencord, Node)

`plugin/rsrpc.js` is an optional JavaScript client that receives activity
from the rsRPC bridge. It is **not** part of the Rust build: nothing is
bundled with `include_str!` or `build.rs`, and the server works fine
without it. Any arRPC-compatible websocket client can consume the bridge
instead.

## Endpoints

| Port | Protocol | URL |
|---|---|---|
| 1337 | JSON | `ws://127.0.0.1:1337?format=json` |
| 1338 | MessagePack | `ws://127.0.0.1:1338?format=msgpack` |

The server auto-detects the protocol of each connection, so either port
works regardless of the `format` query parameter; `format` accepts `json`,
`msgpack`, or `messagepack`, and an absent or unrecognized value falls back
to the port's own protocol. Ports move when they are taken: the JSON bridge
scans 1337–1347 like arRPC, and the MessagePack port steps forward on
collision. Configure them with `--bridge-port`,
`--bridge-port-end`, and `--msgpack-port` (see
[cli-options.md](cli-options.md)).

## Origin policy

WebSocket has no CORS gate of its own, so the bridge checks the
`Origin` header itself, mirroring the game transport:

- Absent `Origin` (native clients) always passes.
- Discord's pages (`https://discord.com`, `https://canary.discord.com`,
  `https://ptb.discord.com`) pass without extra configuration.
- Anything else is refused at connect (closed before READY) unless
  listed in `--bridge-allowed-origins` /
  `RSRPC_BRIDGE_ALLOWED_ORIGINS` (comma-separated; normalized to
  browser form — lowercase, no trailing slash, no default port —
  then exact match).

## Using `RsRpcClient`

```javascript
const client = new RsRpcClient(false); // false = JSON (default, arRPC-compatible)
client.onActivity = (data) => console.log('Activity:', data);
client.connect();
```

```javascript
const client = new RsRpcClient(true, { // true = MessagePack
    jsonPort: 1337,
    msgpackPort: 1338,
    reconnectInterval: 5000, // ms between reconnect attempts
});
client.onConnect = () => console.log('connected');
client.onActivity = (data) => { /* handle presence */ };
client.onDisconnect = () => console.log('gone');
client.onError = (err) => console.error(err);
client.connect();
// later:
client.disconnect();
```

- **JSON (default)** needs no dependencies and speaks arRPC's format.
- **MessagePack** needs the `@msgpack/msgpack` global:
  `<script src="https://unpkg.com/@msgpack/msgpack"></script>`. If the
  library is missing, the client quietly falls back to JSON.
- The client reconnects automatically after a disconnect
  (`reconnectInterval`, default 5000 ms).

## Message format

**The first message on every connection is not an activity.** Once the origin
check passes, the bridge sends a `READY` payload with the user identity
before anything else, and only then replays whatever cards are cached. The
bundled `RsRpcClient` hands *every* frame to `onActivity`, so a consumer
written from this page alone receives that identity object first and has to
tolerate it:

```json
{ "evt": "READY", "data": { "id": "…", "username": "…", "discriminator": "0",
                            "global_name": "…", "avatar": null, "bot": false,
                            "flags": 0, "premium_type": 0, "nonce": null } }
```

A `READY` frame has no `activity` field. Check for it (`if (!data.activity)
return;`) rather than assuming the first callback carries a game.

After that, each activity message is an object with three fields:

```json
{ "activity": { "name": "My Game", "details": "In a match" }, "pid": 1234, "socketId": "1234" }
```

- `activity` — the Rich Presence activity, or `null` for a clear.
- `pid` — the process that published it. Always present as a key; `null`
  when the publisher did not send one.
- `socketId` — who published it: the pid for game-SDK frames, the app id
  for process-detected cards.

MessagePack frames carry the same field names. A clear is `activity: null`
on an otherwise normal frame — it is not a separate message type, and the
`pid`/`socketId` still tell you what was cleared.

Frames you send back are echoed to you and not forwarded, and bridge
clients do not publish presence: only `SET_USER`/`RESET_USER` are treated as
control, and anything else returns as an echo. Binary frames echo like text
ones.

## Vencord plugin and userscript

When `Vencord` is present, `rsrpc.js` registers itself as a plugin named
**rsRPC** ("High-performance Discord RPC bridge with optional MessagePack").
Its only setting, `useMsgPack`, toggles the MessagePack transport
(marked experimental). On each activity it forwards the data into
Discord's local presence via `setLocalPresence`.

The same file works in a browser console or as a userscript — open it, run
`new RsRpcClient(...)` as above, and read `onActivity`.

## Node and bundlers

```javascript
const { RsRpcClient } = require('./plugin/rsrpc.js');
```

(Node needs a global `WebSocket`, which recent Node versions provide.)

## Bridge control messages

These arrive on the JSON port as plain text frames:

- `{"type":"SET_USER","patch":{...}}` — patch the identity rsRPC reports.
  Only these keys are accepted: `id`, `username`, `global_name`,
  `discriminator` (strings; blank values ignored), `avatar` (a string, or
  `null` to clear), `bot`, `flags`, `premium_type`. Acknowledged with
  `SET_USER_ACK`.
- `{"type":"RESET_USER"}` — restore the startup identity (defaults plus
  `RSRPC_USER_*`). Acknowledged with `RESET_USER_ACK`.

Both replies look like
`{"type":"SET_USER_ACK","nonce":...,"data":{"success":true,"user":{...}}}`
— the `nonce` you sent is echoed back. The patch itself is read from
`patch` (arRPC's spelling) or, equivalently, `data`. Identity changes are
broadcast to all bridge clients as the official `CURRENT_USER_UPDATE`
DISPATCH; game clients see the new identity on their next handshake.

Other frames — text and binary alike — are echoed back to the sender.

## Delivery behavior

- **Ready first** — every connection opens with the identity `READY` frame
  described above, before any replay.
- **Replay** — clients that connect late immediately receive the cached
  activities (arRPC parity, up to 50 entries).
- **Refresh** — cached activities are rebroadcast every 30 seconds.
- **Flood guard** — an identical republish of the same `(application_id,
  pid)` within 5 seconds is dropped, and never reaches the broadcast at all.
  A client that watches for every frame will see fewer than it published.
- **Ghost reaping** — caches owned by dead pids are dropped when the scanner
  reports an empty table, so a reaped card implies the process list went
  empty rather than one process exiting.

## See also

- [protocol.md](protocol.md) — what the game-facing transports do.
- [cli-options.md](cli-options.md) — ports and identity variables.
