# OAuth RPC implementation and Social SDK boundary

This crate implements the useful OAuth portion of Discord's **legacy local
RPC protocol**, not the full Discord Social SDK.

This feature is experimental. The legacy RPC surface is large, and most of
its commands need live state from the Discord desktop client, which a
separate local service cannot provide. Adsum implements only what it can
support correctly: browser-based authorization, session authentication,
identity, and the one data query that a user OAuth token can serve. Anything
else stays in the unsupported list instead of faking a success response.

Contributions are welcome. Please test changes end to end, keep them useful
to users, and keep them simple. A command that cannot be supported properly
should stay unsupported.

## RPC `AUTHORIZE`

- Reads `client_id` and `scopes` from RPC args and requires the application ID
to match the IPC handshake or WebSocket connect `client_id`.
- Accepts an optional PKCE S256 `code_challenge` and
  `code_challenge_method: "S256"` extension. The RPC caller retains the
  verifier; Adsum never sees it.
- Builds Discord's standard OAuth2 authorization URL, opens the system
  browser, binds only to IPv4 loopback, and validates a cryptographically
  random `state` in the callback.
- Returns `{ "code": "..." }` in the normal RPC response envelope.
- Does not exchange the code. The calling application or its backend owns
  that step, exactly as the RPC protocol requires.

`ADSUM_OAUTH_REDIRECT_URI` must exactly match an OAuth2 redirect URI registered
in the application's Developer Portal settings. Use an unprivileged local
port, for example `http://127.0.0.1:48152/callback`. Only one browser callback
can be active at once because this prototype uses one configured callback.
The callback also validates Discord's `iss=https://discord.com` parameter.

For the IPC transport, the application ID is bound by the `discord-ipc-N`
handshake. For the WebSocket game transport, OAuth requires the same ID in the
connect query: `/?v=1&encoding=json&client_id=YOUR_APPLICATION_ID`.

## RPC `AUTHENTICATE`

- Accepts `args.access_token`.
- Calls Discord `GET /api/v10/oauth2/@me` with a Bearer header.
- Validates that `application.id` equals the connection's handshake
  application ID.
- Returns Discord's response object, including `application`, `scopes`,
  `expires`, and `user` when the `identify` scope is present.
- Requires the granted `rpc` scope and validates that the application ID
  matches the IPC/WS connection's application ID.
- Creates a per-connection in-memory session containing the returned
  user identity, scopes, and expiry. The access token stays in memory for
  that session only; it is never logged, serialized, or written to disk.
- Accepts an optional `args.refresh_token`. When `ADSUM_OAUTH_CLIENT_SECRET`
  is set, a refresh token (supplied or previously stored) is exchanged for a
  fresh access token without a browser flow, and the rotated refresh token
  is persisted under `~/.cache/adsum/oauth/<client_id>.json` with mode
  0600. Calling `AUTHENTICATE` with no token at all attempts the same
  restore, so a game that authenticated once does not need the browser
  flow again.
- `GET_USER` then returns the authenticated user on that same connection.
  An expired session is cleared when accessed.
- `ADSUM_REQUIRE_RPC_AUTH=1` optionally gates all RPC commands other than
  `AUTHORIZE`/`AUTHENTICATE` until the connection has a valid session. It is
  off by default to preserve Adsum's pre-existing Rich Presence compatibility
  and Social SDK's unauthenticated RPC presence path.

The session is not a general Discord API client: Adsum keeps the access
token in memory but does not expose guild, channel, message or relationship
endpoints. Those RPC commands remain explicitly unsupported rather than
returning fabricated success responses. `GET_GUILDS` is the one
token-backed command Adsum implements (proxied to `GET /users/@me/guilds`
under the `guilds` scope).
`GET_GUILD`, `GET_CHANNELS` and `GET_CHANNEL` stay unsupported: Discord's
REST endpoints for those reject user OAuth tokens, and the live answers come
from the desktop client's own state.

## Refresh-token persistence

When `ADSUM_OAUTH_CLIENT_SECRET` is set, the adapter can hold the grant
across restarts:

- `AUTHENTICATE` with `args.refresh_token` exchanges it for a fresh access
  token, then stores the rotated refresh token.
- `AUTHENTICATE` with no token restores the previously stored grant the same
  way.
- Storage is one JSON file per application under
  `~/.cache/adsum/oauth/` (mode 0600, directory 0700). Access tokens are
  never written to disk.
- Without the secret, refresh and restore are refused with an explicit
  error; nothing is stored and the browser flow keeps working as before.

## Operator setup (installed service)

