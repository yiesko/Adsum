#![allow(dead_code)]
//! Token validation against Discord's /oauth2/@me and session construction.

use std::io::Read;
use std::time::{Duration, SystemTime};

use adsum_types::user::RpcUser;
use serde_json::Value;

use crate::helpers::parse_rfc3339;
use crate::manager::OAuthCancellation;
use crate::store;
use crate::{OAuthError, OAuthSession};

const MAX_ACCESS_TOKEN: usize = 8 * 1024;
const DISCORD_AUTH_INFO_URL: &str = "https://discord.com/api/v10/oauth2/@me";

pub fn authenticate(
  access_token: &str,
  expected_client_id: &str,
) -> Result<(Value, OAuthSession), OAuthError> {
  authenticate_with_context(
    access_token,
    expected_client_id,
    &OAuthCancellation::default(),
  )
}

pub(crate) fn authenticate_with_context(
  access_token: &str,
  expected_client_id: &str,
  cancellation: &OAuthCancellation,
) -> Result<(Value, OAuthSession), OAuthError> {
  cancellation.check()?;
  crate::authorize::validate_client_id(expected_client_id)?;
  if access_token.is_empty()
    || access_token.len() > MAX_ACCESS_TOKEN
    || !access_token
      .bytes()
      .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\')
  {
    return Err(OAuthError::InvalidAccessToken);
  }

  // A fresh agent avoids retaining a bearer token in a daemon-wide client
  // cache. The Authorization header is never logged.
  let agent: ureq::Agent = ureq::Agent::config_builder()
    .timeout_global(Some(Duration::from_secs(12)))
    .max_redirects(0)
    .build()
    .into();
  let authorization_header = format!("Bearer {access_token}");
  let response = match agent
    .get(DISCORD_AUTH_INFO_URL)
    .header("Authorization", authorization_header)
    .header("User-Agent", crate::DISCORD_USER_AGENT)
    .header("Accept", "application/json")
    .call()
  {
    Ok(response) => response,
    Err(ureq::Error::StatusCode(401 | 403)) => {
      return Err(OAuthError::InvalidAccessToken);
    }
    Err(_) => return Err(OAuthError::DiscordUnavailable),
  };
  cancellation.check()?;
  let mut body = String::new();
  response
    .into_body()
    .as_reader()
    .take(65_537)
    .read_to_string(&mut body)
    .map_err(|_| OAuthError::DiscordUnavailable)?;
  cancellation.check()?;
  if body.len() > 65_536 {
    return Err(OAuthError::UnexpectedDiscordResponse);
  }
  let mut data: Value =
    serde_json::from_str(&body).map_err(|_| OAuthError::UnexpectedDiscordResponse)?;

  let session = session_from_auth_response(&data, expected_client_id, access_token)?;
  if !session.scopes.iter().any(|scope| scope == "identify") {
    data["user"] = Value::Null;
  }
  Ok((data, session))
}

/// Parse and validate Discord's `/oauth2/@me` response independently of I/O.
pub(crate) fn session_from_auth_response(
  data: &Value,
  expected_client_id: &str,
  access_token: &str,
) -> Result<OAuthSession, OAuthError> {
  let application_id = data
    .get("application")
    .and_then(|application| application.get("id"))
    .and_then(Value::as_str)
    .ok_or(OAuthError::UnexpectedDiscordResponse)?;
  if application_id != expected_client_id {
    return Err(OAuthError::ApplicationMismatch);
  }

  let scopes = data
    .get("scopes")
    .and_then(Value::as_array)
    .ok_or(OAuthError::UnexpectedDiscordResponse)?
    .iter()
    .map(|scope| {
      scope
        .as_str()
        .map(str::to_owned)
        .ok_or(OAuthError::UnexpectedDiscordResponse)
    })
    .collect::<Result<Vec<_>, _>>()?;
  if !scopes.iter().any(|scope| scope == "rpc") {
    return Err(OAuthError::MissingRpcScope);
  }

  let expires = data
    .get("expires")
    .and_then(Value::as_str)
    .ok_or(OAuthError::UnexpectedDiscordResponse)?;
  let expires_at = parse_rfc3339(expires).ok_or(OAuthError::UnexpectedDiscordResponse)?;
  if expires_at <= SystemTime::now() {
    return Err(OAuthError::InvalidAccessToken);
  }
  let user = match data.get("user") {
    Some(user @ Value::Object(_)) => Some(parse_rpc_user(user)?),
    Some(Value::Null) | None => None,
    _ => return Err(OAuthError::UnexpectedDiscordResponse),
  };

  let user = scopes
    .iter()
    .any(|scope| scope == "identify")
    .then_some(user)
    .flatten();
  Ok(OAuthSession {
    application_id: adsum_types::AppId::from(application_id),
    user,
    scopes,
    expires: expires_at,
    access_token: access_token.to_owned(),
  })
}

