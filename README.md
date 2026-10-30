<div align="center">

# Adsum

**Show what you're playing on Discord, automatically.**<br>
Rich Presence for Linux gaming and alternative Discord clients: one small Rust service, no Node.js.

[![Latest release](https://img.shields.io/github/v/release/yiesko/adsum)](https://github.com/yiesko/adsum/releases/latest)
[![Build](https://img.shields.io/github/actions/workflow/status/yiesko/adsum/build.yml?branch=main&label=build)](https://github.com/yiesko/adsum/actions/workflows/build.yml)
[![Code quality](https://img.shields.io/github/actions/workflow/status/yiesko/adsum/code_quality.yml?branch=main&label=quality)](https://github.com/yiesko/adsum/actions/workflows/code_quality.yml)
[![License: MIT](https://img.shields.io/github/license/yiesko/adsum)](LICENSE)

[Quick start](#quick-start) · [Compatible clients](#compatible-clients) · [Troubleshooting](#troubleshooting) · [Docs](#documentation) · [Releases](https://github.com/yiesko/adsum/releases)

<img src="assets/rpc_sober_limusic.png" alt="Roblox running through Sober, shown next to a Limusic listening activity, both published through Adsum" width="560">

</div>

Adsum is a background service that notices the games you launch and publishes them as Discord Rich Presence. It also speaks Discord's local RPC protocol, so games and companion apps (music players, launchers) can report their own, richer activity through it.

Alternative Discord clients need a local RPC server to show games. Adsum is one: it detects games, accepts activity from apps, and forwards everything to your client over a local WebSocket bridge.

> [!IMPORTANT]
> **Adsum is for alternative Discord clients.** It does not replace, and does not work alongside, the official Discord desktop client's RPC. Use it with a [supported client](#compatible-clients) that can turn off its bundled RPC server and read Adsum's bridge.

## Why Adsum

- **It recognises what you actually run.** Native Linux games, Windows games under Proton or Wine, Steam titles (by AppID), Epic/Heroic titles, and non-Steam shortcuts.
- **Fewer wrong "Now playing" cards.** When a database entry declares executables, the running process has to match one of them. A Steam AppID alone isn't enough.
- **Install once, forget it.** A single command sets up a systemd *user* service. No root, no Node.js, no per-session terminal.
- **Works offline.** A game database is bundled in the binary. Refreshing it from Discord is optional.
- **Richer activity wins.** If a game reports its own activity, it takes priority over the generic one detected from its process. The generic card comes back when the richer publisher stops.
- **Easy to debug.** `adsum --list-detected` shows what the scanner sees, and detection can be traced to explain why a game appeared or disappeared.
- **Yours to tweak.** Add or replace detection rules with `overrides.json` or `overrides.d/`, and ignore game IDs you don't want shown.
- **Verified releases.** The installer checks a signed checksum manifest. Background updates are opt-in.

## Quick start

### 1. Install

Adsum's installer needs [`minisign`](https://github.com/jedisct1/minisign) to verify release signatures:

```bash
sudo pacman -S minisign     # Arch
sudo dnf install minisign   # Fedora
sudo apt install minisign   # Debian / Ubuntu
```

Then run the installer ([read it first](scripts/install.sh) if you like):

```bash
curl -fsSL https://raw.githubusercontent.com/yiesko/adsum/main/scripts/install.sh | bash
```

It detects your architecture, downloads the latest release binary and the systemd user unit, verifies both against the signed manifest, then enables and starts the service. **Run it again any time to update.**

<details>
<summary>No systemd?</summary>

Download a binary from [Releases](https://github.com/yiesko/adsum/releases) and run it directly. The bundled game database needs no network connection.

</details>

### 2. Connect your Discord client

1. **Turn off the client's built-in RPC server** (for example a bundled arRPC), so the two don't compete.
2. **Point the client at Adsum's bridge:** `ws://127.0.0.1:1337`.
   - Vencord-based clients can use the optional [`plugin/adsum.js`](plugin/adsum.js), which registers itself as a plugin named **Adsum**.
   - Any other arRPC-compatible WebSocket client can connect to the bridge directly.

The exact steps depend on the client. See [Compatible clients](#compatible-clients) and the [web client guide](docs/web-client.md).

<!-- TODO(maintainer): add a verified, step-by-step setup per client (Equibop, Vesktop, Dorion, GoofCord), including the client version tested. This is the highest-value section for new users. -->

### 3. Launch a game

Your game should show up on your Discord profile within a few seconds. To check what Adsum sees:

```bash
~/.local/bin/adsum --list-detected     # one scan, then exit
~/.local/bin/adsum --list-database     # entries in the game database
journalctl --user -u adsum.service -f  # follow the service logs
```

`--list-detected` applies your overrides and ignore list, so its output is exactly what the scanner would publish. These commands don't start a second server.

## Compatible clients

These alternative clients have an RPC or bridge integration Adsum can work with:

| Client | Notes |
|---|---|
| [Equibop](https://github.com/Equicord/Equibop) | Disable the built-in RPC, then consume the bridge |
| [Vesktop](https://github.com/Vencord/Vesktop) | Disable the built-in RPC, then consume the bridge |
| [Dorion](https://github.com/SpikeHD/Dorion) | Disable the built-in RPC, then consume the bridge |
| [GoofCord](https://github.com/Milkshiift/GoofCord) | Disable the built-in RPC, then consume the bridge |

Using a client that isn't listed? If it can read an arRPC-style WebSocket bridge, it should work. Tell us how it went in [Discussions](https://github.com/yiesko/adsum/discussions).

## What Adsum detects

| Source | How it's recognised |
|---|---|
| Native Linux games | Executable paths and names matched against the game database |
| Proton / Wine games | Windows executables running under Proton or Wine |
| Steam titles | Steam AppID, plus an executable check when the entry declares executables |
| Epic / Heroic titles | Epic sandbox IDs |
| Non-Steam shortcuts | Fallback matching |
| Games and apps that report their own activity | Discord-compatible IPC and WebSocket endpoints |

Process scanning runs every **5 seconds** by default. The matching rules and their edge cases are in [Process detection](docs/process-detection.md).

## How it works

```text
 games & apps ── Discord IPC / WebSocket (6463–6472) ──┐
                                                       ▼
 running processes ── scanner + bundled game DB ──▶  Adsum  ──▶ JSON         ws://127.0.0.1:1337 ──┐
 (native · Proton · Steam · Epic)                              └▶ MessagePack ws://127.0.0.1:1338   ├──▶ your Discord client ──▶ Discord
                                                                                                    ┘
```

Late-connecting clients immediately receive the cached activities, so restarting your client doesn't lose your current game.

## Configuration

Most settings have both a CLI flag and an environment variable. The main ones:

| Setting | Environment variable | Default | Purpose |
|---|---|---|---|
| JSON bridge port range | `ADSUM_BRIDGE_PORT` / `ADSUM_BRIDGE_PORT_END` | `1337`–`1347` | WebSocket JSON bridge |
| MessagePack bridge port | `ADSUM_MSGPACK_PORT` | `1338` | WebSocket MessagePack bridge |
| Scan interval | `ADSUM_SCAN_INTERVAL` | `5` seconds | Base process-scan cadence |
| Database refresh | `ADSUM_ENABLE_DB_UPDATE` | Off for the bare binary, **on** in the installed service | Refresh the game database hourly |
| Ignored game IDs | `ADSUM_IGNORE_IDS` | None | Never publish these IDs |
| Allowed web origins | `ADSUM_BRIDGE_ALLOWED_ORIGINS` | Discord's web pages only | Let other browser origins use the bridge |
| Background update staging | `ADSUM_AUTO_UPDATE` | Off | Stage verified updates in the background |

For the service, put settings in a drop-in instead of editing the unit. Create `~/.config/systemd/user/adsum.service.d/override.conf`:

```ini
[Service]
Environment=ADSUM_IGNORE_IDS=0000000000000000000
Environment=ADSUM_SCAN_INTERVAL=3
```

```bash
systemctl --user daemon-reload
systemctl --user restart adsum.service
```

Custom game entries live in `~/.config/adsum/overrides.json` (or `overrides.d/`). Every flag and variable is in [CLI options](docs/cli-options.md).

## Troubleshooting

<details>
<summary><strong>My game doesn't show up in Discord</strong></summary>

1. Check the service: `systemctl --user status adsum.service`
2. Check detection: `~/.local/bin/adsum --list-detected`. If the game is listed, Adsum is working and the problem is on the client side (step 3). If it isn't, see "Adsum doesn't detect my game" below.
3. Check the client: its built-in RPC must be **off**, and it must be reading the bridge at `ws://127.0.0.1:1337`.
4. Read the logs: `journalctl --user -u adsum.service -f`

</details>

<details>
<summary><strong>Adsum doesn't detect my game</strong></summary>

The game may not be in the database, or its executable may not match the entry. Add a custom entry with `overrides.json` (see [Process detection](docs/process-detection.md)), and please [open an issue](https://github.com/yiesko/adsum/issues/new) with the output of `adsum --list-detected` so it can be fixed for everyone.

</details>

<details>
<summary><strong>Adsum shows the wrong game, or something I don't want shown</strong></summary>

Hide it with `ADSUM_IGNORE_IDS`. If it looks like a false positive, report it with the process path and the `--list-detected` output.

</details>

<details>
<summary><strong>I need more detail in the logs</strong></summary>

Add a drop-in with a scoped log filter, then reload and restart the service:

```ini
[Service]
Environment=RUST_LOG=adsum_detect=debug
```

</details>

<details>
<summary><strong>The port is already in use</strong></summary>

The JSON bridge tries ports `1337`–`1347` and the MessagePack port moves forward on collision. To pin them, use `--bridge-port`, `--bridge-port-end` and `--msgpack-port` (or the matching `ADSUM_*` variables).

</details>

<details>
<summary><strong>The official Discord client is running</strong></summary>

When official Discord is running at the same socket path, a client that probes index `0` first may connect to it instead of Adsum. Coexistence isn't promised. See [official Discord discovery conflict](docs/protocol.md#official-discord-discovery-conflict).

</details>

<details>
<summary><strong>Can I get instant detection instead of a 5-second scan?</strong></summary>

Adsum can watch process start and exit events from the kernel (`cn_proc`) instead of polling. The default service sandbox runs in a user namespace, where the kernel ignores those events, so it stays on polling. Enabling the watcher means **weakening the sandbox**, which lets the service read and write everything your account can, including `~/.ssh`. The trade-off and the exact drop-in are in the [systemd guide](docs/systemd-user-units.md#enabling-the-event-watcher-opt-in-weaker-sandbox). Most people don't need it.

</details>

## Platform support

**Linux is the primary platform** for development and real-world testing. CI runs tests, Clippy, formatting and documentation checks on Linux, Windows and macOS, and release builds are published for all three. Windows and macOS support is tested but gets far less real-world validation, so treat it as **best-effort**. Reports from those platforms are very welcome.

## Known limitations

- **No OAuth / `AUTHORIZE` flow.** Adsum can't complete authorization that needs the official Discord client and an application's private `client_secret`. Stop Adsum while using games that depend on it.
- **Unreadable command lines are skipped.** Detection reads `/proc/<pid>/cmdline`. If the kernel denies access (another user's process, or a restrictive `hidepid` mount), that process is skipped.
- **Some database entries declare no executables.** Those can match on Steam AppID alone, so a helper process that inherits such an ID can cause a false positive. Tightening this further would lose detections for games that provide no executable names.
- **Official Discord can win discovery.** See the [troubleshooting note](#troubleshooting) above.
- **Local IPC connections are a bounded resource.** Adsum serves at most 16 concurrent IPC connections with a 1 MiB payload limit. A local client can occupy every slot with idle or incomplete connections, because handshake, frame-read and idle timeouts are still to come. See [residual connection-exhaustion risk](docs/protocol.md#residual-connection-exhaustion-risk).

## Adsum and arRPC

[arRPC](https://github.com/OpenAsar/arrpc) is the reference implementation of this idea, and some clients bundle it. Adsum continues [rsRPC](https://github.com/SpikeHD/rsRPC) in Rust and uses an arRPC-style bridge, so arRPC-compatible WebSocket clients can read it.

**Use Adsum if** you play on Linux (native, Proton, Steam, Heroic) and want accurate detection you can inspect, a service that runs independently of your Discord client, and a single binary with no Node.js.

**You may not need Adsum if** your client's bundled RPC already shows your games correctly. Compatibility with every arRPC command isn't complete either, so check [Protocol](docs/protocol.md) if a specific game or app needs one.

## Using Adsum as a Rust library

`adsum-core` is currently consumed from a Git tag rather than crates.io:

```toml
[dependencies]
adsum-core = { git = "https://github.com/yiesko/adsum", tag = "v0.40.0" } # use the latest release tag
tokio = { version = "1.53", features = ["rt-multi-thread", "macros", "signal"] }
```

```rust
use adsum_core::{Daemon, RPCConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let daemon = Daemon::from_bundled(RPCConfig::default())?;
    daemon
        .run_until(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
```

`run_until` consumes the daemon. For one-shot inspection, use `detect_once` or `database_summary` before starting it. More in the [library documentation](docs/library.md).

## Build from source

Requires [Rust and Cargo](https://www.rust-lang.org/) 1.95 or newer.

```bash
git clone https://github.com/yiesko/adsum
cd adsum
cargo build --release -p adsum-cli
cargo test --workspace --locked
```

<details>
<summary>Full check suite and database regeneration</summary>

```bash
cargo test --benches --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

The detection database is committed to the repository, so a fresh checkout builds without downloading anything. To regenerate the snapshot:

```bash
cargo run --manifest-path tools/updater/Cargo.toml
```

</details>

## Documentation

- [CLI options](docs/cli-options.md): flags and environment variables
- [Process detection](docs/process-detection.md): executable matching, Steam and Epic IDs, Proton/Wine, overrides
- [Detection traceability](docs/detection-traceability.md): detection sources, latency and clear reasons
- [Protocol](docs/protocol.md): IPC/WebSocket messages and error codes
- [Web clients](docs/web-client.md): browser, Vencord and Node clients
- [Rust library](docs/library.md): embedding the daemon
- [Systemd setup](docs/systemd-user-units.md) and [operations](docs/systemd-operations.md): installation, logs, updates, uninstall
- [Self-update](docs/self-update.md): release verification and update staging
- [Changelog](CHANGELOG.md): releases and compatibility notes

## Contributing

Adsum is young, and the most useful help right now is real-world coverage:

- **Detection reports**: false positives and false negatives, with your OS, the game's process path (if you're comfortable sharing it) and the output of `adsum --list-detected`.
- **Client setup guides**: a short, tested walkthrough for Equibop, Vesktop, Dorion, GoofCord or another client.
- **Validation** on other distributions, on Windows and on macOS.
- **Packaging** for your distribution.
- **Bug reports, regression tests and documentation fixes.**

The core feature set is intentionally stable, with the focus on reliability, detection accuracy, security and performance. If you'd like something bigger, please start a [discussion](https://github.com/yiesko/adsum/discussions) before writing code.

[Report a bug or detection issue](https://github.com/yiesko/adsum/issues/new) · [Browse open issues](https://github.com/yiesko/adsum/issues)

## Credits

- [SpikeHD/rsRPC](https://github.com/SpikeHD/rsRPC): the original project this repository continues from.
- [OpenAsar/arRPC](https://github.com/OpenAsar/arRPC): inspiration for the local RPC server, detectable database format and bridge behaviour.
- [pog5/rsrpc](https://github.com/pog5/rsrpc): reference for process-detection behaviour and test coverage.

## License

Adsum is licensed under the [MIT License](LICENSE).
