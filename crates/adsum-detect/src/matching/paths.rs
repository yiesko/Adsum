//! Path-shape rules: what it means for a declared executable name to
//! NAME a process path. Tail-on-component-boundary matching with
//! Windows separators folded, the bare-basename fallback for short
//! Proton/Heroic launches, and the macOS bundle interior (whose shape
//! knowledge lives in `platform::macos::bundle` and is consulted here,
//! ungated, because the tier is data-driven).

use crate::platform::macos::bundle::names_a_bundle;

/// Executable stem of an already-normalized (`/`-separated, lowercase) path,
/// without extension: `/games/how to fish.exe` -> `how to fish`. Borrowed:
/// callers only compare it against the map.
#[inline]
pub fn exe_stem(normalized_path: &str) -> &str {
  let base = normalized_path
    .rsplit('/')
    .next()
    .unwrap_or(normalized_path);
  match base.rfind('.') {
    Some(dot) if dot > 0 => &base[..dot],
    _ => base,
  }
}

/// Whether `name` is the tail of `path` on a component boundary,
/// case-insensitive and with Windows separators folded.
fn names_tail(path: &str, name: &str) -> bool {
  // The launcher marker is arrpc's "the arguments carry the launcher"
  // flag, kept raw on the entry for the argument check in
  // `finish_direct_hit`; it is not part of the file name.
  let name = name.strip_prefix('>').unwrap_or(name);
  // A name that normalizes to a bare `/` is dropped at bundle build time
  // (it would match every path), so no hit can point at one — but a name
  // that names nothing must never name anything.
  if name.is_empty() {
    return false;
  }
  let path = path.as_bytes();
  let name = name.as_bytes();
  if let Some(at) = path.len().checked_sub(name.len()) {
    let tail = &path[at..];
    if tail.iter().zip(name).all(|(path_byte, name_byte)| {
      // A declared Windows separator is the `/` the path carries.
      let name_byte = if *name_byte == b'\\' {
        b'/'
      } else {
        *name_byte
      };
      path_byte.eq_ignore_ascii_case(&name_byte)
    }) {
      // What precedes the name has to be a separator — unless the name
      // carries its own leading one, which is the boundary itself (a
      // normalized pattern does). Bytes, not chars: the tail was matched
      // byte-for-byte above, so the split is already known to be aligned.
      let rest = &path[..at];
      if rest.is_empty() || rest.last() == Some(&b'/') || matches!(name.first(), Some(b'/' | b'\\'))
      {
        return true;
      }
    }
  }
  false
}

/// Whether `dir` occurs anywhere in `path` (case-insensitive, `\` folded),
/// so a near-miss component like `xgarrysmod` suppresses the fallback.
fn dir_present_in_path(path: &str, dir: &str) -> bool {
  #[inline]
  fn fold(byte: u8) -> u8 {
    if byte == b'\\' {
      b'/'
    } else {
      byte.to_ascii_lowercase()
    }
  }
  let dir = dir.as_bytes();
  if dir.is_empty() {
    return false;
  }
  path
    .as_bytes()
    .windows(dir.len())
    .any(|window| window.iter().zip(dir).all(|(p, d)| fold(*p) == fold(*d)))
}

/// Corroboration rule: exact tail, then the bare basename when the declared
/// directory is absent (Proton short launches), then the macOS bundle.
pub(crate) fn exe_names_path(path: &str, name: &str) -> bool {
  // The launcher marker is arrpc's "the arguments carry the launcher"
  // flag, kept raw on the entry for the argument check in
  // `finish_direct_hit`; it is not part of the file name.
  let name = name.strip_prefix('>').unwrap_or(name);
  // A name that normalizes to a bare `/` is dropped at bundle build time
  // (it would match every path), so no hit can point at one — but a name
  // that names nothing must never name anything.
  if name.is_empty() {
    return false;
  }
  // Full declared name first (multi-component entries launched with their
  // full relative path).
  if names_tail(path, name) {
    return true;
  }
  // Bare-basename fallback for short Proton/Heroic launches: this tier runs
  // only after a store id (Steam/Epic) already named the entry, so a generic
  // basename at the tail is corroboration, not standalone evidence.
  let basename = name.rsplit(['/', '\\']).next().unwrap_or(name);
  let dirpart = name.rsplit_once(['/', '\\']).map(|(d, _)| d).unwrap_or("");
  if basename != name && !basename.is_empty() {
    // A declared directory present as a substring (case-insensitive,
    // `\` folded) suppresses the fallback: the full name should have
    // corroborated, or it is a near miss.
    let dir_present = !dirpart.is_empty() && dir_present_in_path(path, dirpart);
    if !dir_present && names_tail(path, basename) {
      return true;
    }
  }
  names_a_bundle(path.as_bytes(), name.as_bytes())
}
