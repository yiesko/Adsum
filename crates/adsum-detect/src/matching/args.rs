//! Command-line evidence: the store tokens launchers leave in argv
//! (`AppId=` for Steam's reaper and sandboxed Proton runtimes,
//! `-epicsandboxid=`/`-epicapp=` for Epic/Heroic) and the shortcut-range
//! rule for Steam-assigned non-Steam ids. All borrowed scans — one pass
//! over the argv tail, zero allocations.

/// Steam AppId from a process command line (`reaper SteamLaunch
/// AppId=4508340 ...`): fallback when `/proc/<pid>/environ` is
/// unreadable. Sandboxed Proton runtimes (pressure-vessel/bwrap) hide
/// environ from service contexts while cmdline stays readable — without
/// this, every such game is invisible to automatic detection. Same trust
/// as environ (both launcher-provided, display-only use): the token must
/// stand alone (`AppId=` at a word boundary, followed by digits).
pub fn app_id_from_args(arguments: Option<&str>) -> Option<&str> {
  const TOKEN: &str = "AppId=";
  let args = arguments?;
  let mut rest = args;
  while let Some(pos) = rest.find(TOKEN) {
    let boundary = pos == 0
      || rest[..pos]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_ascii_alphanumeric());
    rest = &rest[pos + TOKEN.len()..];
    if !boundary {
      continue;
    }
    // Borrow the digit run instead of collecting it: ASCII digits are
    // single-byte, so the byte count is always a char boundary.
    let len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if len > 0 {
      return Some(&rest[..len]);
    }
  }
  None
}

/// Both Epic launch tokens found in one pass over the command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct EpicArgs<'a> {
  pub(crate) sandbox_id: Option<&'a str>,
  pub(crate) app_name: Option<&'a str>,
}

/// Find `-epicsandboxid=` and `-epicapp=` in one pass: each flag must start
/// an argument (line start or after whitespace) and its value runs to the
/// next whitespace; empty values are ignored.
pub(crate) fn epic_launch_args(arguments: Option<&str>) -> EpicArgs<'_> {
  const SANDBOX: &[u8] = b"-epicsandboxid=";
  const APP: &[u8] = b"-epicapp=";
  let Some(args) = arguments else {
    return EpicArgs::default();
  };
  let bytes = args.as_bytes();
  let mut found = EpicArgs::default();
  let mut at = 0;
  while at < bytes.len() {
    // Token boundary: the `-` has to start an argument (`x-epicapp=` is
    // the tail of some other flag, not this one).
    if bytes[at] != b'-' || (at != 0 && !bytes[at - 1].is_ascii_whitespace()) {
      at += 1;
      continue;
    }
    let rest = &bytes[at..];
    let (needle, is_sandbox) =
      if rest.len() >= SANDBOX.len() && rest[..SANDBOX.len()].eq_ignore_ascii_case(SANDBOX) {
        (SANDBOX, true)
      } else if rest.len() >= APP.len() && rest[..APP.len()].eq_ignore_ascii_case(APP) {
        (APP, false)
      } else {
        at += 1;
        continue;
      };
    // Borrow the value run instead of collecting it: the members are
    // single-byte ASCII, so the byte count is always a char boundary.
    let value_at = at + needle.len();
    let len = bytes[value_at..]
      .iter()
      .take_while(|byte| !byte.is_ascii_whitespace())
      .count();
    if len == 0 {
      at = value_at;
      continue;
    }
    let value = &args[value_at..value_at + len];
    if is_sandbox {
      found.sandbox_id.get_or_insert(value);
    } else {
      found.app_name.get_or_insert(value);
    }
    at = value_at + len;
  }
  found
}

/// Epic sandbox id (`-epicsandboxid=…`) from a process command line, the
/// distributor evidence Heroic/Epic leaves when the sandbox hides environ.
pub fn epic_id_from_args(arguments: Option<&str>) -> Option<&str> {
  epic_launch_args(arguments).sandbox_id
}

/// Epic app name (`-epicapp=…`) from a process command line: older
/// Legendary releases pass it without a sandbox id.
pub fn epic_app_from_args(arguments: Option<&str>) -> Option<&str> {
  epic_launch_args(arguments).app_name
}

/// Steam's non-Steam shortcut range: ids Steam itself assigns when a
/// shortcut is added carry the high bit (`crc32(exe + name) | 0x80000000`,
/// e.g. 2532755798 for "How to Fish" — verified against a live
/// `shortcuts.vdf`); real store ids are small. Unknown to the DB +
/// shortcut-range means a self-identified non-Steam game.
#[inline]
pub(crate) fn is_shortcut_id(app_id: &str) -> bool {
  app_id.parse::<u32>().is_ok_and(|id| id & 0x8000_0000 != 0)
}
