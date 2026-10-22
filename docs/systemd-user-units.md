# Systemd user units — setup guide (Adsum)

> Shortcut: `curl -fsSL https://raw.githubusercontent.com/yiesko/adsum/main/scripts/install.sh | bash`
> installs the binary, `systemd/adsum.service` and enables the service
> (see `scripts/install.sh --help`). This guide explains what that setup
> does, for manual installs and debugging.

Run `adsum` as a user service — no root needed. Day-to-day operations
(logs, updates, troubleshooting) live in
[systemd-operations.md](systemd-operations.md).

## 1. System vs user units

|  | System unit | User unit |
|---|---|---|
| Path | `/usr/lib/systemd/system/`, `/etc/systemd/system/` | `~/.config/systemd/user/` |
| Needs root | yes | **no** |
| Lifetime | whole machine | your login session (or boot, with linger) |

All commands are identical, with `--user` in the middle:

```bash
systemctl --user status adsum.service
```

## 2. Anatomy of `adsum.service`

This is the unit shipped in `systemd/adsum.service`:

```ini
[Unit]
Description=Adsum - Discord Rich Presence bridge
Documentation=https://github.com/yiesko/adsum
# Works offline via the bundled database; network only refines it.
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=3

[Service]
Type=exec
Environment=XDG_RUNTIME_DIR=%t
# CacheDirectory= creates the cache dir before the sandbox mounts; a
# missing path in ReadWritePaths= fails with 226/NAMESPACE on fresh installs.
CacheDirectory=adsum
# Port/cadence parametrization (clap reads ADSUM_* env). Defaults match
# the CLI; uncomment to pin alternatives for testing.
Environment=ADSUM_BRIDGE_PORT=1337
Environment=ADSUM_MSGPACK_PORT=1338
Environment=ADSUM_WS_PORT_START=6463
Environment=ADSUM_WS_PORT_END=6472
Environment=ADSUM_SCAN_INTERVAL=5
# Custom game mappings (missing file = no overrides, never fatal).
# The CLI resolves $XDG_CONFIG_HOME/adsum/overrides.json (default
# ~/.config/adsum).
# Drop top-level IPC entries before start (the "-" ignores failure).
ExecStartPre=-/bin/sh -c 'rm -f %t/discord-ipc-*; rm -f "${TMPDIR:-/tmp}/discord-ipc-*"'
# Official detectable list with trim, hourly refresh while running,
# offline bundled fallback. Add --auto-update (or ADSUM_AUTO_UPDATE=1
# in a adsum.service.d/override.conf drop-in) to also stage verified
# release binaries in the background.
ExecStart=%h/.local/bin/adsum --enable-db-update
Restart=on-failure
RestartSec=5s
TimeoutStopSec=5s
KillMode=mixed
NoNewPrivileges=yes
# The IPC fan-out symlinks discord-ipc-{n} into host /tmp (games probing
# the last official dir): ProtectSystem=strict mounts /tmp read-only, so
# the host path is bound back in writable. The daemon writes nothing else
# there (cache -> ~/.cache, sockets -> %t).
BindPaths=/tmp
ProtectSystem=strict
ProtectHome=read-only
# CacheDirectory= creates the cache dir (XDG default ~/.cache/adsum) and
# keeps it writable under the read-only home; the config dir is only read,
# so the read-only home already covers it. %t keeps the IPC sockets
# writable; %h/.local/bin lets the sandboxed service swap in staged
# self-updates.
ReadWritePaths=%t %h/.local/bin
# cn_proc watcher needs AF_NETLINK; without it the scanner silently stays
# on polling (diagnose via the missing "watcher live" boot line).
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
LockPersonality=yes
# Kernel, clock, hostname and control-group interfaces: the daemon reads
# the clock but never touches any of these, so they stay read-only or
# invisible.
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectClock=yes
ProtectHostname=yes
ProtectControlGroups=yes
# No device access beyond /dev/null, /dev/zero and /dev/urandom, which
# stay available: headless daemon, no GPU/audio/input needed.
PrivateDevices=yes
# No setuid/setgid binaries are ever executed; no realtime scheduling
# or memory locking is used anywhere in the stack.
RestrictSUIDSGID=yes
RestrictRealtime=yes
# Syscall allowlist: the broad @system-service set covers sockets (TCP,
# netlink), file IO, threads, timers and signals. Verified against a live
# boot (startup DB fetch exercises TLS); if a future dependency needs
# more, denied calls show up in the audit trail before anything breaks
# silently — the service fails closed instead of running crippled.
SystemCallFilter=@system-service
# 64-bit binary only: 32-bit compatibility syscalls can never be legitimate.
SystemCallArchitectures=native
# No writable-executable mappings anywhere in this stack (no JIT):
# removes a whole exploitation primitive class outright.
MemoryDenyWriteExecute=yes

# Personal overrides live in adsum.service.d/override.conf (never in this
# file), e.g.:
#   Environment=ADSUM_AUTO_UPDATE=1
#   Environment=ADSUM_USER_ID=123456789012345678
#   Environment=ADSUM_USER_USERNAME=you
#   Environment=ADSUM_USER_GLOBAL_NAME=Your Name
#   Environment=ADSUM_IGNORE_IDS=0000000000000000000

[Install]
WantedBy=default.target
```

What matters:

- **Specifiers**: `%h` home, `%t` runtime dir (`/run/user/1000`), `%u`
  username.
- **`default.target`**: the service starts at login (add linger below to
  start at boot without a graphical login).