/// Fetch JSON from Discord's REST API with a bearer token.
///
/// Returns the parsed body on 2xx, `InvalidAccessToken` on 401/403 and
/// `DiscordUnavailable` on transport/other errors. The token is never logged.
pub fn fetch_discord_json(access_token: &str, path: &str) -> Result<Value, OAuthError> {
  if !path.starts_with('/')
    || path.starts_with("//")
    || path.contains("..")
    || !path
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || b"/-_.?=&%@".contains(&byte))
  {
    return Err(OAuthError::InvalidArguments);
  }
  let agent: ureq::Agent = ureq::Agent::config_builder()
    .timeout_global(Some(Duration::from_secs(8)))
    .max_redirects(0)
    .build()
    .into();
  let authorization = format!("Bearer {access_token}");
  let url = format!("https://discord.com/api/v10{path}");
  let response = match agent
    .get(&url)
    .header("Authorization", authorization)
    .header("User-Agent", crate::DISCORD_USER_AGENT)
    .header("Accept", "application/json")
    .call()
  {
    Ok(response) => response,
    Err(ureq::Error::StatusCode(401 | 403)) => return Err(OAuthError::InvalidAccessToken),
    Err(_) => return Err(OAuthError::DiscordUnavailable),
  };
  let mut body = String::new();
  response
    .into_body()
    .as_reader()
    .take(1_048_577)
    .read_to_string(&mut body)
    .map_err(|_| OAuthError::DiscordUnavailable)?;
  if body.len() > 1_048_576 {
    return Err(OAuthError::UnexpectedDiscordResponse);
  }
  serde_json::from_str(&body).map_err(|_| OAuthError::UnexpectedDiscordResponse)
}

/// Exchange a refresh token for a new access token (requires client secret).
pub fn refresh_access_token(
  refresh_token: &str,
  client_id: &str,
) -> Result<(String, Option<String>), OAuthError> {
  let secret = store::client_secret().ok_or(OAuthError::InvalidArguments)?;
  let agent: ureq::Agent = ureq::Agent::config_builder()
    .timeout_global(Some(Duration::from_secs(12)))
    .max_redirects(0)
    .build()
    .into();
  let body = format!(
    "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
    crate::helpers::percent_encode(refresh_token),
    crate::helpers::percent_encode(client_id),
    crate::helpers::percent_encode(&secret),
  );
  let response = match agent
    .post("https://discord.com/api/v10/oauth2/token")
    .header("Content-Type", "application/x-www-form-urlencoded")
    .header("User-Agent", crate::DISCORD_USER_AGENT)
    .send(&body)
  {
    Ok(r) => r,
    Err(ureq::Error::StatusCode(400 | 401)) => return Err(OAuthError::InvalidAccessToken),
    Err(_) => return Err(OAuthError::DiscordUnavailable),
  };
  let mut buf = String::new();
  response
    .into_body()
    .as_reader()
    .take(65_537)
    .read_to_string(&mut buf)
    .map_err(|_| OAuthError::DiscordUnavailable)?;
  let data: Value =
    serde_json::from_str(&buf).map_err(|_| OAuthError::UnexpectedDiscordResponse)?;
  let access_token = data
    .get("access_token")
    .and_then(Value::as_str)
    .ok_or(OAuthError::UnexpectedDiscordResponse)?
    .to_owned();
  let new_refresh = data
    .get("refresh_token")
    .and_then(Value::as_str)
    .map(str::to_owned);
  Ok((access_token, new_refresh))
}

