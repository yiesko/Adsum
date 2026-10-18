#![no_main]

use libfuzzer_sys::fuzz_target;

// Full IPC pipeline: parse an activity command, apply fixes, encode both
// bridge protocols. Must never panic on any input: games and tools send
// whatever bytes they want over the socket.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(mut cmd) = serde_json::from_str::<adsum_types::cmd::ActivityCmd>(text) else {
        return;
    };
    cmd.fix();
    // No fingerprint: exercises the always-changed path, like a first publish.
    // `cached_activity` consumes the command — the activity moves into the
    // payload instead of being cloned for it — so the fuzz input runs the
    // whole of what the bridge does with a command before the fan-out.
    let _ = adsum_protocol::commands::cached_activity(cmd, None);
});
