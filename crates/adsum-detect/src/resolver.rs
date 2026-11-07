//! Linux wrapper and emulator resolvers: derive the real target executable
//! from `/proc/<pid>/exe` and `argv`, without overwriting `Exec.path`.
//!
//! Each resolver is specific to one wrapper family. They share helpers
//! (basename, path joins) but keep their own argv index and consumption
//! rules, so a loader's argument layout never leaks into another.

use crate::types::Exec;

/// Basename (last component) of a path, ASCII case-insensitive on the
/// final component only. Returns `None` when the path has no basename.
fn basename(path: &str) -> Option<&str> {
  path.rsplit('/').next().filter(|s| !s.is_empty())
}

/// Whether `name` is a Wine loader that should be skipped in a chain.
pub(crate) fn is_wine_loader_name(name: &str) -> bool {
  name.eq_ignore_ascii_case("wine")
    || name.eq_ignore_ascii_case("wine64")
    || name.eq_ignore_ascii_case("wine-preloader")
    || name.eq_ignore_ascii_case("wine64-preloader")
}

/// Whether `name` is one of the recognised emulators or Wine loaders.
fn is_emulator_or_loader(name: &str) -> bool {
  [
    "box64",
    "fex",
    "fexinterpreter",
    "fexloader",
    "qemu-x86_64",
    "qemu-x86_64-static",
    "wine",
    "wine64",
    "wine-preloader",
    "wine64-preloader",
  ]
  .iter()
  .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// Whether `name` is the Steam reaper launcher.
fn is_reaper(name: &str) -> bool {
  name.eq_ignore_ascii_case("reaper")
}

/// Result of resolving a wrapped process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTarget<'a> {
  pub path: &'a str,
  pub argument_start: usize,
  pub is_windows_binary: bool,
}

fn is_windows_path(target: &str) -> bool {
  target.contains('\\') || target.as_bytes().get(1).is_some_and(|b| *b == b':')
}

fn is_wine_system_path(target: &str) -> bool {
  let lower = target.to_ascii_lowercase();
  lower.contains("/drive_c/windows/")
    || lower.contains("/drive_c\\windows\\")
    || lower.ends_with("/c:/windows/system32")
    || lower.ends_with("/c:\\windows\\system32")
}

fn is_windows_binary(target: &str) -> bool {
  target.len() >= 4 && target[target.len() - 4..].eq_ignore_ascii_case(".exe")
}

/// Resolve a Box64 invocation, including `--` and Wine chains.
pub fn resolve_box64(process: &Exec) -> Option<ResolvedTarget<'_>> {
  let name = basename(&process.path)?;
  if !name.eq_ignore_ascii_case("box64") {
    return None;
  }
  resolve_box64_inner(process)
}

fn resolve_box64_inner(process: &Exec) -> Option<ResolvedTarget<'_>> {
  // `--` separator: target follows.
  if process.argument_at(0) == Some("--") {
    let target = process.argument_at(1)?;
    if target.is_empty() {
      return None;
    }
    return Some(ResolvedTarget {
      path: target,
      argument_start: 2,
      is_windows_binary: false,
    });
  }
  // Skip chained Wine loaders.
  let mut i = 0usize;
  while let Some(tok) = process.argument_at(i) {
    if is_wine_loader_name(tok) {
      i += 1;
    } else {
      break;
    }
  }
  if i > 0 {
    let target = process.argument_at(i)?;
    if target.is_empty() {
      return None;
    }
    if is_windows_path(target) || is_wine_system_path(target) {
      return None;
    }
    return Some(ResolvedTarget {
      path: target,
      argument_start: i + 1,
      is_windows_binary: true,
    });
  }
  // First token must be the target (not an option).
  let target = process.argument_at(0)?;
  if target.is_empty() || target.starts_with('-') {
    return None;
  }
  Some(ResolvedTarget {
    path: target,
    argument_start: 1,
    is_windows_binary: false,
  })
}

/// Resolve a FEX invocation: `fex`, `fexinterpreter` or `fexloader`.
pub(crate) fn resolve_fex(process: &Exec) -> Option<ResolvedTarget<'_>> {
  let name = basename(&process.path)?;
  if !name.eq_ignore_ascii_case("fex")
    && !name.eq_ignore_ascii_case("fexinterpreter")
    && !name.eq_ignore_ascii_case("fexloader")
  {
    return None;
  }
  resolve_fex_inner(process)
}

fn resolve_fex_inner(process: &Exec) -> Option<ResolvedTarget<'_>> {
  if process.argument_at(0) == Some("--") {
    let target = process.argument_at(1)?;
    if target.is_empty() {
      return None;
    }
    return Some(ResolvedTarget {
      path: target,
      argument_start: 2,
      is_windows_binary: is_windows_binary(target),
    });
  }
  let target = process.argument_at(0)?;
  if target.is_empty() || target.starts_with('-') {
    return None;
  }
  Some(ResolvedTarget {
    path: target,
    argument_start: 1,
    is_windows_binary: is_windows_binary(target),
  })
}

/// Resolve a QEMU user-mode invocation, consuming known options.
pub(crate) fn resolve_qemu_user(process: &Exec) -> Option<ResolvedTarget<'_>> {
  let name = basename(&process.path)?;
  if !name.eq_ignore_ascii_case("qemu-x86_64") && !name.eq_ignore_ascii_case("qemu-x86_64-static") {
    return None;
  }
  resolve_qemu_user_inner(process)
}

