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
# Official detectable list with trim, hourly refresh while running,
# offline bundled fallback. Add --auto-update (or ADSUM_AUTO_UPDATE=1
# in a adsum.service.d/override.conf drop-in) to also stage verified
# release binaries in the background.
ExecStart=%h/.local/bin/adsum --enable-db-update
Restart=on-failure
RestartSec=5s
# Covers 2s accept wait + 5s connection drain + teardown/cleanup.
TimeoutStopSec=15s
KillMode=mixed
NoNewPrivileges=yes
# Host /tmp receives the IPC alias symlinks discord-ipc-{n} (games probing
# the last official dir), their ownership markers, and the private fallback
# socket under /tmp/.adsum-ipc-<euid> when the runtime candidates are
# unusable. ProtectSystem=strict mounts /tmp read-only, so the host path is
# bound back in writable; the cache still goes to ~/.cache.
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
- **Socket cleanup**: the IPC transport probes each index before binding;
  `Active`, `Unknown`, and `NotSocket` entries are preserved, while a
  `Stale` socket may be reclaimed after the identity check. Valid orphan
  markers are recovered before bind. `SIGTERM` and Ctrl+C run the same
  graceful shutdown, which removes only the bound socket, the aliases Adsum
  owns, and their markers. `TimeoutStopSec=15s` covers the 2s accept wait,
  the 5s connection drain, and teardown/cleanup.
- **Sandboxing**: `ProtectHome=read-only` plus the writable exceptions:
  `CacheDirectory=` creates the cache directory (XDG default
  `~/.cache/adsum`) before the sandbox mounts; `ReadWritePaths` adds the
  runtime dir `%t` (sockets, including the preferred
  `%t/app/com.discordapp.Discord` directory) and `~/.local/bin` (staged
  self-updates swap in at boot). `BindPaths=/tmp` keeps the host `/tmp`
  writable because compatibility aliases and the private fallback socket
  are still published there. The config directory is only read, so it
  needs no write exception. The unit then applies kernel, clock, hostname
  and cgroup protections, `PrivateDevices`, `RestrictSUIDSGID`/
  `RestrictRealtime`, and a `SystemCallFilter` allowlist with
  `MemoryDenyWriteExecute`, all verified against a live boot.
- **User namespaces and the overflow UID**: in a *user* unit,
  `PrivateDevices=yes` and `ProtectKernelTunables=yes` enable `PrivateUsers`
  implicitly. That namespace also makes `cn_proc` unavailable, so the event
  watcher stays on polling (section 4 shows the opt-in drop-in). The unit
  then runs in a user namespace where host UID 0 (and
  every other unmapped UID) appears as the overflow UID
  (`/proc/sys/kernel/overflowuid`, usually `65534`). Adsum accepts that UID
  only for ancestor mount points and canonical temp roots, only when
  `/proc/self/uid_map` proves parent UID 0 is unmapped and the overflow UID is
  not mapped inside the namespace, and only when `statx`/`STATX_MNT_ID` is
  available (Linux 5.8 or newer); otherwise the exception stays off and the
  bind fails closed. The socket directory itself must still be owned by the
  effective UID with mode `0700`, and `/tmp` is the practical fallback under
  this sandbox. See `protocol.md` for the exact policy.
- **IPC fd operations**: the Linux install opens the temp socket with
  `O_PATH`, checks it with `fstat`, applies `0600` with
  `fchmodat`/`fchmodat2` (falling back to `chmod` via `/proc/self/fd`),
  and publishes it with `linkat` (falling back to the same proc path).
  `SystemCallFilter=@system-service` already includes these calls, so the
  unit needs no extra syscall exception or capability. The unit keeps
  `BindPaths=/tmp` for host aliases and the private fallback, and the `%t`
  write exception for runtime sockets.
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

### Enabling the event watcher (opt-in, weaker sandbox)

The default unit's sandbox creates a user namespace, where the kernel
ignores `cn_proc`; the watcher logs one warning and stays on polling. A
user unit can enable the watcher without a system unit by removing every
directive that creates a user namespace. This stays a user unit: no root,
no system manager, and no username in any file.

Create `~/.config/systemd/user/adsum.service.d/watcher.conf`:

```ini
[Service]
ProtectSystem=no
ProtectHome=no
BindPaths=
ReadWritePaths=
PrivateDevices=no
ProtectKernelTunables=no
ProtectKernelModules=no
ProtectKernelLogs=no
ProtectClock=no
ProtectHostname=no
ProtectControlGroups=no
```

Apply and confirm:

```bash
systemctl --user daemon-reload
systemctl --user restart adsum.service
invocation=$(systemctl --user show -p InvocationID --value adsum.service)
journalctl --user -u adsum.service _SYSTEMD_INVOCATION_ID="$invocation" \
  | grep "watcher live"
```

**Trade-off:** these resets remove the filesystem and device isolation. The
service can read and write everything your account can, including `~/.ssh`
and shell startup files, and it can access the devices your account can.
`NoNewPrivileges`, the `@system-service` syscall allowlist,
`RestrictAddressFamilies` (with `AF_NETLINK`), `LockPersonality`,
`RestrictSUIDSGID`, `RestrictRealtime`, `SystemCallArchitectures` and
`MemoryDenyWriteExecute` stay enabled. Use this only if instant detection is
worth that trade-off.

Do not add `PrivateUsers=yes` or `PrivatePIDs=yes` back: either namespace
makes the kernel ignore `cn_proc` again.

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
shipped unit, keep `AF_NETLINK` (watcher) and `BindPaths=/tmp` (IPC aliases
and the private fallback socket published into host `/tmp`).

## See also

- [systemd-operations.md](systemd-operations.md) — logs, updates, and
  known gotchas while running.
- [cli-options.md](cli-options.md) — every flag and environment variable.
- [self-update.md](self-update.md) — how `--update`/`--rollback` verify
  releases.
