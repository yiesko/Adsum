use super::*;

/// Transient per-connection failures retry at once; persistent ones
/// pause instead of hot-spinning the accept loop.
#[test]
fn accept_errors_back_off_only_when_persistent() {
  use std::io::ErrorKind;
  assert_eq!(
    accept_error_backoff(&std::io::Error::from(ErrorKind::ConnectionAborted)),
    None
  );
  assert_eq!(
    accept_error_backoff(&std::io::Error::from(ErrorKind::Interrupted)),
    None
  );
  assert_eq!(
    accept_error_backoff(&std::io::Error::from(ErrorKind::PermissionDenied)),
    Some(Duration::from_millis(50))
  );
}