Nothing is required for Adsum to run: OAuth is dormant until configured, and
Rich Presence works without it. A game that sends `AUTHORIZE` on an
unconfigured daemon gets the explicit error
"OAuth redirect URI is not configured; set `ADSUM_OAUTH_REDIRECT_URI`".

### Who registers the redirect URI

The `client_id` in `AUTHORIZE` belongs to the **game's** application. Discord
only accepts redirect URIs that the application owner registered in the
Developer Portal, so the setup depends on who owns the application:

- **You develop the game.** Register the loopback URI on your application's
  OAuth2 settings page, then configure Adsum with exactly that URI. The
  client secret is also yours, so you can enable refresh-token persistence.
- **You only play someone else's game.** The game's developer must have
  registered a loopback redirect and told you what it is. Configure Adsum
  with that exact URI. The browser flow works without their secret, but
  refresh and restart restore do not. Never put another developer's client
  secret in your environment.

Discord also limits RPC access for unapproved applications to an explicit
tester list (up to 50 testers), managed on the application's page.

### Configuring the service

The systemd service does not inherit a terminal's environment, so OAuth
settings go in a drop-in, not in the shell:

```bash
mkdir -p ~/.config/systemd/user/adsum.service.d
cat > ~/.config/systemd/user/adsum.service.d/oauth.conf <<'EOF'
[Service]
# Must match the redirect URI registered in the application's Developer
# Portal settings, byte for byte. Unprivileged port, not one of Adsum's own
# listeners (6463-6472, 1337, 1338).
Environment=ADSUM_OAUTH_REDIRECT_URI=http://127.0.0.1:48152/callback
EOF
systemctl --user daemon-reload
systemctl --user restart adsum.service
```

Then log in once from a terminal (which has the display; the daemon itself
never opens a browser) and restart the service so it picks up the grant:

```bash
export ADSUM_OAUTH_CLIENT_SECRET="your client secret"   # optional; see below
adsum oauth login --client-id <application id> \
  --redirect-uri http://127.0.0.1:48152/callback
systemctl --user restart adsum.service
```

`oauth login` opens the browser, exchanges and validates the code, stores
the grant under `~/.cache/adsum/oauth/`, and prints the signed-in identity.
`oauth status` lists stored grants; `oauth logout` removes them.

Foreground runs skip the drop-in and use plain environment variables:

```bash
ADSUM_OAUTH_REDIRECT_URI="http://127.0.0.1:48152/callback" cargo run -p adsum-cli
```

Optional additions to the same drop-in:

| Variable | When to add it |
|---|---|
| `ADSUM_OAUTH_CLIENT_SECRET` | You own the application and want refresh tokens stored and sessions restored after restarts. Keep it out of files other people can read. |
| `ADSUM_REQUIRE_RPC_AUTH=1` | You want every command other than `AUTHORIZE`/`AUTHENTICATE` to require an authenticated connection. Off by default; legacy Rich Presence clients stay unauthenticated. |
| `ADSUM_OAUTH_TIMEOUT` | The default 180-second browser wait does not suit your environment. |
| `ADSUM_OAUTH_RATE_LIMIT_MS` | The default 250 ms per-connection spacing between OAuth commands does not suit your load. `0` disables it. |

A game can also start `AUTHORIZE` itself, and the daemon will try to open
the browser. The service environment usually has no display, so that path
works best in foreground runs; the supported setup path is `adsum oauth
login` from your terminal.

### Verifying the setup

After a restart, the journal states what Adsum found and what happened:

```bash
journalctl --user -u adsum.service | grep -i oauth
```

- `persisted OAuth grant application=… scopes=…` — a stored grant was found.
  `refresh_stored=true` means it can be restored when the client secret is
  set. A warning about file permissions means an older Adsum wrote the file
  with loose permissions; one re-authentication rewrites it with mode 0600.
- `authorization code issued` — a game completed the browser authorization.
- `authenticated user_id=… username=… application=… scopes=…` — a connection
  authenticated successfully.
- `AUTHENTICATE failed … error=InvalidAccessToken` — the token was rejected.
  The game should start a fresh `AUTHORIZE`.
- `stored grant found but ADSUM_OAUTH_CLIENT_SECRET is not set` — a game
  tried to restore a session, but the secret is missing from the
  environment.

To revoke everything and force a fresh browser authorization, run
`adsum oauth logout --all` (or `adsum oauth logout <application id>`), then
restart the service. Deleting the file by hand works too:

```bash
rm ~/.cache/adsum/oauth/<application id>.json
systemctl --user restart adsum.service
```

## What OAuth changes for clients

The transports are local sockets that speak a public protocol. Any local
process can connect, not only games: launchers, editors, music players,
scripts, and browser clients through the bridge. Authentication changes what
those clients can report and ask for:

