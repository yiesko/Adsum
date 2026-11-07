//! Experimental OAuth support for Discord's legacy local RPC protocol.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use adsum_types::user::RpcUser;
use serde_json::Value;

mod authenticate;
mod authorize;
mod helpers;
mod manager;
pub mod store;

/// User-Agent sent on every Discord API request. The version is the
/// project-wide one, resolved at compile time from the workspace.
pub(crate) const DISCORD_USER_AGENT: &str = concat!(
  "DiscordBot (https://github.com/yiesko/Adsum, ",
  env!("CARGO_PKG_VERSION"),
  ")"
);

pub use authenticate::{authenticate, exchange_code, fetch_discord_json, refresh_access_token};
pub use authorize::authorize;
pub use manager::{
  BrowserOAuthBackend, OAuthBackend, OAuthCancellation, OAuthConnection, OAuthManager,
  OAuthMetrics, OAuthTask, RpcAuthPolicy,
};
pub use store::PersistedSession;

/// Run the browser authorization with an explicit redirect URI, for callers
/// outside the daemon (the CLI login) that do not read the env var.
pub fn authorize_with_redirect(
  client_id: &str,
  scopes: &[String],
  redirect_uri: &str,
) -> Result<String, OAuthError> {
  authorize::authorize_with_context(
    client_id,
    scopes,
    None,
    None,
    None,
    None,
    Some(redirect_uri),
    &OAuthCancellation::default(),
  )
}

#[must_use]
pub fn require_rpc_authentication() -> bool {
  std::env::var("ADSUM_REQUIRE_RPC_AUTH")
    .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

#[derive(Clone)]
pub struct OAuthSession {
  pub application_id: adsum_types::AppId,
  pub user: Option<RpcUser>,
  pub scopes: Vec<String>,
  pub expires: SystemTime,
  pub access_token: String,
}

impl std::fmt::Debug for OAuthSession {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // The bearer token is a live credential: never render it, not even in
    // debug output that may end up in logs or crash reports.
    f.debug_struct("OAuthSession")
      .field("application_id", &self.application_id)
      .field("user", &self.user)
      .field("scopes", &self.scopes)
      .field("expires", &self.expires)
      .field("access_token", &"[REDACTED]")
      .finish()
  }
}

impl OAuthSession {
  #[must_use]
  pub fn is_expired(&self) -> bool {
    SystemTime::now() >= self.expires
  }
}

#[derive(Clone)]
pub struct RpcOAuthResult {
  pub data: Value,
  pub session: Option<Arc<OAuthSession>>,
}

impl std::fmt::Debug for RpcOAuthResult {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("RpcOAuthResult")
      .field("data", &"[REDACTED]")
      .field("authenticated", &self.session.is_some())
      .finish()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthError {
  MissingClientId,
  InvalidClientId,
  MissingScopes,
  InvalidScopes,
  RedirectNotConfigured,
  InvalidRedirect,
  RpcTokenUnsupported,
  ProvisionalAccountUnsupported,
  InvalidCodeChallenge,
  RandomUnavailable,
  CallbackBindFailed,
  AuthorizationInProgress,
  BrowserUnavailable,
  AuthorizationDenied,
  WrongIssuer,
  AuthorizationTimeout,
  InvalidAccessToken,
  MissingRpcScope,
  DiscordUnavailable,
  UnexpectedDiscordResponse,
  ApplicationMismatch,
  NotAuthenticated,
  InvalidArguments,
  OperationInProgress,
  WorkerUnavailable,
  Cancelled,
  RateLimited,
  ClientSecretRequired,
}

impl std::fmt::Display for OAuthError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let (code, message) = self.rpc_error();
    write!(f, "{message} (code {code})")
  }
}

impl std::error::Error for OAuthError {}

