use super::*;
use crate::resolver::resolve_box64;

fn exec(path: &str, args: Option<&str>) -> Exec {
  Exec {
    pid: 1000,
    path: path.into(),
    arguments: args.map(str::to_string),
    argument_spans: Vec::new(),
  }
}

#[test]
fn box64_resolves_target_at_argv_one() {
  let e = exec("box64", Some("./game"));
  let r = resolve_box64(&e).expect("resolves");
  assert_eq!(r.path, "./game");
  assert_eq!(r.argument_start, 1);
  assert!(!r.is_windows_binary);
}

#[test]
fn box64_rejects_non_box64_path() {
  let e = exec("/usr/bin/java", Some("./game"));
  assert!(resolve_box64(&e).is_none());
}

#[test]
fn box64_rejects_empty_target() {
  let e = exec("box64", Some(""));
  assert!(resolve_box64(&e).is_none());
}

#[test]
fn box64_rejects_no_arguments() {
  let e = exec("box64", None);
  assert!(resolve_box64(&e).is_none());
}

#[test]
fn box64_preserves_windows_binary_flag_for_exe() {
  let e = exec("box64", Some("game.exe"));
  let r = resolve_box64(&e).expect("resolves");
  assert!(!r.is_windows_binary, "box64 runs native binaries");
}

#[test]
fn box64_resolves_via_basename_case_insensitive() {
  let e = exec("/opt/BOX64", Some("./game"));
  assert!(resolve_box64(&e).is_some());
}

#[test]
fn box64_resolves_double_dash() {
  let e = exec("box64", Some("-- ./game"));
  let r = resolve_box64(&e).expect("resolves");
  assert_eq!(r.path, "./game");
  assert_eq!(r.argument_start, 2);
}

#[test]
fn box64_resolves_wine_chain() {
  let e = exec("box64", Some("wine ./game.exe"));
  let r = resolve_box64(&e).expect("resolves");
  assert_eq!(r.path, "./game.exe");
  assert!(r.is_windows_binary);
}

#[test]
fn box64_resolves_wine_preloader_chain() {
  let e = exec("box64", Some("wine-preloader wine64 ./game.exe"));
  let r = resolve_box64(&e).expect("resolves");
  assert_eq!(r.path, "./game.exe");
}

#[test]
fn box64_rejects_cpu_avx2() {
  let e = exec("box64", Some("-cpu AVX2 ./game"));
  assert!(resolve_box64(&e).is_none());
}

#[test]
fn box64_preserves_target_arguments() {
  let e = exec("box64", Some("./game -windowed -foo"));
  let r = resolve_box64(&e).expect("resolves");
  assert_eq!(r.path, "./game");
  assert_eq!(e.arguments_from(r.argument_start), Some("-windowed -foo"));
}
