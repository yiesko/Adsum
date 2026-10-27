# Process detection

Adsum finds running games without asking them anything. It scans the process
list, matches executables against the detectable database, and publishes a
Rich Presence card for whatever it finds. This page explains how that
matching works and how it coexists with games that report themselves.

## How a process is matched

**Executable names.** The scanner builds Aho-Corasick automatons over the
database's executable names and probes each process path against them. There
are three: the main database, the custom overrides, and — on Linux — a
Proton-only one holding the `win32` entries. Both sides are reversed and
normalized to `/` separators (arRPC parity), and matching is ASCII
case-insensitive. Each process path is also tried with its 64-bit markers
(`64`, `.x64`, `x64`, `_64`) removed, so a game installed as `wow64.exe`
still matches a database entry for `wow.exe`. This is the same approach arRPC
and pog5's rsrpc use.

**An automaton hit is not a match.** The automatons match substrings, so a
hit only says a declared name appears somewhere in the path — which by
itself is not evidence of anything. The executable that fired has to **name
the end of the probed string**, on a path-component boundary. Without this,
any process whose path merely *contains* a declared name reads as the game:
`.../hl2.exe.txt` and `.../hl2.exe.evil` would both be Half-Life 2.

A rejection costs that one match, not the whole probe. The probes walk their
automaton's overlapping matches and keep going, because a shorter name from a
different entry can end *inside* the path and hide the match that does name
the process — Minecraft declares a bare `java`, Illarion declares
`illarion/jre/bin/java.exe`, and on that path the short one ends first.

**Executables flagged `is_launcher` never become patterns.** Clicking Play on
Baldur's Gate III makes Steam run `larilauncher.exe` before the game starts,
and that process carries the game's store id. The database flags these names,
so the automaton skips them at build time.

**macOS bundles.** A declared name ending in `.app` names the bundle, not the
binary: macOS runs `.../Factorio.app/Contents/MacOS/factorio`, so the declared
name is a component in the middle of the path and never its tail. A `.app`
name at a component boundary matches when Apple's bundle interior (`Contents/`)
follows it. Without this, every macOS entry naming a bundle would be
undetectable.

**Proton and Wine.** On Linux, entries marked `win32` are matched by a third
automaton, because a Windows game running under Proton has a Windows
executable path. Without it, those games would never match. It runs **after**
the store-id tier, and a refusal from the native automaton does not stop it:
Hearts of Iron IV declares `hoi4` for Linux and `hoi4.exe` for Windows, and
the short Linux name must not hide the Wine game the Proton automaton can
name.

**Steam application ids.** When no executable matches, the scanner falls
back to Steam AppId. The id is read from the process environment
(`/proc/<pid>/environ`, `SteamAppId=`), then from `AppId=` on the command
line (for sandboxed Proton runtimes that hide the environment), and
finally from the install folder via Steam's own `libraryfolders.vdf` and
`appmanifest_*.acf` files. Ids in Steam's shortcut range (assigned by Steam
itself to non-Steam shortcuts) match by name before location.

