use super::*;

/// Corpus covering case, forbidden punctuation, dots, whitespace
/// runs, empties, separators, `>` prefix and non-ASCII.
const CORPUS: &[&str] = &[
  "Game Name",
  "Name: Subtitle",
  "R.E.P.O. Ghost Haul",
  "Q.U.B.E.",
  "Mr. Bomber",
  "a  b   c",
  "",
  "   ",
  "a/b\\c",
  ">game",
  "/",
  "fish",
  "École: LÉGENDE",
  "MECCHA CHAMELEON",
  "PenguinHotel-Win64-Shipping.exe",
  "  padded  ",
  "a.b.c",
  "...",
];

/// `normalize_name_into` reproduces `normalize_name` byte-for-byte.
#[test]
fn normalize_into_matches_normalize() {
  let mut lower = String::new();
  let mut out = String::new();
  for input in CORPUS {
    normalize_name_into(input, &mut lower, &mut out);
    assert_eq!(&out, &normalize_name(input), "mismatch for {input:?}");
  }
}

/// On already-lowercase input, the lowered variant (no lowercase
/// stage) matches too — this is the hot path (`&lowered`).
#[test]
fn normalize_lowered_into_matches_on_lowercase_input() {
  let mut out = String::new();
  for input in CORPUS {
    let lowered = input.to_ascii_lowercase();
    normalize_lowered_into(&lowered, &mut out);
    assert_eq!(&out, &normalize_name(input), "mismatch for {input:?}");
  }
}

/// The dedotted variant matches `normalize_name(&dedot(_))`: dots
/// become spaces, runs collapse, same as replace-then-normalize.
#[test]
fn normalize_lowered_dedotted_into_matches_dedot_then_normalize() {
  use crate::bundle::dedot;
  let mut out = String::new();
  for input in CORPUS {
    let lowered = input.to_ascii_lowercase();
    normalize_lowered_dedotted_into(&lowered, &mut out);
    assert_eq!(
      &out,
      &normalize_name(&dedot(input)),
      "mismatch for {input:?}"
    );
  }
}

/// Scratch buffers are stable across calls: no reallocations on
/// repeated use.
#[test]
fn match_scratch_buffers_are_stable() {
  let mut scratch = MatchScratch::default();
  let mut lower = String::new();
  normalize_name_into("Some Game: Title", &mut lower, &mut scratch.norm_out);
  normalize_lowered_into("some game title", &mut scratch.norm_out);
  let out_ptr = scratch.norm_out.as_ptr();
  let out_cap = scratch.norm_out.capacity();
  let lower_ptr = lower.as_ptr();
  normalize_name_into("Other: Name Here", &mut lower, &mut scratch.norm_out);
  normalize_lowered_into("other name here", &mut scratch.norm_out);
  assert!(
    std::ptr::eq(out_ptr, scratch.norm_out.as_ptr()),
    "norm_out moved"
  );
  assert!(std::ptr::eq(lower_ptr, lower.as_ptr()), "lower moved");
  assert!(scratch.norm_out.capacity() >= out_cap);
}
