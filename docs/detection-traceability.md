# Detection traceability

How to answer "how was this game found, and why did it disappear" from
logs and snapshots. Every lifecycle transition logs one structured line
(see formats below); correlate stages by pid + app id.

## Log levels

Lifecycle lines (detection, publish, takeover, clear, resume) are
`INFO` and show at defaults. Per-tick and per-frame detail is `DEBUG`
(`RUST_LOG=debug`). Logs go to stderr — the journal under systemd:

```bash
journalctl --user -u rsrpc.service --since "10 minutes ago" | grep -E "Detected|Published|Yielding|Clearing|cleared|took over|now owns|Reaping|went away without CLEAR"
```

## Detection: source and latency

First sighting of a game logs how it was classified and how long the
process had already been alive (`process age`):

```
[Process Scanner] Detected: Hades II (1261791958597741569, pid 4242) via steam-app-id, process age 32118ms
```

The id is printed in full. A shortened form (`331224...`) in a hand-written
example tends to be copied into a grep that then matches nothing.

`process age` is omitted when the start time is unreadable (sandboxed
runtimes, exotic platforms). The start time is read once per newly
detected game — never per process per tick — so tracing costs nothing
on the scan hot path.

Sources (`via`), and what each one actually had to satisfy:

| Source | Meaning |
|---|---|
| `automaton` | A declared executable named the end of the process path |
| `cwd-joined` | Bare exe joined with the process working directory |
| `proton-automaton` | `win32` (Proton/Wine) database entries |
| `steam-app-id` | Store id (Steam environ, cmdline fallback, custom SKU), and the process runs a declared executable — or the entry declares none |
| `steam-library` | Process runs under a known Steam install dir |
| `exe-stem` | Executable stem equals a game name of at least two words |
| `folder` | Install folder carries the title |

## Why a game did not appear: the refusal lines

A `Detected:` line tells you a game matched. The absence of one is the harder
question, and its answer is at `DEBUG`, not `INFO`. Turn it on scoped to the
scanner — it is the noisy component:

```bash
systemctl --user edit rsrpc.service   # [Service]
# Environment=RUST_LOG=rsrpc_detect=debug
systemctl --user daemon-reload && systemctl --user restart rsrpc
```

Then watch for the reason a candidate was dropped:

| Line | Meaning |
|---|---|
| `{path} does not name a declared executable of {entry}, skipping` | The automaton matched a declared name inside the path, but that name does not **end** it — `.../hl2.exe.txt` holding `hl2.exe`. Specific evidence the process is not that game |
| `AppId {appid} does not belong to {argv0} ({entry}), skipping` | Steam handed this process the game's id, but it is not running one of the entry's declared executables. This is the update-time `reaper` |
| `Argument mismatch for pid {pid}, skipping` | The entry declares a required command-line string the process did not carry |
| `Steam helper process, skipping: {basename}` | Caught by the helper basename list — a cheap first line, not the corroboration rule |
| `{path} refused a declared executable match, not answering from its name` | After a refusal, the name and folder heuristics were stood down rather than answering from the install location |
| `{path} refused a declared executable match, not guessing further` | Same, standing down the tiers after the Proton probe |

Read the last two together with [process-detection.md](process-detection.md):
a refusal stops the *guessing* tiers, not the other automaton, so a native
refusal does not prevent the Proton automaton from naming the game. If you
see the first line and then no `Detected:`, the Proton probe also came up
empty — which is a genuinely undetected process, not a silenced one.

Turning the level back off:

```bash
systemctl --user revert rsrpc.service && systemctl --user restart rsrpc
```

Unreadable command lines are a different matter entirely: a process whose
`/proc/<pid>/cmdline` cannot be read is skipped before any matcher runs and
logs nothing here at all.

## Bridge: publish, takeover, clear

- `Published: {name} (app {app}, pid {pid})` — SDK card went out.
- `Published clear for {app} (pid {pid}): {reason}` — card cleared.
- `SDK presence for {app} (pid {pid}) took over slot from generic {name}`
  — a game-specific client (e.g. wwRPC-style companions) took the slot from
  generic detection. Logged once per takeover; steady republishes stay quiet.
  Two variants replace the tail: `now owns its slot` when no generic card was
  showing, and `took over slot from pid {old}` when a slot was already owned
  by something else.
- `Yielding generic {name} ({id}) to live IPC presence (pid {owner}): yielded`
  — generic card withdrawn for the live owner.
- `Source cleared for {app} (pid {pid}), resuming {n} slot(s)` — the SDK
  source sent CLEAR; generic detection re-asserts the still-running game.
- `Owner pid {pid} went away without CLEAR, resuming {n} slot(s)` —
  abrupt close; dead owners release their slots.
- `Clearing removed game slot for {app} (pid {pid}): {reason}` — the scanner
  reports that one process is gone.
- `Clearing {app} (pid {pid}): {reason}` — the scanner's table emptied, so
  every slot went at once. Note the absence of `removed game slot for`: the
  two forms are distinct lines and tell you whether one game or all of them
  went.
- `Reaping ghost card for dead pid {pid}: abrupt-close` — a card whose
  pid is provably dead but never got a clear.

Clear reasons (`: {reason}` suffix):

| Reason | Meaning |
|---|---|
| `sdk-clear` | Genuine null-activity CLEAR frame from the owning connection |
| `abrupt-close` | Owning socket died without CLEAR (ghost reap, pid-owned release) |
| `process-vanished` | Scanner reports the slot gone and liveness confirms the pid dead |
| `scan-absent` | Slot left the classified scan but the pid still runs (database refresh or ignore-list delisted a live game) |
| `yielded` | Generic card withdrawn for a live SDK owner on the same slot |

## Snapshots (`--state-file`)

Published cards carry provenance additively (absent on old cards and
unclassified payloads, so old readers keep working):

- `detectionSource`: matcher source above, or `"sdk"` for
  client-published cards.
- `detectLatencyMs`: process age in ms at first publish, omitted when
  the start time is unreadable — always omitted for SDK cards, which
  have no detection event.

Clears evict their cards, so "why did it stop" lives in the journal
(clear lines above), not the snapshot.

## See also

- [process-detection.md](process-detection.md) — how games are found.
- [cli-options.md](cli-options.md) — flags and environment variables.
- [web-client.md](web-client.md) — bridge consumers.
