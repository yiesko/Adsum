//! Builder eligibility tests: only a Linux build derives compatibility
//! rules, and only from a Darwin `>java` executable with non-empty
//! arguments that is not a launcher. Everything else derives nothing.

use std::sync::Arc;

use crate::compatibility::{CompatibilityBundle, CompatibilityOrigin, build_compatibility_bundle};
use crate::types::{OsName, ScannedEntry, ScannedExe};

fn entry(id: &str, name: &str, executables: Box<[ScannedExe]>) -> Arc<ScannedEntry> {
  Arc::new(ScannedEntry {
    id: id.into(),
    name: name.into(),
    executables,
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  })
}

fn java_exe(os: OsName, name: &str, arguments: Option<&str>, launcher: bool) -> ScannedExe {
  ScannedExe {
    name: name.into(),
    os,
    is_launcher: launcher,
    arguments: arguments.map(Box::from),
  }
}

fn rule_count(rules: &CompatibilityBundle) -> usize {
  rules.rules.len()
}

/// The canonical case: Minecraft's own macOS entry on a Linux build.
#[test]
fn linux_darwin_java_vanilla_builds_fourteen_rules() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1402418491272986635",
      "Minecraft",
      Box::new([java_exe(
        OsName::Darwin,
        ">java",
        Some("net.minecraft.client.main.Main"),
        false,
      )]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 14);
  assert_eq!(
    rules.rules[0].origin,
    CompatibilityOrigin::DarwinJavaOnLinux
  );
}

/// No declared arguments, nothing to match the command line by.
#[test]
fn linux_darwin_java_without_arguments_builds_none() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([java_exe(OsName::Darwin, ">java", None, false)]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 0);
}

/// The `>` marker is required: a bare `java` name is not this rule.
#[test]
fn linux_darwin_java_without_marker_builds_none() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([java_exe(OsName::Darwin, "java", Some("x"), false)]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 0);
}

/// `>javaw` is the Windows helper binary, not the runtime.
#[test]
fn linux_darwin_javaw_builds_none() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([java_exe(OsName::Darwin, ">javaw", Some("x"), false)]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 0);
}

/// Launchers are filtered before the compatibility derivation.
#[test]
fn linux_darwin_java_launcher_builds_none() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([java_exe(OsName::Darwin, ">java", Some("x"), true)]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 0);
}

/// `win32` entries belong to the Proton tier, never this rule.
#[test]
fn linux_win32_java_builds_none() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([java_exe(OsName::Win32, ">java", Some("x"), false)]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 0);
}

/// A native Linux entry is already served by the main automaton.
#[test]
fn linux_native_java_builds_none() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([java_exe(OsName::Linux, ">java", Some("x"), false)]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 0);
}

/// macOS runs its Darwin entries natively: no derived duplicates.
#[test]
fn macos_darwin_java_builds_none() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([java_exe(OsName::Darwin, ">java", Some("x"), false)]),
    )],
    "macos",
  );
  assert_eq!(rule_count(&rules), 0);
}

/// Windows has no Java compatibility rule at all.
#[test]
fn windows_darwin_java_builds_none() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([java_exe(OsName::Darwin, ">java", Some("x"), false)]),
    )],
    "windows",
  );
  assert_eq!(rule_count(&rules), 0);
}

/// Whitespace-only arguments carry no identity.
#[test]
fn linux_darwin_java_whitespace_arguments_build_none() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([java_exe(OsName::Darwin, ">java", Some("   "), false)]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 0);
}

/// Two identical executables on one entry derive one rule, not two.
#[test]
fn duplicate_executables_deduplicate() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Game",
      Box::new([
        java_exe(
          OsName::Darwin,
          ">java",
          Some("net.minecraft.client.main.Main"),
          false,
        ),
        java_exe(
          OsName::Darwin,
          ">java",
          Some("net.minecraft.client.main.Main"),
          false,
        ),
      ]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 14, "1 database rule + 13 loader rules");
}

/// The vanilla signature unlocks exactly the thirteen loader rules.
#[test]
fn vanilla_signature_unlocks_loader_expansion() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1402418491272986635",
      "Minecraft",
      Box::new([java_exe(
        OsName::Darwin,
        ">java",
        Some("net.minecraft.client.main.Main"),
        false,
      )]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 14);
  assert_eq!(
    rules
      .rules
      .iter()
      .filter(|rule| rule.origin == CompatibilityOrigin::MinecraftLoaderExpansion)
      .count(),
    13
  );
}

/// A non-vanilla Java entry never grows loader rules.
#[test]
fn non_vanilla_java_has_no_loader_expansion() {
  let rules = build_compatibility_bundle(
    &[entry(
      "1",
      "Other Java Game",
      Box::new([java_exe(
        OsName::Darwin,
        ">java",
        Some("com.other.game.Main"),
        false,
      )]),
    )],
    "linux",
  );
  assert_eq!(rule_count(&rules), 1);
  assert!(
    rules
      .rules
      .iter()
      .all(|rule| rule.origin == CompatibilityOrigin::DarwinJavaOnLinux)
  );
}
