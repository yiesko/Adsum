//! Name normalization for the map keys and the lookup side alike: one
//! consistent rule (ASCII lowercase, Windows-forbidden punctuation
//! folded to spaces, runs collapsed) applied at BUILD time (bundle aux
//! maps) and at MATCH time (exe stems, folder components) — so the two
//! sides can never drift apart. The `_into` variants refill caller-owned
//! buffers: the hot path (folder walks) allocates nothing.

/// Lowercase-trim a name for map keys and lookups. ASCII-only by
/// design: every lookup side lowercases ASCII too, and the AC automaton
/// matches `ascii_case_insensitive` — one consistent rule for a
/// Windows-centric database, instead of half the paths folding Unicode
/// and the other half not.
///
/// Punctuation forbidden in Windows filenames (`: ? " < > | * / \`)
/// is dropped (via a space, runs collapsed): no real folder or exe stem
/// can ever contain those characters, so a DB title like `Name:
/// Subtitle` still matches its on-disk `Name Subtitle` folder — while
/// the multi-word/length gate keeps generic collisions out exactly as
/// before.
#[inline]
pub fn normalize_name(name: &str) -> String {
  const FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
  name
    .trim()
    .to_ascii_lowercase()
    .replace(&FORBIDDEN[..], " ")
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
}

/// Punctuation forbidden in Windows filenames (see [`normalize_name`]).
/// Shared by the allocating original and the reuse variants below.
const FORBIDDEN_NAME_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Refill `out` with the normalized form of `name`, reusing both
/// buffers: byte-identical to [`normalize_name`]. Single pass —
/// lowercase into `lower`, then split on forbidden/whitespace (runs
/// collapse, leading/trailing trimmed by construction) straight into
/// `out`, with no temporaries — no `replace` String, no pieces `Vec`,
/// no join.
pub fn normalize_name_into(name: &str, lower: &mut String, out: &mut String) {
  lower.clear();
  lower.extend(name.trim().chars().map(|c| c.to_ascii_lowercase()));
  normalize_lowered_into(lower, out);
}

/// Refill `out` with the normalized form of already-lowercase `name`
/// (the hot path feeds `&lowered`): skips the lowercase stage entirely.
/// Byte-identical to [`normalize_name`] on lowercase input.
pub fn normalize_lowered_into(name: &str, out: &mut String) {
  out.clear();
  let mut first = true;
  for word in name
    .split(|c: char| c.is_whitespace() || FORBIDDEN_NAME_CHARS.contains(&c))
    .filter(|word| !word.is_empty())
  {
    if !first {
      out.push(' ');
    }
    first = false;
    out.push_str(word);
  }
}

/// Refill `out` with the de-dotted normalized form of already-lowercase
/// `name`: byte-identical to [`normalize_name`] applied to `dedot(name)`
/// (dots become spaces before the same collapse).
pub fn normalize_lowered_dedotted_into(name: &str, out: &mut String) {
  out.clear();
  let mut first = true;
  for word in name
    .split(|c: char| c.is_whitespace() || c == '.' || FORBIDDEN_NAME_CHARS.contains(&c))
    .filter(|word| !word.is_empty())
  {
    if !first {
      out.push(' ');
    }
    first = false;
    out.push_str(word);
  }
}

/// Conservative gate for the exe-stem fallback: exact, multi-word names with
/// a minimum length. Keeps generic stems (`fish`, `steam`, `game`, `reaper`)
/// from ever matching same-named DB entries.
#[inline]
pub fn name_matchable(normalized: &str) -> bool {
  normalized.contains(' ') && normalized.chars().count() >= 6
}