impl OAuthError {
  #[must_use]
  pub const fn rpc_error(self) -> (u16, &'static str) {
    match self {
      Self::MissingClientId => (4007, "No client id provided"),
      Self::InvalidClientId => (4007, "Invalid client id"),
      Self::MissingScopes => (5000, "No OAuth scopes provided"),
      Self::InvalidScopes => (5000, "Invalid OAuth scopes"),
      Self::RedirectNotConfigured => (
        5000,
        "OAuth redirect URI is not configured; set ADSUM_OAUTH_REDIRECT_URI",
      ),
      Self::InvalidRedirect => (
        5000,
        "OAuth redirect URI must be a registered http://127.0.0.1:<port>/callback URL",
      ),
      Self::RpcTokenUnsupported => (
        5000,
        "rpc_token requires Discord's approved RPC-token flow and is not supported by this browser adapter",
      ),
      Self::ProvisionalAccountUnsupported => (
        5000,
        "Provisional accounts require the Social SDK provisional-account flow",
      ),
      Self::InvalidCodeChallenge => (
        5000,
        "Invalid PKCE code challenge; expected an S256 challenge",
      ),
      Self::RandomUnavailable => (5000, "Could not create secure OAuth state"),
      Self::CallbackBindFailed => (5000, "Could not bind the configured OAuth callback address"),
      Self::AuthorizationInProgress => (5000, "Another OAuth authorization is already in progress"),
      Self::BrowserUnavailable => (
        5000,
        "Could not open the system browser for OAuth authorization",
      ),
      Self::AuthorizationDenied => (5000, "Discord authorization was denied or cancelled"),
      Self::WrongIssuer => (5000, "OAuth callback issuer was not Discord"),
      Self::AuthorizationTimeout => (5000, "OAuth authorization timed out"),
      Self::InvalidAccessToken => (4009, "Invalid or expired access token"),
      Self::MissingRpcScope => (
        4006,
        "The OAuth authorization does not include the rpc scope",
      ),
      Self::DiscordUnavailable => (5000, "Could not contact Discord OAuth API"),
      Self::UnexpectedDiscordResponse => (5000, "Discord returned an unexpected OAuth response"),
      Self::ApplicationMismatch => (4007, "Authorization belongs to a different application"),
      Self::NotAuthenticated => (4006, "Not authenticated; call AUTHENTICATE first"),
      Self::InvalidArguments => (4000, "Invalid OAuth command arguments"),
      Self::OperationInProgress => (5000, "An OAuth operation is already in progress"),
      Self::WorkerUnavailable => (5000, "OAuth worker capacity is unavailable; retry later"),
      Self::Cancelled => (5000, "OAuth operation was cancelled"),
      Self::RateLimited => (5000, "Too many OAuth requests; retry later"),
      Self::ClientSecretRequired => (
        5000,
        "Restoring the stored session requires ADSUM_OAUTH_CLIENT_SECRET",
      ),
    }
  }
}

pub fn handle_rpc_command(
  command: &adsum_types::cmd::ActivityCmd,
  expected_client_id: &str,
) -> Result<RpcOAuthResult, OAuthError> {
  let redirect_uri = std::env::var("ADSUM_OAUTH_REDIRECT_URI").ok();
  handle_rpc_command_with_context(
    command,
    expected_client_id,
    redirect_uri.as_deref(),
    &crate::manager::OAuthCancellation::default(),
  )
}

