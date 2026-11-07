use super::*;
use crate::resolver::{resolve_wine_chain, resolve_wine_chain_with_cwd};

fn exec(path: &str, args: Option<&str>) -> Exec {
  Exec {
    pid: 1000,
    path: path.into(),
    arguments: args.map(str::to_string),
    argument_spans: Vec::new(),
  }
}

#[test]
fn wine_resolves_target() {
  let e = exec("wine", Some("./game.exe"));
  let r = resolve_wine_chain(&e).expect("resolves");
  assert_eq!(r.path, "./game.exe");
  assert_eq!(r.argument_start, 1);
  assert!(r.is_windows_binary);
}

#[test]
fn wine_rejects_windows_path() {
  let e = exec("wine", Some("C:\\path\\game.exe"));
  assert!(resolve_wine_chain(&e).is_none());
}

#[test]
fn wine_rejects_system_directory() {
  let e = exec("wine", Some("/drive_c/windows/system32/foo.exe"));
  assert!(resolve_wine_chain(&e).is_none());
}

#[test]
fn wine_rejects_empty_target() {
  let e = exec("wine", Some(""));
  assert!(resolve_wine_chain(&e).is_none());
}

#[test]
fn wine_rejects_unknown_loader() {
  let e = exec("box64", Some("./game.exe"));
  assert!(resolve_wine_chain(&e).is_none());
}

#[test]
fn wine64_resolves_target() {
  let e = exec("wine64", Some("./game.exe"));
  assert!(resolve_wine_chain(&e).is_some());
}

#[test]
fn wine_preloader_resolves_target() {
  let e = exec("wine-preloader", Some("./game.exe"));
  assert!(resolve_wine_chain(&e).is_some());
}

#[test]
fn wine_native_binary_not_windows() {
  let e = exec("wine", Some("./game"));
  let r = resolve_wine_chain(&e).expect("resolves");
  assert!(!r.is_windows_binary);
}

#[test]
fn wine_resolves_preloader_chain() {
  let e = exec("wine-preloader", Some("wine64 ./game.exe"));
  let r = resolve_wine_chain(&e).expect("resolves");
  assert_eq!(r.path, "./game.exe");
}

#[test]
fn wine_rejects_drive_c_windows() {
  let e = exec("wine", Some("/drive_c/windows/system32/foo.exe"));
  assert!(resolve_wine_chain(&e).is_none());
}

#[test]
fn wine_preserves_target_arguments() {
  let e = exec("wine", Some("./game.exe -windowed"));
  let r = resolve_wine_chain(&e).expect("resolves");
  assert_eq!(r.path, "./game.exe");
  assert_eq!(e.arguments_from(r.argument_start), Some("-windowed"));
}

/// A macOS Wine process carries the actual game in argv, not in the
/// saved image path: a relative target must be joined with the current
/// working directory without taking a heap copy.
#[test]
fn wine_joins_relative_target_with_cwd() {
  let e = exec("wine64", Some("./demo/game.exe -windowed"));
  let mut buffer = String::new();
  let r = resolve_wine_chain_with_cwd(&e, Some("/games/steam"), &mut buffer).expect("resolves");
  assert_eq!(r.path, "/games/steam/demo/game.exe");
  assert_eq!(e.arguments_from(r.argument_start), Some("-windowed"));
}

#[test]
fn wine_rejects_system_target_even_with_cwd() {
  let e = exec("wine", Some("C:\\Windows\\System32\\explorer.exe"));
  let mut buffer = String::new();
  assert!(resolve_wine_chain_with_cwd(&e, Some("/games"), &mut buffer).is_none());
}

#[test]
fn wine_requires_cwd_for_relative_target() {
  let e = exec("wine", Some("./game.exe"));
  let mut buffer = String::new();
  assert!(resolve_wine_chain_with_cwd(&e, None, &mut buffer).is_none());
}
