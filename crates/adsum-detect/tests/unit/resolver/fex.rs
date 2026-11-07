use super::*;
use crate::resolver::{resolve_fex, resolve_qemu_user};

fn exec(path: &str, args: Option<&str>) -> Exec {
  Exec {
    pid: 1000,
    path: path.into(),
    arguments: args.map(str::to_string),
    argument_spans: Vec::new(),
  }
}

#[test]
fn fex_interpreter_resolves_target() {
  let e = exec("fexinterpreter", Some("./game"));
  let r = resolve_fex(&e).expect("resolves");
  assert_eq!(r.path, "./game");
  assert_eq!(r.argument_start, 1);
}

#[test]
fn fex_loader_resolves_target() {
  let e = exec("fexloader", Some("./game"));
  let r = resolve_fex(&e).expect("resolves");
  assert_eq!(r.path, "./game");
}

#[test]
fn fex_rejects_unknown_loader() {
  let e = exec("box64", Some("./game"));
  assert!(resolve_fex(&e).is_none());
}

#[test]
fn fex_rejects_empty_target() {
  let e = exec("fexinterpreter", Some(""));
  assert!(resolve_fex(&e).is_none());
}

#[test]
fn qemu_user_resolves_target() {
  let e = exec("qemu-x86_64", Some("./game"));
  let r = resolve_qemu_user(&e).expect("resolves");
  assert_eq!(r.path, "./game");
  assert_eq!(r.argument_start, 1);
}

#[test]
fn qemu_static_resolves_target() {
  let e = exec("qemu-x86_64-static", Some("./game"));
  assert!(resolve_qemu_user(&e).is_some());
}

#[test]
fn qemu_rejects_unknown_emulator() {
  let e = exec("qemu-aarch64", Some("./game"));
  assert!(resolve_qemu_user(&e).is_none());
}

#[test]
fn qemu_rejects_empty_target() {
  let e = exec("qemu-x86_64", Some(""));
  assert!(resolve_qemu_user(&e).is_none());
}

#[test]
fn fex_resolves_modern_name() {
  let e = exec("fex", Some("./game"));
  let r = resolve_fex(&e).expect("resolves");
  assert_eq!(r.path, "./game");
}

#[test]
fn fex_resolves_binfmt_style() {
  let e = exec("fex", Some("/path/game original-argv0 arg1"));
  let r = resolve_fex(&e).expect("resolves");
  assert_eq!(r.path, "/path/game");
}
