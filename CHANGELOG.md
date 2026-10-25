# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Performance
- **Incremental process table (Linux):** the periodic sweep no longer
  re-reads and reclassifies every process each tick. A per-pid
  classification cache (`crates/adsum-detect/src/cache/procs.rs`) records
  what the EXEC fast path and first-sighting ticks already computed; a
  tick now enumerates `/proc` for liveness, sweeps the dead, revalidates
  cached hits (one cmdline read per detected game, so a zombie can never
  hold a card), and reads + classifies only pids with no current-generation
  entry. Steady state: zero cmdline reads, zero automaton walks, zero
  allocations per tick — measured on the bundled database's shape (1062
  processes): 5.05 → 0.38 ms/tick (−92%), 3152 → 3 allocs/tick, 40.3 KB →
  54 B/tick of allocator churn, identical detections. Every watcher loss
  channel (cn_proc sequence gaps via `SeqTracker`, backlog sheds, socket
  overruns, watcher death AND resubscribe) sets one invalidation flag that
  costs exactly one full-read tick; bundle swaps invalidate through an
  epoch carried inside the generation `Arc`, so writers never touch the
  cache. Watcher disabled/dead, other OSes and one-shot diagnostics keep
  the classic full sweep, byte-for-byte.
- **Zero-allocation `/proc` enumeration:** the sweep reads directory
  records through raw `getdents64` into a reused, 8-byte-aligned arena
  and parses `d_name` in place, replacing `std::fs::read_dir` — whose
  iterator allocates per entry internally and whose `DirEntry::file_name()`
  returns an owned `OsString` (the old code called it twice per pid:
  filter and loop). Measured: ~3 allocations per `/proc` entry per tick
  → 0; at 1000 processes that is ~29 MB/h of allocator churn removed,
  which is what kept the glibc arenas (and the RSS floor between the
  hourly trims) warm.
- **Snapshot lane without strings:** `ScannedEntry.id` is now `AppId`
  (`Arc<str>`) and `.name` is `Arc<str>`, `ProcessDetectedEvent::Removed`
  carries `AppId`, and the scan loop's snapshot `Vec` is hoisted out of
  the tick and swapped with the recorded table instead of rebuilt. Per
  tick with games running: one `String` per hit plus a `Box<str>` copy
  per removal → zero allocations; the daemon pump's `ScannedGame`
  construction (two deep string copies per detection) is two refcount
  bumps. Layout asserts unchanged (`Arc<str>` and `Box<str>` are both
  16 B).
- **Hoisted locks and buffers off the hot path:** the exclusions `RwLock`
  is read once per tick (or per EXEC event) instead of once per process;
  the dead-pid AppId sweep reuses a caller-owned liveness set (was a
  fresh `FxHashSet` per tick); `is_suspended` and the bare-exe cwd probe
  (`read_cwd_into`: raw `readlink`, lowercase in place, joined candidate
  in a scratch buffer) reuse their path/body buffers — three allocations
  per bare-exe miss and one per hit removed.
- **Aux-table arena:** the four `SortedIndex` tables (steam/epic/name/
  nodot) pack their keys into one contiguous per-generation arena with
  `(start, end, index)` `u32` slots: binary searches probe contiguous
  bytes instead of ~20k scattered `Box<str>` allocations (13–39% faster
  miss-heavy lookups, measured over the real key set), and the per-key
  malloc headers vanish from every generation. Build order, first-wins
  ties and lookups are byte-identical (pinned by new unit tests).
- **Slot reuse on Windows/macOS:** the `sysinfo` enumeration refills the
  caller's high-water `Exec` slots in place (exe path into the slot's own
  `String`, argv joined into the recycled arguments buffer) instead of
  rebuilding the `Vec<Exec>` per tick — ~3 allocations per process per
  tick removed on those targets, which now also get the sustained-drop
  slot compaction Linux already had (`compact_slots`, shared).
- `process_start_ms` off Linux refreshes with
  `ProcessRefreshKind::nothing()` instead of `everything()`: `start_time`
  is collected unconditionally per process in sysinfo 0.39, so the old
  call paid cwd/environ/user/root reads per detection to discard them.

### Changed
- **File organization: one concern per module.** The near-twin module
  pairs are gone — `scan.rs` (classification) is now `matching/` with
  the pipeline in `mod.rs` and its rule families in `normalize` (the one
  name-folding rule), `paths` (what it means for a declared name to name
  a path), `args` (borrowed command-line token scans) and `tiers` (the
  aux match tiers + shared hit tail); the tick orchestration moved from
  `scanner.rs` to `sweep.rs`; `database.rs` (bundle swaps) became
  `generations.rs`, leaving `db.rs` as the single wire-model module.
  `runtime.rs` split into `runtime/{mod,workers,lanes,cadence,loops}` —
  `start()` reads as lifecycle and the two 150+ line thread closures are
  named bodies (`run_scan_loop`, `run_refresh_loop`). The reuse-buffer
  family moved to a top-level `scratch.rs` with a new
  `EnumerateScratch` group (pid list, getdents arena, liveness/work
  lists) so the enumeration state no longer rides inside the per-pid
  reader scratch, and the two per-pid caches grouped under `cache/`
  (`appid` + `procs`) with their shared trust model documented once.
  `os_matches` moved beside `OsName` in `types.rs`; `first_sightings` /
  `apply_ignore_list` moved beside the lanes that consume them. Public
  paths that embedders use are preserved (`adsum_detect::proc_start`,
  `runtime::ScanGuard`/`idle_wait`, the `matching::` re-exports); the
  test tree mirrors the source (`tests/unit/matching/`,
  `tests/unit/cache/`, `tests/unit/platform/`). No behavior change: the
  full suite passes untouched, rustdoc is warning-free.
- **Platform backends behind one seam:** the OS-specific code that was
  scattered as `#[cfg]` islands inside shared files (six reader pairs in
  `scan.rs`, two enumerations in `scanner.rs`, the netlink wiring in
  `runtime.rs`, four arena-release arms in `server.rs`) now lives under
  `crates/adsum-detect/src/platform/` — `linux/` (`/proc` readers,
  `getdents64` enumerator, netlink dispatch/watcher), `snapshot/` (the
  `sysinfo` backend for every non-Linux target), `macos/` (the `.app`
  bundle-interior shape — deliberately ungated: the tier is data-driven
  and must run, and be tested, everywhere), `windows/` (documented
  empty: nothing Windows-only is left in the crate), plus unified
  `memory.rs` (arena release) and `proc_start.rs` (public path
  `adsum_detect::proc_start` unchanged). The seam is a static-method
  `ProcessEnumerator` trait (`traits/enumerator.rs`: `begin_tick` +
  `read_one`), so the tick orchestration — BOTH the full sweep and the
  incremental cache — is now 100% cfg-free and runs on every target;
  the incremental mode is gated only by `cache_mode_live` (watcher
  enabled AND live), which today can only hold on Linux. No behavior
  change: the full test suite passes untouched, and the `parse_proc_pid`
  suite moved with its function.

### Added
- **SteamAppId tier on Windows and macOS:** `read_steam_app_id` was a
  `None` stub off Linux, leaving the 57% of database entries with no
  declared executables without their authoritative tier on those
  platforms. sysinfo 0.39 reads the environ block on both (PEB through
  the process handle on Windows, the sysctl arg/env route on macOS); the
  reader uses a `System` scoped to the call — the shared snapshot never
  accumulates environ copies — and the existing per-pid memo
  (`Present`/`Absent`/`Stale` + invalidation sequence) keeps it to one
  read per process lifetime. Permission-gated processes memoize
  `Absent`, exactly like an unreadable `/proc/<pid>/environ`.
