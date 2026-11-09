//! Darwin-Java compatibility rules: macOS `>java` database entries whose
//! declared arguments identify a Java game running natively on Linux
//! (the GameDetection rule), plus the Minecraft loader expansion.

use std::sync::Arc;

use adsum_types::DetectSource;

use crate::bundle::DetectablesBundle;

use crate::matching::live_or_none;
use crate::types::{Exec, ScannedEntry, ScannedHit};

/// The vanilla Minecraft client main class: the only declared argument
/// that unlocks the loader expansion.
const VANILLA_MINECRAFT_MAIN: &str = "net.minecraft.client.main.Main";

/// Loader and launcher signatures appended to the vanilla entry. Client
/// targets only: `fmlserver`-style server launch targets stay out.
const MINECRAFT_CLIENT_SIGNATURES: &[&str] = &[
  "--launchTarget fmlclient",
  "--launchTarget forgeclient",
  "--launchTarget forge_client",
  "--launchTarget neoforgeclient",
  "net.neoforged.fml.startup.Client",
  "net.fabricmc.loader.impl.launch.knot.KnotClient",
  "net.fabricmc.loader.launch.knot.KnotClient",
  "org.quiltmc.loader.impl.launch.knot.KnotClient",
  "net.minecraft.launchwrapper.Launch",
  "org.prismlauncher.EntryPoint",
  "org.polymc.EntryPoint",
  "org.multimc.EntryPoint",
  "com.modrinth.theseus.MinecraftLaunch",
];

/// Where a compatibility rule came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CompatibilityOrigin {
  /// The database's own macOS `>java` executable and its arguments.
  DarwinJavaOnLinux,
  /// A static signature appended to the vanilla Minecraft entry.
  MinecraftLoaderExpansion,
}

/// What arguments a rule requires of the process command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequiredArguments {
  /// Reuses the original executable's `arguments` field.
  DatabaseExecutable,
  /// A static signature appended by the expansion.
  Static(&'static str),
}

/// One derived rule: an index pair into the same bundle generation plus
/// the arguments it requires. No owned text — the entry lives in the
/// same generation and static signatures are constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompatibilityRule {
  pub entry_index: u32,
  pub executable_index: u16,
  pub required_arguments: RequiredArguments,
  pub origin: CompatibilityOrigin,
}

/// Rules derived at bundle build time, immutable for the generation.
#[derive(Debug, Default)]
pub(crate) struct CompatibilityBundle {
  pub rules: Box<[CompatibilityRule]>,
}

/// Whether an executable is a Darwin Java compatibility candidate on
/// Linux: `os == darwin`, exact `>java` name, not a launcher, and
/// non-empty declared arguments. `target_os` is passed explicitly so
/// every platform branch is testable from one machine.
fn is_darwin_java_compat_candidate(executable: &crate::types::ScannedExe, target_os: &str) -> bool {
  target_os == "linux"
    && executable.os.as_str() == "darwin"
    && executable.name.as_ref() == ">java"
    && !executable.is_launcher
    && executable
      .arguments
      .as_deref()
      .is_some_and(|args| !args.trim().is_empty())
}

