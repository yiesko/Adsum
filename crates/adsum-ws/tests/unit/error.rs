use super::*;

/// Both `Full` and `Closed` convert into `SendError`.
#[test]
fn try_send_error_collapses_to_send_error() {
  let _: SendError = TrySendError::Full.into();
  let _: SendError = TrySendError::Closed.into();
}

/// `Bind` keeps the OS error as source (e.g. `AddrInUse` stays matchable).
#[test]
fn bind_error_preserves_os_source() {
  use std::error::Error as _;
  let err = Error::Bind(io::Error::new(io::ErrorKind::AddrInUse, "taken"));
  let source = err.source().expect("Bind must keep its source");
  assert_eq!(
    source
      .downcast_ref::<io::Error>()
      .expect("source is io::Error")
      .kind(),
    io::ErrorKind::AddrInUse
  );
  assert!(err.to_string().starts_with("failed to bind"));
}