- **Steam and Heroic discovery off Linux:** fallback Steam roots now
  include the registry's `SteamPath` (`HKCU\Software\Valve\Steam` via
  `reg query` — dependency-free; discovery runs at startup plus every
  120th tick) and the `Program Files (x86)`/`Program Files` defaults on
  Windows, and `~/Library/Application Support/Steam` on macOS; Heroic's
  cached library is found under `%APPDATA%\heroic` (Windows) and
  `~/Library/Application Support/heroic` (macOS). The
  `ADSUM_STEAM_ROOT`/`ADSUM_HEROIC_ROOT` overrides stay exclusive.
  Without these, the Steam-library and `-epicapp` tiers were silently
  dead off Linux (`$HOME`/XDG shapes do not exist on Windows).
- `DetectablesBundle` carries a monotonic `epoch` (stamped inside the
  generation `Arc`, so a reader can never pair it with a foreign
  bundle): the cache-invalidation signal for refreshes and override
  edits, costing one full-read tick per swap.
- `watch()` takes an `invalidate` flag it sets on every loss channel it
  can observe from inside (sequence gap, shed, `ENOBUFS`); the retry
  loop sets it on both edges of the watch.

### Fixed
- Steam demos whose executable is not in the database (VEIN Demo is the
  motivating case) are now detected when the process runs under the AppId's
  own install dir (`steamapps/common/<installdir>/`). The install dir is
  independent evidence, not an AppId-only match: declared launchers,
  prerequisite installers, Steam helpers and the exclusion list keep their
  current veto.
- A clean install no longer fails with `226/NAMESPACE` when the cache
  directory is missing: the unit creates it before the sandbox mounts
  (`CacheDirectory=`) instead of listing it in `ReadWritePaths=`. The
  installer creates the legacy default directories only when the
  installed unit still lists them there.

## [0.39.0] - 2026-10-21

### Breaking
- Renamed the project from rsRPC to Adsum. The binary is now `adsum`, the
  systemd service is `adsum.service`, environment variables use the
  `ADSUM_*` prefix, and config/cache moved to `~/.config/adsum` and
  `~/.cache/adsum`. Existing 0.38.1 installations need a one-time manual
  migration: the old updater only looks for `rsrpc-cli-*` assets and
  cannot update across the rename.

### Added
- Heroic/Epic sandbox-id detection: Proton games launched by Heroic carry
  `-epicsandboxid=<id>` on the command line, and the service sandbox hides
  the environ — so the cmdline is the only distributor id a service can
  see. GTA V Enhanced is the motivating case.
- Epic `-epicapp` fallback: older Legendary releases (before 0.20.25) omit
  the sandbox id and pass only the app name. Heroic's cached
  `store_cache/legendary_library.json` maps it to the same `epic`
  namespace the database stores; the sandbox id remains the first choice.
  The config root can be overridden exclusively with `ADSUM_HEROIC_ROOT`.

### Fixed
- IPC `SET_ACTIVITY` publications with a missing or zero PID are now
  rejected before any state change, instead of leaving an orphan
  presence downstream.
- An absent `activity` field in an activity update is now rejected
  instead of being treated as a clear; only an explicit null clears the
  presence.

### Security
- Bumped `rustls-webpki` to 0.103.16 and `webpki-roots` to 1.0.10 in all
  three lockfiles; RUSTSEC-2026-0382 (distrusted intermediates accepted)
  and RUSTSEC-2026-0381 (distrusted emSign roots bundled) are fixed in
  those versions. Both crates are reached only through `ureq`/`rustls`,
  the HTTPS client.

## [0.38.1] - 2026-10-15

### Security
- Bumped `rustls` to 0.23.46 in all three workspaces; RUSTSEC-2026-0346,
  -0347, -0348 and -0349 were published on 2026-10-14 and are fixed in
  0.23.46 and later. The crate is reached only through `ureq`, the HTTPS
  client, and the gate went red on a push that had not touched it.

## [0.38.0] - 2026-10-11

> **Note on 0.37.0.** A `0.37.0` was prepared on 2026-09-25 and documented,
> but never tagged, published or installed anywhere — `gh release view
> v0.37.0` returns *release not found*, and the last published release is
> `v0.36.1`. Its content was therefore not lost, just never shipped: it is
> part of this release, under the version it actually left the machine as.
> There is no `0.37.0` in the wild to be compatible with.

### Added
- Detection traceability: every first sighting logs which matcher won
  (`automaton`, `cwd-joined`, `proton-automaton`, `steam-app-id`,
  `steam-library`, `exe-stem`, `folder`) plus process age in ms at
  classification; every clear carries its reason (`sdk-clear`,
  `abrupt-close`, `process-vanished`, `scan-absent`, `yielded`); SDK takeovers and
  generic resumes name both sides of the slot. Snapshots gain additive
  `detectionSource` / `detectLatencyMs` fields per card (`"sdk"` for
  client-published cards, absent on old ones). Tracing only, on
  lifecycle transitions — no hot-path cost, no retained state. Guide:
  `docs/detection-traceability.md`.

### Security
- Bumped `rustls` to 0.23.45 in the `fuzz` and `tools/updater`
  workspaces (RUSTSEC-2026-0285, TLS 1.3 handshake); the main lock was
  already clean. All three locks are now audited in CI.
- The bridge now validates the WebSocket `Origin` header like the game
  transport already did: absent `Origin` (native clients) and Discord's
  own pages pass, anything else is closed at connect before READY unless
  listed in `--bridge-allowed-origins` / `ADSUM_BRIDGE_ALLOWED_ORIGINS`.
  A malicious page can no longer read local activities or send bridge
  commands through `ws://127.0.0.1:1337`; frames pipelined past the
  close are dropped as unregistered. Client ids are now unique
  process-wide across both bridge servers (per-server counters could
  alias registrations), and messages from unregistered ids never reach
  control handling.
- The installer is fail-closed: a missing signature manifest or a
  missing `minisign` binary aborts the install instead of continuing on
  SHA256 alone (explicit `--binary` files stay exempt). The systemd unit
  ships as a signed release asset covered by `SHA256SUMS.txt` and is
  verified the same way before anything is installed from it.
- Presence snapshots are owner-only: the temp file is created with an
  explicit `0600` mode on Unix (the rename carries it to the final
  path), so app ids, pids and ports stay private under a permissive
  umask. Every write uses its own temp, created exclusively without
  following symlinks, so a planted link is refused and concurrent
  daemons sharing a slot still write apart. Slot selection stays
  best-effort — concurrent daemons share slots, last writer wins —
  and stale crash temps are swept under the same staleness rule.