- **`ExecStartPre`**: removes top-level `discord-ipc-*` entries left by
  crashed runs, so a killed service does not block the next start. The
  sweep is unconditional: it does not probe live holders and can remove a
  top-level socket owned by another service. Nested client paths under
  `%t/app/...`, `%t/.flatpak/...`, and `%t/snap.*` are not touched by it;
  the bind probe reclaims a stale socket only for the index and directory
  it tries, and fan-out reclaims a dangling ours-shaped link when it
  publishes that index. A clean shutdown removes the real socket and every
  alias link.
- **Sandboxing**: `ProtectHome=read-only` plus the writable exceptions:
  `CacheDirectory=` creates the cache directory (XDG default
  `~/.cache/adsum`) before the sandbox mounts; `ReadWritePaths` adds the
  runtime dir `%t` (sockets, including the preferred
  `%t/app/com.discordapp.Discord` directory) and `~/.local/bin` (staged
  self-updates swap in at boot). The config directory is only read, so it
  needs no write exception. The unit then applies kernel/clock/hostname/
  cgroup protections, `PrivateDevices`, `RestrictSUIDSGID`/
  `RestrictRealtime`, and a `SystemCallFilter` allowlist with
  `MemoryDenyWriteExecute`, all verified against a live boot.
- **`RestrictAddressFamilies=... AF_NETLINK`**: required by the
  event-driven process watcher; without `AF_NETLINK` the scanner silently
  stays on polling.
- **Personal overrides don't belong here**: use a drop-in (section 4).

## 3. Configuration (environment)

Command-line flags win over environment variables when both are given
(clap `env`). Boolean flags accept `1`, `0`, `true`, `false`, `yes`,
`no`, `on`, `off`.

Variables you will most often set on the service:

| Variable | Default | Meaning |
|---|---|---|
| `ADSUM_BRIDGE_PORT` / `ADSUM_BRIDGE_PORT_END` | `1337` / `1347` | JSON bridge port range |
| `ADSUM_MSGPACK_PORT` | `1338` | MessagePack bridge port |
| `ADSUM_WS_PORT_START` / `ADSUM_WS_PORT_END` | `6463` / `6472` | Game websocket range |
| `ADSUM_SCAN_INTERVAL` | `5` | Process scan seconds |
| `ADSUM_OVERRIDES_FILE` | `$XDG_CONFIG_HOME/adsum/overrides.json`, else `~/.config/adsum/overrides.json` | Custom game mappings |
| `ADSUM_IGNORE_IDS` | unset | Comma-separated app ids the scanner never publishes |
| `ADSUM_AUTO_UPDATE` | unset | `1` stages verified updates in the background |
| `ADSUM_USER_ID` / `ADSUM_USER_USERNAME` / `ADSUM_USER_GLOBAL_NAME` / `ADSUM_USER_DISCRIMINATOR` / `ADSUM_USER_AVATAR` | arRPC's identity | The `READY` user (blank values ignored) |
| `ADSUM_DEBUG` | unset | `1` = debug logging + prints the resolved config |
| `RUST_LOG` | unset | Full `tracing` filter; overrides everything above for logging |

The complete flag/variable reference (ports, database, overrides,
diagnostics, updates) is in [cli-options.md](cli-options.md).

Logging: logs go to stderr (the journal, under systemd). `RUST_LOG` takes
precedence; otherwise `--debug` / `ADSUM_DEBUG=1` selects `debug` and the
default is `info`. There is no `ADSUM_LOG_LEVEL` variable.

## 4. Drop-ins: configure without editing the unit

Instead of touching the `.service` file, create
`~/.config/systemd/user/adsum.service.d/override.conf`:

```ini
[Service]
Environment=ADSUM_DEBUG=1
Environment=ADSUM_IGNORE_IDS=0000000000000000000
Environment=ADSUM_USER_USERNAME=you
```

A drop-in only replaces what it defines. Inspect the result with
`systemctl --user cat adsum.service`. Apply with:

```bash
systemctl --user daemon-reload   # ALWAYS after editing units or drop-ins
systemctl --user restart adsum.service
```

## 5. Lifecycle

```bash
systemctl --user daemon-reload        # ALWAYS after editing units or drop-ins
systemctl --user enable adsum.service # start at login
systemctl --user start|stop|restart adsum.service
systemctl --user status adsum.service # state + recent log lines
systemctl --user is-active adsum.service
systemctl --user cat adsum.service    # shows the unit + applied drop-ins
```

## 6. Surviving logout and reboot (`linger`)

```bash
loginctl show-user $USER | grep Linger   # must say Linger=yes
sudo loginctl enable-linger $USER        # if not (needs sudo, once)
```

With linger, `enable`d units boot even without a graphical login. The
install script tries this for you (plain command, then `sudo -n`).

## 7. Creating a unit from scratch (recipe)

```bash
nano ~/.config/systemd/user/my-service.service   # paste [Unit]/[Service]/[Install]
systemd-analyze --user verify my-service.service # validate syntax
systemctl --user daemon-reload
systemctl --user enable --now my-service.service  # enable + start in one go
systemctl --user status my-service.service
```

Rules of thumb: one concern per unit; `Type=exec` + `Restart=on-failure`
for daemons; prefer `Environment=`/drop-ins over wrapper scripts; secrets
go in `EnvironmentFile=` (mode `600`), never in the unit. If you copy the
shipped unit, keep `AF_NETLINK` (watcher) and the top-level IPC sweep in
`ExecStartPre`.

## See also

- [systemd-operations.md](systemd-operations.md) — logs, updates, and
  known gotchas while running.
- [cli-options.md](cli-options.md) — every flag and environment variable.
- [self-update.md](self-update.md) — how `--update`/`--rollback` verify
  releases.
