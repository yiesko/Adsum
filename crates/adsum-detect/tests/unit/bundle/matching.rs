use super::*;

/// Empty executable names must not reach the automaton: `"/"` matches
/// every reversed path and would detect unrelated processes.
#[test]
fn empty_exe_names_build_no_patterns() {
  let bundle = build_bundle(
    Arc::from(vec![entry_with_exes(
      "1",
      OsName::Empty,
      &["", "\\", ">", "/", "game.exe"],
    )]),
    Arc::from(Vec::new()),
  )
  .expect("builds");
  // Only the real exe survives, with its index intact (every name that
  // normalizes to the match-all `"/"` contributes neither pattern nor
  // index).
  assert_eq!(bundle.indexes[0].resolve(), (0, 4));
  assert_eq!(bundle.ac.patterns_len(), 1);
}

/// Same guard in the Proton fallback: an empty win32 name leaves no automaton.
/// (`_bundle`: the assertion is Linux-only, so other platforms must not
/// see an unused binding under `-D warnings`.)
#[test]
fn proton_builder_ignores_empty_names() {
  let _bundle = build_bundle(
    Arc::from(vec![entry_with_exes("1", OsName::Win32, &[""])]),
    Arc::from(Vec::new()),
  )
  .expect("builds");
  #[cfg(target_os = "linux")]
  assert!(
    _bundle.proton_ac.is_none(),
    "empty name must not arm the Proton automaton"
  );
}

/// Fused collection preserves each pass's contents: launchers and `/`
/// names in neither pass, empty-OS in native, win32 in proton on Linux.
#[test]
fn split_collection_preserves_pass_contents() {
  let entries = vec![
    entry_with_exes("lin", OsName::Linux, &["lin.bin"]),
    entry_with_exes("win", OsName::Win32, &["win.exe"]),
    entry_with_exes("dar", OsName::Darwin, &["mac.app"]),
    entry_with_exes("empty", OsName::Empty, &["any.bin"]),
    entry_with_exes("slash", OsName::Linux, &["", "ok.bin"]),
    launcher_entry("setup"),
  ];
  let split = collect_split_patterns(&entries);
  // No launcher in either pass, ever. Match-all `/` is checked on the
  // normalized patterns below (raw names can differ from what the
  // automaton stores).
  for pair in split.native_idx.iter().chain(split.proton_idx.iter()) {
    let (activity, exe) = pair.resolve();
    let exe = &entries[activity].executables[exe];
    assert!(!exe.is_launcher, "launcher leaked into patterns");
  }
  assert!(
    !split.native_patterns.iter().any(|p| p == "/"),
    "match-all `/` leaked into native patterns"
  );
  assert!(
    !split.proton_patterns.iter().any(|p| p == "/"),
    "match-all `/` leaked into proton patterns"
  );
  // Proton (Linux only) carries exactly the win32 non-launcher exes.
  #[cfg(target_os = "linux")]
  {
    let native_at = |pair: &PatternIndex| {
      let (activity, exe) = pair.resolve();
      entries[activity].executables[exe].name.to_string()
    };
    assert_eq!(split.proton_idx.len(), 1);
    assert_eq!(native_at(&split.proton_idx[0]), "win.exe");
    // Native keeps linux + empty-OS, in walk order, excluding `/` drops.
    let native_names: Vec<String> = split.native_idx.iter().map(native_at).collect();
    assert!(native_names.contains(&"lin.bin".to_string()));
    assert!(native_names.contains(&"any.bin".to_string()));
    assert!(native_names.contains(&"ok.bin".to_string()));
    assert!(
      !native_names
        .iter()
        .any(|name| name == "win.exe" || name == "mac.app")
    );
  }
  // Contents above are the real check: capacity claims on empty-start
  // vectors would only restate the `Vec` invariant.
  assert_eq!(split.native_patterns.len(), split.native_idx.len());
  #[cfg(target_os = "linux")]
  assert_eq!(split.proton_patterns.len(), split.proton_idx.len());
}

/// The marker strip is `str::replace` without its allocation.
///
/// The hand-written loop is the whole risk here: dropping an occurrence,
/// or dropping one after a match that consumed a shared byte, would
/// silently change which database entry a process matches. Compared
/// against the standard library for every interesting shape — including
/// multiple occurrences in one path, which is what `/usr/lib64/x64/…`
/// gives.
#[test]
fn remove_marker_matches_str_replace() {
  for (path, marker) in [
    ("/games/wow64.exe", "64"),
    ("/usr/lib64/x64/game64.exe", "64"),
    ("/usr/lib64/x64/game64.exe", "x64"),
    ("a.x64b.x64c", ".x64"),
    ("no marker here", "x64"),
    ("/games/wow.exe", "64"),
    ("64", "64"),
    ("", "64"),
    ("646464", "64"),
  ] {
    let mut buf = String::new();
    remove_marker_into(path, marker, &mut buf);
    assert_eq!(buf, path.replace(marker, ""), "{path} minus {marker:?}");
  }
}

