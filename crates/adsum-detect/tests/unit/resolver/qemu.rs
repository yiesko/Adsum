use super::*;
use crate::resolver::resolve_qemu_user;

fn exec(path: &str, args: Option<&str>) -> Exec {
  Exec {
    pid: 1000,
    path: path.into(),
    arguments: args.map(str::to_string),
    argument_spans: Vec::new(),
  }
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
fn qemu_resolves_libraries() {
  let e = exec("qemu-x86_64", Some("-L /lib ./game"));
  let r = resolve_qemu_user(&e).expect("resolves");
  assert_eq!(r.path, "./game");
  assert_eq!(r.argument_start, 3);
}

#[test]
fn qemu_resolves_cpu() {
  let e = exec("qemu-x86_64", Some("-cpu max ./game"));
  let r = resolve_qemu_user(&e).expect("resolves");
  assert_eq!(r.path, "./game");
  assert_eq!(r.argument_start, 3);
}

#[test]
fn qemu_resolves_b_r() {
  let e = exec("qemu-x86_64", Some("-B offset -R size ./game"));
  let r = resolve_qemu_user(&e).expect("resolves");
  assert_eq!(r.path, "./game");
}
