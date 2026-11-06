#![allow(dead_code)]
//! Browser-based authorization-code flow and loopback callback handling.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::OAuthError;
use crate::helpers::*;
use crate::manager::OAuthCancellation;

#[derive(Debug)]
pub(crate) struct CallbackTarget {
  address: SocketAddr,
  path: String,
}

fn authorize_timeout() -> std::time::Duration {
  let secs = std::env::var("ADSUM_OAUTH_TIMEOUT")
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
    .filter(|&s| s > 0 && s <= 600)
    .unwrap_or(180);
  std::time::Duration::from_secs(secs)
}
const MAX_HTTP_REQUEST: usize = 8 * 1024;
static AUTHORIZATION_IN_PROGRESS: std::sync::atomic::AtomicBool =
  std::sync::atomic::AtomicBool::new(false);

pub fn authorize(
  client_id: &str,
  scopes: &[String],
  rpc_token: Option<&str>,
  username: Option<&str>,
  code_challenge: Option<&str>,
  code_challenge_method: Option<&str>,
) -> Result<String, OAuthError> {
  let redirect_uri = std::env::var("ADSUM_OAUTH_REDIRECT_URI").ok();
  authorize_with_context(
    client_id,
    scopes,
    rpc_token,
    username,
    code_challenge,
    code_challenge_method,
    redirect_uri.as_deref(),
    &OAuthCancellation::default(),
  )
}

#[allow(clippy::too_many_arguments)]
pub fn authorize_with_context(
  client_id: &str,
  scopes: &[String],
  rpc_token: Option<&str>,
  username: Option<&str>,
  code_challenge: Option<&str>,
  code_challenge_method: Option<&str>,
  redirect_uri: Option<&str>,
  cancellation: &OAuthCancellation,
) -> Result<String, OAuthError> {
  cancellation.check()?;
  // `rpc_token` is explicitly unsupported: redeeming it needs the
  // application's client secret, which this adapter never holds. Return the
  // documented error instead of silently ignoring the field.
  if rpc_token.is_some() {
    return Err(OAuthError::RpcTokenUnsupported);
  }
  if username.is_some() {
    return Err(OAuthError::ProvisionalAccountUnsupported);
  }
  validate_client_id(client_id)?;
  validate_scopes(scopes)?;
  if !scopes.iter().any(|scope| scope == "rpc") {
    return Err(OAuthError::MissingRpcScope);
  }

  if code_challenge.is_none() && code_challenge_method.is_some() {
    return Err(OAuthError::InvalidCodeChallenge);
  }
  if let Some(challenge) = code_challenge
    && (challenge.len() != 43
      || !challenge
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
      || code_challenge_method.unwrap_or("S256") != "S256")
  {
    return Err(OAuthError::InvalidCodeChallenge);
  }

  let redirect_uri = redirect_uri.ok_or(OAuthError::RedirectNotConfigured)?;
  let callback = parse_loopback_redirect(redirect_uri)?;
  let _authorization_guard = AuthorizationGuard::acquire()?;
  let listener = TcpListener::bind(callback.address).map_err(|_| OAuthError::CallbackBindFailed)?;
  listener
    .set_nonblocking(true)
    .map_err(|_| OAuthError::CallbackBindFailed)?;
  cancellation.check()?;
  let mut random = [0_u8; 32];
  getrandom::fill(&mut random).map_err(|_| OAuthError::RandomUnavailable)?;
  let state = hex(&random);
  let scope = scopes.join(" ");
  let mut url = format!(
    "https://discord.com/oauth2/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&prompt=consent",
    percent_encode(client_id),
    percent_encode(redirect_uri),
    percent_encode(&scope),
    percent_encode(&state),
  );
  if let Some(challenge) = code_challenge {
    url.push_str("&code_challenge=");
    url.push_str(&percent_encode(challenge));
    url.push_str("&code_challenge_method=S256");
  }

  let mut opener = open_browser(&url)?;
  wait_for_callback(
    listener,
    &callback.path,
    &state,
    authorize_timeout(),
    cancellation,
    Some(&mut opener),
  )
}

/// Validates an access token against Discord's current-authorization endpoint.
/// The application ID must match the ID bound to the RPC connection.
pub(crate) fn validate_client_id(client_id: &str) -> Result<(), OAuthError> {
  if client_id.is_empty() {
    return Err(OAuthError::MissingClientId);
  }
  if client_id.len() > 20 || !client_id.bytes().all(|byte| byte.is_ascii_digit()) {
    return Err(OAuthError::InvalidClientId);
  }
  Ok(())
}

pub(crate) fn validate_scopes(scopes: &[String]) -> Result<(), OAuthError> {
  if scopes.is_empty() {
    return Err(OAuthError::MissingScopes);
  }
  if scopes.len() > 32
    || scopes.iter().any(|scope| {
      scope.is_empty()
        || scope.len() > 128
        || !scope
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    })
  {
    return Err(OAuthError::InvalidScopes);
  }
  Ok(())
}

