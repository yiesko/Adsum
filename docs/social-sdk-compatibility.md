# Discord Social SDK compatibility in Adsum

## Supported boundary

Adsum is a Rust implementation of Discord's local RPC transport and Rich
Presence bridge. It can be used when a game/Social SDK build chooses the
Discord Desktop RPC route for Rich Presence. The game sends `SET_ACTIVITY`,
and Adsum forwards the presence through its JSON and MessagePack bridges.

The experimental OAuth implementation adds these legacy RPC operations.
It is an independent implementation of the legacy RPC OAuth commands; Adsum
does not bundle, wrap, or implement Discord's native Social SDK.

| RPC operation | Status | Notes |
|---|---|---|
| `SET_ACTIVITY` | Supported | Existing Adsum feature; activity DTO preserves unknown future fields. |
| `AUTHORIZE` | Experimental | Browser-based OAuth2 authorization-code flow, random state and optional PKCE S256. Returns authorization code to caller. |
| `AUTHENTICATE` | Experimental | Requires the `rpc` scope, validates bearer token via `/oauth2/@me`, checks app ID, stores user/scopes/expiry per connection, and keeps the access token in memory only. With `ADSUM_OAUTH_CLIENT_SECRET` set, a supplied or stored refresh token is exchanged for a fresh access token without a browser flow. |
| `GET_USER` after `AUTHENTICATE` | Experimental | Returns authenticated user if the `identify` scope supplied a user object. |
| OAuth over WebSocket | Experimental | Same `AUTHORIZE`/`AUTHENTICATE` flow as IPC; the connection query must include `client_id`. |
| Strict RPC gate | Opt-in | `ADSUM_REQUIRE_RPC_AUTH=1` rejects other RPC commands until the connection authenticates. Off by default for backwards compatibility and unauthenticated Rich Presence. |

## Not equivalent to the native Social SDK

These operations remain outside Adsum's implementation:

- Native `discordpp::Client::Authorize` callback lifecycle and overlay handling.
- `Client::GetToken`, `UpdateToken`, refresh lifecycle and SDK session setup.
- `Client::Connect` and its authenticated service connection.
- Friends/relationships, direct messages, game invites, lobbies, linked
  channels, voice/RTC and the SDK event/callback model.
- Provisional accounts and external identity-provider flows.

They are not RPC command handlers. The official SDK is a native C++ library
with its own API and service protocol. An RPC-compatible server cannot become
a drop-in implementation of that library just by answering more local IPC
commands.

## Validation plan

1. Run the daemon with the registered loopback redirect and drive the flow
   from a game or RPC test client: `AUTHORIZE` (browser consent, PKCE),
   code exchange at the standard token endpoint, `AUTHENTICATE`, then
   authenticated `GET_USER` on the same connection.
2. Exercise the WebSocket transport with the exact game WebSocket port and
   the `client_id` connect query — the same flow over WS.
3. With `ADSUM_OAUTH_CLIENT_SECRET` set, verify restart restore: after a
   daemon restart, `AUTHENTICATE` with no token must bring the session back
   without a browser.
4. Keep commands that require Discord desktop-client state or additional API
   scopes unsupported until their behavior, permission model, response
   mapping, session lifecycle and integration tests are implemented.

Do not treat a successful OAuth RPC test as support for friends, direct
messages, lobbies, voice, or other social services. Those features are outside
Adsum's current scope; the native Social SDK is intentionally not a dependency
or implementation target.