- **Without authentication**, the working surface is `SET_ACTIVITY` plus a
  placeholder identity. `GET_USER` reports the configured fallback user,
  which is a label the operator sets, not a fact about anyone.
- **With an authenticated session**, `GET_USER` returns the identity Discord
  verified for the token, and `GET_GUILDS` returns the grant owner's servers
  if the `guilds` scope was granted. With the application secret configured,
  the session also survives daemon restarts.

Adsum grants nothing itself. Discord issues the scopes at consent time, and
Adsum enforces what the token carries: the application ID must match the
connection, `GET_GUILDS` requires the `guilds` scope, and strict mode
(`ADSUM_REQUIRE_RPC_AUTH=1`) can require authentication for every other
command.

## Use cases

- **Bind a local player to a Discord account.** A game backend runs the
  documented flow (authorize, exchange the code, authenticate) to learn
  which Discord account the local player is. This is the pattern Discord's
  own game-integration samples describe.
- **Companion apps and launchers.** A launcher can greet the signed-in user
  by name with the right avatar, because `GET_USER` is verified, without
  building its own login.
- **Community-aware tooling.** A mod manager can check `GET_GUILDS` for
  membership in its project's server before enabling community features.
  The answer comes from Discord, not from the client.
- **Presence for non-game applications.** Editors, players, and scripts can
  publish Rich Presence and, when authenticated, attach a real identity to
  it instead of the placeholder user.
- **Shared machines.** Strict gating lets an operator require authentication
  before presence is accepted, so only authorized applications drive the
  public profile.

Two boundaries apply to every case: the authenticated identity is the
**grant owner** (see [Identity and account switches](#identity-and-account-switches)
for what happens when the desktop client changes accounts), and OAuth adds
no control over the desktop client itself — voice, channels, invites, and
messages stay outside this surface.

## What is deliberately not implemented

- `rpc_token`: Discord documents this as a one-time RPC credential minted at
  `/oauth2/token/rpc` using an application's client secret and granted only to
  approved/tester applications. Adsum must not contain a third-party app's
  client secret. A game developer may mint this on a secure backend, but the
  official client consumes it as part of the official RPC authorization flow;
  this independent browser adapter rejects `rpc_token` explicitly instead of
  silently ignoring it.
- `username` / provisional account creation: this is a separate Social SDK
  provisional-account / external-auth flow, not a normal legacy RPC OAuth
  username parameter that can safely be implemented by this local server.
- Token revocation and a backend token broker. The test helper exchanges
  PKCE codes using a confidential client's secret, but Adsum intentionally
  does not become the token owner beyond the documented refresh exchange.
  Public Client applications should use the official
  `discordpp::Client::GetToken`.
- Token-backed guild and channel detail queries, messages, relationships,
  voice, invites, lobbies and event subscriptions. Authentication metadata
  alone cannot recreate state and services held by the real Discord desktop
  client.

## Identity and account switches

The session's user is the **grant owner**: the account that approved the
authorization in the browser. If the Discord desktop client later switches
to a different account, the session keeps answering for the original one.
The token is still valid, and nothing in the local RPC protocol announces
the switch. Adsum does not detect it and does not guess.

To re-bind a connection after a switch, authenticate again with a fresh
`AUTHORIZE` (the browser follows whatever account is logged in at
`discord.com`) or with a new `access_token`. To make sure a restart cannot
silently restore the old account, delete
`~/.cache/adsum/oauth/<client id>.json` (or `adsum oauth logout <client id>`).

The fallback identity (what `READY` and unauthenticated `GET_USER` report)
is separate. Bridge clients update it with the `SET_USER` control message.
That channel exists for client-side integrations such as Equibop, which run
inside the Discord client and know the active account. `SET_USER` never
changes OAuth sessions, and OAuth sessions never patch the fallback
identity.

## Social SDK distinction

The native Social SDK APIs `discordpp::Client::Authorize`, `GetToken`,
`UpdateToken` and `Connect` are C++ library operations. The SDK handles its own
authorization callback lifecycle and, after `Connect`, establishes its own
service connection for presence, friends, communication, lobbies and voice.
Those are not legacy RPC commands that Adsum can implement by adding
`AUTHORIZE` and `AUTHENTICATE` to a local socket.

The viable Adsum compatibility surface is the desktop RPC path used by games
that publish Rich Presence through the Discord desktop RPC fallback. Adsum
already supports the `SET_ACTIVITY` transport for this use case. Passing the
native authenticated Social SDK `Connect` flow still requires the official
SDK runtime and its network/service protocol.

See [the live OAuth RPC test guide](oauth-rpc-social-sdk-testing.md) and
[Social SDK compatibility](social-sdk-compatibility.md).