pub(crate) fn parse_loopback_redirect(value: &str) -> Result<CallbackTarget, OAuthError> {
  let rest = value
    .strip_prefix("http://127.0.0.1:")
    .ok_or(OAuthError::InvalidRedirect)?;
  let (port, path) = rest.split_once('/').ok_or(OAuthError::InvalidRedirect)?;
  if path != "callback"
    || port.is_empty()
    || value.bytes().any(|byte| matches!(byte, b'?' | b'#' | b'@'))
    || !port.bytes().all(|byte| byte.is_ascii_digit())
  {
    return Err(OAuthError::InvalidRedirect);
  }
  let port = port
    .parse::<u16>()
    .map_err(|_| OAuthError::InvalidRedirect)?;
  // Use an unprivileged fixed port so Adsum can run as a user service.
  if port < 1024 {
    return Err(OAuthError::InvalidRedirect);
  }
  // Avoid colliding with Adsum's own listener ports.
  if (6463..=6472).contains(&port) || port == 1337 || port == 1338 {
    return Err(OAuthError::InvalidRedirect);
  }
  Ok(CallbackTarget {
    address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
    path: "/callback".to_string(),
  })
}

/// Only one callback listener can own the process-wide configured redirect.
struct AuthorizationGuard;

impl AuthorizationGuard {
  fn acquire() -> Result<Self, OAuthError> {
    AUTHORIZATION_IN_PROGRESS
      .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
      .map(|_| Self)
      .map_err(|_| OAuthError::AuthorizationInProgress)
  }
}

impl Drop for AuthorizationGuard {
  fn drop(&mut self) {
    AUTHORIZATION_IN_PROGRESS.store(false, Ordering::Release);
  }
}

pub(crate) struct BrowserOpener(Option<std::process::Child>);
impl BrowserOpener {
  fn check(&mut self) -> Result<(), OAuthError> {
    if let Some(child) = self.0.as_mut()
      && let Some(status) = child
        .try_wait()
        .map_err(|_| OAuthError::BrowserUnavailable)?
    {
      self.0 = None;
      if !status.success() {
        return Err(OAuthError::BrowserUnavailable);
      }
    }
    Ok(())
  }
}

impl Drop for BrowserOpener {
  fn drop(&mut self) {
    if let Some(mut child) = self.0.take() {
      // Stop/reap only the launcher we created, never the browser process tree.
      if !matches!(child.try_wait(), Ok(Some(_))) && child.kill().is_ok() {
        let _ = child.wait();
      }
    }
  }
}

pub(crate) fn open_browser(url: &str) -> Result<BrowserOpener, OAuthError> {
  use std::process::Stdio;
  #[cfg(target_os = "linux")]
  let result = Command::new("xdg-open")
    .arg(url)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn();

  #[cfg(target_os = "macos")]
  let result = Command::new("open")
    .arg(url)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn();

  #[cfg(target_os = "windows")]
  let result = Command::new("rundll32")
    .args(["url.dll,FileProtocolHandler", url])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn();

  #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
  let result: std::io::Result<std::process::Child> =
    Err(std::io::Error::other("unsupported platform"));

  result
    .map(|child| BrowserOpener(Some(child)))
    .map_err(|_| OAuthError::BrowserUnavailable)
}

