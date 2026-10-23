use super::*;

/// RAII temp dir (std-only): unique per test, removed on drop.
struct TempDir {
  path: PathBuf,
}

impl TempDir {
  fn new(tag: &str) -> Self {
    let path = std::env::temp_dir().join(format!(
      "adsum-ota-test-{}-{}-{}",
      std::process::id(),
      tag,
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("test tempdir");
    Self { path }
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

#[test]
fn parse_version_accepts_bare_and_v_prefixed_tags() {
  assert_eq!(parse_version("v1.2.3"), Some(Version::new(1, 2, 3)));
  assert_eq!(parse_version("1.2.3"), Some(Version::new(1, 2, 3)));
  assert_eq!(parse_version("  v0.32.2\n"), Some(Version::new(0, 32, 2)));
  assert!(parse_version("not-a-version").is_none());
  assert!(parse_version("").is_none());
}

#[test]
fn parse_release_extracts_version_and_assets() {
  let body: serde_json::Value = serde_json::from_str(
    r#"{"tag_name":"v0.33.0","assets":[
        {"name":"adsum-x86_64-unknown-linux-gnu","browser_download_url":"https://example.com/a"},
        {"name":"SHA256SUMS.txt","browser_download_url":"https://example.com/s"},
        {"name":"notes.txt"}
      ]}"#,
  )
  .expect("fixture");
  let release = parse_release(&body).expect("release");
  assert_eq!(release.version, Version::new(0, 33, 0));
  assert_eq!(release.tag, "v0.33.0");
  // The asset without URL is skipped, the rest survive.
  assert_eq!(release.assets.len(), 2);
  let binary = select_asset(&release, "adsum-x86_64-unknown-linux-gnu").expect("binary");
  assert_eq!(binary.url, "https://example.com/a");
  assert!(select_asset(&release, "adsum-mips-unknown-linux-gnu").is_none());
}

#[test]
fn parse_release_rejects_bodies_without_version() {
  assert!(parse_release(&serde_json::json!({"assets": []})).is_none());
  assert!(parse_release(&serde_json::json!({"tag_name": "nope"})).is_none());
  assert!(parse_release(&serde_json::json!({})).is_none());
}

#[test]
fn parse_checksums_handles_both_sum_styles_and_skips_garbage() {
  let manifest = "abc123\n\
      d4e5f6  adsum-x86_64-unknown-linux-gnu\n\
      00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff *adsum-aarch64-unknown-linux-gnu\n\
      nothex  some-file\n";
  let sums = parse_checksums(manifest);
  assert_eq!(sums.len(), 1);
  assert_eq!(
    sums["adsum-aarch64-unknown-linux-gnu"],
    "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
  );
}

#[test]
fn sha256_hex_matches_known_vector() {
  assert_eq!(
    sha256_hex(b"abc"),
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
  );
}

/// Throwaway test keypair (secret destroyed after signing the fixture
/// below with the official minisign CLI — modern prehashed format, the
/// same flags CI uses).
const TEST_PUBKEY: &str = "RWRDXcpFovo7csyMYK21XfRae6S8DOp6cHF4YnP4a3/R7jITl4Jcd7fH";
const TEST_MANIFEST: &str = "deadbeef  adsum-x86_64-unknown-linux-gnu\n";
const TEST_MANIFEST_SIG: &str = "untrusted comment: signature from minisign secret key\n\
    RURDXcpFovo7clDWIF7FDbXjD6ltt1FlUPKr0kUz0LDToOJP+QAN2iSDXIcOgKxWleZPb1rAiS2IYccV5VNb7oRRADxflpMN+Qg=\n\
    trusted comment: adsum test fixture\n\
    lcV2OAEnIIDwFvo/0Jl0BU3dVex6OmTpYZw/MuvHFC691SB8ZuOC7fvAtbCkX5nGyZmBeV4FQITPo64nhFKUAw==\n";

#[test]
fn embedded_release_key_parses() {
  // Guards against transcription typos in UPDATE_PUBKEY: if the
  // embedded key does not even parse, every future stage() fails.
  assert!(minisign_verify::PublicKey::from_base64(UPDATE_PUBKEY).is_ok());
}

#[test]
fn valid_manifest_signature_verifies() {
  assert!(verify_signature(TEST_PUBKEY, TEST_MANIFEST.as_bytes(), TEST_MANIFEST_SIG).is_ok());
}

#[test]
fn tampered_manifest_fails_signature() {
  let tampered = TEST_MANIFEST.replace("deadbeef", "badcafe0");
  assert!(verify_signature(TEST_PUBKEY, tampered.as_bytes(), TEST_MANIFEST_SIG).is_err());
}

#[test]
fn signature_from_another_key_fails() {
  // The production key must reject the test fixture (key-id mismatch):
  // signatures are bound to their key, not just well-formed.
  assert!(verify_signature(UPDATE_PUBKEY, TEST_MANIFEST.as_bytes(), TEST_MANIFEST_SIG).is_err());
}

#[test]
fn garbage_signature_fails() {
  assert!(verify_signature(TEST_PUBKEY, TEST_MANIFEST.as_bytes(), "nope").is_err());
  assert!(
    verify_signature(
      TEST_PUBKEY,
      TEST_MANIFEST.as_bytes(),
      "untrusted comment: x\n!!!!\ntrusted comment: y\n!!!!\n"
    )
    .is_err()
  );
}

#[test]
fn eligibility_refuses_dev_cargo_and_foreign_binaries() {
  let tmp = TempDir::new("eligibility");
  // Dev build layout.
  let dev = tmp.path.join("target").join("debug").join("adsum");
  std::fs::create_dir_all(dev.parent().expect("parent")).expect("mkdir");
  std::fs::write(&dev, b"x").expect("write");
  assert!(matches!(check_eligibility(&dev), Err(Refusal::DevBuild(_))));
  // Cargo-managed install.
  let cargo = tmp.path.join(".cargo").join("bin").join("adsum");
  std::fs::create_dir_all(cargo.parent().expect("parent")).expect("mkdir");
  std::fs::write(&cargo, b"x").expect("write");
  assert!(matches!(
    check_eligibility(&cargo),
    Err(Refusal::CargoManaged(_))
  ));
  // Foreign binary name.
  let foreign = tmp.path.join("bin").join("something-else");
  std::fs::create_dir_all(foreign.parent().expect("parent")).expect("mkdir");
  std::fs::write(&foreign, b"x").expect("write");
  assert!(matches!(
    check_eligibility(&foreign),
    Err(Refusal::UnexpectedName(_))
  ));
  // A proper user-local layout passes.
  let good = tmp.path.join("local").join("bin").join("adsum");
  std::fs::create_dir_all(good.parent().expect("parent")).expect("mkdir");
  std::fs::write(&good, b"x").expect("write");
  assert!(check_eligibility(&good).is_ok());
}

#[test]
fn state_roundtrips_and_tolerates_garbage() {
  let tmp = TempDir::new("state");
  let paths = OtaPaths::with_dir(tmp.path.join("ota"));
  // Missing file -> default state.
  assert_eq!(load_state(&paths), OtaState::default());
  let state = OtaState {
    staged_version: Some("0.33.0".to_string()),
    staged_sha256: Some("abc".to_string()),
    latest_seen: Some("0.33.0".to_string()),
  };
  save_state(&paths, &state);
  assert_eq!(load_state(&paths), state);
  // Garbage file -> default state, never fatal.
  std::fs::write(paths.state_file(), "{nope").expect("write");
  assert_eq!(load_state(&paths), OtaState::default());
}

#[test]
fn env_override_points_verbatim_at_the_dir() {
  // Regression: ADSUM_OTA_DIR used to gain an extra adsum/ota suffix,
  // so staged state written there was never found on boot.
  let tmp = TempDir::new("envdir");
  let dir = tmp.path.join("custom");
  let previous = std::env::var_os(OTA_DIR_ENV);
  // SAFETY: single-threaded test process section touching only this
  // variable (no other test reads ADSUM_OTA_DIR); restored below.
  unsafe {
    std::env::set_var(OTA_DIR_ENV, &dir);
  }
  let resolved = OtaPaths::from_env();
  match previous {
    Some(value) => unsafe {
      std::env::set_var(OTA_DIR_ENV, value);
    },
    None => unsafe {
      std::env::remove_var(OTA_DIR_ENV);
    },
  }
  assert_eq!(resolved.state_file(), dir.join(STATE_FILE));
}

#[test]
fn prev_path_appends_a_single_suffix() {
  assert_eq!(
    prev_path(Path::new("/bin/adsum")),
    PathBuf::from("/bin/adsum.prev")
  );
  assert_eq!(
    prev_path(Path::new("C:/x/adsum.exe")),
    PathBuf::from("C:/x/adsum.exe.prev")
  );
}

#[test]
fn swap_binary_rotates_staged_over_exe() {
  let tmp = TempDir::new("swap");
  let exe = tmp.path.join("adsum");
  let staged = tmp.path.join("adsum.staged");
  std::fs::write(&exe, b"v1").expect("write");
  std::fs::write(&staged, b"v2").expect("write");

  swap_binary(&exe, &staged).expect("swap");

  assert_eq!(std::fs::read(&exe).expect("read"), b"v2");
  assert_eq!(std::fs::read(prev_path(&exe)).expect("read"), b"v1");
  assert!(!staged.exists());
}

#[test]
fn rollback_toggles_without_destroying_either_side() {
  // Regression: the old rollback deleted the prev file before moving
  // it back, so it printed success, changed nothing, and left no prev
  // behind. Distinct bytes on each side catch any content loss.
  let tmp = TempDir::new("rollback");
  let exe = tmp.path.join("adsum");
  let prev = prev_path(&exe);
  std::fs::write(&exe, b"v2-running").expect("write");
  std::fs::write(&prev, b"v1-kept").expect("write");

  swap_back(&exe, &prev).expect("first rollback");
  assert_eq!(std::fs::read(&exe).expect("read"), b"v1-kept");
  assert_eq!(std::fs::read(&prev).expect("read"), b"v2-running");

  swap_back(&exe, &prev).expect("second rollback");
  assert_eq!(std::fs::read(&exe).expect("read"), b"v2-running");
  assert_eq!(std::fs::read(&prev).expect("read"), b"v1-kept");
}

#[test]
fn rollback_after_reexec_is_a_noop() {
  // The re-exec passes the original argv again (`--rollback`), so
  // without the applied-guard the swap toggles exe/prev forever
  // (reproduced: 460 swaps in 3s). The guard makes that pass a no-op.
  // SAFETY: only this test touches the guard var in the test binary.
  unsafe { std::env::set_var(APPLIED_ENV, "1") };
  let result = cmd_rollback();
  // SAFETY: restores the value saved by the set above; no other test
  // reads or writes this variable.
  unsafe { std::env::remove_var(APPLIED_ENV) };
  result.expect("guarded rollback must return before touching the filesystem");
}

#[test]
fn rollback_without_prev_fails() {
  let tmp = TempDir::new("rollback-missing");
  let exe = tmp.path.join("adsum");
  std::fs::write(&exe, b"v1").expect("write");

  assert!(swap_back(&exe, &prev_path(&exe)).is_err());
  // The running image is untouched by the failed attempt.
  assert_eq!(std::fs::read(&exe).expect("read"), b"v1");
}
