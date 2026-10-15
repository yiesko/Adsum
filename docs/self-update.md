# Self-update (over-the-air)

rsRPC can update itself. Updates are downloaded, verified, and staged on
disk; the swap happens the next time the binary starts.

## Commands

| Flag | Environment variable | What it does |
|---|---|---|
| `--check-update` | `RSRPC_CHECK_UPDATE` | Ask GitHub for a newer release and exit. Exit code `2` means an update is available, `0` means you are current. |
| `--update` | `RSRPC_UPDATE` | Download and stage the newest release. It applies on the next start. |
| `--yes` | *(none)* | Skip the confirmation prompt (for scripts). |
| `--rollback` | `RSRPC_ROLLBACK` | Restore the binary the last update replaced, then exit. |
| `--auto-update` | `RSRPC_AUTO_UPDATE` | While the daemon runs, also stage updates in the background. |

```bash
./rsrpc-cli --check-update
# [rsrpc] update available: v0.38.1 (release v0.38.1); run with --update to stage it
# (exit code 2; exit code 0 means you are current)

./rsrpc-cli --update --yes     # stage now, apply on next start
./rsrpc-cli --rollback         # undo the last update
```

## Where files live

Staged files go to `~/.cache/rsrpc/ota/`. `$RSRPC_OTA_DIR` overrides the
location, and `$XDG_CACHE_HOME` is honored when it is set.

## Verification (fail-closed)

Nothing runs unless both checks pass:

1. The release publishes `SHA256SUMS.txt` and `SHA256SUMS.txt.minisig`.
   The signature must verify against the minisign public key embedded in
   the binary. The signing secret exists only in GitHub Secrets (plus an
   offline backup); the release workflow refuses to publish an unsigned
   release.
2. Only then is the downloaded binary hashed and compared against the
   manifest.

An unsigned, tampered, or truncated file is refused before it ever
executes.

## What happens on the next start

1. The staged file's SHA-256 is checked again against the recorded hash
   (the signature was verified when the file was staged).
2. The running binary is swapped out atomically; the previous image is kept
   next to it as `rsrpc-cli.prev`.
3. The process re-executes — on Linux in the same PID, so systemd never
   notices a restart.

`--rollback` swaps `rsrpc-cli.prev` back in place (clearing a pending
staged update first), then re-executes once against the restored image
and exits — the applied-guard keeps that pass from toggling the swap
again.

The swap must be able to write the binary's own directory. If it cannot
(read-only install, or a sandbox that does not allow it), the staged file
is discarded with `cannot replace binary` and the current version keeps
running. The shipped user unit allows it (`~/.local/bin` is in its
`ReadWritePaths`).

## Background checks

While the daemon runs, it checks for a new release once a day, starting
five minutes after boot, plus a small per-process jitter (up to an hour) so
many machines do not all ask at the same moment. By default it only logs
that something newer exists. With `--auto-update` / `RSRPC_AUTO_UPDATE=1`
it also stages the update — but applying still waits for the next start,
and the daemon never restarts itself.

## When it refuses to update

Self-update only manages installed binaries. It says so plainly and stops
when it finds:

- a development build under `target/debug` or `target/release`
  ("rebuild or `cargo install` instead of self-updating");
- a `cargo install` copy under `~/.cargo/bin` (cargo owns that file);
- a binary not named `rsrpc-cli`;
- a read-only install directory.

## Supported targets

**Background update staging only works on Linux x86_64 and Linux ARM64.**
Those are the only two raw binaries published as
`rsrpc-cli-x86_64-unknown-linux-gnu` and
`rsrpc-cli-aarch64-unknown-linux-gnu`. Anything else reports `no published
builds for <triple>` and keeps the version it has.

Every release carries **seven zipped builds**, one per CI target:

| Archive | Platform |
|---|---|
| `rsrpc-x86_64-unknown-linux-gnu.zip` | Linux x86_64 |
| `rsrpc-aarch64-unknown-linux-gnu.zip` | Linux ARM64 |
| `rsrpc-armv7-unknown-linux-gnueabihf.zip` | Linux ARMv7 |
| `rsrpc-x86_64-apple-darwin.zip` | macOS Intel |
| `rsrpc-aarch64-apple-darwin.zip` | macOS Apple Silicon |
| `rsrpc-x86_64-pc-windows-msvc.zip` | Windows x86_64 |
| `rsrpc-aarch64-pc-windows-msvc.zip` | Windows ARM64 |

Alongside them, each release ships `SHA256SUMS.txt` (plus its minisign
signature), the `systemd/rsrpc.service` unit file (covered by the same
manifest), and — on the two Linux targets above — the raw binary that
`--update` fetches. All of it is on the
[releases page](https://github.com/yiesko/discord-rich-presence/releases).

## See also

- [cli-options.md](cli-options.md) — the update flags in context.
- [systemd-operations.md](systemd-operations.md) — applying updates and
  rollbacks while rsRPC runs as a systemd service.
- [systemd-user-units.md](systemd-user-units.md) — running rsRPC as a user
  service.