pub(crate) fn wait_for_callback(
  listener: TcpListener,
  expected_path: &str,
  expected_state: &str,
  timeout: Duration,
  cancellation: &OAuthCancellation,
  mut opener: Option<&mut BrowserOpener>,
) -> Result<String, OAuthError> {
  let deadline = Instant::now() + timeout;
  let host = listener
    .local_addr()
    .map_err(|_| OAuthError::CallbackBindFailed)?
    .to_string();
  while Instant::now() < deadline {
    cancellation.check()?;
    if let Some(opener) = opener.as_deref_mut() {
      opener.check()?;
    }
    match listener.accept() {
      Ok((mut stream, peer)) => {
        if !peer.ip().is_loopback() {
          write_http_response(
            &mut stream,
            403,
            "Forbidden",
            "Only loopback callbacks are accepted.",
          );
          continue;
        }
        // The listener is non-blocking; on BSD/Windows the accepted socket
        // inherits that flag, which would busy-spin in `read_http_request`.
        // Restore blocking mode with bounded read/write timeouts.
        stream
          .set_nonblocking(false)
          .map_err(|_| OAuthError::CallbackBindFailed)?;
        stream
          .set_read_timeout(Some(Duration::from_millis(100)))
          .map_err(|_| OAuthError::CallbackBindFailed)?;
        stream
          .set_write_timeout(Some(Duration::from_millis(250)))
          .map_err(|_| OAuthError::CallbackBindFailed)?;
        let read_deadline = (Instant::now() + Duration::from_secs(3)).min(deadline);
        let Some((path, query)) =
          read_http_request(&mut stream, &host, read_deadline, cancellation)?
        else {
          write_http_response(&mut stream, 400, "Bad Request", "Invalid OAuth callback.");
          continue;
        };
        if path != expected_path {
          write_http_response(&mut stream, 404, "Not Found", "Unknown callback path.");
          continue;
        }
        let Some(params) = parse_query(&query) else {
          write_http_response(&mut stream, 400, "Bad Request", "Invalid callback query.");
          continue;
        };
        if !params
          .get("state")
          .is_some_and(|state| state_matches(state, expected_state))
        {
          write_http_response(
            &mut stream,
            400,
            "Bad Request",
            "OAuth state mismatch. You may close this tab.",
          );
          continue;
        }
        // Error callbacks cannot grant access, and providers may omit `iss`
        // there. A successful code still requires the exact trusted issuer.
        let issuer = params.get("iss").map(String::as_str);
        if issuer.is_some_and(|issuer| issuer != "https://discord.com")
          || (!params.contains_key("error") && issuer.is_none())
        {
          write_http_response(
            &mut stream,
            400,
            "Bad Request",
            "Unexpected OAuth issuer. You may close this tab.",
          );
          return Err(OAuthError::WrongIssuer);
        }
        if params.contains_key("error") {
          if params.contains_key("code") {
            write_http_response(&mut stream, 400, "Bad Request", "Ambiguous OAuth callback.");
            continue;
          }
          write_http_response(
            &mut stream,
            200,
            "OK",
            "Authorization was cancelled. You may close this tab.",
          );
          return Err(OAuthError::AuthorizationDenied);
        }
        let Some(code) = params.get("code") else {
          write_http_response(
            &mut stream,
            400,
            "Bad Request",
            "Discord did not return an authorization code.",
          );
          continue;
        };
        if code.is_empty()
          || code.len() > 4096
          || !code.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
          write_http_response(
            &mut stream,
            400,
            "Bad Request",
            "Invalid authorization code.",
          );
          continue;
        }
        write_http_response(
          &mut stream,
          200,
          "OK",
          "Discord authorization received. You may close this tab and return to your game.",
        );
        cancellation.check()?;
        return Ok(code.clone());
      }
      Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
        std::thread::sleep(
          Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
        );
      }
      Err(_) => return Err(OAuthError::CallbackBindFailed),
    }
  }
  cancellation.check()?;
  Err(OAuthError::AuthorizationTimeout)
}

pub(crate) fn read_http_request(
  stream: &mut TcpStream,
  expected_host: &str,
  deadline: Instant,
  cancellation: &OAuthCancellation,
) -> Result<Option<(String, String)>, OAuthError> {
  let mut buffer = Vec::with_capacity(1024);
  let mut chunk = [0_u8; 1024];
  loop {
    cancellation.check()?;
    if Instant::now() >= deadline {
      return Ok(None);
    }
    let count = match stream.read(&mut chunk) {
      Ok(0) => return Ok(None),
      Ok(count) => count,
      Err(error)
        if matches!(
          error.kind(),
          std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
        ) =>
      {
        continue;
      }
      Err(_) => return Ok(None),
    };
    buffer.extend_from_slice(&chunk[..count]);
    if buffer.len() > MAX_HTTP_REQUEST {
      return Ok(None);
    }
    if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
      break;
    }
  }
  let parse = || -> Option<(String, String)> {
    let request = std::str::from_utf8(&buffer).ok()?;
    let mut lines = request.lines();
    let mut fields = lines.next()?.split_whitespace();
    if fields.next()? != "GET" {
      return None;
    }
    let target = fields.next()?;
    if !target.starts_with('/')
      || target.starts_with("//")
      || !matches!(fields.next()?, "HTTP/1.0" | "HTTP/1.1")
      || fields.next().is_some()
    {
      return None;
    }
    let mut host = None;
    for line in lines.take_while(|line| !line.is_empty()) {
      let (key, value) = line.split_once(':')?;
      if key.eq_ignore_ascii_case("host") && host.replace(value.trim()).is_some() {
        return None;
      }
    }
    if host != Some(expected_host) {
      return None;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Some((path.to_owned(), query.to_owned()))
  };
  Ok(parse())
}

/// Decode an application/x-www-form-urlencoded query. Duplicate keys are
/// rejected instead of silently choosing first/last values for `state`.
pub(crate) fn parse_query(query: &str) -> Option<HashMap<String, String>> {
  let mut values = HashMap::new();
  for pair in query.split('&').filter(|pair| !pair.is_empty()) {
    let (key, value) = pair.split_once('=')?;
    let key = percent_decode(key)?;
    let value = percent_decode(value)?;
    if values.insert(key, value).is_some() {
      return None;
    }
  }
  Some(values)
}

pub(crate) fn write_http_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
  let response = format!(
    "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'\r\n\r\n{body}",
    body.len()
  );
  let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
#[path = "../tests/unit/authorize.rs"]
mod tests;