**Epic sandbox ids and app names.** When no executable matches and the
Steam tier yields no hit, the scanner also reads the `-epicsandboxid=<id>`
argument Heroic / Epic put on the command line. It maps to the database's
`epic` distributor SKU (lowercased for the lookup). The argument is Epic's
documented launch contract, and Heroic/Legendary pass it in practice; see
[Sources](#sources).
The sandbox hides the environment from the service, so the command line is
the distributor evidence those launches can show.

Older Legendary releases (before 0.20.25) omit `-epicsandboxid` and pass
only `-epicapp=<app name>`. The scanner resolves that name to the same
`epic` SKU through Heroic's cached `store_cache/legendary_library.json`
(`library[].app_name` → `library[].namespace`). On Linux, the file is read
from `$ADSUM_HEROIC_ROOT` when set (exclusive), otherwise from
`$XDG_CONFIG_HOME/heroic`, `$HOME/.config/heroic`, or the Flatpak
`$HOME/.var/app/com.heroicgameslauncher.hgl/config/heroic`; it is re-read
only when its mtime or size moves. The sandbox id stays the first choice:
the app-name lookup runs only when it yields no hit. Either way the id is
corroborated by the process path, exactly like Steam's.

**The id has to be corroborated too.** An id says which game was *launched*;
it does not say which binary is *running*. Steam hands its own helpers the
game's environment, and those helpers run during an update. So when the entry
declares executables, the process must be running one of them: the declared
name must end the path, or — for a short Proton/Heroic launch — its bare
basename must, when the declared directory is absent. A second, independent
piece of evidence is the install dir: when Steam's own `libraryfolders.vdf`
and `appmanifest_*.acf` files say the AppId owns
`steamapps/common/<installdir>/`, a process running under that tail is
corroborated even if its binary is not in the database. Demos and
edition-specific binaries are the motivating case, and Proton paths such as
`S:/steamapps/common/...` work because only the tail is compared, not the
host prefix. The tail must sit on a component boundary (`Game/` never matches
`Game 2/`), a path containing `..` is refused outright, only `common/` counts
(`compatdata/` is runtime state, not an install, even when a prefix's virtual
drive contains a nested `steamapps/common`), and a tail claimed by two
different AppIds — two libraries with the same tail, or duplicate manifests in
one library — is refused as ambiguous. An entry with **no** declared
executables is the exception and stands on the id alone, which is what that
tier is for.

The install-dir route is not "accept the AppId alone": it still refuses a
process that names a declared launcher (`is_launcher`), and the exclusion
list keeps its veto. A prerequisite installer on a `_CommonRedist`, `_redist`,
`redist` or `directx` shelf at any level below the install dir is refused
too: Steam runs those with the game's AppId. Steam helpers outside the
install dir (the `reaper`, the runtime, the client, `winedevice.exe`) never
gain this evidence.

The `steam-library` fallback does not use the install-dir evidence: it
derives the AppId from that same path, so the id and the path cannot
corroborate each other. That tier still requires a declared executable, or
an entry that declares none.

**Names.** When no automaton and no id resolve, the scanner falls back to the
executable stem or install-folder name. This runs for **any** entry, not only
those lacking executables, and it is the loosest tier by design — the
database declares normalized *names*, which is all an install folder has to
offer. It is gated to names that both contain a space and are at least six
characters, so a generic folder like `Games` never triggers a false positive
(`how to fish.exe` → `How to Fish`; `fish` alone never matches). Alternative
titles (`aliases`) are indexed too and join the same fallback, as do
punctuation-stripped variants of both (`R.E.P.O.`, `Q.U.B.E.`).

**A refusal stands these tiers down.** When an automaton saw a declared name
that did not name the path, that is specific evidence the process is *not*
the game — `.../Portal 2/portal2.exe.txt` holds `portal2.exe` without ending
with it. The name and folder heuristics are guesses built out of the install
location, and they do not run after one. A genuine no-match is different and
still reaches them: `.../Portal 2/launcher.exe` carries no declared name at
all, and reads as the game from its folder.

**Declared arguments.** Five entries require a string on the command line
alongside the executable — the Source games share one `hl2.exe` binary, and
what distinguishes Garry's Mod from Team Fortress 2 is `-game garrysmod` vs
`-game tf`, and Minecraft is `net.minecraft.client.main.Main`. When an entry
declares one and the process's command line does not carry it, the hit is
refused. This applies to the automaton tiers; the store-id tier does not
check it.

**Discord's exclusion list.** Installer and crash-reporter names and patterns
from Discord's exclusion endpoint never match. With `--enable-db-update`
they are refreshed hourly along with the database.

**Executable OS filter.** The main database is filtered by the executable's
OS (`win32`, `darwin`, `linux`), with the Proton fallback above as the
exception. Custom overrides bypass this filter entirely, which is how a
win32-only entry gets detected under Proton/Wine.

**Edge cases handled:**

- Launches with a bare executable name (no directories in `argv[0]`, common
  under Proton) are retried joined with the process working directory.
- Suspended processes (`SIGSTOP`'d, state `T`) count as absent — a frozen
  frame is not gameplay — and are republished when they resume.
- Corrupt or unreadable entries are skipped rather than crashing the scan.

## Fast path and polling

The scanner normally polls on a timer (`--scan-interval-secs`, default 5
seconds; quiet periods stretch by ×2 per empty tick up to 30 seconds, and
only while the event watcher is live). On top of that, a netlink `cn_proc`
watcher receives `EXEC` and `EXIT` events from the kernel, so a game appears
the moment it starts and clears the moment it exits.

While the watcher is live, polling runs **incrementally**: each pid's
classification is computed once — by the `EXEC` fast path or by the tick
that first sees the process — and cached. A tick then enumerates `/proc`
for liveness (a raw `getdents64` sweep, zero allocations), drops entries
whose process died, revalidates the handful of cached *hits* with one
cmdline read each (so a process that zombied between ticks can never hold
a card), and re-reads + reclassifies only pids whose cache entry is missing
or was classified against an older database generation. A steady tick
performs no cmdline reads, no automaton walks and no allocations at all.

The cache is never trusted past its evidence. Every channel that can lose
an `EXEC`/`EXIT` invalidates it wholesale: kernel sequence gaps, backlog
sheds and socket overruns flag the next tick to re-read everything, the
watcher flags both its death and its resubscribe, and a database refresh
(or override edit) invalidates by generation — each case costs exactly one
full-read tick, then the cache rebuilds itself. When the watcher is
disabled, not live, or the platform has no watcher at all (Windows,
macOS), every tick is the classic full sweep: enumerate, read every
cmdline, classify every process. Liveness itself always comes from the
fresh enumeration, never from the cache.

The kernel delivery is officially lossy, so the watcher is best-effort: it
can rarely go silent for stretches. When its self-test fails it resubscribes
instead of dying, except in a non-initial user namespace, where `cn_proc`
can never deliver: the watcher logs that once and stays off. Polling covers
everything either way. Use
`--no-proc-events` / `ADSUM_NO_PROC_EVENTS=1` to disable the watcher and
run polling only (which also disables the incremental cache), or `-n` /
`--no-process-scan` to disable detection entirely.

## IPC-wins handoff (generic card vs. companion)

When a game is only process-detected, Adsum shows a generic card right away.
The moment a game SDK (or a companion like wwrpc) sends its own
`SET_ACTIVITY` for the same app, the generic card is withdrawn and the
game's own presence takes over. When that source clears, the generic card
comes back while the game process is still alive — the check is
liveness-checked, so there is no flash when the game exits.

The takeover rule across companions is **last publisher wins**: a stale close
from a publisher that has already been superseded is ignored instead of
wrongly resuming the generic card. There is no wire-format change;
coexistence is keyed by the existing `socketId = pid` convention.

Because of this handoff, you usually no longer need `--ignore-ids` for
companions. The flag remains for slots you want silent no matter what.

## What the scanner never publishes

- `--ignore-ids <IDS>` / `ADSUM_IGNORE_IDS` — comma-separated application
  IDs the scanner treats as absent. **Scan-only by design**: frames that a
  game forwards through the bridge are always passed on, so a game that
  really is talking to Adsum is never silenced by this list.
- Anything on Discord's exclusion list.
- Anything that fails the OS filter (main database only; overrides bypass it).
- Steam's own helpers, by basename — a first cheap line, not the rule. The
  rule is the corroboration above: `reaper` is a plain word a game could
  ship as its own binary, so the list only trims what is already cheap to
  reject and cannot be the reason a helper is rejected.
- Executables the database flags `is_launcher`, in the store-id tier.
- A path that contains a declared name without ending with it, in the
  automaton tiers.

## Custom overrides

Overrides let you add, change, or replace entries without touching the
bundled database:

```json
[
  {
    "id": "1234567890",
    "name": "My Game",
    "hook": false,
    "executables": [
      { "name": "mygame.exe", "is_launcher": false, "os": "win32" }
    ]
  }
]
```

- Files are a JSON array or a single object.
- Resolution: `--overrides-file` > `$ADSUM_OVERRIDES_FILE` >
  `$XDG_CONFIG_HOME/adsum/overrides.json` > `~/.config/adsum/overrides.json`;
  the directory variant (`--overrides-dir`, `$ADSUM_OVERRIDES_DIR`,
  `~/.config/adsum/overrides.d`) works the same way and is merged with the
  file.
- Overrides are staged before anything else, so `--list-detected` shows
  exactly what the daemon would publish.
- They bypass the OS filter, which is how a win32-only entry gets detected
  under Proton/Wine.
- **They do not outrank the main database.** Precedence is per automaton, not
  per override: every path variant is probed against the main automaton
  first, and only then against the overrides, so a main pattern wins even
  when only a stripped 64-bit variant collides (main `wow.exe` against
  override `wow64.exe`). The store-id tier is the same: the main database is
  canonical and overrides are a linear fallback. To give an override
  priority, remove or rename the database entry it collides with.

The library exposes the same capability through
`append_detectables` / `remove_detectable_by_name` and
`adsum_core::overrides` — see [library.md](library.md).

## Checking your setup

```bash
./adsum --list-detected
# [adsum] Database: bundled (24299 entries)
# How to Fish (id 1542021058834468927) pid 1234
```

`--list-detected` runs a single scan and exits. Staged overrides and the
ignore-list both apply, so the output is exactly what the daemon would
publish.

## Sources

- Epic's [Testing Guide](https://dev.epicgames.com/docs/epic-games-store/testing-guide)
  documents the launch contract behind `-epicsandboxid`, and its [EOS Auth
  reference](https://dev.epicgames.com/docs/epic-online-services/accounts-and-social/eos-epic-account-services/auth-interface/auth-reference)
  covers the sandbox id.
- Heroic passes it in practice: its [public
  log](https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/issues/4273)
  shows the argument with `HEROIC_APP_RUNNER=legendary`, and the Legendary
  0.20.25 package
  [changelog](https://community.chocolatey.org/packages/legendary/0.20.25)
  records the parameter.
- The database stores the ids as `third_party_skus` entries with
  `distributor: "epic"`; the community-maintained [Userdoccers Application
  SKU object](https://docs.discord.food/resources/game#application-sku-object)
  defines that shape.
- Heroic keeps its per-store caches under its config root; community
  launchers parse the Legendary cache at
  `store_cache/legendary_library.json` — see Ludusavi's
  [Heroic/Legendary loader](https://github.com/mtkennerly/ludusavi/blob/master/src/scan/launchers/heroic/legendary.rs).
  The Flatpak build is
  [com.heroicgameslauncher.hgl](https://flathub.org/apps/com.heroicgameslauncher.hgl).
- The ids are real, not fixtures: the GTA V Enhanced sandbox
  [`b0cd075465c44f87be3b505ac04a2e46`](https://egdata.app/sandboxes/b0cd075465c44f87be3b505ac04a2e46)
  is on egdata (a community mirror), and the Among Us Store URL carries
  [`epic_gameId=33956bcb55d4452d8c47e16b94e294bd`](https://store.epicgames.com/p/among-us?epic_gameId=33956bcb55d4452d8c47e16b94e294bd).

## See also

- [detection-traceability.md](detection-traceability.md) — provenance,
  latency, and clear reasons for debugging.
- [cli-options.md](cli-options.md) — every flag and environment variable.
- [protocol.md](protocol.md) — how games report themselves over IPC and
  websockets.
- [library.md](library.md) — calling detection from Rust.