fn resolve_qemu_user_inner(process: &Exec) -> Option<ResolvedTarget<'_>> {
  let mut idx = 0usize;
  let mut target: Option<&str> = None;
  let mut target_idx = 0usize;
  while let Some(tok) = process.argument_at(idx) {
    if tok == "--" {
      let t = process.argument_at(idx + 1)?;
      if t.is_empty() {
        return None;
      }
      target = Some(t);
      target_idx = idx + 1;
      break;
    }
    if !tok.starts_with('-') {
      target = Some(tok);
      target_idx = idx;
      break;
    }
    match tok {
      "-h" => idx += 1,
      "-d" | "-L" | "-s" | "-cpu" | "-g" | "-B" | "-R" | "-E" | "-U" => {
        process.argument_at(idx + 1)?;
        idx += 2;
      }
      _ => return None,
    }
  }
  let target = target?;
  if target.is_empty() {
    return None;
  }
  Some(ResolvedTarget {
    path: target,
    argument_start: target_idx + 1,
    is_windows_binary: false,
  })
}

/// Resolve a Wine/Proton chain, skipping loaders and rejecting system paths.
pub(crate) fn resolve_wine_chain(process: &Exec) -> Option<ResolvedTarget<'_>> {
  let name = basename(&process.path)?;
  if !is_wine_loader_name(name) {
    return None;
  }
  resolve_wine_chain_inner(process)
}

/// Resolve a Wine chain, joining a relative target to the caller's cwd.
/// `buffer` is then borrowed, so the joined path needs no allocation.
pub(crate) fn resolve_wine_chain_with_cwd<'a>(
  process: &'a Exec,
  cwd: Option<&str>,
  buffer: &'a mut String,
) -> Option<ResolvedTarget<'a>> {
  let resolved = resolve_wine_chain(process)?;
  if resolved.path.starts_with('/') {
    return Some(resolved);
  }
  let cwd = cwd.filter(|cwd| !cwd.is_empty())?;
  let relative = resolved.path.strip_prefix("./").unwrap_or(resolved.path);
  buffer.clear();
  buffer.push_str(cwd.trim_end_matches('/'));
  if !buffer.ends_with('/') {
    buffer.push('/');
  }
  buffer.push_str(relative);
  Some(ResolvedTarget {
    path: buffer.as_str(),
    argument_start: resolved.argument_start,
    is_windows_binary: resolved.is_windows_binary,
  })
}

fn resolve_wine_chain_inner(process: &Exec) -> Option<ResolvedTarget<'_>> {
  let mut i = 0usize;
  while let Some(tok) = process.argument_at(i) {
    if is_wine_loader_name(tok) {
      i += 1;
    } else {
      break;
    }
  }
  let target = process.argument_at(i)?;
  if target.is_empty() {
    return None;
  }
  if is_windows_path(target) || is_wine_system_path(target) {
    return None;
  }
  Some(ResolvedTarget {
    path: target,
    argument_start: i + 1,
    is_windows_binary: is_windows_binary(target),
  })
}

/// Resolve a Steam reaper invocation.
pub(crate) fn resolve_steam_reaper(process: &Exec) -> Option<(u64, usize)> {
  let name = basename(&process.path)?;
  if !is_reaper(name) {
    return None;
  }
  resolve_steam_reaper_inner(process)
}

fn resolve_steam_reaper_inner(process: &Exec) -> Option<(u64, usize)> {
  process.arguments.as_deref()?;
  let mut has_steam_launch = false;
  let mut app_id: Option<u64> = None;
  let mut sep: Option<usize> = None;
  let mut idx = 0usize;
  while let Some(token) = process.argument_at(idx) {
    if token == "SteamLaunch" {
      has_steam_launch = true;
    } else if app_id.is_none()
      && let Some(value) = token.strip_prefix("AppId=")
      && !value.is_empty()
      && value.bytes().all(|b| b.is_ascii_digit())
    {
      app_id = value.parse().ok();
    } else if sep.is_none() && token == "--" {
      sep = Some(idx);
    }
    idx += 1;
  }
  if !has_steam_launch {
    return None;
  }
  let app_id = app_id?;
  let sep = sep?;
  let target_idx = sep + 1;
  process.argument_at(target_idx)?;
  Some((app_id, target_idx + 1))
}

/// Dispatch resolution based on the effective executable identity.
pub(crate) fn resolve_target<'a>(
  process: &'a Exec,
  image_path: Option<&str>,
) -> Option<ResolvedTarget<'a>> {
  let identity = image_path.unwrap_or(&process.path);
  let name = basename(identity)?;
  if name.eq_ignore_ascii_case("box64") {
    resolve_box64_inner(process)
  } else if name.eq_ignore_ascii_case("fex")
    || name.eq_ignore_ascii_case("fexinterpreter")
    || name.eq_ignore_ascii_case("fexloader")
  {
    resolve_fex_inner(process)
  } else if name.eq_ignore_ascii_case("qemu-x86_64")
    || name.eq_ignore_ascii_case("qemu-x86_64-static")
  {
    resolve_qemu_user_inner(process)
  } else if is_wine_loader_name(name) {
    resolve_wine_chain_inner(process)
  } else if is_reaper(name) {
    resolve_steam_reaper_inner(process).map(|(_, idx)| ResolvedTarget {
      path: process.argument_at(idx.saturating_sub(1)).unwrap_or(""),
      argument_start: idx,
      is_windows_binary: false,
    })
  } else {
    None
  }
}

/// Whether a path basename is one of the known emulators/loaders.
pub(crate) fn is_emulator_basename(name: &str) -> bool {
  is_emulator_or_loader(name)
}

#[cfg(test)]
#[path = "../tests/unit/resolver/mod.rs"]
mod tests;
