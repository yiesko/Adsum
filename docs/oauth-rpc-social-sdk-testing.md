# OAuth RPC test guide (Adsum)

This guide tests Discord's **legacy local RPC** commands `AUTHORIZE` and
`AUTHENTICATE` against a running Adsum. It does not turn Adsum into the
native C++ Social SDK client.

## 1. What the test proves

A client connecting to Adsum's `discord-ipc-0` socket runs this sequence:

```text
HANDSHAKE(client_id)
  -> AUTHORIZE(scopes=[rpc, identify], PKCE S256 challenge)
  -> browser consent + loopback callback
  -> authorization code
  -> OAuth2 code exchange (confidential app; outside Adsum)
  -> AUTHENTICATE(access_token)
  -> GET_USER(authenticated connection identity)
```

The code exchange is deliberately outside Adsum. The RPC protocol returns an
authorization code; the calling game/application exchanges it for a token and
then sends `AUTHENTICATE`. Adsum validates the access token at Discord's
`/api/v10/oauth2/@me` endpoint, keeps user/scopes/expiry in memory for that
connection, and serves token-backed commands (`GET_GUILDS`) from it.

## 2. Developer Portal setup

1. Create an application at the Discord Developer Portal. Copy its
   **Application ID** and **Client Secret**.
2. In OAuth2 redirect settings, register this exact URI:
   `http://127.0.0.1:48152/callback`.
3. Ensure your Discord account is allowed to use the application's RPC
   integration. Discord documents a tester list for apps that have not yet
   been approved (up to 50 testers).
4. Keep the application confidential: the client secret is used only by the
   component that exchanges codes — never sent to Adsum, never stored in
   Adsum's configuration unless you explicitly enable refresh-token
   persistence (section 7).

Do not use another developer's application secret or a production user's
access token.

### Why a client secret is needed

Discord's regular OAuth2 token endpoint requires client authentication. The
Social SDK's **Public Client** path is different: an app with Public Client
enabled uses `discordpp::Client::GetToken(...)` to exchange the code without
a backend. That SDK method is not the same as making an anonymous POST to the
standard token endpoint. This guide intentionally exercises the
confidential-client API flow and does not claim to reproduce
`discordpp::Client::GetToken`.

## 3. Build and start Adsum

Build from a checkout of this repository on a clean feature branch.

On Fedora KDE, for the first test run Adsum in the foreground from the
terminal. This gives `xdg-open` access to the current desktop session and
avoids accidentally connecting to a second Adsum instance.

In your Adsum repository, run:

```fish
cargo build --release -p adsum-cli
```

Then start it with the redirect URI configured:

```fish
ADSUM_OAUTH_REDIRECT_URI="http://127.0.0.1:48152/callback" ./target/release/adsum
```

Note the IPC socket path in the startup log (the test client below connects
to it).

## 4. Authorize and authenticate

With the redirect URI configured and the daemon running, the authorization
flow is driven by the game (or by your own test client): it sends
`AUTHORIZE` over IPC, the browser opens, and the returned code is exchanged
at the standard token endpoint with the registered redirect URI, PKCE
verifier and client credentials. The resulting access token goes back to
Adsum through `AUTHENTICATE`, which validates it against `/oauth2/@me` and
answers with the authenticated user; `GET_USER` on the same connection then
returns that identity.

Client secrets, authorization codes, access tokens and refresh tokens must
never be logged or written to disk by anything in this flow.

To verify a successful run without writing a client, the journal answers
everything:

```fish
journalctl --user -u adsum.service | grep -i oauth
```

- `authorization code issued` — a game completed the browser authorization.
- `authenticated user_id=… username=… application=… scopes=…` — a connection
  authenticated successfully.
- `AUTHENTICATE failed … error=InvalidAccessToken` — the token was rejected;
  the game should start a fresh `AUTHORIZE`.

## 5. Is Equibop required?

**No, not for the OAuth test.** The game (or any RPC client) is the one that
sends `AUTHORIZE` and `AUTHENTICATE`; Equibop is not responsible for those
commands.

Equibop is relevant to the separate Rich Presence bridge path. To have it
consume activities from Adsum, keep its built-in server disabled while
leaving Rich Presence enabled:

```json
{
  "arRPC": false,
  "arRPCDisabled": false
}
```

Adsum's JSON bridge is normally `ws://127.0.0.1:1337`. This setting connects
Equibop to that bridge; it does not make Equibop an OAuth test client.

Do not run the official Discord desktop client as a competing IPC server
while testing Adsum. Both can contend for the conventional `discord-ipc-0`
path. After the test, stop foreground Adsum with `Ctrl+C` and restart the
installed service when needed:

```fish
systemctl --user start adsum.service
```

## 6. Refresh tokens and persistence (optional)

With the client secret exported as an environment variable, Adsum can renew
grants without a second browser visit:

```fish
set -x ADSUM_OAUTH_CLIENT_SECRET "your-client-secret"
```

- Pass `refresh_token` (in addition to or instead of `access_token`) in the
  `AUTHENTICATE` arguments. Adsum exchanges it at the standard token endpoint
  and stores the rotated token under `~/.cache/adsum/oauth/<client id>.json`
  with mode 0600.
- After restarting Adsum, call `AUTHENTICATE` with no token at all. Adsum
  restores the stored grant the same way, so the browser flow is needed only
  once per grant.
- Without the secret, both paths fail with an explicit
  "requires ADSUM_OAUTH_CLIENT_SECRET" error, and nothing is stored.
- Delete the stored file to force a fresh browser authorization.

The refresh exchange uses the same confidential-client endpoint the code
exchange uses; the secret belongs in Adsum's environment, never on the RPC
wire.

## 7. Common failures

| Result | Meaning / action |
|---|---|
| `4007 Invalid client id` | `client_id` in `AUTHORIZE` does not match the IPC handshake, or the token belongs to another application. |
| RPC authorization denied | The app may not be approved and your Discord account may not be on its tester list. |
| Callback bind error | Port 48152 is occupied or the callback URI is not the configured loopback URI. |
| State mismatch | The callback does not belong to this pending authorization; it is rejected. Restart the test. |
| Token endpoint rejects exchange | Check the client secret, registered redirect URI and PKCE verifier. |
| `4009 Invalid token` | The token is invalid, expired or revoked. Authenticate again. |
| `4006` in strict mode | The connection has not authenticated, or lacks the required `rpc` scope. |
| `GET_USER` returns null | The token did not grant `identify`, so the response has no user object. |
| Browser does not open | Run Adsum in the foreground from your KDE session and check that `xdg-open` is installed. |

## 8. What this does not test

This proves the legacy RPC OAuth path only. It does not prove that the native
C++ Social SDK's `discordpp::Client::Authorize`, `GetToken`, `UpdateToken` or
`Connect` work, and it does not make Adsum a drop-in replacement for that
SDK. Friends, direct messages, lobbies, voice and the authenticated Social
SDK service connection remain out of scope — see
[Social SDK compatibility](social-sdk-compatibility.md).
