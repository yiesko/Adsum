#![allow(dead_code)]
//! Shared helpers: percent-encoding, RFC3339 parsing, byte hex.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) fn state_matches(actual: &str, expected: &str) -> bool {
  actual.len() == expected.len()
    && actual
      .bytes()
      .zip(expected.bytes())
      .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
      == 0
}

pub(crate) fn parse_query(query: &str) -> Option<HashMap<String, String>> {
  let mut values = HashMap::new();
  for pair in query.split('&').filter(|pair| !pair.is_empty()) {
    let (key, value) = pair.split_once('=')?;
    let key = percent_decode(key)?;
    let value = percent_decode(value)?;
    if values.insert(key, value).is_some() {
      return None;
    }
  }
  Some(values)
}

pub(crate) fn percent_encode(value: &str) -> String {
  let mut encoded = String::with_capacity(value.len());
  for byte in value.bytes() {
    if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
      encoded.push(char::from(byte));
    } else {
      use std::fmt::Write as _;
      let _ = write!(encoded, "%{byte:02X}");
    }
  }
  encoded
}

pub(crate) fn percent_decode(value: &str) -> Option<String> {
  let bytes = value.as_bytes();
  let mut decoded = Vec::with_capacity(bytes.len());
  let mut index = 0;
  while index < bytes.len() {
    match bytes[index] {
      b'+' => {
        decoded.push(b' ');
        index += 1;
      }
      b'%' if index + 2 < bytes.len() => {
        let high = hex_digit(bytes[index + 1])?;
        let low = hex_digit(bytes[index + 2])?;
        decoded.push((high << 4) | low);
        index += 3;
      }
      b'%' => return None,
      byte => {
        decoded.push(byte);
        index += 1;
      }
    }
  }
  String::from_utf8(decoded).ok()
}

pub(crate) fn hex_digit(byte: u8) -> Option<u8> {
  match byte {
    b'0'..=b'9' => Some(byte - b'0'),
    b'a'..=b'f' => Some(byte - b'a' + 10),
    b'A'..=b'F' => Some(byte - b'A' + 10),
    _ => None,
  }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
  let mut output = String::with_capacity(bytes.len() * 2);
  for byte in bytes {
    use std::fmt::Write as _;
    let _ = write!(output, "{byte:02x}");
  }
  output
}

pub(crate) fn parse_rfc3339(value: &str) -> Option<SystemTime> {
  if !value.is_ascii() || value.len() < 20 || value.len() > 35 {
    return None;
  }
  let (date, time_and_zone) = value.split_once('T')?;
  if date.len() != 10
    || date.as_bytes()[4] != b'-'
    || date.as_bytes()[7] != b'-'
    || !date
      .bytes()
      .enumerate()
      .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
  {
    return None;
  }
  let mut date_parts = date.split('-');
  let year = date_parts.next()?.parse::<i64>().ok()?;
  let month = date_parts.next()?.parse::<i64>().ok()?;
  let day = date_parts.next()?.parse::<i64>().ok()?;
  if date_parts.next().is_some() || !(1..=12).contains(&month) {
    return None;
  }
  let max_day = days_in_month(year, month)?;
  if !(1..=max_day).contains(&day) {
    return None;
  }

  let (clock_with_fraction, offset_seconds) = if let Some(clock) = time_and_zone.strip_suffix('Z') {
    (clock, 0_i64)
  } else {
    let sign_pos = time_and_zone
      .char_indices()
      .rev()
      .find(|(_, ch)| *ch == '+' || *ch == '-')
      .map(|(index, _)| index)?;
    let (clock, offset) = time_and_zone.split_at(sign_pos);
    if offset.len() != 6
      || offset.as_bytes()[3] != b':'
      || !offset[1..3].bytes().all(|byte| byte.is_ascii_digit())
      || !offset[4..6].bytes().all(|byte| byte.is_ascii_digit())
    {
      return None;
    }
    let sign = if offset.starts_with('+') {
      1_i64
    } else {
      -1_i64
    };
    let mut parts = offset[1..].split(':');
    let hours = parts.next()?.parse::<i64>().ok()?;
    let minutes = parts.next()?.parse::<i64>().ok()?;
    if parts.next().is_some() || !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
      return None;
    }
    (clock, sign.checked_mul(hours * 3600 + minutes * 60)?)
  };

  let (clock, nanos) = if let Some((clock, fraction)) = clock_with_fraction.split_once('.') {
    if fraction.is_empty()
      || fraction.len() > 9
      || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
      return None;
    }
    let nanos = fraction
      .parse::<u32>()
      .ok()?
      .checked_mul(10_u32.pow(9 - u32::try_from(fraction.len()).ok()?))?;
    (clock, nanos)
  } else {
    (clock_with_fraction, 0)
  };
  if clock.len() != 8
    || clock.as_bytes()[2] != b':'
    || clock.as_bytes()[5] != b':'
    || !clock
      .bytes()
      .enumerate()
      .all(|(index, byte)| matches!(index, 2 | 5) || byte.is_ascii_digit())
  {
    return None;
  }
  let mut time_parts = clock.split(':');
  let hour = time_parts.next()?.parse::<i64>().ok()?;
  let minute = time_parts.next()?.parse::<i64>().ok()?;
  let second = time_parts.next()?.parse::<i64>().ok()?;
  if time_parts.next().is_some()
    || !(0..=23).contains(&hour)
    || !(0..=59).contains(&minute)
    || !(0..=59).contains(&second)
  {
    return None;
  }

  let days = days_from_civil(year, month, day)?;
  let day_seconds = days.checked_mul(86_400)?;
  let clock_seconds = hour
    .checked_mul(3600)?
    .checked_add(minute.checked_mul(60)?)?
    .checked_add(second)?;
  let epoch = day_seconds
    .checked_add(clock_seconds)?
    .checked_sub(offset_seconds)?;
  let epoch = u64::try_from(epoch).ok()?;
  UNIX_EPOCH.checked_add(Duration::new(epoch, nanos))
}

pub(crate) fn is_leap_year(year: i64) -> bool {
  year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub(crate) fn days_in_month(year: i64, month: i64) -> Option<i64> {
  match month {
    1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
    4 | 6 | 9 | 11 => Some(30),
    2 => Some(if is_leap_year(year) { 29 } else { 28 }),
    _ => None,
  }
}

pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
  let adjust = if month <= 2 { 1_i64 } else { 0_i64 };
  let y = year.checked_sub(adjust)?;
  let era = y.div_euclid(400);
  let year_of_era = y.checked_sub(era.checked_mul(400)?)?;
  let shifted_month = month.checked_add(if month > 2 { -3 } else { 9 })?;
  let day_of_year =
    (153_i64.checked_mul(shifted_month)?.checked_add(2)? / 5).checked_add(day.checked_sub(1)?)?;
  let day_of_era = year_of_era
    .checked_mul(365)?
    .checked_add(year_of_era / 4)?
    .checked_sub(year_of_era / 100)?
    .checked_add(day_of_year)?;
  era
    .checked_mul(146_097)?
    .checked_add(day_of_era)?
    .checked_sub(719_468)
}
