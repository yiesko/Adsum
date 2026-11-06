use super::*;
use crate::resolver::resolve_wine_chain;

fn exec(path: &str, args: Option<&str>) -> Exec {
  Exec {
    pid: 1000,
    path: path.into(),
    arguments: args.map(str::to_string),
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