/// Path variants still dedup, stay bounded, and lead with the raw path.
///
/// `/usr/lib64/x64/game64.exe` matches two markers, and the `64` strip
/// removes **three** occurrences — including the one inside `x64`. That is
/// the shape a first-occurrence-only implementation gets wrong, and the
/// reason the previous test exists.
#[test]
fn path_variants_strip_every_marker_and_stay_bounded() {
  let mut out: [String; 5] = Default::default();
  let count = path_variants_into("/usr/lib64/x64/game64.exe", &mut out);

  assert_eq!(out[0], "/usr/lib64/x64/game64.exe", "the raw path leads");
  assert_eq!(count, 3, "one raw plus two distinct marker strips");
  // Every `64` goes, the one inside `x64` included: `64` is a substring of
  // `x64`, and `str::replace` removes all of them.
  assert_eq!(out[1], "/usr/lib/x/game.exe");
  assert_eq!(out[2], "/usr/lib64//game64.exe");
  assert!(count <= out.len(), "never past the caller's slots");
}

/// A path with no markers yields exactly the raw path: no strip, no
/// dedup work, nothing written.
#[test]
fn a_plain_path_yields_itself() {
  let mut out: [String; 5] = Default::default();
  let count = path_variants_into("/games/terraria", &mut out);
  assert_eq!(count, 1);
  assert_eq!(out[0], "/games/terraria");
}

/// The collected name table reserves its exact bound, not twice the
/// entry count.
///
/// One slot per entry plus one per alias is the most the table can ever
/// hold, so reserving that is both slack-free and growth-free. The old
/// `len * 2` over-bought by the alias deficit: at ~24k entries with few
/// aliases that is most of a megabyte of scratch held only for the
/// duration of the build, paid at every refresh and override append.
///
/// Asserted on `collect_names` — the function that both reserves and
/// fills — because a bound tested in isolation says nothing about the
/// call site. The alias counts are chosen so growth cannot land on the
/// bound by accident: with few aliases `len * 2` is already too big, and
/// with many it overshoots on the last doubling.
#[test]
fn collected_names_reserve_their_exact_bound() {
  for (entries, aliases) in [(100usize, 0usize), (100, 5)] {
    let fixtures: Vec<Arc<ScannedEntry>> =
      (0..entries).map(|_| entry_with_aliases(aliases)).collect();
    let names = collect_names(&fixtures);
    let bound = name_map_bound(&fixtures);

    assert_eq!(
      names.len(),
      bound,
      "every reserved slot is filled ({entries} entries, {aliases} aliases)"
    );
    assert_eq!(
      names.capacity(),
      bound,
      "the collected table must reserve exactly its bound, no doubling slack \
         ({entries} entries, {aliases} aliases)"
    );
  }
}

/// The custom-override index releases its growth slack like the native
/// and Proton tables do.
///
/// `custom_indexes` was the only stored table left holding amortized
/// doubling slack. The native and Proton ones get `shrink_to_fit` after
/// their source vectors are dropped, so the bundle's tables are exactly
/// their length; the custom one grew from empty with no bound and kept
/// whatever the last doubling left it.
#[test]
fn custom_index_releases_its_growth_slack() {
  // Five entries force at least one doubling (`Vec` grows 4 → 8), so
  // there is slack to release. One exe each: one pattern per entry.
  let custom: Vec<Arc<ScannedEntry>> = (0..5)
    .map(|i| entry_with_exes(&format!("c{i}"), OsName::Linux, &["custom.bin"]))
    .collect();
  let detectable: Vec<Arc<ScannedEntry>> = (0..3)
    .map(|i| entry_with_exes(&format!("d{i}"), OsName::Linux, &["native.bin"]))
    .collect();
  let bundle =
    build_bundle(Arc::from(detectable), Arc::from(custom)).expect("custom automaton builds");

  assert!(
    bundle.custom_indexes.len() >= 5,
    "the custom patterns were built (got {})",
    bundle.custom_indexes.len()
  );
  assert_eq!(
    bundle.custom_indexes.capacity(),
    bundle.custom_indexes.len(),
    "the custom index must store exactly its length, not doubling slack"
  );
  // The sibling tables keep the same contract, so a later change cannot
  // release one and forget the others.
  assert_eq!(
    bundle.indexes.capacity(),
    bundle.indexes.len(),
    "the native index must store exactly its length"
  );
}
