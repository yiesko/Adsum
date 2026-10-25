//! The macOS `.app` bundle interior — Apple filesystem *shape*
//! knowledge, deliberately NOT cfg-gated.
//!
//! The tier is data-driven, not OS-driven: the database ships `darwin`
//! executables on every platform, a custom override may declare a
//! `.app` name anywhere, and the corroboration runs wherever the
//! automaton runs (a Proton path never looks like a bundle, so the
//! shape check simply never fires there). Keeping it compiled — and
//! unit-tested — everywhere is what preserves that behavior; gating it
//! to macOS would silently change matching on the other targets and
//! blind the Linux CI to it.

/// The macOS bundle interior, which every bundle Apple has ever shipped
/// carries: `.../<Something>.app/Contents/MacOS/<binary>`.
const BUNDLE_INTERIOR: &[u8] = b"/contents/";

/// Whether `name` is a `.app` bundle standing at a component boundary with
/// that interior behind it.
///
/// A macOS entry names the bundle rather than the binary inside it:
/// Factorio declares `factorio.app` beside its Linux `factorio` and its
/// `factorio.exe`, but the process macOS runs is
/// `.../Factorio.app/Contents/MacOS/factorio` — the declared name is a
/// component in the middle of the path and never its tail, so the rule
/// above refuses the running game.
///
/// Requiring the interior is what keeps this from reopening the substring
/// hole the function exists to close: a name in the middle of a path names
/// nothing unless a bundle's own contents follow it, and
/// `.../factorio.app/something/factorio` still names nothing. 61 declared
/// executables across 56 entries in the bundled database are spelled this
/// way, every one of them `os: "darwin"`.
pub(crate) fn names_a_bundle(path: &[u8], name: &[u8]) -> bool {
  // Cheap enough to sit in front of the search: 56 of 24.299 entries
  // carry a `.app` name, and the four-byte compare is what keeps the
  // other ~24.000 off the walk below. This runs per declared executable
  // per probe, so it cannot afford to be unconditional.
  if name.len() < 4 || !name[name.len() - 4..].eq_ignore_ascii_case(b".app") {
    return false;
  }
  path.windows(name.len()).enumerate().any(|(at, window)| {
    let whole = window.iter().zip(name).all(|(path_byte, name_byte)| {
      let name_byte = if *name_byte == b'\\' {
        b'/'
      } else {
        *name_byte
      };
      path_byte.eq_ignore_ascii_case(&name_byte)
    });
    if !whole {
      return false;
    }
    // Same boundary rule as the tail case: a `/` before, or a name that
    // carries its own leading one.
    let fronted = at == 0 || path[at - 1] == b'/' || matches!(name.first(), Some(b'/' | b'\\'));
    fronted && starts_with_bundle_interior(&path[at + name.len()..])
  })
}

/// Whether `rest` opens with [`BUNDLE_INTERIOR`], folded case — the
/// default filesystem holds `Contents` with that spelling and plenty of
/// bundles arrive lowercased from a Windows-authored download.
fn starts_with_bundle_interior(rest: &[u8]) -> bool {
  rest.len() >= BUNDLE_INTERIOR.len()
    && rest[..BUNDLE_INTERIOR.len()].eq_ignore_ascii_case(BUNDLE_INTERIOR)
}
