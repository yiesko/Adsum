use super::fold_and_anchor;

/// A UNC path folds to the double-slash form the cached prefixes use.
#[test]
fn unc_path_folds_to_anchored_double_slash() {
  let mut out = String::new();
  fold_and_anchor(r"\\server\share\game.exe", &mut out);
  assert_eq!(out, "//server/share/game.exe");
}

/// A drive path gets exactly one leading anchor, never a doubled one.
#[test]
fn drive_path_gets_one_anchor() {
  let mut out = String::new();
  fold_and_anchor(r"C:\Games\game.exe", &mut out);
  assert_eq!(out, "/C:/Games/game.exe");
}

/// An already-anchored folded path passes through byte-identical.
#[test]
fn anchored_path_is_untouched() {
  let mut out = String::new();
  fold_and_anchor("/usr/bin/game", &mut out);
  assert_eq!(out, "/usr/bin/game");
}

/// A leading single backslash folds into the anchor, not `//`.
#[test]
fn leading_backslash_folds_to_single_anchor() {
  let mut out = String::new();
  fold_and_anchor(r"\foo\bar", &mut out);
  assert_eq!(out, "/foo/bar");
}

/// An empty path stays empty (no phantom anchor).
#[test]
fn empty_path_stays_empty() {
  let mut out = String::new();
  fold_and_anchor("", &mut out);
  assert_eq!(out, "");
}
