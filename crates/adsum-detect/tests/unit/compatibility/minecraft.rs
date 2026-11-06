//! Minecraft detection tests: the full matcher path — build the bundle
//! from the Darwin `>java` entry, then match real command lines against
//! it. Positive, negative and ambiguity cases.

use std::sync::Arc;

use crate::bundle::{DetectablesBundle, build_bundle};
use crate::compatibility::match_compatibility;
use crate::types::Exec;
use crate::types::{DetectSource, OsName, ScannedEntry, ScannedExe, ScannedHit};

/// A Minecraft-detectable entry plus the bundle built from it.
fn fixture() -> (Arc<ScannedEntry>, DetectablesBundle) {
  let entry = Arc::new(ScannedEntry {
    id: "1402418491272986635".into(),
    name: "Minecraft".into(),
    executables: Box::new([ScannedExe {
      name: ">java".into(),
      os: OsName::Darwin,
      is_launcher: false,
      arguments: Some("net.minecraft.client.main.Main".into()),
    }]),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let entries: Arc<[Arc<ScannedEntry>]> = Arc::from(vec![entry]);
  let bundle = build_bundle(entries, Arc::from(Vec::new())).expect("bundle builds");
  let entry = bundle.list[0].clone();
  (entry, bundle)
}

/// One process: a `java` basename and a command line.
fn java_process(arguments: &str) -> Exec {
  Exec {
    pid: 1000,
    path: "/usr/lib/jvm/bin/java".into(),
    arguments: Some(arguments.into()),
  }
}

fn is_minecraft(hit: &ScannedHit) -> bool {
  hit.entry.id.as_ref() == "1402418491272986635"
}

/// Positive: every client signature detects as `compatibility`.
#[test]
fn client_signatures_detect() {
  let cases = [
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
  let (entry, bundle) = fixture();
  let mut stat = String::new();
  for signature in cases {
    let process = java_process(&format!("-Xmx4G -cp libraries.jar {signature} --gameDir ."));
    let hit = match_compatibility(&process, &bundle, &mut stat)
      .unwrap_or_else(|| panic!("{signature} must detect"));
    assert!(is_minecraft(&hit), "{signature} resolves to Minecraft");
    assert_eq!(hit.source, DetectSource::Compatibility);
    assert_eq!(hit.entry.name, entry.name);
  }
}

/// The vanilla main class itself detects (the database rule).
#[test]
fn vanilla_main_class_detects() {
  let (_, bundle) = fixture();
  let mut stat = String::new();
  let process = java_process("-Xmx2G net.minecraft.client.main.Main --version 1.21");
  let hit = match_compatibility(&process, &bundle, &mut stat).expect("vanilla detects");
  assert!(is_minecraft(&hit));
}

/// Server launch targets and server main classes must never detect: the
/// loader classes are shared with dedicated servers.
#[test]
fn server_launches_do_not_detect() {
  let (_, bundle) = fixture();
  let mut stat = String::new();
  for arguments in [
    "--launchTarget fmlserver",
    "--launchTarget neoforgeserver",
    "net.minecraft.server.Main",
    "net.minecraftforge.fml.server.ServerMain",
  ] {
    let process = java_process(&format!("-Xmx2G {arguments} nogui"));
    assert!(
      match_compatibility(&process, &bundle, &mut stat).is_none(),
      "{arguments} is a server launch, not the game"
    );
  }
}

/// A token carrying the signature as a substring or suffix is refused:
/// the match is whole-token, exact.
#[test]
fn partial_tokens_do_not_detect() {
  let (_, bundle) = fixture();
  let mut stat = String::new();
  for arguments in [
    "-cp app.jar net.minecraft.client.main.MainX",
    "-cp app.jar Xnet.minecraft.client.main.Main",
    "--launchTargetX fmlclient",
    "--launchTarget fmlclientX",
  ] {
    let process = java_process(arguments);
    assert!(
      match_compatibility(&process, &bundle, &mut stat).is_none(),
      "{arguments} must not detect"
    );
  }
}

/// A different Java application with no matching signature never becomes
/// Minecraft.
#[test]
fn other_java_processes_do_not_detect() {
  let (_, bundle) = fixture();
  let mut stat = String::new();
  for arguments in [
    "-jar app.jar",
    "-version",
    "org.elasticsearch.bootstrap.Elasticsearch",
  ] {
    let process = java_process(arguments);
    assert!(
      match_compatibility(&process, &bundle, &mut stat).is_none(),
      "{arguments} is not a game"
    );
  }
}

/// The basename gate: only a `java` executable reaches the rules.
#[test]
fn non_java_executables_do_not_detect() {
  let (_, bundle) = fixture();
  let mut stat = String::new();
  for path in [
    "/usr/bin/javaw",
    "/usr/lib/jvm/bin/javac",
    "/opt/game/launcher",
  ] {
    let process = Exec {
      pid: 1000,
      path: path.into(),
      arguments: Some("net.minecraft.client.main.Main".into()),
    };
    assert!(
      match_compatibility(&process, &bundle, &mut stat).is_none(),
      "{path} is not the Java runtime"
    );
  }
}

/// A case-insensitive basename still matches: `Java` is the same runtime.
#[test]
fn java_basename_matches_case_insensitively() {
  let (_, bundle) = fixture();
  let mut stat = String::new();
  let process = Exec {
    pid: 1000,
    path: "/opt/JDK/Java".into(),
    arguments: Some("-cp x.jar net.minecraft.client.main.Main".into()),
  };
  let hit = match_compatibility(&process, &bundle, &mut stat).expect("`Java` detects");
  assert!(is_minecraft(&hit));
}

/// An entry whose declared arguments are not the vanilla signature gets
/// its database rule only: no loader expansion without the vanilla base.
#[test]
fn non_vanilla_entry_has_no_loader_signatures() {
  let entries: Arc<[Arc<ScannedEntry>]> = Arc::from(vec![Arc::new(ScannedEntry {
    id: "999".into(),
    name: "Other Java Game".into(),
    executables: Box::new([ScannedExe {
      name: ">java".into(),
      os: OsName::Darwin,
      is_launcher: false,
      arguments: Some("com.other.game.Main".into()),
    }]),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  })]);
  let bundle = build_bundle(entries, Arc::from(Vec::new())).expect("bundle builds");
  let mut stat = String::new();
  let process = java_process("--launchTarget fmlclient");
  assert!(
    match_compatibility(&process, &bundle, &mut stat).is_none(),
    "loader signatures never leak into a non-vanilla entry"
  );
}

/// Two entries declaring the same signature are ambiguous: refused, not
/// answered by scan order.
#[test]
fn ambiguous_entries_are_refused() {
  let mk = |id: &str| {
    Arc::new(ScannedEntry {
      id: id.into(),
      name: format!("Game {id}").into(),
      executables: Box::new([ScannedExe {
        name: ">java".into(),
        os: OsName::Darwin,
        is_launcher: false,
        arguments: Some("net.minecraft.client.main.Main".into()),
      }]),
      steam_ids: Box::new([]),
      epic_ids: Box::new([]),
      aliases: Box::new([]),
    })
  };
  let entries: Arc<[Arc<ScannedEntry>]> = Arc::from(vec![mk("1"), mk("2")]);
  let bundle = build_bundle(entries, Arc::from(Vec::new())).expect("bundle builds");
  let mut stat = String::new();
  let process = java_process("-cp game.jar net.minecraft.client.main.Main");
  assert!(
    match_compatibility(&process, &bundle, &mut stat).is_none(),
    "two different applications with the same signature must be refused"
  );
}
