# Adsum

[![Latest release](https://img.shields.io/github/v/release/yiesko/adsum)](https://github.com/yiesko/adsum/releases/latest)
[![Build](https://img.shields.io/github/actions/workflow/status/yiesko/adsum/build.yml?branch=main&label=build)](https://github.com/yiesko/adsum/actions/workflows/build.yml)
[![Code quality](https://img.shields.io/github/actions/workflow/status/yiesko/adsum/code_quality.yml?branch=main&label=quality)](https://github.com/yiesko/adsum/actions/workflows/code_quality.yml)
[![License: MIT](https://img.shields.io/github/license/yiesko/adsum)](LICENSE)

**A standalone Discord Rich Presence server, CLI, and Rust library.**

Adsum implements Discord-compatible local RPC endpoints for supported alternative clients. It detects running games, accepts activity from games and companion apps, and forwards presence to clients through a local WebSocket bridge. It continues the work of the original [SpikeHD/rsRPC](https://github.com/SpikeHD/rsRPC) project and is inspired by [OpenAsar/arRPC](https://github.com/OpenAsar/arRPC).

> [!IMPORTANT]
> **Adsum does not replace or work alongside the official Discord desktop client's RPC implementation.** Use it with a supported alternative client that can disable its bundled RPC server and consume Adsum's bridge. The OAuth `AUTHORIZE` flow is not supported; see [Known limitations](#known-limitations).

## Features

- **Automatic game detection** — detects native games, Proton/Wine games, Steam titles, Epic/Heroic titles, and non-Steam shortcuts using executable paths, Steam AppIDs, Epic sandbox IDs, and fallback matching.
- **Fewer misleading detections** — when an entry declares executables, the detected process must match a declared game executable; a Steam AppID alone is not enough for those entries.
- **Discord-compatible transports** — accepts game RPC over local IPC and WebSocket endpoints.
- **Local bridge** — publishes activity to WebSocket clients as JSON on port `1337` and MessagePack on port `1338` by default.
- **Clean activity handoff** — when a game publishes its own activity, that takes priority over the generic activity detected from its process, and the generic activity can return when the richer publisher stops.
- **Custom game entries** — add or replace detection rules using `overrides.json` or `overrides.d/` without editing the bundled database.
- **Offline by default** — ships with a game-detection database; refreshing it from Discord's endpoint is optional.
- **Diagnostics** — inspect detected games and database entries, and trace why a game appeared or disappeared.
- **Verified releases** — the installer verifies the signed checksum manifest and release artifacts. Background update staging is opt-in.

## Screenshots

A game can report its own activity over IPC while a companion application reports a listening activity, and both can coexist through Adsum:

![Roblox running through Sober alongside a Limusic listening activity through Adsum](assets/rpc_sober_limusic.png)

A closer look at the listening activity:

![Limusic activity card with track progress and YouTube Music buttons](assets/rpc_limusic.png)

## Compatible Discord clients

The following alternative clients have an RPC or bridge integration that Adsum can work with:

- [Equibop](https://github.com/Equicord/Equibop)
- [Vesktop](https://github.com/Vencord/Vesktop)
- [Dorion](https://github.com/SpikeHD/Dorion)
- [GoofCord](https://github.com/Milkshiift/GoofCord)

If a client bundles its own RPC server, disable that server before running Adsum. The client-side plugin or integration must consume Adsum's bridge, which defaults to `ws://127.0.0.1:1337`. The exact configuration depends on the client.

## Install (Linux + systemd)

The installer requires [`minisign`](https://github.com/jedisct1/minisign) to verify release signatures. Review the [installer script](scripts/install.sh) before running it.

```bash
curl -fsSL https://raw.githubusercontent.com/yiesko/adsum/main/scripts/install.sh | bash
```

The installer detects your architecture, downloads the latest release binary and systemd user unit, verifies both against the signed checksum manifest, then enables and starts the user service. Running it again updates the installation.

For systems without systemd, download a binary from [Releases](https://github.com/yiesko/adsum/releases) and run it directly. The bundled game database does not require a network connection.

See [systemd installation and operations](docs/systemd-user-units.md) for install options, updates, and uninstall instructions.

## Quick start and diagnostics

After installing, the systemd user service starts automatically. Use the installed binary for one-shot diagnostics; these commands do not start a second long-running server:

```bash
~/.local/bin/adsum --list-detected
~/.local/bin/adsum --list-database
```

Follow the running service's logs:

```bash
journalctl --user -u adsum.service -f
```

`--list-detected` runs a single scan and exits. It applies the configured overrides and ignore list, so its output shows what the scanner would publish. For every CLI flag and environment variable, see [CLI options](docs/cli-options.md).

## Platform support

Linux is the primary platform for development and for real-world testing. CI runs tests, Clippy, formatting and documentation checks on Linux, Windows and macOS, and release builds are published for all three. Support on Windows and macOS is therefore tested, but it receives far less real-world validation and should be considered best-effort.

## Known limitations

- **No OAuth / `AUTHORIZE` flow.** Adsum cannot complete authorization that requires the official Discord client and an application's private `client_secret`. Games that depend on this flow may need direct access to the official client; stop Adsum while using them.
- **Unreadable Linux command lines are skipped.** Detection reads `/proc/<pid>/cmdline`. If the kernel denies access — for example, for another user's process or under a restrictive `hidepid` mount — that process is skipped before game matching runs.
- **Some database entries have no declared executables.** Those entries can be matched by Steam AppID alone, so a helper process that inherits such an ID can still cause a false positive. Tightening that fallback any further would also lose detections for games that provide no executable names.
- **Official Discord can win discovery.** When the official Discord client is running at the same official socket path, a client that probes index `0` first may connect to official Discord instead of Adsum. Coexistence is not promised; see [official Discord discovery conflict](docs/protocol.md#official-discord-discovery-conflict).
- **Local IPC connections are a bounded resource.** Adsum serves at most 16 concurrent IPC pumps with a 1 MiB payload limit. A local client can occupy every pump with idle or incomplete connections; handshake, frame-read, and idle timeouts are deferred. See [residual connection-exhaustion risk](docs/protocol.md#residual-connection-exhaustion-risk).

See [Process detection](docs/process-detection.md) for the matching rules and their edge cases, and [Detection traceability](docs/detection-traceability.md) for diagnosing activity lifecycle events.

## Configuration

Most options have both a CLI flag and an environment variable. These are the main settings; the complete reference is in [docs/cli-options.md](docs/cli-options.md).

| Setting | Environment variable | Default | Purpose |
|---|---|---|---|
| JSON bridge port range | `ADSUM_BRIDGE_PORT` / `ADSUM_BRIDGE_PORT_END` | `1337`–`1347` | WebSocket JSON bridge |
| MessagePack bridge port | `ADSUM_MSGPACK_PORT` | `1338` | WebSocket MessagePack bridge |
| Scan interval | `ADSUM_SCAN_INTERVAL` | `5` seconds | Base process-scan cadence |
| Database refresh | `ADSUM_ENABLE_DB_UPDATE` | Off | Refresh the game database hourly |
| Ignored game IDs | `ADSUM_IGNORE_IDS` | None | Skip these IDs in process detection |
| Background update staging | `ADSUM_AUTO_UPDATE` | Off | Stage verified updates in the background |

Logs go to stderr. `RUST_LOG` overrides the log level; when investigating detection, prefer a scoped filter such as `RUST_LOG=adsum_detect=debug` instead of enabling debug output for every component.

## Using Adsum as a Rust library

`adsum-core` is currently consumed from a Git tag rather than crates.io:

```toml
[dependencies]
adsum-core = { git = "https://github.com/yiesko/adsum", tag = "v0.40.0" }
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

`run_until` consumes the daemon. For one-shot inspection, use `detect_once` or `database_summary` before starting it. See [library documentation](docs/library.md) for constructors and details.

## Build and test from source

Requirements: [Rust and Cargo](https://www.rust-lang.org/) 1.95 or newer.

```bash
git clone https://github.com/yiesko/adsum
cd adsum
cargo build --release -p adsum-cli
cargo test --workspace --locked
cargo test --benches --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

The detection database is committed to the repository, so a fresh checkout builds without downloading it. To regenerate the snapshot, run:

```bash
cargo run --manifest-path tools/updater/Cargo.toml
```

## Documentation

- [CLI options](docs/cli-options.md) — flags and environment variables
- [Process detection](docs/process-detection.md) — executable matching, Steam and Epic IDs, Proton/Wine and overrides
- [Detection traceability](docs/detection-traceability.md) — detection sources, latency and clear reasons
- [Protocol](docs/protocol.md) — IPC/WebSocket messages and error codes
- [Web clients](docs/web-client.md) — browser, Vencord and Node clients
- [Rust library](docs/library.md) — embedding the daemon
- [Systemd setup](docs/systemd-user-units.md) — installation and service configuration
- [Systemd operations](docs/systemd-operations.md) — logs, updates and troubleshooting
- [Self-update](docs/self-update.md) — release verification and update staging
- [Changelog](CHANGELOG.md) — releases and compatibility notes

## Project scope and contributions

The feature scope is frozen. Work focuses on fixing bugs, improving detection accuracy, security, performance, tests and documentation rather than adding new features. Feature requests outside that scope are unlikely to be accepted.

Useful contributions include reproducible bug reports, false-positive and false-negative examples, regression tests, documentation fixes, and real-world validation on Windows and macOS. When reporting a detection issue, please include the operating system, the affected game's process path if you are comfortable sharing it, and the output of `adsum --list-detected`.

[Report a bug or detection issue](https://github.com/yiesko/adsum/issues/new) · [Browse open issues](https://github.com/yiesko/adsum/issues)

## Credits

- [SpikeHD/rsRPC](https://github.com/SpikeHD/rsRPC) — the original project this repository continues from.
- [OpenAsar/arRPC](https://github.com/OpenAsar/arRPC) — inspiration for the local RPC server, detectable database format and bridge behavior.
- [pog5/rsrpc](https://github.com/pog5/rsrpc) — reference for process-detection behavior and test coverage.

## License

Adsum is licensed under the [MIT License](LICENSE).