### Performance
- The per-tick, per-publish and per-connection allocations left behind by
  the earlier memory fix (#2) are gone, under one rule: **one owned
  materialization at the boundary, zero deep copies after it** (61 commits,
  59 files, +6394/-1349). Same games detected, same order, same log lines.
- Ids (`AppId`, `SocketId`) are `Arc<str>` newtypes with `Deref<str>`, so
  cloning one is a refcount bump: every path that copied a client id,
  socket id or app id now shares it.
- Payloads that reach more than one consumer cross as
  `Bytes`/`Utf8Bytes`/`Arc<T>` and are encoded once; the MessagePack
  derivation is lazy and shared instead of rebuilt per consumer.
- Scratch buffers (`ExecScratch`, `MatchScratch`, the `/proc` stat and
  environ paths) are owned by the loop and reused across ticks, which is
  where roughly 400 allocations every 5 s per process came from. The scan
  tick classifies every process on the machine, so this is the path that
  decides the resident set, not a hot path in the usual sense.
- Collections that outlive their build are `Box<[T]>` and shrunk;
  transient ones deliberately are not. Entries carry packed `[u32;2]`
  index pairs, and the automaton is built in a single pass with exact
  reservations.
- Structural caps on the fetched database, measured rather than guessed:
  the bundled snapshot is 24,299 entries, and the maxima within an entry are
  16 executables, 8 aliases, 30 Steam SKUs and 204-character names. Every
  cap sits an order of magnitude above that. A body decoding past one is
  rejected as `DatabaseTooLarge`,
  distinct from a parse failure, and the caller keeps the database it
  already holds. The 64 MiB body limit bounded the bytes; it never
  bounded what they decode into — one entry with tens of thousands of
  executables is a megabyte of JSON becoming one automaton pattern and
  one index pair each.
- The census now reports the resident high-water mark beside the current
  RSS and the live/free heap split from the allocator, so a growth can be
  told from a floor that held while the peak climbed. Both were verified
  against the kernel before being written: the `kB` unit in
  `/proc/self/status` is really KiB, confirmed by comparing resident pages
  against `VmRSS`.
- **Two deliberate deviations.** The `SET_ACTIVITY` echo emits `data`
  keys in declaration order instead of the `BTreeMap`'s alphabetical
  order (the echo contract already allows key-order variance and every
  test compares parsed values), and the IPC close-clear nonce now carries
  the value the client sent instead of its JSON *text* — that one is a
  fix, see below.

### Changed
- Folded `adsum-state` into `adsum-bridge` (12 crates down to 11): the
  snapshot slots had exactly one consumer, so the crate boundary only
  added hops. `adsum-telemetry` stays separate: four crates share it,
  and folding it into the bridge would point their dependencies the
  wrong way. `BindPaths=/tmp` in the unit stays as is: tightening was
  investigated, but BindPaths takes no globs, the sockets only exist at
  runtime, and the fan-out symlinks need the host path writable.
- Regression nets now pin the memory bounds: replay-cache pruning and
  broadcast fan-out never exceed `MAX_CACHED_ACTIVITIES`, and the
  flood-guard table stays within its cap.
- Shared package metadata and dependency pins now live in the workspace
  root (`[workspace.package]` + `[workspace.dependencies]`): edition,
  MSRV, license, repository, authors and 15 dependency versions change
  once instead of per crate. `Cargo.lock` is byte-identical, and per-crate
  versions are untouched. `authors` names the historical author and the
  current maintainer (`spikehd`, `yieskoW`). `tokio-util` stays pinned
  per crate on purpose: inheritance cannot override `default-features`.
- CI now runs the test suite on Windows and macOS as well as Linux,
  audits all three lockfiles, and validates non-Rust artifacts
  (`node --check` on the plugin, `systemd-analyze verify` on the unit,
  typecheck of the fuzz workspace). A new fuzz workflow smoke-checks
  targets on PRs and runs the engine nightly (10 minutes per target:
  `ipc_frame`, `activity_full`, `trim_db`, `db_parse`), with corpus
  caching between runs. New `ipc_frame` fuzz target drives the IPC
  reader (`handle_stream` with a mock facilitator and a split
  byte-duplex), asserting every response byte belongs to a complete
  frame (708k runs clean locally).
- All GitHub Actions references are pinned to commit SHAs (tags kept as
  comments), closing the mutable-tag supply-chain window on the release
  pipeline especially.
- Unix IPC pumps are capped at 16 concurrent peers like Windows (which
  keeps 32): connection floods past the cap drop peers at accept instead
  of parking blocking threads without bound, leaving room in the
  32-thread blocking pool for unrelated work.
- Split the two biggest modules internally, same crates and same
  behavior: `crates/adsum-detect/src/server.rs` is now foundation plus
  `runtime` (threads, shutdown, join), `database` (bundle ownership and
  swaps), `scanner` (enumeration and classification), `steam`
  (library roots) alongside the existing `bundle`/`db`/`refresh`/`scan`
  modules; `crates/adsum-bridge/src/bridge.rs` is now construction plus pumps
  alongside `consumer` (fan-out and pruning), `router` (activity
  decisions), `replay` (cache and ghost-reap pid resolution), `snapshot`
  (persistence) and `control` (`SET_USER`/`RESET_USER`). `Shared` fields
  are crate-visible for the new units; wire protocols unchanged. One
  breaking change for library callers: `adsum_proc_events::watch` now
  takes a shutdown flag (`watch(events, stop)`), so the watcher thread
  joins on directed shutdown instead of blocking on netlink.
- CodeQL now scans the workspace as one job with the v4 action inputs, and
  re-scans whenever any workflow changes rather than only its own.
- Dependabot maintains the three cargo workspaces and the pinned actions;
  CI declares the least permissions each workflow needs, and the runners
  are pinned by image label instead of floating on `ubuntu-latest`.
- The three workspaces share one grouped `cargo` entry in CI instead of
  three near-duplicates.

### Fixed
- Hostile timestamps saturate instead of overflowing: `normalize_timestamp`
  uses `saturating_mul`, so extreme inputs floor at `i64::MIN` in both
  debug and release builds.
- Payload builders (`empty_cached`, `generic_payload`) return `None` on
  encode failure instead of shipping empty frames: callers log, count a
  dropped broadcast and skip. The hand-written clear JSON is gone — clears
  serialize from the same struct as everything else — and the emergency
  error frames escape the command name instead of interpolating it raw.
- The flood-guard key builds once per call instead of twice (an `AppId`
  key would allocate identically, since callers hold `&str`).
- Scanner threads now belong to the daemon lifecycle: `ProcessServer`
  gained directed `shutdown()` + `join()` (plus `adsum-*` thread names),
  every long sleep became an interruptible park, the netlink watcher
  honors a stop flag (including its self-test), and `Daemon::run_until`
  shuts down, joins and drops the scanner on success and bind failures.
  Future abandonment (dropping the future via `select!`/timeout/abort)
  signals shutdown through a cancellation guard instead, so threads exit
  on their own within about one poll interval. When `run_until` returns,
  no Adsum thread is still running, so library callers may run another
  daemon afterwards (`crates/adsum-detect/src/server.rs`,
  `crates/adsum-proc-events/src/lib.rs`,
  `crates/adsum-core/src/daemon.rs`).
- The module split broke non-Linux builds (the Linux-only `/proc`
  reader was imported unconditionally: E0432 on every Windows/macOS
  job). The reader import and its Linux-only companions are now gated
  on the target, and the whole workspace checks warning-free for
  Windows again.
- `--binary … --no-systemd` outside a checkout died demanding a
  signature manifest for a unit the user asked not to install. Every
  unit step (resolution, validation, directory creation, copy) is now
  skipped in `--no-systemd` mode — binary-only install, exit 0 — and
  the auto-update drop-in and closing log line follow the same flag
  (`scripts/install.sh`).
- Configured extra origins normalize once in the builder (lowercase,
  no trailing slash, no default port), so `https://client/` or
  `https://CLIENT` matches the serialized `Origin` the browser sends
  instead of failing with only a `Refused origin` log. The
  per-connection check stays an exact comparison.
- A failed `/proc` directory entry can no longer abort the scan tick:
  the skip is now explicit (the surrounding filter already excluded
  errors; the invariant is documented, behavior unchanged).
- Steam prefix matching works on Windows: library keys and queries are
  canonicalized once (lowercase, backslash to slash, leading slash), so
  native `C:\...` paths meet the cached keys. Without this no prefix
  ever matched there and Steam games went undetected. The library cache
  schema bumped to v2 for the same reason: v1 caches holding backslash
  keys are ignored once and rescanned instead of reused empty.
- The crash-temp sweep only removes snapshot temps: it now requires the
  snapshot filename prefix too, so other programs' `*.tmp-*` files in
  the shared temp dir are never touched.
- **Downloading or updating a game on Steam opened a presence for a game
  nobody was playing.** Steam ends an update by running the app's script
  phase and setting up the Proton prefix, and hands both the game's
  environment — `SteamAppId` included. Under Proton the environ is hidden
  from a service context, so the id reached the scanner by way of the
  command line, about a second after Steam committed thousands of updated
  files. A `SteamAppId` now only corroborates when the process runs an
  executable the database entry declares, matched on a path-component
  boundary; an entry with no declared executables keeps the id on its own,
  which is the case the tier exists for. Verified in the field with the
  deployed binary: the log names Steam's own `reaper` and no presence
  appears.
- **A declared name that merely appears in the path no longer reads as
  the game.** The automaton matches substrings, so `.../hl2.exe.txt` and
  `.../hl2.exe.evil` were detected as Half-Life 2 with no store id
  involved. The executable that fired now has to name the *end* of the
  probed string on a boundary.
- The two tiers disagreed about spelling: the store-id tier compared the
  raw declared name while the automaton compared the normalized one, so
  an entry declaring only `>hl2.exe` or `bin\gmad.exe` was rejected by the
  id tier and accepted by the automaton. Both now corroborate through the
  same allocation-free predicate.
- **A launcher is not the game.** The database flags these with
  `is_launcher`, and Steam runs them first: clicking Play on Baldur's Gate
  III launches `larilauncher.exe`, which carries the game's AppId before
  the game has started — the same shape as the update above, one tier over.
  Twenty executables across thirteen entries were in that position. The
  `>` marker is a different mechanism ("the arguments carry the launcher")
  and is unaffected.
- **A macOS `.app` bundle names the binary inside it.** Sixty-one
  executables across fifty-six entries are declared as the bundle
  (`factorio.app`), but the process macOS runs is the binary within
  `.../Factorio.app/Contents/MacOS/factorio`. Those games were detected
  before the boundary rule and stopped being detected after it; a declared
  `.app` at a component boundary now matches when Apple's bundle interior
  follows it.
- **A refusal is remembered for the whole automaton walk.** A declared name
  seen in a path without naming it is specific evidence that the process
  is not the game, and the stem and folder heuristics must not answer over
  it — but the evidence belongs to the path, not to the automaton that read
  it, so a second pass or a stripped 64-bit variant no longer erases it.
  It also no longer stands between the two automata: Hearts of Iron IV
  declares `hoi4` for Linux and `hoi4.exe` for Windows, and the Linux name
  was hiding the Wine game the Proton probe was about to name.
- A rejected automaton match no longer hides the match that *does* name
  the process: the probes walk their automaton's overlapping matches and a
  refusal costs that match, not the variant. Without this, Minecraft's
  short `java` pattern ended inside Illarion's longer
  `illarion/jre/bin/java.exe` and the correct hit was never tried.
- The IPC close-clear nonce now carries the value the client sent instead
  of its JSON text: `"abc"` used to be echoed back as `"\"abc\""`, and a
  null nonce as the literal string `"null"`.

## [0.36.1] - 2026-09-24

### Fixed
- `--rollback` no longer re-executes into an infinite toggle loop. The
  re-exec passes the original argv back and `cmd_rollback` had no
  applied-guard, so every pass swapped `adsum-cli`/`adsum-cli.prev` again
  (reproduced: 460 swaps in 3 seconds, killed by timeout). It now does
  one swap, re-execs once and exits, mirroring the guard in
  `apply_pending_on_boot` (`cli/src/update.rs`).
- `ADSUM_STATE_FILE` is wired up again: `--state-file` /
  `ADSUM_STATE_FILE` points the bridge at the temp-dir snapshot slots
  (`<tmpdir>/adsum-state-{0..9}`), as documented since 0.32.0. The env
  check was lost in the 0.36.0 workspace refactor while
  `BridgeConfig::state_dir` stayed `None`, so the CLI activation never
  happened; `RPCConfig::state_file` now carries it through to
  `Bridge::bind` (`crates/adsum-core/src/{config,daemon}.rs`,
  `cli/src/main.rs`).
- `systemd/adsum.service` keeps `%h/.local/bin` writable via
  `ReadWritePaths`: under `ProtectHome=read-only` + `ProtectSystem=strict`
  the sandboxed service could not swap in its own staged self-updates
  (discarded at boot with `cannot replace binary`), and the dead
  `ADSUM_LOGS_ENABLED=1` line (no longer read by the logger) is gone.

### Changed
- The repository moved from `yiesko/Adsum` to
  `yiesko/adsum`; every checked-in URL (crates,
  install script, docs, changelog links, self-update API) now points
  at the new location.
- The README was rewritten in plain English with a screenshots section,
  and the deep guides moved into `docs/` (`cli-options`,
  `process-detection`, `protocol`, `web-client`, `library`,
  `self-update`, `systemd-operations`); `systemd-user-units.md` is now
  a step-by-step setup guide quoting the unit byte-for-byte.

## [0.36.0] - 2026-09-23

### Breaking
- The `lib/` monolith is now a 12-crate workspace plus the thin `cli`
  binary (`adsum-ws`, `adsum-types`, `adsum-protocol`, `adsum-transport-ws`,
  `adsum-transport-ipc`, `adsum-state`, `adsum-bridge`, `adsum-telemetry`,
  `adsum-proc-events`, `adsum-steam`, `adsum-detect`, `adsum-core`): one
  Tokio runtime owned by the caller, `[workspace.lints]` on every member,
  fuzz targets under `fuzz/`, benches under `adsum-detect` /
  `adsum-protocol`. The old `adsum` library name is gone: embedders use
  `adsum-core::Daemon` / `RPCConfig::builder()`. `Message::Text(String)`
  is now `Text(Utf8Bytes)` and bridge payloads are `Arc<CachedActivity>`
  (`json: Utf8Bytes, msgpack: Bytes, is_clear: bool`).

### Fixed
- `Shared::broadcast_raw` now prunes dead bridge clients the same
  way `send_to_all` already did: when `Responder::send` returns `false` the
  client is removed and a `Pruning dead consumer` warning is emitted.
  Previously a bridge client that died without a clean disconnect (killed
  process, closed browser tab) and only received `INVITE_BROWSER` /
  `DEEP_LINK` events would keep its `Responder` and queued frames pinned in
  memory forever (`Shared::broadcast_raw` in `crates/adsum-bridge/src/bridge.rs`).
- WebSocket game-client handlers (`handle_browser_command`,
  `handle_deep_link`, `handle_connections_callback`, `handle_set_activity`)
  now report whether their reply was delivered and the pump
  (`Event::Disconnect` in `crates/adsum-transport-ws/src/transport.rs:801`)
  removes the client and clears every published pid when delivery fails,
  freeing the per-client slots the same way a clean `Disconnect` does
  (`crates/adsum-transport-ws/src/handlers.rs:291`). Verified with real
  `Responder` tests (RFC6455 handshake + killed-connection simulation with
  condition-wait for `send==false` and `Disconnect`).
- Hourly bundle-swap RSS floor ratchet: the swap ran `malloc_trim` while
  the old generation was still pinned by an in-flight tick, stranding
  ~10 MB per rebuild in allocator arenas (production floor ratcheted
  52→104 MB). The swap now trims once more 60 s past any tick and
  reports the result (six consecutive recounts showed the old
  generation always releases within 30 s); the post-retrim line also
  reports live vs free heap bytes from `mallinfo2` (glibc-only,
  cfg-gated) (`crates/adsum-detect/src/db.rs`, `server.rs`). The
  canonical content hash is factored per entry (byte-identical stream,
  pinned by a fold test).
- First-sighting `INFO` logs report every new id again, restoring main-branch
  log parity. The remembered-id set stays bounded at
  `MAX_SEEN_IDS = 1024`: past it the set is dropped whole rather than grown
  without limit, so a repeat `INFO` may surface after enough id churn. That
  is the accepted trade for constant memory — the log line is diagnostic,
  never state.

### Added
- Read-only resource census for long-session memory diagnosis (no behavior
  change): `QueueGauge` / `GaugeSender` / `GaugeReceiver` wrappers around
  the three previously unbounded channels sharing an `AtomicUsize` depth,
  plus a `StatsSnapshot` census (`rss` via `/proc/self/statm` on Linux,
  `n/a` elsewhere) with bridge `json`/`msgpack` and `ws` client counts and
  all three queue depths
  (`crates/adsum-telemetry/src/lib.rs:20`). Helpers, rendering and the
  daemon wiring are covered by `crates/adsum-telemetry/tests/telemetry.rs`
  (gauge sharing, `rss_bytes`, snapshot shape) and the bridge census
  suite (live counts, session-boundary lines): the bridge logs the
  census hourly plus `game-start`/`game-end` on process-table
  transitions, with consumer counts and input queue depths sampled live.
- Tests `crates/adsum-ws/tests/integration.rs` and
  `crates/adsum-transport-ws/tests/integration.rs` (dead vs live
  `Responder` liveness).

### Changed
- Cheaper `SET_ACTIVITY` pipeline: the activity fingerprint is computed
  first and flood-dropped publishes return before the JSON + MessagePack
  envelope is ever built (~0.76µs vs ~2.5µs per dropped publish); accepted
  publishes compare stored fingerprint bytes instead of re-serializing and
  re-parsing (`CachedActivity.activity_json`, same pattern as `is_clear`).
  No behavior change (covered by the flood/change/replay suites plus a
  `publish_flood_drop` bench).
- Per-tick buffer reuse in the scan and match paths: ~400 processes ×
  fresh `String`/`Vec` temporaries every 5 s tick become caller-owned
  reuse buffers — `ExecScratch` + `read_exec_into` (path, cmdline and
  args buffers allocated once per thread), `process_list_into`
  refilling every `Exec` slot across ticks with high-water retention
  (no `truncate`: surplus slots keep their string buffers, only the
  filled prefix is classified), and `MatchScratch` for the lowered
  path and normalized folders (lowercase stage skipped on the
  already-lowercase hot path). Behavior is byte-identical, covered by
  parity and no-realloc regression tests
  (`crates/adsum-detect/src/scan.rs`, `server.rs`).
- Single-pass automaton build: entries are walked once, partitioning
  executables into native and Proton pattern vectors (identical
  contents, order and filters as the two walks), with pattern, index
  and aux-map vectors pre-sized from the entry counts and each source
  vector dropped right after its automaton compiles — peak is one
  automaton plus one vector plus builder scratch
  (`crates/adsum-detect/src/bundle.rs`).
- `tokio-util` trimmed to default features workspace-wide: only
  `sync::CancellationToken` is used, so the explicit full features
  pulled codec, compat, io, net, rt and time machinery for nothing
  (`crates/adsum-bridge/Cargo.toml`, `crates/adsum-ws/Cargo.toml`).

## [0.35.0] - 2026-09-15

### Breaking
- Removed `adsum::cmd::Button` (unreferenced; buttons travel as plain
  values) and the deprecated `get_user_response` shim (its one-release
  cycle since 0.33.0 is over — use `user_response`); `empty_activity`
  is now private. New `RPCConfig::initial_db_content_hash` field
  (seeded by the CLI; `..Default::default()` constructions are
  unaffected, struct literals need the field).

### Added
- Trimmed-content refresh guard: the hourly DB check now hashes the
  canonical scanner projection (trimmed fields, body order preserved
  because first-wins ties depend on it) alongside the raw body, so
  volatile CDN bytes around identical games skip the automaton rebuild
  entirely (previously every new ETag rebuilt, spiking RSS to ~165MB).
- `RPCServer::from_parsed` (infallible): build from already-parsed
  activities, letting the CLI boot path parse the fetched body once
  instead of twice.

### Fixed
- Staged overrides fold into the initial scanner build
  (`ProcessServer::new_with_custom`): boot no longer builds the
  automata twice (three times counting the fetch probe parse).
- Negative SteamAppId lookups are memoized (`Absent` vs `Stale`): desktop
  processes without a store id no longer re-read kilobytes of environ
  every scan tick (the biggest per-process I/O cost).
- Scan loop forwards detection deltas only, and the bridge skips fan-out
  for byte-identical republications: steady-state wakeups, channel
  traffic and snapshot rewrites drop to ~zero with no observable change
  (dedup already lived downstream).
- Scanner bundle stores a slim entry form (id/name/executables/steam
  ids/aliases) instead of the ~30-field public struct, and hits carry
  `(entry, pid, start)` instead of a full-struct clone per hit per tick.
- Steam refresh returns early when all fingerprints match (no per-tick
  prefix recopy) and re-collects roots every 120th tick instead of 30th.
- OTA staging streams the binary to disk with incremental hashing
  instead of holding the whole image in RAM.
- Slim scanner strings (`Box<str>`) and an `os` enum (hot values as
  unit variants, exotics owned per generation instead of leaked): ~130k
  string fields drop the `String` capacity word, and the 11k `os`
  values point at 3 shared statics instead of heap-allocating.
- Aux lookups (`steam_map`/`name_map`/`nodot`) are sorted vectors with
  binary search instead of hash maps: no buckets, no hashing, same
  first-wins tie rules; lookups only happen on match misses.
- Boot/rebuild logs report automaton heap bytes (`memory_usage`), and a
  compile-time assert pins the slim entry layout against regrowth.
- Hourly refresh seeds content hashes from the boot fetch
  (`initial_db_content_hash`): the first check skips parse/rebuild like
  later ones instead of one guaranteed redundant rebuild per boot; the
  fat parse structs are dropped before the automata build, halving the
  rebuild peak overlap.
- Client event queues are bounded (backpressure, no drops): a spinning
  local client throttles to bridge speed instead of growing the queue
  without bound.
- Steam watch markers prune vanished roots; dead helpers removed
  (`Button`, deprecated `get_user_response` shim); duplicate trim/parse
  paths unified.

## [0.34.0] - 2026-09-13

### Added
- One-liner install (`scripts/install.sh`, Linux/systemd): arch
  detection, newest-release download with SHA256 (+ minisign when
  available) verification, install to `~/.local/bin` +
  `~/.config/systemd/user` with timestamped backups, service
  enable/start/linger, optional opt-in auto-update drop-in; idempotent
  re-runs update. `scripts/uninstall.sh` removes service + binary
  (`--purge` also drops config/caches/backups). Ships
  `systemd/adsum.service` (hardened user unit: `ProtectSystem=strict`
  + host `/tmp` bind for the IPC fan-out, `AF_NETLINK` for the proc
  watcher, personal overrides via commented drop-in examples).
- Proc-events honesty knob: the `watcher live` line now states the
  best-effort nature inline, and `--no-proc-events` /
  `ADSUM_NO_PROC_EVENTS=1` disables the netlink watcher thread entirely
  (process polling continues either way).

### Fixed
- OTA rollback actually restores the previous image: it used the same
  swap helper as apply, which deleted the `.prev` source before moving
  it back — printing success while changing nothing and leaving no
  backup behind. Rollback is now a true exchange (rolling back twice
  restores the starting state), and the sidecar name is a single
  predictable suffix (`adsum-cli.prev`).
- Proc-events watcher resubscribes instead of dying on one failed
  self-test: a transient kernel stall no longer pins polling until the
  next daemon restart (first failure still warns, later retries stay in
  debug). The self-test now reports datagram/parsed counts, telling a
  silent kernel apart from framing drift in the log line.
- Proc-events loss accounting: every observed message feeds a per-CPU
  sequence tracker over the kernel `cn_msg.seq` counter (delivery is
  officially lossy), so silent drops surface as debug gap lines plus a
  counter. Generic across topologies (wrapping-aware, re-anchors on
  counter restarts) and self-neutralizing where counter semantics do
  not hold. The self-test also reports timeout-vs-fatal breakdown and
  whether the subscribe ACK arrived, so the next silent window names
  its mechanism instead of just its symptom.

## [0.33.1] - 2026-09-11

### Added
- OTA manifest signatures (minisign): releases now publish
  `SHA256SUMS.txt.minisig`, and staging requires a valid signature from
  the embedded release key before any hash is trusted (fail-closed;
  legacy non-prehashed signatures rejected). CI signs with
  `MINISIGN_SECRET_KEY` and self-verifies before publishing; the secret
  key lives only in GitHub Secrets plus an offline backup.
- Self-update (OTA) for `adsum-cli` (Linux x86_64 + ARM64): `--check-update`
  (exit 2 when a newer release exists), `--update [--yes]` (downloads,
  SHA256-verifies against the release manifest, stages under
  `~/.cache/adsum/ota/`), `--rollback` (restores the kept `.prev` image),
  and opt-in `--auto-update`/`ADSUM_AUTO_UPDATE=1` for background staging
  in the daemon (daily check + log either way). Staged binaries apply on
  the next start via atomic swap + re-exec (same PID on Linux); dev builds,
  `cargo install` copies, foreign names and read-only dirs are refused with
  a plain message. CI publishes raw `adsum-cli-{target}` binaries plus
  `SHA256SUMS.txt` on tag releases.

### Fixed
- `SET_ACTIVITY` replies now echo the activity intact (official echo
  semantics): `name`/`type` (Playing/Listening/Watching/Competing) and
  every other field survive the round-trip instead of being rewritten to
  `""`/`0`. The lock-step guarantee stays — a reply is always sent.
- `SET_ACTIVITY` flood guard: byte-identical republishes from one
  `(application_id, pid)` inside 5s are collapsed before broadcast
  (changed bytes and clears always pass; clears re-arm the slot), so a
  spinning SDK can no longer fan out to bridge consumers at full rate.

### Changed
- Documented the `SUBSCRIBE` blind-ACK scope: only `READY`, `ERROR` and
  `CURRENT_USER_UPDATE` are ever dispatched; voice/guild/message/invite/
  relationship/entitlement subscriptions are ACKed and then silent (they
  need the real Discord client).
- **Breaking:** fallible APIs now return `adsum::error::AdsumError`
  (`thiserror`) instead of boxed errors; `ClientConnector::new`,
  `WebsocketConnector::new` and `RPCServer::start` return `Result`
  (bind failures surface to the caller — the library never exits the
  process anymore); `AppId`/`SocketId` newtypes replace raw strings for
  slot ids (same wire format); `remove_detectable_by_name(&str)`;
  `get_user_response` renamed to `user_response` (deprecated shim kept);
  `process_alive`/`suppresses` renamed to `is_process_alive`/
  `is_suppressed`; over-`pub` items narrowed to `pub(crate)`.
  Error messages are lowercase; public constructors document `# Errors`.

- Proton-aware detection: `win32` executables from the main database are
  now indexed in a fallback automaton on Linux. Wine/Proton games whose
  Steam AppId is unreadable match by path instead of staying invisible.
  Precedence is native patterns, then user overrides, then the Steam
  AppId, then the Proton patterns, then the stem/folder heuristics.
- Alternative titles (`aliases`) are preserved by the trim and indexed
  for stem/folder matching (same conservative multi-word gate as
  canonical names, which win ties). The bundled snapshot was regenerated
  with aliases (1397 entries carry them).
- Discord detection exclusions (`GET /games/detectable/exclusions`):
  installer/crash-reporter basenames + regex patterns are dropped before
  any matching. Fetched hourly alongside the DB when `--enable-db-update`
  is set (`--exclusions-url`/`ADSUM_EXCLUSIONS_URL` overrides); empty set
  behaves exactly like before.
- `tools/updater` now uses the shared `adsum::detection::trim_detectable`
  instead of a hand-rolled copy (the copy had silently dropped aliases
  from the bundled snapshot).
- Steam library provider: install-dir -> AppId from Steam's own files
  (`libraryfolders.vdf` + `appmanifest_<id>.acf` + `compatdata/<id>/pfx`),
  for games whose per-process store id is unreadable. Roots are found
  dynamically (running `steam` via `/proc`, `PATH`, mounted partitions
  via `/proc/mounts`, home fallbacks — a secondary library never shadows
  the primary root); `$ADSUM_STEAM_ROOT` overrides exclusively,
  `$ADSUM_STEAM_LIBRARIES` (`:`-separated) adds manually. A JSON cache
  with per-library fingerprints revalidates by stat and reparses only
  what changed. Precedence: Steam AppId, Proton patterns, Steam
  install-dir, stem/folder — except shortcut-range AppIds (high bit set,
  assigned by Steam itself to non-Steam shortcuts, e.g. 2532755798):
  those self-identify as non-Steam, so the exe/folder name wins over
  location instead.
- Event-driven detection (Linux): a netlink `cn_proc` watcher classifies
  `EXEC` processes the moment they spawn (cards in milliseconds, not next
  tick) and wakes the scan loop early when a tracked game `EXIT`s
  (clears at once). Best-effort with a boot self-test — kernels/LSMs
  that silently drop `cn_proc` fall back to pure polling with one honest
  log line; steady state is a single blocking `recv`, and only tracked
  pids can trigger an early scan. The scan cadence itself moved from
  `sleep` to `park_timeout` to allow the wakeups. Operational note:
  systemd units sandboxing with `RestrictAddressFamilies` must add
  `AF_NETLINK`, otherwise the subscription fails and the scanner stays
  on polling (this was the production failure mode — diagnosed via the
  missing `watcher live` line).
- Overrides v2: `--overrides-dir`/`ADSUM_OVERRIDES_DIR` loads every
  `*.json` in a directory (array or single object per file, sorted,
  corrupt files skipped with a warning) additive to `--overrides-file`
  — the home for MultiMC/Prism/Hydra mappings and Proton-only titles.
  Loading moved into a library module (`adsum::overrides`, shared
  defaults) and `append_detectables` stages pre-start: `--list-detected`
  now applies staged overrides AND the ignore-list, so diagnostics show
  exactly what the daemon would publish (previously main-DB-only).
- Hardening (correctness + DoS): the detection database (automata,
  indexes, lists, aux maps) now swaps atomically per generation — a
  refresh landing mid-scan can no longer panic or mis-attribute games;
  the VDF parser is iterative with depth/file/token caps; exclusion
  regexes are capped and matched as one size-limited `RegexSet`; the
  handoff tables are bounded (dead owners purged, hard cap 64); all
  network fetches carry timeouts; `/proc`/VDF reads are size-capped;
  hot-path locks survive poisoning instead of killing the daemon.
- Scan efficiency (measured with hotpath-rs, see `cargo profile
  profiling`): case-insensitive automata (no per-process lowercase
  allocation), borrowed paths (`Cow`, zero alloc on the common case),
  memoized SteamAppId per pid (one environ read per process lifetime,
  swept per tick, invalidated on EXEC), `HashSet` exclusions, hoisted
  broadcast serialization, and idle backoff (5s → 30s cap, reset by any
  detection or early wake — EXEC still delivers starts instantly).
  Steady state: ~0.4% CPU, ~57MB RSS; per-process classify 54µs → 23µs.

- Total detection blindness from size-checking `/proc` files: they
  report `st_size 0` despite having content, so the check skipped every
  process (only IPC-driven cards kept showing). Removed the checks, kept
  the byte caps; regression-tested against our own pid.
- EXIT-wake storm: tracked short-lived Proton helpers unparked the scan
  loop several times per second (0.76s effective cadence during NFS).
  Wakes are debounced to at most one per second now.
- Exclusive sources diluted on refresh: `ADSUM_STEAM_ROOT` (and injected
  layouts) were merged with re-discovered roots on every folders change.
  Refresh re-resolves from the same source discovery used.
- `ADSUM_DEBUG=1` (and friends) rejected by clap bool parsing, crashing
  the daemon at startup: all env-backed bool flags accept
  `1/0/true/false/yes/no/on/off` now.
- Dead re-entrancy guard (`scanning` checked but never set): real RAII
  guard, acquired atomically and released on every exit path.
- IPC/bridge bind exhaustion no longer panics the daemon: `create_socket`
  failures surface as typed `AdsumError::IpcBind` / `WsBind` (keeping the
  last `io::Error` as source) through the fallible `start()`; `to_fs_name`
  failures propagate instead of unwrapping.
- Poison-tolerance gaps closed: the last bare `lock().unwrap()`s on hot
  paths (bridge, handoff, replay cache, Steam prefix, `start()`) use
  `into_inner`; double `start()` on any connector is a logged no-op
  instead of a panic (including a `started` guard on the scan loop).
- Automaton build failures (pathological DB exceeding builder limits)
  keep the current generation with a warning instead of panicking the
  refresh thread or startup; same fail-open as the empty-DB guard.
- Error fidelity: `InvalidJson` keeps the serde error as `source`;
  `database_summary` returns `AdsumError`; bridge bind message lowercased
  without log prefix; `# Errors` on all fallible constructors.
- Custom steam SKUs participate in AppId matching (linear fallback,
  canonical map wins); main patterns beat custom ones across 64-bit
  path variants (automata probed outer, variants inner) — both
  regression-tested.
- Stale AppId race closed: memoized environ carries an invalidation
  sequence, so an EXEC racing an in-flight read discards (never pins)
  the pre-exec environ; EXIT-wake debounce peeks before consuming, so a
  recent tracked EXIT can still wake later.
- Diagnostics parity: `--list-detected` applies exclusions when hourly
  updates are on (fail-open fetch, like the daemon); a fetched-but-garbage
  DB falls back to the bundled snapshot instead of killing boot.
- Watcher honesty: netlink fallback logs at `warn` (the documented
  one-line diagnosis for `AF_NETLINK` sandboxing); the boot self-test
  falls back to polling (never claims live) when it cannot prove
  delivery; steady-state datagrams sized 64KiB for EXEC storms.
- Steam provider hardening: cache entries re-validate library ownership
  on load; fingerprints cover `compatdata` appearances; `read_limited`
  re-checks size after reading (stat/read TOCTOU); VDF tokenizer caps
  tokens and counts mid-stream; mounts-table escapes decoded in one pass;
  library scans capped; `mount_library_roots_for` tested for escapes.
- Input robustness: WebSocket handlers drop (logged) frames that cannot
  encode instead of panicking the poll loop; fan-out encode failures and
  dead-peer IPC replies log at debug; `last_process` bounded like the
  handoff tables; remaining bool flags (`--list-detected`,
  `--list-database`) and the logger accept boolish values.
- Dotted titles match their folders as a last tier (`R.E.P.O.`, `Q.U.B.E.`,
  `Mr. Bomber`): the exact walk still skips dotted components as versions,
  then a de-dotted twin map (`undotted_names`, canonical-first ties) is
  consulted — exact matches always win. Punctuation forbidden in Windows
  filenames (`: ? " < > | * / \`) already folds in `normalize_name`.
- Observability without debug builds: boot logs the database source and
  entry count (`fetched-direct/trimmed/bundled-fallback`, `file`,
  `bundled`, `empty`); the scan loop logs first sightings per game id
  per boot at INFO (bounded, same cadence as bridge publishes). A silent
  daemon is now distinguishable from a blind scanner.
- Scan liveness tripwire: the first completed tick logs its game count
  at INFO once per boot (`First tick complete: N game(s)`), so a scan
  loop that never completes its first pass is visible without debug.
- Native-presence capture: a peer that connects but never handshakes
  (bailed SDK probe, crashed launcher) logs one INFO line instead of
  debug-only, so clients the bridge never identifies leave a trace.
- Rich presence fidelity: `SET_ACTIVITY` frames round-trip every modeled
  field (details/state, timestamps, assets+urls, party+privacy, secrets,
  buttons, flags, emoji, display type) on both JSON and MessagePack legs,
  and unmodeled future keys survive via a flattened catch-all instead of
  being dropped on parse — verified live against the daemon and pinned by
  a round-trip test.
- IPC sockets in every official dir: the bound `discord-ipc-{n}` is
  symlinked into `XDG_RUNTIME_DIR`/`TMPDIR`/`TMP`/`TEMP`/`/tmp` (Discord's
  resolution order), so games probing any location find the bridge; stale
  ours-shaped links repoint, foreign files are never touched, links die
  with the socket.

## [0.32.2] - 2026-09-09

### Fixed
- Generic process payloads carried `timestamps.start` as a string;
  strict clients (Equibop) silently drop mistyped frames, so generic
  cards never displayed. Epoch millis are now emitted as a number end
  to end (`DetectableActivity.timestamp`, `ScannedGame.start`,
  `ProcessTimestamps.start`).
- The scan loop forwarded only the first detected game, starving
  co-running games (WuWa + NTE). Every detected game now gets its own
  per-slot event with per-id dedup and no cross-clearing; the null
  event drains every armed slot.
- App slots whose IPC/WS owner died without CLEAR stayed suppressed by
  a ghost owner forever (`send_empty` carries no app id). Abrupt closes
  now release every slot owned by the dead pid and resume their
  generics.
- Steam AppId fallback via command line (`reaper SteamLaunch
  AppId=4508340 ...`) when `/proc/<pid>/environ` is unreadable.
  Sandboxed Proton runtimes (pressure-vessel/bwrap) hide environ from
  service contexts while cmdline stays readable — without this, games
  like NTE are invisible to automatic detection.

## [0.32.1] - 2026-09-08

### Added
- `docs/systemd-user-units.md`: practical guide to user units
  (anatomy, drop-ins, lifecycle, logs, linger, gotchas, from-scratch
  recipe) with real examples from this setup.

### Fixed
- `party.privacy` (0 = private, 1 = public) and `status_display_type`
  (0 = name, 1 = state, 2 = details) were silently dropped on parse
  (serde ignores unknown fields) — real SDKs (pypresence, Tauon) send
  them. Both now cross the bridge untouched.

## [0.32.0] - 2026-09-08

### Added
- arRPC protocol parity: `SUBSCRIBE`/`UNSUBSCRIBE` blind ACKs, `Unknown
  command` / `CONNECTIONS_CALLBACK is not supported` ERROR replies
  (code 1000), invite-code validation (4011/4017), malformed-frame
  rejection (4005), 1 MiB IPC payload limit with `1003` close, refusal
  of unknown packet types (1003) instead of misreading them as frames.
- Microsecond/nanosecond timestamp normalization to milliseconds
  (seconds/milliseconds behavior unchanged).
- Bridge port-range scan (`--bridge-port`..`--bridge-port-end`,
  default 1337-1347 like arRPC) with MessagePack collision skip.
- Bounded bridge replay cache (50 entries, oldest-first eviction) with
  30s rebroadcast refresh for late/missed clients.
- Runtime identity: `ADSUM_USER_*` startup overrides plus bridge
  `SET_USER`/`RESET_USER` with ACKs (whitelisted keys only, blank
  strings ignored like the env overrides, huge integers clamped
  instead of wrapping); READY frames reflect the current identity on
  every transport.
- Presence state snapshot (`ADSUM_STATE_FILE=1` writes
  `<tmpdir>/adsum-state-{0..9}` with servers + activities, arRPC
  layout, `adsum-` prefix); `--list-database` summary diagnostics.
- Official-protocol gaps closed: `GET_USER` (current identity, or
  `null` for other ids) on IPC + websocket; `GIFT_CODE_BROWSER`
  forwarded like the other `*_BROWSER` commands (`4016` on missing
  code); clickable-asset URL fields (`details_url`, `state_url`,
  `large_url`, `small_url`) preserved through the bridge;
  `CURRENT_USER_UPDATE` DISPATCH fanned out to bridge clients on
  `SET_USER`/`RESET_USER`.
- Honest errors for known-but-unbacked commands (shared table):
  OAuth → `5000`, activity invites → `5006`, voice/guilds/overlay/
  store → "requires the real Discord client"; genuinely unknown
  commands still get `Unknown command`.
- IPC-wins handoff: generic process detection shows immediately and
  yields its slot when a game SDK publishes for the same app (last
  publisher wins across companions; stale closes ignored), resuming
  while the game process is still alive when the source clears.
  `--ignore-ids` stays as full-silence opt-in, no longer needed for
  companions.
- Stale IPC socket PING/PONG probe (1s): wedged holders are reclaimed,
  live holders (Discord/arRPC/Adsum) are left alone.
- `--ignore-ids` / `ADSUM_IGNORE_IDS`: scan-only coexistence filter —
  ignored application IDs behave as absent (null event, clear) so a
  richer publisher owns those slots; forwarded client frames always
  pass, and `--list-detected` still shows ignored games.
- Log severity system (`DEBUG`/`INFO`/`WARN`/`ERROR`): `ADSUM_LOG_LEVEL`
  sets the floor (default `info`), `--debug` / `ADSUM_DEBUG=1` forces
  debug and prints the resolved config; `INFO` keeps the historical
  untagged shape, other levels are tagged.
- Presence log lines: `Published: {name} (app {id}, pid {pid})` via a
  `display_name()` fallback (name → details → state → `?`), `INFO` on
  change and `DEBUG` on duplicate republishes/clears.

### Changed
- Tree-wide log reclassification: per-tick chatter (scan ticks, match
  details, repeat sends) demoted to `DEBUG`, fallbacks/retries/prunes
  to `WARN`, so the default log shows one line per state change.
- Hourly DB refresh logs condensed to one line per check (`DB check:
  unchanged (etag …)` / `same bytes` / `updated: N entries`).
- `SUBSCRIBE`/`UNSUBSCRIBE` and unknown/unbacked commands are answered
  at the edge (ACK/ERROR) and no longer forwarded to bridge clients;
  only `SET_ACTIVITY` and the known secondary commands
  (`INVITE_BROWSER`, `GUILD_TEMPLATE_BROWSER`, `GIFT_CODE_BROWSER`,
  `DEEP_LINK`) reach the bridge.
- State snapshot is rewritten on every 30s refresh tick, so a
  live-but-idle daemon never looks stale to slot reuse.

### Fixed
- `SET_USER` patch can no longer wipe identity fields with blank
  strings or wrap `flags`/`premium_type` on out-of-range integers.

## [0.31.0] - 2026-09-07

### Added
- Install-folder fallback in process detection (Hydra / non-Steam layouts
  with generic exes), cwd-joined probe for bare-exe Proton launches.
- Suspended (SIGSTOP'd) processes count as absent; republished on resume.
- Conditional database refresh (ETag + content hash): unchanged hours cost
  one header round trip, no parse or rebuild.
- Startup database ownership moves to the scanner (no pinned duplicate
  generation); refresh ETag seeded from the startup fetch.
- Fail-fast supervision: worker panics exit for systemd restart; hot-path
  channel sends are fail-soft.
- Bridge backpressure: dead websocket clients are pruned on failed send.
- `spectate`/`match` activity secrets preserved through the bridge.
- Empty database updates refused (keeps serving current data).
- CI: test + doc jobs, `--locked` everywhere, modern actions, Node 24
  runtimes, cross-platform (Linux/macOS/Windows) clippy.

### Fixed
- Stuck presence after game close (dual-keyspace clear: IPC pid-keyed vs
  process app-id-keyed desync).
- Server panic on malformed WS disconnect without args.
- macOS/Windows builds (Linux-only `malloc_trim`, `read_steam_app_id`,
  `parse_stat_state` gating; per-OS memory release).

### Changed
- Scan I/O: `/proc/<pid>/environ` read only on executable-pattern misses.
- Logging is lazy (no allocation when disabled); DB parses skip the JSON
  DOM (`serde` ignores unknown fields).
- Path-variant scratch buffers reused across processes/scans.

## [0.30.0] - 2026-09-06

### Added
- Dual-protocol bridge (JSON + MessagePack), bundled offline detectable
  snapshot with hourly refresh, custom overrides.
- Install-folder fallback in process detection.
- Single-shot `--list-detected` diagnostics.

### Fixed
- Stuck presence after game close.

[Unreleased]: https://github.com/yiesko/adsum/compare/v0.39.0...HEAD
[0.39.0]: https://github.com/yiesko/adsum/releases/tag/v0.39.0
[0.38.1]: https://github.com/yiesko/adsum/releases/tag/v0.38.1
[0.38.0]: https://github.com/yiesko/adsum/releases/tag/v0.38.0
[0.36.1]: https://github.com/yiesko/adsum/releases/tag/v0.36.1
[0.36.0]: https://github.com/yiesko/adsum/releases/tag/v0.36.0
[0.35.0]: https://github.com/yiesko/adsum/releases/tag/v0.35.0
[0.34.0]: https://github.com/yiesko/adsum/releases/tag/v0.34.0
[0.33.1]: https://github.com/yiesko/adsum/releases/tag/v0.33.1
[0.32.2]: https://github.com/yiesko/adsum/releases/tag/v0.32.2
[0.32.1]: https://github.com/yiesko/adsum/releases/tag/v0.32.1
[0.32.0]: https://github.com/yiesko/adsum/releases/tag/v0.32.0
[0.31.0]: https://github.com/yiesko/adsum/releases/tag/v0.31.0
[0.30.0]: https://github.com/yiesko/adsum/releases/tag/v0.30.0