/// Build the compatibility rules for one bundle generation. Only Linux
/// derives them; other platforms get an empty bundle (macOS runs its
/// Darwin entries natively, Windows has no Java rule).
pub(crate) fn build_compatibility_bundle(
  entries: &[Arc<ScannedEntry>],
  target_os: &str,
) -> CompatibilityBundle {
  if target_os != "linux" {
    return CompatibilityBundle::default();
  }
  let mut rules = Vec::new();
  for (entry_index, entry) in entries.iter().enumerate() {
    for (executable_index, executable) in entry.executables.iter().enumerate() {
      if !is_darwin_java_compat_candidate(executable, target_os) {
        continue;
      }
      let Some(arguments) = executable.arguments.as_deref() else {
        continue;
      };
      let entry_index = u32::try_from(entry_index).unwrap_or(u32::MAX);
      let executable_index = u16::try_from(executable_index).unwrap_or(u16::MAX);
      rules.push(CompatibilityRule {
        entry_index,
        executable_index,
        required_arguments: RequiredArguments::DatabaseExecutable,
        origin: CompatibilityOrigin::DarwinJavaOnLinux,
      });
      // Only the vanilla signature unlocks the loader expansion: other
      // Java entries must never grow Minecraft rules.
      if arguments == VANILLA_MINECRAFT_MAIN {
        rules.extend(
          MINECRAFT_CLIENT_SIGNATURES
            .iter()
            .map(|signature| CompatibilityRule {
              entry_index,
              executable_index,
              required_arguments: RequiredArguments::Static(signature),
              origin: CompatibilityOrigin::MinecraftLoaderExpansion,
            }),
        );
      }
    }
  }
  // Deterministic dedup: identity includes the resolved argument text so
  // two executables on the same entry with different arguments stay distinct.
  let rule_key = |rule: &CompatibilityRule| {
    let args = entries
      .get(rule.entry_index as usize)
      .and_then(|entry| required_arguments(rule, entry))
      .unwrap_or("");
    (rule.entry_index, rule.origin, args)
  };
  rules.sort_by(|a, b| rule_key(a).cmp(&rule_key(b)));
  rules.dedup_by(|a, b| rule_key(a) == rule_key(b));
  if !rules.is_empty() {
    tracing::info!(
      "[Compatibility] Built {} Darwin-Java compatibility rules for Linux",
      rules.len()
    );
  }
  CompatibilityBundle {
    rules: rules.into_boxed_slice(),
  }
}

/// The arguments a rule requires, borrowed from the generation's entry.
/// The arguments a rule requires, borrowed from the generation's entry.
fn required_arguments<'a>(rule: &CompatibilityRule, entry: &'a ScannedEntry) -> Option<&'a str> {
  match rule.required_arguments {
    RequiredArguments::DatabaseExecutable => {
      let exe = entry.executables.get(rule.executable_index as usize)?;
      // An empty requirement means nothing to match (same as the
      // automaton tier treats an absent field).
      let args = exe.arguments.as_deref()?;
      (!args.trim().is_empty()).then_some(args)
    }
    RequiredArguments::Static(signature) => Some(signature),
  }
}

/// Match a Linux Java process against the compatibility rules.
///
/// Gated on a `java` basename (case-insensitive) so non-Java processes
/// pay only one cheap compare. Rules resolving to different application
/// ids are refused as ambiguous rather than answered by scan order.
pub(crate) fn match_compatibility(
  process: &Exec,
  bundle: &DetectablesBundle,
  stat: &mut String,
) -> Option<ScannedHit> {
  let rules = &bundle.compatibility.rules;
  if rules.is_empty() {
    return None;
  }
  let basename = process.path.rsplit('/').next().unwrap_or(&process.path);
  if !basename.eq_ignore_ascii_case("java") {
    return None;
  }
  let mut matched: Option<&CompatibilityRule> = None;
  for rule in rules {
    let entry = bundle.list.get(rule.entry_index as usize)?;
    let Some(required) = required_arguments(rule, entry) else {
      continue;
    };
    // Whole-token sequence match anywhere in argv, using the on-demand
    // Exec helper (no per-token allocation).
    let mut found_sequence = false;
    let mut start = 0usize;
    while process.argument_at(start).is_some() {
      if process.has_argument_sequence(start, required) {
        found_sequence = true;
        break;
      }
      start += 1;
    }
    if required.split_ascii_whitespace().count() == 0 {
      found_sequence = true;
    }
    if !found_sequence {
      continue;
    }
    match matched {
      None => matched = Some(rule),
      Some(previous) => {
        if bundle.list.get(previous.entry_index as usize)?.id != entry.id {
          tracing::debug!(
            "[Compatibility] Ambiguous Java match for pid {}: {} and {} declare the same signature",
            process.pid,
            bundle.list.get(previous.entry_index as usize)?.name,
            entry.name
          );
          return None;
        }
        // Same application id: one detection either way.
      }
    }
  }
  let rule = matched?;
  let entry = bundle.list.get(rule.entry_index as usize)?;
  live_or_none(
    entry,
    process.pid,
    DetectSource::Compatibility,
    process.path.as_str(),
    stat,
  )
}

#[cfg(test)]
#[path = "../tests/unit/compatibility/mod.rs"]
mod tests;