/// One OAuth2 code-for-token exchange result. `refresh_token` is present
/// only when Discord issued one.
pub struct ExchangeOutcome {
  pub access_token: String,
  pub refresh_token: Option<String>,
  pub expires_in: u64,
  pub scopes: Vec<String>,
}

/// Exchange an authorization code for tokens (needs the client secret).
pub fn exchange_code(
  code: &str,
  client_id: &str,
  redirect_uri: &str,
) -> Result<ExchangeOutcome, OAuthError> {
  let Some(secret) = crate::store::client_secret() else {
    return Err(OAuthError::ClientSecretRequired);
  };
  let agent: ureq::Agent = ureq::Agent::config_builder()
    .timeout_global(Some(Duration::from_secs(12)))
    .max_redirects(0)
    .build()
    .into();
  let body = format!(
    "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}",
    crate::helpers::percent_encode(code),
    crate::helpers::percent_encode(redirect_uri),
    crate::helpers::percent_encode(client_id),
    crate::helpers::percent_encode(&secret),
  );
  let response = match agent
    .post("https://discord.com/api/v10/oauth2/token")
    .header("Content-Type", "application/x-www-form-urlencoded")
    .header("User-Agent", crate::DISCORD_USER_AGENT)
    .send(&body)
  {
    Ok(r) => r,
    Err(ureq::Error::StatusCode(400 | 401)) => return Err(OAuthError::InvalidAccessToken),
    Err(_) => return Err(OAuthError::DiscordUnavailable),
  };
  let mut buf = String::new();
  response
    .into_body()
    .as_reader()
    .take(65_537)
    .read_to_string(&mut buf)
    .map_err(|_| OAuthError::DiscordUnavailable)?;
  let data: Value =
    serde_json::from_str(&buf).map_err(|_| OAuthError::UnexpectedDiscordResponse)?;
  let access_token = data
    .get("access_token")
    .and_then(Value::as_str)
    .ok_or(OAuthError::UnexpectedDiscordResponse)?
    .to_owned();
  let scopes = data
    .get("scope")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .split(' ')
    .filter(|s| !s.is_empty())
    .map(str::to_owned)
    .collect();
  Ok(ExchangeOutcome {
    access_token,
    refresh_token: data
      .get("refresh_token")
      .and_then(Value::as_str)
      .map(str::to_owned),
    expires_in: data
      .get("expires_in")
      .and_then(Value::as_u64)
      .unwrap_or(604_800),
    scopes,
  })
}

/// Parse a Discord user object into the shared RPC identity type.
pub(crate) fn parse_rpc_user(value: &Value) -> Result<RpcUser, OAuthError> {
  let object = value
    .as_object()
    .ok_or(OAuthError::UnexpectedDiscordResponse)?;
  let id = object
    .get("id")
    .and_then(Value::as_str)
    .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
    .ok_or(OAuthError::UnexpectedDiscordResponse)?;
  let username = object
    .get("username")
    .and_then(Value::as_str)
    .filter(|name| !name.is_empty())
    .ok_or(OAuthError::UnexpectedDiscordResponse)?;
  Ok(RpcUser {
    id: id.to_owned(),
    username: username.to_owned(),
    discriminator: object
      .get("discriminator")
      .and_then(Value::as_str)
      .unwrap_or("0")
      .to_owned(),
    global_name: object
      .get("global_name")
      .and_then(Value::as_str)
      .map(str::to_owned),
    avatar: object
      .get("avatar")
      .and_then(Value::as_str)
      .map(str::to_owned),
    avatar_decoration_data: object.get("avatar_decoration_data").cloned(),
    bot: object.get("bot").and_then(Value::as_bool).unwrap_or(false),
    flags: object
      .get("flags")
      .or_else(|| object.get("public_flags"))
      .and_then(Value::as_u64)
      .and_then(|flags| u32::try_from(flags).ok())
      .unwrap_or(0),
    premium_type: object
      .get("premium_type")
      .and_then(Value::as_u64)
      .and_then(|premium| u32::try_from(premium).ok())
      .unwrap_or(0),
  })
}

#[cfg(test)]
#[path = "../tests/unit/authenticate.rs"]
mod tests;
