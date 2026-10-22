# Operating Adsum under systemd

Setup (installing the unit, drop-ins, linger) is covered in
[systemd-user-units.md](systemd-user-units.md). This page is about running
the service: reading logs, diagnosing the process watcher, updating the
binary, and known gotchas.

## 1. Logs (everything goes to the journal)

```bash
journalctl --user -u adsum.service -f              # live
journalctl --user -u adsum.service --since "10 min ago" | grep -iE "warn|error"
journalctl --user -u adsum.service -b              # since boot
```

Level control (stderr → journal):

- `RUST_LOG` wins when set (full `tracing` filter, e.g. `RUST_LOG=debug`).
- Otherwise `ADSUM_DEBUG=1` (or `-D` on the command line) selects `debug`.
- Default is `info`.

`INFO` lines are one per state change (detects, clears, connects, hourly
DB checks); per-tick chatter needs debug. Module names are omitted from
the output. `ADSUM_LOG_LEVEL` and `ADSUM_LOGS_ENABLED` do not exist in
the current binary — the options above are the only knobs.

## 2. Diagnosing the process watcher

A healthy Linux boot logs:

```
[Process Scanner] proc-events watcher live (netlink cn_proc; best-effort, may rarely go silent — polling backstops)
```

If that line is missing, the event watcher did not start:

- `ADSUM_NO_PROC_EVENTS=1` disables it on purpose, or
- the unit restricts address families without `AF_NETLINK` (the shipped
  unit includes it: `RestrictAddressFamilies=... AF_NETLINK`), or
- you are not on Linux.

Polling keeps working either way — games are detected at the scan cadence
(`ADSUM_SCAN_INTERVAL`) instead of instantly. Verify with
`adsum --list-detected`.

## 3. Updating the binary

**Why you cannot just overwrite it:** the service executes the file in
place, so `cp` over a running binary fails with `Text file busy`. Stop
the service first — or use the built-in updater below.

The shipped unit lists `~/.local/bin` in `ReadWritePaths`, so the
sandboxed service can swap its own binary: stage an update, restart, and
the swap happens during startup (the staged hash is checked again, atomic
rename, same-PID re-exec).

### Built-in OTA

```bash
adsum --update --yes               # stage + verify from your shell
systemctl --user restart adsum.service # applies the staged file at start
```

Hands-off variant: add `Environment=ADSUM_AUTO_UPDATE=1` to a drop-in —
the daemon stages updates in the background while it runs, and the next
restart applies them (see [self-update.md](self-update.md)). The daemon
never restarts itself.

### Manual copy

```bash
systemctl --user stop adsum.service
cp ./adsum ~/.local/bin/adsum   # release binary you downloaded, or target/release/adsum
systemctl --user start adsum.service
systemctl --user status adsum.service
```

### Rolling back

```bash
adsum --rollback                   # swaps adsum.prev back, re-execs once, exits
systemctl --user restart adsum.service # service picks up the restored binary
```

## 4. Known gotchas

- **Restart with a busy IPC socket**: normal. Adsum probes the holder
  behind `discord-ipc-0` with a PING (1s budget); anything live keeps the
  path and Adsum binds `discord-ipc-1` instead. It goes back to `-0` on
  the next clean start once the old holder is gone. The WARN at that
  moment is expected, not an error.
- **SIGTERM vs SIGINT**: `systemctl --user stop` sends SIGTERM, which has
  no in-process handler — only Ctrl+C (SIGINT) runs the graceful shutdown
  that removes the socket files. After a `stop`, leftover
  `discord-ipc-*` files are swept by the unit's `ExecStartPre` on the
  next start. Known behavior, not a bug.
- **No `--ignore-ids` needed for companions**: since Adsum 0.32.0 the
  handoff does this automatically (generic card shows → the game's own
  SDK takes over → generic resumes on clear). Details:
  [process-detection.md](process-detection.md). The flag stays as a
  full-silence opt-in.
- **Home read-only**: the cache directory (XDG default `~/.cache/adsum`)
  is created by systemd through `CacheDirectory=` and stays writable; the
  config directory (`~/.config/adsum` by default) is read-only, which is
  all the service needs. `%t` (sockets) and `~/.local/bin` (the update
  swap) are the other writable paths. Anything else you add to the unit
  needs its own directory directive or `ReadWritePaths` entry.

## See also

- [systemd-user-units.md](systemd-user-units.md) — install, drop-ins,
  lifecycle, linger.
- [self-update.md](self-update.md) — verification and refusal rules for
  `--update`.
- [cli-options.md](cli-options.md) — every flag and environment variable.