fn handle_rpc_command_with_context(
  command: &adsum_types::cmd::ActivityCmd,
  expected_client_id: &str,
  redirect_uri: Option<&str>,
  cancellation: &crate::manager::OAuthCancellation,
) -> Result<RpcOAuthResult, OAuthError> {
  cancellation.check()?;
  if command
    .application_id
    .as_ref()
    .is_some_and(|id| id.as_ref() != expected_client_id)
  {
    return Err(OAuthError::ApplicationMismatch);
  }
  let args = command.args.as_ref();
  match command.cmd.as_ref() {
    "AUTHORIZE" => {
      let args = args.ok_or(OAuthError::MissingClientId)?;
      let extra = &args.extra;
      let client_id = extra
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or(OAuthError::MissingClientId)?;
      if expected_client_id.is_empty() || client_id != expected_client_id {
        return Err(OAuthError::InvalidClientId);
      }
      let scopes = extra
        .get("scopes")
        .and_then(Value::as_array)
        .ok_or(OAuthError::MissingScopes)?
        .iter()
        .map(|scope| {
          scope
            .as_str()
            .map(str::to_owned)
            .ok_or(OAuthError::InvalidScopes)
        })
        .collect::<Result<Vec<_>, _>>()?;
      let optional_string = |key: &str| match extra.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        _ => Err(OAuthError::InvalidArguments),
      };
      let code = authorize::authorize_with_context(
        client_id,
        &scopes,
        optional_string("rpc_token")?,
        optional_string("username")?,
        optional_string("code_challenge")?,
        optional_string("code_challenge_method")?,
        redirect_uri,
        cancellation,
      )?;
      Ok(RpcOAuthResult {
        data: serde_json::json!({ "code": code }),
        session: None,
      })
    }
    "AUTHENTICATE" => {
      let provided_refresh = args
        .and_then(|a| a.extra.get("refresh_token"))
        .and_then(Value::as_str);
      let provided_access = args
        .and_then(|a| a.extra.get("access_token"))
        .and_then(Value::as_str);

      // Resolve an access token plus the refresh token to persist (if any):
      // - both tokens supplied: validate the access token, store the refresh
      //   token for future restarts only when a client secret is configured;
      // - only a refresh token: exchange it now, which needs the secret;
      // - neither: try to restore the stored session from disk, so a game
      //   that authenticated once never needs the browser flow again.
      let (token, rotated_refresh) = match (provided_access, provided_refresh) {
        (Some(access), refresh) => (
          access.to_owned(),
          refresh
            .filter(|_| store::client_secret().is_some())
            .map(str::to_owned),
        ),
        (None, Some(rt)) => {
          let Some(_secret) = store::client_secret() else {
            return Err(OAuthError::ClientSecretRequired);
          };
          cancellation.check()?;
          let (new_access, new_refresh) =
            authenticate::refresh_access_token(rt, expected_client_id)?;
          // Discord invalidates `rt` as soon as the exchange succeeds, so a
          // rotated token is persisted before the access token is validated.
          let previous = store::load(expected_client_id);
          save_rotated(
            expected_client_id,
            new_refresh.as_deref(),
            previous
              .as_ref()
              .map(|p| p.scopes.clone())
              .unwrap_or_default(),
            previous.as_ref().map_or(0, |p| p.expires),
          );
          (new_access, new_refresh.or_else(|| Some(rt.to_owned())))
        }
        (None, None) => {
          let stored = store::load(expected_client_id);
          let Some(stored) = stored else {
            tracing::info!(
              application = %expected_client_id,
              "[oauth] no token supplied and no stored grant; browser authorization required"
            );
            return Err(OAuthError::InvalidAccessToken);
          };
          let Some(rt) = stored.refresh_token else {
            tracing::info!(
              application = %expected_client_id,
              "[oauth] stored grant has no refresh token; browser authorization required"
            );
            return Err(OAuthError::InvalidAccessToken);
          };
          let Some(_secret) = store::client_secret() else {
            tracing::warn!(
              application = %expected_client_id,
              "[oauth] stored grant found but ADSUM_OAUTH_CLIENT_SECRET is not set"
            );
            return Err(OAuthError::ClientSecretRequired);
          };
          // Restoring a stored grant without proof of possession is opt-in.
          if std::env::var("ADSUM_OAUTH_ALLOW_RESTORE").ok().as_deref() != Some("1") {
            tracing::info!(
              application = %expected_client_id,
              "[oauth] stored grant restore disabled (set ADSUM_OAUTH_ALLOW_RESTORE=1 to enable)"
            );
            return Err(OAuthError::InvalidAccessToken);
          }
          tracing::info!(
            application = %expected_client_id,
            stored_scopes = %stored.scopes.join(" "),
            "[oauth] restoring stored grant after restart"
          );
          cancellation.check()?;
          let (new_access, new_refresh) =
            authenticate::refresh_access_token(&rt, expected_client_id)?;
          save_rotated(
            expected_client_id,
            new_refresh.as_deref(),
            stored.scopes,
            stored.expires,
          );
          (new_access, new_refresh.or(Some(rt)))
        }
      };
      let (data, session) =
        authenticate::authenticate_with_context(&token, expected_client_id, cancellation)?;
      // Persist the (possibly rotated) refresh token for future restarts.
      if let Some(rt) = rotated_refresh {
        let persisted = store::PersistedSession {
          client_id: expected_client_id.to_owned(),
          refresh_token: Some(rt),
          scopes: session.scopes.clone(),
          expires: session
            .expires
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        };
        if store::save(&persisted).is_err() {
          tracing::warn!(
            application = %expected_client_id,
            "[oauth] could not persist the refresh token; the stored grant may be stale"
          );
        }
      }
      Ok(RpcOAuthResult {
        data,
        session: Some(Arc::new(session)),
      })
    }
    _ => Err(OAuthError::UnexpectedDiscordResponse),
  }
}

/// Store a rotated refresh token right after the exchange, so a later
/// failure (network, cancellation, scope check) cannot strand the grant.
fn save_rotated(client_id: &str, refresh: Option<&str>, scopes: Vec<String>, expires: u64) {
  let Some(refresh) = refresh else {
    return;
  };
  if store::save(&store::PersistedSession {
    client_id: client_id.to_owned(),
    refresh_token: Some(refresh.to_owned()),
    scopes,
    expires,
  })
  .is_err()
  {
    tracing::warn!(
      application = %client_id,
      "[oauth] could not persist the rotated refresh token; the stored grant may be stale"
    );
  }
}
